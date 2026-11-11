//! The in-process transport: the same [`AdminBackend`] dispatch as the socket server
//! (state gating, upload validation), called directly, with no socket involved.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::Stream;
use futures_util::stream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::proto::{AdminRequest, AdminResponse, DaemonState, DeployOpts, ErrorCode, FileMeta};
use crate::server::{AdminBackend, Outcome, RequestIo, dispatch, needs_own_method};

/// Lines buffered between the backend and the consumer of a stream.
const LINE_BUFFER: usize = 64;

/// A handle that calls an [`AdminBackend`] directly. Cheap to clone.
#[derive(Clone)]
pub struct InProcessAdmin {
    backend: Arc<dyn AdminBackend>,
}

/// `Outcome::Close` means the connection went away mid-request, so no reply is wanted. For
/// the in-process transport that is only reachable in `logs_tail`, after the consumer
/// dropped its stream; the response is then never read.
fn into_response(outcome: Outcome) -> AdminResponse {
    match outcome {
        Outcome::Reply(r) | Outcome::ReplyAndClose(r) => r,
        Outcome::Close => AdminResponse::error(ErrorCode::Internal, "the request was aborted"),
    }
}

enum Lines {
    Idle(InProcessAdmin, AdminRequest),
    Running {
        rx: mpsc::Receiver<Vec<String>>,
        task: JoinHandle<AdminResponse>,
        buf: VecDeque<String>,
    },
    Done,
}

impl InProcessAdmin {
    pub fn new(backend: Arc<dyn AdminBackend>) -> InProcessAdmin {
        InProcessAdmin { backend }
    }

    pub fn daemon_state(&self) -> DaemonState {
        self.backend.state()
    }

    /// Answers one request (which may be `AdminResponse::Error`). `DeployStatic`,
    /// `LogsTail` and `Probe` have their own methods.
    pub async fn request(&self, req: AdminRequest) -> AdminResponse {
        if needs_own_method(&req) {
            return AdminResponse::error(
                ErrorCode::InvalidArgument,
                "DeployStatic, LogsTail and Probe go through deploy_static and logs_tail",
            );
        }
        self.run(req, Vec::new()).await
    }

    /// Deploys a static site from files held in memory. Upload limits and paths are
    /// checked exactly as for the socket server.
    pub async fn deploy_static(
        &self,
        name: String,
        opts: DeployOpts,
        files: Vec<(FileMeta, Vec<u8>)>,
    ) -> AdminResponse {
        let (metas, data) = files.into_iter().unzip();
        let req = AdminRequest::DeployStatic {
            name,
            opts,
            files: metas,
        };
        self.run(req, data).await
    }

    /// Streams the lines of a line-streaming request (`LogsTail` and `Probe`) until the ending response: an `Ok` end yields nothing more,
    /// an error end yields `Err(resp)`. The backend starts on the first poll; dropping the
    /// stream ends the request: the backend sees its line sends fail (or `client_gone` resolve) and
    /// finishes by itself, as when a socket client leaves.
    pub fn logs_tail(
        &self,
        req: AdminRequest,
    ) -> impl Stream<Item = Result<String, AdminResponse>> + Send + 'static {
        stream::unfold(Lines::Idle(self.clone(), req), |state| async move {
            let mut state = state;
            loop {
                match state {
                    Lines::Idle(admin, req) => {
                        let (tx, rx) = mpsc::channel(LINE_BUFFER);
                        let io = RequestIo::in_memory(&req, Vec::new(), tx);
                        let backend = admin.backend.clone();
                        let task = tokio::spawn(async move {
                            into_response(dispatch(&*backend, req, io).await)
                        });
                        state = Lines::Running {
                            rx,
                            task,
                            buf: VecDeque::new(),
                        };
                    }
                    Lines::Running {
                        mut rx,
                        task,
                        mut buf,
                    } => {
                        if let Some(line) = buf.pop_front() {
                            return Some((Ok(line), Lines::Running { rx, task, buf }));
                        }
                        match rx.recv().await {
                            Some(lines) => {
                                buf.extend(lines);
                                state = Lines::Running { rx, task, buf };
                            }
                            None => {
                                let mut task = task;
                                let resp = (&mut task).await.unwrap_or_else(|e| {
                                    AdminResponse::error(ErrorCode::Internal, e.to_string())
                                });
                                return match resp {
                                    AdminResponse::Ok => None,
                                    err => Some((Err(err), Lines::Done)),
                                };
                            }
                        }
                    }
                    Lines::Done => return None,
                }
            }
        })
    }

    async fn run(&self, req: AdminRequest, data: Vec<Vec<u8>>) -> AdminResponse {
        // No lines are expected; a backend that sends some finds the channel closed.
        let (tx, _) = mpsc::channel(1);
        let io = RequestIo::in_memory(&req, data, tx);
        into_response(dispatch(&*self.backend, req, io).await)
    }
}
