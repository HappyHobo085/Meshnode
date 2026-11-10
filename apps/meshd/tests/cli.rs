//! The `meshd` binary, run as a child process on temporary data directories.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const NOT_RUNNING: &str =
    "meshd is not running (start it with \"meshd run\" or \"meshd service install\")";

fn meshd(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_meshd"))
        .env_remove("MESHD_DATA")
        .env_remove("RUST_LOG")
        .arg("--data")
        .arg(data)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run meshd")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A data directory whose config keeps the node off the network and the gateway on a free
/// loopback port.
fn offline_dir() -> TempDir {
    let dir = TempDir::new().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(
        dir.path().join("meshd.toml"),
        format!(
            "relay = \"disabled\"\ndiscovery = false\n[gateway]\nlisten = \"127.0.0.1:{port}\"\nports = {{}}\n"
        ),
    )
    .unwrap();
    dir
}

/// A `meshd run` child, killed on drop.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Initialises `dir` locally and starts a daemon on it, waiting for its admin endpoint.
fn start_daemon(dir: &Path) -> Running {
    let o = meshd(dir, &["init", "--name", "pc"]);
    assert!(o.status.success(), "init: {} {}", stdout(&o), stderr(&o));
    let child = Command::new(env!("CARGO_BIN_EXE_meshd"))
        .env_remove("MESHD_DATA")
        .arg("--data")
        .arg(dir)
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn meshd run");
    let running = Running(child);
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let o = meshd(dir, &["status", "--json"]);
        if o.status.success() && stdout(&o).contains("\"Running\"") {
            return running;
        }
        assert!(Instant::now() < deadline, "daemon did not come up");
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn cli_reports_daemon_not_running() {
    let dir = TempDir::new().unwrap();
    for cmd in [&["status"][..], &["members"], &["services"], &["logs"]] {
        let o = meshd(dir.path(), cmd);
        assert_eq!(o.status.code(), Some(3), "{cmd:?}");
        assert_eq!(stderr(&o).trim(), NOT_RUNNING, "{cmd:?}");
    }
}

#[test]
fn init_local_then_status_through_daemon() {
    let dir = offline_dir();
    let _daemon = start_daemon(dir.path());
    let o = meshd(dir.path(), &["status"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.contains("pc"), "{text}");
    assert!(text.contains("running"), "{text}");
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&meshd(dir.path(), &["status", "--json"]))).unwrap();
    assert_eq!(v["name"], "pc");
    assert_eq!(v["state"], "Running");
    // Init through the daemon again is refused with the daemon's message.
    let o = meshd(dir.path(), &["init", "--name", "again"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("already belongs to a mesh"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn join_bad_ticket_readable_error() {
    let dir = offline_dir();
    let o = meshd(dir.path(), &["join", "not-a-ticket", "--name", "x"]);
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(!err.contains("panicked"), "{err}");
    assert!(!err.trim().is_empty());
    assert!(err.to_lowercase().contains("ticket"), "{err}");
    // Nothing was created.
    assert!(!dir.path().join("mesh.json").exists());
}

#[test]
fn tags_and_nodes_mutually_exclusive() {
    let dir = TempDir::new().unwrap();
    let site = TempDir::new().unwrap();
    let site = site.path().to_str().unwrap();
    let o = meshd(
        dir.path(),
        &[
            "deploy", "static", "site", site, "--tags", "a", "--nodes", "ab",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let o = meshd(
        dir.path(),
        &["set-placement", "site", "--tags", "a", "--nodes", "ab"],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    // An unknown command is a usage error too.
    assert_eq!(meshd(dir.path(), &["frobnicate"]).status.code(), Some(2));
}

#[test]
fn json_output_parses() {
    let dir = offline_dir();
    let _daemon = start_daemon(dir.path());
    for cmd in [&["members"][..], &["services"], &["status"]] {
        let mut args = cmd.to_vec();
        args.push("--json");
        let o = meshd(dir.path(), &args);
        assert_eq!(o.status.code(), Some(0), "{cmd:?}: {}", stderr(&o));
        serde_json::from_str::<serde_json::Value>(&stdout(&o)).expect("json");
    }
    let o = meshd(dir.path(), &["members", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    // The plain table lists the member too.
    let o = meshd(dir.path(), &["members"]);
    assert!(stdout(&o).contains("pc"), "{}", stdout(&o));
    let o = meshd(dir.path(), &["config", "get", "name"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(stdout(&o).contains("pc"));
    let o = meshd(dir.path(), &["invite", "--ttl", "10m"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("mesh1"), "{}", stdout(&o));
    assert!(stdout(&o).contains('\u{2588}') || stdout(&o).contains('\u{2580}'));
}

#[test]
fn remove_unknown_prefix_error() {
    let dir = offline_dir();
    let _daemon = start_daemon(dir.path());
    let o = meshd(dir.path(), &["remove", "zzzz"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("no member"), "{}", stderr(&o));
}

#[test]
fn deploy_static_skips_symlinks() {
    let dir = offline_dir();
    let site = TempDir::new().unwrap();
    std::fs::write(site.path().join("index.html"), "<h1>hi</h1>").unwrap();
    std::fs::create_dir(site.path().join("sub")).unwrap();
    std::fs::write(site.path().join("sub").join("a.txt"), "a").unwrap();
    let target = site.path().join("index.html");
    let link = site.path().join("link.html");
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&target, &link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&target, &link);
    if let Err(e) = made {
        eprintln!("SKIPPED: cannot create a symlink here ({e})");
        return;
    }
    let _daemon = start_daemon(dir.path());
    let o = meshd(
        dir.path(),
        &["deploy", "static", "site", site.path().to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(0), "{} {}", stdout(&o), stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("skipping symlink"), "{err}");
    assert!(err.contains("link.html"), "{err}");
    let o = meshd(dir.path(), &["services", "--json"]);
    assert!(stdout(&o).contains("\"site\""), "{}", stdout(&o));
}
