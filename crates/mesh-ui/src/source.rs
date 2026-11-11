//! Where the files of a static deploy come from: a desktop folder, or (on Android) a
//! picked document tree implemented by the shell.

use std::fs;
use std::path::{Path, PathBuf};

use mesh_admin::{FileMeta, MAX_UPLOAD_FILE, MAX_UPLOAD_FILES, MAX_UPLOAD_TOTAL, validate_upload};

use crate::error::{UiError, UiErrorCode};

pub trait FileSource: Send {
    /// Every file to upload, sorted by path, with `/`-separated relative paths.
    fn list(&mut self) -> Result<Vec<FileMeta>, UiError>;
    /// The contents of one file listed by `list`.
    fn read(&mut self, path: &str) -> Result<Vec<u8>, UiError>;
}

/// Files of a desktop folder, walked recursively.
pub struct DirSource {
    root: PathBuf,
}

const OS_FILES: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];

impl DirSource {
    pub fn new(root: PathBuf) -> DirSource {
        DirSource { root }
    }

    fn walk(dir: &Path, prefix: &str, out: &mut Vec<FileMeta>) -> Result<(), UiError> {
        for entry in fs::read_dir(dir).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                UiError::invalid(format!(
                    "{} has a name that is not valid text",
                    entry.path().display()
                ))
            })?;
            if OS_FILES.contains(&name) {
                continue;
            }
            // `file_type` does not follow symlinks.
            let ft = entry.file_type().map_err(io_err)?;
            if ft.is_symlink() {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if ft.is_dir() {
                Self::walk(&entry.path(), &rel, out)?;
            } else if ft.is_file() {
                if out.len() >= MAX_UPLOAD_FILES {
                    return Err(UiError::invalid(format!(
                        "The folder has more than {MAX_UPLOAD_FILES} files"
                    )));
                }
                let size = entry.metadata().map_err(io_err)?.len();
                out.push(FileMeta { path: rel, size });
            }
        }
        Ok(())
    }
}

fn io_err(e: std::io::Error) -> UiError {
    UiError::new(UiErrorCode::Io, e.to_string())
}

impl FileSource for DirSource {
    fn list(&mut self) -> Result<Vec<FileMeta>, UiError> {
        let mut files = Vec::new();
        Self::walk(&self.root, "", &mut files)?;
        if files.is_empty() {
            return Err(UiError::invalid("The folder is empty"));
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut total = 0u64;
        for f in &files {
            if f.size > MAX_UPLOAD_FILE {
                return Err(UiError::invalid(format!(
                    "{} is larger than 64 MiB",
                    f.path
                )));
            }
            total = total.saturating_add(f.size);
        }
        if total > MAX_UPLOAD_TOTAL {
            return Err(UiError::invalid("The folder is larger than 1 GiB in total"));
        }
        validate_upload(&files).map_err(UiError::invalid)?;
        Ok(files)
    }

    fn read(&mut self, path: &str) -> Result<Vec<u8>, UiError> {
        let mut full = self.root.clone();
        for seg in path.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." || seg.contains(['\\', ':']) {
                return Err(UiError::invalid(format!("invalid file path {path:?}")));
            }
            full.push(seg);
        }
        fs::read(&full).map_err(io_err)
    }
}
