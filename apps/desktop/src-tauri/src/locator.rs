//! Finding the daemon to talk to (spec §4.1): the system service, else a daemon running as
//! the user. Each candidate is probed with a `Hello` that must answer within 1 s.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use futures_util::future::join_all;
use mesh_admin::{AdminClient, Endpoint, endpoint_for};
use serde::Deserialize;

use crate::launch::user_data_dir;

/// How long a candidate has to answer `Hello`.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(1);

/// Which daemon: the installed system service, or one running as the user.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    System,
    User,
}

/// A data directory a daemon may be serving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: ConnectionKind,
    pub data_dir: PathBuf,
}

/// The candidates, in order of preference.
#[derive(Clone, Debug, Default)]
pub struct DaemonLocator {
    pub candidates: Vec<Candidate>,
}

/// Whether a daemon answers `Hello` on `endpoint` within [`HELLO_TIMEOUT`].
pub async fn answers(endpoint: &Endpoint) -> bool {
    matches!(
        tokio::time::timeout(HELLO_TIMEOUT, AdminClient::connect(endpoint)).await,
        Ok(Ok(_))
    )
}

impl DaemonLocator {
    /// The first candidate whose daemon answers, its endpoint, and whether another
    /// candidate's daemon answers too. All candidates are probed at once, so this takes at
    /// most about [`HELLO_TIMEOUT`].
    pub async fn locate(&self) -> Option<(ConnectionKind, Endpoint, bool)> {
        let endpoints: Vec<Endpoint> = self
            .candidates
            .iter()
            .map(|c| endpoint_for(&c.data_dir))
            .collect();
        let up = join_all(endpoints.iter().map(answers)).await;
        let first = up.iter().position(|&a| a)?;
        let other = up
            .iter()
            .enumerate()
            .any(|(i, &a)| a && i != first && endpoints[i] != endpoints[first]);
        Some((self.candidates[first].kind, endpoints[first].clone(), other))
    }
}

/// The candidates of spec §4.1: the system service's data directory (as its registration
/// names it, else the platform default), then the per-user directory.
pub fn default_candidates(service_data_dir: Option<PathBuf>) -> Vec<Candidate> {
    let system = service_data_dir.unwrap_or_else(mesh_admin::default_data_dir);
    let user = user_data_dir();
    let mut out = vec![Candidate {
        kind: ConnectionKind::System,
        data_dir: system,
    }];
    if endpoint_for(&user) != endpoint_for(&out[0].data_dir) {
        out.push(Candidate {
            kind: ConnectionKind::User,
            data_dir: user,
        });
    }
    out
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStatusLine {
    data_dir: Option<PathBuf>,
}

/// The installed service's data directory, from `meshd service status --json` (read-only).
/// `None` when it is not installed, names none, or `meshd` cannot be run.
pub fn service_data_dir(meshd: &Path) -> Option<PathBuf> {
    let mut command = Command::new(meshd);
    command
        .args(["service", "status", "--json"])
        .env_remove("MESHD_DATA")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let out = command.output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_service_status(&String::from_utf8_lossy(&out.stdout))
}

/// The `dataDir` of a `service status --json` line.
pub fn parse_service_status(line: &str) -> Option<PathBuf> {
    serde_json::from_str::<ServiceStatusLine>(line.trim())
        .ok()?
        .data_dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_status_line_is_parsed() {
        assert_eq!(
            parse_service_status("{\"status\":\"running\",\"dataDir\":\"/var/lib/meshnode\"}\n"),
            Some(PathBuf::from("/var/lib/meshnode"))
        );
        assert_eq!(
            parse_service_status("{\"status\":\"not-installed\",\"dataDir\":null}"),
            None
        );
        assert_eq!(parse_service_status("garbage"), None);
    }

    #[test]
    fn default_candidates_are_system_then_user() {
        let c = default_candidates(Some(PathBuf::from("/srv/meshnode")));
        assert_eq!(c[0].kind, ConnectionKind::System);
        assert_eq!(c[0].data_dir, PathBuf::from("/srv/meshnode"));
        assert_eq!(c[1].kind, ConnectionKind::User);
        assert_eq!(c[1].data_dir, user_data_dir());
        let c = default_candidates(None);
        assert_eq!(c[0].data_dir, mesh_admin::default_data_dir());
    }
}
