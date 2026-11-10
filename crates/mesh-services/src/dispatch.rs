//! Request dispatch: [`Services::handle`] (spec §5.3) and [`Services::local_state`], both
//! answered from [`Services::route`] so they always agree.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Response, StatusCode, header};
use mesh_core::NodeStatus;
use mesh_runtime::RequestCtx;
use tracing::warn;

use crate::Services;
use crate::deployer::{Active, Payload};
use crate::kv::ServiceKv;
use crate::manifest::{data_ns, read_manifest};
use crate::placement::placed;

const NOT_REPLICATED: &str = "Service not yet replicated";
const NOT_PLACED: &str = "Service not placed on this node";
const NOT_DEPLOYED: &str = "Service not deployed";
const REBOOTSTRAPPING: &str = "Node re-bootstrapping";

/// Whether this node serves a service itself right now, and if not, why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalState {
    /// Served here.
    Active,
    /// Deployed, but its placement does not select this node
    /// (`503 Service not placed on this node`).
    NotPlaced,
    /// Placed here, but not fetched and loaded yet (`503 Service not yet replicated`).
    /// Also reported when this node cannot read the service's manifest.
    NotReady,
    /// This node is re-bootstrapping; its store is incomplete (`503 Node re-bootstrapping`).
    Frozen,
    /// No manifest (`404 Service not deployed`).
    NotDeployed,
}

/// What this node does with a request for a service.
enum Route {
    Serve(Arc<Active>),
    /// Refused with the status of this state (never `Active`).
    Refuse(Refusal),
    /// The manifest cannot be read here (`500`).
    Unreadable,
}

#[derive(Clone, Copy)]
enum Refusal {
    NotPlaced,
    NotReady,
    Frozen,
    NotDeployed,
}

impl Services {
    /// Serves one request for service `name` on this node. Never fails: every outcome is
    /// an HTTP response.
    pub async fn handle(&self, name: &str, req: http::Request<Bytes>) -> Response<Bytes> {
        match self.route(name).await {
            Route::Serve(active) => self.serve(name, &active, req).await,
            Route::Refuse(Refusal::Frozen) => {
                text(StatusCode::SERVICE_UNAVAILABLE, REBOOTSTRAPPING)
            }
            Route::Refuse(Refusal::NotPlaced) => text(StatusCode::SERVICE_UNAVAILABLE, NOT_PLACED),
            Route::Refuse(Refusal::NotReady) => {
                text(StatusCode::SERVICE_UNAVAILABLE, NOT_REPLICATED)
            }
            Route::Refuse(Refusal::NotDeployed) => text(StatusCode::NOT_FOUND, NOT_DEPLOYED),
            Route::Unreadable => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Service manifest unreadable",
            ),
        }
    }

    /// What [`Services::handle`] would do with a request for `name` now: serve it
    /// (`Active`), or answer the `503` or `404` of the other states. A manifest this node
    /// cannot read (`handle` answers `500`) is reported as `NotReady`: another host may
    /// still serve the service.
    pub async fn local_state(&self, name: &str) -> LocalState {
        match self.route(name).await {
            Route::Serve(_) => LocalState::Active,
            Route::Refuse(Refusal::NotPlaced) => LocalState::NotPlaced,
            Route::Refuse(Refusal::NotReady) | Route::Unreadable => LocalState::NotReady,
            Route::Refuse(Refusal::Frozen) => LocalState::Frozen,
            Route::Refuse(Refusal::NotDeployed) => LocalState::NotDeployed,
        }
    }

    async fn route(&self, name: &str) -> Route {
        let shared = &self.shared;
        // The store is being rebuilt: its KV data and manifests are incomplete.
        if matches!(shared.node.status(), NodeStatus::Rebootstrapping { .. }) {
            return Route::Refuse(Refusal::Frozen);
        }
        if let Some(active) = shared.active(name) {
            return Route::Serve(active);
        }
        match read_manifest(shared.node.store(), name).await {
            Ok(None) => Route::Refuse(Refusal::NotDeployed),
            Ok(Some(m)) if placed(&m, &shared.node) => Route::Refuse(Refusal::NotReady),
            Ok(Some(_)) => Route::Refuse(Refusal::NotPlaced),
            Err(e) => {
                warn!(service = name, error = %e, "cannot read the service manifest");
                Route::Unreadable
            }
        }
    }

    /// Serves a request for an active service. WASM requests are bounded by the runtime;
    /// a static-site request takes an in-flight slot too, and slot wait and serving run
    /// under the manifest's `request_timeout_ms` (`504`).
    async fn serve(
        &self,
        name: &str,
        active: &Active,
        req: http::Request<Bytes>,
    ) -> Response<Bytes> {
        let shared = &self.shared;
        let limits = active.manifest.limits;
        match &active.payload {
            Payload::Wasm(loaded) => {
                let ctx = RequestCtx {
                    service: name.to_owned(),
                    node_id: shared.node.node_id(),
                    kv: Arc::new(ServiceKv::new(
                        shared.node.store().clone(),
                        data_ns(name, active.generation),
                    )),
                    limits,
                    outbound: active.manifest.outbound.clone(),
                };
                shared.runtime.handle(loaded, req, ctx).await
            }
            Payload::Static(site) => {
                let timeout = Duration::from_millis(limits.request_timeout_ms);
                let deadline = Instant::now() + timeout;
                let served = async {
                    let _slot = shared.runtime.acquire_slot(deadline).await?;
                    Some(site.serve(shared.node.blobs(), &req).await)
                };
                match tokio::time::timeout_at(deadline.into(), served).await {
                    Ok(Some(resp)) => resp,
                    Ok(None) | Err(_) => {
                        warn!(service = name, "request timed out after {timeout:?}");
                        let mut resp = Response::new(Bytes::new());
                        *resp.status_mut() = StatusCode::GATEWAY_TIMEOUT;
                        resp
                    }
                }
            }
        }
    }
}

fn text(code: StatusCode, body: &'static str) -> Response<Bytes> {
    let mut resp = Response::new(Bytes::from_static(body.as_bytes()));
    *resp.status_mut() = code;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}
