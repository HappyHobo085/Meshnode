//! Windows-only file helpers for `local`: a durable rename and an owner-only DACL.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, DACL_SECURITY_INFORMATION, GetLengthSid,
    GetTokenInformation, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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

/// The current process user's `TOKEN_USER` (its SID points into the returned buffer).
/// `u64` elements keep the buffer aligned for the structure.
pub(crate) fn current_user() -> io::Result<Vec<u64>> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` is a valid out pointer; the pseudo handle needs no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0u32;
    // SAFETY: a size query with a null buffer; `len` is a valid out pointer.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `len` bytes, aligned for `TOKEN_USER`.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            len,
            &mut len,
        )
    };
    let err = io::Error::last_os_error();
    // SAFETY: `token` was opened above and is closed once.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(err);
    }
    Ok(buf)
}

/// Replaces the DACL of `path` with one granting full access to the current user only,
/// with inheritance from the parent directory disabled (spec §3.1: `node.key` is
/// owner-only).
pub(crate) fn restrict_to_current_user(path: &Path) -> io::Result<()> {
    let user = current_user()?;
    // SAFETY: `current_user` filled the buffer with a `TOKEN_USER`.
    let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: `sid` is a valid SID inside `user`.
    let sid_len = unsafe { GetLengthSid(sid) };
    // ACL header + one ACCESS_ALLOWED_ACE (whose last u32 is the SID's start) + the SID.
    let acl_len = size_of::<ACL>() + 2 * size_of::<u32>() + 2 * size_of::<u16>() + sid_len as usize;
    let mut acl_buf = vec![0u64; acl_len.div_ceil(8)];
    let acl = acl_buf.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl` points to `acl_len` writable bytes, aligned for `ACL`.
    if unsafe { InitializeAcl(acl, acl_len as u32, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `acl` was initialised above with room for this ACE; `sid` is valid.
    if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let path = wide(path);
    // SAFETY: `path` is NUL-terminated; `acl` is a valid ACL; no owner, group or SACL.
    let r = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    if r != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, EqualSid, GetAce, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED,
    };

    use super::*;

    /// The DACL of `path`: whether it is protected, and each ACE's type, mask and whether
    /// its SID is the current user's.
    fn read_dacl(path: &Path) -> (bool, Vec<(u8, u32, bool)>) {
        let user = current_user().unwrap();
        // SAFETY: `current_user` filled the buffer with a `TOKEN_USER`.
        let me = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let path = wide(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated path and out pointers; `sd` is freed below.
        let r = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(r, ERROR_SUCCESS);
        assert!(!dacl.is_null(), "a DACL is present");
        let (mut control, mut rev) = (0u16, 0u32);
        // SAFETY: `sd` is the descriptor returned above.
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut rev) },
            0
        );
        let mut aces = Vec::new();
        // SAFETY: `dacl` points into `sd`, which is still alive.
        let count = unsafe { (*dacl).AceCount };
        for i in 0..u32::from(count) {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: `i` is below the ACE count.
            assert_ne!(unsafe { GetAce(dacl, i, &mut ace) }, 0);
            let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: every ACE starts with the header; for an access-allowed ACE the SID
            // starts at `SidStart`.
            let (kind, mask, sid) = unsafe {
                (
                    (*ace).Header.AceType,
                    (*ace).Mask,
                    std::ptr::addr_of_mut!((*ace).SidStart).cast::<c_void>(),
                )
            };
            // SAFETY: both SIDs are valid.
            aces.push((kind, mask, unsafe { EqualSid(sid, me) } != 0));
        }
        // SAFETY: `sd` was allocated by `GetNamedSecurityInfoW`.
        unsafe { LocalFree(sd) };
        (control & SE_DACL_PROTECTED != 0, aces)
    }

    #[test]
    fn restricted_file_has_only_the_current_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"x").unwrap();
        restrict_to_current_user(&path).unwrap();
        let (protected, aces) = read_dacl(&path);
        assert!(protected, "inheritance from the directory is disabled");
        assert_eq!(aces, vec![(0, FILE_ALL_ACCESS, true)]);
        // Still readable by this user.
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn node_key_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        crate::local::load_or_create_key(dir.path()).unwrap();
        let (protected, aces) = read_dacl(&dir.path().join("node.key"));
        assert!(protected, "inheritance from the directory is disabled");
        assert_eq!(aces, vec![(0, FILE_ALL_ACCESS, true)]);
    }

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
