//! Request dispatch: [`Services::handle`] (spec §5.3).

use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode, header};
use mesh_runtime::RequestCtx;
use tracing::warn;

use crate::Services;
use crate::deployer::Payload;
use crate::kv::ServiceKv;
use crate::manifest::{data_ns, read_manifest};
use crate::placement::placed;

const NOT_REPLICATED: &str = "Service not yet replicated";
const NOT_PLACED: &str = "Service not placed on this node";
const NOT_DEPLOYED: &str = "Service not deployed";

impl Services {
    /// Serves one request for service `name` on this node. Never fails: every outcome is
    /// an HTTP response.
    pub async fn handle(&self, name: &str, req: http::Request<Bytes>) -> Response<Bytes> {
        let shared = &self.shared;
        if let Some(active) = shared.active(name) {
            return match &active.payload {
                Payload::Wasm(loaded) => {
                    let ctx = RequestCtx {
                        service: name.to_owned(),
                        node_id: shared.node.node_id(),
                        kv: Arc::new(ServiceKv::new(
                            shared.node.store().clone(),
                            data_ns(name, active.generation),
                        )),
                        limits: active.manifest.limits,
                        outbound: active.manifest.outbound.clone(),
                    };
                    shared.runtime.handle(loaded, req, ctx).await
                }
                Payload::Static => text(
                    StatusCode::NOT_IMPLEMENTED,
                    "Static sites are not served yet",
                ),
            };
        }
        match read_manifest(shared.node.store(), name).await {
            Ok(None) => text(StatusCode::NOT_FOUND, NOT_DEPLOYED),
            Ok(Some(m)) if placed(&m, &shared.node) => {
                text(StatusCode::SERVICE_UNAVAILABLE, NOT_REPLICATED)
            }
            Ok(Some(_)) => text(StatusCode::SERVICE_UNAVAILABLE, NOT_PLACED),
            Err(e) => {
                warn!(service = name, error = %e, "cannot read the service manifest");
                text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Service manifest unreadable",
                )
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
