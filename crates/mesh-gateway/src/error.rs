//! Why a frame could not become an HTTP message, and why a forward attempt failed.

use http::StatusCode;
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

/// A request that cannot be put in an `httpfwd` frame; answered with [`Unframeable::status`]
/// instead of being forwarded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Unframeable {
    #[error(
        "more than {} headers or {} bytes of headers",
        mesh_proto::MAX_HEADERS,
        mesh_proto::MAX_HEADER_BYTES
    )]
    HeaderFields,
    #[error("path and query longer than {} bytes", mesh_proto::MAX_PATH)]
    Uri,
    #[error("request frame larger than {} bytes", mesh_proto::HTTPFWD_MAX_FRAME)]
    TooLarge,
    #[error("invalid request: {0}")]
    Invalid(mesh_proto::ProtoError),
}

impl Unframeable {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Unframeable::HeaderFields => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Unframeable::Uri => StatusCode::URI_TOO_LONG,
            Unframeable::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Unframeable::Invalid(_) => StatusCode::BAD_REQUEST,
        }
    }
}
