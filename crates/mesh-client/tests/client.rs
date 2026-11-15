//! `mesh-client` against in-process hosts (sub-spec 8a §6.3). The hosts are mesh-core nodes
//! running `Services` and serving `httpfwd` with the gateway's handler; the client side
//! uses only the SDK (its loopback-only test constructors aside).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mesh_client::{ClientError, ClientEvent, ClientOptions, MeshClient, Role};
use mesh_core::frame::read_frame;
use mesh_core::{
    AcceptError, Connection, DynProtocolHandler, Node, NodeConfig, ProtocolHandler, RelayChoice,
    Timings,
};
use mesh_gateway::{HttpFwdHandler, HttpFwdSlot};
use mesh_proto::{ALPN_HTTPFWD, ForwardRequest, HTTPFWD_MAX_FRAME, Limits, NodeId, Placement};
use mesh_runtime::test_fixtures::fixture;
use mesh_runtime::{CodeCache, Runtime, RuntimeConfig};
use mesh_services::{DeployOpts, DeployerTimings, LocalStatus, Services, ServicesConfig};
use tempfile::TempDir;
use tokio::sync::{OnceCell, broadcast};
use tokio::time::{Instant, sleep, timeout};

/// Limit for any single wait.
const T: Duration = Duration::from_secs(30);
const SITE: &str = "site";
const PAGE: &str = "<h1>site</h1>";
const COUNTER: &str = "counter";

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

/// A code cache shared by every test in this process, filled once with `counter`.
async fn warm_cache() -> CodeCache {
    static WARM: OnceCell<()> = OnceCell::const_new();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mesh-client-code-cache");
    WARM.get_or_init(|| async {
        let rt = Runtime::new(RuntimeConfig {
            cache: CodeCache::Read(dir.clone()),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let wasm = fixture(COUNTER);
        rt.load(*blake3::hash(&wasm).as_bytes(), &wasm)
            .await
            .unwrap();
    })
    .await;
    CodeCache::Read(dir)
}

/// How a host serves `httpfwd`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Serve {
    /// With its services.
    Attached,
    /// Answers the retryable `503 Service not yet replicated`.
    Unattached,
    /// Reads each request and never answers.
    Hang,
}

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

fn host_cfg(dir: &Path, name: &str, handler: Arc<dyn DynProtocolHandler>) -> NodeConfig {
    let mut cfg = NodeConfig::new(dir, name);
    cfg.relay = RelayChoice::Disabled;
    cfg.discovery = false;
    cfg.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    cfg.timings = Timings::fast_for_tests();
    cfg.extra_protocols.push((ALPN_HTTPFWD.to_vec(), handler));
    cfg
}

struct Running {
    node: Node,
    svcs: Arc<Services>,
    slot: HttpFwdSlot,
}

/// An in-process host that can be stopped and started again on its data directory.
struct Host {
    name: String,
    serve: Serve,
    dir: TempDir,
    run: Option<Running>,
}

impl Host {
    async fn start(name: &str, serve: Serve, ticket: Option<mesh_proto::InviteTicket>) -> Host {
        let dir = tempfile::tempdir().unwrap();
        let (handler, slot) = Self::handler(serve);
        let cfg = host_cfg(dir.path(), name, handler);
        let node = match ticket {
            None => Node::create_mesh(cfg).await.unwrap(),
            Some(t) => {
                let node = timeout(T, Node::join(cfg, t))
                    .await
                    .expect("join timed out")
                    .unwrap();
                assert!(node.wait_first_sync(T).await);
                node
            }
        };
        let mut h = Host {
            name: name.into(),
            serve,
            dir,
            run: None,
        };
        h.run = Some(h.services(node, slot).await);
        h
    }

    async fn services(&self, node: Node, slot: HttpFwdSlot) -> Running {
        let cfg = ServicesConfig {
            cache: Some(warm_cache().await),
            ..ServicesConfig::default()
        };
        let timings = DeployerTimings {
            rescan_every: Duration::from_secs(2),
            retry_min: Duration::from_millis(200),
            retry_max: Duration::from_secs(1),
        };
        let svcs = Arc::new(
            Services::start_with_timings(&node, cfg, timings)
                .await
                .unwrap(),
        );
        if self.serve == Serve::Attached {
            slot.attach(&svcs);
        }
        Running { node, svcs, slot }
    }

    fn handler(serve: Serve) -> (Arc<dyn DynProtocolHandler>, HttpFwdSlot) {
        let (handler, slot) = HttpFwdHandler::new();
        if serve == Serve::Hang {
            (Arc::new(Hang), slot)
        } else {
            (Arc::new(handler), slot)
        }
    }

    fn run(&self) -> &Running {
        self.run.as_ref().expect("host running")
    }

    fn node(&self) -> &Node {
        &self.run().node
    }

    fn id(&self) -> NodeId {
        self.node().node_id()
    }

    fn svcs(&self) -> &Services {
        &self.run().svcs
    }

    fn served(&self) -> u64 {
        self.run().slot.served()
    }

    async fn invite(&self, role: Role) -> String {
        self.node()
            .invite(role, Duration::from_secs(900))
            .await
            .unwrap()
            .to_text()
    }

    async fn stop(&mut self) {
        let Running { node, svcs, slot } = self.run.take().expect("host running");
        drop(slot);
        if let Ok(s) = Arc::try_unwrap(svcs) {
            timeout(T, s.shutdown()).await.expect("services shutdown");
        }
        timeout(T, node.shutdown()).await.expect("node shutdown");
    }

    async fn restart(&mut self) {
        let (handler, slot) = Self::handler(self.serve);
        let cfg = host_cfg(self.dir.path(), &self.name.clone(), handler);
        let node = timeout(T, Node::open(cfg))
            .await
            .expect("open timed out")
            .unwrap();
        self.run = Some(self.services(node, slot).await);
    }

    async fn shutdown(mut self) {
        if self.run.is_some() {
            self.stop().await;
        }
    }
}

/// A founder host plus one host per further entry; every host sees every other online
/// before this returns.
async fn hosts(serve: &[Serve]) -> Vec<Host> {
    let mut out: Vec<Host> = Vec::new();
    for (i, a) in serve.iter().enumerate() {
        let ticket = match out.first() {
            None => None,
            Some(f) => Some(f.node().invite(Role::Host, T * 30).await.unwrap()),
        };
        out.push(Host::start(&format!("h{i}"), *a, ticket).await);
    }
    let ids: Vec<NodeId> = out.iter().map(Host::id).collect();
    wait_until("every host sees every other online", || {
        out.iter().all(|h| {
            let m = h.node().members();
            ids.iter()
                .all(|id| m.iter().any(|s| s.id == *id && s.online && s.effective))
        })
    })
    .await;
    out
}

async fn wait_ready(h: &Host, name: &str) {
    let svcs = h.svcs();
    wait_until(&format!("{name} ready on {}", h.name), || {
        svcs.services()
            .iter()
            .any(|s| s.manifest.name == name && matches!(s.local, LocalStatus::Ready { .. }))
    })
    .await;
}

/// Deploys a one-page static site on every host from `h`; keep the returned dir alive.
async fn deploy_site(h: &Host) -> TempDir {
    let site = tempfile::tempdir().unwrap();
    std::fs::write(site.path().join("index.html"), PAGE).unwrap();
    h.svcs()
        .deploy_static(SITE, site.path(), DeployOpts::default())
        .await
        .unwrap();
    site
}

async fn deploy_counter(h: &Host, placement: Placement) {
    h.svcs()
        .deploy_wasm(
            COUNTER,
            Bytes::from(fixture(COUNTER)),
            DeployOpts {
                placement,
                ..DeployOpts::default()
            },
        )
        .await
        .unwrap();
}

fn opts() -> ClientOptions {
    ClientOptions {
        name: "app".into(),
        platform: "test".into(),
        request_timeout: Duration::from_secs(20),
    }
}

async fn join(dir: &Path, ticket: &str, opts: ClientOptions) -> MeshClient {
    timeout(T, MeshClient::join_for_test(dir, ticket, opts))
        .await
        .expect("client join timed out")
        .expect("client join")
}

async fn open(dir: &Path, opts: ClientOptions) -> MeshClient {
    timeout(T, MeshClient::open_for_test(dir, opts))
        .await
        .expect("client open timed out")
        .expect("client open")
}

/// A new client data dir (not yet created: `join` creates it).
fn client_dir(root: &TempDir) -> PathBuf {
    root.path().join("client")
}

/// Waits until `c` lists `service` placed on `n` hosts, all of them online.
async fn wait_service(c: &MeshClient, service: &str, n: usize) {
    wait_until(
        &format!("client sees {service} on {n} online hosts"),
        || {
            let online: Vec<String> = c
                .members()
                .into_iter()
                .filter(|m| m.role == Role::Host && m.online)
                .map(|m| hex(m.id))
                .collect();
            c.services().iter().any(|s| {
                s.name == service
                    && s.hosts.len() == n
                    && s.hosts.iter().all(|h| online.contains(h))
            })
        },
    )
    .await;
}

fn hex(id: NodeId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn get(path: &str) -> http::Request<Bytes> {
    http::Request::get(path).body(Bytes::new()).unwrap()
}

async fn fetch(c: &MeshClient, service: &str, path: &str) -> (u16, String) {
    let resp = timeout(T, c.request(service, get(path)))
        .await
        .expect("request timed out")
        .expect("request");
    (
        resp.status().as_u16(),
        String::from_utf8_lossy(resp.body()).into_owned(),
    )
}

/// Waits for an event matching `want` on `rx`.
async fn wait_event(rx: &mut broadcast::Receiver<ClientEvent>, want: ClientEvent) {
    timeout(T, async {
        loop {
            match rx.recv().await {
                Ok(e) if e == want => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("events closed"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {want:?} event"));
}

// -------------------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_and_request_static_site() {
    let hs = hosts(&[Serve::Attached]).await;
    let _site = deploy_site(&hs[0]).await;
    wait_ready(&hs[0], SITE).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(
        &client_dir(&root),
        &hs[0].invite(Role::Client).await,
        opts(),
    )
    .await;
    wait_service(&c, SITE, 1).await;

    assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
    // The host's own error answers come back as they are.
    assert_eq!(fetch(&c, SITE, "/missing").await.0, 404);

    let st = c.status();
    assert!(st.first_sync_done);
    assert!(!st.removed);
    assert_eq!(st.online_hosts, 1);
    assert_eq!(st.mesh.len(), 64);
    assert_eq!(st.id.len(), 64);
    // Its own record and info (written after the first round) replicate back in time.
    wait_until("client sees its own info", || {
        c.members()
            .iter()
            .any(|m| hex(m.id) == st.id && m.name == "app")
    })
    .await;
    let members = c.members();
    let me = members.iter().find(|m| hex(m.id) == st.id).expect("self");
    assert_eq!(me.role, Role::Client);
    assert_eq!(me.name, "app");
    assert_eq!(me.platform, "test");
    let host = members.iter().find(|m| m.id == hs[0].id()).expect("host");
    assert_eq!(
        (host.role, host.online, host.name.as_str()),
        (Role::Host, true, "h0")
    );
    let svc = c.services();
    assert_eq!(svc.len(), 1);
    assert_eq!(svc[0].kind, "static");
    assert_eq!(svc[0].hosts, vec![hex(hs[0].id())]);
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_and_request_wasm_fixture() {
    let hs = hosts(&[Serve::Attached]).await;
    deploy_counter(&hs[0], Placement::AllHosts).await;
    wait_ready(&hs[0], COUNTER).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(
        &client_dir(&root),
        &hs[0].invite(Role::Client).await,
        opts(),
    )
    .await;
    wait_service(&c, COUNTER, 1).await;
    assert_eq!(fetch(&c, COUNTER, "/").await, (200, "1".into()));
    assert_eq!(fetch(&c, COUNTER, "/").await, (200, "2".into()));
    assert_eq!(c.services()[0].kind, "wasm");
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_between_two_hosts() {
    // A answers the retryable `503 Service not yet replicated`; B serves.
    let hs = hosts(&[Serve::Unattached, Serve::Attached]).await;
    let (a, b) = (hs[0].id(), hs[1].id());
    deploy_counter(&hs[1], Placement::Nodes(vec![a, b])).await;
    wait_ready(&hs[1], COUNTER).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(
        &client_dir(&root),
        &hs[0].invite(Role::Client).await,
        opts(),
    )
    .await;
    wait_service(&c, COUNTER, 2).await;
    c.try_first(vec![a, b]);
    assert_eq!(fetch(&c, COUNTER, "/").await, (200, "1".into()));
    assert_eq!(c.attempts(), 2);
    assert_eq!(hs[0].served(), 1, "A was tried first");
    assert_eq!(hs[1].served(), 1, "B served it");
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_fails_over_when_host_restarts() {
    let mut hs = hosts(&[Serve::Attached, Serve::Attached]).await;
    let (a, b) = (hs[0].id(), hs[1].id());
    let _site = deploy_site(&hs[0]).await;
    wait_ready(&hs[0], SITE).await;
    wait_ready(&hs[1], SITE).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(
        &client_dir(&root),
        &hs[0].invite(Role::Client).await,
        opts(),
    )
    .await;
    wait_service(&c, SITE, 2).await;
    c.try_first(vec![a, b]);
    assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
    assert_eq!(hs[0].served(), 1);

    // A goes down: B serves, whether or not the client has noticed yet.
    hs[0].stop().await;
    let before = hs[1].served();
    assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
    assert!(hs[1].served() > before, "B served while A was down");

    // A comes back (answering `503` until its site is ready): every request is served,
    // and A serves again once it can.
    hs[0].restart().await;
    let deadline = Instant::now() + T;
    loop {
        assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
        if hs[0].served() > 0 && c.members().iter().any(|m| m.id == a && m.online) {
            let served = hs[0].served();
            assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
            if hs[0].served() > served {
                break;
            }
        }
        assert!(Instant::now() < deadline, "A never served again");
        sleep(Duration::from_millis(100)).await;
    }
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_host_when_none_reachable() {
    let mut hs = hosts(&[Serve::Attached]).await;
    let _site = deploy_site(&hs[0]).await;
    wait_ready(&hs[0], SITE).await;
    let root = tempfile::tempdir().unwrap();
    let limit = Duration::from_secs(5);
    let c = join(
        &client_dir(&root),
        &hs[0].invite(Role::Client).await,
        ClientOptions {
            request_timeout: limit,
            ..opts()
        },
    )
    .await;
    wait_service(&c, SITE, 1).await;
    assert_eq!(fetch(&c, SITE, "/").await.0, 200);
    let mut rx = c.subscribe();

    // An undeployed service has no host.
    let r = c.request("nope", get("/")).await;
    assert!(matches!(r, Err(ClientError::NoHost)), "{r:?}");

    hs[0].stop().await;
    wait_event(&mut rx, ClientEvent::Offline).await;
    assert_eq!(c.status().online_hosts, 0);
    let start = Instant::now();
    let r = c.request(SITE, get("/")).await;
    assert!(matches!(r, Err(ClientError::NoHost)), "{r:?}");
    assert!(start.elapsed() < limit, "took {:?}", start.elapsed());
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion5_client_via_remaining_host() {
    let mut hs = hosts(&[Serve::Attached, Serve::Attached]).await;
    let _site = deploy_site(&hs[0]).await;
    wait_ready(&hs[0], SITE).await;
    wait_ready(&hs[1], SITE).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(
        &client_dir(&root),
        &hs[0].invite(Role::Client).await,
        opts(),
    )
    .await;
    wait_service(&c, SITE, 2).await;
    assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));

    // A (the founder and the client's inviter) shuts down: B serves everything.
    hs[0].stop().await;
    let before = hs[1].served();
    for _ in 0..5 {
        assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
    }
    assert_eq!(hs[1].served(), before + 5);
    wait_until("client sees one online host", || {
        c.status().online_hosts == 1
    })
    .await;
    assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_client_gets_removed_event_and_errors() {
    let hs = hosts(&[Serve::Attached]).await;
    let _site = deploy_site(&hs[0]).await;
    let root = tempfile::tempdir().unwrap();
    let c = join(
        &client_dir(&root),
        &hs[0].invite(Role::Client).await,
        opts(),
    )
    .await;
    wait_service(&c, SITE, 1).await;
    let mut rx = c.subscribe();
    let me = c.status().id;
    // The client's own record may reach its replica after the manifest: ask the host.
    wait_until("host sees the client effective", || {
        hs[0]
            .node()
            .members()
            .iter()
            .any(|m| hex(m.id) == me && m.effective)
    })
    .await;
    let id = hs[0]
        .node()
        .members()
        .into_iter()
        .find(|m| hex(m.id) == me)
        .expect("client member")
        .id;
    hs[0].node().remove(id).await.unwrap();

    wait_event(&mut rx, ClientEvent::Removed).await;
    assert!(c.status().removed);
    let r = c.request(SITE, get("/")).await;
    assert!(matches!(r, Err(ClientError::Removed)), "{r:?}");
    let r = c.invite(Role::Client, Duration::from_secs(60)).await;
    assert!(matches!(r, Err(ClientError::Removed)), "{r:?}");
    let r = c.remove(hs[0].id()).await;
    assert!(matches!(r, Err(ClientError::Removed)), "{r:?}");
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_reuses_identity() {
    let hs = hosts(&[Serve::Attached]).await;
    let _site = deploy_site(&hs[0]).await;
    let root = tempfile::tempdir().unwrap();
    let dir = client_dir(&root);
    let c = join(&dir, &hs[0].invite(Role::Client).await, opts()).await;
    wait_service(&c, SITE, 1).await;
    let (id, mesh) = (c.status().id, c.status().mesh);
    c.shutdown().await;

    let c = open(&dir, opts()).await;
    assert_eq!((c.status().id, c.status().mesh), (id, mesh));
    wait_until("first sync after open", || c.status().first_sync_done).await;
    wait_service(&c, SITE, 1).await;
    assert_eq!(fetch(&c, SITE, "/").await, (200, PAGE.into()));
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_open_same_dir_refused() {
    let hs = hosts(&[Serve::Attached]).await;
    let root = tempfile::tempdir().unwrap();
    let dir = client_dir(&root);
    let c = join(&dir, &hs[0].invite(Role::Client).await, opts()).await;

    let r = MeshClient::open_for_test(&dir, opts()).await;
    assert!(matches!(r, Err(ClientError::AlreadyOpen)), "{:?}", r.err());
    let r = MeshClient::open(&dir, opts()).await;
    assert!(matches!(r, Err(ClientError::AlreadyOpen)), "{:?}", r.err());
    let ticket = hs[0].invite(Role::Client).await;
    let r = MeshClient::join_for_test(&dir, &ticket, opts()).await;
    assert!(matches!(r, Err(ClientError::AlreadyOpen)), "{:?}", r.err());

    // Once the first is shut down, the dir opens again.
    c.shutdown().await;
    let c = open(&dir, opts()).await;
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_ticket_refused() {
    let hs = hosts(&[Serve::Attached]).await;
    let root = tempfile::tempdir().unwrap();
    let dir = client_dir(&root);
    let ticket = hs[0].invite(Role::Host).await;
    for t in [ticket.as_str(), "garbage", ""] {
        let r = MeshClient::join(&dir, t, opts()).await;
        assert!(
            matches!(r, Err(ClientError::InvalidTicket)),
            "{:?}",
            r.err()
        );
    }
    assert!(!dir.join("node.key").exists(), "no identity was created");
    // The directory is still good for a client ticket.
    let c = join(&dir, &hs[0].invite(Role::Client).await, opts()).await;
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_into_used_dir_refused() {
    let hs = hosts(&[Serve::Attached]).await;
    let root = tempfile::tempdir().unwrap();
    let dir = client_dir(&root);
    let c = join(&dir, &hs[0].invite(Role::Client).await, opts()).await;
    c.shutdown().await;
    let ticket = hs[0].invite(Role::Client).await;
    let r = MeshClient::join_for_test(&dir, &ticket, opts()).await;
    assert!(
        matches!(r, Err(ClientError::AlreadyJoined)),
        "{:?}",
        r.err()
    );

    // `open` needs an identity.
    let empty = root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    for d in [empty, root.path().join("missing")] {
        let r = MeshClient::open(&d, opts()).await;
        assert!(matches!(r, Err(ClientError::NotJoined)), "{:?}", r.err());
    }
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_from_client_works() {
    let hs = hosts(&[Serve::Attached]).await;
    let _site = deploy_site(&hs[0]).await;
    wait_ready(&hs[0], SITE).await;
    let root = tempfile::tempdir().unwrap();
    let c1 = join(
        &root.path().join("c1"),
        &hs[0].invite(Role::Client).await,
        opts(),
    )
    .await;
    wait_service(&c1, SITE, 1).await;

    let ticket = c1
        .invite(Role::Client, Duration::from_secs(900))
        .await
        .unwrap();
    let c2 = join(
        &root.path().join("c2"),
        &ticket,
        ClientOptions {
            name: "app2".into(),
            ..opts()
        },
    )
    .await;
    wait_service(&c2, SITE, 1).await;
    assert_eq!(fetch(&c2, SITE, "/").await, (200, PAGE.into()));
    let id2 = c2.status().id;
    wait_until("the host and the inviter see the new client", || {
        hs[0].node().members().iter().any(|m| hex(m.id) == id2)
            && c1
                .members()
                .iter()
                .any(|m| hex(m.id) == id2 && m.name == "app2" && m.role == Role::Client)
    })
    .await;
    c2.shutdown().await;
    c1.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn services_follow_deploy_and_remove_after_offline() {
    let hs = hosts(&[Serve::Attached]).await;
    let root = tempfile::tempdir().unwrap();
    let dir = client_dir(&root);
    let c = join(&dir, &hs[0].invite(Role::Client).await, opts()).await;
    wait_until("client online", || c.status().online_hosts == 1).await;
    let mut rx = c.subscribe();
    assert!(c.services().is_empty());

    let _site = deploy_site(&hs[0]).await;
    wait_event(&mut rx, ClientEvent::ServicesChanged).await;
    wait_service(&c, SITE, 1).await;
    c.shutdown().await;

    // Undeployed while the client was off: gone once it has caught up.
    hs[0].svcs().undeploy(SITE).await.unwrap();
    let c = open(&dir, opts()).await;
    wait_until("client sees the undeploy", || {
        c.status().first_sync_done && c.services().is_empty()
    })
    .await;
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_body_gets_local_413() {
    let hs = hosts(&[Serve::Attached]).await;
    let _site = deploy_site(&hs[0]).await;
    wait_ready(&hs[0], SITE).await;
    let root = tempfile::tempdir().unwrap();
    let dir = client_dir(&root);
    let c = join(&dir, &hs[0].invite(Role::Client).await, opts()).await;
    wait_service(&c, SITE, 1).await;

    let big = http::Request::post("/")
        .body(Bytes::from(vec![0u8; 16 * 1024 * 1024 + 1]))
        .unwrap();
    let resp = c.request(SITE, big).await.unwrap();
    assert_eq!(resp.status(), 413);
    // Not a path: a local 400.
    let star = http::Request::options("*").body(Bytes::new()).unwrap();
    let resp = c.request(SITE, star).await.unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(c.attempts(), 0, "nothing was forwarded");
    // 16 MiB exactly is forwarded; the host's answer comes back as it is.
    let max = http::Request::post("/")
        .body(Bytes::from(vec![0u8; 16 * 1024 * 1024]))
        .unwrap();
    let resp = timeout(T, c.request(SITE, max)).await.unwrap().unwrap();
    assert_ne!(resp.status(), 200);
    assert_eq!(c.attempts(), 1);
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_times_out_when_the_host_never_answers() {
    // H reads every request and never answers; the counter is placed on H alone, with
    // the shortest service timeout (each attempt: 100 ms + 5 s).
    let hs = hosts(&[Serve::Attached, Serve::Hang]).await;
    hs[0]
        .svcs()
        .deploy_wasm(
            COUNTER,
            Bytes::from(fixture(COUNTER)),
            DeployOpts {
                limits: Limits {
                    request_timeout_ms: 100,
                    ..Limits::default()
                },
                placement: Placement::Nodes(vec![hs[1].id()]),
                ..DeployOpts::default()
            },
        )
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = client_dir(&root);
    let short = Duration::from_secs(2);
    let c = join(
        &dir,
        &hs[0].invite(Role::Client).await,
        ClientOptions {
            request_timeout: short,
            ..opts()
        },
    )
    .await;
    wait_service(&c, COUNTER, 1).await;

    // `request_timeout` bounds the whole call.
    let start = Instant::now();
    let r = c.request(COUNTER, get("/")).await;
    assert!(matches!(r, Err(ClientError::Timeout)), "{r:?}");
    let took = start.elapsed();
    assert!(took >= short && took < short * 2, "took {took:?}");
    c.shutdown().await;

    // With room for the attempt, the attempt's own timeout ends it (ruling R11).
    let c = open(&dir, opts()).await;
    wait_service(&c, COUNTER, 1).await;
    let start = Instant::now();
    let r = timeout(T, c.request(COUNTER, get("/"))).await.unwrap();
    assert!(matches!(r, Err(ClientError::Timeout)), "{r:?}");
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(5100), "took {took:?}");
    assert!(took < opts().request_timeout, "took {took:?}");
    assert_eq!(c.attempts(), 1);
    c.shutdown().await;
    for h in hs {
        h.shutdown().await;
    }
}
