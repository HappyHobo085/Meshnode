//! `PeerManager` (spec §6.1): one task per target peer that keeps **one** `sync/1`
//! connection to it and runs the session on it.
//!
//! Each slot task dials with backoff (`backoff_min` doubling to `backoff_max`, reset after a
//! session that completed a round and by `reset_backoff_all`), takes incoming connections
//! handed over by the gated `SyncHandler`, and keeps at most one of them: when two
//! connections exist between a pair, the one dialled by the lower `NodeId` survives and the
//! other is closed with `SHUTDOWN`. Both sides apply the same rule, so they agree.
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

use iroh::endpoint::{Connection, ConnectionError, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use mesh_proto::{MeshId, NodeAddrBytes, NodeId};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::Timings;
use crate::db::StoreHandle;
use crate::events::NodeEvent;
use crate::membership::MeshAcceptor;
use crate::node::supervise;
use crate::sync::{PeerMetrics, SessionCmd, SessionCtx, SessionEnd, run_session};
use crate::transport::{
    ALPN_SYNC, PROTOCOL, REMOVED, SHUTDOWN, addr_from_bytes, endpoint_id_of, node_id_of,
};

/// Upper bound on one dial attempt.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
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
        if st.stopped || st.blocked.contains(&peer) || peer == self.0.env.me {
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

// ---------------------------------------------------------------- slot task

/// A running session and the connection it runs on.
struct Live {
    conn: Connection,
    /// Who dialled `conn`.
    by: NodeId,
    fut: BoxFut<SessionEnd>,
    rounds_at_start: u64,
}

enum Ev {
    Cancelled,
    Kick,
    Dialed(Result<Connection, String>),
    Ended(SessionEnd),
    DialDue,
}

async fn opt<T>(f: &mut Option<BoxFut<T>>) -> T {
    match f {
        Some(f) => f.await,
        None => std::future::pending().await,
    }
}

async fn session_end(live: &mut Option<Live>) -> SessionEnd {
    match live {
        Some(l) => (&mut l.fut).await,
        None => std::future::pending().await,
    }
}

async fn until(at: Option<Instant>) {
    match at {
        Some(t) => sleep_until(t).await,
        None => std::future::pending().await,
    }
}

struct SlotTask {
    sh: Arc<Shared>,
    slot: Arc<Slot>,
    backoff: Duration,
    next_dial: Option<Instant>,
    dialing: Option<BoxFut<Result<Connection, String>>>,
    live: Option<Live>,
}

async fn run_slot(sh: Arc<Shared>, slot: Arc<Slot>) {
    // A restart after a panic: the previous run's session is gone.
    let stale_live = {
        let mut s = slot.lock();
        s.cmds = None;
        s.live.take()
    };
    if let Some(c) = stale_live {
        c.close(SHUTDOWN, b"restart");
        let _ = sh.env.events.send(NodeEvent::PeerDown(slot.peer));
    }
    let mut t = SlotTask {
        backoff: sh.env.timings.backoff_min,
        next_dial: Some(Instant::now()),
        dialing: None,
        live: None,
        sh,
        slot,
    };
    t.run().await;
}

impl SlotTask {
    fn env(&self) -> &PeerEnv {
        &self.sh.env
    }

    fn blocked(&self) -> bool {
        lock(&self.sh.st).blocked.contains(&self.slot.peer)
    }

    async fn run(&mut self) {
        loop {
            self.control();
            if self.live.is_none()
                && self.dialing.is_none()
                && self.next_dial.is_some_and(|t| t <= Instant::now())
            {
                self.next_dial = None;
                self.start_dial();
            }
            let dial_due = if self.live.is_none() && self.dialing.is_none() {
                self.next_dial
            } else {
                None
            };
            let ev = tokio::select! {
                _ = self.slot.token.cancelled() => Ev::Cancelled,
                _ = self.slot.kick.notified() => Ev::Kick,
                r = opt(&mut self.dialing) => Ev::Dialed(r),
                e = session_end(&mut self.live) => Ev::Ended(e),
                _ = until(dial_due) => Ev::DialDue,
            };
            match ev {
                Ev::Cancelled => break,
                Ev::Kick | Ev::DialDue => {}
                Ev::Dialed(r) => {
                    self.dialing = None;
                    self.on_dialed(r);
                }
                Ev::Ended(end) => self.on_ended(end),
            }
        }
        self.dialing = None;
        // A close requested together with the cancellation (step 4: `REMOVED`) wins.
        let code = self.slot.lock().close.take().unwrap_or(SHUTDOWN);
        if let Some(l) = self.live.take() {
            l.conn.close(code, b"shutdown");
            self.finish(l);
        }
    }

    /// Applies requests from the rest of the node.
    fn control(&mut self) {
        let (incoming, reset, close) = {
            let mut s = self.slot.lock();
            (
                std::mem::take(&mut s.incoming),
                std::mem::take(&mut s.reset_backoff),
                s.close.take(),
            )
        };
        let blocked = self.blocked();
        if let Some(code) = close
            && let Some(l) = self.live.take()
        {
            l.conn.close(code, b"closed");
            self.finish(l);
            self.schedule_redial();
        }
        if blocked {
            self.dialing = None;
            if let Some(l) = self.live.take() {
                l.conn.close(SHUTDOWN, b"partitioned");
                self.finish(l);
            }
        }
        if reset {
            self.backoff = self.env().timings.backoff_min;
            if self.live.is_none() {
                self.next_dial = Some(Instant::now());
            }
        }
        for conn in incoming {
            if blocked {
                conn.close(SHUTDOWN, b"partitioned");
            } else {
                let peer = self.slot.peer;
                self.offer(conn, peer);
            }
        }
    }

    fn dial_addr(&self) -> Option<EndpointAddr> {
        let (addr, hint) = {
            let s = self.slot.lock();
            (s.addr.clone(), s.hint.clone())
        };
        let id = endpoint_id_of(&self.slot.peer).ok()?;
        let mut target = EndpointAddr::new(id);
        for bytes in [addr, hint].into_iter().flatten() {
            match addr_from_bytes(&bytes) {
                Ok(a) if a.id == id => target.addrs.extend(a.addrs),
                Ok(_) => warn!(peer = %self.slot.peer.short(), "address for another node ignored"),
                Err(e) => debug!(peer = %self.slot.peer.short(), error = %e, "bad address ignored"),
            }
        }
        (!target.addrs.is_empty() || self.env().discovery).then_some(target)
    }

    fn start_dial(&mut self) {
        if self.blocked() {
            return;
        }
        // Without an address (and no discovery) wait for one: `set_targets` / `add_hint`
        // kick the task with a backoff reset.
        let Some(addr) = self.dial_addr() else {
            return;
        };
        let ep = self.env().endpoint.clone();
        debug!(peer = %self.slot.peer.short(), "dialling");
        self.dialing = Some(Box::pin(async move {
            match timeout(DIAL_TIMEOUT, ep.connect(addr, ALPN_SYNC)).await {
                Ok(Ok(c)) => Ok(c),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("dial timed out".into()),
            }
        }));
    }

    fn on_dialed(&mut self, r: Result<Connection, String>) {
        match r {
            Ok(conn) => {
                if self.blocked() {
                    conn.close(SHUTDOWN, b"partitioned");
                    return;
                }
                lock(&self.sh.st).track(conn.clone());
                let me = self.env().me;
                self.offer(conn, me);
            }
            Err(e) => {
                debug!(peer = %self.slot.peer.short(), error = %e, "dial failed");
                if self.live.is_none() {
                    self.schedule_redial();
                }
            }
        }
    }

    fn schedule_redial(&mut self) {
        let t = &self.sh.env.timings;
        self.next_dial = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(t.backoff_max);
    }

    /// A new connection dialled by `by`: run the session on it unless the current one
    /// wins the duplicate rule (spec §6.1).
    fn offer(&mut self, conn: Connection, by: NodeId) {
        let (me, peer) = (self.env().me, self.slot.peer);
        match self.live.take() {
            None => {
                self.live = Some(self.start_session(conn, by));
                info!(peer = %peer.short(), "peer up");
                let _ = self.env().events.send(NodeEvent::PeerUp(peer));
            }
            Some(cur) => {
                // Same dialer: the peer replaced a connection it considers dead.
                if by == cur.by || by == me.min(peer) {
                    debug!(peer = %peer.short(), "replacing the connection");
                    cur.conn.close(SHUTDOWN, b"duplicate connection");
                    self.live = Some(self.start_session(conn, by));
                } else {
                    debug!(peer = %peer.short(), "closing a duplicate connection");
                    conn.close(SHUTDOWN, b"duplicate connection");
                    self.live = Some(cur);
                }
            }
        }
    }

    fn start_session(&self, conn: Connection, by: NodeId) -> Live {
        let env = self.env();
        let dialer = by == env.me;
        let (tx, rx) = mpsc::channel(8);
        let ctx = SessionCtx {
            store: env.store.clone(),
            acc: env.acc.clone(),
            mesh: env.mesh,
            me: env.me,
            peer: self.slot.peer,
            timings: env.timings.clone(),
            wake: self.slot.wake.clone(),
            cmds: rx,
            events: env.events.clone(),
            metrics: self.slot.metrics.clone(),
            serve_paused: env.serve_paused.clone(),
        };
        {
            let mut s = self.slot.lock();
            s.live = Some(conn.clone());
            s.cmds = Some(tx);
        }
        let c = conn.clone();
        let fut = Box::pin(async move {
            let streams = if dialer {
                c.open_bi().await
            } else {
                c.accept_bi().await
            };
            match streams {
                Ok((send, recv)) => run_session(ctx, recv, send, dialer).await,
                Err(e) => {
                    debug!(error = %e, "no sync stream");
                    SessionEnd::Closed
                }
            }
        });
        Live {
            conn,
            by,
            fut,
            rounds_at_start: self.slot.metrics.rounds_completed.load(Ordering::SeqCst),
        }
    }

    fn on_ended(&mut self, end: SessionEnd) {
        let Some(l) = self.live.take() else {
            return;
        };
        let peer = self.slot.peer.short();
        match end {
            SessionEnd::Closed => {
                if let Some(ConnectionError::ApplicationClosed(c)) = l.conn.close_reason()
                    && c.error_code == REMOVED
                {
                    info!(peer, "peer refuses this node as removed");
                } else {
                    debug!(peer, "session closed");
                }
            }
            SessionEnd::Protocol(m) => {
                warn!(peer, reason = %m, "protocol violation; closing");
                l.conn.close(PROTOCOL, b"protocol violation");
            }
            SessionEnd::Store(e) => warn!(peer, error = %e, "store error; session closed"),
            SessionEnd::Stale => {
                warn!(peer, "store is stale; session stopped");
                let _ = self.env().stale.try_send(self.slot.peer);
            }
        }
        if self.slot.metrics.rounds_completed.load(Ordering::SeqCst) > l.rounds_at_start {
            self.backoff = self.env().timings.backoff_min;
        }
        self.finish(l);
        self.schedule_redial();
    }

    /// Bookkeeping when the session on `l` is over.
    fn finish(&self, l: Live) {
        l.conn.close(SHUTDOWN, b"session over");
        {
            let mut s = self.slot.lock();
            s.live = None;
            s.cmds = None;
        }
        info!(peer = %self.slot.peer.short(), "peer down");
        let _ = self.env().events.send(NodeEvent::PeerDown(self.slot.peer));
    }
}
