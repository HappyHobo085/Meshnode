//! Serving one HTTP request with a fresh instance of a loaded component.

use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use wasmtime::Store;
use wasmtime_wasi_http::WasiHttpView;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;

use crate::RequestCtx;
use crate::engine::EPOCH_TICK;
use crate::host::{KvStatus, State};

/// Placeholder deadline, one hour in epoch ticks, so requests run until per-request
/// timeouts are wired from `Limits::request_timeout_ms`.
const PROVISIONAL_DEADLINE_TICKS: u64 = 3_600_000 / EPOCH_TICK.as_millis() as u64;

/// Instantiates `pre` in a new store, calls `wasi:http/incoming-handler.handle` and
/// collects the response. Every failure becomes an error status.
pub(crate) async fn serve(
    pre: &ProxyPre<State>,
    req: Request<Bytes>,
    ctx: RequestCtx,
) -> Response<Bytes> {
    let service = ctx.service;
    let kv_status = KvStatus::default();
    let state = State::new(&service, ctx.kv, ctx.node_id, kv_status.clone());
    let mut store = Store::new(pre.engine(), state);
    store.set_epoch_deadline(PROVISIONAL_DEADLINE_TICKS);

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
    let mut guest = AbortOnDrop(tokio::spawn(async move {
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
    let body = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return failed(&service, "response body", &e),
    };
    match (&mut guest.0).await {
        Ok(Ok(())) => Response::from_parts(parts, body),
        Ok(Err(e)) => trapped(&service, &kv_status, &e),
        Err(e) => failed(&service, "guest task", &e),
    }
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

/// A guest trap: the status of the KV error that caused it, else `500`.
fn trapped(service: &str, kv_status: &KvStatus, err: &dyn std::fmt::Debug) -> Response<Bytes> {
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
