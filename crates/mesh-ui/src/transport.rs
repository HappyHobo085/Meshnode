//! How the UI reaches a daemon.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use mesh_admin::{AdminRequest, AdminResponse, DeployOpts, FileMeta, InProcessAdmin};
use tokio::sync::mpsc;

use crate::error::{UiError, UiErrorCode};
use crate::source::FileSource;

#[async_trait]
pub trait AdminTransport: Send + Sync + 'static {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError>;

    /// Runs a line-streaming request (`LogsTail`, `Probe`), sending each line to `tx`.
    /// Returns when the daemon ends the request, or when `tx` is closed (the connection
    /// is then dropped).
    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError>;

    /// Deploys a static site read from `src`. `progress` gets the cumulative bytes read.
    async fn deploy_static(
        &self,
        name: String,
        opts: DeployOpts,
        src: Box<dyn FileSource>,
        progress: mpsc::Sender<u64>,
    ) -> Result<AdminResponse, UiError>;
}

/// Reports `done` bytes without waiting; a full channel just skips this update.
pub(crate) fn report(progress: &mpsc::Sender<u64>, done: u64) {
    let _ = progress.try_send(done);
}

/// Reads every listed file through `src` (blocking), reporting progress.
fn read_all(
    src: &mut dyn FileSource,
    metas: &[FileMeta],
    progress: &mpsc::Sender<u64>,
) -> Result<Vec<Vec<u8>>, UiError> {
    let mut done = 0u64;
    let mut out = Vec::with_capacity(metas.len());
    for m in metas {
        let bytes = src.read(&m.path)?;
        done += bytes.len() as u64;
        report(progress, done);
        out.push(bytes);
    }
    Ok(out)
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

    async fn deploy_static(
        &self,
        name: String,
        opts: DeployOpts,
        mut src: Box<dyn FileSource>,
        progress: mpsc::Sender<u64>,
    ) -> Result<AdminResponse, UiError> {
        let (metas, datas, progress) = tokio::task::spawn_blocking(move || {
            let metas = src.list()?;
            let datas = read_all(src.as_mut(), &metas, &progress)?;
            Ok::<_, UiError>((metas, datas, progress))
        })
        .await
        .map_err(|e| UiError::internal(e.to_string()))??;
        let total: u64 = metas.iter().map(|m| m.size).sum();
        // The last value always arrives, even if the channel was full above.
        let _ = progress.send(total).await;
        let files = metas.into_iter().zip(datas).collect();
        Ok(self.0.deploy_static(name, opts, files).await)
    }
}

/// Runs every call of `inner` on the runtime of `handle`, and awaits it from wherever it is
/// called: an in-process daemon's requests spawn tasks that must live on the daemon's own
/// runtime, not on the caller's (the Android webview's commands run on Tauri's). Dropping a
/// call aborts it there, as dropping it in place would.
pub struct OnRuntime {
    handle: tokio::runtime::Handle,
    inner: Arc<dyn AdminTransport>,
}

impl OnRuntime {
    pub fn new(handle: tokio::runtime::Handle, inner: Arc<dyn AdminTransport>) -> OnRuntime {
        OnRuntime { handle, inner }
    }

    async fn run<T, F>(&self, fut: F) -> Result<T, UiError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, UiError>> + Send + 'static,
    {
        struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
        impl<T> Drop for AbortOnDrop<T> {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let mut task = AbortOnDrop(self.handle.spawn(fut));
        (&mut task.0)
            .await
            .map_err(|e| UiError::internal(e.to_string()))?
    }
}

#[async_trait]
impl AdminTransport for OnRuntime {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError> {
        let inner = self.inner.clone();
        self.run(async move { inner.request(req).await }).await
    }

    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError> {
        let inner = self.inner.clone();
        self.run(async move { inner.stream(req, tx).await }).await
    }

    async fn deploy_static(
        &self,
        name: String,
        opts: DeployOpts,
        src: Box<dyn FileSource>,
        progress: mpsc::Sender<u64>,
    ) -> Result<AdminResponse, UiError> {
        let inner = self.inner.clone();
        self.run(async move { inner.deploy_static(name, opts, src, progress).await })
            .await
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

    async fn deploy_static(
        &self,
        _name: String,
        _opts: DeployOpts,
        _src: Box<dyn FileSource>,
        _progress: mpsc::Sender<u64>,
    ) -> Result<AdminResponse, UiError> {
        Err(UiError::new(
            UiErrorCode::NotRunning,
            "the node is not running",
        ))
    }
}
