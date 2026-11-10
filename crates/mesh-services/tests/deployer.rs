//! Placement, the deployer loop, dispatch and status on small meshes (spec §5.1–§5.4).

mod common;

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use common::{T, fixture, test_cfg, wait_until};
use mesh_core::membership::MESH_NS;
use mesh_core::{Node, NodeEvent, NodeStatus};
use mesh_hlc::ManualClock;
use mesh_proto::{NodeId, Placement, Role, ServiceKind, ServiceManifest};
use mesh_runtime::{CodeCache, Runtime, RuntimeConfig};
use mesh_services::{
    DeployOpts, DeployerTimings, LocalStatus, ServiceStatus, Services, ServicesConfig, data_ns,
    placed, placed_hosts,
};
use tempfile::TempDir;
use tokio::sync::OnceCell;
use tokio::time::{Instant, sleep, timeout};

const NOT_PLACED: &str = "Service not placed on this node";
const NOT_REPLICATED: &str = "Service not yet replicated";
const NOT_DEPLOYED: &str = "Service not deployed";
const REBOOTSTRAPPING: &str = "Node re-bootstrapping";
const DAY_MS: u64 = 86_400_000;

fn timings() -> DeployerTimings {
    DeployerTimings {
        rescan_every: Duration::from_secs(2),
        retry_min: Duration::from_millis(200),
        retry_max: Duration::from_secs(1),
    }
}

async fn start(node: &Node, cache: CodeCache) -> Services {
    let cfg = ServicesConfig {
        cache,
        ..ServicesConfig::default()
    };
    Services::start_with_timings(node, cfg, timings())
        .await
        .unwrap()
}

/// A code cache shared by every test in this process, filled once with the fixtures used
/// here. Compiling a component in a debug build takes seconds; a dozen tests compiling the
/// same fixtures at once on every node would make the ready waits time out.
async fn warm_cache() -> CodeCache {
    static WARM: OnceCell<()> = OnceCell::const_new();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mesh-services-code-cache");
    WARM.get_or_init(|| async {
        let rt = Runtime::new(RuntimeConfig {
            cache: CodeCache::Read(dir.clone()),
            ..RuntimeConfig::default()
        })
        .unwrap();
        for name in ["counter", "echo"] {
            let wasm = fixture(name);
            rt.load(*blake3::hash(&wasm).as_bytes(), &wasm)
                .await
                .unwrap();
        }
    })
    .await;
    CodeCache::Read(dir)
}

/// A founder host plus one node per `(role, tags)`; every node runs `Services`.
struct Net {
    nodes: Vec<Node>,
    svcs: Vec<Services>,
    dirs: Vec<TempDir>,
}

impl Net {
    fn id(&self, i: usize) -> NodeId {
        self.nodes[i].node_id()
    }

    async fn shutdown(self) {
        for s in self.svcs {
            s.shutdown().await;
        }
        for n in self.nodes {
            n.shutdown().await;
        }
    }
}

async fn net(founder_tags: &[&str], others: &[(Role, &[&str])]) -> Net {
    net_on(None, founder_tags, others).await
}

/// [`net`] with every node on `clock` when given.
async fn net_on(
    clock: Option<&ManualClock>,
    founder_tags: &[&str],
    others: &[(Role, &[&str])],
) -> Net {
    let on_clock = |mut cfg: mesh_core::NodeConfig| {
        if let Some(c) = clock {
            cfg.wall = std::sync::Arc::new(c.clone());
        }
        cfg
    };
    let mut dirs = vec![tempfile::tempdir().unwrap()];
    let mut cfg = on_clock(test_cfg(dirs[0].path(), "n0"));
    cfg.tags = founder_tags.iter().map(|t| t.to_string()).collect();
    let mut nodes = vec![Node::create_mesh(cfg).await.unwrap()];
    for (i, (role, tags)) in others.iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = on_clock(test_cfg(dir.path(), &format!("n{}", i + 1)));
        cfg.tags = tags.iter().map(|t| t.to_string()).collect();
        let ticket = nodes[0]
            .invite(*role, Duration::from_secs(900))
            .await
            .unwrap();
        let node = timeout(T, Node::join(cfg, ticket))
            .await
            .expect("join timed out")
            .unwrap();
        assert!(node.wait_first_sync(T).await);
        nodes.push(node);
        dirs.push(dir);
    }
    let ids: Vec<NodeId> = nodes.iter().map(Node::node_id).collect();
    wait_until("every node sees every other online", T, || {
        nodes.iter().all(|n| {
            ids.iter().all(|id| {
                n.members()
                    .iter()
                    .any(|m| m.id == *id && m.online && m.effective)
            })
        })
    })
    .await;
    let mut svcs = Vec::new();
    for n in &nodes {
        svcs.push(start(n, warm_cache().await).await);
    }
    Net { nodes, svcs, dirs }
}

async fn wait_for<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + T;
    while !cond().await {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn call(svc: &Services, name: &str, path: &str) -> (u16, String) {
    let req = http::Request::get(format!("http://{name}.mesh.localhost{path}"))
        .body(Bytes::new())
        .unwrap();
    let resp = svc.handle(name, req).await;
    (
        resp.status().as_u16(),
        String::from_utf8_lossy(resp.body()).into_owned(),
    )
}

fn status(svc: &Services, name: &str) -> Option<ServiceStatus> {
    svc.services().into_iter().find(|s| s.manifest.name == name)
}

fn local(svc: &Services, name: &str) -> Option<LocalStatus> {
    status(svc, name).map(|s| s.local)
}

async fn wait_ready(svc: &Services, name: &str, version: u64) {
    wait_until(&format!("{name} ready at v{version}"), T, || {
        local(svc, name) == Some(LocalStatus::Ready { version })
    })
    .await;
}

async fn wait_not_placed(svc: &Services, name: &str) {
    wait_until(&format!("{name} not placed"), T, || {
        local(svc, name) == Some(LocalStatus::NotPlaced)
    })
    .await;
}

/// Waits until `node` holds `want` at `ns/key`.
async fn wait_kv(node: &Node, ns: &str, key: &str, want: &str) {
    wait_for(&format!("{ns}/{key} = {want}"), || async {
        node.store().get(ns, key).await.unwrap().as_deref() == Some(want.as_bytes())
    })
    .await;
}

fn online(node: &Node, id: NodeId) -> bool {
    node.members().iter().any(|m| m.id == id && m.online)
}

fn module_of(m: &ServiceManifest) -> [u8; 32] {
    match m.kind {
        ServiceKind::Wasm { module } => module,
        ServiceKind::Static { root } => root,
    }
}

fn wasm(name: &str) -> Bytes {
    Bytes::from(fixture(name))
}

fn opts(placement: Placement) -> DeployOpts {
    DeployOpts {
        placement,
        ..DeployOpts::default()
    }
}

/// Cuts A (index 0) off from B (index 1) both ways.
async fn split(n: &Net) {
    let (ia, ib) = (n.id(0), n.id(1));
    n.nodes[0].partition(&[ib]);
    n.nodes[1].partition(&[ia]);
    wait_until("A and B apart", T, || {
        !online(&n.nodes[0], ib) && !online(&n.nodes[1], ia)
    })
    .await;
}

async fn heal(n: &Net) {
    n.nodes[0].heal();
    n.nodes[1].heal();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_wasm_on_a_served_on_b() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    let m = a
        .deploy_wasm("counter", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "counter", 1).await;
    wait_ready(b, "counter", 1).await;
    // The module B fetched is service-owned there, so B's blob GC may collect it later.
    let owned = std::fs::read_to_string(n.dirs[1].path().join("services/owned.json")).unwrap();
    assert!(owned.contains(&hex::encode(module_of(&m))), "{owned}");

    assert_eq!(call(a, "counter", "/").await, (200, "1".into()));
    wait_kv(&n.nodes[1], "counter", "n", "1").await;
    assert_eq!(call(b, "counter", "/").await, (200, "2".into()));
    wait_kv(&n.nodes[0], "counter", "n", "2").await;
    assert_eq!(call(a, "counter", "/").await, (200, "3".into()));
    let st = status(b, "counter").unwrap();
    assert_eq!(st.generation, 0);
    let mut hosts = st.placed_hosts.clone();
    hosts.sort();
    let mut want = vec![n.id(0), n.id(1)];
    want.sort();
    assert_eq!(hosts, want);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_version_swaps_old_serves_until_ready() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    a.deploy_wasm("web", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(b, "web", 1).await;

    // v2's blob is withheld from B: A and B are partitioned, and only the manifest is
    // hand-delivered to B.
    split(&n).await;
    let m2 = a
        .deploy_wasm("web", wasm("echo"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "web", 2).await;
    assert_eq!(call(a, "web", "/?size=2").await, (200, "aa".into()));
    n.nodes[1].deliver_from(&n.nodes[0]).await.unwrap();
    wait_until("B learns v2 and fails to fetch it", T, || {
        status(b, "web")
            .is_some_and(|s| s.manifest == m2 && matches!(s.local, LocalStatus::Failed { .. }))
    })
    .await;
    // B keeps serving v1 (the counter) meanwhile.
    let (code, body) = call(b, "web", "/?size=2").await;
    assert_eq!(code, 200);
    assert!(body.parse::<u64>().is_ok(), "v1 answers: {body}");
    assert!(!n.nodes[1].blobs().has(module_of(&m2).into()).await.unwrap());

    heal(&n).await;
    wait_ready(b, "web", 2).await;
    assert_eq!(call(b, "web", "/?size=2").await, (200, "aa".into()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_tags_and_nodes() {
    let n = net(&[], &[(Role::Host, &["edge"]), (Role::Host, &["core"])]).await;
    let (a, b, c) = (&n.svcs[0], &n.svcs[1], &n.svcs[2]);
    let (ia, ib, ic) = (n.id(0), n.id(1), n.id(2));
    let m = a
        .deploy_wasm(
            "web",
            wasm("echo"),
            opts(Placement::Tags(vec!["edge".into()])),
        )
        .await
        .unwrap();
    wait_ready(b, "web", 1).await;
    wait_not_placed(a, "web").await;
    wait_not_placed(c, "web").await;
    assert!(placed(&m, &n.nodes[1].handle()));
    assert!(!placed(&m, &n.nodes[0].handle()));
    assert!(!placed(&m, &n.nodes[2].handle()));
    assert_eq!(placed_hosts(&m, &n.nodes[0].handle()), vec![ib]);
    assert_eq!(status(c, "web").unwrap().placed_hosts, vec![ib]);
    assert_eq!(call(c, "web", "/").await, (503, NOT_PLACED.into()));

    // Node list: C and A run it, B stops.
    a.set_placement("web", Placement::Nodes(vec![ic, ia]))
        .await
        .unwrap();
    wait_ready(a, "web", 2).await;
    wait_ready(c, "web", 2).await;
    wait_not_placed(b, "web").await;
    assert_eq!(call(b, "web", "/").await, (503, NOT_PLACED.into()));
    assert_eq!(call(c, "web", "/?size=1").await, (200, "a".into()));

    // Back to tags; then C gains the tag and starts running it.
    a.set_placement("web", Placement::Tags(vec!["edge".into()]))
        .await
        .unwrap();
    wait_ready(b, "web", 3).await;
    wait_not_placed(c, "web").await;
    n.nodes[2]
        .set_info(None, Some(vec!["edge".into()]))
        .await
        .unwrap();
    wait_ready(c, "web", 3).await;
    wait_until("A sees B and C placed", T, || {
        let mut h = status(a, "web").unwrap().placed_hosts;
        h.sort();
        let mut want = vec![ib, ic];
        want.sort();
        h == want
    })
    .await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_placed_gives_503_and_no_fetch() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    let m = a
        .deploy_wasm("web", wasm("echo"), opts(Placement::Nodes(vec![n.id(0)])))
        .await
        .unwrap();
    wait_ready(a, "web", 1).await;
    wait_not_placed(b, "web").await;
    assert_eq!(call(b, "web", "/").await, (503, NOT_PLACED.into()));
    assert_eq!(call(a, "web", "/?size=1").await, (200, "a".into()));
    assert!(!n.nodes[1].blobs().has(module_of(&m).into()).await.unwrap());
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_matches_no_host_status_warns() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    let m = a
        .deploy_wasm(
            "web",
            wasm("echo"),
            opts(Placement::Tags(vec!["nowhere".into()])),
        )
        .await
        .unwrap();
    wait_not_placed(a, "web").await;
    wait_not_placed(b, "web").await;
    assert!(placed_hosts(&m, &n.nodes[0].handle()).is_empty());
    assert!(status(a, "web").unwrap().placed_hosts.is_empty());
    assert!(status(b, "web").unwrap().placed_hosts.is_empty());
    assert_eq!(call(a, "web", "/").await, (503, NOT_PLACED.into()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undeploy_gives_404_data_kept() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    a.deploy_wasm("counter", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "counter", 1).await;
    wait_ready(b, "counter", 1).await;
    assert_eq!(call(a, "counter", "/").await, (200, "1".into()));
    wait_kv(&n.nodes[1], "counter", "n", "1").await;
    assert_eq!(call(b, "counter", "/").await, (200, "2".into()));

    a.undeploy("counter").await.unwrap();
    for s in [a, b] {
        wait_until("status gone", T, || status(s, "counter").is_none()).await;
        assert_eq!(call(s, "counter", "/").await, (404, NOT_DEPLOYED.into()));
    }
    assert_eq!(call(a, "ghost", "/").await, (404, NOT_DEPLOYED.into()));
    wait_kv(&n.nodes[0], "counter", "n", "2").await;
    wait_kv(&n.nodes[1], "counter", "n", "2").await;

    // The data survives a redeploy.
    a.deploy_wasm("counter", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "counter", 1).await;
    assert_eq!(call(a, "counter", "/").await, (200, "3".into()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_bumps_generation_hides_keys_and_sets() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    a.deploy_wasm("counter", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "counter", 1).await;
    wait_ready(b, "counter", 1).await;
    assert_eq!(call(a, "counter", "/").await, (200, "1".into()));
    assert_eq!(call(a, "counter", "/").await, (200, "2".into()));
    assert_eq!(
        call(a, "counter", "/setadd?s=x&m=a").await,
        (200, "ok".into())
    );
    assert_eq!(call(a, "counter", "/members?s=x").await, (200, "a".into()));
    wait_kv(&n.nodes[1], "counter", "n", "2").await;
    wait_for("B has the set", || async {
        call(b, "counter", "/members?s=x").await == (200, "a".into())
    })
    .await;

    a.purge("counter").await.unwrap();
    for s in [a, b] {
        wait_until("generation 1", T, || {
            status(s, "counter").is_some_and(|st| st.generation == 1)
        })
        .await;
    }
    // The counter starts again from 1, shared by both nodes, in the new namespace.
    assert_eq!(call(b, "counter", "/").await, (200, "1".into()));
    wait_kv(&n.nodes[0], &data_ns("counter", 1), "n", "1").await;
    assert_eq!(call(a, "counter", "/").await, (200, "2".into()));
    for s in [a, b] {
        assert_eq!(
            call(s, "counter", "/members?s=x").await,
            (200, String::new())
        );
        assert_eq!(call(s, "counter", "/get?k=n").await.0, 200);
    }
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deploys_lww_winner_everywhere() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    split(&n).await;
    a.deploy_wasm("web", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    b.deploy_wasm("web", wasm("echo"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "web", 1).await;
    wait_ready(b, "web", 1).await;
    heal(&n).await;

    wait_until("one manifest everywhere, ready on both", T, || {
        match (status(a, "web"), status(b, "web")) {
            (Some(sa), Some(sb)) => {
                sa.manifest == sb.manifest
                    && sa.local == LocalStatus::Ready { version: 1 }
                    && sb.local == LocalStatus::Ready { version: 1 }
            }
            _ => false,
        }
    })
    .await;
    // Both run the winner: either both echo or both count.
    let (ca, ra) = call(a, "web", "/?size=2").await;
    let (cb, rb) = call(b, "web", "/?size=2").await;
    assert_eq!((ca, cb), (200, 200));
    assert_eq!(ra == "aa", rb == "aa", "A: {ra}, B: {rb}");
    let winner = status(a, "web").unwrap().manifest;
    for node in &n.nodes {
        assert!(node.blobs().has(module_of(&winner).into()).await.unwrap());
    }
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_fetched_gives_503() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    split(&n).await;
    let m = a
        .deploy_wasm("web", wasm("echo"), DeployOpts::default())
        .await
        .unwrap();
    n.nodes[1].deliver_from(&n.nodes[0]).await.unwrap();
    wait_until("B placed but failing", T, || {
        matches!(local(b, "web"), Some(LocalStatus::Failed { .. }))
    })
    .await;
    assert_eq!(call(b, "web", "/").await, (503, NOT_REPLICATED.into()));
    assert!(!n.nodes[1].blobs().has(module_of(&m).into()).await.unwrap());

    heal(&n).await;
    wait_ready(b, "web", 1).await;
    assert_eq!(call(b, "web", "/?size=3").await, (200, "aaa".into()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kv_write_refused_when_removed_gives_503() {
    let n = net(&[], &[(Role::Host, &[])]).await;
    let (a, b) = (&n.svcs[0], &n.svcs[1]);
    a.deploy_wasm("counter", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "counter", 1).await;
    wait_ready(b, "counter", 1).await;
    assert_eq!(call(a, "counter", "/").await.0, 200);
    wait_kv(&n.nodes[1], "counter", "n", "1").await;

    // Freeze B's deployer so the service is still active there when B learns of its
    // removal (left alone, the deployer would unplace it: B is no longer effective).
    b.pause_deployer(true);
    n.nodes[0].remove(n.id(1)).await.unwrap();
    wait_until("B learns of its removal", T, || {
        matches!(n.nodes[1].status(), NodeStatus::Removed { .. })
    })
    .await;
    // The write is refused by the store gate: the runtime's 503, with no body.
    assert_eq!(call(b, "counter", "/").await, (503, String::new()));
    // Reads still work, so the 503 came from the write.
    assert_eq!(call(b, "counter", "/get?k=n").await, (200, "1".into()));

    b.pause_deployer(false);
    wait_not_placed(b, "counter").await;
    assert_eq!(call(b, "counter", "/").await, (503, NOT_PLACED.into()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_role_never_runs() {
    let n = net(&[], &[(Role::Client, &[])]).await;
    let (a, c) = (&n.svcs[0], &n.svcs[1]);
    let m = a
        .deploy_wasm("web", wasm("echo"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(a, "web", 1).await;
    wait_not_placed(c, "web").await;
    assert!(!placed(&m, &n.nodes[1].handle()));
    assert_eq!(placed_hosts(&m, &n.nodes[1].handle()), vec![n.id(0)]);
    assert_eq!(call(c, "web", "/").await, (503, NOT_PLACED.into()));
    assert!(!n.nodes[1].blobs().has(module_of(&m).into()).await.unwrap());
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn services_resume_after_restart() {
    let Net {
        mut nodes,
        mut svcs,
        dirs,
    } = net(&[], &[(Role::Host, &[])]).await;
    let cache = tempfile::tempdir().unwrap();
    let mmap = |p: &Path| CodeCache::Mmap(p.to_path_buf());
    // B runs with a code cache.
    svcs.pop().unwrap().shutdown().await;
    svcs.push(start(&nodes[1], mmap(cache.path())).await);
    svcs[0]
        .deploy_wasm("counter", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    wait_ready(&svcs[1], "counter", 1).await;
    assert_eq!(svcs[1].cache_stats(), (0, 1));
    assert_eq!(call(&svcs[1], "counter", "/").await, (200, "1".into()));

    // Stop everything; B comes back alone, with no peer to fetch from.
    for s in svcs.drain(..).rev() {
        s.shutdown().await;
    }
    while let Some(node) = nodes.pop() {
        node.shutdown().await;
    }
    let b = Node::open(test_cfg(dirs[1].path(), "n1")).await.unwrap();
    let svc = start(&b, mmap(cache.path())).await;
    wait_ready(&svc, "counter", 1).await;
    assert_eq!(svc.cache_stats(), (1, 0));
    assert_eq!(call(&svc, "counter", "/").await, (200, "2".into()));
    svc.shutdown().await;
    b.shutdown().await;
}

/// Waits until `node` completed a full sync at or after wall time `ms`.
async fn full_sync_since(node: &Node, ms: u64) {
    wait_for("a full sync at the new time", || async {
        node.store()
            .call(|r| r.last_full_sync_ms())
            .await
            .unwrap()
            .is_some_and(|t| t >= ms)
    })
    .await;
}

/// While its node re-bootstraps (store emptied, `_mesh` refilling), a node's services
/// answer 503 and the deployer keeps every active service; afterwards they serve again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebootstrap_freezes_services() {
    let clock = ManualClock::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    );
    // Founder X, hosts A (index 1, the node that goes stale) and C (index 2).
    let n = net_on(Some(&clock), &[], &[(Role::Host, &[]), (Role::Host, &[])]).await;
    let (x, a, c) = (&n.nodes[0], &n.nodes[1], &n.nodes[2]);
    let svc_a = &n.svcs[1];
    n.svcs[0]
        .deploy_wasm("counter", wasm("counter"), DeployOpts::default())
        .await
        .unwrap();
    for s in &n.svcs {
        wait_ready(s, "counter", 1).await;
    }
    assert_eq!(call(svc_a, "counter", "/").await.0, 200);

    // A is cut off; X and C keep syncing while the clock moves past A's 80-day staleness.
    let (ix, ia, ic) = (n.id(0), n.id(1), n.id(2));
    a.partition(&[ix, ic]);
    x.partition(&[ia]);
    c.partition(&[ia]);
    wait_until("A cut off", T, || {
        !online(a, ix) && !online(a, ic) && !online(x, ia) && !online(c, ia)
    })
    .await;
    let day0 = mesh_hlc::WallClock::now_ms(&clock);
    for day in [50, 81] {
        clock.set(day0 + day * DAY_MS);
        full_sync_since(x, day0 + day * DAY_MS).await;
        full_sync_since(c, day0 + day * DAY_MS).await;
    }

    // On contact A re-bootstraps. It is held before its reset while it is cut off again,
    // so its store stays empty: nothing can refill `_mesh`.
    a.hold_rebootstrap_before_reset(true);
    let mut events = a.subscribe();
    a.heal();
    x.heal();
    c.heal();
    wait_until("A re-bootstrapping", T, || {
        matches!(a.status(), NodeStatus::Rebootstrapping { .. })
    })
    .await;
    a.partition(&[ix, ic]);
    x.partition(&[ia]);
    c.partition(&[ia]);
    wait_until("A cut off again", T, || {
        !online(a, ix) && !online(a, ic) && !online(x, ia) && !online(c, ia)
    })
    .await;
    a.hold_rebootstrap_before_reset(false);
    wait_for("A's store reset", || async {
        a.store()
            .list_keys(MESH_NS, "service/", None, 1)
            .await
            .unwrap()
            .is_empty()
    })
    .await;
    // Two full deployer scans see the emptied store.
    let scans = svc_a.deployer_scans();
    wait_until("two deployer scans", T, || {
        svc_a.deployer_scans() >= scans + 2
    })
    .await;
    assert_eq!(
        call(svc_a, "counter", "/").await,
        (503, REBOOTSTRAPPING.into())
    );
    assert_eq!(
        local(svc_a, "counter"),
        Some(LocalStatus::Ready { version: 1 }),
        "the service stays known and active"
    );

    a.heal();
    x.heal();
    c.heal();
    timeout(T, async {
        loop {
            if events.recv().await.unwrap() == NodeEvent::RebootstrapDone {
                return;
            }
        }
    })
    .await
    .expect("no RebootstrapDone");
    wait_for("A serves counter again", || async {
        call(svc_a, "counter", "/").await.0 == 200
    })
    .await;
    assert_eq!(
        local(svc_a, "counter"),
        Some(LocalStatus::Ready { version: 1 })
    );
    n.shutdown().await;
}
