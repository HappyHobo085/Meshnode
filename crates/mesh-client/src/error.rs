//! What a [`crate::MeshClient`] call can fail with.

use mesh_core::NodeError;
use mesh_proto::RefuseReason;

/// Why a [`crate::MeshClient`] call failed. A host's own answer, `4xx`/`5xx` included, is
/// never an error: [`crate::MeshClient::request`] returns it as a response.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The ticket is not a valid client invite: unreadable, for another role, expired,
    /// already used, or refused by the inviter.
    #[error("not a valid client invite ticket")]
    InvalidTicket,
    /// `join` into a directory that already holds a node identity (`node.key`).
    #[error("this directory already holds a node identity")]
    AlreadyJoined,
    /// `open` on a directory that holds no joined node.
    #[error("this directory holds no joined client")]
    NotJoined,
    /// Another client (in this process or another) has this directory open.
    #[error("another client has this directory open")]
    AlreadyOpen,
    /// This client has been removed from the mesh; every call fails so from now on.
    #[error("this client has been removed from the mesh")]
    Removed,
    /// No host could be reached: none runs the service and is online, the service is not
    /// deployed, or every attempt failed to connect (or got a retryable `503`). For
    /// `join`: the inviter could not be reached.
    #[error("no host could serve the request")]
    NoHost,
    /// A host was reached but did not answer in time, or the request took longer than
    /// [`crate::ClientOptions::request_timeout`].
    #[error("the request timed out")]
    Timeout,
    #[error("i/o error: {0}")]
    Io(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl ClientError {
    pub(crate) fn io(e: std::io::Error) -> ClientError {
        ClientError::Io(e.to_string())
    }

    /// A node error from `invite`, `remove`, `open` and the like.
    pub(crate) fn node(e: NodeError) -> ClientError {
        match e {
            NodeError::Removed => ClientError::Removed,
            NodeError::Io(e) => ClientError::io(e),
            NodeError::NotInitialised => ClientError::NotJoined,
            e => ClientError::Internal(e.to_string()),
        }
    }

    /// A node error from `join`.
    pub(crate) fn join(e: NodeError) -> ClientError {
        match e {
            NodeError::JoinRefused(RefuseReason::InviterUnavailable)
            | NodeError::InviterUnreachable => ClientError::NoHost,
            NodeError::JoinRefused(_) => ClientError::InvalidTicket,
            NodeError::AlreadyInMesh => ClientError::AlreadyJoined,
            e => ClientError::node(e),
        }
    }
}
