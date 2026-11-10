//! Where the daemon keeps its data (spec §6.1).

use std::path::{Path, PathBuf};

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
