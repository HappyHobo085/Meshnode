//! The daemon locator, relocation and "run without installing", against real daemons on
//! temporary data directories. The machine's own data directories are never used: the
//! "system" and "user" candidates are temp dirs too.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mesh_admin::endpoint_for;
use mesh_host::config::GatewaySection;
use mesh_host::{Daemon, HostConfig, TestOptions};
use mesh_ui::{AdminTransport, ConnectionKindDto, Platform, Ui, UiErrorCode};
use meshnode_desktop::launch::{run_user_command, spawn_detached};
use meshnode_desktop::locator::{Candidate, ConnectionKind, DaemonLocator};
use meshnode_desktop::relocating::RelocatingTransport;
use tempfile::TempDir;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn test_config() -> HostConfig {
    HostConfig {
        relay: "disabled".into(),
        discovery: false,
        gateway: GatewaySection {
            listen: SocketAddr::from(([127, 0, 0, 1], free_port())),
            ports: BTreeMap::new(),
        },
        ..HostConfig::default()
    }
}

async fn daemon_on(dir: &Path) -> Daemon {
    Daemon::start_with(dir.to_path_buf(), test_config(), TestOptions::fast())
        .await
        .expect("daemon")
}

fn locator(system: &Path, user: &Path) -> DaemonLocator {
    DaemonLocator {
        candidates: vec![
            Candidate {
                kind: ConnectionKind::System,
                data_dir: system.to_path_buf(),
            },
            Candidate {
                kind: ConnectionKind::User,
                data_dir: user.to_path_buf(),
            },
        ],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn locator_prefers_system_and_flags_other() {
    let (system, user) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let d1 = daemon_on(system.path()).await;
    let d2 = daemon_on(user.path()).await;
    let (kind, endpoint, other) = locator(system.path(), user.path())
        .locate()
        .await
        .expect("a daemon");
    assert_eq!(kind, ConnectionKind::System);
    assert_eq!(endpoint, endpoint_for(system.path()));
    assert!(other, "the user daemon answers too");
    d1.shutdown().await;
    d2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn locator_falls_back_to_user() {
    let (system, user) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let d = daemon_on(user.path()).await;
    let (kind, endpoint, other) = locator(system.path(), user.path())
        .locate()
        .await
        .expect("a daemon");
    assert_eq!(kind, ConnectionKind::User);
    assert_eq!(endpoint, endpoint_for(user.path()));
    assert!(!other);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn locator_none_when_nothing_answers() {
    let (system, user) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let started = Instant::now();
    let found = locator(system.path(), user.path()).locate().await;
    assert!(found.is_none(), "{found:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

/// No daemon answers, and one candidate's last start was refused: the connection names why.
#[tokio::test(flavor = "multi_thread")]
async fn connection_without_daemon_carries_the_start_error() {
    let (system, user) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let transport = RelocatingTransport::new(locator(system.path(), user.path()));
    transport.relocate().await;
    assert_eq!(transport.connection().start_error, None);
    std::fs::write(
        user.path().join(mesh_admin::START_ERROR_FILE),
        "data was written by an incompatible Meshnode version\nmore\n",
    )
    .unwrap();
    let c = transport.connection();
    assert_eq!(c.kind, ConnectionKindDto::None);
    assert_eq!(
        c.start_error.as_deref(),
        Some("data was written by an incompatible Meshnode version")
    );
}

/// The system service's data directory is there but this user may not search it (Linux:
/// not in the `meshnode` group): the connection says so, and reports no start error.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn connection_without_access_to_the_service_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let (root, user) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let system = root.path().join("meshnode");
    std::fs::create_dir(&system).unwrap();
    let transport = RelocatingTransport::new(locator(&system, user.path()));
    transport.relocate().await;
    assert!(!transport.connection().no_access);
    std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root ignores the mode: the case cannot be made.
    let denied = std::fs::read_dir(&system).is_err();
    let c = transport.connection();
    std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(c.kind, ConnectionKindDto::None);
    assert_eq!(c.no_access, denied);
    assert_eq!(c.start_error, None);
}

/// A command through the desktop `Ui` after its daemon went away: the first attempt fails
/// with `DaemonUnreachable`, the transport locates once more and retries.
#[tokio::test(flavor = "multi_thread")]
async fn locator_relocates_after_daemon_restart() {
    let (system, user) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let d = daemon_on(user.path()).await;
    let transport = Arc::new(RelocatingTransport::new(locator(
        system.path(),
        user.path(),
    )));
    let t = transport.clone();
    let ui = Ui::new(
        Arc::new(move || t.clone() as Arc<dyn AdminTransport>),
        Platform::DesktopWindows,
    );
    // The first command finds the daemon.
    ui.status().await.expect("status");
    assert_eq!(transport.connection().kind, ConnectionKindDto::User);
    let base = transport.relocations();

    // It goes away: one relocate, which finds nothing.
    d.shutdown().await;
    let err = ui.status().await.unwrap_err();
    assert_eq!(err.code, UiErrorCode::DaemonUnreachable, "{err:?}");
    assert_eq!(transport.relocations(), base + 1);
    assert_eq!(transport.connection().kind, ConnectionKindDto::None);

    // A new daemon on the same directory: the next command locates it and succeeds.
    let d = daemon_on(user.path()).await;
    ui.status().await.expect("status after restart");
    assert_eq!(transport.relocations(), base + 2);
    assert_eq!(transport.connection().kind, ConnectionKindDto::User);

    // It goes away and one comes up on the other candidate: the stale endpoint fails, one
    // relocate finds the new daemon and the retry succeeds.
    d.shutdown().await;
    let d = daemon_on(system.path()).await;
    ui.status().await.expect("status after moving");
    assert_eq!(transport.relocations(), base + 3);
    assert_eq!(transport.connection().kind, ConnectionKindDto::System);
    d.shutdown().await;
}

/// The `meshd` the spawn test runs: `$MESHD_BIN` when set (CI builds it and sets this; it
/// must then exist), else one built in this target directory (`cargo build -p meshd`).
/// Without one the test is skipped locally, but fails under CI (`$CI` set), so it can never
/// pass silently there.
fn meshd_for_spawn_test() -> Option<PathBuf> {
    if let Some(bin) = std::env::var_os("MESHD_BIN") {
        let bin = PathBuf::from(bin);
        assert!(bin.is_file(), "MESHD_BIN={} is not a file", bin.display());
        return Some(bin);
    }
    let found = built_meshd();
    assert!(
        found.is_some() || std::env::var_os("CI").is_none(),
        "run_user_spawns_daemon needs meshd under CI: build it (cargo build -p meshd) and set MESHD_BIN"
    );
    found
}

/// A `meshd` binary built in this target directory (`cargo build -p meshd`), if any.
fn built_meshd() -> Option<PathBuf> {
    let exe = format!("meshd{}", std::env::consts::EXE_SUFFIX);
    let mut dirs = Vec::new();
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR") {
        dirs.push(PathBuf::from(t).join("debug"));
    }
    // <target>/debug/deps/<this test>
    if let Ok(me) = std::env::current_exe()
        && let Some(debug) = me.parent().and_then(Path::parent)
    {
        dirs.push(debug.to_path_buf());
    }
    dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Kills the spawned daemon however the test ends.
struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// "Run without installing" against a temp data directory: the spawned `meshd run` answers
/// within 10 s; the test then kills it. The OS service is never involved.
#[tokio::test(flavor = "multi_thread")]
async fn run_user_spawns_daemon() {
    let Some(meshd) = meshd_for_spawn_test() else {
        eprintln!(
            "run_user_spawns_daemon: SKIPPED, no built meshd (cargo build -p meshd, or set MESHD_BIN)"
        );
        return;
    };
    let data = TempDir::new().unwrap();
    // Off the network, the gateway on a free loopback port.
    std::fs::write(
        data.path().join("meshd.toml"),
        format!(
            "relay = \"disabled\"\ndiscovery = false\n[gateway]\nlisten = \"127.0.0.1:{}\"\nports = {{}}\n",
            free_port()
        ),
    )
    .unwrap();
    let cmd = run_user_command(&meshd, data.path());
    let log = data.path().join("meshd.log");
    let child = Kill(spawn_detached(&cmd, &log).expect("spawn meshd run"));
    let nowhere = TempDir::new().unwrap();
    let locator = locator(nowhere.path(), data.path());
    let deadline = Instant::now() + Duration::from_secs(10);
    let found = loop {
        if let Some(found) = locator.locate().await {
            break found;
        }
        assert!(
            Instant::now() < deadline,
            "meshd run did not answer within 10 s; log: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(found.0, ConnectionKind::User);
    drop(child);
}
