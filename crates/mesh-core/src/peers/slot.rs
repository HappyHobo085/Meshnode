//! The per-peer slot task: dial with backoff, take handed-over connections, keep one
//! connection by the duplicate rule (spec §6.1) and run the `sync/1` session on it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use iroh::EndpointAddr;
use iroh::endpoint::{Connection, ConnectionError};
use mesh_proto::NodeId;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};
use tracing::{debug, info, warn};

use super::{BoxFut, DIAL_TIMEOUT, DUPLICATE_GRACE, PeerEnv, Shared, Slot, lock};
use crate::events::NodeEvent;
use crate::node::NodeStatus;
use crate::sync::{SessionCtx, SessionEnd, run_session};
use crate::transport::{ALPN_SYNC, PROTOCOL, REMOVED, SHUTDOWN};

/// A running session and the connection it runs on.
struct Live {
    conn: Connection,
    /// Who dialled `conn`.
    by: NodeId,
    fut: BoxFut<SessionEnd>,
    rounds_at_start: u64,
    /// When the session on `conn` started.
    since: Instant,
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

pub(super) async fn run_slot(sh: Arc<Shared>, slot: Arc<Slot>) {
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
        super::dial_target(self.slot.peer, [addr, hint], self.env().discovery)
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
                // Arriving well after the current one: not a simultaneous dial. A side dials
                // only while it has no connection, so the peer has lost the current one (it
                // restarted or crashed); the newer connection wins on both sides.
                let late = cur.since.elapsed() > DUPLICATE_GRACE;
                if by == cur.by || by == me.min(peer) || late {
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
            signout: env.signout.clone(),
            incoming: Some(super::PeerManager::incoming_hook(&self.sh)),
            pulls: env.pulls.clone(),
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
                Ok((mut send, mut recv)) => {
                    let end = run_session(ctx, &mut recv, &mut send, dialer).await;
                    // Close before the streams drop: dropping an unfinished send stream
                    // sends a FIN, and a peer reading that EOF first would never see the
                    // `PROTOCOL` code.
                    if matches!(end, SessionEnd::Protocol(_)) {
                        c.close(PROTOCOL, b"protocol violation");
                    }
                    end
                }
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
            since: Instant::now(),
        }
    }

    fn on_ended(&mut self, end: SessionEnd) {
        let Some(l) = self.live.take() else {
            return;
        };
        let peer = self.slot.peer.short();
        #[cfg(any(test, feature = "test-hooks"))]
        {
            self.slot.lock().peer_close = match l.conn.close_reason() {
                Some(ConnectionError::ApplicationClosed(c)) => Some(c.error_code.into_inner()),
                _ => None,
            };
        }
        // The peer's claim that this node was removed (not a signed record, spec §4).
        let refused = matches!(
            l.conn.close_reason(),
            Some(ConnectionError::ApplicationClosed(c)) if c.error_code == REMOVED
        );
        if refused {
            info!(peer, "peer refuses this node as removed");
            self.note_refused();
            self.env().signout.peer_refused();
        }
        let reset = matches!(end, SessionEnd::Reset);
        match end {
            SessionEnd::Closed => {
                if !refused {
                    debug!(peer, "session closed");
                }
            }
            // The session future already closed the connection with `PROTOCOL`.
            SessionEnd::Protocol(m) => warn!(peer, reason = %m, "protocol violation; closed"),
            SessionEnd::Store(e) => warn!(peer, error = %e, "store error; session closed"),
            SessionEnd::Reset => debug!(peer, "store reset; restarting the session"),
            SessionEnd::Stale => {
                warn!(
                    peer,
                    "re-bootstrap due (stale store or role upgrade); session stopped"
                );
                // Already paused by the session (`check_stale`); the re-bootstrap clears this.
                self.env().serve_paused.store(true, Ordering::SeqCst);
                let _ = self.env().stale.try_send(self.slot.peer);
            }
        }
        if self.slot.metrics.rounds_completed.load(Ordering::SeqCst) > l.rounds_at_start || reset {
            // A reset restarts every session at once (ruling R10).
            self.backoff = self.env().timings.backoff_min;
        }
        if refused {
            // Spec §4: a refused node keeps trying, but only at the slowest pace.
            self.backoff = self.env().timings.backoff_max;
        }
        self.finish(l);
        self.schedule_redial();
    }

    /// Records that this peer closed with `REMOVED`; the node keeps running (spec §4).
    fn note_refused(&self) {
        let peer = self.slot.peer;
        let env = self.env();
        let changed = env.status.send_if_modified(|s| match s {
            NodeStatus::Running => {
                *s = NodeStatus::RefusedAsRemoved { by: vec![peer] };
                true
            }
            NodeStatus::RefusedAsRemoved { by } if !by.contains(&peer) => {
                by.push(peer);
                true
            }
            _ => false,
        });
        if changed {
            let _ = env.events.send(NodeEvent::RefusedAsRemoved(peer));
        }
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
