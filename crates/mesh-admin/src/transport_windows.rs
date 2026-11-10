//! The admin endpoint as a Windows named pipe. Its DACL, built from SDDL, grants access to
//! the daemon's own account (read from the process token), Administrators and, with
//! [`Access::WindowsUser`], one more user; its owner is the daemon's account. Remote
//! clients are refused. Before talking to a pipe, the client checks which account serves
//! it, so a pipe squatted by another user is not trusted.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::time::Instant;
use tracing::warn;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::AdminError;
use crate::transport::{Access, AdminStream};

/// While every pipe instance is taken, the client retries this often, for this long.
const BUSY_RETRY: Duration = Duration::from_millis(50);
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Accounts a daemon may run as besides the client's own user.
const LOCAL_SERVICE: &str = "S-1-5-19";
const LOCAL_SYSTEM: &str = "S-1-5-18";

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

/// Opens the pipe and checks who serves it. A missing pipe is [`AdminError::NotRunning`];
/// a busy one is retried for a while, then [`AdminError::Busy`]; one served by an account
/// other than LocalService, LocalSystem or the current user is [`AdminError::Untrusted`].
pub(crate) async fn connect(name: &str) -> Result<AdminStream, AdminError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match ClientOptions::new().open(name) {
            Ok(c) => {
                let account = server_account(c.as_raw_handle())?;
                if !trusted_server(&account, &current_user_sid()?) {
                    return Err(AdminError::Untrusted {
                        endpoint: name.to_string(),
                        account,
                    });
                }
                return Ok(Box::new(c));
            }
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

/// Whether a pipe served by `account` may be trusted by a client running as `current`.
fn trusted_server(account: &str, current: &str) -> bool {
    account == LOCAL_SERVICE || account == LOCAL_SYSTEM || account == current
}

/// The account serving a pipe: the user of the server process's token, or, when this
/// client may not open that process or its token (a service account's process), the
/// pipe's owner, which the daemon sets to its own account and which another user cannot
/// set to one of the trusted accounts.
fn server_account(pipe: HANDLE) -> io::Result<String> {
    match server_process_sid(pipe) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => pipe_owner_sid(pipe),
        r => r,
    }
}

fn server_process_sid(pipe: HANDLE) -> io::Result<String> {
    let mut pid = 0u32;
    // SAFETY: `pipe` is a valid pipe handle; `pid` is a valid out pointer.
    if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: plain call; a null handle reports failure.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let sid = token_user_sid(process);
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(process) };
    sid
}

fn pipe_owner_sid(pipe: HANDLE) -> io::Result<String> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a valid handle; `owner` points into `sd`, which is freed below.
    let r = unsafe {
        GetSecurityInfo(
            pipe,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let sid = sid_string(owner);
    // SAFETY: LocalAlloc'ed by GetSecurityInfo, freed once.
    unsafe { LocalFree(sd) };
    sid
}

/// `O:<daemon>D:P(A;;GA;;;<daemon>)(A;;GA;;;BA)[(A;;GA;;;<user>)]`: owned by the daemon's
/// account, with a protected DACL giving full access to it, Administrators and the
/// optional extra user.
fn pipe_sddl(own_sid: &str, access: &Access) -> Result<String, AdminError> {
    let mut sddl = format!("O:{own_sid}D:P(A;;GA;;;{own_sid})(A;;GA;;;BA)");
    match access {
        Access::CurrentUserOnly => {}
        Access::WindowsUser { sid } => {
            if !valid_account_sid(sid) {
                return Err(AdminError::InvalidAccess(format!(
                    "{sid:?} is not a user account SID"
                )));
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

/// A SID of an individual account: `S-1-5-21-…` (local or domain user) or `S-1-12-1-…`
/// (Entra ID user). Groups and well-known SIDs (Everyone, Users, …) are refused: the extra
/// pipe user must be one person.
fn valid_account_sid(s: &str) -> bool {
    valid_sid(s) && (s.starts_with("S-1-5-21-") || s.starts_with("S-1-12-1-")) && s.len() > 9
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
    // SAFETY: the pseudo handle of this process needs no closing.
    token_user_sid(unsafe { GetCurrentProcess() })
}

/// The user SID of a process's token.
fn token_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is a valid handle; `token` is a valid out pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
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
    sid_string(unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the caller passes a valid SID; `text` is a valid out pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returned a LocalAlloc'ed NUL-terminated string.
    Ok(unsafe { take_local_wide(text) })
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
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
        // A made-up account SID stands in for the recorded user.
        let name = pipe_name("dacl");
        let mut l = Listener::bind(
            &name,
            &Access::WindowsUser {
                sid: "S-1-5-21-1-2-3-1001".into(),
            },
        )
        .unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        let dacl = dacl_of(&client);
        assert!(dacl.starts_with("D:P"), "{dacl}");
        assert!(dacl.contains(&format!(";;;{own})")), "{dacl}");
        assert!(dacl.contains(";;;BA)"), "{dacl}");
        assert!(dacl.contains(";;;S-1-5-21-1-2-3-1001)"), "{dacl}");
        assert_eq!(dacl.matches("(A;").count(), 3, "{dacl}");
        assert!(
            !dacl.contains(";;;WD)") && !dacl.contains(";;;AN)"),
            "{dacl}"
        );
        // The instance created after an accept carries the same DACL.
        let _served = l.accept().await.unwrap();
        let second = ClientOptions::new().open(&name).unwrap();
        assert_eq!(dacl_of(&second), dacl);
        drop(client);

        let name = pipe_name("own");
        let _l = Listener::bind(&name, &Access::CurrentUserOnly).unwrap();
        let dacl = dacl_of(&ClientOptions::new().open(&name).unwrap());
        assert_eq!(dacl.matches("(A;").count(), 2, "{dacl}");
    }

    #[tokio::test]
    async fn client_trusts_a_pipe_served_by_its_own_user() {
        let own = current_user_sid().unwrap();
        let name = pipe_name("trust");
        let mut l = Listener::bind(&name, &Access::CurrentUserOnly).unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        let _served = l.accept().await.unwrap();
        let pipe = client.as_raw_handle();
        assert_eq!(server_process_sid(pipe).unwrap(), own);
        assert_eq!(
            pipe_owner_sid(pipe).unwrap(),
            own,
            "the daemon owns its pipe"
        );
        assert_eq!(server_account(pipe).unwrap(), own);
        drop(client);
        assert!(connect(&name).await.is_ok());
    }

    #[test]
    fn only_service_accounts_or_the_current_user_are_trusted() {
        let me = "S-1-5-21-1-2-3-1001";
        assert!(trusted_server(me, me));
        assert!(trusted_server(LOCAL_SERVICE, me));
        assert!(trusted_server(LOCAL_SYSTEM, me));
        for other in ["S-1-5-21-1-2-3-1002", "S-1-5-20", "S-1-5-32-544", "S-1-1-0"] {
            assert!(!trusted_server(other, me), "{other}");
        }
        let e = AdminError::Untrusted {
            endpoint: "pipe".into(),
            account: "S-1-5-21-9".into(),
        };
        assert!(e.to_string().contains("S-1-5-21-9"), "{e}");
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

    #[test]
    fn only_account_sids_reach_the_pipe() {
        assert!(valid_account_sid("S-1-5-21-1-2-3-1001"));
        assert!(valid_account_sid("S-1-12-1-1-2-3-4"));
        for group in [
            "S-1-1-0",
            "S-1-5-11",
            "S-1-5-32-545",
            "S-1-5-21-",
            "S-1-5-21",
        ] {
            assert!(!valid_account_sid(group), "{group}");
            assert!(matches!(
                pipe_sddl("S-1-5-19", &Access::WindowsUser { sid: group.into() }),
                Err(AdminError::InvalidAccess(_))
            ));
        }
    }
}
