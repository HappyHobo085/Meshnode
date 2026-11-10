//! The admin endpoint as a Unix domain socket, protected by its file mode and group.

use std::ffi::CString;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use crate::AdminError;
use crate::transport::{Access, AdminStream};

pub(crate) struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Binds the socket, replacing a stale socket file nobody answers on;
    /// [`AdminError::InUse`] if another server answers there.
    pub(crate) fn bind(path: &Path, access: &Access) -> Result<Listener, AdminError> {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(AdminError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                )));
            }
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(AdminError::InUse(path.display().to_string()));
            }
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        let inner = UnixListener::bind(path)?;
        let listener = Listener {
            inner,
            path: path.to_path_buf(),
        };
        // On failure `listener` is dropped, which removes the socket file.
        restrict(path, access)?;
        Ok(listener)
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
