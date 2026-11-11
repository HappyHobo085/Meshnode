//! The Android hooks on real daemons: wake, the power policy's wake locks and Saver's
//! pause, the in-process admin API, and the `[android]` config section.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mesh_admin::{
    AdminRequest, AdminResponse, DaemonState, DeployOpts, InProcessAdmin, LocalStateView,
    StatusInfo,
};
use mesh_core::DynClock;
use mesh_hlc::ManualClock;
use mesh_host::config::{CONFIG_FILE, GatewaySection};
use mesh_host::{
    AndroidConfig, Daemon, HostConfig, HostOptions, Periodic, PowerPolicy, PowerState, TestOptions,
    WakeLockSink, WakeOutcome, WakeReason,
};
use mesh_proto::{ALPN_HTTPFWD, NodeId, Placement, Role};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

/// Limit for any single await.
const T: Duration = Duration::from_secs(60);

/// Records every acquire (`+tag:max`) and release (`-tag`).
#[derive(Default)]
struct Rec(Mutex<Vec<String>>);

impl WakeLockSink for Rec {
    fn acquire(&self, tag: &str, max_ms: u64) {
        self.0.lock().unwrap().push(format!("+{tag}:{max_ms}"));
    }
    fn release(&self, tag: &str) {
        self.0.lock().unwrap().push(format!("-{tag}"));
    }
}

impl Rec {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    /// The events for `tag` only.
    fn of(&self, tag: &str) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|e| e[1..].split(':').next() == Some(tag))
            .collect()
    }
}

/// A loopback port nobody listens on right now.
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
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

/// An Android-like host: no admin socket, driven periodic work, wake locks recorded.
fn phone() -> (HostOptions, Arc<Rec>) {
    let rec = Arc::new(Rec::default());
    let sink: Arc<dyn WakeLockSink> = rec.clone();
    let host = HostOptions {
        admin_socket: false,
        periodic: Periodic::Driven,
        power: Some(sink),
    };
    (host, rec)
}

struct D {
    daemon: Daemon,
    admin: InProcessAdmin,
    dir: TempDir,
}

impl D {
    async fn req(&self, req: AdminRequest) -> AdminResponse {
        timeout(T, self.admin.request(req))
            .await
            .expect("request timed out")
    }

    async fn status(&self) -> StatusInfo {
        match self.req(AdminRequest::Status).await {
            AdminResponse::Status(s) => s,
            other => panic!("status: {other:?}"),
        }
    }

    async fn id(&self) -> NodeId {
        self.status().await.node_id.expect("a node id")
    }

    fn gateway(&self) -> SocketAddr {
        self.daemon.gateway_addr().expect("gateway running")
    }

    async fn stop(self) -> TempDir {
        timeout(Duration::from_secs(30), self.daemon.shutdown())
            .await
            .expect("daemon shutdown");
        self.dir
    }
}

async fn start(cfg: HostConfig, host: HostOptions) -> D {
    start_in(tempfile::tempdir().unwrap(), cfg, host).await
}

async fn start_in(dir: TempDir, cfg: HostConfig, host: HostOptions) -> D {
    let opts = TestOptions {
        host,
        ..TestOptions::fast()
    };
    start_test(dir, cfg, opts).await
}

async fn start_test(dir: TempDir, mut cfg: HostConfig, opts: TestOptions) -> D {
    // `free_port` races with parallel tests: when the gateway cannot bind, try a fresh port.
    let mut attempt = 1;
    let daemon = loop {
        let r = timeout(
            T,
            Daemon::start_with(dir.path().to_path_buf(), cfg.clone(), opts.clone()),
        )
        .await
        .expect("daemon start timed out");
        match r {
            Ok(d) => break d,
            Err(mesh_host::DaemonError::Gateway(e)) if attempt < 3 => {
                eprintln!("gateway bind failed ({e}); retrying on a fresh port");
                attempt += 1;
                cfg.gateway.listen = SocketAddr::from(([127, 0, 0, 1], free_port()));
            }
            Err(e) => panic!("daemon start: {e}"),
        }
    };
    let admin = daemon.in_process_admin();
    D { daemon, admin, dir }
}

/// Polls `cond` until it holds; panics after [`T`].
async fn wait_for<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + T;
    while !cond().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(50)).await;
    }
}

/// A desktop daemon `a` (initialised) and a phone daemon `b` (joined), both online.
async fn pair() -> (D, D, Arc<Rec>) {
    pair_with(None).await
}

/// [`pair`] with `b`'s node on wall clock `wall_b`.
async fn pair_with(wall_b: Option<DynClock>) -> (D, D, Arc<Rec>) {
    let a = start(test_config(), HostOptions::default()).await;
    assert_eq!(
        a.req(AdminRequest::Init { name: "a".into() }).await,
        AdminResponse::Ok
    );
    let (host, rec) = phone();
    let opts = TestOptions {
        host,
        wall: wall_b,
        ..TestOptions::fast()
    };
    let b = start_test(tempfile::tempdir().unwrap(), test_config(), opts).await;
    let ticket = match a
        .req(AdminRequest::Invite {
            role: Role::Host,
            ttl_secs: 900,
        })
        .await
    {
        AdminResponse::Ticket { text } => text,
        other => panic!("invite: {other:?}"),
    };
    let r = b
        .req(AdminRequest::Join {
            ticket,
            name: "b".into(),
        })
        .await;
    assert_eq!(r, AdminResponse::Ok);
    for d in [&a, &b] {
        wait_for("both members online", || async {
            match d.req(AdminRequest::Members).await {
                AdminResponse::Members(m) => m.len() == 2 && m.iter().all(|m| m.online),
                _ => false,
            }
        })
        .await;
    }
    (a, b, rec)
}

/// `a` deploys the counter, placed on `b` only; waits until `b` serves it.
async fn counter_on(a: &D, b: &D) {
    let opts = DeployOpts {
        placement: Placement::Nodes(vec![b.id().await]),
        ..DeployOpts::default()
    };
    let r = a
        .req(AdminRequest::DeployWasm {
            name: "counter".into(),
            wasm: mesh_runtime::test_fixtures::fixture("counter"),
            opts,
        })
        .await;
    assert!(matches!(r, AdminResponse::Manifest(_)), "{r:?}");
    wait_for("counter ready on b", || async {
        match b.req(AdminRequest::Services).await {
            AdminResponse::Services(s) => s
                .iter()
                .any(|s| s.name == "counter" && matches!(s.local, LocalStateView::Ready { .. })),
            _ => false,
        }
    })
    .await;
}

/// `(status, body)` of a GET through a gateway, with this Host header.
async fn http_get(addr: SocketAddr, host: &str, path: &str) -> (u16, String) {
    let mut s = timeout(T, TcpStream::connect(addr)).await.unwrap().unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    timeout(T, s.read_to_end(&mut buf))
        .await
        .expect("http response timed out")
        .unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a response head");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_string())
}

fn balanced(charging: bool, screen_on: bool) -> PowerState {
    PowerState {
        policy: PowerPolicy::Balanced,
        charging,
        screen_on,
    }
}

#[tokio::test]
async fn wake_triggers_catch_up() {
    let (a, b, rec) = pair().await;
    let out = timeout(T, b.daemon.wake(WakeReason::Network))
        .await
        .expect("wake returns within its limit");
    assert_eq!(out.peers, 1, "{out:?}");
    assert_eq!(out.rounds_completed, 1, "{out:?}");
    assert!(!out.timed_out, "{out:?}");
    // Balanced, not charging: the catch-up lock, for the duration of the wake only.
    assert_eq!(rec.of("catchup"), ["+catchup:60000", "-catchup"]);
    assert!(
        rec.of("alarm").is_empty(),
        "no alarm lock with periodic_wake_s = 0"
    );

    // The status summary.
    let s = b.daemon.status();
    assert_eq!(s.state, DaemonState::Running);
    assert_eq!(s.node_id, Some(hex::encode(b.id().await.0)));
    let mesh = b.status().await.mesh_id.unwrap();
    assert_eq!(s.mesh_name, Some(hex::encode(mesh.0)));
    assert_eq!(s.peers_online, 1);
    assert_eq!(s.gateway_ports, [b.gateway().port()]);
    b.daemon.network_changed();
    a.stop().await;
    b.stop().await;
}

/// Spec §5.3: Balanced holds the catch-up lock during the incoming-traffic catch-up too.
#[tokio::test]
async fn incoming_after_silence_takes_catchup_lock() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let clock = ManualClock::new(now);
    let (a, b, rec) = pair_with(Some(DynClock(Arc::new(clock.clone())))).await;
    // Joined and online on the frozen clock: no silence yet, so no catch-up lock.
    assert!(rec.of("catchup").is_empty(), "{:?}", rec.all());
    // `a`'s periodic rounds reach `b` after more than a minute of (wall) silence.
    clock.advance(61_000);
    wait_for("the catch-up lock released", || async {
        rec.of("catchup").len() >= 2
    })
    .await;
    assert_eq!(rec.of("catchup"), ["+catchup:60000", "-catchup"]);
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn wake_when_uninitialised_is_noop() {
    let (host, rec) = phone();
    let d = start(test_config(), host).await;
    let out = d.daemon.wake(WakeReason::Alarm).await;
    assert_eq!(
        out,
        WakeOutcome {
            peers: 0,
            rounds_completed: 0,
            timed_out: false
        }
    );
    d.daemon.network_changed();
    assert!(rec.all().is_empty(), "{:?}", rec.all());
    let s = d.daemon.status();
    assert_eq!(s.state, DaemonState::Uninitialised);
    assert_eq!((s.node_id, s.mesh_name), (None, None));
    assert_eq!((s.services_hosted, s.peers_online), (0, 0));
    // The power state set while no node runs applies to the node started later.
    d.daemon.set_power(PowerState {
        policy: PowerPolicy::Saver,
        charging: false,
        screen_on: false,
    });
    assert_eq!(
        d.req(AdminRequest::Init { name: "p".into() }).await,
        AdminResponse::Ok
    );
    assert_eq!(d.daemon.status().state, DaemonState::Running);
    assert!(rec.all().is_empty(), "Saver takes no lock: {:?}", rec.all());
    d.stop().await;
    assert!(rec.all().is_empty());
}

#[tokio::test]
async fn saver_screen_off_refuses_httpfwd_503() {
    let (a, b, _rec) = pair().await;
    counter_on(&a, &b).await;
    // Only the phone serves the counter.
    assert_eq!(b.daemon.status().services_hosted, 1);
    assert_eq!(a.daemon.status().services_hosted, 0);
    b.daemon.set_power(PowerState {
        policy: PowerPolicy::Saver,
        charging: false,
        screen_on: false,
    });
    let served = b.daemon.httpfwd_served_for_test().await;
    // The paused phone answers `503 Service not yet replicated`, which the desktop's
    // forwarder retries elsewhere; with no other host it gives up with a 502.
    let (code, body) = http_get(a.gateway(), "counter.mesh.localhost", "/").await;
    assert_eq!(
        (code, body.as_str()),
        (502, "No host could serve counter"),
        "the only host is paused"
    );
    assert!(
        b.daemon.httpfwd_served_for_test().await > served,
        "the phone itself answered (with its 503)"
    );
    b.daemon.set_power(PowerState {
        policy: PowerPolicy::Saver,
        charging: false,
        screen_on: true,
    });
    let (code, body) = http_get(a.gateway(), "counter.mesh.localhost", "/").await;
    assert_eq!((code, body.as_str()), (200, "1"));
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn request_lock_acquired_and_released() {
    let (a, b, rec) = pair().await;
    counter_on(&a, &b).await;
    assert!(rec.of("request").is_empty());
    let (code, body) = http_get(a.gateway(), "counter.mesh.localhost", "/").await;
    assert_eq!((code, body.as_str()), (200, "1"));
    wait_for("the request ended on b", || async {
        b.daemon.requests_in_flight() == 0
    })
    .await;
    // Balanced: the request's lock (the longest service timeout + 30 s), kept for 30 s.
    assert_eq!(rec.of("request"), ["+request:330000"]);

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(35)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let got = rec.of("request");
    tokio::time::resume();
    assert_eq!(got, ["+request:330000", "-request"]);
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn always_on_holds_until_policy_change() {
    let (host, rec) = phone();
    let mut cfg = test_config();
    cfg.android.power = PowerPolicy::AlwaysOn;
    let d = start(cfg, host).await;
    // Taken at start, with no node running.
    assert_eq!(rec.all(), ["+always:18446744073709551615"]);
    d.daemon.set_power(PowerState {
        policy: PowerPolicy::AlwaysOn,
        charging: true,
        screen_on: false,
    });
    assert_eq!(rec.all().len(), 1, "still held: {:?}", rec.all());
    d.daemon.set_power(balanced(false, true));
    d.daemon.set_power(balanced(true, true));
    assert_eq!(
        rec.all(),
        [
            "+always:18446744073709551615",
            "-always",
            "+charging:18446744073709551615"
        ]
    );
    // Shutdown lets go of it.
    d.stop().await;
    assert_eq!(rec.all().last().map(String::as_str), Some("-charging"));
}

#[test]
fn android_config_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(CONFIG_FILE);
    let cfg = HostConfig::default();
    cfg.save(dir.path()).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.contains("android"), "defaults are omitted:\n{text}");
    assert_eq!(HostConfig::load_or_default(dir.path()).unwrap(), cfg);

    for (power, name) in [
        (PowerPolicy::AlwaysOn, "always_on"),
        (PowerPolicy::Saver, "saver"),
        (PowerPolicy::Balanced, "balanced"),
    ] {
        let cfg = HostConfig {
            android: AndroidConfig {
                power,
                periodic_wake_s: 20,
            },
            ..HostConfig::default()
        };
        cfg.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("[android]"), "{text}");
        assert!(text.contains(&format!("power = \"{name}\"")), "{text}");
        assert!(text.contains("periodic_wake_s = 20"), "{text}");
        assert_eq!(HostConfig::load_or_default(dir.path()).unwrap(), cfg);
    }

    // Written by hand: a partial section takes the defaults; unknown values are refused.
    std::fs::write(&file, "[android]\npower = \"saver\"\n").unwrap();
    let cfg = HostConfig::load_or_default(dir.path()).unwrap();
    assert_eq!(cfg.android.power, PowerPolicy::Saver);
    assert_eq!(cfg.android.periodic_wake_s, 0);
    std::fs::write(&file, "[android]\npower = \"turbo\"\n").unwrap();
    assert!(HostConfig::load_or_default(dir.path()).is_err());
    std::fs::write(&file, "[android]\nbogus = 1\n").unwrap();
    assert!(HostConfig::load_or_default(dir.path()).is_err());
}

#[tokio::test]
async fn request_lock_released_after_failed_request() {
    let (a, b, rec) = pair().await;
    let conn = a
        .daemon
        .node_handle_for_test()
        .unwrap()
        .connect(b.id().await, ALPN_HTTPFWD)
        .await
        .expect("connect to b");
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    // The start of a frame, never completed.
    send.write_all(&[0, 0, 1, 0, 7]).await.unwrap();
    wait_for("b accepted the stream", || async {
        !rec.of("request").is_empty()
    })
    .await;
    conn.close(0u32.into(), b"gone");
    wait_for("the request failed on b", || async {
        b.daemon.requests_in_flight() == 0
    })
    .await;
    assert_eq!(rec.of("request"), ["+request:330000"]);

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(35)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let got = rec.of("request");
    tokio::time::resume();
    assert_eq!(got, ["+request:330000", "-request"]);
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn network_changed_from_a_foreign_thread() {
    let (host, _rec) = phone();
    let d = start(test_config(), host).await;
    assert_eq!(
        d.req(AdminRequest::Init { name: "p".into() }).await,
        AdminResponse::Ok
    );
    let before = d.daemon.network_changes_for_test().await;
    // As from a JNI thread: no Tokio runtime there.
    std::thread::scope(|s| {
        s.spawn(|| {
            assert!(tokio::runtime::Handle::try_current().is_err());
            d.daemon.network_changed();
        });
    });
    wait_for("iroh saw the network change", || async {
        d.daemon.network_changes_for_test().await > before
    })
    .await;
    d.stop().await;
}

#[tokio::test]
async fn power_policy_persists_across_restart() {
    let (host, _rec) = phone();
    let d = start(test_config(), host.clone()).await;
    d.daemon.set_power(PowerState {
        policy: PowerPolicy::Saver,
        charging: false,
        screen_on: true,
    });
    let file = d.dir.path().join(CONFIG_FILE);
    wait_for("power saved", || async {
        std::fs::read_to_string(&file).is_ok_and(|t| t.contains("power = \"saver\""))
    })
    .await;
    // Charging or screen changes alone do not touch the file.
    let saved = std::fs::read_to_string(&file).unwrap();
    d.daemon.set_power(PowerState {
        policy: PowerPolicy::Saver,
        charging: true,
        screen_on: false,
    });
    let dir = d.stop().await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), saved);

    // As the hosts do: the config comes from meshd.toml.
    let cfg = HostConfig::load_or_default(dir.path()).unwrap();
    assert_eq!(cfg.android.power, PowerPolicy::Saver);
    let d = start_in(dir, cfg, host).await;
    assert_eq!(d.daemon.power_state().policy, PowerPolicy::Saver);
    // Back to the default: the section leaves the file again.
    d.daemon.set_power(balanced(false, true));
    let dir = d.stop().await;
    let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
    assert!(!text.contains("android"), "{text}");
}
