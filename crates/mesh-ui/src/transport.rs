//! How the UI reaches a daemon.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use mesh_admin::{AdminRequest, AdminResponse, InProcessAdmin};
use tokio::sync::mpsc;

use crate::error::{UiError, UiErrorCode};

#[async_trait]
pub trait AdminTransport: Send + Sync + 'static {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError>;

    /// Runs a line-streaming request (`LogsTail`, `Probe`), sending each line to `tx`.
    /// Returns when the daemon ends the request, or when `tx` is closed (the connection
    /// is then dropped).
    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError>;
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

    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError> {
        let mut lines = Box::pin(self.0.logs_tail(req));
        loop {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                item = lines.next() => match item {
                    None => return Ok(()),
                    Some(Err(AdminResponse::Error { code, message })) => {
                        return Err((code, message).into());
                    }
                    Some(Err(other)) => {
                        return Err(UiError::internal(format!("unexpected end of stream: {other:?}")));
                    }
                    Some(Ok(line)) => {
                        if tx.send(line).await.is_err() {
                            return Ok(());
                        }
                    }
                },
            }
        }
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

    async fn stream(&self, _req: AdminRequest, _tx: mpsc::Sender<String>) -> Result<(), UiError> {
        Err(UiError::new(
            UiErrorCode::NotRunning,
            "the node is not running",
        ))
    }
}
