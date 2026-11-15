//! The error type of `mesh-services` (spec §7).

use mesh_core::NodeError;
use mesh_runtime::RuntimeError;

#[derive(Debug, thiserror::Error)]
pub enum ServicesError {
    #[error("service names are 1 to 32 characters of a-z, 0-9 and dash")]
    InvalidName,
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid component: {0}")]
    InvalidComponent(String),
    #[error("no such service")]
    NotFound,
    /// The operation needs a full replica of the mesh's data, which this node (a client, or
    /// a client re-invited as a host before its rebuild) does not hold.
    #[error(
        "{0} must run on a host: this device does not hold the services' data (run it on a host device)"
    )]
    NotFullReplica(&'static str),
    #[error(transparent)]
    Node(#[from] NodeError),
    #[error(transparent)]
    Runtime(RuntimeError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
