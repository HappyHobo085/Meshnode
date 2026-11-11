//! Shared core of the Meshnode management UI: UI-shaped DTOs (exported to TypeScript), the
//! error type with its hints, the transport to a daemon and the request/response commands.

pub mod commands;
pub mod dto;
pub mod error;
pub mod qr;
pub mod transport;

pub use commands::{Platform, Ui};
pub use dto::*;
pub use error::{JoinRefusalDto, UiError, UiErrorCode, hint_for};
pub use qr::qr_svg;
pub use transport::{AdminTransport, InProcessTransport, NoNode, TransportFn};
