//! The localhost HTTP/1.1 server (spec §4.1): listeners, per-request routing, limits.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use mesh_core::NodeHandle;
use mesh_services::Services;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::forward::Forwarder;
use crate::routing::{Target, normalize_host, route_main, route_mapped};
use crate::status_page::Snapshot;

/// Open connections across all listeners. Past this, accepted connections wait (unserved)
/// for a free slot.
const MAX_CONNECTIONS: usize = 256;
/// Time to receive a request's headers, counted from the end of the previous response (or
/// the connection's start), so it is also the keep-alive idle timeout.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Time to receive a request's body.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Request body cap (spec §4.1): 64 MiB.
const MAX_BODY: u64 = 64 * 1024 * 1024;
/// How long shutdown waits for in-flight connections before cutting them off.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Lingering close: how long a finished connection keeps reading (and discarding) after
/// its FIN. Far above a loopback round trip plus client scheduling, short enough that a
/// client which never stops sending holds a connection slot only briefly.
const LINGER_TIME: Duration = Duration::from_secs(2);
/// Lingering close: the most it discards. Covers the tail a client already has in flight
/// when it sees the response (socket buffers are a few MiB), while bounding the work a
/// client that ignores the response can make the gateway do.
const LINGER_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The server's time limits; tests shorten them.
#[derive(Clone, Copy)]
struct Timeouts {
    header_read: Duration,
    body_read: Duration,
}

const TIMEOUTS: Timeouts = Timeouts {
    header_read: HEADER_READ_TIMEOUT,
    body_read: BODY_READ_TIMEOUT,
};

struct Shared {
    timeouts: Timeouts,
    forwarder: Forwarder,
    node: NodeHandle,
    services: Arc<Services>,
    addrs: Vec<(SocketAddr, Option<String>)>,
}

/// The running gateway. [`Gateway::shutdown`] releases everything it holds.
pub struct Gateway {
    addrs: Vec<(SocketAddr, Option<String>)>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl Gateway {
    /// Binds the main listener (failure is an error) and one per port mapping (failure is
    /// logged and skipped), then serves until [`Gateway::shutdown`].
    pub async fn start(
        cfg: GatewayConfig,
        forwarder: Forwarder,
        node: NodeHandle,
        services: Arc<Services>,
    ) -> Result<Gateway, GatewayError> {
        Self::start_inner(cfg, forwarder, node, services, TIMEOUTS).await
    }

    /// [`Gateway::start`] with custom header-read (also keep-alive idle) and body-read
    /// timeouts.
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn start_with_timeouts(
        cfg: GatewayConfig,
        forwarder: Forwarder,
        node: NodeHandle,
        services: Arc<Services>,
        header_read: Duration,
        body_read: Duration,
    ) -> Result<Gateway, GatewayError> {
        let t = Timeouts {
            header_read,
            body_read,
        };
        Self::start_inner(cfg, forwarder, node, services, t).await
    }

    async fn start_inner(
        cfg: GatewayConfig,
        forwarder: Forwarder,
        node: NodeHandle,
        services: Arc<Services>,
        timeouts: Timeouts,
    ) -> Result<Gateway, GatewayError> {
        cfg.validate()?;
        let main = TcpListener::bind(cfg.listen)
            .await
            .map_err(|source| GatewayError::Bind {
                addr: cfg.listen,
                source,
            })?;
        let mut listeners = vec![(main, None)];
        for (port, svc) in &cfg.ports {
            let want = SocketAddr::new(cfg.listen.ip(), *port);
            match TcpListener::bind(want).await {
                Ok(l) => listeners.push((l, Some(svc.clone()))),
                Err(e) => error!("gateway: cannot listen on {want} for {svc}: {e}; skipped"),
            }
        }
        let mut addrs = Vec::new();
        for (l, svc) in &listeners {
            let addr = l.local_addr().map_err(|source| GatewayError::Bind {
                addr: cfg.listen,
                source,
            })?;
            addrs.push((addr, svc.clone()));
        }
        let shared = Arc::new(Shared {
            timeouts,
            forwarder,
            node,
            services,
            addrs: addrs.clone(),
        });
        let cancel = CancellationToken::new();
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let tasks = listeners
            .into_iter()
            .map(|(listener, mapped)| {
                tokio::spawn(accept_loop(
                    listener,
                    mapped,
                    shared.clone(),
                    permits.clone(),
                    cancel.clone(),
                ))
            })
            .collect();
        Ok(Gateway {
            addrs,
            cancel,
            tasks,
        })
    }

    /// The bound listeners: the main one (`None`) first, then each mapped port with its
    /// service.
    pub fn local_addrs(&self) -> Vec<(SocketAddr, Option<String>)> {
        self.addrs.clone()
    }

    /// Stops accepting, lets in-flight requests finish (at most a few seconds), and drops
    /// every reference to the forwarder, node handle and services.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for t in std::mem::take(&mut self.tasks) {
            let _ = t.await;
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn accept_loop(
    listener: TcpListener,
    mapped: Option<String>,
    shared: Arc<Shared>,
    permits: Arc<Semaphore>,
    cancel: CancellationToken,
) {
    let mapped = Arc::new(mapped);
    let mut conns = JoinSet::new();
    loop {
        let stream = tokio::select! {
            _ = cancel.cancelled() => break,
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(e) => {
                    warn!("gateway: accept failed: {e}");
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        while conns.try_join_next().is_some() {}
        // Past the cap the accepted connection waits here for a free slot.
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            p = permits.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        conns.spawn(serve_conn(
            stream,
            shared.clone(),
            mapped.clone(),
            cancel.clone(),
            permit,
        ));
    }
    drop(listener);
    let drained = timeout(SHUTDOWN_GRACE, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        conns.shutdown().await;
    }
}

async fn serve_conn(
    stream: TcpStream,
    shared: Arc<Shared>,
    mapped: Arc<Option<String>>,
    cancel: CancellationToken,
    _permit: OwnedSemaphorePermit,
) {
    let _ = stream.set_nodelay(true);
    let timeouts = shared.timeouts;
    let svc = service_fn(move |req| handle(shared.clone(), mapped.clone(), req));
    let mut conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(timeouts.header_read)
        .keep_alive(true)
        .serve_connection(TokioIo::new(stream), svc);
    let mut closing = false;
    loop {
        tokio::select! {
            _ = &mut conn => break,
            _ = cancel.cancelled(), if !closing => {
                closing = true;
                std::pin::Pin::new(&mut conn).graceful_shutdown();
            }
        }
    }
    // On shutdown, close at once: the linger must not stretch the drain.
    if closing {
        return;
    }
    let stream = conn.into_parts().io.into_inner();
    tokio::select! {
        _ = linger(stream) => {}
        _ = cancel.cancelled() => {}
    }
}

/// Lingering close (as nginx and Apache do). Closing a socket that still has unread
/// request data makes the OS send a reset, and a reset can discard a response the client
/// has received but not yet read (Windows does) -- e.g. a `413` sent while the client is
/// still uploading. So: send our FIN, then read and discard until the client closes, or
/// [`LINGER_TIME`] or [`LINGER_MAX_BYTES`] runs out, whichever comes first.
async fn linger(mut stream: TcpStream) {
    // hyper has flushed and shut down its write side already when it ended cleanly;
    // after an error it may not have, so do it here (a repeat is harmless).
    let _ = stream.shutdown().await;
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = LINGER_MAX_BYTES;
    let _ = timeout(LINGER_TIME, async {
        while left > 0 {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => left = left.saturating_sub(n),
            }
        }
    })
    .await;
}

type Resp = Response<Full<Bytes>>;

fn reply(code: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Resp {
    let mut r = Response::new(Full::new(body.into()));
    *r.status_mut() = code;
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

fn text(code: StatusCode, body: &'static str) -> Resp {
    reply(code, "text/plain; charset=utf-8", body)
}

async fn handle(
    shared: Arc<Shared>,
    mapped: Arc<Option<String>>,
    req: Request<Incoming>,
) -> Result<Resp, Infallible> {
    Ok(route(&shared, mapped.as_deref(), req).await)
}

/// The request's Host: the single Host header, or the authority of an absolute-form
/// target. `None` (answered `400`) if there is neither, several Host headers, or an
/// absolute-form authority that differs from the Host header.
fn request_host(req: &Request<Incoming>) -> Option<String> {
    let mut hosts = req.headers().get_all(header::HOST).iter();
    let header_host = match (hosts.next(), hosts.next()) {
        (_, Some(_)) => return None,
        (Some(v), None) => Some(v.to_str().ok()?.to_owned()),
        (None, None) => None,
    };
    let authority = req.uri().authority().map(|a| a.to_string());
    match (header_host, authority) {
        (Some(h), Some(a)) if normalize_host(&h) != normalize_host(&a) => None,
        (Some(h), _) => Some(h),
        (None, a) => a,
    }
}

async fn route(shared: &Shared, mapped: Option<&str>, req: Request<Incoming>) -> Resp {
    let Some(host) = request_host(&req) else {
        return text(StatusCode::BAD_REQUEST, "Missing or invalid Host header");
    };
    let target = match mapped {
        None => route_main(&host),
        Some(svc) => route_mapped(&host, svc),
    };
    match target {
        Target::Unknown => text(StatusCode::NOT_FOUND, "Unknown host"),
        Target::Misdirected => text(StatusCode::MISDIRECTED_REQUEST, "Misdirected Request"),
        Target::Status => status(shared, &req),
        Target::Service(name) => service(shared, &name, req).await,
    }
}

fn status(shared: &Shared, req: &Request<Incoming>) -> Resp {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        let mut r = text(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return r;
    }
    let page = |json: bool| {
        let snap = Snapshot::take(&shared.node, &shared.services, &shared.addrs);
        let mut r = if json {
            reply(StatusCode::OK, "application/json", snap.json())
        } else {
            reply(StatusCode::OK, "text/html; charset=utf-8", snap.html())
        };
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        r
    };
    match req.uri().path() {
        "/" => page(false),
        "/status.json" => page(true),
        _ => text(StatusCode::NOT_FOUND, "Not found"),
    }
}

async fn service(shared: &Shared, name: &str, req: Request<Incoming>) -> Resp {
    let too_large = || text(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY) {
        return too_large();
    }
    let (parts, body) = req.into_parts();
    let read = timeout(
        shared.timeouts.body_read,
        Limited::new(body, MAX_BODY as usize).collect(),
    )
    .await;
    let body = match read {
        Err(_) => {
            let mut r = text(StatusCode::REQUEST_TIMEOUT, "Request body timed out");
            r.headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
            return r;
        }
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(e)) if e.downcast_ref::<LengthLimitError>().is_some() => return too_large(),
        Ok(Err(_)) => return text(StatusCode::BAD_REQUEST, "Bad request body"),
    };
    let resp = shared
        .forwarder
        .dispatch(name, Request::from_parts(parts, body))
        .await;
    resp.map(Full::new)
}
