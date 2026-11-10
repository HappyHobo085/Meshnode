//! Logging (spec §6.5): `tracing` to stdout and to a daily rolling file
//! `<data>/logs/meshd.log.<date>`, 7 files kept. `RUST_LOG` overrides the level. Also
//! answers `LogsTail` from those files.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use mesh_admin::{AdminResponse, ErrorCode, RequestIo};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::paths::logs_dir;

/// The log files' name before the date suffix.
pub const LOG_PREFIX: &str = "meshd.log";
/// Daily files kept.
pub const LOG_FILES_KEPT: usize = 7;
/// How often a following `LogsTail` looks for new lines.
const FOLLOW_POLL: Duration = Duration::from_millis(250);
/// Lines per `LogLines` frame.
const LINES_PER_FRAME: usize = 1000;
/// Bytes read per look at a log file.
const READ_CHUNK: u64 = 1024 * 1024;

/// Keeps the file writer flushing; drop it last, when the process ends.
pub struct LoggingGuard {
    _file: Option<WorkerGuard>,
}

/// Installs the global subscriber: stdout, plus the rolling file when the log directory
/// can be created (otherwise stdout only, with a warning). `level` is the `log_level`
/// setting; `RUST_LOG`, when set, wins. A second call leaves the first subscriber in place.
pub fn init(data_dir: &Path, level: &str) -> LoggingGuard {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let dir = logs_dir(data_dir);
    let appender = std::fs::create_dir_all(&dir)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            Builder::new()
                .rotation(Rotation::DAILY)
                .filename_prefix(LOG_PREFIX)
                .max_log_files(LOG_FILES_KEPT)
                .build(&dir)
                .map_err(|e| e.to_string())
        });
    let stdout = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);
    match appender {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stdout)
                .with(file)
                .try_init();
            LoggingGuard { _file: Some(guard) }
        }
        Err(e) => {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stdout)
                .try_init();
            tracing::warn!("cannot write log files in {}: {e}", dir.display());
            LoggingGuard { _file: None }
        }
    }
}

/// Answers `LogsTail`: the last `lines` lines of the newest log file in `dir`, then, with
/// `follow`, new lines (also from a newer file once the day rolls over) until the client
/// disconnects.
pub(crate) async fn tail(
    dir: &Path,
    lines: u32,
    follow: bool,
    io: &mut RequestIo<'_>,
) -> AdminResponse {
    let d = dir.to_path_buf();
    let first = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let Some(file) = newest_log(&d)? else {
            return Ok((None, 0, Vec::new()));
        };
        let (lines, end) = last_lines(&file, lines as usize)?;
        Ok((Some(file), end, lines))
    })
    .await;
    let (mut file, mut pos, first) = match first {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return AdminResponse::error(ErrorCode::Internal, format!("reading the log: {e}"));
        }
        Err(e) => return AdminResponse::error(ErrorCode::Internal, e.to_string()),
    };
    if send(io, first).await.is_err() || !follow {
        return AdminResponse::Ok;
    }
    loop {
        tokio::select! {
            _ = io.client_gone() => return AdminResponse::Ok,
            _ = tokio::time::sleep(FOLLOW_POLL) => {}
        }
        let d = dir.to_path_buf();
        let cur = file.clone();
        let new = match tokio::task::spawn_blocking(move || new_lines(&d, cur, pos)).await {
            Ok(Ok((f, p, new))) => {
                (file, pos) = (f, p);
                new
            }
            Ok(Err(e)) => {
                tracing::debug!("following the log: {e}");
                continue;
            }
            Err(e) => return AdminResponse::error(ErrorCode::Internal, e.to_string()),
        };
        if send(io, new).await.is_err() {
            return AdminResponse::Ok;
        }
    }
}

async fn send(io: &mut RequestIo<'_>, lines: Vec<String>) -> io::Result<()> {
    for chunk in lines.chunks(LINES_PER_FRAME) {
        io.send_lines(chunk.to_vec()).await?;
    }
    Ok(())
}

/// The newest `meshd.log.<date>` file in `dir` (dates sort by name).
fn newest_log(dir: &Path) -> io::Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let prefix = format!("{LOG_PREFIX}.");
    let mut newest: Option<PathBuf> = None;
    for e in entries {
        let e = e?;
        let name = e.file_name();
        let is_log = name.to_str().is_some_and(|n| n.starts_with(&prefix));
        let newer = newest
            .as_ref()
            .is_none_or(|p| p.file_name() < Some(name.as_os_str()));
        if is_log && newer && e.file_type()?.is_file() {
            newest = Some(e.path());
        }
    }
    Ok(newest)
}

/// The last `n` complete lines of `path`, and the offset just after them (a partial last
/// line is left for following).
fn last_lines(path: &Path, n: usize) -> io::Result<(Vec<String>, u64)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    // Read backwards until the buffer holds more than `n` line ends, or the whole file.
    let mut buf: Vec<u8> = Vec::new();
    let mut start = len;
    while start > 0 && buf.iter().filter(|b| **b == b'\n').count() <= n {
        let step = start.min(64 * 1024);
        start -= step;
        let mut chunk = vec![0; step as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }
    let complete = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let end = start + complete as u64;
    let mut lines = split_lines(&buf[..complete]);
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // may be cut at the front
    }
    let skip = lines.len().saturating_sub(n);
    Ok((lines.split_off(skip), end))
}

/// Complete lines added to `cur` after `pos`; once `cur` has nothing new and a newer file
/// exists, that file's lines from its start.
fn new_lines(
    dir: &Path,
    cur: Option<PathBuf>,
    pos: u64,
) -> io::Result<(Option<PathBuf>, u64, Vec<String>)> {
    if let Some(p) = &cur {
        let (lines, end) = read_from(p, pos)?;
        if !lines.is_empty() {
            return Ok((cur, end, lines));
        }
    }
    match newest_log(dir)? {
        Some(newest) if Some(&newest) != cur.as_ref() => {
            let (lines, end) = read_from(&newest, 0)?;
            Ok((Some(newest), end, lines))
        }
        _ => Ok((cur, pos, Vec::new())),
    }
}

/// Complete lines of `path` from `pos` (at most [`READ_CHUNK`] bytes; a longer line comes
/// in pieces), and the offset after them. A file that shrank is read from 0; one that
/// vanished reads as empty.
fn read_from(path: &Path, pos: u64) -> io::Result<(Vec<String>, u64)> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), pos)),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    let pos = if len < pos { 0 } else { pos };
    let want = (len - pos).min(READ_CHUNK);
    let mut buf = vec![0; want as usize];
    f.seek(SeekFrom::Start(pos))?;
    f.read_exact(&mut buf)?;
    let used = match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => i + 1,
        None if want == READ_CHUNK => buf.len(),
        None => 0,
    };
    Ok((split_lines(&buf[..used]), pos + used as u64))
}

fn split_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_lines_and_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(newest_log(dir.path()).unwrap(), None);
        let old = dir.path().join("meshd.log.2026-11-09");
        let cur = dir.path().join("meshd.log.2026-11-10");
        std::fs::write(&old, "x\n").unwrap();
        std::fs::write(dir.path().join("other.txt"), "y\n").unwrap();
        let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&cur, format!("{text}partial")).unwrap();
        assert_eq!(newest_log(dir.path()).unwrap().as_ref(), Some(&cur));
        let (lines, end) = last_lines(&cur, 3).unwrap();
        assert_eq!(lines, ["line 19997", "line 19998", "line 19999"]);
        assert_eq!(end, text.len() as u64);
        let (all, _) = last_lines(&cur, 50_000).unwrap();
        assert_eq!(all.len(), 20_000);
        assert!(last_lines(&old, 0).unwrap().0.is_empty());
        // The partial line completes; nothing more; then the day rolls over.
        std::fs::write(&cur, format!("{text}partial done\n")).unwrap();
        let (f, pos, l) = new_lines(dir.path(), Some(cur.clone()), end).unwrap();
        assert_eq!(
            (l, f.as_ref()),
            (vec!["partial done".to_string()], Some(&cur))
        );
        let (f, pos2, l) = new_lines(dir.path(), f, pos).unwrap();
        assert_eq!((l.len(), f.as_ref(), pos2), (0, Some(&cur), pos));
        let next = dir.path().join("meshd.log.2026-11-11");
        std::fs::write(&next, "tomorrow\n").unwrap();
        let (f, _, l) = new_lines(dir.path(), Some(cur.clone()), pos).unwrap();
        assert_eq!((l, f), (vec!["tomorrow".to_string()], Some(next)));
    }
}
