//! How the UI reaches a daemon.

use std::sync::Arc;

use async_trait::async_trait;
use mesh_admin::{AdminRequest, AdminResponse, InProcessAdmin};

use crate::error::{UiError, UiErrorCode};

#[async_trait]
pub trait AdminTransport: Send + Sync + 'static {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError>;
}

/// Android picks a transport per call (the node may be stopped).
pub type TransportFn = Arc<dyn Fn() -> Arc<dyn AdminTransport> + Send + Sync>;

/// A daemon living in this process.
pub struct InProcessTransport(pub InProcessAdmin);

#[async_trait]
impl AdminTransport for InProcessTransport {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError> {
        Ok(self.0.request(req).await)
    }
}

/// No node is running: every request fails with `NotRunning`.
pub struct NoNode;

#[async_trait]
impl AdminTransport for NoNode {
    async fn request(&self, _req: AdminRequest) -> Result<AdminResponse, UiError> {
        Err(UiError::new(
            UiErrorCode::NotRunning,
            "the node is not running",
        ))
    }
}
