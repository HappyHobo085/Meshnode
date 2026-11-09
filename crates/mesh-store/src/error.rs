//! Store error type and conversions from redb errors.

use mesh_proto::ProtoError;
use std::io;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// I/O or database-engine failure (including "database already open").
    #[error("store I/O error: {0}")]
    Io(String),
    /// Data on disk could not be decoded or violates an invariant.
    #[error("corrupt data in table `{table}`: {detail}")]
    Corrupt { table: &'static str, detail: String },
    /// The disk is full; the transaction was rolled back.
    #[error("disk full")]
    Full,
    /// The store file belongs to a different mesh.
    #[error("store belongs to a different mesh")]
    WrongMesh,
    #[error(transparent)]
    Proto(#[from] ProtoError),
}

impl StoreError {
    pub(crate) fn corrupt(table: &'static str, detail: impl Into<String>) -> Self {
        StoreError::Corrupt {
            table,
            detail: detail.into(),
        }
    }
}

fn is_disk_full(e: &io::Error) -> bool {
    // ENOSPC = 28 (Linux/Android); ERROR_DISK_FULL = 112, ERROR_HANDLE_DISK_FULL = 39 (Windows).
    e.kind() == io::ErrorKind::StorageFull
        || matches!(e.raw_os_error(), Some(code) if code == 28 && cfg!(unix))
        || matches!(e.raw_os_error(), Some(112 | 39) if cfg!(windows))
}

impl From<redb::Error> for StoreError {
    fn from(e: redb::Error) -> Self {
        match e {
            redb::Error::Io(io) if is_disk_full(&io) => StoreError::Full,
            redb::Error::Corrupted(detail) => StoreError::corrupt("redb", detail),
            redb::Error::DatabaseAlreadyOpen => StoreError::Io("database already open".into()),
            other => StoreError::Io(other.to_string()),
        }
    }
}

macro_rules! via_redb_error {
    ($($t:ty),* $(,)?) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                StoreError::from(redb::Error::from(e))
            }
        }
    )*};
}

via_redb_error!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
);
