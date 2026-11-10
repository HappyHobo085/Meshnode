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
    MAX_BATCH_BYTES, MAX_BATCH_OPS, MeshId, NodeId, SignedOp, SyncMsg, VersionVector, batch_ops,
};
use mesh_store::{StoreError, SyncItem};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::debug;

use crate::config::Timings;
use crate::db::StoreHandle;
use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::frame::{FrameError, SYNC_MAX_FRAME, read_frame, write_frame};
use crate::membership::MeshAcceptor;

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
    /// The store is stale; the node re-bootstraps (spec §7).
    Stale,
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
    /// The vv from the peer's latest `Hello`.
    advertised: VersionVector,
    /// What we believe the peer holds.
    known: VersionVector,
    /// Bumped on every `Hello` received (it resets `known`).
    hello_gen: u64,
    /// Live mode is on once the peer's first `Hello` arrived.
    live: bool,
    round: Round,
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
    /// confirmed `vv[me]`, count it and emit `RoundComplete`.
    async fn maybe_complete(&self) -> Result<(), SessionEnd> {
        let synced = {
            let mut st = self.lock();
            if !st.round.complete() {
                return Ok(());
            }
            st.round = Round::default();
            st.advertised.get(&self.ctx.me)
        };
        self.call(move |s| s.mark_full_sync(synced)).await?;
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
            // Android rule (master §10.2): traffic after a long silence starts a round,
            // unless the peer is starting one itself.
            if after_silence && !is_hello {
                self.request_start(Start::Silence);
            }
        }
    }

    async fn on_message(&self, msg: SyncMsg) -> Result<(), SessionEnd> {
        match msg {
            SyncMsg::Hello { mesh, vv } => {
                if mesh != self.ctx.mesh {
                    return Err(SessionEnd::Protocol("Hello for another mesh".into()));
                }
                self.on_hello(vv);
                Ok(())
            }
            SyncMsg::Ops { ops } => self.on_ops(ops).await,
            SyncMsg::Covered { origin, upto } => {
                let advertised = self.lock().advertised.get(&origin);
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
                }
                self.maybe_complete().await
            }
        }
    }

    fn on_hello(&self, vv: VersionVector) {
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
            // Spec §7: a stale node does not take part in rounds.
            if self.call(|s| s.is_stale()).await? {
                return Err(SessionEnd::Stale);
            }
            let vv = self.call(|s| s.version_vector()).await?;
            let mesh = self.ctx.mesh;
            self.send(w, &SyncMsg::Hello { mesh, vv }).await?;
        }

        if self.paused() || !self.lock().live {
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

    /// Streams `diff_since(known)` page by page until a page is empty, raising `known` for
    /// every item sent. Returns the `hello_gen` the empty page was computed under, or
    /// `None` if serving was paused meanwhile.
    async fn flush<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<Option<u64>, SessionEnd> {
        loop {
            if self.paused() {
                return Ok(None);
            }
            let (known, gen_now) = {
                let st = self.lock();
                (st.known.clone(), st.hello_gen)
            };
            let items = self.call(move |s| s.diff_since(&known, PAGE)).await?;
            if items.is_empty() {
                return Ok(Some(gen_now));
            }
            let mut ops = Vec::new();
            for item in items {
                match item {
                    SyncItem::Op(op) => ops.push(op),
                    SyncItem::Covered { origin, upto } => {
                        self.send_ops(w, std::mem::take(&mut ops)).await?;
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
