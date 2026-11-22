//! The desktop transport: the admin pipe of the daemon the locator found. When a call finds
//! no daemon there (`DaemonUnreachable`), it locates once more and retries once.

use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use mesh_admin::{AdminRequest, AdminResponse, DeployOpts, Endpoint};
use mesh_ui::{
    AdminTransport, ConnectionDto, ConnectionKindDto, FileSource, PipeTransport, UiError,
    UiErrorCode,
};
use tokio::sync::mpsc;

use crate::locator::{Candidate, ConnectionKind, DaemonLocator, answers};

/// What the last locate found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub kind: ConnectionKind,
    pub endpoint: Endpoint,
    /// Another candidate's daemon answers too.
    pub other_running: bool,
}

pub struct RelocatingTransport {
    locator: Mutex<DaemonLocator>,
    current: Mutex<Option<Located>>,
    relocations: AtomicU32,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn unreachable() -> UiError {
    UiError::new(
        UiErrorCode::DaemonUnreachable,
        "Meshnode is not running on this computer",
    )
}

fn is_unreachable(e: &UiError) -> bool {
    e.code == UiErrorCode::DaemonUnreachable
}

impl RelocatingTransport {
    /// Nothing is located until the first call (or [`relocate`](Self::relocate)).
    pub fn new(locator: DaemonLocator) -> RelocatingTransport {
        RelocatingTransport {
            locator: Mutex::new(locator),
            current: Mutex::new(None),
            relocations: AtomicU32::new(0),
        }
    }

    /// Replaces the candidates (e.g. once the service's data directory is known).
    pub fn set_candidates(&self, candidates: Vec<Candidate>) {
        lock(&self.locator).candidates = candidates;
    }

    /// Runs the locator and remembers what it found.
    pub async fn relocate(&self) -> Option<Located> {
        let locator = lock(&self.locator).clone();
        let found = locator
            .locate()
            .await
            .map(|(kind, endpoint, other_running)| Located {
                kind,
                endpoint,
                other_running,
            });
        self.relocations.fetch_add(1, Ordering::Relaxed);
        lock(&self.current).clone_from(&found);
        found
    }

    /// What the last locate found.
    pub fn current(&self) -> Option<Located> {
        lock(&self.current).clone()
    }

    /// How many times the locator has run (for tests and diagnostics).
    pub fn relocations(&self) -> u32 {
        self.relocations.load(Ordering::Relaxed)
    }

    /// The connection as the UI shows it, from the last locate.
    pub fn connection(&self) -> ConnectionDto {
        match self.current() {
            None => ConnectionDto {
                kind: ConnectionKindDto::None,
                other_running: false,
                start_error: self.start_error(),
                no_access: self.no_access(),
            },
            Some(l) => ConnectionDto {
                kind: match l.kind {
                    ConnectionKind::System => ConnectionKindDto::System,
                    ConnectionKind::User => ConnectionKindDto::User,
                },
                other_running: l.other_running,
                start_error: None,
                no_access: false,
            },
        }
    }

    /// Why a candidate's daemon last refused to start (its `start-error.txt`), in the
    /// candidates' order.
    fn start_error(&self) -> Option<String> {
        lock(&self.locator)
            .candidates
            .iter()
            .find_map(|c| mesh_admin::start_error(&c.data_dir))
    }

    /// Whether this user is denied the system service's admin endpoint.
    fn no_access(&self) -> bool {
        lock(&self.locator)
            .candidates
            .iter()
            .any(|c| c.kind == ConnectionKind::System && mesh_admin::access_denied(&c.data_dir))
    }

    /// The located endpoint, locating first if nothing is.
    async fn endpoint(&self) -> Result<(Endpoint, bool), UiError> {
        if let Some(l) = self.current() {
            return Ok((l.endpoint, false));
        }
        let l = self.relocate().await.ok_or_else(unreachable)?;
        Ok((l.endpoint, true))
    }

    /// Runs `call` on the located daemon; on `DaemonUnreachable` locates once more and
    /// retries once (unless it had just located).
    async fn with_retry<T, F, Fut>(&self, call: F) -> Result<T, UiError>
    where
        F: Fn(PipeTransport) -> Fut,
        Fut: Future<Output = Result<T, UiError>>,
    {
        let (endpoint, fresh) = self.endpoint().await?;
        match call(PipeTransport::new(endpoint)).await {
            Err(e) if is_unreachable(&e) && !fresh => {
                let l = self.relocate().await.ok_or(e)?;
                call(PipeTransport::new(l.endpoint)).await
            }
            Err(e) if is_unreachable(&e) => {
                // It had just been found and is gone already.
                lock(&self.current).take();
                Err(e)
            }
            other => other,
        }
    }
}

#[async_trait]
impl AdminTransport for RelocatingTransport {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError> {
        self.with_retry(|t| {
            let req = req.clone();
            async move { t.request(req).await }
        })
        .await
    }

    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError> {
        // `DaemonUnreachable` comes from connecting, before any line was sent.
        self.with_retry(|t| {
            let (req, tx) = (req.clone(), tx.clone());
            async move { t.stream(req, tx).await }
        })
        .await
    }

    /// The source can be read only once, so instead of retrying, the daemon is checked
    /// (and located again if gone) before the upload starts.
    async fn deploy_static(
        &self,
        name: String,
        opts: DeployOpts,
        src: Box<dyn FileSource>,
        progress: mpsc::Sender<u64>,
    ) -> Result<AdminResponse, UiError> {
        let (mut endpoint, fresh) = self.endpoint().await?;
        if !fresh && !answers(&endpoint).await {
            endpoint = self.relocate().await.ok_or_else(unreachable)?.endpoint;
        }
        PipeTransport::new(endpoint)
            .deploy_static(name, opts, src, progress)
            .await
    }
}
