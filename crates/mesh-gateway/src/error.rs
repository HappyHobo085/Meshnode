//! Why a frame could not become an HTTP message, and why a forward attempt failed.

use mesh_core::NodeError;
use mesh_core::frame::FrameError;

/// A received `httpfwd` frame does not make a valid `http` request or response.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FrameConvError {
    #[error("invalid method")]
    Method,
    #[error("invalid path")]
    Path,
    #[error("invalid status")]
    Status,
    #[error("invalid header name {0:?}")]
    HeaderName(String),
    #[error("invalid header value")]
    HeaderValue,
}

/// One forward attempt to one host failed; the forwarder tries the next host.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ForwardError {
    #[error("connect failed: {0}")]
    Connect(#[from] NodeError),
    #[error("stream failed: {0}")]
    Stream(String),
    #[error("bad response frame: {0}")]
    Frame(#[from] FrameError),
    #[error("stream ended without a response")]
    NoResponse,
    #[error("bad response: {0}")]
    Convert(#[from] FrameConvError),
    #[error("timed out after {0} ms")]
    Timeout(u64),
}
