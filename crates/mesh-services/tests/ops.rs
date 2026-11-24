//! Manifest operations and the KV adapter (spec §4, §7).

mod common;

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use common::{T, fixture, test_cfg, wait_until};
use mesh_core::membership::MESH_NS;
use mesh_core::{Node, NodeStatus};
use mesh_proto::{Limits, Placement, Role, ServiceKind, ServiceManifest, decode_raw};
use mesh_runtime::{KvBackend, KvError};
use mesh_services::{
    DeployOpts, LocalStatus, ServiceKv, Services, ServicesConfig, ServicesError,
    current_generation, data_ns, manifest_key,
};
use tempfile::TempDir;
use tokio::time::timeout;

async fn solo() -> (Node, Services, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::create_mesh(test_cfg(dir.path(), "a")).await.unwrap();
    let svc = Services::start(&node, ServicesConfig::default())
        .await
        .unwrap();
    (node, svc, dir)
}

async fn manifest(node: &Node, name: &str) -> Option<ServiceManifest> {
    let raw = node
        .store()
        .get(MESH_NS, &manifest_key(name))
        .await
        .unwrap();
    raw.map(|b| decode_raw(&b).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_wasm_writes_manifest_v1_then_v2() {
    let (node, svc, d) = solo().await;
    let wasm = Bytes::from(fixture("echo"));
    let m1 = svc
        .deploy_wasm("web", wasm.clone(), DeployOpts::default())
        .await
        .unwrap();
    assert_eq!(m1.version, 1);
    assert_eq!(m1.limits, Limits::default());
    assert_eq!(m1.placement, Placement::AllHosts);
    let ServiceKind::Wasm { module } = m1.kind else {
        panic!("not wasm")
    };
    assert_eq!(module, *blake3::hash(&wasm).as_bytes());
    assert_eq!(manifest(&node, "web").await, Some(m1));
    assert!(node.blobs().has(module.into()).await.unwrap());
    // The default config caches compiled components under `<data>/services/cache`.
    let cached = std::fs::read_dir(d.path().join("services").join("cache"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().extension().is_some_and(|x| x == "cwasm"));
    assert!(cached, "no compiled component in the default code cache");

    let opts = DeployOpts {
        placement: Placement::Tags(vec!["edge".into()]),
        ..DeployOpts::default()
    };
    let m2 = svc.deploy_wasm("web", wasm, opts).await.unwrap();
    assert_eq!(m2.version, 2);
    assert_eq!(m2.placement, Placement::Tags(vec!["edge".into()]));
    assert_eq!(manifest(&node, "web").await, Some(m2));
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_static_writes_collection_manifest() {
    let (node, svc, _d) = solo().await;
    let site = tempfile::tempdir().unwrap();
    std::fs::write(site.path().join("index.html"), "<h1>hi</h1>").unwrap();
    let m = svc
        .deploy_static("site", site.path(), DeployOpts::default())
        .await
        .unwrap();
    let ServiceKind::Static { root } = m.kind else {
        panic!("not static")
    };
    assert_eq!(m.version, 1);
    assert!(node.blobs().has_collection(root.into()).await.unwrap());
    assert_eq!(manifest(&node, "site").await, Some(m));
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_bytes_rejected() {
    let (node, svc, _d) = solo().await;
    let core_module = wat::parse_str("(module)").unwrap();
    let cases: [Vec<u8>; 4] = [
        Vec::new(),
        vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        core_module,
        // A valid component that imports outside the world.
        fixture("bad"),
    ];
    let pins = node.blobs().pins().await.unwrap();
    for bytes in cases {
        let r = svc
            .deploy_wasm("junk", Bytes::from(bytes), DeployOpts::default())
            .await;
        assert!(
            matches!(r, Err(ServicesError::InvalidComponent(_))),
            "{r:?}"
        );
        assert_eq!(manifest(&node, "junk").await, None);
        assert_eq!(node.blobs().pins().await.unwrap(), pins);
    }
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_module_rejected() {
    let (node, svc, _d) = solo().await;
    let pins = node.blobs().pins().await.unwrap();
    let r = svc
        .deploy_wasm(
            "big",
            Bytes::from(vec![0u8; (64 << 20) + 1]),
            DeployOpts::default(),
        )
        .await;
    match r {
        Err(ServicesError::InvalidComponent(e)) => assert!(e.contains("64 MiB"), "{e}"),
        r => panic!("{r:?}"),
    }
    assert_eq!(manifest(&node, "big").await, None);
    assert_eq!(node.blobs().pins().await.unwrap(), pins);

    // A manifest naming an oversized module (from elsewhere) fails there without loading it.
    let big = node
        .blobs()
        .add_bytes(Bytes::from(vec![0u8; (64 << 20) + 1]))
        .await
        .unwrap();
    let m = ServiceManifest {
        name: "big".into(),
        kind: ServiceKind::Wasm {
            module: *big.as_bytes(),
        },
        version: 1,
        limits: Limits::default(),
        placement: Placement::AllHosts,
        outbound: vec![],
    };
    node.store()
        .set(MESH_NS, &manifest_key("big"), mesh_proto::encode_raw(&m))
        .await
        .unwrap();
    wait_until("the oversized module fails", T, || {
        svc.services().iter().any(|s| {
            s.manifest.name == "big"
                && matches!(&s.local, LocalStatus::Failed { error, .. } if error.contains("64 MiB"))
        })
    })
    .await;
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_name_rejected() {
    let (node, svc, _d) = solo().await;
    let wasm = Bytes::from(fixture("echo"));
    let long = "a".repeat(33);
    for name in ["A", "_mesh", "a.b", long.as_str()] {
        let r = svc
            .deploy_wasm(name, wasm.clone(), DeployOpts::default())
            .await;
        assert!(
            matches!(r, Err(ServicesError::InvalidName)),
            "{name}: {r:?}"
        );
        assert!(matches!(
            svc.deploy_static(name, Path::new("."), DeployOpts::default())
                .await,
            Err(ServicesError::InvalidName)
        ));
        assert!(matches!(
            svc.undeploy(name).await,
            Err(ServicesError::InvalidName)
        ));
        assert!(matches!(
            svc.purge(name).await,
            Err(ServicesError::InvalidName)
        ));
        assert!(matches!(
            svc.set_placement(name, Placement::AllHosts).await,
            Err(ServicesError::InvalidName)
        ));
    }
    assert!(node.blobs().pins().await.unwrap().is_empty());
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manifest_validation_rejects_bad_values() {
    let (node, svc, _d) = solo().await;
    let m = svc
        .deploy_wasm("web", Bytes::from(fixture("echo")), DeployOpts::default())
        .await
        .unwrap();
    let bad_limits = Limits {
        memory_mb: 0,
        ..Limits::default()
    };
    assert!(matches!(
        svc.set_limits("web", bad_limits).await,
        Err(ServicesError::InvalidManifest(_))
    ));
    for o in [vec!["nohostport".to_string()], vec!["h:1".to_string(); 65]] {
        assert!(matches!(
            svc.set_outbound("web", o).await,
            Err(ServicesError::InvalidManifest(_))
        ));
    }
    assert!(matches!(
        svc.set_placement("web", Placement::Tags(vec!["t".repeat(33)]))
            .await,
        Err(ServicesError::InvalidManifest(_))
    ));
    // Bad deploy options are caught before any blob is stored.
    let pins = node.blobs().pins().await.unwrap();
    let opts = DeployOpts {
        limits: bad_limits,
        ..DeployOpts::default()
    };
    assert!(matches!(
        svc.deploy_wasm("other", Bytes::from(fixture("counter")), opts)
            .await,
        Err(ServicesError::InvalidManifest(_))
    ));
    assert_eq!(node.blobs().pins().await.unwrap(), pins);
    // Nothing changed.
    assert_eq!(manifest(&node, "web").await, Some(m));
    assert!(matches!(
        svc.set_limits("ghost", Limits::default()).await,
        Err(ServicesError::NotFound)
    ));
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_placement_bumps_version() {
    let (node, svc, _d) = solo().await;
    let m1 = svc
        .deploy_wasm("web", Bytes::from(fixture("echo")), DeployOpts::default())
        .await
        .unwrap();
    let tags = Placement::Tags(vec!["a".into()]);
    let m2 = svc.set_placement("web", tags.clone()).await.unwrap();
    assert_eq!((m2.version, &m2.placement), (2, &tags));
    let m3 = svc
        .set_outbound("web", vec!["example.com:443".into()])
        .await
        .unwrap();
    assert_eq!(m3.version, 3);
    assert_eq!(m3.placement, tags);
    let lim = Limits {
        memory_mb: 128,
        ..Limits::default()
    };
    let m4 = svc.set_limits("web", lim).await.unwrap();
    assert_eq!((m4.version, m4.limits), (4, lim));
    assert_eq!(m4.outbound, m3.outbound);
    assert_eq!(m4.kind, m1.kind);
    assert_eq!(manifest(&node, "web").await, Some(m4));
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undeploy_deletes_manifest_keeps_data() {
    let (node, svc, _d) = solo().await;
    svc.deploy_wasm("web", Bytes::from(fixture("echo")), DeployOpts::default())
        .await
        .unwrap();
    node.store().set("web", "k", b"v".to_vec()).await.unwrap();
    svc.undeploy("web").await.unwrap();
    assert_eq!(manifest(&node, "web").await, None);
    assert_eq!(
        node.store().get("web", "k").await.unwrap(),
        Some(b"v".to_vec())
    );
    assert!(matches!(
        svc.set_placement("web", Placement::AllHosts).await,
        Err(ServicesError::NotFound)
    ));
    // Redeploying starts again at version 1.
    let m = svc
        .deploy_wasm("web", Bytes::from(fixture("echo")), DeployOpts::default())
        .await
        .unwrap();
    assert_eq!(m.version, 1);
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_bumps_generation() {
    let (node, svc, _d) = solo().await;
    let store = node.store();
    assert_eq!(current_generation(store, "web").await.unwrap(), 0);
    assert_eq!(data_ns("web", 0), "web");
    assert_eq!(data_ns("web", 1), "web.1");
    store.set("web", "k", b"v".to_vec()).await.unwrap();

    // Purge is allowed whether or not the service is deployed.
    svc.purge("web").await.unwrap();
    assert_eq!(current_generation(store, "web").await.unwrap(), 1);
    assert_eq!(store.get("web", "k").await.unwrap(), None);
    // The new namespace is empty and writable through the adapter.
    let kv = ServiceKv::new(store.clone(), data_ns("web", 1));
    assert_eq!(kv.get("k").await.unwrap(), None);
    kv.set("k", b"new".to_vec()).await.unwrap();
    assert_eq!(
        store.get("web.1", "k").await.unwrap(),
        Some(b"new".to_vec())
    );

    svc.purge("web").await.unwrap();
    assert_eq!(current_generation(store, "web").await.unwrap(), 2);
    assert_eq!(store.get("web.1", "k").await.unwrap(), None);
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_kv_roundtrip_is_confined_to_its_namespace() {
    let (node, svc, _d) = solo().await;
    let store = node.store().clone();
    let kv = ServiceKv::new(store.clone(), "web".into());
    kv.set("a/1", b"x".to_vec()).await.unwrap();
    kv.set("a/2", b"y".to_vec()).await.unwrap();
    kv.set("b/1", b"z".to_vec()).await.unwrap();
    kv.delete("a/2").await.unwrap();
    assert_eq!(kv.get("a/1").await.unwrap(), Some(b"x".to_vec()));
    assert_eq!(kv.get("a/2").await.unwrap(), None);
    assert_eq!(kv.list_keys("a/", None, 10).await.unwrap(), ["a/1"]);
    assert_eq!(kv.list_keys("", Some("a/1"), 10).await.unwrap(), ["b/1"]);
    kv.set_ordered("o", vec![2], Some(b"two".to_vec()))
        .await
        .unwrap();
    kv.set_ordered("o", vec![1], Some(b"one".to_vec()))
        .await
        .unwrap();
    assert_eq!(kv.get("o").await.unwrap(), Some(b"two".to_vec()));
    for m in [b"m2", b"m1", b"m3"] {
        kv.set_add("s", m.to_vec()).await.unwrap();
    }
    assert!(kv.set_contains("s", b"m1").await.unwrap());
    assert!(!kv.set_contains("s", b"m4").await.unwrap());
    let after: &[u8] = b"m1";
    assert_eq!(
        kv.set_members("s", Some(after), 1).await.unwrap(),
        [b"m2".to_vec()]
    );
    assert_eq!(store.get("web", "a/1").await.unwrap(), Some(b"x".to_vec()));
    assert_eq!(store.get("other", "a/1").await.unwrap(), None);
    svc.shutdown().await;
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_kv_error_mapping() {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(da.path(), "a")).await.unwrap();
    let ticket = a
        .invite(Role::Host, Duration::from_secs(900))
        .await
        .unwrap();
    let b = timeout(T, Node::join(test_cfg(db.path(), "b"), ticket))
        .await
        .expect("join timed out")
        .unwrap();
    assert!(b.wait_first_sync(T).await);
    let kv = ServiceKv::new(b.store().clone(), "web".into());
    kv.set("k", b"v".to_vec()).await.unwrap();

    // b signs itself out, so the removal record is its own. Removing b from a would race:
    // a may close b with `REMOVED` before the record syncs, and b then stays
    // `RefusedAsRemoved` with writes still allowed (node-core spec §3.3), which is not what
    // this test checks.
    b.remove(b.node_id()).await.unwrap();
    wait_until("b shows its own sign-out", T, || {
        matches!(b.status(), NodeStatus::Removed { .. })
    })
    .await;
    assert_eq!(kv.set("k", b"x".to_vec()).await, Err(KvError::Removed));
    assert_eq!(kv.delete("k").await, Err(KvError::Removed));
    assert_eq!(kv.set_add("s", b"m".to_vec()).await, Err(KvError::Removed));
    a.shutdown().await;
    b.shutdown().await;
}
