//! The localhost HTTP/1.1 server (spec §4.1): listeners, per-request routing, limits.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::forward::Forwarder;
use crate::routing::{Target, route_main, route_mapped};
use crate::status_page::Snapshot;

/// Open connections across all listeners. Past this, `accept` is not called: further
/// connections wait in the kernel backlog until one closes.
const MAX_CONNECTIONS: usize = 256;
/// Time for a new connection to send its first request's headers.
const FIRST_HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Keep-alive idle time. hyper's header-read timer also runs while a connection waits for
/// its next request, so it is the idle timeout.
const KEEP_ALIVE_IDLE: Duration = Duration::from_secs(60);
/// Request body cap (spec §4.1): 64 MiB.
const MAX_BODY: u64 = 64 * 1024 * 1024;
/// How long shutdown waits for in-flight connections before cutting them off.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

struct Shared {
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
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            p = permits.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
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
    let seen = Arc::new(AtomicBool::new(false));
    let svc = {
        let seen = seen.clone();
        service_fn(move |req| {
            seen.store(true, Ordering::Relaxed);
            handle(shared.clone(), mapped.clone(), req)
        })
    };
    let conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(KEEP_ALIVE_IDLE)
        .keep_alive(true)
        .serve_connection(TokioIo::new(stream), svc);
    tokio::pin!(conn);
    let first_deadline = sleep(FIRST_HEAD_TIMEOUT);
    tokio::pin!(first_deadline);
    let mut first_done = false;
    let mut closing = false;
    loop {
        tokio::select! {
            _ = &mut conn => break,
            _ = &mut first_deadline, if !first_done => {
                // No request head by now: drop the connection. Otherwise the first
                // request arrived in time and only hyper's idle timer applies.
                if !seen.load(Ordering::Relaxed) {
                    break;
                }
                first_done = true;
            }
            _ = cancel.cancelled(), if !closing => {
                closing = true;
                conn.as_mut().graceful_shutdown();
            }
        }
    }
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

async fn route(shared: &Shared, mapped: Option<&str>, req: Request<Incoming>) -> Resp {
    let host = match req.headers().get(header::HOST) {
        Some(v) => v.to_str().ok().map(str::to_owned),
        None => req.uri().authority().map(|a| a.to_string()),
    };
    let Some(host) = host else {
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
    let body = match Limited::new(body, MAX_BODY as usize).collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => return too_large(),
        Err(_) => return text(StatusCode::BAD_REQUEST, "Bad request body"),
    };
    let resp = shared
        .forwarder
        .dispatch(name, Request::from_parts(parts, body))
        .await;
    resp.map(Full::new)
}
