//! The admin client used by the `meshd` CLI.

use std::path::PathBuf;

use futures_util::Stream;
use mesh_core::frame::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::AdminError;
use crate::proto::{
    AdminRequest, AdminResponse, DaemonState, DeployOpts, FileData, FileMeta, Hello, MAX_FRAME,
    PROTOCOL_VERSION, validate_upload,
};
use crate::transport::{AdminStream, Endpoint, connect_raw};

const MAX_HELLO_FRAME: usize = 1024;

/// One connection to the daemon. Requests run one at a time.
pub struct AdminClient {
    conn: AdminStream,
    state: DaemonState,
    /// An exchange did not finish (an error, a dropped future or stream, or a refused
    /// upload after which the daemon closes): the connection can no longer be used.
    broken: bool,
}

impl AdminClient {
    /// Connects and exchanges `Hello`. [`AdminError::NotRunning`] when no daemon listens;
    /// [`AdminError::VersionMismatch`] when it speaks another protocol version.
    pub async fn connect(endpoint: &Endpoint) -> Result<AdminClient, AdminError> {
        let mut conn = connect_raw(endpoint).await?;
        let state = handshake(&mut conn).await?;
        Ok(AdminClient {
            conn,
            state,
            broken: false,
        })
    }

    /// The daemon's state as of the `Hello` exchange.
    pub fn daemon_state(&self) -> DaemonState {
        self.state
    }

    /// Sends one request and returns the daemon's response (which may be
    /// [`AdminResponse::Error`]). `DeployStatic`, `LogsTail` and `Probe` have their own methods.
    pub async fn request(&mut self, req: AdminRequest) -> Result<AdminResponse, AdminError> {
        if crate::server::needs_own_method(&req) {
            return Err(AdminError::Protocol(
                "DeployStatic, LogsTail and Probe go through deploy_static, logs_tail and probe"
                    .into(),
            ));
        }
        self.start(&req).await?;
        let resp = self.read_response().await?;
        self.broken = false;
        Ok(resp)
    }

    /// Deploys a static site, streaming each file from disk (`files` pairs the uploaded
    /// path and size with the local file). Paths and sizes are checked before anything is
    /// sent. After an `Error` response the connection is not reused.
    pub async fn deploy_static(
        &mut self,
        name: String,
        opts: DeployOpts,
        files: Vec<(FileMeta, PathBuf)>,
    ) -> Result<AdminResponse, AdminError> {
        let metas: Vec<FileMeta> = files.iter().map(|(m, _)| m.clone()).collect();
        validate_upload(&metas).map_err(AdminError::InvalidUpload)?;
        for (meta, path) in &files {
            let len = tokio::fs::metadata(path).await?.len();
            if len != meta.size {
                return Err(size_changed(path, len, meta.size));
            }
        }
        self.start(&AdminRequest::DeployStatic {
            name,
            opts,
            files: metas,
        })
        .await?;
        for (meta, path) in &files {
            let bytes = tokio::fs::read(path).await?;
            if bytes.len() as u64 != meta.size {
                return Err(size_changed(path, bytes.len() as u64, meta.size));
            }
            if let Err(e) = write_frame(&mut self.conn, &FileData { bytes }).await {
                // The daemon refuses a bad upload by answering and closing; the answer
                // may still be readable.
                return match read_frame(&mut self.conn, MAX_FRAME).await {
                    Ok(Some(resp)) => Ok(resp),
                    _ => Err(e.into()),
                };
            }
        }
        let resp = self.read_response().await?;
        self.broken = matches!(resp, AdminResponse::Error { .. });
        Ok(resp)
    }

    /// The last `lines` lines of the daemon's log, then, with `follow`, new lines as they
    /// are written. Dropping a stream before it ends leaves the connection unusable.
    pub fn logs_tail(
        &mut self,
        lines: u32,
        follow: bool,
    ) -> impl Stream<Item = Result<Vec<String>, AdminError>> + '_ {
        self.stream_lines(AdminRequest::LogsTail { lines, follow })
    }

    /// The lines of a line-streaming request (`Probe`), until the daemon ends it. Dropping
    /// a stream before it ends leaves the connection unusable.
    pub fn probe(
        &mut self,
        node: String,
        count: u32,
        interval_ms: u32,
        timeout_ms: u32,
    ) -> impl Stream<Item = Result<Vec<String>, AdminError>> + '_ {
        self.stream_lines(AdminRequest::Probe {
            node,
            count,
            interval_ms,
            timeout_ms,
        })
    }

    fn stream_lines(
        &mut self,
        req: AdminRequest,
    ) -> impl Stream<Item = Result<Vec<String>, AdminError>> + '_ {
        // State: the client, whether the request was sent, whether the stream ended.
        futures_util::stream::unfold((self, false, false), move |(c, sent, done)| {
            let req = req.clone();
            async move {
                if done {
                    return None;
                }
                if !sent && let Err(e) = c.start(&req).await {
                    return Some((Err(e), (c, true, true)));
                }
                let item = match c.read_response().await {
                    Ok(AdminResponse::LogLines(l)) => return Some((Ok(l), (c, true, false))),
                    Ok(AdminResponse::Ok) => {
                        c.broken = false;
                        return None;
                    }
                    Ok(AdminResponse::Error { code, message }) => {
                        c.broken = false;
                        Err(AdminError::Daemon { code, message })
                    }
                    Ok(other) => Err(AdminError::Protocol(format!(
                        "unexpected response to a line stream: {other:?}"
                    ))),
                    Err(e) => Err(e),
                };
                Some((item, (c, true, true)))
            }
        })
    }

    /// Sends a request frame, marking the connection busy until its exchange completes.
    async fn start(&mut self, req: &AdminRequest) -> Result<(), AdminError> {
        if self.broken {
            return Err(AdminError::Protocol(
                "the connection is unusable after an unfinished request; reconnect".into(),
            ));
        }
        self.broken = true;
        write_frame(&mut self.conn, req).await?;
        Ok(())
    }

    async fn read_response(&mut self) -> Result<AdminResponse, AdminError> {
        read_frame(&mut self.conn, MAX_FRAME)
            .await?
            .ok_or_else(|| AdminError::Protocol("the daemon closed the connection".into()))
    }
}

fn size_changed(path: &std::path::Path, len: u64, declared: u64) -> AdminError {
    AdminError::InvalidUpload(format!(
        "{} is {len} bytes, not the declared {declared}",
        path.display()
    ))
}

/// Sends our `Hello` and checks the daemon's.
async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
) -> Result<DaemonState, AdminError> {
    let ours = Hello {
        version: PROTOCOL_VERSION,
        state: None,
    };
    write_frame(s, &ours).await?;
    let theirs: Hello = read_frame(s, MAX_HELLO_FRAME).await?.ok_or_else(|| {
        AdminError::Protocol("the daemon closed the connection during the handshake".into())
    })?;
    if theirs.version != PROTOCOL_VERSION {
        return Err(AdminError::VersionMismatch {
            cli: PROTOCOL_VERSION,
            daemon: theirs.version,
        });
    }
    theirs
        .state
        .ok_or_else(|| AdminError::Protocol("the daemon did not report its state".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn newer_daemon_is_a_version_mismatch() {
        let (mut ours, mut daemon) = tokio::io::duplex(4096);
        let fake = tokio::spawn(async move {
            let h: Hello = read_frame(&mut daemon, 1024).await.unwrap().unwrap();
            assert_eq!(h.version, PROTOCOL_VERSION);
            let reply = Hello {
                version: 2,
                state: Some(DaemonState::Running),
            };
            write_frame(&mut daemon, &reply).await.unwrap();
        });
        let r = handshake(&mut ours).await;
        assert!(
            matches!(r, Err(AdminError::VersionMismatch { cli: 1, daemon: 2 })),
            "{r:?}"
        );
        fake.await.unwrap();
    }
}
