//! `PeerManager` (spec §6.1): one task per target peer that keeps **one** `sync/1`
//! connection to it and runs the session on it.
//!
//! Each slot task dials with backoff (`backoff_min` doubling to `backoff_max`, reset after a
//! session that completed a round and by `reset_backoff_all`), takes incoming connections
//! handed over by the gated `SyncHandler`, and keeps at most one of them: when two
//! connections exist between a pair, the one dialled by the lower `NodeId` survives and the
//! other is closed with `SHUTDOWN`. Both sides apply the same rule, so they agree. The
//! exception is a connection arriving more than `DUPLICATE_GRACE` after the current one
//! started: the peer has lost the current one (a restart or a crash), so the newer wins.
//!
//! Slot state the rest of the node touches (address, hint, handed-over connections,
//! requests) lives behind a mutex in the [`Slot`]; the task is woken through `kick`, a
//! `Notify` whose stored permit means no request is lost while the task is busy.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use iroh::endpoint::{Connection, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use mesh_proto::{MeshId, NodeAddrBytes, NodeId, VersionVector};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::config::Timings;
use crate::db::StoreHandle;
use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::membership::MeshAcceptor;
use crate::node::{NodeStatus, supervise};
use slot::run_slot;

mod slot;
use crate::sync::{PeerMetrics, SessionCmd};
use crate::transport::{Provisional, SHUTDOWN, addr_from_bytes, endpoint_id_of, node_id_of};

/// Upper bound on one dial attempt.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Two connections between a pair that start less than this apart are a simultaneous
/// dial, settled by the lower-`NodeId` rule. A connection arriving later means the peer
/// lost the current one (a restart or a crash), so the newer one wins (spec §6.1).
const DUPLICATE_GRACE: Duration = Duration::from_secs(2);
/// How long `join` waits for the slot tasks before aborting them.
const JOIN_TIMEOUT: Duration = Duration::from_secs(3);

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Sign-out in progress (spec §4): this node wrote its own removal record, so it keeps its
/// sessions until a peer proves it holds the record, by closing with `REMOVED` or by a
/// `Hello` whose version vector covers `seq`. The reactor then stops the node.
#[derive(Default)]
pub(crate) struct SignOut {
    /// This node's highest seq when the sign-out was noticed; 0 = not signing out.
    seq: AtomicU64,
    proved: Notify,
}

impl SignOut {
    pub fn begin(&self, seq: u64) {
        self.seq.store(seq.max(1), Ordering::SeqCst);
    }

    pub fn begun(&self) -> bool {
        self.seq.load(Ordering::SeqCst) != 0
    }

    /// A peer's `Hello` advertised `vv`.
    pub fn peer_hello(&self, me: &NodeId, vv: &VersionVector) {
        let seq = self.seq.load(Ordering::SeqCst);
        if seq != 0 && vv.get(me) >= seq {
            self.proved.notify_one();
        }
    }

    /// A peer closed with `REMOVED`.
    pub fn peer_refused(&self) {
        if self.begun() {
            self.proved.notify_one();
        }
    }

    pub async fn proved(&self) {
        self.proved.notified().await;
    }
}

/// What every session of this node shares.
pub(crate) struct PeerEnv {
    pub store: StoreHandle,
    pub acc: Arc<MeshAcceptor>,
    pub mesh: MeshId,
    pub me: NodeId,
    pub timings: Timings,
    pub discovery: bool,
    pub endpoint: Endpoint,
    pub events: broadcast::Sender<NodeEvent>,
    /// Peers that close with `REMOVED` are recorded here (spec §4).
    pub status: Arc<watch::Sender<NodeStatus>>,
    pub signout: Arc<SignOut>,
    pub serve_paused: Arc<AtomicBool>,
    /// A session found the store stale (spec §7); the re-bootstrap hook.
    pub stale: mpsc::Sender<NodeId>,
}

#[derive(Clone)]
pub struct PeerManager(Arc<Shared>);

impl std::fmt::Debug for PeerManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerManager")
            .field("me", &self.0.env.me.short())
            .finish_non_exhaustive()
    }
}

struct Shared {
    env: PeerEnv,
    cancel: CancellationToken,
    st: Mutex<PmState>,
    /// A joiner's provisional targets (see [`Provisional`]); `set_targets` keeps them.
    provisional: Provisional,
}

#[derive(Default)]
struct PmState {
    slots: HashMap<NodeId, Arc<Slot>>,
    hints: HashMap<NodeId, NodeAddrBytes>,
    /// Test partitions: no dials, accepts or sessions with these peers.
    blocked: HashSet<NodeId>,
    /// This node was removed: no targets any more (spec §3.3 step 6).
    stopped: bool,
    tasks: JoinSet<()>,
    /// Every connection handed to a slot, pruned once closed (`connection_count`).
    conns: Vec<Connection>,
}

struct Slot {
    peer: NodeId,
    token: CancellationToken,
    kick: Notify,
    /// The session's store-change wake-up; persists across sessions.
    wake: Arc<Notify>,
    metrics: Arc<PeerMetrics>,
    st: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    /// From `_mesh/addr/<hex>`.
    addr: Option<NodeAddrBytes>,
    /// From a join reply or a caller.
    hint: Option<NodeAddrBytes>,
    /// Incoming connections not yet looked at by the task.
    incoming: Vec<Connection>,
    reset_backoff: bool,
    close: Option<VarInt>,
    /// The connection the session runs on.
    live: Option<Connection>,
    cmds: Option<mpsc::Sender<SessionCmd>>,
    /// The application close code the peer gave when the last session ended, if any.
    #[cfg(any(test, feature = "test-hooks"))]
    peer_close: Option<u64>,
}

/// The address to dial `peer` at, built from the known address records. Without any, the
/// target is still dialled when discovery can resolve it.
fn dial_target(
    peer: NodeId,
    records: impl IntoIterator<Item = Option<NodeAddrBytes>>,
    discovery: bool,
) -> Option<EndpointAddr> {
    let id = endpoint_id_of(&peer).ok()?;
    let mut target = EndpointAddr::new(id);
    for bytes in records.into_iter().flatten() {
        match addr_from_bytes(&bytes) {
            Ok(a) if a.id == id => target.addrs.extend(a.addrs),
            Ok(_) => warn!(peer = %peer.short(), "address for another node ignored"),
            Err(e) => debug!(peer = %peer.short(), error = %e, "bad address ignored"),
        }
    }
    (!target.addrs.is_empty() || discovery).then_some(target)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every guarded value stays consistent across a panic (plain field writes).
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PmState {
    /// Remembers `conn` and forgets connections that have closed since.
    fn track(&mut self, conn: Connection) {
        self.conns.retain(|c| c.close_reason().is_none());
        self.conns.push(conn);
    }
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, SlotState> {
        lock(&self.st)
    }
}

impl PeerManager {
    pub(crate) fn new(env: PeerEnv, cancel: CancellationToken) -> PeerManager {
        PeerManager(Arc::new(Shared {
            env,
            cancel,
            st: Mutex::new(PmState::default()),
            provisional: Provisional::default(),
        }))
    }

    /// The provisional set this manager keeps targets for; the `sync/1` gate admits it too.
    pub(crate) fn provisional(&self) -> Provisional {
        self.0.provisional.clone()
    }

    /// A joiner's inviter and listed peers (spec §5.3 step 5): dialled, and kept as targets
    /// and admitted by the gate although the snapshot does not list them, until
    /// [`PeerManager::end_provisional`]. A no-op once that was called.
    pub fn add_provisional(&self, ids: &[NodeId]) {
        let mut st = self.lock();
        if st.stopped || !self.0.provisional.add(ids) {
            return;
        }
        for id in ids {
            if *id != self.0.env.me && !st.slots.contains_key(id) {
                self.spawn_slot(&mut st, *id, None);
            }
        }
    }

    /// The first sync completed: provisional targets are no longer kept; the next
    /// `set_targets` (the caller triggers a rescan) drops those that are not members.
    pub fn end_provisional(&self) {
        self.0.provisional.end();
    }

    fn lock(&self) -> MutexGuard<'_, PmState> {
        lock(&self.0.st)
    }

    /// Replaces the target set: new peers get a task, dropped peers' tasks stop (closing
    /// with `SHUTDOWN`), and a changed address resets that peer's backoff. Provisional
    /// targets (see [`PeerManager::add_provisional`]) are kept.
    pub fn set_targets(&self, targets: Vec<(NodeId, Option<NodeAddrBytes>)>) {
        let mut st = self.lock();
        if st.stopped {
            return;
        }
        let me = self.0.env.me;
        let want: HashMap<NodeId, Option<NodeAddrBytes>> =
            targets.into_iter().filter(|(id, _)| *id != me).collect();
        let prov = &self.0.provisional;
        st.slots.retain(|id, slot| {
            let keep = want.contains_key(id) || prov.contains(id);
            if !keep {
                slot.token.cancel();
            }
            keep
        });
        for (id, addr) in want {
            match st.slots.get(&id) {
                Some(slot) => {
                    let mut s = slot.lock();
                    if s.addr != addr {
                        s.addr = addr;
                        s.reset_backoff = true;
                        drop(s);
                        slot.kick.notify_one();
                    }
                }
                None => {
                    self.spawn_slot(&mut st, id, addr);
                }
            }
        }
    }

    /// Adds targets without dropping or changing any (the remembered peers of a resumed
    /// re-bootstrap, spec §7); their addresses come from hints.
    pub fn add_targets(&self, ids: &[NodeId]) {
        let mut st = self.lock();
        if st.stopped {
            return;
        }
        for id in ids {
            if *id != self.0.env.me && !st.slots.contains_key(id) {
                self.spawn_slot(&mut st, *id, None);
            }
        }
    }

    fn spawn_slot(&self, st: &mut PmState, peer: NodeId, addr: Option<NodeAddrBytes>) -> Arc<Slot> {
        let slot = Arc::new(Slot {
            peer,
            token: self.0.cancel.child_token(),
            kick: Notify::new(),
            wake: Arc::new(Notify::new()),
            metrics: Arc::new(PeerMetrics::default()),
            st: Mutex::new(SlotState {
                addr,
                hint: st.hints.get(&peer).cloned(),
                ..SlotState::default()
            }),
        });
        let (sh, s) = (self.0.clone(), slot.clone());
        let task = supervise(
            "peer",
            slot.token.clone(),
            self.0.env.timings.clone(),
            move || run_slot(sh.clone(), s.clone()),
        );
        while st.tasks.try_join_next().is_some() {}
        st.tasks.spawn(task);
        st.slots.insert(peer, slot.clone());
        slot
    }

    /// An extra address for `id` (a join reply's peer list); dials it now.
    pub fn add_hint(&self, id: NodeId, addr: NodeAddrBytes) {
        let mut st = self.lock();
        st.hints.insert(id, addr.clone());
        if let Some(slot) = st.slots.get(&id) {
            let mut s = slot.lock();
            s.hint = Some(addr);
            s.reset_backoff = true;
            drop(s);
            slot.kick.notify_one();
        }
    }

    /// Dials `id` on `alpn` with the address knowledge the slots use, within the dial
    /// timeout. The connection is the caller's; it does not touch the `sync/1` slot.
    pub(crate) async fn connect(&self, id: NodeId, alpn: &[u8]) -> Result<Connection, NodeError> {
        let (addr, hint) = {
            let st = self.lock();
            if st.stopped {
                return Err(NodeError::Removed);
            }
            match st.slots.get(&id) {
                Some(slot) => {
                    let s = slot.lock();
                    (
                        s.addr.clone(),
                        s.hint.clone().or_else(|| st.hints.get(&id).cloned()),
                    )
                }
                None => (None, st.hints.get(&id).cloned()),
            }
        };
        let target = dial_target(id, [addr, hint], self.0.env.discovery)
            .ok_or_else(|| NodeError::Iroh("no known address for peer".into()))?;
        match timeout(DIAL_TIMEOUT, self.0.env.endpoint.connect(target, alpn)).await {
            Ok(Ok(c)) => Ok(c),
            Ok(Err(e)) => Err(NodeError::Iroh(e.to_string())),
            Err(_) => Err(NodeError::Iroh("dial timed out".into())),
        }
    }

    /// Closes the current connection to `id` with `code`; the task reconnects with
    /// backoff if `id` stays a target.
    pub fn close(&self, id: NodeId, code: VarInt) {
        if let Some(slot) = self.lock().slots.get(&id) {
            slot.lock().close = Some(code);
            slot.kick.notify_one();
        }
    }

    pub fn start_round_all(&self) {
        for slot in self.lock().slots.values() {
            if let Some(tx) = &slot.lock().cmds {
                // A full queue already holds a pending start.
                let _ = tx.try_send(SessionCmd::StartRound);
            }
        }
    }

    pub fn reset_backoff_all(&self) {
        for slot in self.lock().slots.values() {
            slot.lock().reset_backoff = true;
            slot.kick.notify_one();
        }
    }

    /// Wakes every session's live push (a store change).
    pub fn wake_all(&self) {
        for slot in self.lock().slots.values() {
            slot.wake.notify_one();
        }
    }

    pub fn metrics(&self, id: &NodeId) -> Option<Arc<PeerMetrics>> {
        self.lock().slots.get(id).map(|s| s.metrics.clone())
    }

    /// The peers that currently have a task.
    pub fn target_ids(&self) -> Vec<NodeId> {
        self.lock().slots.keys().copied().collect()
    }

    /// Whether a session runs with `id`, and its RTT in ms if iroh knows one.
    pub fn link(&self, id: &NodeId) -> Option<Option<u64>> {
        let st = self.lock();
        let slot = st.slots.get(id)?;
        let s = slot.lock();
        let conn = s.live.as_ref()?;
        let rtt = conn
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| u64::try_from(p.rtt().as_millis()).unwrap_or(u64::MAX));
        if let Some(ms) = rtt {
            slot.metrics.rtt_ms.store(ms, Ordering::SeqCst);
        }
        Some(rtt)
    }

    /// An incoming `sync/1` connection that passed the gate.
    pub(crate) fn accept(&self, conn: Connection) {
        let peer = node_id_of(conn.remote_id());
        let mut st = self.lock();
        // After cancellation (shutdown has begun) nothing new may start.
        let refuse = st.stopped || self.0.cancel.is_cancelled();
        if refuse || st.blocked.contains(&peer) || peer == self.0.env.me {
            conn.close(SHUTDOWN, b"not accepting");
            return;
        }
        let slot = match st.slots.get(&peer) {
            Some(s) => s.clone(),
            // Admitted by the gate before the reactor added it as a target.
            None => self.spawn_slot(&mut st, peer, None),
        };
        st.track(conn.clone());
        slot.lock().incoming.push(conn);
        slot.kick.notify_one();
    }

    /// Stops every task and refuses new peers for good (this node was removed).
    pub fn stop(&self) {
        let mut st = self.lock();
        st.stopped = true;
        for (_, slot) in st.slots.drain() {
            slot.token.cancel();
        }
    }

    /// Waits for every slot task after the node's token was cancelled; aborts stragglers.
    pub(crate) async fn join(&self) {
        let mut tasks = std::mem::take(&mut self.lock().tasks);
        let all = async { while tasks.join_next().await.is_some() {} };
        if timeout(JOIN_TIMEOUT, all).await.is_err() {
            warn!("peer tasks did not stop in time; aborting them");
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        for c in self.lock().conns.drain(..) {
            c.close(SHUTDOWN, b"shutdown");
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn partition(&self, ids: &[NodeId]) {
        let mut st = self.lock();
        st.blocked.extend(ids.iter().copied());
        for id in ids {
            if let Some(slot) = st.slots.get(id) {
                slot.kick.notify_one();
            }
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn heal(&self) {
        self.lock().blocked.clear();
        self.reset_backoff_all();
    }

    /// Hands raw bytes to the session with `id` to write as they are; false without one.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn send_raw(&self, id: &NodeId, bytes: Vec<u8>) -> bool {
        let st = self.lock();
        let Some(slot) = st.slots.get(id) else {
            return false;
        };
        let s = slot.lock();
        s.cmds
            .as_ref()
            .is_some_and(|tx| tx.try_send(SessionCmd::RawFrame(bytes)).is_ok())
    }

    /// The application close code `id` gave when the last session with it ended.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn peer_close_code(&self, id: &NodeId) -> Option<u64> {
        self.lock().slots.get(id).and_then(|s| s.lock().peer_close)
    }

    /// Open connections held by this node's slots (live or not yet resolved).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn connection_count(&self) -> usize {
        let mut st = self.lock();
        st.conns.retain(|c| c.close_reason().is_none());
        st.conns.len()
    }
}

/// The `sync/1` protocol handler, mounted behind the [`crate::transport::Gate`].
#[derive(Debug, Clone)]
pub(crate) struct SyncHandler(pub PeerManager);

impl ProtocolHandler for SyncHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        self.0.accept(conn);
        Ok(())
    }
}
