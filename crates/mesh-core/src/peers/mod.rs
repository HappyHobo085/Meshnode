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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::{Connection, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use mesh_proto::{MeshId, NodeAddrBytes, NodeId};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::Timings;
use crate::db::StoreHandle;
use crate::events::NodeEvent;
use crate::membership::MeshAcceptor;
use crate::node::supervise;
use slot::run_slot;

mod slot;
use crate::sync::{PeerMetrics, SessionCmd};
use crate::transport::{SHUTDOWN, node_id_of};

/// Upper bound on one dial attempt.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Two connections between a pair that start less than this apart are a simultaneous
/// dial, settled by the lower-`NodeId` rule. A connection arriving later means the peer
/// lost the current one (a restart or a crash), so the newer one wins (spec §6.1).
const DUPLICATE_GRACE: Duration = Duration::from_secs(2);
/// How long `join` waits for the slot tasks before aborting them.
const JOIN_TIMEOUT: Duration = Duration::from_secs(3);

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

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
        }))
    }

    fn lock(&self) -> MutexGuard<'_, PmState> {
        lock(&self.0.st)
    }

    /// Replaces the target set: new peers get a task, dropped peers' tasks stop (closing
    /// with `SHUTDOWN`), and a changed address resets that peer's backoff.
    pub fn set_targets(&self, targets: Vec<(NodeId, Option<NodeAddrBytes>)>) {
        let mut st = self.lock();
        if st.stopped {
            return;
        }
        let me = self.0.env.me;
        let want: HashMap<NodeId, Option<NodeAddrBytes>> =
            targets.into_iter().filter(|(id, _)| *id != me).collect();
        st.slots.retain(|id, slot| {
            let keep = want.contains_key(id);
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
