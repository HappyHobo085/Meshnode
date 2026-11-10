//! Meshnode gateway: `httpfwd` forwarding between nodes and the localhost HTTP server.

mod config;
mod error;
mod forward;
mod routing;
mod serve_httpfwd;
mod server;
mod status_page;

pub use config::GatewayConfig;
pub use error::GatewayError;
pub use forward::Forwarder;
pub use serve_httpfwd::{HttpFwdHandler, HttpFwdSlot};
pub use server::Gateway;
