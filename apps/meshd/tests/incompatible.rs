//! A data directory written by an incompatible Meshnode version: `meshd run` refuses with
//! its own exit status (so systemd does not restart it) and `meshd status` says why.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn meshd(data: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_meshd"));
    c.env_remove("MESHD_DATA")
        .env_remove("RUST_LOG")
        .arg("--data")
        .arg(data)
        .args(args)
        .stdin(Stdio::null());
    c
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `meshd run` as a child, killed if it is still running after `limit`.
fn run_with_timeout(data: &Path, limit: Duration) -> Output {
    let mut child = meshd(data, &["run"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn meshd run");
    let deadline = Instant::now() + limit;
    while child.try_wait().expect("wait").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let o = child.wait_with_output().expect("collect output");
            panic!(
                "meshd run did not exit within {limit:?}; stderr: {}",
                text(&o.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    child.wait_with_output().expect("collect output")
}

/// An initialised node kept off the network, its store then bumped to a newer schema.
fn incompatible_dir() -> TempDir {
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
    let o = meshd(dir.path(), &["init", "--name", "pc"])
        .output()
        .expect("run meshd init");
    assert!(o.status.success(), "init: {}", text(&o.stderr));
    mesh_store::test_hooks::set_schema(
        &dir.path().join("store.redb"),
        mesh_store::SCHEMA_VERSION + 1,
    )
    .unwrap();
    dir
}

#[test]
fn meshd_run_exits_4_on_incompatible_store() {
    let dir = incompatible_dir();
    let o = run_with_timeout(dir.path(), Duration::from_secs(60));
    let stderr = text(&o.stderr);
    assert_eq!(o.status.code(), Some(4), "stderr: {stderr}");
    assert!(stderr.contains("store schema"), "{stderr}");

    // `meshd status` names the refusal instead of the bare "not running".
    let o = meshd(dir.path(), &["status"])
        .output()
        .expect("run meshd status");
    assert_eq!(o.status.code(), Some(3));
    let stderr = text(&o.stderr);
    assert!(stderr.starts_with("not running: "), "{stderr}");
    assert!(stderr.contains("store schema"), "{stderr}");
}
