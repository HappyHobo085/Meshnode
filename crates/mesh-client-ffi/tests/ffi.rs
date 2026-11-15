//! The UniFFI surface against an in-process host (no JVM): the same calls the Kotlin
//! bindings make, through the FFI types. The client side uses the exported API only (its
//! loopback-only test constructors aside).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mesh_client_ffi::{
    MeshClient, MeshClientError, MeshClientListener, MeshClientOptions, MeshEvent, MeshHeader,
    MeshRequest, MeshRole,
};
use mesh_core::frame::read_frame;
use mesh_core::{
    AcceptError, Connection, DynProtocolHandler, Node, NodeConfig, ProtocolHandler, RelayChoice,
    Timings,
};
use mesh_gateway::{HttpFwdHandler, HttpFwdSlot};
use mesh_proto::{ALPN_HTTPFWD, ForwardRequest, HTTPFWD_MAX_FRAME, Role};
use mesh_services::{DeployOpts, DeployerTimings, LocalStatus, Services, ServicesConfig};
use tempfile::TempDir;
use tokio::time::{Instant, sleep, timeout};

/// Limit for any single wait.
const T: Duration = Duration::from_secs(30);
const SITE: &str = "site";
const PAGE: &str = "<h1>site</h1>";

// ------------------------------------------------------------------------ harness

async fn wait_until<F: FnMut() -> bool>(what: &str, mut cond: F) {
    let deadline = Instant::now() + T;
    while !cond() {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

/// Reads each request and never answers.
#[derive(Debug)]
struct Hang;

impl ProtocolHandler for Hang {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        while let Ok((send, mut recv)) = conn.accept_bi().await {
            let _ = read_frame::<_, ForwardRequest>(&mut recv, HTTPFWD_MAX_FRAME).await;
            tokio::spawn(async move {
                sleep(Duration::from_secs(60)).await;
                drop(send);
            });
        }
        Ok(())
    }
}

/// A founder host running `Services` and serving `httpfwd` (with its services, or `Hang`).
struct Host {
    node: Node,
    svcs: Arc<Services>,
    _slot: HttpFwdSlot,
    _dir: TempDir,
    _site: TempDir,
}

impl Host {
    /// Starts the host with the one-page static site [`SITE`] deployed and ready.
    async fn start(hang: bool) -> Host {
        let dir = tempfile::tempdir().unwrap();
        let (handler, slot) = HttpFwdHandler::new();
        let handler: Arc<dyn DynProtocolHandler> = if hang {
            Arc::new(Hang)
        } else {
            Arc::new(handler)
        };
        let mut cfg = NodeConfig::new(dir.path(), "h0");
        cfg.relay = RelayChoice::Disabled;
        cfg.discovery = false;
        cfg.bind_addr = Some("127.0.0.1:0".parse().unwrap());
        cfg.timings = Timings::fast_for_tests();
        cfg.extra_protocols.push((ALPN_HTTPFWD.to_vec(), handler));
        let node = Node::create_mesh(cfg).await.unwrap();
        let timings = DeployerTimings {
            rescan_every: Duration::from_secs(2),
            retry_min: Duration::from_millis(200),
            retry_max: Duration::from_secs(1),
        };
        let svcs = Arc::new(
            Services::start_with_timings(&node, ServicesConfig::default(), timings)
                .await
                .unwrap(),
        );
        if !hang {
            slot.attach(&svcs);
        }
        let site = tempfile::tempdir().unwrap();
        std::fs::write(site.path().join("index.html"), PAGE).unwrap();
        svcs.deploy_static(SITE, site.path(), DeployOpts::default())
            .await
            .unwrap();
        let s = svcs.clone();
        wait_until("site ready", || {
            s.services()
                .iter()
                .any(|s| s.manifest.name == SITE && matches!(s.local, LocalStatus::Ready { .. }))
        })
        .await;
        Host {
            node,
            svcs,
            _slot: slot,
            _dir: dir,
            _site: site,
        }
    }

    async fn invite(&self) -> String {
        self.node
            .invite(Role::Client, Duration::from_secs(900))
            .await
            .unwrap()
            .to_text()
    }

    async fn shutdown(self) {
        let Host { node, svcs, .. } = self;
        if let Ok(s) = Arc::try_unwrap(svcs) {
            timeout(T, s.shutdown()).await.expect("services shutdown");
        }
        timeout(T, node.shutdown()).await.expect("node shutdown");
    }
}

fn opts() -> MeshClientOptions {
    MeshClientOptions {
        name: "app".into(),
        platform: "test".into(),
        request_timeout_ms: 20_000,
    }
}

fn dir_str(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

async fn join(dir: &Path, ticket: String) -> Arc<MeshClient> {
    timeout(T, MeshClient::join_for_test(dir_str(dir), ticket, opts()))
        .await
        .expect("client join timed out")
        .expect("client join")
}

/// Waits until the client lists [`SITE`] on an online host.
async fn wait_site(c: &MeshClient) {
    wait_until("client sees the site on an online host", || {
        let online: Vec<String> = c
            .members()
            .unwrap()
            .into_iter()
            .filter(|m| m.role == MeshRole::Host && m.online)
            .map(|m| m.id)
            .collect();
        c.services()
            .unwrap()
            .iter()
            .any(|s| s.name == SITE && s.hosts.iter().any(|h| online.contains(h)))
    })
    .await;
}

fn get(path: &str) -> MeshRequest {
    MeshRequest {
        method: "GET".into(),
        path: path.into(),
        headers: vec![MeshHeader {
            name: "accept".into(),
            value: "text/html".into(),
        }],
        body: Vec::new(),
    }
}

/// Collects the events it is given.
#[derive(Default)]
struct Collect(Arc<Mutex<Vec<MeshEvent>>>);

impl MeshClientListener for Collect {
    fn on_event(&self, event: MeshEvent) {
        self.0.lock().unwrap().push(event);
    }
}

// -------------------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ffi_join_invalid_ticket_maps_error() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("client");
    // The real constructors: both fail before any node starts.
    let err = MeshClient::join(dir_str(&dir), "not a ticket".into(), opts())
        .await
        .err()
        .expect("join with a garbage ticket");
    assert_eq!(err, MeshClientError::InvalidTicket);
    assert!(!dir.join("node.key").exists(), "nothing written");
    let err = MeshClient::open(dir_str(root.path()), opts())
        .await
        .err()
        .expect("open on an empty dir");
    assert_eq!(err, MeshClientError::NotJoined);
    // A missing dir is not created.
    let err = MeshClient::open(dir_str(&dir), opts()).await.err().unwrap();
    assert_eq!(err, MeshClientError::NotJoined);
    assert!(!dir.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ffi_request_roundtrip() {
    let h = Host::start(false).await;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("client");
    let c = join(&dir, h.invite().await).await;
    wait_site(&c).await;

    let resp = timeout(T, c.clone().request(SITE.into(), get("/")))
        .await
        .expect("request timed out")
        .expect("request");
    assert_eq!(resp.status, 200);
    assert_eq!(String::from_utf8_lossy(&resp.body), PAGE);
    assert!(
        resp.headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("content-type") && h.value.contains("text/html")),
        "{:?}",
        resp.headers
    );
    // A request that cannot be built is answered locally.
    let mut bad = get("/");
    bad.method = "BAD METHOD".into();
    let resp = c.clone().request(SITE.into(), bad).await.unwrap();
    assert_eq!(resp.status, 400);
    let resp = c
        .clone()
        .request(SITE.into(), get("no-slash"))
        .await
        .unwrap();
    assert_eq!(resp.status, 400);
    // A POST body goes through (a static site refuses it, which is still a response).
    let post = MeshRequest {
        method: "POST".into(),
        body: b"hello".to_vec(),
        ..get("/")
    };
    let resp = c.clone().request(SITE.into(), post).await.unwrap();
    assert!(resp.status >= 400, "{}", resp.status);

    let st = c.status().unwrap();
    assert_eq!(st.mesh.len(), 64);
    assert_eq!(st.id.len(), 64);
    assert!(st.online_hosts >= 1);
    assert!(!st.removed);
    let me = c
        .members()
        .unwrap()
        .into_iter()
        .find(|m| m.id == st.id)
        .expect("self listed");
    assert_eq!(me.role, MeshRole::Client);
    let host = hex(h.node.node_id().0);
    let svc = c.services().unwrap();
    assert!(
        svc.iter()
            .any(|s| s.name == SITE && s.kind == "static" && s.hosts == vec![host.clone()]),
        "{svc:?}"
    );
    let ticket = c.clone().invite(MeshRole::Client, 600).await.unwrap();
    assert!(!ticket.is_empty());
    let err = c.clone().remove("zz".into()).await.unwrap_err();
    assert!(
        matches!(err, MeshClientError::InvalidArgument { .. }),
        "{err:?}"
    );
    // The service is unknown: no host.
    let err = c
        .clone()
        .request("nope".into(), get("/"))
        .await
        .unwrap_err();
    assert_eq!(err, MeshClientError::NoHost);

    // Shutdown releases the directory at once and closes the object.
    c.clone().shutdown().await;
    assert_eq!(c.status().unwrap_err(), MeshClientError::Closed);
    assert_eq!(c.members().unwrap_err(), MeshClientError::Closed);
    assert_eq!(
        c.clone().request(SITE.into(), get("/")).await.unwrap_err(),
        MeshClientError::Closed
    );
    c.clone().shutdown().await; // twice is fine
    let c2 = timeout(T, MeshClient::open_for_test(dir_str(&dir), opts()))
        .await
        .unwrap()
        .expect("reopen after shutdown");
    assert_eq!(c2.status().unwrap().id, st.id);
    c2.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ffi_listener_receives_events() {
    let h = Host::start(false).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(&root.path().join("client"), h.invite().await).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    c.set_listener(Box::new(Collect(seen.clone()))).unwrap();
    wait_site(&c).await;
    // A second deploy is a manifest change the client is told about.
    let site = tempfile::tempdir().unwrap();
    std::fs::write(site.path().join("index.html"), "two").unwrap();
    seen.lock().unwrap().clear();
    h.svcs
        .deploy_static("two", site.path(), DeployOpts::default())
        .await
        .unwrap();
    wait_until("ServicesChanged", || {
        seen.lock().unwrap().contains(&MeshEvent::ServicesChanged)
    })
    .await;
    wait_until("client lists the second site", || {
        c.services().unwrap().iter().any(|s| s.name == "two")
    })
    .await;
    // The host going away: Offline.
    h.shutdown().await;
    wait_until("Offline", || {
        seen.lock().unwrap().contains(&MeshEvent::Offline)
    })
    .await;
    // A replaced listener gets nothing more; shutdown ends delivery.
    let second = Arc::new(Mutex::new(Vec::new()));
    c.set_listener(Box::new(Collect(second.clone()))).unwrap();
    c.clone().shutdown().await;
    assert_eq!(
        c.set_listener(Box::new(Collect::default())).unwrap_err(),
        MeshClientError::Closed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ffi_shutdown_ends_a_request_in_flight() {
    let h = Host::start(true).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(&root.path().join("client"), h.invite().await).await;
    wait_site(&c).await;
    let req = tokio::spawn(c.clone().request(SITE.into(), get("/")));
    sleep(Duration::from_millis(500)).await;
    assert!(!req.is_finished(), "the hanging host answered");
    let started = Instant::now();
    timeout(Duration::from_secs(10), c.clone().shutdown())
        .await
        .expect("shutdown waited for the request");
    assert!(started.elapsed() < Duration::from_secs(10));
    let r = timeout(T, req).await.unwrap().unwrap();
    assert_eq!(r.unwrap_err(), MeshClientError::Closed);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ffi_drop_without_shutdown_releases_the_dir() {
    // Kotlin never called shutdown: the object is collected on a finalizer thread (no
    // runtime). The SDK tears the node down on its own runtime (ruling R16), and the
    // directory can be opened again afterwards.
    let h = Host::start(false).await;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("client");
    let c = join(&dir, h.invite().await).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    c.set_listener(Box::new(Collect(seen))).unwrap();
    wait_until("first sync", || c.status().unwrap().first_sync_done).await;
    std::thread::spawn(move || drop(c)).join().unwrap();
    let deadline = Instant::now() + T;
    let c = loop {
        match MeshClient::open_for_test(dir_str(&dir), opts()).await {
            Ok(c) => break c,
            Err(MeshClientError::AlreadyOpen) => {}
            Err(e) => panic!("reopen after drop: {e:?}"),
        }
        assert!(Instant::now() < deadline, "never reopened");
        sleep(Duration::from_millis(20)).await;
    };
    c.shutdown().await;
    h.shutdown().await;
}

fn hex(b: [u8; 32]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
