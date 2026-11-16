//! The client side of `httpfwd` (spec §3.3): forward a request to the best placed host, over
//! cached connections, trying the next host when one cannot serve it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use iroh::endpoint::VarInt;
use mesh_core::frame::FrameError;
use mesh_core::{Connection, NodeError, NodeHandle};
use mesh_proto::{ALPN_HTTPFWD, NodeId, PING_SERVICE, ServiceManifest};
use tokio::sync::Semaphore;
use tokio::time::{Instant, timeout_at};
use tracing::warn;

use crate::frame::{FrameConvError, Unframeable, encode_request, exchange, to_response};
use crate::placement::{fetch_manifest, placed_hosts};

/// Hosts tried per request.
const MAX_ATTEMPTS: usize = 3;
/// Forwards in flight on one forwarder at once.
const MAX_FORWARDS: usize = 64;
/// Longest the connect phase of one attempt may take: a host listed online that no longer
/// answers is left for the next one, whatever the attempt's own limit (ruling R15 M6).
const CONNECT_CAP: Duration = Duration::from_secs(5);
/// Close code for a cached connection dropped after an error.
const DROPPED: VarInt = VarInt::from_u32(0);

/// `503` bodies after which the next host is tried.
const RETRYABLE: [&str; 3] = [
    "Service not placed on this node",
    "Service not yet replicated",
    "Node re-bootstrapping",
];

/// Why [`RemoteForwarder::forward`] gave no answer, or why one attempt to one host failed
/// (the attempt errors are logged, and the next host is tried).
///
/// `forward` itself returns only [`NoHost`](Self::NoHost), [`Timeout`](Self::Timeout),
/// [`NotDeployed`](Self::NotDeployed) and [`Unframeable`](Self::Unframeable).
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// No host could serve the service: none is placed and online, the manifest is
    /// unreadable, or no attempt timed out after reaching its host (each failed to
    /// connect, failed on the stream, or got a retryable `503`).
    #[error("no host could serve the service")]
    NoHost,
    /// The service has no manifest (undeployed).
    #[error("service not deployed")]
    NotDeployed,
    /// The request cannot be put in an `httpfwd` frame.
    #[error("request cannot be forwarded: {0}")]
    Unframeable(#[from] Unframeable),
    #[error("connect failed: {0}")]
    Connect(#[from] NodeError),
    /// Connecting took the attempt's connect limit (milliseconds: at most 5 s, less if
    /// the attempt's whole limit is shorter): counted as a failure to connect, not as a
    /// timeout.
    #[error("connect timed out after {0} ms")]
    ConnectTimeout(u64),
    #[error("stream failed: {0}")]
    Stream(String),
    #[error("bad response frame: {0}")]
    Frame(#[from] FrameError),
    #[error("stream ended without a response")]
    NoResponse,
    #[error("bad response: {0}")]
    Convert(#[from] FrameConvError),
    /// An attempt reached its host and got no answer within its limit (milliseconds).
    /// From `forward`: every host was tried, and at least one of them timed out so
    /// (ruling R11; the others failed in any way).
    #[error("timed out after {0} ms")]
    Timeout(u64),
}

/// Forwards requests over `httpfwd` to the hosts a service is placed on. Cheap to clone;
/// clones share the connection cache and the forwarding limit. It never serves a request
/// itself, and needs nothing but a node handle.
#[derive(Clone)]
pub struct RemoteForwarder {
    inner: Arc<Inner>,
}

struct Inner {
    node: NodeHandle,
    /// One connection per peer, dropped (and closed) after an error.
    conns: Mutex<HashMap<NodeId, Connection>>,
    permits: Semaphore,
    /// Forward attempts made (test hook).
    attempts: AtomicU64,
    /// Connections opened (test hook).
    connects: AtomicU64,
    /// Hosts tried first, in this order (test hook).
    first: Mutex<Vec<NodeId>>,
    /// Hosts whose connect never completes (test hook).
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    hang: Mutex<Vec<NodeId>>,
}

impl RemoteForwarder {
    pub fn new(node: NodeHandle) -> RemoteForwarder {
        RemoteForwarder {
            inner: Arc::new(Inner {
                node,
                conns: Mutex::new(HashMap::new()),
                permits: Semaphore::new(MAX_FORWARDS),
                attempts: AtomicU64::new(0),
                connects: AtomicU64::new(0),
                first: Mutex::new(Vec::new()),
                hang: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Forwards `req` for `service` to an online effective host its placement selects (this
    /// node excluded), nearest first, trying up to three. Each attempt gets the manifest's
    /// `request_timeout_ms` plus `slack` (connecting at most 5 s of it), and a host that
    /// fails, or answers one of the retryable `503`s (not placed there, not yet
    /// replicated, re-bootstrapping), is followed by the next. Returns the first other answer, whatever its status. When no
    /// host answered: [`ForwardError::Timeout`] if some attempt reached its host and timed
    /// out, else [`ForwardError::NoHost`].
    pub async fn forward(
        &self,
        service: &str,
        req: Request<Bytes>,
        slack: Duration,
    ) -> Result<Response<Bytes>, ForwardError> {
        self.inner.forward(service, req, slack).await
    }

    /// Forward attempts this forwarder has made.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn attempts(&self) -> u64 {
        self.inner.attempts.load(Ordering::SeqCst)
    }

    /// `httpfwd` connections this forwarder has opened.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn connects(&self) -> u64 {
        self.inner.connects.load(Ordering::SeqCst)
    }

    /// Tries the candidates among `ids` first, in this order, instead of by RTT.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn try_first(&self, ids: Vec<NodeId>) {
        *self.inner.first.lock().unwrap_or_else(|e| e.into_inner()) = ids;
    }

    /// Connecting to any of `ids` hangs from now on (a host listed online that no longer
    /// answers).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn hang_connect_to(&self, ids: Vec<NodeId>) {
        *self.inner.hang.lock().unwrap_or_else(|e| e.into_inner()) = ids;
    }
}

impl Inner {
    async fn forward(
        &self,
        name: &str,
        req: Request<Bytes>,
        slack: Duration,
    ) -> Result<Response<Bytes>, ForwardError> {
        let frame = encode_request(name, req).inspect_err(|e| {
            warn!(service = name, error = %e, "request cannot be forwarded");
        })?;
        let manifest = match fetch_manifest(&self.node, name).await {
            Ok(Some(m)) => m,
            // Undeployed since the caller's check.
            Ok(None) => return Err(ForwardError::NotDeployed),
            Err(()) => return Err(ForwardError::NoHost),
        };
        let candidates = self.candidates(&manifest);
        if candidates.is_empty() {
            warn!(service = name, "no online host runs the service");
            return Err(ForwardError::NoHost);
        }
        let Ok(_permit) = self.permits.acquire().await else {
            return Err(ForwardError::NoHost);
        };
        let per_attempt = Duration::from_millis(manifest.limits.request_timeout_ms) + slack;
        let mut timed_out = None;
        for peer in candidates.into_iter().take(MAX_ATTEMPTS) {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            match self.attempt(peer, &frame, per_attempt).await {
                Ok(resp) if retryable(&resp) => warn!(
                    service = name,
                    host = %peer.short(),
                    reason = %String::from_utf8_lossy(resp.body()),
                    "host cannot serve the service"
                ),
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    warn!(
                        service = name,
                        host = %peer.short(),
                        error = %e,
                        "forward to host failed"
                    );
                    if let ForwardError::Timeout(ms) = e {
                        timed_out = Some(ms);
                    }
                }
            }
        }
        Err(timed_out.map_or(ForwardError::NoHost, ForwardError::Timeout))
    }

    /// Online effective hosts the placement selects, except this node, by recent RTT
    /// (unknown RTT last).
    fn candidates(&self, m: &ServiceManifest) -> Vec<NodeId> {
        let me = self.node.node_id();
        let hosts = placed_hosts(m, &self.node);
        let mut members: Vec<_> = self
            .node
            .members()
            .into_iter()
            .filter(|s| s.id != me && s.online && s.effective && hosts.contains(&s.id))
            .collect();
        let first = self.first.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let rank = |id: &NodeId| first.iter().position(|f| f == id).unwrap_or(usize::MAX);
        members.sort_by_key(|s| (rank(&s.id), s.rtt_ms.is_none(), s.rtt_ms));
        members.into_iter().map(|s| s.id).collect()
    }

    /// One request to `peer` over a cached or new connection, within `limit`, of which
    /// connecting may take at most [`CONNECT_CAP`]. On any error the connection is closed
    /// and dropped from the cache.
    async fn attempt(
        &self,
        peer: NodeId,
        frame: &[u8],
        limit: Duration,
    ) -> Result<Response<Bytes>, ForwardError> {
        let deadline = Instant::now() + limit;
        let ms = limit.as_millis() as u64;
        let cap = limit.min(CONNECT_CAP);
        let conn = timeout_at(Instant::now() + cap, self.conn(peer))
            .await
            .map_err(|_| ForwardError::ConnectTimeout(cap.as_millis() as u64))??;
        let result = match timeout_at(deadline, exchange(&conn, frame)).await {
            Ok(r) => r.and_then(to_response),
            Err(_) => Err(ForwardError::Timeout(ms)),
        };
        if result.is_err() {
            self.discard(peer, &conn);
        }
        result
    }

    async fn conn(&self, peer: NodeId) -> Result<Connection, ForwardError> {
        #[cfg(any(test, feature = "test-hooks"))]
        if self
            .hang
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&peer)
        {
            std::future::pending::<()>().await;
        }
        {
            let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
            match conns.get(&peer) {
                Some(c) if c.close_reason().is_none() => return Ok(c.clone()),
                Some(_) => {
                    conns.remove(&peer);
                }
                None => {}
            }
        }
        let conn = self.node.connect(peer, ALPN_HTTPFWD).await?;
        self.connects.fetch_add(1, Ordering::SeqCst);
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        // A concurrent request may have connected meanwhile: keep one connection.
        if let Some(c) = conns.get(&peer).filter(|c| c.close_reason().is_none()) {
            conn.close(DROPPED, b"duplicate");
            return Ok(c.clone());
        }
        conns.insert(peer, conn.clone());
        Ok(conn)
    }

    fn discard(&self, peer: NodeId, conn: &Connection) {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        if conns
            .get(&peer)
            .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            conns.remove(&peer);
        }
        conn.close(DROPPED, b"httpfwd error");
    }
}

/// Why a `_ping` failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PingError {
    /// No answer within the limit (milliseconds).
    #[error("timed out after {0} ms")]
    Timeout(u64),
    #[error("connect failed: {0}")]
    Connect(String),
    /// The peer answered something other than `204`.
    #[error("unexpected status {0}")]
    Status(u16),
    /// The request or response frame, or the stream, failed.
    #[error("{0}")]
    Frame(String),
}

/// Where the time of a successful [`ping`] went.
#[derive(Clone, Copy, Debug)]
pub struct PingTiming {
    /// Dialling the peer.
    pub connect: Duration,
    /// Request out, response back, on the new connection.
    pub exchange: Duration,
    /// Both.
    pub total: Duration,
}

/// Closes its connection when dropped, whichever way the ping ends (or is cancelled).
struct CloseOnDrop(Connection);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close(DROPPED, b"ping done");
    }
}

/// Sends one `_ping` `httpfwd` request straight to `peer` on a fresh connection (the cold
/// path), within `limit` in all, and reports the connect and exchange times. The peer
/// answers `204` without consulting its services, whatever its state. Uses the
/// forwarder's frame helpers but not its connection cache or its host selection.
pub async fn ping(
    node: &NodeHandle,
    peer: NodeId,
    limit: Duration,
) -> Result<PingTiming, PingError> {
    let req = Request::get("/")
        .body(Bytes::new())
        .map_err(|e| PingError::Frame(e.to_string()))?;
    let frame = encode_request(PING_SERVICE, req).map_err(|e| PingError::Frame(e.to_string()))?;
    let start = Instant::now();
    let deadline = start + limit;
    let timed_out = || PingError::Timeout(limit.as_millis() as u64);
    // The dial gets a little more than the deadline, so that the deadline reports it.
    let dial = limit + Duration::from_secs(1);
    let conn = timeout_at(deadline, node.connect_within(peer, ALPN_HTTPFWD, dial))
        .await
        .map_err(|_| timed_out())?
        .map_err(|e| PingError::Connect(e.to_string()))?;
    let conn = CloseOnDrop(conn);
    let connected = Instant::now();
    let fr = timeout_at(deadline, exchange(&conn.0, &frame))
        .await
        .map_err(|_| timed_out())?
        .map_err(|e| PingError::Frame(e.to_string()))?;
    let done = Instant::now();
    if fr.status != 204 {
        return Err(PingError::Status(fr.status));
    }
    Ok(PingTiming {
        connect: connected - start,
        exchange: done - connected,
        total: done - start,
    })
}

fn retryable(resp: &Response<Bytes>) -> bool {
    resp.status() == StatusCode::SERVICE_UNAVAILABLE
        && RETRYABLE
            .iter()
            .any(|b| resp.body().as_ref() == b.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_503_bodies_retry() {
        let resp = |code: u16, body: &'static str| {
            let mut r = Response::new(Bytes::from_static(body.as_bytes()));
            *r.status_mut() = StatusCode::from_u16(code).unwrap();
            r
        };
        for b in RETRYABLE {
            assert!(retryable(&resp(503, b)));
        }
        assert!(!retryable(&resp(503, "Memory budget exhausted")));
        assert!(!retryable(&resp(500, "Service not yet replicated")));
        assert!(!retryable(&resp(200, "")));
    }
}
