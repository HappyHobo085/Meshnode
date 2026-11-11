//! The forwarding side of `httpfwd` (spec §3.3): serve a request locally when this node
//! runs the service, otherwise forward it to the best placed host.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode, header};
use iroh::endpoint::VarInt;
use mesh_core::frame::read_frame;
use mesh_core::membership::MESH_NS;
use mesh_core::{Connection, NodeHandle};
use mesh_proto::{
    ALPN_HTTPFWD, ForwardRequest, ForwardResponse, HTTPFWD_MAX_FRAME, MAX_HEADER_BYTES,
    MAX_HEADERS, MAX_PATH, NodeId, PING_SERVICE, Role, ServiceManifest, decode_raw,
};
use mesh_services::{LocalState, Services, manifest_key, placed_hosts};
use tokio::sync::Semaphore;
use tokio::time::{Instant, timeout_at};
use tracing::warn;

use crate::error::{ForwardError, FrameConvError, Unframeable};
use crate::serve_httpfwd::{copy_headers, frame_headers};

/// Hosts tried per request.
const MAX_ATTEMPTS: usize = 3;
/// Forwards in flight on this node at once.
const MAX_FORWARDS: usize = 64;
/// Added to the manifest's `request_timeout_ms` for each attempt.
const ATTEMPT_SLACK: Duration = Duration::from_secs(5);
/// Close code for a cached connection dropped after an error.
const DROPPED: VarInt = VarInt::from_u32(0);

const NOT_DEPLOYED: &str = "Service not deployed";
/// `503` bodies after which the next host is tried.
const RETRYABLE: [&str; 3] = [
    "Service not placed on this node",
    "Service not yet replicated",
    "Node re-bootstrapping",
];

/// Serves requests for mesh services: locally when this node runs the service, else
/// forwarded over `httpfwd` to a placed host. Cheap to clone; clones share the connection
/// cache and the forwarding limit.
#[derive(Clone)]
pub struct Forwarder {
    inner: Arc<Inner>,
}

struct Inner {
    node: NodeHandle,
    services: Arc<Services>,
    /// One connection per peer, dropped (and closed) after an error.
    conns: Mutex<HashMap<NodeId, Connection>>,
    permits: Semaphore,
    /// Forward attempts made (test hook).
    attempts: AtomicU64,
    /// Connections opened (test hook).
    connects: AtomicU64,
    /// Hosts tried first, in this order (test hook).
    first: Mutex<Vec<NodeId>>,
}

impl Forwarder {
    pub fn new(node: NodeHandle, services: Arc<Services>) -> Forwarder {
        Forwarder {
            inner: Arc::new(Inner {
                node,
                services,
                conns: Mutex::new(HashMap::new()),
                permits: Semaphore::new(MAX_FORWARDS),
                attempts: AtomicU64::new(0),
                connects: AtomicU64::new(0),
                first: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Answers a request for service `name`. Never fails: every outcome is an HTTP
    /// response (`404 Service not deployed`, `502 No host could serve <name>`, or the
    /// serving host's response).
    pub async fn dispatch(&self, name: &str, req: Request<Bytes>) -> Response<Bytes> {
        let inner = &self.inner;
        match inner.services.local_state(name).await {
            LocalState::Active if !inner.is_client() => {
                return inner.services.handle(name, req).await;
            }
            LocalState::NotDeployed => return text(StatusCode::NOT_FOUND, NOT_DEPLOYED.into()),
            _ => {}
        }
        inner.forward(name, req).await
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
}

impl Inner {
    fn is_client(&self) -> bool {
        let me = self.node.node_id();
        self.node
            .members()
            .iter()
            .any(|m| m.id == me && m.role == Role::Client)
    }

    async fn forward(&self, name: &str, req: Request<Bytes>) -> Response<Bytes> {
        let frame = match encode_request(name, req) {
            Ok(f) => f,
            Err(e) => {
                warn!(service = name, error = %e, "request cannot be forwarded");
                let code = e.status();
                return text(code, code.canonical_reason().unwrap_or("").into());
            }
        };
        let manifest = match self.node.store().get(MESH_NS, &manifest_key(name)).await {
            Ok(Some(raw)) => match decode_raw::<ServiceManifest>(&raw) {
                Ok(m) => m,
                Err(e) => {
                    warn!(service = name, error = %e, "cannot read the service manifest");
                    return no_host(name);
                }
            },
            // Undeployed since the local check.
            Ok(None) => return text(StatusCode::NOT_FOUND, NOT_DEPLOYED.into()),
            Err(e) => {
                warn!(service = name, error = %e, "cannot read the service manifest");
                return no_host(name);
            }
        };
        let candidates = self.candidates(&manifest);
        if candidates.is_empty() {
            warn!(service = name, "no online host runs the service");
            return no_host(name);
        }
        let Ok(_permit) = self.permits.acquire().await else {
            return no_host(name);
        };
        let per_attempt = Duration::from_millis(manifest.limits.request_timeout_ms) + ATTEMPT_SLACK;
        for peer in candidates.into_iter().take(MAX_ATTEMPTS) {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            match self.attempt(peer, &frame, per_attempt).await {
                Ok(resp) if retryable(&resp) => warn!(
                    service = name,
                    host = %peer.short(),
                    reason = %String::from_utf8_lossy(resp.body()),
                    "host cannot serve the service"
                ),
                Ok(resp) => return resp,
                Err(e) => warn!(
                    service = name,
                    host = %peer.short(),
                    error = %e,
                    "forward to host failed"
                ),
            }
        }
        no_host(name)
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

    /// One request to `peer` over a cached or new connection, within `limit`. On any
    /// error the connection is closed and dropped from the cache.
    async fn attempt(
        &self,
        peer: NodeId,
        frame: &[u8],
        limit: Duration,
    ) -> Result<Response<Bytes>, ForwardError> {
        let deadline = Instant::now() + limit;
        let timeout_ms = || ForwardError::Timeout(limit.as_millis() as u64);
        let conn = timeout_at(deadline, self.conn(peer))
            .await
            .map_err(|_| timeout_ms())??;
        let result = match timeout_at(deadline, exchange(&conn, frame)).await {
            Ok(r) => r.and_then(to_response),
            Err(_) => Err(timeout_ms()),
        };
        if result.is_err() {
            self.discard(peer, &conn);
        }
        result
    }

    async fn conn(&self, peer: NodeId) -> Result<Connection, ForwardError> {
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

/// Sends one request frame on a new stream and reads the response frame (at most
/// `HTTPFWD_MAX_FRAME`; a larger length prefix fails at once).
async fn exchange(conn: &Connection, frame: &[u8]) -> Result<ForwardResponse, ForwardError> {
    let stream = |e: &dyn std::fmt::Display| ForwardError::Stream(e.to_string());
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| stream(&e))?;
    send.write_all(frame).await.map_err(|e| stream(&e))?;
    send.finish().map_err(|e| stream(&e))?;
    read_frame::<_, ForwardResponse>(&mut recv, HTTPFWD_MAX_FRAME)
        .await?
        .ok_or(ForwardError::NoResponse)
}

/// The whole `httpfwd` frame (length prefix included) for `req`, without hop-by-hop
/// headers.
fn encode_request(name: &str, req: Request<Bytes>) -> Result<Vec<u8>, Unframeable> {
    let (parts, body) = req.into_parts();
    if body.len() > HTTPFWD_MAX_FRAME {
        return Err(Unframeable::TooLarge);
    }
    let fr = ForwardRequest {
        service: name.to_owned(),
        method: parts.method.as_str().to_owned(),
        path_and_query: parts
            .uri
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned(),
        headers: frame_headers(&parts.headers),
        body: body.to_vec(),
    };
    let header_bytes: usize = fr.headers.iter().map(|(n, v)| n.len() + v.len()).sum();
    if fr.headers.len() > MAX_HEADERS || header_bytes > MAX_HEADER_BYTES {
        return Err(Unframeable::HeaderFields);
    }
    if fr.path_and_query.len() > MAX_PATH {
        return Err(Unframeable::Uri);
    }
    fr.validate().map_err(Unframeable::Invalid)?;
    let body = mesh_proto::encode(&fr);
    if body.len() > HTTPFWD_MAX_FRAME {
        return Err(Unframeable::TooLarge);
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// The response a frame describes, without hop-by-hop headers.
fn to_response(fr: ForwardResponse) -> Result<Response<Bytes>, ForwardError> {
    // An informational status is no final answer to a forwarded request.
    if fr.status < 200 {
        return Err(FrameConvError::Status.into());
    }
    let status = StatusCode::from_u16(fr.status).map_err(|_| FrameConvError::Status)?;
    let mut resp = Response::new(Bytes::from(fr.body));
    *resp.status_mut() = status;
    copy_headers(fr.headers, resp.headers_mut())?;
    Ok(resp)
}

fn retryable(resp: &Response<Bytes>) -> bool {
    resp.status() == StatusCode::SERVICE_UNAVAILABLE
        && RETRYABLE
            .iter()
            .any(|b| resp.body().as_ref() == b.as_bytes())
}

fn no_host(name: &str) -> Response<Bytes> {
    text(
        StatusCode::BAD_GATEWAY,
        format!("No host could serve {name}"),
    )
}

fn text(code: StatusCode, body: String) -> Response<Bytes> {
    let mut resp = Response::new(Bytes::from(body));
    *resp.status_mut() = code;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frame_strips_hop_by_hop() {
        let req = Request::post("http://web.mesh.localhost/p?q=1")
            .header("host", "web.mesh.localhost")
            .header("connection", "close")
            .header("transfer-encoding", "chunked")
            .header("x-a", "1")
            .body(Bytes::from_static(b"b"))
            .unwrap();
        let frame = encode_request("web", req).unwrap();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let fr: ForwardRequest = mesh_proto::decode(&frame[4..]).unwrap();
        assert_eq!(fr.service, "web");
        assert_eq!(fr.method, "POST");
        assert_eq!(fr.path_and_query, "/p?q=1");
        assert_eq!(fr.body, b"b");
        assert_eq!(
            fr.headers,
            [
                ("host".to_string(), b"web.mesh.localhost".to_vec()),
                ("x-a".to_string(), b"1".to_vec()),
            ]
        );
    }

    #[test]
    fn response_from_frame_rejects_bad_headers() {
        let fr = |headers: Vec<(String, Vec<u8>)>| ForwardResponse {
            status: 200,
            headers,
            body: Vec::new(),
        };
        for h in [
            ("x-a".to_string(), b"a\r\nb".to_vec()),
            ("bad name".to_string(), b"v".to_vec()),
        ] {
            assert!(to_response(fr(vec![h])).is_err());
        }
        let ok = to_response(fr(vec![
            ("upgrade".into(), b"h2c".to_vec()),
            ("x-a".into(), b"1".to_vec()),
        ]))
        .unwrap();
        assert_eq!(ok.headers().len(), 1);
        assert_eq!(ok.headers()["x-a"], "1");
    }

    #[test]
    fn unframeable_requests_get_their_status() {
        let get = |path: &str| Request::get(path.to_owned());
        let mut many = get("/");
        for i in 0..=MAX_HEADERS {
            many = many.header(format!("x-{i}"), "v");
        }
        let cases = [
            (many.body(Bytes::new()).unwrap(), 431),
            (
                get("/")
                    .header("x-big", "a".repeat(MAX_HEADER_BYTES))
                    .body(Bytes::new())
                    .unwrap(),
                431,
            ),
            (
                get(&format!("/{}", "a".repeat(MAX_PATH)))
                    .body(Bytes::new())
                    .unwrap(),
                414,
            ),
            (
                get("/")
                    .body(Bytes::from(vec![0u8; HTTPFWD_MAX_FRAME + 1]))
                    .unwrap(),
                413,
            ),
            // The body fits, but not with the rest of the frame.
            (
                get("/")
                    .body(Bytes::from(vec![0u8; HTTPFWD_MAX_FRAME - 8]))
                    .unwrap(),
                413,
            ),
        ];
        for (req, want) in cases {
            let err = encode_request("web", req).unwrap_err();
            assert_eq!(err.status().as_u16(), want, "{err}");
        }
        // At the limits it still frames.
        let mut most = get(&format!("/{}", "a".repeat(MAX_PATH - 1)));
        for i in 0..MAX_HEADERS {
            most = most.header(format!("x-{i}"), "v");
        }
        assert!(encode_request("web", most.body(Bytes::new()).unwrap()).is_ok());
    }

    #[test]
    fn informational_peer_status_rejected() {
        for status in [100, 101, 199] {
            let fr = ForwardResponse {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            };
            assert!(to_response(fr).is_err(), "{status}");
        }
    }

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
