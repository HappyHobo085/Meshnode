//! The admin endpoint as a Unix domain socket, protected by its file mode and group.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::net::{UnixListener, UnixStream};

use crate::AdminError;
use crate::transport::{Access, AdminStream};

pub(crate) struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

/// Distinguishes concurrent binds in one process.
static BIND_SEQ: AtomicU32 = AtomicU32::new(0);

impl Listener {
    /// Binds the socket, replacing a stale socket file nobody answers on;
    /// [`AdminError::InUse`] if another server answers there or wins a race for the path.
    ///
    /// The socket is bound inside a fresh `0700` directory, given its mode and group
    /// there, and only then linked to `path`, so it is never reachable with looser
    /// permissions. The link does not replace an existing file.
    pub(crate) fn bind(path: &Path, access: &Access) -> Result<Listener, AdminError> {
        let in_use = || AdminError::InUse(path.display().to_string());
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(AdminError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                )));
            }
            if probe_live(path)? {
                return Err(in_use());
            }
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        let parent = path.parent().unwrap_or(Path::new("."));
        let seq = BIND_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!(".adm-{}-{seq}", std::process::id()));
        // A leftover from a crashed run of this pid.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let tmp = dir.join("s");
        let bound = (|| -> Result<UnixListener, AdminError> {
            let inner = UnixListener::bind(&tmp).map_err(|e| {
                if e.raw_os_error() == Some(libc::EADDRINUSE) {
                    in_use()
                } else {
                    AdminError::Io(e)
                }
            })?;
            restrict(&tmp, access)?;
            std::fs::hard_link(&tmp, path).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    in_use()
                } else {
                    AdminError::Io(e)
                }
            })?;
            Ok(inner)
        })();
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_dir(&dir);
        Ok(Listener {
            inner: bound?,
            path: path.to_path_buf(),
        })
    }

    pub(crate) async fn accept(&mut self) -> io::Result<AdminStream> {
        let (s, _) = self.inner.accept().await?;
        Ok(Box::new(s))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether a server listens on the socket at `path`. The probe connects without
/// blocking: a Unix-socket connect either completes, is refused (nobody listens: a stale
/// file), or would block only because a live listener's backlog is full.
fn probe_live(path: &Path) -> io::Result<bool> {
    // SAFETY: sockaddr_un is plain old data; all zeroes is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket path {} is too long", path.display()),
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    // SAFETY: plain socket creation.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just created and is owned only here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl on a valid descriptor.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `addr` is a valid sockaddr_un of the given size.
    let r = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if r == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EAGAIN | libc::EINPROGRESS) => Ok(true),
        Some(libc::ECONNREFUSED | libc::ENOENT) => Ok(false),
        _ => Err(e),
    }
}

/// Mode `0600`, or `0660` with the socket's group set to `group`.
fn restrict(path: &Path, access: &Access) -> Result<(), AdminError> {
    let mode = match access {
        Access::CurrentUserOnly => 0o600,
        Access::UnixGroup { group } => {
            let gid = group_id(group)?;
            std::os::unix::fs::chown(path, None, Some(gid))?;
            0o660
        }
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn group_id(name: &str) -> Result<u32, AdminError> {
    let c = CString::new(name)
        .map_err(|_| AdminError::InvalidAccess(format!("group name {name:?} contains NUL")))?;
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: `group` is plain old data that getgrnam_r fills in.
        let mut group: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and `buf.len()` is its real size.
        let r = unsafe {
            libc::getgrnam_r(
                c.as_ptr(),
                &mut group,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut found,
            )
        };
        if r == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r).into());
        }
        if found.is_null() {
            return Err(AdminError::InvalidAccess(format!(
                "no group named {name:?}"
            )));
        }
        return Ok(group.gr_gid);
    }
}

/// Connects to the socket; a missing socket or one nobody listens on is
/// [`AdminError::NotRunning`].
pub(crate) async fn connect(path: &Path) -> Result<AdminStream, AdminError> {
    match UnixStream::connect(path).await {
        Ok(s) => Ok(Box::new(s)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(AdminError::NotRunning)
        }
        Err(e) => Err(e.into()),
    }
}
