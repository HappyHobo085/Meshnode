//! The data directory's advisory lock `<data>/meshd.lock`: held by the daemon for its whole
//! lifetime and by each local CLI operation (`init`, `join`, `reset-identity`) for its
//! duration, so two of them never work on one data directory at once.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The lock file's name inside the data directory.
pub const LOCK_FILE: &str = "meshd.lock";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another meshd is already running for {}", .0.display())]
    Held(PathBuf),
    #[error("cannot lock {}: {source}", .path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// An exclusive lock on a data directory, released when dropped (or when the process ends).
#[derive(Debug)]
pub struct DataLock {
    file: File,
}

impl DataLock {
    /// Takes the lock without waiting. The directory must exist.
    pub fn acquire(data_dir: &Path) -> Result<DataLock, LockError> {
        let path = data_dir.join(LOCK_FILE);
        let io_err = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_err)?;
        match file.try_lock() {
            Ok(()) => Ok(DataLock { file }),
            Err(TryLockError::WouldBlock) => Err(LockError::Held(data_dir.to_path_buf())),
            Err(TryLockError::Error(e)) => Err(io_err(e)),
        }
    }
}

impl Drop for DataLock {
    /// Unlocks explicitly: closing the file alone releases the lock only once no copy of
    /// its descriptor is left, and a process spawned meanwhile holds one until it execs.
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_refused_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataLock::acquire(dir.path()).unwrap();
        let err = DataLock::acquire(dir.path()).unwrap_err();
        assert!(matches!(err, LockError::Held(_)), "{err}");
        assert!(
            err.to_string()
                .starts_with("another meshd is already running for")
        );
        drop(first);
        DataLock::acquire(dir.path()).unwrap();
    }

    /// A process spawned while the lock is held shares its open file description until it
    /// execs; dropping the lock must release it anyway. The clone stands in for that copy.
    #[test]
    fn drop_releases_the_lock_while_a_copy_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataLock::acquire(dir.path()).unwrap();
        let copy = first.file.try_clone().unwrap();
        drop(first);
        DataLock::acquire(dir.path()).unwrap();
        drop(copy);
    }
}
