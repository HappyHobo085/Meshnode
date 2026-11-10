//! The admin endpoint as a Windows named pipe. Its DACL, built from SDDL, grants access to
//! the daemon's own account (read from the process token), Administrators and, with
//! [`Access::WindowsUser`], one more user. Remote clients are refused.

use std::io;
use std::time::Duration;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::time::Instant;
use tracing::warn;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::AdminError;
use crate::transport::{Access, AdminStream};

/// While every pipe instance is taken, the client retries this often, for this long.
const BUSY_RETRY: Duration = Duration::from_millis(50);
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Listener {
    name: String,
    sd: SecurityDescriptor,
    /// The instance waiting for the next client. One always exists while serving, so a
    /// client finds the pipe (and waits for a slot) rather than seeing it missing.
    next: Option<NamedPipeServer>,
}

impl Listener {
    /// Creates the first instance of the pipe; [`AdminError::InUse`] if the pipe exists.
    pub(crate) fn bind(name: &str, access: &Access) -> Result<Listener, AdminError> {
        let sddl = pipe_sddl(&current_user_sid()?, access)?;
        let sd = SecurityDescriptor::from_sddl(&sddl)?;
        let first = create(name, &sd, true).map_err(|e| {
            // `first_pipe_instance` fails with access denied when the pipe already exists.
            if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                AdminError::InUse(name.to_string())
            } else {
                AdminError::Io(e)
            }
        })?;
        Ok(Listener {
            name: name.to_string(),
            sd,
            next: Some(first),
        })
    }

    pub(crate) async fn accept(&mut self) -> io::Result<AdminStream> {
        let server = match self.next.take() {
            Some(s) => s,
            None => create(&self.name, &self.sd, false)?,
        };
        let connected = server.connect().await;
        match create(&self.name, &self.sd, false) {
            Ok(next) => self.next = Some(next),
            Err(e) => warn!(pipe = %self.name, "creating the next admin pipe instance: {e}"),
        }
        connected?;
        Ok(Box::new(server))
    }
}

fn create(name: &str, sd: &SecurityDescriptor, first: bool) -> io::Result<NamedPipeServer> {
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first).reject_remote_clients(true);
    // SAFETY: `attrs` is a valid SECURITY_ATTRIBUTES for the duration of the call, and its
    // descriptor is owned by `sd`, which outlives it.
    unsafe {
        opts.create_with_security_attributes_raw(
            name,
            (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
}

/// Opens the pipe. A missing pipe is [`AdminError::NotRunning`]; a busy one is retried
/// for a while, then [`AdminError::Busy`].
pub(crate) async fn connect(name: &str) -> Result<AdminStream, AdminError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match ClientOptions::new().open(name) {
            Ok(c) => return Ok(Box::new(c)),
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                return Err(AdminError::NotRunning);
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                if Instant::now() >= deadline {
                    return Err(AdminError::Busy);
                }
                tokio::time::sleep(BUSY_RETRY).await;
            }
            Err(e) => return Err(AdminError::Io(e)),
        }
    }
}

/// `D:P(A;;GA;;;<daemon>)(A;;GA;;;BA)[(A;;GA;;;<user>)]`: a protected DACL with full
/// access for the daemon's account, Administrators and the optional extra user.
fn pipe_sddl(own_sid: &str, access: &Access) -> Result<String, AdminError> {
    let mut sddl = format!("D:P(A;;GA;;;{own_sid})(A;;GA;;;BA)");
    match access {
        Access::CurrentUserOnly => {}
        Access::WindowsUser { sid } => {
            if !valid_sid(sid) {
                return Err(AdminError::InvalidAccess(format!("{sid:?} is not a SID")));
            }
            sddl.push_str(&format!("(A;;GA;;;{sid})"));
        }
    }
    Ok(sddl)
}

/// `S-1-<digits>(-<digits>)*`, so nothing else can reach the SDDL string.
fn valid_sid(s: &str) -> bool {
    s.strip_prefix("S-1-").is_some_and(|rest| {
        rest.split('-')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// A self-relative security descriptor allocated by
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is never modified after creation; the OS only reads it.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<SecurityDescriptor> {
        let w = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `w` is NUL-terminated; `sd` is a valid out pointer; the size is optional.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SecurityDescriptor(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated with LocalAlloc by the conversion above, freed once.
        unsafe { LocalFree(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Takes ownership of a `LocalAlloc`ed, NUL-terminated wide string.
///
/// # Safety
/// `p` must be such a string, not used afterwards.
unsafe fn take_local_wide(p: *mut u16) -> String {
    let mut len = 0;
    // SAFETY: the caller guarantees NUL termination.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` u16s were just read.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
    // SAFETY: allocated with LocalAlloc, freed once.
    unsafe { LocalFree(p.cast()) };
    s
}

/// The string SID of the account this process runs as (S-1-5-19 under LocalService).
fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` is a valid out pointer; the pseudo handle needs no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0u32;
    // SAFETY: a size query with a null buffer; `len` is a valid out pointer.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    // `u64` elements keep the buffer aligned for TOKEN_USER.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `len` bytes, aligned for TOKEN_USER.
    let ok =
        unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) };
    let err = io::Error::last_os_error();
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(err);
    }
    // SAFETY: the call above filled `buf` with a TOKEN_USER whose SID points into `buf`.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is valid while `buf` lives; `text` is a valid out pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returned a LocalAlloc'ed NUL-terminated string.
    Ok(unsafe { take_local_wide(text) })
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    use super::*;

    /// The pipe's DACL as SDDL, read through a client handle.
    fn dacl_of(client: &tokio::net::windows::named_pipe::NamedPipeClient) -> String {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a valid handle; only the descriptor out pointer is requested.
        let r = unsafe {
            GetSecurityInfo(
                client.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(r, ERROR_SUCCESS);
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sd` came from GetSecurityInfo; `text` is a valid out pointer.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0);
        // SAFETY: `sd` was LocalAlloc'ed by GetSecurityInfo.
        unsafe { LocalFree(sd) };
        // SAFETY: a LocalAlloc'ed NUL-terminated string.
        unsafe { take_local_wide(text) }
    }

    fn pipe_name(tag: &str) -> String {
        format!(r"\\.\pipe\meshnode-admin-test-{tag}-{}", std::process::id())
    }

    #[tokio::test]
    async fn pipe_dacl_grants_daemon_admins_and_recorded_user_only() {
        let own = current_user_sid().unwrap();
        assert!(valid_sid(&own), "{own}");
        // BU (Users) stands in for the recorded user.
        let name = pipe_name("dacl");
        let mut l = Listener::bind(
            &name,
            &Access::WindowsUser {
                sid: "S-1-5-32-545".into(),
            },
        )
        .unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        let dacl = dacl_of(&client);
        assert!(dacl.starts_with("D:P"), "{dacl}");
        assert!(dacl.contains(&format!(";;;{own})")), "{dacl}");
        assert!(dacl.contains(";;;BA)"), "{dacl}");
        assert!(dacl.contains(";;;BU)"), "{dacl}");
        assert_eq!(dacl.matches("(A;").count(), 3, "{dacl}");
        assert!(
            !dacl.contains(";;;WD)") && !dacl.contains(";;;AN)"),
            "{dacl}"
        );
        let _served = l.accept().await.unwrap();

        let name = pipe_name("own");
        let _l = Listener::bind(&name, &Access::CurrentUserOnly).unwrap();
        let dacl = dacl_of(&ClientOptions::new().open(&name).unwrap());
        assert_eq!(dacl.matches("(A;").count(), 2, "{dacl}");
    }

    #[test]
    fn sid_strings_are_checked() {
        assert!(valid_sid("S-1-5-21-1-2-3-1001"));
        assert!(valid_sid("S-1-5-19"));
        for bad in [
            "",
            "S-1-",
            "S-1-5-",
            "S-1-5-x",
            "BA",
            "S-1-5)(A;;GA;;;WD",
            "s-1-5-19",
        ] {
            assert!(!valid_sid(bad), "{bad}");
        }
        assert!(matches!(
            pipe_sddl(
                "S-1-5-19",
                &Access::WindowsUser {
                    sid: "S-1-5)(A;;GA;;;WD".into()
                }
            ),
            Err(AdminError::InvalidAccess(_))
        ));
    }
}
