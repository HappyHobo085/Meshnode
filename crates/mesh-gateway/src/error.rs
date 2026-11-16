//! Why the gateway could not start.

/// Why the gateway could not start.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("invalid gateway configuration: {0}")]
    Invalid(String),
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        addr: std::net::SocketAddr,
        source: std::io::Error,
    },
}
