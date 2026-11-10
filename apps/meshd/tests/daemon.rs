//! Real daemons on temporary data directories, driven through their admin endpoints.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use mesh_admin::{
    AdminClient, AdminRequest, AdminResponse, DaemonState, DeployOpts, ErrorCode, FileMeta,
    LocalStateView, StatusInfo,
};
use mesh_proto::{Limits, Placement, Role};
use meshd::config::GatewaySection;
use meshd::{Daemon, MeshdConfig, TestOptions};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

/// Limit for any single await.
const T: Duration = Duration::from_secs(60);

/// A loopback port nobody listens on right now.
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn test_config() -> MeshdConfig {
    MeshdConfig {
        relay: "disabled".into(),
        discovery: false,
        gateway: GatewaySection {
            listen: SocketAddr::from(([127, 0, 0, 1], free_port())),
            ports: BTreeMap::new(),
        },
        ..MeshdConfig::default()
    }
}

struct D {
    daemon: Daemon,
    dir: TempDir,
}

impl D {
    fn data(&self) -> &Path {
        self.dir.path()
    }

    async fn client(&self) -> AdminClient {
        timeout(T, AdminClient::connect(self.daemon.endpoint()))
            .await
            .expect("connect timed out")
            .expect("connect")
    }

    async fn req(&self, req: AdminRequest) -> AdminResponse {
        let mut c = self.client().await;
        timeout(T, c.request(req))
            .await
            .expect("request timed out")
            .expect("request")
    }

    async fn status(&self) -> StatusInfo {
        match self.req(AdminRequest::Status).await {
            AdminResponse::Status(s) => s,
            other => panic!("status: {other:?}"),
        }
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

async fn start_in(dir: TempDir, cfg: MeshdConfig) -> D {
    let daemon = timeout(
        T,
        Daemon::start_with(dir.path().to_path_buf(), cfg, TestOptions::fast()),
    )
    .await
    .expect("daemon start timed out")
    .expect("daemon start");
    D { daemon, dir }
}

async fn uninitialised() -> D {
    start_in(tempfile::tempdir().unwrap(), test_config()).await
}

async fn initialised(name: &str) -> D {
    let d = uninitialised().await;
    let r = d.req(AdminRequest::Init { name: name.into() }).await;
    assert_eq!(r, AdminResponse::Ok);
    d
}

fn assert_error(r: &AdminResponse, want: ErrorCode) {
    match r {
        AdminResponse::Error { code, .. } => assert_eq!(*code, want, "{r:?}"),
        other => panic!("expected {want:?}, got {other:?}"),
    }
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

async fn invite(d: &D) -> String {
    match d
        .req(AdminRequest::Invite {
            role: Role::Host,
            ttl_secs: 900,
        })
        .await
    {
        AdminResponse::Ticket { text } => text,
        other => panic!("invite: {other:?}"),
    }
}

/// A initialised, B joined through its daemon; both see each other online.
async fn pair() -> (D, D) {
    let a = initialised("a").await;
    let b = uninitialised().await;
    let ticket = invite(&a).await;
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
    (a, b)
}

async fn services(d: &D) -> Vec<mesh_admin::ServiceInfoView> {
    match d.req(AdminRequest::Services).await {
        AdminResponse::Services(s) => s,
        other => panic!("services: {other:?}"),
    }
}

async fn wait_ready(d: &D, name: &str) {
    wait_for(&format!("{name} ready"), || async {
        services(d)
            .await
            .iter()
            .any(|s| s.name == name && matches!(s.local, LocalStateView::Ready { .. }))
    })
    .await;
}

fn manifest_of(r: AdminResponse) -> mesh_proto::ServiceManifest {
    match r {
        AdminResponse::Manifest(m) => m,
        other => panic!("expected a manifest, got {other:?}"),
    }
}

/// Every entry of the data directory, by name.
fn entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn uninitialised_accepts_only_init_join() {
    let d = uninitialised().await;
    assert_eq!(d.client().await.daemon_state(), DaemonState::Uninitialised);
    let s = d.status().await;
    assert_eq!(s.state, DaemonState::Uninitialised);
    assert_eq!(s.node_id, None);
    assert_eq!(s.gateway, None);
    assert!(d.daemon.gateway_addr().is_none());
    for req in [
        AdminRequest::Members,
        AdminRequest::Services,
        AdminRequest::Invite {
            role: Role::Host,
            ttl_secs: 60,
        },
        AdminRequest::Undeploy { name: "x".into() },
    ] {
        assert_error(&d.req(req).await, ErrorCode::WrongState);
    }
    // Config works without a mesh.
    assert!(matches!(
        d.req(AdminRequest::ConfigGet { key: None }).await,
        AdminResponse::Config(_)
    ));
    // A bad ticket is an argument error, and leaves the daemon uninitialised.
    let r = d
        .req(AdminRequest::Join {
            ticket: "not a ticket".into(),
            name: "x".into(),
        })
        .await;
    assert_error(&r, ErrorCode::InvalidArgument);
    assert_eq!(d.status().await.state, DaemonState::Uninitialised);
    // Init starts the node in place; a second Init is refused.
    let r = d.req(AdminRequest::Init { name: "pc".into() }).await;
    assert_eq!(r, AdminResponse::Ok);
    assert_eq!(d.status().await.state, DaemonState::Running);
    let r = d.req(AdminRequest::Init { name: "pc".into() }).await;
    assert_error(&r, ErrorCode::WrongState);
    d.stop().await;
}

#[tokio::test]
async fn init_then_status_running() {
    let d = initialised("pc").await;
    let s = d.status().await;
    assert_eq!(s.state, DaemonState::Running);
    assert_eq!(s.name.as_deref(), Some("pc"));
    assert_eq!(s.role, Some(Role::Host));
    assert!(s.node_id.is_some() && s.mesh_id.is_some());
    assert_eq!((s.members_total, s.members_online, s.services), (1, 1, 0));
    assert_eq!(s.gateway, Some(d.gateway().to_string()));
    assert_eq!(s.data_dir, d.data().display().to_string());
    // The gateway serves its status page.
    let (code, body) = http_get(d.gateway(), "127.0.0.1", "/status.json").await;
    assert_eq!(code, 200, "{body}");
    // The name was written to meshd.toml.
    let cfg = MeshdConfig::load_or_default(d.data()).unwrap();
    assert_eq!(cfg.name, "pc");
    // A restart opens the existing mesh directly.
    let node_id = s.node_id;
    let dir = d.stop().await;
    let d = start_in(dir, cfg).await;
    let s = d.status().await;
    assert_eq!((s.state, s.node_id), (DaemonState::Running, node_id));
    d.stop().await;
}

#[tokio::test]
async fn join_through_daemon() {
    let (a, b) = pair().await;
    let sa = a.status().await;
    let sb = b.status().await;
    assert_eq!(sb.state, DaemonState::Running);
    assert_eq!(sa.mesh_id, sb.mesh_id);
    assert_eq!(sb.name.as_deref(), Some("b"));
    assert_eq!(MeshdConfig::load_or_default(b.data()).unwrap().name, "b");
    b.stop().await;
    a.stop().await;
}

#[tokio::test]
async fn every_request_against_real_daemon() {
    let mut cfg = test_config();
    let mapped = free_port();
    cfg.gateway.ports = BTreeMap::from([(mapped, "counter".to_string())]);
    let d = start_in(tempfile::tempdir().unwrap(), cfg).await;
    assert_eq!(
        d.req(AdminRequest::Init { name: "pc".into() }).await,
        AdminResponse::Ok
    );
    let me = d.status().await.node_id.unwrap();
    assert_eq!(
        d.status().await.ports,
        vec![(mapped, "counter".to_string())]
    );

    // Members, invite, set-info, set-tags-of.
    match d.req(AdminRequest::Members).await {
        AdminResponse::Members(m) => {
            assert_eq!(m.len(), 1);
            assert!(m[0].is_self && m[0].id == me && m[0].name == "pc");
        }
        other => panic!("{other:?}"),
    }
    let ticket = invite(&d).await;
    assert!(mesh_proto::InviteTicket::from_text(&ticket).is_ok());
    let r = d
        .req(AdminRequest::SetInfo {
            name: Some("pc2".into()),
            tags: Some(vec!["desktop".into()]),
        })
        .await;
    assert_eq!(r, AdminResponse::Ok);
    // set-tags-of keeps the name it sees in the membership, so let set-info land first.
    wait_for("set-info applied", || async {
        d.status().await.name.as_deref() == Some("pc2")
    })
    .await;
    let r = d
        .req(AdminRequest::SetTagsOf {
            id: me,
            tags: vec!["desktop".into(), "big".into()],
        })
        .await;
    assert_eq!(r, AdminResponse::Ok);
    wait_for("info applied", || async {
        let s = d.status().await;
        s.name.as_deref() == Some("pc2") && s.tags == ["desktop", "big"]
    })
    .await;
    assert_eq!(MeshdConfig::load_or_default(d.data()).unwrap().name, "pc2");

    // Deploy the counter and call it through the gateway, on the main and mapped ports.
    let wasm = mesh_runtime::test_fixtures::fixture("counter");
    let m = manifest_of(
        d.req(AdminRequest::DeployWasm {
            name: "counter".into(),
            wasm,
            opts: DeployOpts::default(),
        })
        .await,
    );
    assert_eq!(m.name, "counter");
    wait_ready(&d, "counter").await;
    let (code, body) = http_get(d.gateway(), "counter.mesh.localhost", "/").await;
    assert_eq!((code, body.as_str()), (200, "1"));
    let mapped_addr = SocketAddr::from(([127, 0, 0, 1], mapped));
    let (code, body) = http_get(mapped_addr, "127.0.0.1", "/").await;
    assert_eq!((code, body.as_str()), (200, "2"));
    // An invalid component is an argument error.
    let r = d
        .req(AdminRequest::DeployWasm {
            name: "bad".into(),
            wasm: b"not wasm".to_vec(),
            opts: DeployOpts::default(),
        })
        .await;
    assert_error(&r, ErrorCode::InvalidArgument);

    // Deploy a static site from uploaded files; the staging directory is cleaned up.
    let src = tempfile::tempdir().unwrap();
    let files: Vec<(FileMeta, PathBuf)> = [("index.html", "hello site"), ("a/b.txt", "deep")]
        .iter()
        .map(|(p, body)| {
            let path = src
                .path()
                .join(p.replace('/', std::path::MAIN_SEPARATOR_STR));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            let meta = FileMeta {
                path: p.to_string(),
                size: body.len() as u64,
            };
            (meta, path)
        })
        .collect();
    let mut c = d.client().await;
    let r = timeout(
        T,
        c.deploy_static("site".into(), DeployOpts::default(), files),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(manifest_of(r).name, "site");
    assert!(entries(&d.data().join("tmp")).is_empty());
    wait_ready(&d, "site").await;
    let (code, body) = http_get(d.gateway(), "site.mesh.localhost", "/a/b.txt").await;
    assert_eq!((code, body.as_str()), (200, "deep"));

    // set-placement, set-outbound, set-limits each write a new manifest version.
    let m = manifest_of(
        d.req(AdminRequest::SetPlacement {
            name: "counter".into(),
            placement: Placement::Nodes(vec![me]),
        })
        .await,
    );
    assert_eq!(m.placement, Placement::Nodes(vec![me]));
    let m = manifest_of(
        d.req(AdminRequest::SetOutbound {
            name: "counter".into(),
            outbound: vec!["example.com:443".into()],
        })
        .await,
    );
    assert_eq!(m.outbound, ["example.com:443"]);
    let limits = Limits {
        memory_mb: 32,
        ..Limits::default()
    };
    let m = manifest_of(
        d.req(AdminRequest::SetLimits {
            name: "counter".into(),
            limits,
        })
        .await,
    );
    assert_eq!((m.limits, m.version), (limits, 4));
    let r = d
        .req(AdminRequest::SetLimits {
            name: "nope".into(),
            limits: Limits::default(),
        })
        .await;
    assert_error(&r, ErrorCode::NotFound);
    let all = services(&d).await;
    assert_eq!(
        all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["counter", "site"]
    );
    assert_eq!(d.status().await.services, 2);

    // Undeploy and purge.
    let r = d
        .req(AdminRequest::Undeploy {
            name: "site".into(),
        })
        .await;
    assert_eq!(r, AdminResponse::Ok);
    let r = d
        .req(AdminRequest::Purge {
            name: "site".into(),
        })
        .await;
    assert_eq!(r, AdminResponse::Ok);
    wait_for("site undeployed", || async {
        services(&d).await.iter().all(|s| s.name != "site")
    })
    .await;

    // Config get and set.
    match d
        .req(AdminRequest::ConfigGet {
            key: Some("name".into()),
        })
        .await
    {
        AdminResponse::Config(kv) => assert_eq!(kv, [("name".to_string(), "pc2".to_string())]),
        other => panic!("{other:?}"),
    }
    match d.req(AdminRequest::ConfigGet { key: None }).await {
        AdminResponse::Config(kv) => {
            let keys: Vec<&str> = kv.iter().map(|(k, _)| k.as_str()).collect();
            for k in ["name", "tags", "relay", "discovery", "gateway.listen"] {
                assert!(keys.contains(&k), "{k} missing from {keys:?}");
            }
        }
        other => panic!("{other:?}"),
    }
    let r = d
        .req(AdminRequest::ConfigSet {
            key: "log_level".into(),
            value: "debug".into(),
        })
        .await;
    assert!(matches!(r, AdminResponse::ConfigSet { .. }), "{r:?}");

    // Logs: the tail of the newest log file, then followed lines.
    let logs = d.data().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("meshd.log.2000-01-01"), "old\n").unwrap();
    let today = logs.join("meshd.log.2999-01-01");
    std::fs::write(&today, "one\ntwo\nthree\n").unwrap();
    let mut c = d.client().await;
    let got: Vec<Vec<String>> = timeout(T, c.logs_tail(2, false).map(|r| r.unwrap()).collect())
        .await
        .unwrap();
    assert_eq!(got.concat(), ["two", "three"]);
    let mut c = d.client().await;
    {
        let mut stream = Box::pin(c.logs_tail(1, true));
        let first = timeout(T, stream.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(first, ["three"]);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&today)
            .unwrap();
        std::io::Write::write_all(&mut f, b"four\n").unwrap();
        drop(f);
        let mut seen = Vec::new();
        while !seen.contains(&"four".to_string()) {
            let lines = timeout(T, stream.next()).await.unwrap().unwrap().unwrap();
            seen.extend(lines);
        }
        assert_eq!(seen, ["four"]);
    }
    drop(c);
    d.stop().await;
}

#[tokio::test]
async fn reset_identity_only_when_removed() {
    let (a, b) = pair().await;
    let b_id = b.status().await.node_id.unwrap();
    // Running: refused.
    let r = b.req(AdminRequest::ResetIdentity { confirm: true }).await;
    assert_error(&r, ErrorCode::WrongState);
    // A removes B; B sees it is removed.
    assert_eq!(
        a.req(AdminRequest::Remove { id: b_id }).await,
        AdminResponse::Ok
    );
    wait_for("b removed", || async {
        b.status().await.state == DaemonState::Removed
    })
    .await;
    let r = b.req(AdminRequest::ResetIdentity { confirm: true }).await;
    assert_eq!(r, AdminResponse::Ok);
    let s = b.status().await;
    assert_eq!((s.state, s.node_id), (DaemonState::Uninitialised, None));
    assert!(b.daemon.gateway_addr().is_none());
    // Only meshd.toml, the logs and the admin socket are left.
    for e in entries(b.data()) {
        assert!(
            ["meshd.toml", "logs", "admin.sock"].contains(&e.as_str()),
            "{e} survived the reset"
        );
    }
    assert!(b.data().join("meshd.toml").exists());
    // B can found a new mesh with a new identity.
    assert_eq!(
        b.req(AdminRequest::Init { name: "b2".into() }).await,
        AdminResponse::Ok
    );
    assert_ne!(b.status().await.node_id, Some(b_id));
    // Removing an unknown member is NotFound; the last host cannot be removed.
    let a_id = a.status().await.node_id.unwrap();
    let r = a
        .req(AdminRequest::Remove {
            id: b.status().await.node_id.unwrap(),
        })
        .await;
    assert_error(&r, ErrorCode::NotFound);
    let r = a.req(AdminRequest::Remove { id: a_id }).await;
    assert_error(&r, ErrorCode::LastHost);
    b.stop().await;
    a.stop().await;
}

#[tokio::test]
async fn config_set_validation() {
    let d = initialised("pc").await;
    let set = |key: &str, value: &str| AdminRequest::ConfigSet {
        key: key.into(),
        value: value.into(),
    };
    for (k, v) in [
        ("gateway.listen", "0.0.0.0:7777"),
        ("gateway.listen", "127.0.0.1:80"),
        ("gateway.listen", "nonsense"),
        ("gateway.ports", "80=sync"),
        ("gateway.ports", "7801=Bad_Name"),
        ("services.max_memory_mb", "10"),
        ("discovery", "maybe"),
        ("relay", "ftp://x"),
        ("no.such.key", "1"),
    ] {
        assert_error(&d.req(set(k, v)).await, ErrorCode::InvalidArgument);
    }
    // `name` applies at once.
    let r = d.req(set("name", "renamed")).await;
    assert_eq!(
        r,
        AdminResponse::ConfigSet {
            restart_required: false
        }
    );
    wait_for("name applied", || async {
        d.status().await.name.as_deref() == Some("renamed")
    })
    .await;
    // `gateway.listen` needs a restart, and is saved.
    let r = d.req(set("gateway.listen", "127.0.0.1:7999")).await;
    assert_eq!(
        r,
        AdminResponse::ConfigSet {
            restart_required: true
        }
    );
    let r = d.req(set("gateway.ports", "7801=sync,7802=counter")).await;
    assert_eq!(
        r,
        AdminResponse::ConfigSet {
            restart_required: true
        }
    );
    let saved = MeshdConfig::load_or_default(d.data()).unwrap();
    assert_eq!(saved.name, "renamed");
    assert_eq!(saved.gateway.listen, "127.0.0.1:7999".parse().unwrap());
    assert_eq!(
        saved.gateway.ports.get(&7802).map(String::as_str),
        Some("counter")
    );
    d.stop().await;
}

#[tokio::test]
async fn shutdown_within_20s() {
    let d = initialised("pc").await;
    let wasm = mesh_runtime::test_fixtures::fixture("counter");
    let r = d
        .req(AdminRequest::DeployWasm {
            name: "counter".into(),
            wasm,
            opts: DeployOpts::default(),
        })
        .await;
    manifest_of(r);
    wait_ready(&d, "counter").await;
    assert_eq!(
        http_get(d.gateway(), "counter.mesh.localhost", "/").await.0,
        200
    );
    // An idle gateway connection and a following log tail stay open across the shutdown.
    let _idle = TcpStream::connect(d.gateway()).await.unwrap();
    let mut follower = d.client().await;
    let follow = tokio::spawn(async move {
        let mut s = Box::pin(follower.logs_tail(10, true));
        while let Some(Ok(_)) = s.next().await {}
    });
    sleep(Duration::from_millis(200)).await;

    let D { daemon, dir } = d;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let run = tokio::spawn(daemon.run_until(async move {
        let _ = rx.await;
    }));
    let started = Instant::now();
    tx.send(()).unwrap();
    timeout(Duration::from_secs(25), run)
        .await
        .expect("run_until did not return")
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    timeout(T, follow)
        .await
        .expect("log follower ended")
        .unwrap();
    // Everything was released: the same data directory starts again, Running.
    let cfg = MeshdConfig::load_or_default(dir.path()).unwrap();
    let d = start_in(dir, cfg).await;
    assert_eq!(d.status().await.state, DaemonState::Running);
    d.stop().await;
}

#[tokio::test]
async fn second_daemon_on_same_data_dir_refused() {
    let d = initialised("pc").await;
    let cfg = MeshdConfig::load_or_default(d.data()).unwrap();
    let r = Daemon::start_with(d.data().to_path_buf(), cfg, TestOptions::fast()).await;
    let err = r.err().expect("second daemon refused").to_string();
    assert_eq!(
        err,
        format!(
            "another meshd is already running for {}",
            d.data().display()
        )
    );
    // The first one is unaffected.
    assert_eq!(d.status().await.state, DaemonState::Running);
    d.stop().await;
}
