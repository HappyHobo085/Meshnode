//! End to end through the real `meshd` binary: two daemons on temporary data directories,
//! driven only by the command line and plain HTTP on loopback.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const T: Duration = Duration::from_secs(90);

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

fn ok(data: &Path, args: &[&str]) -> String {
    let o = meshd(data, args);
    assert!(
        o.status.success(),
        "meshd {args:?}: {} {}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn status(data: &Path) -> serde_json::Value {
    serde_json::from_str(&ok(data, &["status", "--json"])).expect("status json")
}

/// A `meshd run` child, killed on drop (so also when the test panics).
struct Running(Child);

impl Running {
    /// Asks the daemon to stop (Ctrl-C equivalent on Unix; killed on Windows, where a console
    /// child cannot be signalled from here) and waits for it to exit.
    fn stop(&mut self) -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-INT", &self.0.id().to_string()])
                .status();
        }
        #[cfg(not(unix))]
        {
            let _ = self.0.kill();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(s) = self.0.try_wait().expect("wait") {
                return s;
            }
            if Instant::now() > deadline {
                let _ = self.0.kill();
                panic!("daemon did not stop in time");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Node {
    dir: TempDir,
    port: u16,
    daemon: Running,
}

impl Node {
    fn data(&self) -> &Path {
        self.dir.path()
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts an uninitialised daemon (offline, gateway on a free loopback port) and waits for
/// its admin endpoint.
fn start() -> Node {
    // `free_port` races with parallel tests: a daemon that exits during start-up (its port
    // was taken) is started again on a fresh port, up to 3 attempts in all.
    for _ in 0..2 {
        if let Some(node) = try_start() {
            return node;
        }
        eprintln!("daemon exited during start-up; retrying on a fresh port");
    }
    try_start().expect("daemon exited during start-up")
}

fn try_start() -> Option<Node> {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    std::fs::write(
        dir.path().join("meshd.toml"),
        format!(
            "relay = \"disabled\"\ndiscovery = false\n[gateway]\nlisten = \"127.0.0.1:{port}\"\nports = {{}}\n"
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_meshd"))
        .env_remove("MESHD_DATA")
        .arg("--data")
        .arg(dir.path())
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn meshd run");
    let mut daemon = Running(child);
    let deadline = Instant::now() + T;
    loop {
        if meshd(dir.path(), &["status", "--json"]).status.success() {
            break;
        }
        if daemon.0.try_wait().ok().flatten().is_some() {
            return None;
        }
        assert!(Instant::now() < deadline, "daemon did not come up");
        std::thread::sleep(Duration::from_millis(200));
    }
    Some(Node { dir, port, daemon })
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + T;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `(status, body)` of a GET with an explicit Host header, or `None` while the gateway is
/// not answering yet.
fn http_get(port: u16, host: &str, path: &str) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.split(' ').nth(1)?.parse().ok()?;
    Some((status, body.to_string()))
}

#[test]
fn two_daemons_serve_and_remove() {
    let mut a = start();
    let mut b = start();

    ok(a.data(), &["init", "--name", "a"]);
    let invite = ok(a.data(), &["invite"]);
    let ticket = invite
        .lines()
        .next()
        .expect("ticket line")
        .trim()
        .to_string();
    ok(b.data(), &["join", &ticket, "--name", "b"]);
    for n in [&a, &b] {
        wait_for("both members online", || {
            let s = status(n.data());
            s["state"] == "Running" && s["members_online"] == 2
        });
    }

    // Deploy a static site on A (placement: all hosts); fetch it through B's gateway.
    let site = TempDir::new().unwrap();
    std::fs::write(
        site.path().join("index.html"),
        "<h1>hello from the mesh</h1>",
    )
    .unwrap();
    ok(
        a.data(),
        &["deploy", "static", "site", site.path().to_str().unwrap()],
    );
    let host = format!("site.mesh.localhost:{}", b.port);
    let mut got = None;
    wait_for("the site through B's gateway", || {
        got = http_get(b.port, &host, "/");
        matches!(got, Some((200, _)))
    });
    assert_eq!(got.unwrap().1, "<h1>hello from the mesh</h1>");

    // A removes B; B notices.
    let b_id: String = status(b.data())["node_id"]
        .as_array()
        .expect("node_id is a byte array")
        .iter()
        .map(|b| format!("{:02x}", b.as_u64().unwrap()))
        .collect();
    ok(a.data(), &["remove", &b_id[..12]]);
    wait_for("B to see its removal", || {
        status(b.data())["state"] == "Removed"
    });
    assert_ne!(status(a.data())["state"], "Removed");

    // Both daemons stop; afterwards nothing answers on their admin endpoints.
    let sb = b.daemon.stop();
    let sa = a.daemon.stop();
    #[cfg(unix)]
    assert!(sa.success() && sb.success(), "{sa:?} {sb:?}");
    let _ = (sa, sb);
    for n in [&a, &b] {
        assert_eq!(meshd(n.data(), &["status"]).status.code(), Some(3));
    }
}
