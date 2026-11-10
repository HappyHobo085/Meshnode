//! Blob GC (spec §5.5): a service blob that no manifest and no active service references
//! is removed once it has been unreferenced for 24 hours.
//!
//! The first-unreferenced times live in `<data>/services/gc.json` and are measured with the
//! node's wall clock. A pass never removes anything unless the node is `Running`: while it
//! re-bootstraps or is removed, the manifests in `_mesh` may be incomplete.
//!
//! The GC considers every pinned blob on the node, not only service blobs: spec §5.5 says
//! "a pinned hash". Anything pinned that no service references ages out after 24 hours.
//!
//! A pass holds the write side of `Shared::gc_lock`, which deploys hold (read side) from
//! storing a blob until its manifest is written. Just before removing, it recomputes the
//! referenced set, so a manifest that arrived from another node meanwhile also saves a blob.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mesh_core::local::atomic_write;
use mesh_core::membership::MESH_NS;
use mesh_core::{BlobHash, NodeStatus};
use mesh_proto::ServiceKind;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use tracing::{info, warn};

use crate::deployer::Shared;
use crate::error::ServicesError;
use crate::manifest::read_manifest;

/// How long a blob must stay unreferenced before it is removed.
const GRACE_MS: u64 = 24 * 60 * 60 * 1000;
/// The GC runs this often.
pub(crate) const GC_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long `stop` waits for a pass before aborting it.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const PAGE: u32 = 256;

/// `gc.json`: lower-case hex hash -> wall-clock ms when it was first seen unreferenced.
#[derive(Default, Serialize, Deserialize)]
struct GcState {
    first_unreferenced_ms: BTreeMap<String, u64>,
}

fn state_path(shared: &Shared) -> PathBuf {
    shared.node.data_dir().join("services").join("gc.json")
}

fn load_state(path: &std::path::Path) -> GcState {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(error = %e, "unreadable gc.json; starting fresh");
            GcState::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GcState::default(),
        Err(e) => {
            warn!(error = %e, "cannot read gc.json; starting fresh");
            GcState::default()
        }
    }
}

fn save_state(path: &std::path::Path, state: &GcState) -> Result<(), ServicesError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    atomic_write(path, &bytes)?;
    Ok(())
}

/// Every hash that must stay: manifests' modules and roots, static roots' children, and
/// the active table. `None` if the node is not `Running` (before or after reading).
async fn referenced(shared: &Shared) -> Result<Option<BTreeSet<BlobHash>>, ServicesError> {
    let node = &shared.node;
    if node.status() != NodeStatus::Running {
        return Ok(None);
    }
    let store = node.store();
    let mut names = BTreeSet::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .list_keys(MESH_NS, "service/", after.take(), PAGE)
            .await?;
        let full = page.len() == PAGE as usize;
        after = page.last().cloned();
        names.extend(
            page.iter()
                .filter_map(|k| k.strip_prefix("service/").map(str::to_owned)),
        );
        if !full {
            break;
        }
    }
    let mut kinds = shared.active_kinds();
    for name in names {
        // An unreadable manifest hides what it references, so the pass is abandoned.
        if let Some(m) = read_manifest(store, &name).await? {
            kinds.push(m.kind);
        }
    }
    let mut keep = BTreeSet::new();
    for kind in kinds {
        match kind {
            ServiceKind::Wasm { module } => {
                keep.insert(BlobHash::from(module));
            }
            ServiceKind::Static { root } => {
                let root = BlobHash::from(root);
                keep.insert(root);
                if let Some(entries) = node.blobs().collection_entries(root).await? {
                    keep.extend(entries.into_iter().map(|(_, h)| h));
                }
            }
        }
    }
    // The manifest view is only trusted if the node stayed up while it was read.
    Ok((node.status() == NodeStatus::Running).then_some(keep))
}

/// Where the test hook pauses a pass that has chosen blobs to remove.
async fn hold_point(shared: &Shared) {
    shared.gc_reached.notify_one();
    let mut hold = shared.gc_hold.subscribe();
    let _ = hold.wait_for(|on| !*on).await;
}

/// One GC pass.
pub(crate) async fn run_once(shared: &Shared) -> Result<(), ServicesError> {
    let _exclusive = shared.gc_lock.write().await;
    let Some(keep) = referenced(shared).await? else {
        info!("blob GC skipped: node is not running");
        return Ok(());
    };
    let now = shared.node.now_ms();
    let path = state_path(shared);
    let old = load_state(&path);
    let mut next = GcState::default();
    let mut doomed = Vec::new();
    for (hash, _kind) in shared.node.blobs().pins().await? {
        if keep.contains(&hash) {
            continue;
        }
        let key = hash.to_hex().to_string();
        // A first sighting in the future (the clock moved back) restarts the grace period.
        let first = old
            .first_unreferenced_ms
            .get(&key)
            .copied()
            .filter(|f| *f <= now)
            .unwrap_or(now);
        if now - first >= GRACE_MS {
            doomed.push(hash);
        } else {
            next.first_unreferenced_ms.insert(key, first);
        }
    }
    if !doomed.is_empty() {
        hold_point(shared).await;
        // Manifests may have changed since `keep` was computed: look again before removing.
        let Some(keep) = referenced(shared).await? else {
            info!("blob GC skipped: node is not running");
            return Ok(());
        };
        doomed.retain(|h| !keep.contains(h));
    }
    save_state(&path, &next)?;
    if doomed.is_empty() {
        return Ok(());
    }
    // One sweep for all of them (a few blob-GC ticks), so the lock is held about as long
    // as a single removal.
    match shared.node.blobs().remove_many(&doomed).await {
        Ok(()) => {
            for hash in &doomed {
                info!(hash = %hash.to_hex(), "removed unreferenced service blob");
            }
        }
        Err(e) => warn!(count = doomed.len(), error = %e, "blob removal failed"),
    }
    Ok(())
}

/// The running hourly GC; [`GcTask::stop`] ends it.
pub(crate) struct GcTask {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl GcTask {
    pub fn spawn(shared: Arc<Shared>, every: Duration) -> GcTask {
        let (stop, rx) = watch::channel(false);
        let join = tokio::spawn(run(shared, every, rx));
        GcTask { stop, join }
    }

    /// Stops the task and waits until it has dropped its node handle.
    pub async fn stop(self) {
        let GcTask { stop, mut join } = self;
        let _ = stop.send(true);
        if timeout(STOP_TIMEOUT, &mut join).await.is_err() {
            warn!("blob GC did not stop in time; aborting it");
            join.abort();
            let _ = join.await;
        }
    }
}

async fn run(shared: Arc<Shared>, every: Duration, mut stop: watch::Receiver<bool>) {
    let cancelled = shared.node.cancelled();
    tokio::pin!(cancelled);
    // The first pass is one interval after start, not at start.
    let mut tick = interval_at(Instant::now() + every, every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = &mut cancelled => return,
            _ = stop.changed() => return,
            _ = tick.tick() => {}
        }
        tokio::select! {
            _ = &mut cancelled => return,
            _ = stop.changed() => return,
            r = run_once(&shared) => {
                if let Err(e) = r {
                    warn!(error = %e, "blob GC pass failed");
                }
            }
        }
    }
}
