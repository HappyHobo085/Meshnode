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
            if is_os_file(name) {
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

/// A file the OS leaves in folders (`.DS_Store`, `Thumbs.db`, `desktop.ini`): never uploaded.
pub fn is_os_file(name: &str) -> bool {
    OS_FILES.contains(&name)
}

/// The upload rules every source applies to its listing, from the listed sizes and before
/// any file is read: drops OS files, refuses an empty listing, sorts by path, and enforces
/// mesh-admin's limits (files, bytes per file and in total) and path rules, naming the
/// offending path.
pub fn check_listing(files: Vec<FileMeta>) -> Result<Vec<FileMeta>, UiError> {
    let mut files: Vec<FileMeta> = files
        .into_iter()
        .filter(|f| !is_os_file(f.path.rsplit('/').next().unwrap_or("")))
        .collect();
    if files.is_empty() {
        return Err(UiError::invalid("The folder is empty"));
    }
    if files.len() > MAX_UPLOAD_FILES {
        return Err(UiError::invalid(format!(
            "The folder has more than {MAX_UPLOAD_FILES} files"
        )));
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

/// Reads the file `path`, listed with `size` bytes, through `read_at(offset, len)` (which
/// returns at most `len` bytes, fewer only at the end) in chunks of at most `chunk` bytes.
/// The result is allocated once at `size`; a file that is shorter or longer than listed (it
/// changed since the listing) is refused, so no more than the listed bytes are ever held.
pub fn read_sized(
    path: &str,
    size: u64,
    chunk: usize,
    mut read_at: impl FnMut(u64, usize) -> Result<Vec<u8>, UiError>,
) -> Result<Vec<u8>, UiError> {
    let changed = || UiError::invalid(format!("{path} changed while it was read"));
    let len = usize::try_from(size).map_err(|_| changed())?;
    let chunk = chunk.max(1);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let want = chunk.min(len - out.len());
        let got = read_at(out.len() as u64, want)?;
        if got.is_empty() || got.len() > want {
            return Err(changed());
        }
        out.extend_from_slice(&got);
    }
    // One byte past the listed size must not exist.
    if !read_at(size, 1)?.is_empty() {
        return Err(changed());
    }
    Ok(out)
}

fn io_err(e: std::io::Error) -> UiError {
    UiError::new(UiErrorCode::Io, e.to_string())
}

impl FileSource for DirSource {
    fn list(&mut self) -> Result<Vec<FileMeta>, UiError> {
        let mut files = Vec::new();
        Self::walk(&self.root, "", &mut files)?;
        check_listing(files)
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
