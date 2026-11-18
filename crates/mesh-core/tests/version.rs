//! The `meshnode/version` probe and the "incompatible version" member status (8b spec §5.4).

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use common::{T, test_cfg, wait_until};
use ed25519_dalek::SigningKey;
use iroh::endpoint::ConnectionError;
use mesh_core::local::{load_or_create_key, read_genesis};
use mesh_core::membership::{MEMBERS, MESH_NS, addr_key, member_bytes};
use mesh_core::transport::{ALPN_VERSION, NOT_MEMBER, addr_to_bytes, build_endpoint};
use mesh_core::{MemberStatus, Node, NodeConfig};
use mesh_proto::{NodeAddrBytes, NodeId, Role};
use tokio::time::timeout;

fn id_of_dir(dir: &Path) -> NodeId {
    NodeId(load_or_create_key(dir).unwrap().verifying_key().to_bytes())
}

fn cfg(dir: &Path, name: &str, product: &str) -> NodeConfig {
    let mut c = test_cfg(dir, name).0;
    c.product_version = product.into();
    c
}

/// A config that advertises (and speaks) `sync/2`, like a newer, incompatible build.
fn sync2_cfg(dir: &Path, name: &str, product: &str) -> NodeConfig {
    let mut c = cfg(dir, name, product);
    c.sync_alpn_override = Some(b"meshnode/sync/2".to_vec());
    c.protocols_override = Some(BTreeMap::from([
        ("sync".to_owned(), 2),
        ("join".to_owned(), 1),
        ("httpfwd".to_owned(), 1),
    ]));
    c
}

async fn own_addr(n: &Node) -> NodeAddrBytes {
    NodeAddrBytes(
        n.store()
            .get(MESH_NS, &addr_key(&n.node_id()))
            .await
            .unwrap()
            .expect("addr register written"),
    )
}

/// Founder `a` and member `b` (added by `a` as a host), each given the other's address.
async fn pair(da: &Path, db: &Path, ca: NodeConfig, cb: NodeConfig) -> (Node, Node) {
    let a = Node::create_mesh(ca).await.unwrap();
    let b_id = id_of_dir(db);
    a.store()
        .set_add(MESH_NS, MEMBERS, member_bytes(&b_id, Role::Host).to_vec())
        .await
        .unwrap();
    let genesis = read_genesis(da).unwrap().unwrap();
    let hints = vec![(a.node_id(), own_addr(&a).await)];
    let b = Node::start_member_for_test(cb, genesis, hints)
        .await
        .unwrap();
    a.add_hint(b_id, addr_to_bytes(&b.endpoint_addr_for_test()));
    (a, b)
}

fn status_of(n: &Node, id: NodeId) -> Option<MemberStatus> {
    n.members().into_iter().find(|m| m.id == id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompatible_peer_is_reported() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(
        da.path(),
        db.path(),
        cfg(da.path(), "a", "0.1.0"),
        sync2_cfg(db.path(), "b", "0.2.0"),
    )
    .await;
    let (a_id, b_id) = (a.node_id(), b.node_id());
    wait_until("A sees B as incompatible", T, || async {
        status_of(&a, b_id).is_some_and(|s| s.incompatible)
    })
    .await;
    let s = status_of(&a, b_id).unwrap();
    assert!(!s.online, "{s:?}");
    let v = s.version.expect("B's version known");
    assert_eq!(v.protocols["sync"], 2);
    assert_eq!(v.product, "0.2.0");
    wait_until("B sees A as incompatible", T, || async {
        status_of(&b, a_id).is_some_and(|s| s.incompatible)
    })
    .await;
    let s = status_of(&b, a_id).unwrap();
    assert!(!s.online, "{s:?}");
    assert_eq!(s.version.expect("A's version known").protocols["sync"], 1);
    timeout(T, a.shutdown()).await.unwrap();
    timeout(T, b.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compatible_peer_has_version() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(
        da.path(),
        db.path(),
        cfg(da.path(), "a", "1.2.3"),
        cfg(db.path(), "b", "1.2.4"),
    )
    .await;
    let (a_id, b_id) = (a.node_id(), b.node_id());
    let known = |n: &Node, id: NodeId, product: &'static str| {
        status_of(n, id)
            .is_some_and(|s| s.online && s.version.as_ref().is_some_and(|v| v.product == product))
    };
    wait_until("A knows B's version", T, || async {
        known(&a, b_id, "1.2.4")
    })
    .await;
    wait_until("B knows A's version", T, || async {
        known(&b, a_id, "1.2.3")
    })
    .await;
    assert!(!status_of(&a, b_id).unwrap().incompatible);
    assert!(!status_of(&b, a_id).unwrap().incompatible);
    timeout(T, a.shutdown()).await.unwrap();
    timeout(T, b.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_probe_refuses_non_member() {
    let (da, dx) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Node::create_mesh(cfg(da.path(), "a", "0.1.0"))
        .await
        .unwrap();
    let outsider = build_endpoint(
        &cfg(dx.path(), "x", "0.1.0"),
        &SigningKey::from_bytes(&[9; 32]),
    )
    .await
    .unwrap();
    let conn = timeout(
        T,
        outsider.connect(a.endpoint_addr_for_test(), ALPN_VERSION),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    // The gate closes before any stream is served.
    let reason = timeout(Duration::from_secs(10), conn.closed())
        .await
        .expect("no close");
    match reason {
        ConnectionError::ApplicationClosed(c) => assert_eq!(c.error_code, NOT_MEMBER),
        other => panic!("unexpected close: {other:?}"),
    }
    outsider.close().await;
    timeout(T, a.shutdown()).await.unwrap();
}
