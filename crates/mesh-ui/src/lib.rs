//! Shared core of the Meshnode management UI: UI-shaped DTOs (exported to TypeScript), the
//! error type with its hints, the transport to a daemon and the request/response commands.

pub mod commands;
pub mod dto;
pub mod error;
pub mod pipe;
pub mod qr;
pub mod source;
pub mod streams;
pub mod transport;

pub use commands::{Platform, Ui};
pub use dto::*;
pub use error::{JoinRefusalDto, UiError, UiErrorCode, hint_for};
pub use pipe::PipeTransport;
pub use qr::qr_svg;
pub use source::{DirSource, FileSource};
pub use streams::{StreamGuard, Streams};
pub use transport::{AdminTransport, InProcessTransport, NoNode, TransportFn};
