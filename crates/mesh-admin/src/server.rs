//! The admin server: accepts connections, runs the `Hello` exchange and passes requests,
//! in order, to an [`AdminBackend`]. Rules that hold for every backend are enforced here:
//! which requests each [`DaemonState`] accepts, `LogsTail` and `ResetIdentity` arguments,
//! and the static-upload checks (paths, sizes, declared vs received).

use std::collections::VecDeque;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mesh_core::frame::{FrameError, read_frame, write_frame};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, warn};

use crate::AdminError;
use crate::proto::{
    AdminRequest, AdminResponse, DaemonState, ErrorCode, FileData, FileMeta, Hello, MAX_FRAME,
    MAX_LOG_LINES, PROTOCOL_VERSION, validate_upload,
};
use crate::transport::{self, Access, AdminIo, AdminStream, Endpoint, Listener};

/// At most this many admin connections are served at once; more wait for a free slot.
pub const MAX_CONNECTIONS: usize = 16;
/// A client must send its `Hello` within this time.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HELLO_FRAME: usize = 1024;
/// Pause after a failed accept, so a persistent failure does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// A connection with no request for this long is closed (a following `LogsTail` is not
/// idle: it ends when the client leaves).
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// While a static upload is received, each `FileData` frame must arrive within this time.
const UPLOAD_FRAME_TIMEOUT: Duration = Duration::from_secs(60);
/// A `FileData` frame's bytes beyond the file itself (version byte and length varint).
const FILE_FRAME_ENVELOPE: usize = 16;

/// How long a connection may sit idle between requests, and how long each upload frame
/// may take. Only the test-hooks constructor changes them.
#[derive(Clone, Copy, Debug)]
pub struct ServerTimeouts {
    pub idle: Duration,
    pub upload_frame: Duration,
}

impl Default for ServerTimeouts {
    fn default() -> Self {
        ServerTimeouts {
            idle: IDLE_TIMEOUT,
            upload_frame: UPLOAD_FRAME_TIMEOUT,
        }
    }
}

/// What the daemon does with each request.
#[async_trait]
pub trait AdminBackend: Send + Sync {
    fn state(&self) -> DaemonState;
    /// Answers one request. For `DeployStatic` the files are read through `io` (files the
    /// backend leaves unread are read and discarded afterwards); for `LogsTail` lines are
    /// sent through `io` and the returned response (`Ok` or an error) ends the stream.
    async fn handle(&self, req: AdminRequest, io: &mut RequestIo<'_>) -> AdminResponse;
}

/// A static upload that cannot be completed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    /// The upload is not acceptable (the client sent something other than it declared, or
    /// a path this system cannot store). The client gets `InvalidArgument`.
    #[error("invalid upload: {0}")]
    Invalid(String),
    /// The connection failed while reading the upload.
    #[error("upload connection failed: {0}")]
    Connection(String),
    /// Writing a received file to disk failed.
    #[error("writing an uploaded file failed: {0}")]
    Write(String),
}

/// The connection as seen by [`AdminBackend::handle`] during one request.
pub struct RequestIo<'a> {
    conn: &'a mut dyn AdminIo,
    /// Declared files of a `DeployStatic` not read yet.
    files: VecDeque<FileMeta>,
    /// Set once the upload broke; the connection then closes after this request.
    failed: Option<UploadError>,
    /// The client left, or sent bytes it should not have: close without answering.
    gone: bool,
    upload_frame_timeout: Duration,
}

impl RequestIo<'_> {
    /// The next uploaded file of a `DeployStatic`, checked against its declared size;
    /// `None` once all are read, and for any other request.
    pub async fn next_file(&mut self) -> Result<Option<(FileMeta, Vec<u8>)>, UploadError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        let Some(meta) = self.files.pop_front() else {
            return Ok(None);
        };
        let max = meta.size as usize + FILE_FRAME_ENVELOPE;
        let read = timeout(
            self.upload_frame_timeout,
            read_frame::<_, FileData>(&mut self.conn, max),
        );
        let Ok(read) = read.await else {
            let err = UploadError::Connection(format!("no data for {:?} in time", meta.path));
            self.failed = Some(err.clone());
            return Err(err);
        };
        let err = match read {
            Ok(Some(d)) if d.bytes.len() as u64 == meta.size => return Ok(Some((meta, d.bytes))),
            Ok(Some(d)) => UploadError::Invalid(format!(
                "file {:?} was declared as {} bytes but {} arrived",
                meta.path,
                meta.size,
                d.bytes.len()
            )),
            Err(FrameError::TooLarge(_)) => UploadError::Invalid(format!(
                "file {:?} is larger than its declared {} bytes",
                meta.path, meta.size
            )),
            Ok(None) => UploadError::Connection("the client closed the connection".into()),
            Err(FrameError::Io(e)) => UploadError::Connection(e.to_string()),
            Err(e) => UploadError::Invalid(e.to_string()),
        };
        self.failed = Some(err.clone());
        Err(err)
    }

    /// Reads every remaining uploaded file and writes it under `dir`, creating
    /// subdirectories as needed.
    pub async fn save_files(&mut self, dir: &Path) -> Result<(), UploadError> {
        while let Some((meta, bytes)) = self.next_file().await? {
            let target = storable_path(dir, &meta.path)?;
            let write = |e: io::Error| UploadError::Write(format!("{}: {e}", target.display()));
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(write)?;
            }
            tokio::fs::write(&target, &bytes).await.map_err(write)?;
        }
        Ok(())
    }

    /// Sends one `LogLines` frame. An error means the client is gone.
    pub async fn send_lines(&mut self, lines: Vec<String>) -> io::Result<()> {
        let r = write_frame(&mut self.conn, &AdminResponse::LogLines(lines)).await;
        if r.is_err() {
            self.gone = true;
        }
        r
    }

    /// Resolves once the client has disconnected; meant for a following `LogsTail`, while
    /// the client sends nothing. Cancel-safe.
    pub async fn client_gone(&mut self) {
        if !self.gone {
            // EOF, an error, or a byte the client may not send now: either way it is done.
            let mut b = [0u8; 1];
            let _ = self.conn.read(&mut b).await;
            self.gone = true;
        }
    }

    /// After the backend answered: reads files it left unread, and decides how the
    /// connection goes on.
    async fn finish(mut self, resp: AdminResponse) -> Outcome {
        if self.gone {
            return Outcome::Close;
        }
        while !self.files.is_empty() && self.failed.is_none() {
            let _ = self.next_file().await;
        }
        match self.failed {
            None => Outcome::Reply(resp),
            Some(UploadError::Invalid(m)) => {
                Outcome::ReplyAndClose(AdminResponse::error(ErrorCode::InvalidArgument, m))
            }
            Some(_) => Outcome::Close,
        }
    }
}

/// `dir` joined with an upload path, refusing segments this system would not store as a
/// plain file name (on Windows: drive or stream colons, wildcard characters, trailing dots
/// or spaces, device names).
fn storable_path(dir: &Path, rel: &str) -> Result<PathBuf, UploadError> {
    let mut p = dir.to_path_buf();
    for seg in rel.split('/') {
        let mut c = Path::new(seg).components();
        let plain = matches!((c.next(), c.next()), (Some(Component::Normal(_)), None));
        if !plain || (cfg!(windows) && !windows_file_name(seg)) {
            return Err(UploadError::Invalid(format!(
                "{rel:?} cannot be stored on this system"
            )));
        }
        p.push(seg);
    }
    Ok(p)
}

fn windows_file_name(seg: &str) -> bool {
    const DEVICES: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let stem = seg
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device = DEVICES.contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    !device
        && !seg.ends_with(['.', ' '])
        && !seg.contains([':', '*', '?', '"', '<', '>', '|'])
        && !seg.bytes().any(|b| b < 0x20)
}

enum Outcome {
    Reply(AdminResponse),
    /// The response, then close: the rest of the request's frames cannot be trusted.
    ReplyAndClose(AdminResponse),
    Close,
}

/// A running admin server. Dropping it stops it without waiting; prefer
/// [`AdminServer::shutdown`].
pub struct AdminServer {
    cancel: CancellationToken,
    tasks: TaskTracker,
    accept: Option<JoinHandle<()>>,
}

impl AdminServer {
    /// Binds the endpoint with `access` applied and starts serving. Refuses with
    /// [`AdminError::InUse`] when another server is live there. Must be called inside a
    /// tokio runtime.
    pub fn bind(
        endpoint: &Endpoint,
        access: Access,
        backend: Arc<dyn AdminBackend>,
    ) -> Result<AdminServer, AdminError> {
        Self::start(endpoint, access, backend, ServerTimeouts::default())
    }

    /// [`AdminServer::bind`] with other timeouts.
    #[cfg(feature = "test-hooks")]
    pub fn bind_with_timeouts(
        endpoint: &Endpoint,
        access: Access,
        backend: Arc<dyn AdminBackend>,
        timeouts: ServerTimeouts,
    ) -> Result<AdminServer, AdminError> {
        Self::start(endpoint, access, backend, timeouts)
    }

    fn start(
        endpoint: &Endpoint,
        access: Access,
        backend: Arc<dyn AdminBackend>,
        timeouts: ServerTimeouts,
    ) -> Result<AdminServer, AdminError> {
        let listener = transport::bind(endpoint, &access)?;
        let cancel = CancellationToken::new();
        let tasks = TaskTracker::new();
        let accept = tokio::spawn(accept_loop(
            listener,
            backend,
            timeouts,
            cancel.clone(),
            tasks.clone(),
        ));
        Ok(AdminServer {
            cancel,
            tasks,
            accept: Some(accept),
        })
    }

    /// Stops accepting, closes every connection and waits for their tasks.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(accept) = self.accept.take() {
            let _ = accept.await;
        }
        self.tasks.close();
        self.tasks.wait().await;
    }
}

impl Drop for AdminServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn accept_loop(
    mut listener: Listener,
    backend: Arc<dyn AdminBackend>,
    timeouts: ServerTimeouts,
    cancel: CancellationToken,
    tasks: TaskTracker,
) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        // Take a slot first: with all of them in use, new clients wait in the listen
        // backlog (Unix) or on the pending pipe instance (Windows).
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            p = slots.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            r = listener.accept() => r,
        };
        match accepted {
            Ok(conn) => {
                let backend = backend.clone();
                let cancel = cancel.clone();
                tasks.spawn(async move {
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = serve(conn, &*backend, timeouts) => {}
                    }
                    drop(permit);
                });
            }
            Err(e) => {
                warn!("accepting an admin connection failed: {e}");
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(ACCEPT_RETRY) => {}
                }
            }
        }
    }
}

/// One connection: `Hello`, then requests in order until the client leaves or breaks the
/// protocol. Nothing here affects other connections.
async fn serve(mut conn: AdminStream, backend: &dyn AdminBackend, timeouts: ServerTimeouts) {
    let hello = match timeout(
        HELLO_TIMEOUT,
        read_frame::<_, Hello>(&mut conn, MAX_HELLO_FRAME),
    )
    .await
    {
        Ok(Ok(Some(h))) => h,
        Ok(Err(e)) => {
            debug!("closing admin connection: bad hello: {e}");
            return;
        }
        Ok(Ok(None)) | Err(_) => return,
    };
    let reply = Hello {
        version: PROTOCOL_VERSION,
        state: Some(backend.state()),
    };
    if write_frame(&mut conn, &reply).await.is_err() {
        return;
    }
    if hello.version != PROTOCOL_VERSION {
        debug!(
            client = hello.version,
            "closing admin connection: protocol version mismatch"
        );
        return;
    }
    loop {
        let next = timeout(
            timeouts.idle,
            read_frame::<_, AdminRequest>(&mut conn, MAX_FRAME),
        );
        let req = match next.await {
            Ok(Ok(Some(r))) => r,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => {
                debug!("closing admin connection: {e}");
                return;
            }
            Err(_) => {
                debug!("closing idle admin connection");
                return;
            }
        };
        let (resp, close) = match handle_one(&mut conn, backend, req, timeouts).await {
            Outcome::Reply(r) => (r, false),
            Outcome::ReplyAndClose(r) => (r, true),
            Outcome::Close => return,
        };
        if write_frame(&mut conn, &resp).await.is_err() || close {
            return;
        }
    }
}

async fn handle_one(
    conn: &mut AdminStream,
    backend: &dyn AdminBackend,
    req: AdminRequest,
    timeouts: ServerTimeouts,
) -> Outcome {
    if let Err((code, message)) = check(backend.state(), &req) {
        let resp = AdminResponse::Error { code, message };
        // A refused upload's files are already on their way: answer, then close.
        return if matches!(req, AdminRequest::DeployStatic { .. }) {
            Outcome::ReplyAndClose(resp)
        } else {
            Outcome::Reply(resp)
        };
    }
    let files = match &req {
        AdminRequest::DeployStatic { files, .. } => files.iter().cloned().collect(),
        _ => VecDeque::new(),
    };
    let mut io = RequestIo {
        conn: &mut **conn,
        files,
        failed: None,
        gone: false,
        upload_frame_timeout: timeouts.upload_frame,
    };
    let resp = backend.handle(req, &mut io).await;
    io.finish(resp).await
}

/// The rules every backend gets (spec §5.1): `Init`/`Join` only while uninitialised;
/// `ResetIdentity` only when removed or uninitialised, and confirmed; while uninitialised
/// only those, `Status`, config and logs; argument limits.
fn check(state: DaemonState, req: &AdminRequest) -> Result<(), (ErrorCode, String)> {
    use AdminRequest as R;
    let wrong = |m: &str| Err((ErrorCode::WrongState, m.to_string()));
    let invalid = |m: String| Err((ErrorCode::InvalidArgument, m));
    match req {
        R::Init { .. } | R::Join { .. } if state != DaemonState::Uninitialised => {
            return wrong("this node already belongs to a mesh");
        }
        R::ResetIdentity { .. }
            if !matches!(state, DaemonState::Removed | DaemonState::Uninitialised) =>
        {
            return wrong("reset-identity is only possible once this node was removed");
        }
        R::ResetIdentity { confirm: false } => {
            return invalid("reset-identity must be confirmed".into());
        }
        R::LogsTail { lines, .. } if *lines > MAX_LOG_LINES => {
            return invalid(format!(
                "at most {MAX_LOG_LINES} log lines can be asked for"
            ));
        }
        R::DeployStatic { files, .. } => validate_upload(files).or_else(invalid)?,
        _ => {}
    }
    let uninitialised_ok = matches!(
        req,
        R::Status
            | R::Init { .. }
            | R::Join { .. }
            | R::ResetIdentity { .. }
            | R::ConfigGet { .. }
            | R::ConfigSet { .. }
            | R::LogsTail { .. }
    );
    if state == DaemonState::Uninitialised && !uninitialised_ok {
        return wrong("this node has no mesh yet; run \"meshd init\" or \"meshd join\" first");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storable_paths() {
        let dir = Path::new("base");
        let p = storable_path(dir, "a/b.txt").unwrap();
        assert_eq!(p, dir.join("a").join("b.txt"));
        if cfg!(windows) {
            for bad in ["a/b:c", "con", "x/NUL.txt", "com1", "a./b", "a /b", "a/b*"] {
                assert!(storable_path(dir, bad).is_err(), "{bad}");
            }
            assert!(storable_path(dir, "console/com10.txt").is_ok());
        }
    }
}
