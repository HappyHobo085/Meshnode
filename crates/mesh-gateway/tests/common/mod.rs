//! Harness for multi-node gateway tests: loopback only, relay and discovery off, fast
//! timings; every node runs `Services`, and serves `httpfwd` with an [`HttpFwdHandler`]
//! (attached or not) or a raw test handler.
#![allow(dead_code, unused_imports)]

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use mesh_core::{DynProtocolHandler, Node, NodeConfig, RelayChoice, Timings};
use mesh_gateway::{Forwarder, HttpFwdHandler, HttpFwdSlot};
use mesh_hlc::ManualClock;
use mesh_proto::{ALPN_HTTPFWD, NodeId, Role};
use mesh_runtime::{CodeCache, Runtime, RuntimeConfig};
use mesh_services::{DeployerTimings, LocalStatus, Services, ServicesConfig};
use tempfile::TempDir;
use tokio::sync::OnceCell;
use tokio::time::{Instant, sleep, timeout};

pub use mesh_runtime::test_fixtures::fixture;

/// Default timeout for a single await in the tests.
pub const T: Duration = Duration::from_secs(30);

pub fn test_cfg(dir: &Path, name: &str) -> NodeConfig {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1_700_000_000_000);
    let mut cfg = NodeConfig::new(dir, name);
    cfg.relay = RelayChoice::Disabled;
    cfg.discovery = false;
    cfg.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    cfg.timings = Timings::fast_for_tests();
    cfg.wall = Arc::new(ManualClock::new(now));
    cfg
}

/// Polls `cond` until it holds. Panics after `limit`.
pub async fn wait_until<F: FnMut() -> bool>(what: &str, limit: Duration, mut cond: F) {
    let deadline = Instant::now() + limit;
    while !cond() {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

/// Polls the async `cond` until it holds. Panics after [`T`].
pub async fn wait_for<F, Fut>(what: &str, mut cond: F)
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

/// A code cache shared by every test in this process, filled once with `counter`.
async fn warm_cache() -> CodeCache {
    static WARM: OnceCell<()> = OnceCell::const_new();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mesh-gateway-code-cache");
    WARM.get_or_init(|| async {
        let rt = Runtime::new(RuntimeConfig {
            cache: CodeCache::Read(dir.clone()),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let wasm = fixture("counter");
        rt.load(*blake3::hash(&wasm).as_bytes(), &wasm)
            .await
            .unwrap();
    })
    .await;
    CodeCache::Read(dir)
}

/// How a node serves `httpfwd`.
pub enum Serve {
    /// An `HttpFwdHandler` attached to the node's `Services`.
    Attached,
    /// An `HttpFwdHandler` never attached.
    Unattached,
    /// A raw test handler.
    Raw(Arc<dyn DynProtocolHandler>),
}

pub struct Member {
    pub node: Node,
    pub svcs: Arc<Services>,
    /// `None` for a raw handler.
    pub slot: Option<HttpFwdSlot>,
    pub fwd: Forwarder,
    _dir: TempDir,
}

pub struct Mesh {
    pub m: Vec<Member>,
}

impl Mesh {
    pub fn id(&self, i: usize) -> NodeId {
        self.m[i].node.node_id()
    }

    pub fn svcs(&self, i: usize) -> &Services {
        &self.m[i].svcs
    }

    pub fn fwd(&self, i: usize) -> &Forwarder {
        &self.m[i].fwd
    }

    pub fn served(&self, i: usize) -> u64 {
        self.m[i].slot.as_ref().expect("an HttpFwdHandler").served()
    }

    pub async fn shutdown(self) {
        for m in self.m {
            let Member {
                node,
                svcs,
                slot,
                fwd,
                _dir,
            } = m;
            drop(fwd);
            drop(slot);
            if let Ok(s) = Arc::try_unwrap(svcs) {
                timeout(T, s.shutdown()).await.expect("services shutdown");
            }
            timeout(T, node.shutdown()).await.expect("node shutdown");
            drop(_dir);
        }
    }
}

fn deployer_timings() -> DeployerTimings {
    DeployerTimings {
        rescan_every: Duration::from_secs(2),
        retry_min: Duration::from_millis(200),
        retry_max: Duration::from_secs(1),
    }
}

/// A founder (always a host) plus one node per further entry, each with its role and its
/// way of serving `httpfwd`. Every node sees every other online before this returns.
pub async fn mesh(nodes: Vec<(Role, Serve)>) -> Mesh {
    assert_eq!(nodes[0].0, Role::Host, "the founder is a host");
    let mut built: Vec<(Node, Option<HttpFwdSlot>, bool, TempDir)> = Vec::new();
    for (i, (role, serve)) in nodes.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_cfg(dir.path(), &format!("n{i}"));
        let (handler, slot, attach): (Arc<dyn DynProtocolHandler>, _, _) = match serve {
            Serve::Attached => {
                let (h, s) = HttpFwdHandler::new();
                (Arc::new(h), Some(s), true)
            }
            Serve::Unattached => {
                let (h, s) = HttpFwdHandler::new();
                (Arc::new(h), Some(s), false)
            }
            Serve::Raw(h) => (h, None, false),
        };
        cfg.extra_protocols.push((ALPN_HTTPFWD.to_vec(), handler));
        let node = if i == 0 {
            Node::create_mesh(cfg).await.unwrap()
        } else {
            let ticket = built[0]
                .0
                .invite(role, Duration::from_secs(900))
                .await
                .unwrap();
            let node = timeout(T, Node::join(cfg, ticket))
                .await
                .expect("join timed out")
                .unwrap();
            assert!(node.wait_first_sync(T).await);
            node
        };
        built.push((node, slot, attach, dir));
    }
    let ids: Vec<NodeId> = built.iter().map(|b| b.0.node_id()).collect();
    wait_until("every node sees every other online", T, || {
        built.iter().all(|(n, ..)| {
            ids.iter().all(|id| {
                n.members()
                    .iter()
                    .any(|m| m.id == *id && m.online && m.effective)
            })
        })
    })
    .await;
    let mut m = Vec::new();
    for (node, slot, attach, dir) in built {
        let cfg = ServicesConfig {
            cache: Some(warm_cache().await),
            ..ServicesConfig::default()
        };
        let svcs = Arc::new(
            Services::start_with_timings(&node, cfg, deployer_timings())
                .await
                .unwrap(),
        );
        if attach {
            slot.as_ref().unwrap().attach(&svcs);
        }
        let fwd = Forwarder::new(node.handle(), svcs.clone());
        m.push(Member {
            node,
            svcs,
            slot,
            fwd,
            _dir: dir,
        });
    }
    Mesh { m }
}

pub fn online(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id && m.online)
}

/// Cuts `a` and `b` apart and waits until neither sees the other online.
pub async fn split(mesh: &Mesh, a: usize, b: usize) {
    let (ia, ib) = (mesh.id(a), mesh.id(b));
    mesh.m[a].node.partition(&[ib]);
    mesh.m[b].node.partition(&[ia]);
    wait_until("nodes apart", T, || {
        !online(&mesh.m[a].node, ib) && !online(&mesh.m[b].node, ia)
    })
    .await;
}

pub async fn wait_ready(svc: &Services, name: &str, version: u64) {
    wait_until(&format!("{name} ready at v{version}"), T, || {
        svc.services()
            .iter()
            .any(|s| s.manifest.name == name && s.local == LocalStatus::Ready { version })
    })
    .await;
}

/// Waits until `node` holds `want` at `ns/key`.
pub async fn wait_kv(node: &Node, ns: &str, key: &str, want: &str) {
    wait_for(&format!("{ns}/{key} = {want}"), || async {
        node.store().get(ns, key).await.unwrap().as_deref() == Some(want.as_bytes())
    })
    .await;
}

pub fn get(name: &str, path: &str) -> http::Request<Bytes> {
    http::Request::get(path)
        .header(http::header::HOST, format!("{name}.mesh.localhost"))
        .body(Bytes::new())
        .unwrap()
}

/// `(status, body)` of a dispatch through `fwd`, under [`T`].
pub async fn dispatch(fwd: &Forwarder, name: &str, path: &str) -> (u16, String) {
    let resp = timeout(T, fwd.dispatch(name, get(name, path)))
        .await
        .expect("dispatch timed out");
    (
        resp.status().as_u16(),
        String::from_utf8_lossy(resp.body()).into_owned(),
    )
}
