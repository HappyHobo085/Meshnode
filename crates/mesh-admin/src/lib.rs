//! The local admin API of `meshd` (spec §5): wire types, the Unix-socket / Windows
//! named-pipe transports with operating-system access control, a server driven by an
//! [`AdminBackend`], and a client.

mod client;
mod in_process;
mod proto;
mod server;
mod transport;
#[cfg(unix)]
mod transport_unix;
#[cfg(windows)]
mod transport_windows;

use std::io;

use mesh_core::frame::FrameError;

pub use client::AdminClient;
pub use in_process::InProcessAdmin;
pub use proto::{
    AdminRequest, AdminResponse, DAEMON_STOPPING, DaemonState, DeployOpts, ErrorCode, FileData,
    FileMeta, Hello, LocalStateView, MAX_FRAME, MAX_LOG_LINES, MAX_PROBE_COUNT,
    MAX_PROBE_TIMEOUT_MS, MAX_UPLOAD_FILE, MAX_UPLOAD_FILES, MAX_UPLOAD_TOTAL,
    MIN_PROBE_TIMEOUT_MS, MemberInfoView, PROTOCOL_VERSION, ServiceInfoView, ServiceKindView,
    StatusInfo, valid_upload_path, validate_upload,
};
pub use server::{
    AdminBackend, AdminServer, MAX_CONNECTIONS, RequestIo, ServerTimeouts, UploadError,
};
pub use transport::{
    Access, AdminIo, AdminStream, Endpoint, connect_raw, default_data_dir, endpoint_for,
};

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("meshd is not running (start it with \"meshd run\" or \"meshd service install\")")]
    NotRunning,
    #[error("meshd and the running daemon are different versions (cli {cli}, daemon {daemon})")]
    VersionMismatch { cli: u32, daemon: u32 },
    #[error("another meshd is already serving the admin endpoint {0}")]
    InUse(String),
    #[error(
        "refusing the admin endpoint {endpoint}: it is served by account {account}, not by \
         LocalService, LocalSystem or the current user"
    )]
    Untrusted { endpoint: String, account: String },
    #[error("the daemon has too many admin connections; try again")]
    Busy,
    #[error("{code}: {message}")]
    Daemon { code: ErrorCode, message: String },
    #[error("invalid upload: {0}")]
    InvalidUpload(String),
    #[error("invalid admin access setting: {0}")]
    InvalidAccess(String),
    #[error("admin protocol error: {0}")]
    Protocol(String),
    #[error("admin connection: {0}")]
    Io(#[from] io::Error),
}

impl From<FrameError> for AdminError {
    fn from(e: FrameError) -> Self {
        match e {
            FrameError::Io(e) => AdminError::Io(e),
            other => AdminError::Protocol(other.to_string()),
        }
    }
}
