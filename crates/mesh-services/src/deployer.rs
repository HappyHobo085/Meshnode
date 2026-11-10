//! The deployer (spec §5.2): one supervised task per node that keeps the active table in
//! line with the manifests in `_mesh` and this node's placement.
//!
//! Store change events and node events are only wake-up signals: every wake-up runs a
//! full scan of the manifests, so a lost event costs nothing. A scan also runs every
//! `rescan_every` and when either event stream lags. Placement depends on the membership
//! snapshot, which the node updates after the `_mesh` change that caused it, so membership
//! and tag changes are picked up from `NodeEvent::MembersChanged`.
//!
//! While the node re-bootstraps its store is emptied and refilled, so a scan then leaves
//! every known and active service as it is (it may only add services); the scan after
//! `RebootstrapDone` brings everything up to date.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use mesh_core::membership::MESH_NS;
use mesh_core::{BlobHash, Change, ChangeKind, NodeEvent, NodeHandle, NodeStatus};
use mesh_proto::{ServiceKind, ServiceManifest};
use mesh_runtime::{Loaded, Runtime};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::watch;
use tokio::task::{AbortHandle, Id, JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, sleep_until, timeout};
use tracing::{error, info, warn};

use crate::error::ServicesError;
use crate::gc::mark_owned;
use crate::manifest::{current_generation, data_ns, module_too_large, read_manifest};
use crate::placement::placed_among;
use crate::static_site::StaticSite;
use crate::status::LocalStatus;

/// A fetch that takes longer is abandoned and retried.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
/// How long `stop` waits for the deployer before aborting it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const PAGE: u32 = 256;

/// The deployer's intervals. `Default` is the spec's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeployerTimings {
    /// A full scan runs at least this often (events are lossy).
    pub rescan_every: Duration,
    /// The first retry after a failed fetch or load; doubled on each further failure.
    pub retry_min: Duration,
    pub retry_max: Duration,
}

impl Default for DeployerTimings {
    fn default() -> Self {
        DeployerTimings {
            rescan_every: Duration::from_secs(60),
            retry_min: Duration::from_secs(5),
            retry_max: Duration::from_secs(300),
        }
    }
}

/// A service this node serves. Requests clone the `Arc`, so an in-flight request finishes
/// on the version it started with.
pub(crate) struct Active {
    pub manifest: ServiceManifest,
    pub generation: u64,
    pub payload: Payload,
}

#[derive(Clone)]
pub(crate) enum Payload {
    Wasm(Loaded),
    /// The collection is fetched and its index loaded.
    Static(Arc<StaticSite>),
}

/// A deployed service as the last scan saw it.
pub(crate) struct Known {
    pub manifest: ServiceManifest,
    pub generation: u64,
    pub local: LocalStatus,
    pub data_bytes: u64,
}

/// State shared by `Services` and the deployer.
pub(crate) struct Shared {
    pub node: NodeHandle,
    pub runtime: Runtime,
    active: RwLock<HashMap<String, Arc<Active>>>,
    known: Mutex<BTreeMap<String, Known>>,
    /// While true the deployer does not scan (a test hook; always present so the loop
    /// has one shape).
    pub paused: watch::Sender<bool>,
    /// Held by the deployer for each scan (checking `paused` under it), so a pause can wait
    /// until no scan runs.
    pub scanning: tokio::sync::Mutex<()>,
    /// Blob GC holds the write side while it decides and removes; deploys hold the read
    /// side from storing a blob until its manifest is written, so GC never removes a blob
    /// a deploy is about to reference (this also serializes GC passes).
    pub gc_lock: tokio::sync::RwLock<()>,
    /// Serializes read-modify-write cycles of `owned.json` (see `gc`).
    pub owned_file: Mutex<()>,
    /// Test hook: while true, a GC pass that has chosen blobs to remove waits (holding the
    /// lock) before re-checking and removing them.
    pub gc_hold: watch::Sender<bool>,
    /// Test hook: notified when a pass reaches that wait.
    pub gc_reached: tokio::sync::Notify,
    /// Scans the deployer has finished (a test hook reads it).
    pub scans: AtomicU64,
}

impl Shared {
    pub fn new(node: NodeHandle, runtime: Runtime) -> Shared {
        Shared {
            node,
            runtime,
            active: RwLock::default(),
            known: Mutex::default(),
            paused: watch::Sender::new(false),
            scanning: tokio::sync::Mutex::new(()),
            gc_lock: tokio::sync::RwLock::new(()),
            owned_file: Mutex::new(()),
            gc_hold: watch::Sender::new(false),
            gc_reached: tokio::sync::Notify::new(),
            scans: AtomicU64::new(0),
        }
    }

    pub fn active(&self, name: &str) -> Option<Arc<Active>> {
        let table = self.active.read().unwrap_or_else(PoisonError::into_inner);
        table.get(name).cloned()
    }

    fn set_active(&self, name: &str, a: Active) {
        let mut table = self.active.write().unwrap_or_else(PoisonError::into_inner);
        table.insert(name.to_owned(), Arc::new(a));
    }

    fn remove_active(&self, name: &str) {
        let mut table = self.active.write().unwrap_or_else(PoisonError::into_inner);
        if table.remove(name).is_some() {
            info!(service = name, "service stopped on this node");
        }
    }

    /// What every active service was loaded from.
    pub fn active_kinds(&self) -> Vec<ServiceKind> {
        let table = self.active.read().unwrap_or_else(PoisonError::into_inner);
        table.values().map(|a| a.manifest.kind.clone()).collect()
    }

    fn active_names(&self) -> Vec<String> {
        let table = self.active.read().unwrap_or_else(PoisonError::into_inner);
        table.keys().cloned().collect()
    }

    pub fn with_known<R>(&self, f: impl FnOnce(&mut BTreeMap<String, Known>) -> R) -> R {
        f(&mut self.known.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The running deployer; [`DeployerTask::stop`] ends it.
pub(crate) struct DeployerTask {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl DeployerTask {
    pub fn spawn(shared: Arc<Shared>, timings: DeployerTimings) -> DeployerTask {
        let (stop, rx) = watch::channel(false);
        let join = tokio::spawn(supervise(shared, timings, rx));
        DeployerTask { stop, join }
    }

    /// Stops the deployer and waits until it, and every fetch it started, has ended, so
    /// none of them holds the node any more.
    pub async fn stop(self) {
        let DeployerTask { stop, mut join } = self;
        let _ = stop.send(true);
        if timeout(STOP_TIMEOUT, &mut join).await.is_err() {
            warn!("deployer did not stop in time; aborting it");
            join.abort();
            let _ = join.await;
        }
    }
}

/// Aborts the task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs the deployer and restarts it with backoff if it panics.
async fn supervise(shared: Arc<Shared>, timings: DeployerTimings, mut stop: watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut task = AbortOnDrop(tokio::spawn(run(shared.clone(), timings, stop.clone())));
        match (&mut task.0).await {
            Err(e) if e.is_panic() => error!(error = %e, "deployer panicked; restarting"),
            _ => return,
        }
        let cancelled = shared.node.cancelled();
        tokio::select! {
            _ = cancelled => return,
            _ = stop.changed() => return,
            _ = sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// A service whose wanted payload is not the active one.
struct Track {
    kind: ServiceKind,
    loading: Option<AbortHandle>,
    failures: u32,
    retry_at: Option<Instant>,
    /// The last failure, shown until the next attempt starts.
    failed: Option<LocalStatus>,
}

impl Track {
    fn new(kind: ServiceKind) -> Track {
        Track {
            kind,
            loading: None,
            failures: 0,
            retry_at: None,
            failed: None,
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        // A stale fetch is abandoned.
        if let Some(h) = self.loading.take() {
            h.abort();
        }
    }
}

type LoadDone = (String, ServiceKind, Result<Payload, String>);

struct Deployer {
    shared: Arc<Shared>,
    timings: DeployerTimings,
    tracks: HashMap<String, Track>,
    tasks: JoinSet<LoadDone>,
    /// Bytes per namespace as of the last periodic rescan: computing them reads the whole
    /// store, so the other wake-ups reuse these.
    sizes: BTreeMap<String, u64>,
}

async fn run(shared: Arc<Shared>, timings: DeployerTimings, mut stop: watch::Receiver<bool>) {
    let mut changes = shared.node.store().subscribe();
    let mut events = shared.node.subscribe();
    let mut paused = shared.paused.subscribe();
    let cancelled = shared.node.cancelled();
    tokio::pin!(cancelled);
    let mut tick = interval(timings.rescan_every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut d = Deployer {
        shared,
        timings,
        tracks: HashMap::new(),
        tasks: JoinSet::new(),
        sizes: BTreeMap::new(),
    };
    loop {
        let retry = d.next_retry();
        let mut scan = false;
        let mut periodic = false;
        tokio::select! {
            _ = &mut cancelled => break,
            _ = stop.changed() => break,
            _ = tick.tick() => {
                scan = true;
                periodic = true;
            }
            _ = paused.changed() => scan = true,
            r = changes.recv() => match r {
                Ok(ch) => scan = relevant(&ch),
                Err(RecvError::Lagged(n)) => {
                    warn!(missed = n, "store events lagged; rescanning services");
                    scan = true;
                }
                Err(RecvError::Closed) => break,
            },
            r = events.recv() => match r {
                Ok(ev) => scan = wakes(&ev),
                Err(RecvError::Lagged(_)) => scan = true,
                Err(RecvError::Closed) => break,
            },
            Some(done) = d.tasks.join_next_with_id() => d.finished(done),
            _ = sleep_until(retry.unwrap_or_else(Instant::now)), if retry.is_some() => {
                d.release_due();
                scan = true;
            }
        }
        // Coalesce a burst into one scan.
        loop {
            match changes.try_recv() {
                Ok(ch) => scan |= relevant(&ch),
                Err(TryRecvError::Lagged(_)) => scan = true,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        loop {
            match events.try_recv() {
                Ok(ev) => scan |= wakes(&ev),
                Err(TryRecvError::Lagged(_)) => scan = true,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        if scan {
            let shared = d.shared.clone();
            let _scanning = shared.scanning.lock().await;
            if !*paused.borrow_and_update() {
                d.scan(periodic).await;
            }
        }
    }
    d.tracks.clear();
    d.tasks.shutdown().await;
}

/// A manifest or a data generation changed.
fn relevant(ch: &Change) -> bool {
    ch.ns == MESH_NS
        && match &ch.kind {
            ChangeKind::Key(k) => k.starts_with("service/"),
            ChangeKind::Set(s) => s.starts_with("service-gen/"),
        }
}

/// The membership snapshot (roles, removals, tags) changed, or the store was replaced.
fn wakes(ev: &NodeEvent) -> bool {
    matches!(ev, NodeEvent::MembersChanged | NodeEvent::RebootstrapDone)
}

impl Deployer {
    /// A full scan; `periodic` (the rescan tick) also recomputes the namespace sizes.
    async fn scan(&mut self, periodic: bool) {
        if periodic {
            match self.shared.node.store().call(|r| r.namespace_sizes()).await {
                Ok(sizes) => self.sizes = sizes,
                Err(e) => warn!(error = %e, "cannot compute namespace sizes"),
            }
        }
        if let Err(e) = self.try_scan().await {
            warn!(error = %e, "service scan failed");
        }
        self.shared.scans.fetch_add(1, Ordering::Relaxed);
    }

    async fn try_scan(&mut self) -> Result<(), ServicesError> {
        let node = self.shared.node.clone();
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
        names.extend(
            self.shared
                .with_known(|k| k.keys().cloned().collect::<Vec<_>>()),
        );
        names.extend(self.shared.active_names());
        names.extend(self.tracks.keys().cloned());

        let mut seen = Vec::with_capacity(names.len());
        for name in names {
            let manifest = match read_manifest(store, &name).await {
                Ok(m) => m,
                Err(ServicesError::InvalidManifest(e)) => {
                    warn!(service = %name, error = %e, "unreadable manifest; left as it was");
                    continue;
                }
                Err(e) => return Err(e),
            };
            let generation = match &manifest {
                Some(_) => current_generation(store, &name).await?,
                None => 0,
            };
            seen.push((name, manifest, generation));
        }
        // Read after the store: the node turns `Rebootstrapping` before it resets its store,
        // so `Running` here means everything above was read from a whole store. While it
        // re-bootstraps, `_mesh` is being refilled: nothing is forgotten, un-placed or
        // changed, only services not active yet may be added (`RebootstrapDone` rescans).
        let frozen = matches!(node.status(), NodeStatus::Rebootstrapping { .. });
        let members = node.members();
        let me = node.node_id();
        for (name, manifest, generation) in seen {
            if frozen && self.shared.active(&name).is_some() {
                continue;
            }
            let Some(m) = manifest else {
                if !frozen {
                    self.forget(&name);
                }
                continue;
            };
            let placed = placed_among(&m, me, &members);
            if frozen && !placed {
                continue;
            }
            let data_bytes = self
                .sizes
                .get(&data_ns(&name, generation))
                .copied()
                .unwrap_or(0);
            self.apply(&name, m, generation, placed, data_bytes);
        }
        Ok(())
    }

    /// Undeployed: stop serving and forget it.
    fn forget(&mut self, name: &str) {
        self.tracks.remove(name);
        self.shared.remove_active(name);
        self.shared.with_known(|k| k.remove(name));
    }

    fn apply(
        &mut self,
        name: &str,
        m: ServiceManifest,
        generation: u64,
        placed: bool,
        data_bytes: u64,
    ) {
        let local = if !placed {
            self.tracks.remove(name);
            self.shared.remove_active(name);
            LocalStatus::NotPlaced
        } else if let Some(a) = self
            .shared
            .active(name)
            .filter(|a| a.manifest.kind == m.kind)
        {
            // Same module or root: only limits, outbound, placement, version or generation
            // can differ, and they take effect at once.
            self.tracks.remove(name);
            if a.manifest != m || a.generation != generation {
                self.shared.set_active(
                    name,
                    Active {
                        manifest: m.clone(),
                        generation,
                        payload: a.payload.clone(),
                    },
                );
            }
            LocalStatus::Ready { version: m.version }
        } else {
            self.want(name, &m.kind)
        };
        self.shared.with_known(|k| {
            k.insert(
                name.to_owned(),
                Known {
                    manifest: m,
                    generation,
                    local,
                    data_bytes,
                },
            )
        });
    }

    /// Makes sure `kind` is being fetched (unless waiting to retry); the previous version,
    /// if any, keeps serving. Returns the local status.
    fn want(&mut self, name: &str, kind: &ServiceKind) -> LocalStatus {
        if self.tracks.get(name).is_some_and(|t| t.kind != *kind) {
            // A newer manifest replaced the target: the stale fetch is abandoned.
            self.tracks.remove(name);
        }
        if !self.tracks.contains_key(name) {
            self.tracks
                .insert(name.to_owned(), Track::new(kind.clone()));
        }
        let t = &self.tracks[name];
        if t.loading.is_none() && t.retry_at.is_none() {
            let shared = self.shared.clone();
            let (n, k) = (name.to_owned(), kind.clone());
            let handle = self.tasks.spawn(async move {
                let r = load(&shared, &k).await;
                (n, k, r)
            });
            let t = self.tracks.get_mut(name).expect("inserted above");
            t.loading = Some(handle);
            t.failed = None;
        }
        let t = &self.tracks[name];
        match (&t.loading, &t.failed) {
            (None, Some(f)) => f.clone(),
            _ => LocalStatus::Fetching,
        }
    }

    fn finished(&mut self, done: Result<(Id, LoadDone), JoinError>) {
        let (id, outcome) = match done {
            Ok((id, (name, kind, r))) => (id, Ok((name, kind, r))),
            Err(e) if e.is_cancelled() => return,
            Err(e) => (e.id(), Err(e.to_string())),
        };
        let name = match &outcome {
            Ok((name, ..)) => Some(name.clone()),
            Err(_) => self
                .tracks
                .iter()
                .find(|(_, t)| t.loading.as_ref().is_some_and(|h| h.id() == id))
                .map(|(n, _)| n.clone()),
        };
        let Some(name) = name else { return };
        let Some(t) = self.tracks.get_mut(&name) else {
            return;
        };
        if t.loading.as_ref().map(AbortHandle::id) != Some(id) {
            return; // stale
        }
        t.loading = None;
        match outcome {
            Ok((_, kind, Ok(payload))) => self.swap_in(&name, &kind, payload),
            Ok((_, _, Err(e))) => self.failed(&name, e),
            Err(e) => self.failed(&name, format!("load task failed: {e}")),
        }
    }

    /// The fetched payload replaces the active one, under the latest manifest.
    fn swap_in(&mut self, name: &str, kind: &ServiceKind, payload: Payload) {
        let shared = self.shared.clone();
        let swapped = shared.with_known(|known| {
            let k = known.get_mut(name)?;
            if k.manifest.kind != *kind || k.local == LocalStatus::NotPlaced {
                return None;
            }
            shared.set_active(
                name,
                Active {
                    manifest: k.manifest.clone(),
                    generation: k.generation,
                    payload,
                },
            );
            k.local = LocalStatus::Ready {
                version: k.manifest.version,
            };
            Some(k.manifest.version)
        });
        if let Some(version) = swapped {
            info!(service = name, version, "service ready");
            self.tracks.remove(name);
        }
    }

    fn failed(&mut self, name: &str, error: String) {
        let Some(t) = self.tracks.get_mut(name) else {
            return;
        };
        t.failures = t.failures.saturating_add(1);
        let factor = 1u32 << (t.failures - 1).min(16);
        let delay = self
            .timings
            .retry_min
            .saturating_mul(factor)
            .min(self.timings.retry_max);
        warn!(service = name, error = %error, retry_in = ?delay, "service fetch or load failed");
        t.retry_at = Some(Instant::now() + delay);
        let status = LocalStatus::Failed {
            error,
            retry_at: SystemTime::now() + delay,
        };
        t.failed = Some(status.clone());
        self.shared.with_known(|known| {
            if let Some(k) = known.get_mut(name) {
                k.local = status;
            }
        });
    }

    /// Lets the next scan retry every track whose backoff has elapsed. (Clearing the
    /// deadline here, not in the scan, keeps a failing or paused scan from spinning.)
    fn release_due(&mut self) {
        let now = Instant::now();
        for t in self.tracks.values_mut() {
            if t.retry_at.is_some_and(|r| r <= now) {
                t.retry_at = None;
            }
        }
    }

    /// The earliest pending retry.
    fn next_retry(&self) -> Option<Instant> {
        self.tracks
            .values()
            .filter(|t| t.loading.is_none())
            .filter_map(|t| t.retry_at)
            .min()
    }
}

/// Fetches the payload (a no-op when it is already here) and loads it.
async fn load(shared: &Shared, kind: &ServiceKind) -> Result<Payload, String> {
    let blobs = shared.node.blobs();
    match kind {
        ServiceKind::Wasm { module } => {
            let hash = BlobHash::from(*module);
            fetched(blobs.fetch(hash)).await?;
            owned(shared, hash);
            match blobs.size(hash).await.map_err(|e| e.to_string())? {
                Some(len) => {
                    if let Some(e) = module_too_large(len) {
                        return Err(e);
                    }
                }
                None => return Err("module not stored after fetch".into()),
            }
            let bytes = blobs.read(hash).await.map_err(|e| e.to_string())?;
            let loaded = shared
                .runtime
                .load(*module, &bytes)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Payload::Wasm(loaded))
        }
        ServiceKind::Static { root } => {
            let hash = BlobHash::from(*root);
            fetched(blobs.fetch_collection(hash)).await?;
            owned(shared, hash);
            let site = StaticSite::load(blobs, *root)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Payload::Static(Arc::new(site)))
        }
    }
}

/// Records a fetched module or root as service-owned (blob GC may then remove it once it
/// is unreferenced). A failure only means the blob is never collected.
fn owned(shared: &Shared, hash: BlobHash) {
    if let Err(e) = mark_owned(shared, hash) {
        warn!(hash = %hash.to_hex(), error = %e, "cannot record a service blob as owned");
    }
}

async fn fetched(f: impl Future<Output = Result<(), mesh_core::NodeError>>) -> Result<(), String> {
    match timeout(FETCH_TIMEOUT, f).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("fetch timed out".into()),
    }
}
