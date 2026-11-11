//! The serving side of `httpfwd` (spec §3.2): one request frame and one response frame per
//! bidirectional stream, answered by [`Services::handle`], which serves locally only, so a
//! forwarded request is never forwarded again.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};
use iroh::endpoint::{RecvStream, SendStream, VarInt};
use mesh_core::frame::{read_frame, write_frame};
use mesh_core::{AcceptError, Connection, ProtocolHandler};
use mesh_proto::{ForwardRequest, ForwardResponse, HTTPFWD_MAX_FRAME, PING_SERVICE, is_hop_by_hop};
use mesh_services::Services;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::error::FrameConvError;

/// The request frame must arrive within this time, or the stream is reset.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The peer must take the response frame within this time, or the stream is reset.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Streams served at once on one connection.
const MAX_STREAMS: usize = 256;
/// Stream reset code: the request frame was late, oversized or malformed.
const BAD_REQUEST: VarInt = VarInt::from_u32(1);
/// Stream reset code: the response cannot be framed within the `httpfwd` limits.
const BAD_RESPONSE: VarInt = VarInt::from_u32(2);

const NOT_REPLICATED: &str = "Service not yet replicated";

/// The `httpfwd` protocol handler, registered through `NodeConfig::extra_protocols` (so
/// only effective members reach it) before the node's [`Services`] exist; they are bound
/// later through the [`HttpFwdSlot`]. Until then, and once they are dropped, every request
/// is answered `503 Service not yet replicated`.
#[derive(Clone)]
pub struct HttpFwdHandler {
    slot: Arc<SlotInner>,
}

/// Binds the [`Services`] that an [`HttpFwdHandler`] serves with.
#[derive(Clone)]
pub struct HttpFwdSlot {
    slot: Arc<SlotInner>,
}

/// An inbound stream starting or finishing being served.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Activity {
    /// A stream was accepted; it is read and answered from now on.
    Start,
    /// That stream is done: its response was written, or it failed, was reset or timed out.
    /// Every `Start` is followed by exactly one `End`.
    End,
}

type ActivityFn = Arc<dyn Fn(Activity) + Send + Sync>;

/// Reports `Start` on creation and `End` on drop, so that every exit path of
/// [`serve_stream`] is balanced.
struct ActivityGuard(Option<ActivityFn>);

impl ActivityGuard {
    fn start(slot: &SlotInner) -> ActivityGuard {
        let f = slot
            .activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(f) = &f {
            f(Activity::Start);
        }
        ActivityGuard(f)
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            f(Activity::End);
        }
    }
}

#[derive(Default)]
struct SlotInner {
    /// Weak: the handler lives in the node's router, and `Services` hold the node's
    /// handle; a strong reference would keep both alive forever.
    services: OnceLock<Weak<Services>>,
    /// Set by [`HttpFwdSlot::detach`]: new requests no longer reach the services.
    detached: AtomicBool,
    /// Set by [`HttpFwdSlot::set_paused`]: new requests are answered like `detached` ones,
    /// except `_ping`.
    paused: AtomicBool,
    /// Called when inbound streams start and finish being served.
    activity: Mutex<Option<ActivityFn>>,
    /// Requests answered (test hook).
    served: AtomicU64,
}

impl HttpFwdHandler {
    /// A handler and the slot that later binds its services.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> (HttpFwdHandler, HttpFwdSlot) {
        let slot = Arc::new(SlotInner::default());
        (HttpFwdHandler { slot: slot.clone() }, HttpFwdSlot { slot })
    }
}

impl HttpFwdSlot {
    /// Serves requests with `services` from now on. The slot keeps only a weak reference:
    /// the caller owns `services`. Only the first call takes effect.
    pub fn attach(&self, services: &Arc<Services>) {
        if self.slot.services.set(Arc::downgrade(services)).is_err() {
            warn!("httpfwd handler already attached; ignoring");
        }
    }

    /// Stops handing new requests to the services: they are answered `503 Service not yet
    /// replicated` (which the forwarder retries elsewhere), so that only requests already
    /// in flight still hold the services.
    pub fn detach(&self) {
        self.slot.detached.store(true, Ordering::SeqCst);
    }

    /// While paused, new requests are answered `503 Service not yet replicated` (as after
    /// [`detach`](Self::detach), but reversible); `_ping` is still answered.
    pub fn set_paused(&self, paused: bool) {
        self.slot.paused.store(paused, Ordering::SeqCst);
    }

    /// Calls `f` with [`Activity::Start`] when an inbound stream begins being served and
    /// [`Activity::End`] when it is done, whichever way. Replaces any earlier callback; it
    /// runs on the serving task, so it must not block.
    pub fn set_activity(&self, f: Arc<dyn Fn(Activity) + Send + Sync>) {
        *self.slot.activity.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// Requests this handler has answered, including the `503`s before attach.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn served(&self) -> u64 {
        self.slot.served.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for HttpFwdHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpFwdHandler")
    }
}

impl fmt::Debug for HttpFwdSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpFwdSlot")
    }
}

impl ProtocolHandler for HttpFwdHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let limit = Arc::new(Semaphore::new(MAX_STREAMS));
        let mut streams = JoinSet::new();
        loop {
            // A stream is accepted only once a slot is free; QUIC flow control holds back
            // further streams meanwhile.
            let Ok(permit) = limit.clone().acquire_owned().await else {
                break;
            };
            let (send, recv) = tokio::select! {
                s = conn.accept_bi() => match s {
                    Ok(s) => s,
                    Err(_) => break,
                },
                // Reap finished streams so the set stays small on long connections.
                Some(_) = streams.join_next(), if !streams.is_empty() => continue,
            };
            let slot = self.slot.clone();
            streams.spawn(async move {
                serve_stream(&slot, send, recv).await;
                drop(permit);
            });
        }
        while streams.join_next().await.is_some() {}
        Ok(())
    }
}

async fn serve_stream(slot: &SlotInner, mut send: SendStream, mut recv: RecvStream) {
    let _activity = ActivityGuard::start(slot);
    let req = match timeout(
        READ_TIMEOUT,
        read_frame::<_, ForwardRequest>(&mut recv, HTTPFWD_MAX_FRAME),
    )
    .await
    {
        Ok(Ok(Some(req))) => req,
        // The peer finished the stream without a request.
        Ok(Ok(None)) => {
            let _ = send.finish();
            return;
        }
        Ok(Err(e)) => {
            debug!(error = %e, "bad httpfwd request frame");
            reset(&mut send, &mut recv, BAD_REQUEST);
            return;
        }
        Err(_) => {
            debug!("httpfwd request frame not received in time");
            reset(&mut send, &mut recv, BAD_REQUEST);
            return;
        }
    };
    let service = req.service.clone();
    let resp = if service == PING_SERVICE {
        // Before the services lookup: answered whatever the state.
        no_content()
    } else {
        let services = if slot.detached.load(Ordering::SeqCst) || slot.paused.load(Ordering::SeqCst)
        {
            None
        } else {
            slot.services.get().and_then(Weak::upgrade)
        };
        match services {
            None => not_replicated(),
            Some(services) => match to_request(req) {
                Ok(r) => services.handle(&service, r).await,
                Err(e) => {
                    debug!(service, error = %e, "httpfwd request not convertible");
                    reset(&mut send, &mut recv, BAD_REQUEST);
                    return;
                }
            },
        }
    };
    slot.served.fetch_add(1, Ordering::SeqCst);
    let frame = from_response(resp);
    if let Err(e) = frame.validate() {
        warn!(service, error = %e, "response exceeds the httpfwd limits");
        reset(&mut send, &mut recv, BAD_RESPONSE);
        return;
    }
    match timeout(WRITE_TIMEOUT, write_frame(&mut send, &frame)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            debug!(service, error = %e, "httpfwd response not written");
            return;
        }
        Err(_) => {
            debug!(service, "httpfwd response not taken in time");
            reset(&mut send, &mut recv, BAD_RESPONSE);
            return;
        }
    }
    let _ = send.finish();
}

fn reset(send: &mut SendStream, recv: &mut RecvStream, code: VarInt) {
    let _ = send.reset(code);
    let _ = recv.stop(code);
}

fn no_content() -> Response<Bytes> {
    let mut resp = Response::new(Bytes::new());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    resp
}

fn not_replicated() -> Response<Bytes> {
    let mut resp = Response::new(Bytes::from_static(NOT_REPLICATED.as_bytes()));
    *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// The request a frame describes, without hop-by-hop headers. Header names and values are
/// checked as `http` checks them (no CR, LF or NUL).
fn to_request(fr: ForwardRequest) -> Result<Request<Bytes>, FrameConvError> {
    let method = Method::from_bytes(fr.method.as_bytes()).map_err(|_| FrameConvError::Method)?;
    let mut req = Request::new(Bytes::from(fr.body));
    *req.method_mut() = method;
    *req.uri_mut() = fr
        .path_and_query
        .parse()
        .map_err(|_| FrameConvError::Path)?;
    copy_headers(fr.headers, req.headers_mut())?;
    Ok(req)
}

/// Appends `headers` to `map`, skipping hop-by-hop ones.
pub(crate) fn copy_headers(
    headers: Vec<(String, Vec<u8>)>,
    map: &mut http::HeaderMap,
) -> Result<(), FrameConvError> {
    for (name, value) in headers {
        if is_hop_by_hop(&name) {
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| FrameConvError::HeaderName(name))?;
        let value = HeaderValue::from_bytes(&value).map_err(|_| FrameConvError::HeaderValue)?;
        map.append(name, value);
    }
    Ok(())
}

/// `headers` as frame headers, without hop-by-hop ones.
pub(crate) fn frame_headers(headers: &http::HeaderMap) -> Vec<(String, Vec<u8>)> {
    headers
        .iter()
        .filter(|(n, _)| !is_hop_by_hop(n.as_str()))
        .map(|(n, v)| (n.as_str().to_owned(), v.as_bytes().to_vec()))
        .collect()
}

fn from_response(resp: Response<Bytes>) -> ForwardResponse {
    let (parts, body) = resp.into_parts();
    ForwardResponse {
        status: parts.status.as_u16(),
        headers: frame_headers(&parts.headers),
        body: body.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fr(headers: Vec<(&str, &[u8])>) -> ForwardRequest {
        ForwardRequest {
            service: "web".into(),
            method: "PUT".into(),
            path_and_query: "/a/b?c=1".into(),
            headers: headers
                .into_iter()
                .map(|(n, v)| (n.to_owned(), v.to_vec()))
                .collect(),
            body: b"body".to_vec(),
        }
    }

    #[test]
    fn request_from_frame_strips_hop_by_hop() {
        let req = to_request(fr(vec![
            ("host", b"web.mesh.localhost"),
            ("Connection", b"close"),
            ("te", b"trailers"),
            ("proxy-connection", b"keep-alive"),
            ("x-a", b"1"),
            ("x-a", b"2"),
        ]))
        .unwrap();
        assert_eq!(req.method(), Method::PUT);
        assert_eq!(req.uri(), "/a/b?c=1");
        assert_eq!(req.body().as_ref(), b"body");
        let h = req.headers();
        assert_eq!(h.len(), 3);
        assert_eq!(h["host"], "web.mesh.localhost");
        let xa: Vec<_> = h.get_all("x-a").iter().collect();
        assert_eq!(xa, ["1", "2"]);
    }

    #[test]
    fn request_with_bad_header_rejected() {
        for (n, v) in [
            ("x-a", &b"a\r\nInjected: 1"[..]),
            ("x-a", b"a\nb"),
            ("x-a", b"a\0b"),
            ("bad name", b"v"),
            ("x\r\ny", b"v"),
            ("", b"v"),
        ] {
            assert!(to_request(fr(vec![(n, v)])).is_err(), "{n:?} {v:?}");
        }
    }

    #[test]
    fn response_to_frame_strips_hop_by_hop() {
        let resp = Response::builder()
            .status(201)
            .header("transfer-encoding", "chunked")
            .header("keep-alive", "timeout=5")
            .header("upgrade", "h2c")
            .header("content-type", "text/plain")
            .body(Bytes::from_static(b"x"))
            .unwrap();
        let f = from_response(resp);
        assert_eq!(f.status, 201);
        assert_eq!(f.body, b"x");
        assert_eq!(
            f.headers,
            [("content-type".to_string(), b"text/plain".to_vec())]
        );
    }
}
