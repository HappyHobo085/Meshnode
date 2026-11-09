//! The public `Node` (spec §3.2): lifecycle, the supervisor that owns every task, and the
//! read side of membership and peer state.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, Watcher};
use mesh_proto::{
    InviteTicket, MemberInfo, MeshGenesis, MeshId, NodeAddrBytes, NodeId, RemovalRecord, Role,
};
use mesh_store::StoreError;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::blobs::{Blobs, PeerSource};
use crate::config::{DynClock, NodeConfig, Timings};
use crate::db::{CoreStore, StoreHandle};
use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::join::{JoinHandler, JoinInner};
use crate::local::{
    Invites, load_or_create_key, read_genesis, read_rebootstrap, remove_rebootstrap, write_genesis,
};
use crate::membership::{
    MEMBERS, MESH_NS, MembershipSnapshot, MeshAcceptor, REMOVED, addr_key, encode_addr,
    encode_info, encode_removal, info_key, member_bytes,
};
use crate::peers::{PeerEnv, PeerManager, SignOut, SyncHandler};
use crate::reactor::{Reactor, ReactorCtx};
use crate::transport::{ALPN_JOIN, ALPN_SYNC, Gate, addr_to_bytes, build_endpoint};

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

/// A running mesh node. Call [`Node::shutdown`] to stop it; dropping it only cancels its
/// tasks.
pub struct Node {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) cfg: NodeConfig,
    pub(crate) genesis: MeshGenesis,
    pub(crate) me: NodeId,
    pub(crate) endpoint: Endpoint,
    router: Router,
    blobs: Blobs,
    pub(crate) store: StoreHandle,
    pub(crate) acc: Arc<MeshAcceptor>,
    pub(crate) invites: Arc<Mutex<Invites>>,
    join: Arc<JoinInner>,
    /// Sticky: true once any round has completed since start.
    first_sync: Arc<watch::Sender<bool>>,
    pub(crate) membership: Arc<watch::Sender<Arc<MembershipSnapshot>>>,
    pub(crate) peers: PeerManager,
    pub(crate) events: broadcast::Sender<NodeEvent>,
    pub(crate) status: Arc<watch::Sender<NodeStatus>>,
    /// Re-bootstrap pauses serving; whoever clears it must wake every session.
    pub(crate) serve_paused: Arc<AtomicBool>,
    /// Held for a whole re-bootstrap run: one at a time.
    pub(crate) rebootstrap: tokio::sync::Mutex<()>,
    /// Test hook: the next re-bootstrap stops the node right after its step 3.
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    pub(crate) crash_after_reset: AtomicBool,
    tasks: Mutex<JoinSet<()>>,
    pub(crate) cancel: CancellationToken,
    /// Makes the reactor run a full rescan (a test hook, and the end of a re-bootstrap).
    pub(crate) force_lagged: Arc<Notify>,
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    lose_events: Arc<AtomicBool>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn join_err(e: tokio::task::JoinError) -> NodeError {
    NodeError::Io(std::io::Error::other(e))
}

pub(crate) fn own_info(cfg: &NodeConfig) -> MemberInfo {
    MemberInfo {
        name: cfg.name.clone(),
        platform: cfg.platform.clone(),
        tags: cfg.tags.clone(),
    }
}

/// Aborts the task when dropped.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
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
        // Aborting this supervisor (a timed-out shutdown) drops the guard, which aborts
        // the inner task too, so nothing it holds (the store) outlives the abort.
        let mut task = AbortOnDrop(tokio::spawn(make()));
        match (&mut task.0).await {
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
pub(crate) async fn publish_addr(
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
/// It also ends a joiner's provisional targets and triggers the rescan that drops those
/// the now complete membership does not list.
async fn first_sync(
    mut rx: broadcast::Receiver<NodeEvent>,
    events: broadcast::Sender<NodeEvent>,
    done: Arc<AtomicBool>,
    sticky: Arc<watch::Sender<bool>>,
    peers: PeerManager,
    rescan: Arc<Notify>,
    cancel: CancellationToken,
) {
    loop {
        let r = tokio::select! {
            _ = cancel.cancelled() => return,
            r = rx.recv() => r,
        };
        match r {
            Ok(NodeEvent::RoundComplete(_)) => {
                peers.end_provisional();
                rescan.notify_one();
                sticky.send_replace(true);
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
    let invites = Arc::new(Mutex::new(invites));
    let acc = Arc::new(MeshAcceptor::new(genesis.clone()));
    let g = genesis.clone();
    let snap = store
        .call_raw(move |s| MembershipSnapshot::load(s, &g))
        .await?;
    // Spec §7: a re-bootstrap is due at open if one was interrupted or the store is stale.
    // A removed node has nothing to rebuild (an interrupted run is dropped). Serving is
    // paused before any session starts.
    let removed_self = snap.removed_by(&me).is_some();
    let dir = cfg.data_dir.clone();
    let pending = tokio::task::spawn_blocking(move || read_rebootstrap(&dir))
        .await
        .map_err(join_err)??;
    if removed_self && pending.is_some() {
        warn!("this node was removed; dropping its interrupted re-bootstrap");
        remove_rebootstrap(&cfg.data_dir)?;
    }
    let rebootstrap_due =
        !removed_self && (pending.is_some() || store.call_raw(|s| s.is_stale()).await?);
    // The remembered peers of a resumed run: the emptied store holds no addresses.
    let resume_peers: Vec<(NodeId, NodeAddrBytes)> = pending
        .filter(|_| rebootstrap_due)
        .map(|f| f.peers)
        .unwrap_or_default();
    store.refuse_writes_rebootstrapping(rebootstrap_due);
    let (membership, membership_rx) = watch::channel(Arc::new(snap));
    let membership = Arc::new(membership);
    let status = Arc::new(watch::channel(NodeStatus::Running).0);
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    let cancel = CancellationToken::new();
    let serve_paused = Arc::new(AtomicBool::new(rebootstrap_due));
    let force_lagged = Arc::new(Notify::new());
    let lose_events = Arc::new(AtomicBool::new(false));
    let signout = Arc::new(SignOut::default());
    let (stale_tx, stale_rx) = mpsc::channel(16);

    if let Some(ms) = store.call_raw(|s| Ok(s.clock_floor_ahead_ms())).await? {
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
            status: status.clone(),
            signout: signout.clone(),
            serve_paused: serve_paused.clone(),
            stale: stale_tx,
        },
        cancel.child_token(),
    );
    for (id, addr) in hints.into_iter().chain(resume_peers.iter().cloned()) {
        peers.add_hint(id, addr);
    }
    let join = Arc::new(JoinInner::new(
        genesis.clone(),
        me,
        store.clone(),
        invites.clone(),
        cfg.wall.clone(),
        status.clone(),
        endpoint.clone(),
    ));
    let blobs = Blobs::open(
        &cfg.data_dir,
        &endpoint,
        PeerSource {
            membership: membership.clone(),
            peers: peers.clone(),
            me,
        },
    )
    .await?;
    let router = Router::builder(endpoint.clone())
        .accept(
            ALPN_SYNC,
            Gate::new(SyncHandler(peers.clone()), membership_rx.clone())
                .with_provisional(peers.provisional()),
        )
        .accept(
            iroh_blobs::ALPN,
            Gate::new(blobs.protocol(), membership_rx.clone()),
        )
        // Not gated: a joiner is not a member yet.
        .accept(ALPN_JOIN, JoinHandler(join.clone()))
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
        lose_events: lose_events.clone(),
        signout: signout.clone(),
        rebootstrapping: serve_paused.clone(),
    };
    // Subscribed before the first rescan starts any peer, so no round is missed.
    let mut first_rx = Some(events.subscribe());
    let mut reactor = Reactor::new(rctx.clone());
    reactor.rescan_logged(false).await;
    let resume_ids: Vec<NodeId> = resume_peers.iter().map(|(id, _)| *id).collect();
    peers.add_targets(&resume_ids);
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
    let first_sync_flag = Arc::new(watch::channel(false).0);
    let fsf = first_sync_flag.clone();
    let (ev, done, pm, fl, c) = (
        events.clone(),
        Arc::new(AtomicBool::new(false)),
        peers.clone(),
        force_lagged.clone(),
        cancel.clone(),
    );
    tasks.spawn(supervise("first-sync", cancel.clone(), t, move || {
        let rx = first_rx.take().unwrap_or_else(|| ev.subscribe());
        first_sync(
            rx,
            ev.clone(),
            done.clone(),
            fsf.clone(),
            pm.clone(),
            fl.clone(),
            c.clone(),
        )
    }));

    let inner = Arc::new(Inner {
        cfg,
        genesis,
        me,
        endpoint,
        router,
        blobs,
        store,
        acc,
        invites,
        join,
        first_sync: first_sync_flag,
        membership,
        peers,
        events,
        status,
        serve_paused,
        rebootstrap: tokio::sync::Mutex::new(()),
        crash_after_reset: AtomicBool::new(false),
        tasks: Mutex::new(tasks),
        cancel,
        force_lagged,
        lose_events,
    });
    let (stale, i, c) = (
        Arc::new(tokio::sync::Mutex::new(stale_rx)),
        inner.clone(),
        inner.cancel.clone(),
    );
    let mut due = Some(rebootstrap_due);
    lock(&inner.tasks).spawn(supervise(
        "rebootstrap",
        c,
        inner.cfg.timings.clone(),
        move || {
            // A restart after a panic checks again whether a re-bootstrap is due.
            crate::rebootstrap::driver(i.clone(), stale.clone(), due.take().unwrap_or(true))
        },
    ));
    let (i, c) = (inner.clone(), inner.cancel.clone());
    lock(&inner.tasks).spawn(supervise(
        "maint",
        c,
        inner.cfg.timings.clone(),
        move || crate::maint::run(i.clone()),
    ));
    Ok(Node { inner })
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

    /// Joins the mesh a ticket belongs to (spec §5.3).
    pub async fn join(cfg: NodeConfig, ticket: InviteTicket) -> Result<Node, NodeError> {
        crate::join::join(cfg, ticket).await
    }

    /// Issues a single-use invite ticket valid for `ttl` (spec §5.2).
    pub async fn invite(&self, role: Role, ttl: Duration) -> Result<InviteTicket, NodeError> {
        self.inner.join.invite(role, ttl).await
    }

    /// After a join: dials the inviter and the peers its reply listed, admitting them before
    /// the membership lists them, until the first sync completes (spec §5.3 step 5).
    pub(crate) fn add_join_peers(&self, ids: &[NodeId]) {
        self.inner.peers.add_provisional(ids);
    }

    /// After a join: writes `info/<self>` and `addr/<self>` once the first round completed.
    pub(crate) fn spawn_publish_own(&self) {
        let i = &self.inner;
        lock(&i.tasks).spawn(crate::join::publish_own(
            i.events.subscribe(),
            i.membership.subscribe(),
            i.store.clone(),
            i.endpoint.clone(),
            i.me,
            own_info(&i.cfg),
            i.cancel.clone(),
        ));
    }

    /// Opens an existing data directory.
    pub async fn open(cfg: NodeConfig) -> Result<Node, NodeError> {
        let genesis = read_genesis(&cfg.data_dir)?.ok_or(NodeError::NotInitialised)?;
        let key = load_or_create_key(&cfg.data_dir)?;
        start(cfg, key, genesis, Vec::new()).await
    }

    /// Whether a sync round has completed since this node started (sticky; no race with
    /// `subscribe`).
    pub fn first_sync_complete(&self) -> bool {
        *self.inner.first_sync.borrow()
    }

    /// By how many ms the store's HLC floor is ahead of the wall clock beyond the allowed
    /// skew, read live (the state behind `NodeEvent::ClockFloorAhead`, which is emitted
    /// once at open and so is easily missed).
    pub async fn clock_floor_ahead_ms(&self) -> Option<u64> {
        self.inner
            .store
            .call_raw(|s| Ok(s.clock_floor_ahead_ms()))
            .await
            .ok()
            .flatten()
    }

    /// Waits until [`Node::first_sync_complete`] holds; false if `limit` passes first.
    pub async fn wait_first_sync(&self, limit: Duration) -> bool {
        let mut rx = self.inner.first_sync.subscribe();
        timeout(limit, rx.wait_for(|done| *done))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// The blob store (spec §8).
    pub fn blobs(&self) -> &Blobs {
        &self.inner.blobs
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

    /// Removes member `id` (spec §4). Takes effect on each node once it receives the
    /// record; the resulting `_mesh` change drives the rescan that applies it here.
    pub async fn remove(&self, id: NodeId) -> Result<(), NodeError> {
        let (genesis, me) = (self.inner.genesis.clone(), self.inner.me);
        let (snap, vv) = self
            .inner
            .store
            .call_raw(move |s| Ok((MembershipSnapshot::load(s, &genesis)?, s.version_vector()?)))
            .await?;
        if !snap.is_effective(&me) || !snap.is_effective(&id) {
            return Err(NodeError::NotEffective);
        }
        if snap.effective_hosts() == [id] {
            return Err(NodeError::LastHost);
        }
        let record = RemovalRecord {
            node_id: id,
            cutoff_seq: vv.get(&id),
        };
        self.inner
            .store
            .set_add(MESH_NS, REMOVED, encode_removal(&record))
            .await
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
        // The router already asked the blobs handler to stop; make sure the store is closed.
        if timeout(STOP_TIMEOUT, inner.blobs.shutdown()).await.is_err() {
            warn!("blob store shutdown timed out");
        }
        let store = Arc::downgrade(inner.store.raw());
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

    /// This node's current iroh address.
    pub fn endpoint_addr_for_test(&self) -> EndpointAddr {
        self.inner.endpoint.addr()
    }

    /// The next join this node accepts is applied, but its reply is never sent.
    pub fn drop_next_join_reply(&self) {
        self.inner.join.drop_next_reply();
    }

    /// Closes connections to, and refuses dials and accepts from, `ids`.
    pub fn partition(&self, ids: &[NodeId]) {
        self.inner.peers.partition(ids);
    }

    pub fn heal(&self) {
        self.inner.peers.heal();
    }

    /// Runs one maintenance job now.
    pub async fn maint_now(&self, job: crate::maint::MaintJob) -> Result<(), NodeError> {
        crate::maint::run_job(&self.inner, job).await
    }

    /// Makes the reactor behave as if its change subscription lagged (a full rescan with
    /// the applied-cutoff cache cleared).
    pub fn force_lagged(&self) {
        self.inner.force_lagged.notify_one();
    }

    /// While `on`, the reactor discards the store change events as if they were lost
    /// (only `force_lagged` or a later event brings the rescan).
    pub fn lose_events(&self, on: bool) {
        self.inner.lose_events.store(on, Ordering::SeqCst);
    }

    /// Hand-delivers to this node every op `other` holds that it lacks, as one sync round
    /// would, without any connection. The reactor then reacts as usual.
    pub async fn deliver_from(&self, other: &Node) -> Result<(), NodeError> {
        let mine = self.inner.store.call_raw(|s| s.version_vector()).await?;
        let (items, theirs) = other
            .inner
            .store
            .call_raw(move |s| Ok((s.diff_since(&mine, 100_000)?, s.version_vector()?)))
            .await?;
        let acc = self.inner.acc.clone();
        self.inner
            .store
            .call_raw(move |s| {
                let mut ops = Vec::new();
                for item in items {
                    match item {
                        mesh_store::SyncItem::Op(op) => ops.push(op),
                        mesh_store::SyncItem::Covered { origin, upto } => {
                            s.apply_batch(std::mem::take(&mut ops), &*acc)?;
                            s.apply_covered(origin, upto, theirs.get(&origin), &*acc)?;
                        }
                    }
                }
                s.apply_batch(ops, &*acc).map(drop)
            })
            .await
    }

    /// An extra dial address for a peer, as a join reply would give.
    pub fn add_hint(&self, id: NodeId, addr: NodeAddrBytes) {
        self.inner.peers.add_hint(id, addr);
    }

    /// Writes `bytes` as they are (a whole frame, length prefix included) on the session
    /// with `peer`, between two of its frames. False if no session runs with `peer`.
    pub fn send_raw_sync_frame(&self, peer: NodeId, bytes: Vec<u8>) -> bool {
        self.inner.peers.send_raw(&peer, bytes)
    }

    /// The application close code `peer` gave when the last session with it ended
    /// (`None` if it ended otherwise, or none has ended yet).
    pub fn last_peer_close_code(&self, peer: NodeId) -> Option<u64> {
        self.inner.peers.peer_close_code(&peer)
    }

    /// Arms a one-shot crash: the next re-bootstrap stops this node (cancels every task)
    /// right after its step 3, leaving `rebootstrap.bin` and the reset store behind.
    pub fn crash_after_reset(&self) {
        self.inner.crash_after_reset.store(true, Ordering::SeqCst);
    }

    /// Whether this node's tasks were stopped (by `shutdown`, a drop or a test crash).
    pub fn is_stopped(&self) -> bool {
        self.inner.cancel.is_cancelled()
    }

    /// Open `sync/1` connections this node holds.
    pub fn connection_count(&self) -> usize {
        self.inner.peers.connection_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborting_supervise_aborts_the_inner_task() {
        let held = Arc::new(());
        let h = held.clone();
        let sup = tokio::spawn(supervise(
            "test",
            CancellationToken::new(),
            Timings::fast_for_tests(),
            move || {
                let h = h.clone();
                async move {
                    let _h = h;
                    std::future::pending::<()>().await;
                }
            },
        ));
        // The factory's clone plus the running task's.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&held) < 3 && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(Arc::strong_count(&held), 3, "inner task running");
        sup.abort();
        let _ = sup.await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&held) > 1 && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            Arc::strong_count(&held),
            1,
            "inner task aborted with its supervisor"
        );
    }
}
