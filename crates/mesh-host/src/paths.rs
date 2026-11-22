//! Where the daemon keeps its data (spec §6.1).

use std::path::{Path, PathBuf};

pub use mesh_admin::{START_ERROR_FILE, start_error};

/// The environment variable that overrides the default data directory.
pub const DATA_ENV: &str = "MESHD_DATA";

/// `MESHD_DATA` when set, else the OS default: Linux `/var/lib/meshnode`, Windows
/// `%ProgramData%\meshnode`, macOS `/Library/Application Support/meshnode`.
pub fn default_data_dir() -> PathBuf {
    match std::env::var_os(DATA_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => mesh_admin::default_data_dir(),
    }
}

/// `<data>/logs`: the daily rolling log files.
pub fn logs_dir(data: &Path) -> PathBuf {
    data.join("logs")
}

/// `<data>/tmp`: staging for static uploads, emptied at start.
pub fn tmp_dir(data: &Path) -> PathBuf {
    data.join("tmp")
}

/// Records why a start was refused in `<data>/start-error.txt` (atomically), for
/// `meshd status` and the desktop app while no daemon answers.
pub fn write_start_error(data: &Path, message: &str) -> std::io::Result<()> {
    let path = data.join(START_ERROR_FILE);
    mesh_core::local::atomic_write(&path, format!("{message}\n").as_bytes()).map_err(
        |e| match e {
            mesh_core::NodeError::Io(e) => e,
            e => std::io::Error::other(e.to_string()),
        },
    )?;
    // The Windows service's data directory is LocalService/Administrators only; the
    // message (a version notice, not a secret) must reach `meshd status` and the desktop
    // app run as a standard user.
    #[cfg(windows)]
    mesh_core::local::grant_read_to_authenticated_users(&path)?;
    Ok(())
}

/// Removes `<data>/start-error.txt` (a start succeeded, or failed for another reason); a
/// missing file is fine.
pub fn clear_start_error(data: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(data.join(START_ERROR_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
