//! The forwarding side of `httpfwd` (spec §3.3): serve a request locally when this node
//! runs the service, otherwise forward it to the best placed host.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode, header};
use mesh_core::{NodeHandle, NodeStatus};
use mesh_forward::{ForwardError, RemoteForwarder};
#[cfg(any(test, feature = "test-hooks"))]
use mesh_proto::NodeId;
use mesh_proto::{Role, Scope};
use mesh_services::{LocalState, Services};

pub use mesh_forward::{PingError, PingTiming, ping};

/// Added to the manifest's `request_timeout_ms` for each forward attempt.
const ATTEMPT_SLACK: Duration = Duration::from_secs(5);

const NOT_DEPLOYED: &str = "Service not deployed";

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
    remote: RemoteForwarder,
}

impl Forwarder {
    pub fn new(node: NodeHandle, services: Arc<Services>) -> Forwarder {
        Forwarder {
            inner: Arc::new(Inner {
                remote: RemoteForwarder::new(node.clone()),
                node,
                services,
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
        match inner.remote.forward(name, req, ATTEMPT_SLACK).await {
            Ok(resp) => resp,
            Err(ForwardError::NotDeployed) => text(StatusCode::NOT_FOUND, NOT_DEPLOYED.into()),
            Err(ForwardError::Unframeable(e)) => {
                let code = e.status();
                text(code, code.canonical_reason().unwrap_or("").into())
            }
            Err(_) => no_host(name),
        }
    }

    /// Forward attempts this forwarder has made.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn attempts(&self) -> u64 {
        self.inner.remote.attempts()
    }

    /// `httpfwd` connections this forwarder has opened.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn connects(&self) -> u64 {
        self.inner.remote.connects()
    }

    /// Tries the candidates among `ids` first, in this order, instead of by RTT.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn try_first(&self, ids: Vec<NodeId>) {
        self.inner.remote.try_first(ids);
    }
}

impl Inner {
    /// Whether this node must forward rather than serve from its own store: it is a client,
    /// or its store is not a full replica (yet). A client re-invited as a host is listed as
    /// a host while its store is still Partial, and then while it re-bootstraps as Full
    /// (sub-spec 8a §3); any re-bootstrapping store lacks data until it is done.
    fn is_client(&self) -> bool {
        if self.node.scope() != Scope::Full
            || matches!(self.node.status(), NodeStatus::Rebootstrapping { .. })
        {
            return true;
        }
        let me = self.node.node_id();
        self.node
            .members()
            .iter()
            .any(|m| m.id == me && m.role == Role::Client)
    }
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
