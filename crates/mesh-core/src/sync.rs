//! The `sync/1` replication session (spec §6.3–§6.6): rounds plus live push over one
//! bidirectional byte stream.
//!
//! One reader future and one writer future run joined by `tokio::select!`; the first to
//! finish decides the [`SessionEnd`]. The reader never waits on the writer (it only updates
//! the shared state and kicks the writer), so two sessions pushing large diffs at each
//! other cannot deadlock on full pipes. Every outgoing frame is written by the writer.
//!
//! Wake-ups: the node must call `notify_one` on the session's `wake` after store changes
//! and after clearing `serve_paused`; a stored permit guarantees no wake-up is lost while
//! the writer is busy.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use mesh_proto::{
    MAX_BATCH_BYTES, MAX_BATCH_OPS, MeshId, NodeId, Scope, SignedOp, SyncMsg, VersionVector,
    batch_ops,
};
use mesh_store::{StoreError, SyncItem};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info};

use crate::config::Timings;
use crate::db::StoreHandle;
use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::frame::{FrameError, SYNC_MAX_FRAME, read_frame, write_frame};
use crate::membership::{MeshAcceptor, scope_of_role};

/// Items per `diff_since` page.
const PAGE: usize = 256;

/// Per-peer numbers for `Node::members` (spec §6.6). `0` means unknown for the times.
#[derive(Debug, Default)]
pub struct PeerMetrics {
    /// Set by the node from iroh's connection stats.
    pub rtt_ms: AtomicU64,
    /// Wall time of the last message received.
    pub last_seen_ms: AtomicU64,
    /// Σ over origins of `max(0, own_vv[o] − known[o])`.
    pub lag: AtomicU64,
    pub rounds_completed: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCmd {
    /// `sync_now()`: start a round unless one is already in progress.
    StartRound,
    /// Writes these bytes to the stream as they are (length prefix included), between two
    /// frames. Test hook for protocol-violation tests.
    #[cfg(any(test, feature = "test-hooks"))]
    RawFrame(Vec<u8>),
}

#[derive(Debug)]
pub enum SessionEnd {
    /// The stream ended or failed.
    Closed,
    /// The peer broke the protocol; close with `PROTOCOL`.
    Protocol(String),
    Store(StoreError),
    /// The store is stale and a Full peer that is not stale announced itself: the node
    /// re-bootstraps from it (spec §7, sub-spec 8a ruling C1).
    Stale,
    /// The store is stale and so is the Full peer: neither is a source to restore from,
    /// so they do not sync and neither resets (ruling C1). Redialed with backoff.
    PeerStale,
}

/// Completed pulls from Full peers, per re-bootstrap epoch (sub-spec 8a ruling I2).
///
/// A re-bootstrap calls [`FullPulls::begin_epoch`] when it resets the store; a session
/// records, with each `Hello` it sends, the epoch current then, and when a Full peer
/// answers that `Hello` with `Done` (it sent everything it holds beyond our vector) the
/// session reports that epoch. A re-bootstrap completes only once a pull of its own epoch
/// was reported: everything a Full peer had, against a `Hello` sent after the reset.
#[derive(Debug, Default)]
pub struct FullPulls {
    epoch: AtomicU64,
    pulled: AtomicU64,
}

impl FullPulls {
    /// Starts a new epoch (the store was just reset) and returns it.
    pub fn begin_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn note(&self, epoch: u64) {
        self.pulled.fetch_max(epoch, Ordering::SeqCst);
    }

    /// Whether a Full peer's `Done` answered a `Hello` sent in `epoch` (or later).
    pub fn reached(&self, epoch: u64) -> bool {
        self.pulled.load(Ordering::SeqCst) >= epoch
    }
}

pub struct SessionCtx {
    pub(crate) store: StoreHandle,
    pub(crate) acc: Arc<MeshAcceptor>,
    pub(crate) mesh: MeshId,
    pub(crate) me: NodeId,
    pub(crate) peer: NodeId,
    pub(crate) timings: Timings,
    pub(crate) wake: Arc<Notify>,
    pub(crate) cmds: mpsc::Receiver<SessionCmd>,
    pub(crate) events: broadcast::Sender<NodeEvent>,
    pub(crate) metrics: Arc<PeerMetrics>,
    pub(crate) serve_paused: Arc<AtomicBool>,
    pub(crate) signout: Arc<crate::peers::SignOut>,
    /// Told of every message read (the node's incoming-traffic catch-up).
    pub(crate) incoming: Option<crate::peers::IncomingHook>,
    pub(crate) pulls: Arc<FullPulls>,
}

/// The other ends of a test session's channels.
#[cfg(any(test, feature = "test-hooks"))]
pub struct TestHandles {
    pub cmds: mpsc::Sender<SessionCmd>,
    pub events: broadcast::Receiver<NodeEvent>,
    pub wake: Arc<Notify>,
    pub metrics: Arc<PeerMetrics>,
    pub serve_paused: Arc<AtomicBool>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl SessionCtx {
    pub fn for_test(
        store: StoreHandle,
        acc: Arc<MeshAcceptor>,
        peer: NodeId,
        timings: Timings,
    ) -> (SessionCtx, TestHandles) {
        let (cmd_tx, cmds) = mpsc::channel(8);
        let (events, events_rx) = broadcast::channel(256);
        let wake = Arc::new(Notify::new());
        let metrics = Arc::new(PeerMetrics::default());
        let serve_paused = Arc::new(AtomicBool::new(false));
        let ctx = SessionCtx {
            mesh: store.raw().mesh(),
            me: store.raw().node_id(),
            store,
            acc,
            peer,
            timings,
            wake: wake.clone(),
            cmds,
            events,
            metrics: metrics.clone(),
            serve_paused: serve_paused.clone(),
            signout: Arc::default(),
            incoming: None,
            pulls: Arc::default(),
        };
        let handles = TestHandles {
            cmds: cmd_tx,
            events: events_rx,
            wake,
            metrics,
            serve_paused,
        };
        (ctx, handles)
    }
}

/// Flags of the current round (spec §6.3).
#[derive(Debug, Default)]
struct Round {
    /// Our `Hello` must go out (a start, or the reply to the peer's).
    hello_due: bool,
    /// We started this round and the peer's `Hello` has not arrived yet: the next `Hello`
    /// received is the reply (or a crossing start) and is not answered. At most one of
    /// our starts is ever unanswered, so a received `Hello` is never mistaken for a start
    /// and two sides cannot bounce `Hello`s at each other.
    awaiting_reply: bool,
    hello_sent: bool,
    hello_received: bool,
    done_sent: bool,
    done_received: bool,
    /// The [`FullPulls`] epoch when our `Hello` of this round went out.
    hello_epoch: u64,
    /// When the round began (our start or the peer's `Hello`); `None` when idle.
    began: Option<Instant>,
}

impl Round {
    /// A fresh round that this side starts.
    fn start() -> Round {
        Round {
            hello_due: true,
            awaiting_reply: true,
            began: Some(Instant::now()),
            ..Round::default()
        }
    }

    fn in_progress(&self) -> bool {
        self.hello_due || self.hello_sent || self.hello_received
    }

    fn complete(&self) -> bool {
        self.hello_sent && self.hello_received && self.done_sent && self.done_received
    }
}

#[derive(Debug, Default)]
struct State {
    /// The vv from the peer's latest `Hello`, raised by its `Advertise`s.
    advertised: VersionVector,
    /// What we last announced we hold (our `Hello`, then our `Advertise`s): the bound the
    /// peer checks our `Covered` markers against.
    told: VersionVector,
    /// What we believe the peer holds.
    known: VersionVector,
    /// Bumped on every `Hello` received (it resets `known`).
    hello_gen: u64,
    /// Live mode is on once the peer's first `Hello` arrived.
    live: bool,
    /// The scope from the peer's latest `Hello` (`None` before the first). Decides what
    /// we serve it and whether its `Covered` markers count (sub-spec 8a §3).
    peer_scope: Option<Scope>,
    round: Round,
}

impl State {
    /// The peer announced a Partial scope: its `Covered` markers may mean "filtered",
    /// and its `vv[me]` does not prove it holds our ops.
    fn peer_partial(&self) -> bool {
        matches!(self.peer_scope, Some(Scope::Partial(_)))
    }
}

/// Why a round start was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Session entry or `sync_now()`: only when no round is in progress.
    Requested,
    /// The `sync_interval` tick: also replaces a round in progress for longer than that.
    Tick,
    /// A message after `silence` (spec §6.3): replaces any round in progress.
    Silence,
}

fn raise(vv: &mut VersionVector, origin: NodeId, seq: u64) {
    if vv.get(&origin) < seq {
        vv.set(origin, seq);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn store_end(e: NodeError) -> SessionEnd {
    match e {
        NodeError::Store(e) => SessionEnd::Store(e),
        other => SessionEnd::Store(StoreError::Io(other.to_string())),
    }
}

/// Everything but the command receiver, shared by the reader and the writer.
struct Session {
    ctx: Parts,
    st: Mutex<State>,
    /// Reader → writer: something to send.
    kick: Notify,
}

/// [`SessionCtx`] without the command receiver (the writer owns that).
struct Parts {
    store: StoreHandle,
    acc: Arc<MeshAcceptor>,
    mesh: MeshId,
    me: NodeId,
    peer: NodeId,
    timings: Timings,
    wake: Arc<Notify>,
    events: broadcast::Sender<NodeEvent>,
    metrics: Arc<PeerMetrics>,
    serve_paused: Arc<AtomicBool>,
    signout: Arc<crate::peers::SignOut>,
    incoming: Option<crate::peers::IncomingHook>,
    pulls: Arc<FullPulls>,
}

/// Runs one session until the stream closes or an error ends it. The dialer starts a
/// round on entry and every `sync_interval`.
pub async fn run_session<R, W>(ctx: SessionCtx, r: R, w: W, dialer: bool) -> SessionEnd
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let SessionCtx {
        store,
        acc,
        mesh,
        me,
        peer,
        timings,
        wake,
        mut cmds,
        events,
        metrics,
        serve_paused,
        signout,
        incoming,
        pulls,
    } = ctx;
    let s = Session {
        ctx: Parts {
            store,
            acc,
            mesh,
            me,
            peer,
            timings,
            wake,
            events,
            metrics,
            serve_paused,
            signout,
            incoming,
            pulls,
        },
        st: Mutex::new(State::default()),
        kick: Notify::new(),
    };
    tokio::select! {
        end = s.read_loop(r) => end,
        end = s.write_loop(w, &mut cmds, dialer) => end,
    }
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent across a panic in another holder (plain flag writes).
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn paused(&self) -> bool {
        self.ctx.serve_paused.load(Ordering::SeqCst)
    }

    /// Starts a round now if `why` allows it against the current round (decided at once,
    /// so a Hello read right after cannot be overtaken by a stale request); the writer
    /// sends the `Hello`.
    fn request_start(&self, why: Start) {
        let started = {
            let mut st = self.lock();
            let ok = self.may_start(&st.round, why);
            if ok {
                st.round = Round::start();
            }
            ok
        };
        if started {
            self.kick.notify_one();
        }
    }

    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&crate::db::CoreStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, SessionEnd> {
        self.ctx.store.call_raw(f).await.map_err(store_end)
    }

    async fn update_lag(&self) -> Result<(), SessionEnd> {
        let own = self.call(|s| s.version_vector()).await?;
        let lag = {
            let st = self.lock();
            own.0
                .iter()
                .map(|(o, &seq)| seq.saturating_sub(st.known.get(o)))
                .fold(0u64, u64::saturating_add)
        };
        self.ctx.metrics.lag.store(lag, Ordering::SeqCst);
        Ok(())
    }

    /// Completes the round if both `Done`s have crossed: `mark_full_sync` with the peer's
    /// confirmed `vv[me]` (not with a Partial peer), count it and emit `RoundComplete`.
    async fn maybe_complete(&self) -> Result<(), SessionEnd> {
        let (synced, partial) = {
            let mut st = self.lock();
            if !st.round.complete() {
                return Ok(());
            }
            st.round = Round::default();
            (st.advertised.get(&self.ctx.me), st.peer_partial())
        };
        // A Partial peer's vector moves past what it filtered out, so its `vv[me]` does
        // not prove it holds our ops, and a round with it is no full sync: marking one
        // would let a re-bootstrap drop own ops only this node holds (spec §7).
        if !partial {
            self.call(move |s| s.mark_full_sync(synced)).await?;
        }
        self.ctx
            .metrics
            .rounds_completed
            .fetch_add(1, Ordering::SeqCst);
        debug!(peer = %self.ctx.peer.short(), synced, "sync round complete");
        let _ = self
            .ctx
            .events
            .send(NodeEvent::RoundComplete(self.ctx.peer));
        Ok(())
    }

    // ------------------------------------------------------------ reader

    async fn read_loop<R: AsyncRead + Unpin>(&self, mut r: R) -> SessionEnd {
        let silence_ms = u64::try_from(self.ctx.timings.silence.as_millis()).unwrap_or(u64::MAX);
        loop {
            let msg = match read_frame::<_, SyncMsg>(&mut r, SYNC_MAX_FRAME).await {
                Ok(Some(m)) => m,
                Ok(None) | Err(FrameError::Io(_)) => return SessionEnd::Closed,
                Err(e) => return SessionEnd::Protocol(e.to_string()),
            };
            let now = now_ms();
            let prev = self.ctx.metrics.last_seen_ms.swap(now, Ordering::SeqCst);
            let after_silence = prev != 0 && now.saturating_sub(prev) > silence_ms;
            let is_hello = matches!(msg, SyncMsg::Hello { .. });
            if let Err(end) = self.on_message(msg).await {
                return end;
            }
            if let Some(hook) = &self.ctx.incoming {
                hook.traffic();
            }
            // Android rule (master §10.2): traffic after a long silence starts a round,
            // unless the peer is starting one itself.
            if after_silence && !is_hello {
                self.request_start(Start::Silence);
            }
        }
    }

    async fn on_message(&self, msg: SyncMsg) -> Result<(), SessionEnd> {
        match msg {
            SyncMsg::Hello {
                mesh,
                vv,
                scope,
                stale,
            } => {
                if mesh != self.ctx.mesh {
                    return Err(SessionEnd::Protocol("Hello for another mesh".into()));
                }
                self.check_scope(&scope).await?;
                self.check_stale(&scope, stale).await?;
                self.on_hello(vv, scope);
                Ok(())
            }
            SyncMsg::Advertise { vv } => {
                self.ctx.signout.peer_hello(&self.ctx.me, &vv);
                let mut st = self.lock();
                for (o, &seq) in &vv.0 {
                    raise(&mut st.advertised, *o, seq);
                    raise(&mut st.known, *o, seq);
                }
                Ok(())
            }
            SyncMsg::Ops { ops } => self.on_ops(ops).await,
            SyncMsg::Covered { origin, upto } => {
                let (advertised, partial) = {
                    let st = self.lock();
                    (st.advertised.get(&origin), st.peer_partial())
                };
                if partial {
                    // Sub-spec 8a §3: from a Partial peer a marker may mean "filtered", not
                    // "compacted"; the gap waits in `pending` for a Full peer. `known` is
                    // not raised either (the scoped diff we serve covers it anyway).
                    debug!(
                        peer = %self.ctx.peer.short(),
                        origin = %origin.short(),
                        upto,
                        "ignoring Covered from a partial peer"
                    );
                    return Ok(());
                }
                let acc = self.ctx.acc.clone();
                self.call(move |s| s.apply_covered(origin, upto, advertised, &*acc))
                    .await?;
                if upto <= advertised {
                    raise(&mut self.lock().known, origin, upto);
                }
                Ok(())
            }
            SyncMsg::Done => {
                {
                    let mut st = self.lock();
                    if !st.round.hello_received || st.round.done_received {
                        debug!(peer = %self.ctx.peer.short(), "ignoring a Done outside a round");
                        return Ok(());
                    }
                    st.round.done_received = true;
                    // A Full peer sent all it holds beyond our Hello of this round.
                    if st.round.hello_sent && !st.peer_partial() {
                        self.ctx.pulls.note(st.round.hello_epoch);
                    }
                }
                self.maybe_complete().await
            }
        }
    }

    /// Spec §7 with sub-spec 8a ruling C1, on each `Hello` from a Full peer while this
    /// store is stale: a peer that is no restore source either ends the session
    /// ([`SessionEnd::PeerStale`]; nothing is served or reset), a fresh one ends it with
    /// [`SessionEnd::Stale`] so the node re-bootstraps from it. Nothing is served to a Full
    /// peer from a stale store. With a Partial peer (a client) a stale node syncs on as
    /// usual: a client cannot restore it, and such rounds do not refresh its staleness.
    async fn check_stale(&self, peer_scope: &Scope, peer_stale: bool) -> Result<(), SessionEnd> {
        if matches!(peer_scope, Scope::Partial(_)) || !self.call(|s| s.is_stale()).await? {
            return Ok(());
        }
        if peer_stale {
            info!(
                peer = %self.ctx.peer.short(),
                "this store and the peer's are both stale; not syncing with it"
            );
            return Err(SessionEnd::PeerStale);
        }
        Err(SessionEnd::Stale)
    }

    /// Refuses a peer whose announced scope contradicts its role in `_mesh` (host ⇔
    /// `Full`, client ⇔ `Scope::client()`), like an unknown member. A peer with no role
    /// yet (a provisional peer during a join) is taken at its word: serving it scoped and
    /// ignoring its markers is always safe, and `Full` is today's treatment.
    async fn check_scope(&self, scope: &Scope) -> Result<(), SessionEnd> {
        let (acc, peer) = (self.ctx.acc.clone(), self.ctx.peer);
        let role = self.call(move |s| acc.role_of(s, &peer)).await?;
        match role {
            Some(role) if scope_of_role(role) != *scope => Err(SessionEnd::Protocol(format!(
                "Hello scope {scope:?} contradicts the peer's role {role:?}"
            ))),
            _ => Ok(()),
        }
    }

    fn on_hello(&self, vv: VersionVector, scope: Scope) {
        self.ctx.signout.peer_hello(&self.ctx.me, &vv);
        let mut st = self.lock();
        if st.round.awaiting_reply {
            // The reply to our start, or the peer's start crossing ours: either way it
            // pairs with our Hello and is not answered (spec §6.3 step 2).
            let r = &mut st.round;
            r.awaiting_reply = false;
            r.hello_received = true;
            r.done_sent = false;
            r.done_received = false;
        } else {
            // The peer starts a round (possibly over the current one): reply exactly once.
            st.round = Round {
                hello_due: true,
                hello_received: true,
                began: Some(Instant::now()),
                ..Round::default()
            };
        }
        st.advertised = vv.clone();
        st.known = vv;
        st.peer_scope = Some(scope);
        st.hello_gen += 1;
        st.live = true;
        drop(st);
        self.kick.notify_one();
    }

    async fn on_ops(&self, ops: Vec<SignedOp>) -> Result<(), SessionEnd> {
        if ops.len() > MAX_BATCH_OPS {
            return Err(SessionEnd::Protocol(format!(
                "Ops with {} ops (limit {MAX_BATCH_OPS})",
                ops.len()
            )));
        }
        // A single op may exceed the byte budget (it gets its own batch); the frame cap
        // still bounds it.
        if ops.len() > 1 {
            let bytes: usize = ops.iter().map(|o| mesh_proto::encode(o).len()).sum();
            if bytes > MAX_BATCH_BYTES {
                return Err(SessionEnd::Protocol(format!(
                    "Ops of {bytes} bytes (limit {MAX_BATCH_BYTES})"
                )));
            }
        }
        // The peer holds these ops: raise `known` before applying them, so the writer never
        // diffs against a `known` that lacks them and echoes them back. A failed apply ends
        // the session, and the next session starts from the peer's Hello.
        {
            let mut st = self.lock();
            for o in &ops {
                raise(&mut st.known, o.body.origin, o.body.seq);
            }
        }
        let acc = self.ctx.acc.clone();
        self.call(move |s| s.apply_batch(ops, &*acc)).await?;
        self.update_lag().await
    }

    // ------------------------------------------------------------ writer

    async fn write_loop<W: AsyncWrite + Unpin>(
        &self,
        mut w: W,
        cmds: &mut mpsc::Receiver<SessionCmd>,
        dialer: bool,
    ) -> SessionEnd {
        let every = self.ctx.timings.sync_interval;
        let mut tick = tokio::time::interval_at(Instant::now() + every, every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut cmds_open = true;
        if dialer {
            self.request_start(Start::Requested);
        }
        loop {
            if let Err(end) = self.write_pending(&mut w).await {
                return end;
            }
            tokio::select! {
                _ = self.kick.notified() => {}
                _ = self.ctx.wake.notified() => {}
                cmd = cmds.recv(), if cmds_open => match cmd {
                    Some(SessionCmd::StartRound) => self.request_start(Start::Requested),
                    #[cfg(any(test, feature = "test-hooks"))]
                    Some(SessionCmd::RawFrame(bytes)) => {
                        use tokio::io::AsyncWriteExt;
                        let r = async { w.write_all(&bytes).await?; w.flush().await }.await;
                        if r.is_err() {
                            return SessionEnd::Closed;
                        }
                    }
                    None => cmds_open = false,
                },
                _ = tick.tick(), if dialer => self.request_start(Start::Tick),
            }
        }
    }

    async fn send<W: AsyncWrite + Unpin>(
        &self,
        w: &mut W,
        msg: &SyncMsg,
    ) -> Result<(), SessionEnd> {
        write_frame(w, msg).await.map_err(|e| {
            debug!(peer = %self.ctx.peer.short(), error = %e, "sync write failed");
            SessionEnd::Closed
        })
    }

    /// Whether a start for `why` may replace `round`. Never while one of our Hellos is
    /// unanswered or a reply is about to go out: the Hello coming back (or going out)
    /// already exchanges fresh vectors, and a second one in flight would be answered as a
    /// new round.
    fn may_start(&self, round: &Round, why: Start) -> bool {
        if round.awaiting_reply || round.hello_due {
            return false;
        }
        if !round.in_progress() {
            return true;
        }
        match why {
            Start::Requested => false,
            Start::Tick => round
                .began
                .is_some_and(|t| t.elapsed() > self.ctx.timings.sync_interval),
            Start::Silence => true,
        }
    }

    /// Sends whatever is due: a `Hello`, the diff, and `Done`.
    async fn write_pending<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<(), SessionEnd> {
        let send_hello = {
            let mut st = self.lock();
            let r = &mut st.round;
            let due = r.hello_due;
            if due {
                r.hello_due = false;
                r.hello_sent = true;
            }
            due
        };
        if send_hello {
            // Spec §7 / ruling C1: a stale (or re-bootstrapping) node says so; whether it
            // syncs is decided on the peer's `Hello` (`check_stale`).
            let (vv, scope, stale) = self
                .call(|s| Ok((s.version_vector()?, s.scope(), s.is_stale()?)))
                .await?;
            let stale = stale || self.paused();
            {
                let mut st = self.lock();
                st.round.hello_epoch = self.ctx.pulls.current();
                st.told = vv.clone();
            }
            let mesh = self.ctx.mesh;
            let hello = SyncMsg::Hello {
                mesh,
                vv,
                scope,
                stale,
            };
            self.send(w, &hello).await?;
        }

        if self.paused() || !self.lock().live {
            return Ok(());
        }
        // A stale store serves no Full peer (spec §7): it would hand on what the other
        // replicas may have GC'd the tombstones of. Live mode from before the store went
        // stale ends here; the peer's next `Hello` decides (`check_stale`).
        if !self.lock().peer_partial() && self.call(|s| s.is_stale()).await? {
            return Ok(());
        }
        let Some(gen_flushed) = self.flush(w).await? else {
            return Ok(());
        };
        let send_done = {
            let mut st = self.lock();
            // `known` was reset by a Hello after the flush: the next pass re-diffs first.
            let ready = st.hello_gen == gen_flushed
                && st.round.hello_received
                && st.round.hello_sent
                && !st.round.done_sent;
            if ready {
                st.round.done_sent = true;
            }
            ready
        };
        if send_done {
            self.send(w, &SyncMsg::Done).await?;
            self.maybe_complete().await?;
        }
        Ok(())
    }

    /// Streams `diff_since_scoped(known, peer scope)` page by page until a page is empty,
    /// raising `known` for every item sent. A Partial peer gets only what its scope
    /// admits; the rest of each origin goes as `Covered` markers. Returns the `hello_gen`
    /// the empty page was computed under, or `None` if serving was paused meanwhile.
    async fn flush<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<Option<u64>, SessionEnd> {
        loop {
            if self.paused() {
                return Ok(None);
            }
            let (known, gen_now, scope) = {
                let st = self.lock();
                // `live` (checked by the caller) implies a Hello, so the scope is known;
                // were it not, serving less is the safe side.
                let scope = st.peer_scope.clone().unwrap_or_else(Scope::client);
                (st.known.clone(), st.hello_gen, scope)
            };
            let items = self
                .call(move |s| s.diff_since_scoped(&known, PAGE, &scope))
                .await?;
            if items.is_empty() {
                return Ok(Some(gen_now));
            }
            let mut ops = Vec::new();
            for item in items {
                match item {
                    SyncItem::Op(op) => ops.push(op),
                    SyncItem::Covered { origin, upto } => {
                        self.send_ops(w, std::mem::take(&mut ops)).await?;
                        // Ruling I3: the peer ignores a marker above what we announced, so
                        // in live mode (a write since our Hello) announce more first.
                        if upto > self.lock().told.get(&origin) {
                            let vv = self.call(|s| s.version_vector()).await?;
                            self.send(w, &SyncMsg::Advertise { vv: vv.clone() }).await?;
                            self.lock().told = vv;
                        }
                        self.send(w, &SyncMsg::Covered { origin, upto }).await?;
                        raise(&mut self.lock().known, origin, upto);
                    }
                }
            }
            self.send_ops(w, ops).await?;
            self.update_lag().await?;
        }
    }

    async fn send_ops<W: AsyncWrite + Unpin>(
        &self,
        w: &mut W,
        ops: Vec<SignedOp>,
    ) -> Result<(), SessionEnd> {
        for msg in batch_ops(ops) {
            self.send(w, &msg).await?;
            if let SyncMsg::Ops { ops } = &msg {
                let mut st = self.lock();
                for op in ops {
                    raise(&mut st.known, op.body.origin, op.body.seq);
                }
            }
        }
        Ok(())
    }
}
