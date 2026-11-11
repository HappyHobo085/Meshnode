//! The desktop transport: the daemon's admin pipe (Windows) or socket (Unix).

use async_trait::async_trait;
use futures_util::StreamExt;
use mesh_admin::{AdminClient, AdminError, AdminRequest, AdminResponse, Endpoint};
use tokio::sync::mpsc;

use crate::error::{UiError, UiErrorCode};
use crate::transport::AdminTransport;

pub struct PipeTransport {
    endpoint: Endpoint,
}

impl PipeTransport {
    pub fn new(endpoint: Endpoint) -> PipeTransport {
        PipeTransport { endpoint }
    }

    async fn connect(&self) -> Result<AdminClient, UiError> {
        AdminClient::connect(&self.endpoint)
            .await
            .map_err(|e| match e {
                AdminError::NotRunning | AdminError::Io(_) => {
                    UiError::new(UiErrorCode::DaemonUnreachable, e.to_string())
                }
                other => map_error(other),
            })
    }
}

fn map_error(e: AdminError) -> UiError {
    match e {
        AdminError::NotRunning => UiError::new(UiErrorCode::DaemonUnreachable, e.to_string()),
        AdminError::Busy => UiError::new(UiErrorCode::Busy, e.to_string()),
        AdminError::Daemon { code, message } => (code, message).into(),
        AdminError::Io(_) => UiError::new(UiErrorCode::Io, e.to_string()),
        other => UiError::internal(other.to_string()),
    }
}

#[async_trait]
impl AdminTransport for PipeTransport {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError> {
        let mut client = self.connect().await?;
        match client.request(req).await {
            Ok(resp) => Ok(resp),
            // The daemon went away between connect and reply.
            Err(AdminError::Io(e)) => {
                Err(UiError::new(UiErrorCode::DaemonUnreachable, e.to_string()))
            }
            Err(e) => Err(map_error(e)),
        }
    }

    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError> {
        let mut client = self.connect().await?;
        let result = {
            let stream = match req {
                AdminRequest::LogsTail { lines, follow } => client.logs_tail(lines, follow).boxed(),
                AdminRequest::Probe {
                    node,
                    count,
                    interval_ms,
                    timeout_ms,
                } => client.probe(node, count, interval_ms, timeout_ms).boxed(),
                other => {
                    return Err(UiError::internal(format!("not a line stream: {other:?}")));
                }
            };
            let mut stream = stream;
            loop {
                tokio::select! {
                    _ = tx.closed() => break Ok(()),
                    item = stream.next() => match item {
                        None => break Ok(()),
                        Some(Err(e)) => break Err(map_error(e)),
                        Some(Ok(lines)) => {
                            for line in lines {
                                if tx.send(line).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                    },
                }
            }
        };
        // Dropping an unfinished stream leaves the connection unusable: drop it too.
        drop(client);
        result
    }
}
