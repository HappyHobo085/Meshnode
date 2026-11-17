//! The node-level error type (spec §10).

use mesh_proto::{ProtoError, RefuseReason};
use mesh_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("protocol error: {0}")]
    Proto(#[from] ProtoError),
    #[error("iroh error: {0}")]
    Iroh(String),
    /// A state file or the store was written by an incompatible Meshnode version (or is
    /// unreadable as one this build knows).
    #[error("{file}: {detail}")]
    Incompatible { file: String, detail: String },
    #[error("node.key is not a valid 32-byte key")]
    BadKey,
    #[error("data directory holds no mesh")]
    NotInitialised,
    #[error("store belongs to a different mesh")]
    WrongMesh,
    #[error("data directory already belongs to a mesh")]
    AlreadyInMesh,
    #[error("this node is not an effective member")]
    NotEffective,
    #[error("refusing to remove the last host")]
    LastHost,
    #[error("this node has been removed from the mesh")]
    Removed,
    #[error("node is rebootstrapping; local writes are refused")]
    Rebootstrapping,
    #[error("this node never completed a full sync; there is nothing to declare fresh")]
    NeverSynced,
    #[error("join refused: {0:?}")]
    JoinRefused(RefuseReason),
    #[error("inviter unreachable")]
    InviterUnreachable,
    #[error("extra protocol {0}")]
    ProtocolConflict(String),
    #[error("cannot connect to this node itself")]
    ConnectToSelf,
    #[error("node is shutting down")]
    ShuttingDown,
    #[error("blob unavailable from every peer")]
    BlobUnavailable,
}
