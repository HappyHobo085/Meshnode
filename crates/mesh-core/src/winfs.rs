//! Windows-only file helpers for `local`: a durable rename.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Renames `from` to `to` and returns only once the rename is on disk
/// (`MOVEFILE_WRITE_THROUGH`). Without `replace` it fails with `AlreadyExists` if `to`
/// exists.
pub(crate) fn durable_rename(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both arguments are NUL-terminated UTF-16 strings that outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_rename_replaces_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"2").unwrap();
        let e = durable_rename(&a, &b, false).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        durable_rename(&a, &b, true).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"1");
        assert!(!a.exists());
    }
}
