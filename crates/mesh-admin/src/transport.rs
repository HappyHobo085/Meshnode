//! Where the admin endpoint lives and who may open it (spec §5.2).

use std::fmt;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncRead, AsyncWrite};

use crate::AdminError;

/// A byte stream to or from the admin endpoint.
pub trait AdminIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AdminIo for T {}

pub type AdminStream = Box<dyn AdminIo>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix domain socket at this path.
    #[cfg(unix)]
    Socket(PathBuf),
    /// A named pipe with this full name (`\\.\pipe\...`).
    #[cfg(windows)]
    Pipe(String),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(unix)]
            Endpoint::Socket(p) => write!(f, "{}", p.display()),
            #[cfg(windows)]
            Endpoint::Pipe(name) => f.write_str(name),
        }
    }
}

/// Who besides the daemon's own account may open the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Unix: mode `0600`. Windows: the daemon's account and Administrators.
    CurrentUserOnly,
    /// Unix: mode `0660`, group set to this group (`meshnode` on Linux, `admin` on macOS).
    #[cfg(unix)]
    UnixGroup { group: String },
    /// Windows: the daemon's account, Administrators, and this user SID
    /// (`[admin] windows_user_sid`).
    #[cfg(windows)]
    WindowsUser { sid: String },
}

/// The OS default data directory: Linux `/var/lib/meshnode`, Windows
/// `%ProgramData%\meshnode`, macOS `/Library/Application Support/meshnode`.
pub fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        PathBuf::from(base).join("meshnode")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/meshnode")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        PathBuf::from("/var/lib/meshnode")
    }
}

/// The file in the data directory that holds why the last start was refused for
/// incompatible data (written by the daemon, removed by its next successful start), so that
/// clients can say why no daemon answers.
pub const START_ERROR_FILE: &str = "start-error.txt";

/// The first line of `<data_dir>/start-error.txt`, when there is one. A file that exists
/// but cannot be read is reported as `(start error unreadable: <error>)` rather than
/// hidden, except on Unix when permission is denied: there that is what a user outside the
/// data directory's group always gets, file or not, so whether the daemon refused is
/// unknown and nothing is reported (see [`access_denied`]).
pub fn start_error(data_dir: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(data_dir.join(START_ERROR_FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        #[cfg(unix)]
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(e) => return Some(format!("(start error unreadable: {e})")),
    };
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// Whether this user is denied access to the data directory's admin endpoint (Linux: the
/// service's directory is `0750 meshnode:meshnode` and the user is not, or not yet, in the
/// group). Always `false` on Windows.
pub fn access_denied(data_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        std::fs::metadata(data_dir.join("admin.sock"))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        false
    }
}

/// The admin endpoint for a data directory. Unix: `<data>/admin.sock`. Windows:
/// `\\.\pipe\meshnode-admin` for the default data directory, else
/// `\\.\pipe\meshnode-admin-<first 8 hex of blake3(canonical data dir)>`.
pub fn endpoint_for(data_dir: &Path) -> Endpoint {
    #[cfg(unix)]
    {
        Endpoint::Socket(data_dir.join("admin.sock"))
    }
    #[cfg(windows)]
    {
        let dir = canonical(data_dir);
        if dir == canonical(&default_data_dir()) {
            return Endpoint::Pipe(r"\\.\pipe\meshnode-admin".into());
        }
        let hash = blake3::hash(dir.as_bytes());
        Endpoint::Pipe(format!(r"\\.\pipe\meshnode-admin-{}", &hash.to_hex()[..8]))
    }
}

/// The canonical form of a directory, for naming its pipe: the resolved path when it
/// exists, else the absolute path; without the `\\?\` prefix of a drive path, and
/// lowercase, since Windows paths ignore case.
#[cfg(windows)]
fn canonical(dir: &Path) -> String {
    let p = std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf());
    let s = p.to_string_lossy().to_lowercase();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with(r"unc\") => rest.to_string(),
        _ => s,
    }
}

/// Opens a raw stream to the endpoint, without the `Hello` exchange.
/// [`AdminError::NotRunning`] when nothing listens there.
pub async fn connect_raw(endpoint: &Endpoint) -> Result<AdminStream, AdminError> {
    match endpoint {
        #[cfg(unix)]
        Endpoint::Socket(path) => crate::transport_unix::connect(path).await,
        #[cfg(windows)]
        Endpoint::Pipe(name) => crate::transport_windows::connect(name).await,
    }
}

#[cfg(unix)]
pub(crate) use crate::transport_unix::Listener;
#[cfg(windows)]
pub(crate) use crate::transport_windows::Listener;

/// Binds the platform listener for an endpoint, with its access control applied.
pub(crate) fn bind(endpoint: &Endpoint, access: &Access) -> Result<Listener, AdminError> {
    match endpoint {
        #[cfg(unix)]
        Endpoint::Socket(path) => Listener::bind(path, access),
        #[cfg(windows)]
        Endpoint::Pipe(name) => Listener::bind(name, access),
    }
}

#[cfg(test)]
mod start_error_tests {
    use super::*;

    #[test]
    fn start_error_reads_the_first_line_and_reports_an_unreadable_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(start_error(dir.path()), None);
        std::fs::write(dir.path().join(START_ERROR_FILE), "why\nmore\n").unwrap();
        assert_eq!(start_error(dir.path()).as_deref(), Some("why"));
        // Not readable as a file: said, not swallowed.
        std::fs::remove_file(dir.path().join(START_ERROR_FILE)).unwrap();
        std::fs::create_dir(dir.path().join(START_ERROR_FILE)).unwrap();
        let shown = start_error(dir.path()).unwrap();
        assert!(shown.starts_with("(start error unreadable: "), "{shown}");
    }

    /// A data directory this user may not search (Linux: not in the `meshnode` group):
    /// whether the daemon refused is unknown, so nothing is reported.
    #[cfg(unix)]
    #[test]
    fn start_error_in_a_directory_without_access_is_unknown() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join(START_ERROR_FILE), "why\n").unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores the mode: the case cannot be made.
        let denied = std::fs::metadata(data.join(START_ERROR_FILE))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        let (shown, no_access) = (start_error(&data), access_denied(&data));
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        if denied {
            assert_eq!(shown, None);
            assert!(no_access);
        }
        assert!(!access_denied(&data));
        assert!(!access_denied(&dir.path().join("missing")));
    }
}
