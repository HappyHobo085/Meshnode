//! Serving one HTTP request with a fresh instance of a loaded component, within the
//! service's limits (sub-spec 3 §3.3).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use mesh_proto::Limits;
use tokio::sync::Semaphore;
use wasmtime::{Store, Trap, UpdateDeadline};
use wasmtime_wasi_http::WasiHttpView;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

use crate::RequestCtx;
use crate::engine::TickerHandle;
use crate::host::{KvStatus, State};

/// Node-wide state shared by every request a `Runtime` serves.
pub(crate) struct Shared {
    /// One permit per request allowed to run at once.
    pub(crate) in_flight: Arc<Semaphore>,
    /// The memory budget in MiB: the permits of `memory` when none is reserved.
    pub(crate) max_memory_mb: u32,
    /// One permit per MiB of the budget; a request reserves its `limits.memory_mb`.
    pub(crate) memory: Arc<Semaphore>,
    /// Instances created so far (a test hook reads it).
    pub(crate) instantiations: Arc<AtomicU64>,
    /// Keeps the epoch ticking while a guest runs.
    pub(crate) ticker: TickerHandle,
    /// The `wasi:clocks/wall-clock` time every guest sees, in ms since the UNIX epoch, or
    /// [`REAL_WALL_CLOCK`] for the host's real clock. Only a test hook changes it
    /// (`Runtime::set_wall_clock_ms`).
    pub(crate) fixed_wall_ms: Arc<AtomicI64>,
}

/// [`Shared::fixed_wall_ms`] when guests see the real wall clock.
pub(crate) const REAL_WALL_CLOCK: i64 = i64::MIN;

/// The body of the `503` for a request the memory budget cannot take.
const MEMORY_EXHAUSTED: &str = "Memory budget exhausted";

/// Serves `req` within `ctx.limits`: an oversized request body is refused before
/// anything is instantiated (`413`), and so is a `memory_mb` larger than the whole
/// memory budget (`503`); everything after that, including the waits for an in-flight
/// slot and for the memory reservation, and host calls, runs under `request_timeout_ms`
/// (`504`, or `503` if it ran out while waiting for memory).
pub(crate) async fn serve(
    pre: &ProxyPre<State>,
    shared: &Shared,
    req: Request<Bytes>,
    ctx: RequestCtx,
) -> Response<Bytes> {
    let start = Instant::now();
    let limits = ctx.limits;
    let body_len = req.body().len() as u64;
    if body_len > limits.max_body_bytes {
        tracing::warn!(
            service = ctx.service,
            "request body of {body_len} bytes exceeds the {} byte limit",
            limits.max_body_bytes
        );
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    }
    if limits.memory_mb > shared.max_memory_mb {
        tracing::warn!(
            service = ctx.service,
            "request needs {} MiB, more than the whole {} MiB memory budget",
            limits.memory_mb,
            shared.max_memory_mb
        );
        return memory_exhausted();
    }
    let timeout = Duration::from_millis(limits.request_timeout_ms);
    let service = ctx.service.clone();
    // On timeout `run` is dropped, which aborts the guest task (`AbortOnDrop`); a guest
    // busy in wasm yields every epoch tick, so the abort takes effect at its next yield,
    // and its epoch callback traps it once `deadline` has passed.
    // Limits from a validated manifest are at most 300 s; cap absurd values instead of
    // overflowing `Instant`.
    let deadline = start
        .checked_add(timeout)
        .unwrap_or_else(|| start + Duration::from_secs(86_400));
    let mut waiting_for_memory = false;
    let running = run(pre, shared, req, ctx, deadline, &mut waiting_for_memory);
    match tokio::time::timeout_at(deadline.into(), running).await {
        Ok(resp) => resp,
        Err(_) if waiting_for_memory => {
            tracing::warn!(service, "no memory free within {timeout:?}");
            memory_exhausted()
        }
        Err(_) => {
            tracing::warn!(service, "request timed out after {timeout:?}");
            status(StatusCode::GATEWAY_TIMEOUT)
        }
    }
}

/// Waits for an in-flight slot, then reserves `limits.memory_mb` of the memory budget
/// (`waiting_for_memory` is true during that wait), instantiates `pre` in a new store
/// that yields every epoch tick and traps once `deadline` has passed, calls
/// `wasi:http/incoming-handler.handle` and collects the response. Every failure becomes
/// an error status.
async fn run(
    pre: &ProxyPre<State>,
    shared: &Shared,
    req: Request<Bytes>,
    ctx: RequestCtx,
    deadline: Instant,
    waiting_for_memory: &mut bool,
) -> Response<Bytes> {
    let RequestCtx {
        service,
        node_id,
        kv,
        limits,
        outbound,
    } = ctx;
    // The permits move into the guest task, so the slot and the memory stay taken until
    // the guest has actually stopped, not merely until this request has been answered.
    let permit = match shared.in_flight.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(e) => return failed(&service, "in-flight slot", &e),
    };
    *waiting_for_memory = true;
    let reserved = shared
        .memory
        .clone()
        .acquire_many_owned(limits.memory_mb)
        .await;
    *waiting_for_memory = false;
    let reserved = match reserved {
        Ok(reserved) => reserved,
        Err(e) => return failed(&service, "memory reservation", &e),
    };

    let kv_status = KvStatus::default();
    let state = State::new(
        &service,
        outbound,
        kv,
        node_id,
        kv_status.clone(),
        limits.memory_mb,
        Some(shared.fixed_wall_ms.load(Ordering::Relaxed)).filter(|&ms| ms != REAL_WALL_CLOCK),
    );
    let mut store = Store::new(pre.engine(), state);
    store.limiter(State::limiter);
    // Every epoch tick the guest yields to the executor, so a spinning guest never holds
    // a worker thread; once `deadline` has passed (the wait for a slot included) it traps
    // with `Trap::Interrupt` (`504`).
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_| {
        if Instant::now() >= deadline {
            Ok(UpdateDeadline::Interrupt)
        } else {
            Ok(UpdateDeadline::Yield(1))
        }
    });

    let scheme = match req.uri().scheme_str() {
        Some("https") => Scheme::Https,
        _ => Scheme::Http,
    };
    let req = with_authority(req, &service).map(Full::new);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let resources = store
        .data_mut()
        .http()
        .new_incoming_request(scheme, req)
        .and_then(|req| Ok((req, store.data_mut().http().new_response_outparam(tx)?)));
    let (req, out) = match resources {
        Ok(r) => r,
        Err(e) => return failed(&service, "prepare request", &e),
    };

    let pre = pre.clone();
    let instantiations = shared.instantiations.clone();
    let busy = shared.ticker.busy();
    let mut guest = AbortOnDrop(tokio::spawn(async move {
        let _permit = permit;
        let _reserved = reserved;
        let _ticking = busy;
        instantiations.fetch_add(1, Ordering::Relaxed);
        let proxy = pre.instantiate_async(&mut store).await?;
        proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, req, out)
            .await
    }));

    let resp = match rx.await {
        Ok(Ok(resp)) => resp,
        Ok(Err(code)) => return failed(&service, "guest returned an error", &code),
        Err(_) => {
            return match (&mut guest.0).await {
                Ok(Err(e)) => trapped(&service, &kv_status, &e),
                Ok(Ok(())) => failed(&service, "guest set no response", &""),
                Err(e) => failed(&service, "guest task", &e),
            };
        }
    };
    let (parts, body) = resp.into_parts();
    let body = match collect_capped(body, limits).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => {
            tracing::warn!(
                service,
                "response body exceeds the {} byte limit",
                limits.max_body_bytes
            );
            return status(StatusCode::INTERNAL_SERVER_ERROR);
        }
        // A broken body stream usually means the guest trapped mid-body; report the trap.
        Err(BodyError::Stream(err)) => {
            return match (&mut guest.0).await {
                Ok(Err(e)) => trapped(&service, &kv_status, &e),
                _ => failed(&service, "response body", &err),
            };
        }
    };
    match (&mut guest.0).await {
        Ok(Ok(())) => Response::from_parts(parts, body),
        Ok(Err(e)) => trapped(&service, &kv_status, &e),
        Err(e) => failed(&service, "guest task", &e),
    }
}

enum BodyError {
    TooLarge,
    Stream(wasmtime_wasi_http::Error),
}

/// Reads `body` frame by frame, failing as soon as it would exceed `max_body_bytes`, so
/// at most the limit plus one frame is ever buffered. Trailers are ignored.
async fn collect_capped(mut body: HyperOutgoingBody, limits: Limits) -> Result<Bytes, BodyError> {
    let mut buf = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(BodyError::Stream)?;
        if let Ok(data) = frame.into_data() {
            if (buf.len() + data.len()) as u64 > limits.max_body_bytes {
                return Err(BodyError::TooLarge);
            }
            buf.extend_from_slice(&data);
        }
    }
    Ok(buf.freeze())
}

/// `wasi:http` needs an authority; a path-only request without `Host` gets the service name.
fn with_authority(mut req: Request<Bytes>, service: &str) -> Request<Bytes> {
    if req.uri().authority().is_none()
        && !req.headers().contains_key(header::HOST)
        && let Ok(host) = header::HeaderValue::from_str(service)
    {
        req.headers_mut().insert(header::HOST, host);
    }
    req
}

/// A guest trap: `504` for an expired epoch deadline, else the status of the KV error
/// that caused it, else `500` (out-of-memory included).
fn trapped(service: &str, kv_status: &KvStatus, err: &wasmtime::Error) -> Response<Bytes> {
    if err.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
        tracing::warn!(service, "guest exceeded its deadline: {err:?}");
        return status(StatusCode::GATEWAY_TIMEOUT);
    }
    match kv_status.get() {
        Some(code) => {
            tracing::warn!(service, "guest trapped on a KV error: {err:?}");
            status(code)
        }
        None => failed(service, "guest trapped", err),
    }
}

fn failed(service: &str, what: &str, err: &dyn std::fmt::Debug) -> Response<Bytes> {
    tracing::warn!(service, "{what}: {err:?}");
    status(StatusCode::INTERNAL_SERVER_ERROR)
}

fn memory_exhausted() -> Response<Bytes> {
    let mut resp = status(StatusCode::SERVICE_UNAVAILABLE);
    *resp.body_mut() = Bytes::from_static(MEMORY_EXHAUSTED.as_bytes());
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn status(code: StatusCode) -> Response<Bytes> {
    let mut resp = Response::new(Bytes::new());
    *resp.status_mut() = code;
    resp
}

/// Stops the guest task if the request future is dropped.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
