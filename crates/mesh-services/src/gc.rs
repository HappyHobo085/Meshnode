//! Blob GC (spec §5.5): a service blob that no manifest and no active service references
//! is removed once it has been unreferenced for 24 hours.
//!
//! The first-unreferenced times live in `<data>/services/gc.json` and are measured with the
//! node's wall clock. A pass never removes anything unless the node is `Running`: while it
//! re-bootstraps or is removed, the manifests in `_mesh` may be incomplete.
//!
//! The GC only considers blobs services own: the modules and static roots this node
//! deployed or fetched for a service, recorded in `<data>/services/owned.json` (a static
//! root's children are covered by its collection). Anything else pinned on the node, such
//! as a blob the app added itself, is never collected here. An owned hash is dropped from
//! the set once it is removed.
//!
//! A pass holds the write side of `Shared::gc_lock`, which deploys hold (read side) from
//! storing a blob until its manifest is written. Just before removing, it recomputes the
//! referenced set, so a manifest that arrived from another node meanwhile also saves a blob.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
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

/// `owned.json`: the lower-case hex hashes of the blobs services own.
#[derive(Default, Serialize, Deserialize)]
struct Owned {
    owned: BTreeSet<String>,
}

fn owned_path(shared: &Shared) -> PathBuf {
    shared.node.data_dir().join("services").join("owned.json")
}

/// Loads `owned.json`, lets `f` change it and saves it if `f` returns true, all under
/// `Shared::owned_file`.
fn update_owned(
    shared: &Shared,
    f: impl FnOnce(&mut BTreeSet<String>) -> bool,
) -> Result<(), ServicesError> {
    let _file = shared
        .owned_file
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let path = owned_path(shared);
    let mut owned: Owned = load_json(&path, "owned.json");
    if f(&mut owned.owned) {
        save_json(&path, &owned)?;
    }
    Ok(())
}

/// Records `hash` (a module or a static root, pinned on this node) as service-owned, so
/// the GC may remove it once nothing references it.
pub(crate) fn mark_owned(shared: &Shared, hash: BlobHash) -> Result<(), ServicesError> {
    update_owned(shared, |owned| owned.insert(hash.to_hex().to_string()))
}

/// The hashes services own, as lower-case hex.
fn owned(shared: &Shared) -> BTreeSet<String> {
    let _file = shared
        .owned_file
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    load_json::<Owned>(&owned_path(shared), "owned.json").owned
}

fn state_path(shared: &Shared) -> PathBuf {
    shared.node.data_dir().join("services").join("gc.json")
}

fn load_json<S: Default + for<'de> Deserialize<'de>>(path: &std::path::Path, what: &str) -> S {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(error = %e, "unreadable {what}; starting fresh");
            S::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => S::default(),
        Err(e) => {
            warn!(error = %e, "cannot read {what}; starting fresh");
            S::default()
        }
    }
}

fn save_json(path: &std::path::Path, state: &impl Serialize) -> Result<(), ServicesError> {
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
    let old: GcState = load_json(&path, "gc.json");
    let owned = owned(shared);
    let mut next = GcState::default();
    let mut doomed = Vec::new();
    for (hash, _kind) in shared.node.blobs().pins().await? {
        let key = hash.to_hex().to_string();
        if keep.contains(&hash) || !owned.contains(&key) {
            continue;
        }
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
    save_json(&path, &next)?;
    if doomed.is_empty() {
        return Ok(());
    }
    // One sweep for all of them (a few blob-GC ticks), so the lock is held about as long
    // as a single removal.
    let blobs = shared.node.blobs();
    if let Err(e) = blobs.remove_many(&doomed).await {
        warn!(count = doomed.len(), error = %e, "blob removal failed");
        return Ok(());
    }
    let mut removed = BTreeSet::new();
    for hash in &doomed {
        // A fetch may have pinned it again meanwhile; then it stays owned.
        if !blobs.is_pinned(*hash).await? {
            info!(hash = %hash.to_hex(), "removed unreferenced service blob");
            removed.insert(hash.to_hex().to_string());
        }
    }
    update_owned(shared, |owned| {
        let before = owned.len();
        owned.retain(|h| !removed.contains(h));
        owned.len() != before
    })?;
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
