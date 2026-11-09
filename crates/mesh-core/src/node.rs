//! The public `Node` (spec §3.2): lifecycle, the supervisor that owns every task, and the
//! read side of membership and peer state.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, Watcher};
use mesh_proto::{MemberInfo, MeshGenesis, MeshId, NodeAddrBytes, NodeId, Role};
use mesh_store::StoreError;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::config::{DynClock, NodeConfig, Timings};
use crate::db::{CoreStore, StoreHandle};
use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::local::{Invites, load_or_create_key, read_genesis, write_genesis};
use crate::membership::{
    MEMBERS, MESH_NS, MembershipSnapshot, MeshAcceptor, addr_key, encode_addr, encode_info,
    info_key, member_bytes,
};
use crate::peers::{PeerEnv, PeerManager, SyncHandler};
use crate::reactor::{Reactor, ReactorCtx};
use crate::transport::{ALPN_SYNC, Gate, addr_to_bytes, build_endpoint};

const STORE_FILE: &str = "store.redb";
const EVENT_CAPACITY: usize = 1024;
/// How long `shutdown` waits for each group of tasks before aborting them.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `shutdown` waits for in-flight blocking store calls to release the store file.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeStatus {
    Running,
    Rebootstrapping {
        waiting_for_self_seq: u64,
    },
    Removed {
        by: NodeId,
    },
    /// Peers closed with `REMOVED`; the record itself has not arrived.
    RefusedAsRemoved {
        by: Vec<NodeId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberStatus {
    pub id: NodeId,
    pub role: Role,
    pub info: Option<MemberInfo>,
    pub effective: bool,
    /// A session runs with this member (always true for this node itself).
    pub online: bool,
    pub rtt_ms: Option<u64>,
    /// Wall time (ms) of the last message received from this member.
    pub last_seen_ms: Option<u64>,
    /// Ops this node holds that the member may lack.
    pub lag: u64,
}

/// Scheduled maintenance jobs (spec §9). `Node::maint_now` runs one at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintJob {
    RetryPending,
    Compact,
    Gc,
    PruneInvites,
}

/// A running mesh node. Call [`Node::shutdown`] to stop it; dropping it only cancels its
/// tasks.
pub struct Node {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: NodeConfig,
    genesis: MeshGenesis,
    me: NodeId,
    endpoint: Endpoint,
    router: Router,
    store: StoreHandle,
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    acc: Arc<MeshAcceptor>,
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    invites: Arc<Mutex<Invites>>,
    membership: Arc<watch::Sender<Arc<MembershipSnapshot>>>,
    peers: PeerManager,
    events: broadcast::Sender<NodeEvent>,
    status: Arc<watch::Sender<NodeStatus>>,
    /// Re-bootstrap pauses serving; whoever clears it must wake every session.
    #[allow(dead_code)]
    serve_paused: Arc<AtomicBool>,
    tasks: Mutex<JoinSet<()>>,
    cancel: CancellationToken,
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    force_lagged: Arc<Notify>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn join_err(e: tokio::task::JoinError) -> NodeError {
    NodeError::Io(std::io::Error::other(e))
}

fn own_info(cfg: &NodeConfig) -> MemberInfo {
    MemberInfo {
        name: cfg.name.clone(),
        platform: cfg.platform.clone(),
        tags: cfg.tags.clone(),
    }
}

/// Runs the task built by `make` and restarts it with backoff if it panics (spec §10).
/// Returns when the task returns normally or is cancelled.
pub(crate) async fn supervise<F, Fut>(
    name: &'static str,
    cancel: CancellationToken,
    timings: Timings,
    mut make: F,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut delay = timings.backoff_min;
    loop {
        match tokio::spawn(make()).await {
            Ok(()) => return,
            Err(e) if e.is_panic() => {
                error!(task = name, "task panicked; restarting");
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = sleep(delay) => {}
                }
                delay = (delay * 2).min(timings.backoff_max);
            }
            Err(_) => return,
        }
    }
}

/// Writes `addr/<me>` if the encoded value differs from the stored one.
async fn publish_addr(
    store: &StoreHandle,
    me: NodeId,
    addr: &EndpointAddr,
) -> Result<(), NodeError> {
    if addr.addrs.is_empty() {
        return Ok(());
    }
    let value = encode_addr(&addr_to_bytes(addr));
    let key = addr_key(&me);
    if store.get(MESH_NS, &key).await?.as_deref() != Some(value.as_slice()) {
        debug!(addr = ?addr, "publishing own address");
        store.set(MESH_NS, &key, value).await?;
    }
    Ok(())
}

async fn publish_addrs(
    endpoint: Endpoint,
    store: StoreHandle,
    me: NodeId,
    cancel: CancellationToken,
) {
    let mut w = endpoint.watch_addr();
    loop {
        if let Err(e) = publish_addr(&store, me, &w.get()).await {
            warn!(error = %e, "could not publish own address");
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = w.updated() => if r.is_err() { return },
        }
    }
}

/// Emits `FirstSyncComplete` the first time any round completes (nothing is persisted).
async fn first_sync(
    mut rx: broadcast::Receiver<NodeEvent>,
    events: broadcast::Sender<NodeEvent>,
    done: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    loop {
        let r = tokio::select! {
            _ = cancel.cancelled() => return,
            r = rx.recv() => r,
        };
        match r {
            Ok(NodeEvent::RoundComplete(_)) => {
                if !done.swap(true, Ordering::SeqCst) {
                    let _ = events.send(NodeEvent::FirstSyncComplete);
                }
                return;
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The re-bootstrap hook (spec §7): sessions report a stale store here.
async fn stale_hook(mut rx: mpsc::Receiver<NodeId>, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = rx.recv() => match r {
                Some(peer) => warn!(
                    peer = %peer.short(),
                    "store is stale; re-bootstrap is not available yet"
                ),
                None => return,
            },
        }
    }
}

/// Starts a node on an established membership: opens the store, binds the endpoint, mounts
/// the gated `sync/1` handler, publishes the address, runs the first rescan and spawns
/// the reactor, the address watcher and the peer tasks.
pub(crate) async fn start(
    cfg: NodeConfig,
    key: SigningKey,
    genesis: MeshGenesis,
    hints: Vec<(NodeId, NodeAddrBytes)>,
) -> Result<Node, NodeError> {
    let me = NodeId(key.verifying_key().to_bytes());
    let mesh = genesis.mesh_id();
    let path = cfg.data_dir.join(STORE_FILE);
    let (k, wall) = (key.clone(), DynClock(cfg.wall.clone()));
    let store = tokio::task::spawn_blocking(move || CoreStore::open(&path, mesh, k, wall))
        .await
        .map_err(join_err)?
        .map_err(|e| match e {
            StoreError::WrongMesh => NodeError::WrongMesh,
            e => NodeError::Store(e),
        })?;
    let store = StoreHandle::new(Arc::new(store));
    let dir = cfg.data_dir.clone();
    let invites = tokio::task::spawn_blocking(move || Invites::load(&dir))
        .await
        .map_err(join_err)??;
    let acc = Arc::new(MeshAcceptor::new(genesis.clone()));
    let g = genesis.clone();
    let snap = store.call(move |s| MembershipSnapshot::load(s, &g)).await?;
    let (membership, membership_rx) = watch::channel(Arc::new(snap));
    let membership = Arc::new(membership);
    let status = Arc::new(watch::channel(NodeStatus::Running).0);
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    let cancel = CancellationToken::new();
    let serve_paused = Arc::new(AtomicBool::new(false));
    let force_lagged = Arc::new(Notify::new());
    let (stale_tx, stale_rx) = mpsc::channel(16);

    if let Some(ms) = store.call(|s| Ok(s.clock_floor_ahead_ms())).await? {
        warn!(ms, "HLC floor is ahead of the wall clock");
        let _ = events.send(NodeEvent::ClockFloorAhead { ms });
    }

    let endpoint = build_endpoint(&cfg, &key).await?;
    let peers = PeerManager::new(
        PeerEnv {
            store: store.clone(),
            acc: acc.clone(),
            mesh,
            me,
            timings: cfg.timings.clone(),
            discovery: cfg.discovery,
            endpoint: endpoint.clone(),
            events: events.clone(),
            serve_paused: serve_paused.clone(),
            stale: stale_tx,
        },
        cancel.child_token(),
    );
    for (id, addr) in hints {
        peers.add_hint(id, addr);
    }
    let router = Router::builder(endpoint.clone())
        .accept(
            ALPN_SYNC,
            Gate::new(SyncHandler(peers.clone()), membership_rx),
        )
        .spawn();

    if let Err(e) = publish_addr(&store, me, &endpoint.addr()).await {
        warn!(error = %e, "could not publish own address");
    }

    let rctx = ReactorCtx {
        store: store.clone(),
        acc: acc.clone(),
        genesis: genesis.clone(),
        me,
        membership: membership.clone(),
        status: status.clone(),
        peers: peers.clone(),
        events: events.clone(),
        force_lagged: force_lagged.clone(),
    };
    // Subscribed before the first rescan starts any peer, so no round is missed.
    let mut first_rx = Some(events.subscribe());
    let mut reactor = Reactor::new(rctx.clone());
    reactor.rescan_logged(false).await;
    let mut reactor = Some(reactor);

    let mut tasks = JoinSet::new();
    let t = cfg.timings.clone();
    let c = cancel.clone();
    tasks.spawn(supervise("reactor", cancel.clone(), t.clone(), move || {
        // The first run keeps the startup rescan's state; a restart starts afresh.
        let r = reactor.take().unwrap_or_else(|| Reactor::new(rctx.clone()));
        r.run(c.clone())
    }));
    let (ep, st, c) = (endpoint.clone(), store.clone(), cancel.clone());
    tasks.spawn(supervise("addr", cancel.clone(), t.clone(), move || {
        publish_addrs(ep.clone(), st.clone(), me, c.clone())
    }));
    let (ev, done, c) = (
        events.clone(),
        Arc::new(AtomicBool::new(false)),
        cancel.clone(),
    );
    tasks.spawn(supervise("first-sync", cancel.clone(), t, move || {
        let rx = first_rx.take().unwrap_or_else(|| ev.subscribe());
        first_sync(rx, ev.clone(), done.clone(), c.clone())
    }));
    tasks.spawn(stale_hook(stale_rx, cancel.clone()));

    Ok(Node {
        inner: Arc::new(Inner {
            cfg,
            genesis,
            me,
            endpoint,
            router,
            store,
            acc,
            invites: Arc::new(Mutex::new(invites)),
            membership,
            peers,
            events,
            status,
            serve_paused,
            tasks: Mutex::new(tasks),
            cancel,
            force_lagged,
        }),
    })
}

impl Node {
    /// Founds a new mesh in an empty data directory (spec §5.3 "Founder").
    pub async fn create_mesh(cfg: NodeConfig) -> Result<Node, NodeError> {
        if read_genesis(&cfg.data_dir)?.is_some() {
            return Err(NodeError::AlreadyInMesh);
        }
        let info = own_info(&cfg);
        info.validate()?;
        let key = load_or_create_key(&cfg.data_dir)?;
        let me = NodeId(key.verifying_key().to_bytes());
        let genesis = MeshGenesis {
            founder: me,
            nonce: rand::random(),
        };
        write_genesis(&cfg.data_dir, &genesis)?;
        let node = start(cfg, key, genesis, Vec::new()).await?;
        let store = node.store().clone();
        let r = async {
            store
                .set_add(MESH_NS, MEMBERS, member_bytes(&me, Role::Host).to_vec())
                .await?;
            store.set(MESH_NS, &info_key(&me), encode_info(&info)).await
        }
        .await;
        match r {
            Ok(()) => Ok(node),
            Err(e) => {
                node.shutdown().await;
                Err(e)
            }
        }
    }

    /// Opens an existing data directory.
    pub async fn open(cfg: NodeConfig) -> Result<Node, NodeError> {
        let genesis = read_genesis(&cfg.data_dir)?.ok_or(NodeError::NotInitialised)?;
        let key = load_or_create_key(&cfg.data_dir)?;
        start(cfg, key, genesis, Vec::new()).await
    }

    pub fn node_id(&self) -> NodeId {
        self.inner.me
    }

    pub fn mesh(&self) -> MeshId {
        self.inner.genesis.mesh_id()
    }

    /// Every member of the mesh, with this node's view of its link.
    pub fn members(&self) -> Vec<MemberStatus> {
        let snap = self.inner.membership.borrow().clone();
        let peers = &self.inner.peers;
        snap.members()
            .map(|(id, m)| {
                let mut s = MemberStatus {
                    id: *id,
                    role: m.role,
                    info: m.info.clone(),
                    effective: snap.is_effective(id),
                    online: *id == self.inner.me,
                    rtt_ms: None,
                    last_seen_ms: None,
                    lag: 0,
                };
                if *id != self.inner.me {
                    if let Some(rtt) = peers.link(id) {
                        s.online = true;
                        s.rtt_ms = rtt;
                    }
                    if let Some(mx) = peers.metrics(id) {
                        let seen = mx.last_seen_ms.load(Ordering::SeqCst);
                        s.last_seen_ms = (seen != 0).then_some(seen);
                        s.lag = mx.lag.load(Ordering::SeqCst);
                    }
                }
                s
            })
            .collect()
    }

    pub fn status(&self) -> NodeStatus {
        self.inner.status.borrow().clone()
    }

    /// Reads and local writes; local writes fail with `Removed` once this node is removed.
    pub fn store(&self) -> &StoreHandle {
        &self.inner.store
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.inner.events.subscribe()
    }

    /// Starts a round with every connected peer (Android alarm hook).
    pub fn sync_now(&self) {
        self.inner.peers.start_round_all();
    }

    /// The network changed (Android hook): tell iroh and redial every peer now.
    pub fn network_change(&self) {
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let ep = self.inner.endpoint.clone();
            rt.spawn(async move { ep.network_change().await });
        }
        self.inner.peers.reset_backoff_all();
    }

    /// Changes this node's own name and/or tags in `info/<self>`.
    pub async fn set_info(
        &self,
        name: Option<String>,
        tags: Option<Vec<String>>,
    ) -> Result<(), NodeError> {
        let me = self.inner.me;
        let cur = self
            .inner
            .membership
            .borrow()
            .info(&me)
            .cloned()
            .unwrap_or_else(|| own_info(&self.inner.cfg));
        let info = MemberInfo {
            name: name.unwrap_or(cur.name),
            platform: self.inner.cfg.platform.clone(),
            tags: tags.unwrap_or(cur.tags),
        };
        info.validate()?;
        self.store()
            .set(MESH_NS, &info_key(&me), encode_info(&info))
            .await
    }

    /// Replaces the tags of member `id` (any member may change tags).
    pub async fn set_tags_of(&self, id: NodeId, tags: Vec<String>) -> Result<(), NodeError> {
        let mut info = {
            let snap = self.inner.membership.borrow();
            if !snap.is_effective(&id) {
                return Err(NodeError::NotEffective);
            }
            snap.info(&id).cloned().unwrap_or(MemberInfo {
                name: String::new(),
                platform: String::new(),
                tags: Vec::new(),
            })
        };
        info.tags = tags;
        info.validate()?;
        self.store()
            .set(MESH_NS, &info_key(&id), encode_info(&info))
            .await
    }

    /// Stops every task, closes every connection and the endpoint, and releases the store
    /// file, so the data directory can be opened again once this returns.
    pub async fn shutdown(self) {
        let inner = self.inner.clone();
        drop(self); // cancels the node's token
        let mut tasks = std::mem::take(&mut *lock(&inner.tasks));
        let all = async { while tasks.join_next().await.is_some() {} };
        if timeout(STOP_TIMEOUT, all).await.is_err() {
            warn!("node tasks did not stop in time; aborting them");
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        inner.peers.join().await;
        match timeout(STOP_TIMEOUT, inner.router.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "router shutdown failed"),
            Err(_) => warn!("router shutdown timed out"),
        }
        let store = Arc::downgrade(inner.store.inner());
        drop(inner);
        // A blocking store call still in flight holds the store open until it returns.
        let deadline = Instant::now() + RELEASE_TIMEOUT;
        while store.strong_count() > 0 && Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
        }
        if store.strong_count() > 0 {
            warn!("store still in use after shutdown");
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.inner.cancel.cancel();
    }
}

#[cfg(any(test, feature = "test-hooks"))]
impl Node {
    /// Starts a member without `join` (the founder adds its `members` record itself).
    pub async fn start_member_for_test(
        cfg: NodeConfig,
        genesis: MeshGenesis,
        hints: Vec<(NodeId, NodeAddrBytes)>,
    ) -> Result<Node, NodeError> {
        let key = load_or_create_key(&cfg.data_dir)?;
        if read_genesis(&cfg.data_dir)?.is_none() {
            write_genesis(&cfg.data_dir, &genesis)?;
        }
        start(cfg, key, genesis, hints).await
    }

    /// Closes connections to, and refuses dials and accepts from, `ids`.
    pub fn partition(&self, ids: &[NodeId]) {
        self.inner.peers.partition(ids);
    }

    pub fn heal(&self) {
        self.inner.peers.heal();
    }

    /// Runs one maintenance job now.
    pub async fn maint_now(&self, job: MaintJob) -> Result<(), NodeError> {
        let store = &self.inner.store;
        match job {
            MaintJob::RetryPending => {
                let acc = self.inner.acc.clone();
                store.call(move |s| s.retry_pending(&*acc)).await.map(drop)
            }
            MaintJob::Compact => store.call(|s| s.compact()).await.map(drop),
            MaintJob::Gc => store.call(|s| s.gc()).await.map(drop),
            MaintJob::PruneInvites => {
                let invites = self.inner.invites.clone();
                let now = self.inner.cfg.wall.now_ms();
                tokio::task::spawn_blocking(move || lock(&invites).prune(now))
                    .await
                    .map_err(join_err)?
            }
        }
    }

    /// Makes the reactor behave as if its change subscription lagged (a full rescan with
    /// the applied-cutoff cache cleared).
    pub fn force_lagged(&self) {
        self.inner.force_lagged.notify_one();
    }

    /// An extra dial address for a peer, as a join reply would give.
    pub fn add_hint(&self, id: NodeId, addr: NodeAddrBytes) {
        self.inner.peers.add_hint(id, addr);
    }

    /// Open `sync/1` connections this node holds.
    pub fn connection_count(&self) -> usize {
        self.inner.peers.connection_count()
    }
}
