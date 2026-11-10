//! Blob GC (spec §5.5): service blobs unreferenced for 24 h are removed.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::{T, fixture, test_cfg, wait_until};
use mesh_core::{BlobHash, Node};
use mesh_hlc::ManualClock;
use mesh_proto::{Placement, ServiceKind, ServiceManifest};
use mesh_services::{DeployOpts, LocalStatus, Services, ServicesConfig};
use tempfile::TempDir;

const HOUR_MS: u64 = 3_600_000;

struct One {
    node: Node,
    svc: Services,
    clock: ManualClock,
    dir: TempDir,
}

async fn one(gc_every: Option<Duration>) -> One {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_cfg(dir.path(), "n0");
    let clock = ManualClock::new(1_700_000_000_000);
    cfg.wall = Arc::new(clock.clone());
    let node = Node::create_mesh(cfg).await.unwrap();
    let svc = match gc_every {
        Some(every) => Services::start_with_gc_interval(&node, ServicesConfig::default(), every)
            .await
            .unwrap(),
        None => Services::start(&node, ServicesConfig::default())
            .await
            .unwrap(),
    };
    One {
        node,
        svc,
        clock,
        dir,
    }
}

impl One {
    async fn shutdown(self) {
        self.svc.shutdown().await;
        self.node.shutdown().await;
    }

    async fn ready(&self, name: &str, version: u64) {
        wait_until(&format!("{name} ready at v{version}"), T, || {
            self.svc
                .services()
                .iter()
                .any(|s| s.manifest.name == name && s.local == LocalStatus::Ready { version })
        })
        .await;
    }

    async fn deploy(&self, name: &str, fixture_name: &str, version: u64) -> BlobHash {
        let m = self
            .svc
            .deploy_wasm(
                name,
                Bytes::from(fixture(fixture_name)),
                DeployOpts::default(),
            )
            .await
            .unwrap();
        assert_eq!(m.version, version);
        self.ready(name, version).await;
        module_of(&m)
    }

    async fn pinned(&self, h: BlobHash) -> bool {
        self.node.blobs().is_pinned(h).await.unwrap()
    }

    fn gc_file(&self) -> std::path::PathBuf {
        self.dir.path().join("services").join("gc.json")
    }
}

fn module_of(m: &ServiceManifest) -> BlobHash {
    match m.kind {
        ServiceKind::Wasm { module } => BlobHash::from(module),
        ServiceKind::Static { root } => BlobHash::from(root),
    }
}

fn write_site(dir: &Path) {
    std::fs::write(dir.join("index.html"), "<h1>hi</h1>").unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/a.txt"), "doc a").unwrap();
}

fn elsewhere() -> DeployOpts {
    DeployOpts {
        placement: Placement::Tags(vec!["elsewhere".into()]),
        ..DeployOpts::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_gc_after_24h_unreferenced_keeps_referenced() {
    let t = one(None).await;
    let v1 = t.deploy("web", "counter", 1).await;
    let v2 = t.deploy("web", "echo", 2).await;
    assert_ne!(v1, v2);

    // A static site: its root and children stay.
    let site = tempfile::tempdir().unwrap();
    write_site(site.path());
    let m = t
        .svc
        .deploy_static("site", site.path(), DeployOpts::default())
        .await
        .unwrap();
    t.ready("site", 1).await;
    let root = module_of(&m);
    let children = t
        .node
        .blobs()
        .collection_entries(root)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(children.len(), 2);

    // Deployed but not placed here: kept so others can fetch it.
    let m = t
        .svc
        .deploy_wasm("faraway", Bytes::from(fixture("outbound")), elsewhere())
        .await
        .unwrap();
    let far = module_of(&m);

    assert!(t.pinned(v1).await);
    t.svc.gc_now().await.unwrap();
    assert!(t.gc_file().exists(), "gc.json is written");
    t.clock.advance(HOUR_MS);
    t.svc.gc_now().await.unwrap();
    assert!(t.pinned(v1).await, "kept at +1 h");

    t.clock.advance(24 * HOUR_MS);
    t.svc.gc_now().await.unwrap();
    assert!(!t.pinned(v1).await, "removed at +25 h");
    assert!(t.pinned(v2).await);
    assert!(t.pinned(root).await);
    for (_, h) in &children {
        assert!(t.node.blobs().size(*h).await.unwrap().is_some());
    }
    assert!(t.pinned(far).await);
    t.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rereferencing_before_24h_clears_the_entry() {
    let t = one(None).await;
    let v1 = t.deploy("web", "counter", 1).await;
    t.deploy("web", "echo", 2).await;
    t.svc.gc_now().await.unwrap(); // v1 first unreferenced at t0

    t.clock.advance(HOUR_MS);
    t.deploy("web", "counter", 3).await; // v1 referenced again
    t.svc.gc_now().await.unwrap(); // clears the entry
    t.deploy("web", "echo", 4).await; // unreferenced again
    t.svc.gc_now().await.unwrap(); // a fresh entry at t0 + 1 h

    // 24.5 h after the first record, 23.5 h after the fresh one.
    t.clock.advance(23 * HOUR_MS + HOUR_MS / 2);
    t.svc.gc_now().await.unwrap();
    assert!(t.pinned(v1).await, "the old entry was cleared");

    t.clock.advance(HOUR_MS);
    t.svc.gc_now().await.unwrap();
    assert!(!t.pinned(v1).await);
    t.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_gc_file_starts_fresh() {
    let t = one(None).await;
    let v1 = t.deploy("web", "counter", 1).await;
    t.deploy("web", "echo", 2).await;
    std::fs::create_dir_all(t.gc_file().parent().unwrap()).unwrap();
    std::fs::write(t.gc_file(), b"{not json").unwrap();
    t.svc.gc_now().await.unwrap(); // starts fresh: v1 recorded now
    assert!(t.gc_file().exists());
    t.clock.advance(23 * HOUR_MS);
    t.svc.gc_now().await.unwrap();
    assert!(t.pinned(v1).await);
    t.clock.advance(2 * HOUR_MS);
    t.svc.gc_now().await.unwrap();
    assert!(!t.pinned(v1).await);
    t.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hourly_task_removes_without_gc_now() {
    let t = one(Some(Duration::from_millis(100))).await;
    let v1 = t.deploy("web", "counter", 1).await;
    t.deploy("web", "echo", 2).await;
    // Let the task record v1, then jump past 24 h.
    tokio::time::sleep(Duration::from_millis(500)).await;
    t.clock.advance(25 * HOUR_MS);
    let deadline = tokio::time::Instant::now() + T;
    while t.pinned(v1).await {
        assert!(tokio::time::Instant::now() < deadline, "v1 never removed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    t.shutdown().await;
}
