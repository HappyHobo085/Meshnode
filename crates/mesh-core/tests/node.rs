//! `Node` lifecycle, peer connections and replication between in-process nodes.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{T, converged, test_cfg, wait_event, wait_until};
use mesh_core::local::{load_or_create_key, read_genesis, write_genesis};
use mesh_core::membership::{MEMBERS, MESH_NS, addr_key, member_bytes};
use mesh_core::{Node, NodeError, NodeEvent, NodeStatus};
use mesh_proto::{MeshGenesis, NodeAddrBytes, NodeId, Role};
use tokio::time::timeout;

fn id_of_dir(dir: &Path) -> NodeId {
    NodeId(load_or_create_key(dir).unwrap().verifying_key().to_bytes())
}

async fn own_addr(n: &Node) -> NodeAddrBytes {
    let raw = n
        .store()
        .get(MESH_NS, &addr_key(&n.node_id()))
        .await
        .unwrap()
        .expect("addr register written");
    NodeAddrBytes(raw)
}

fn online(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id && m.online)
}

/// A founder `a` and a member `b` (added by `a` as a host), not yet connected.
async fn pair(da: &Path, db: &Path, hints_for_b: bool) -> (Node, Node) {
    let a = Node::create_mesh(test_cfg(da, "a").0).await.unwrap();
    let b_id = id_of_dir(db);
    a.store()
        .set_add(MESH_NS, MEMBERS, member_bytes(&b_id, Role::Host).to_vec())
        .await
        .unwrap();
    let genesis = read_genesis(da).unwrap().unwrap();
    let hints = if hints_for_b {
        vec![(a.node_id(), own_addr(&a).await)]
    } else {
        Vec::new()
    };
    let b = Node::start_member_for_test(test_cfg(db, "b").0, genesis, hints)
        .await
        .unwrap();
    assert_eq!(b.node_id(), b_id);
    (a, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_mesh_writes_genesis_and_self_member() {
    let d = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(d.path(), "a").0).await.unwrap();
    let g = read_genesis(d.path()).unwrap().expect("mesh.json written");
    assert_eq!(g.founder, a.node_id());
    assert_eq!(g.mesh_id(), a.mesh());
    assert_eq!(a.status(), NodeStatus::Running);

    let me = a.members();
    assert_eq!(me.len(), 1);
    assert_eq!(me[0].id, a.node_id());
    assert_eq!(me[0].role, Role::Host);
    assert!(me[0].effective);
    let self_member = a
        .store()
        .call({
            let m = member_bytes(&a.node_id(), Role::Host);
            move |s| s.set_contains(MESH_NS, MEMBERS, &m)
        })
        .await
        .unwrap();
    assert!(self_member, "founder's members record written");
    let addr = own_addr(&a).await;
    let decoded = mesh_core::transport::addr_from_bytes(&addr).unwrap();
    assert_eq!(mesh_core::transport::node_id_of(decoded.id), a.node_id());
    wait_until("own info in members()", T, || async {
        a.members()[0].info.as_ref().is_some_and(|i| i.name == "a")
    })
    .await;
    timeout(T, a.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_after_shutdown_resumes() {
    let d = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(d.path(), "a").0).await.unwrap();
    let (id, mesh) = (a.node_id(), a.mesh());
    a.store().set("app", "k", b"v".to_vec()).await.unwrap();
    timeout(T, a.shutdown()).await.unwrap();

    let a = Node::open(test_cfg(d.path(), "a").0).await.unwrap();
    assert_eq!(a.node_id(), id);
    assert_eq!(a.mesh(), mesh);
    assert_eq!(a.status(), NodeStatus::Running);
    assert_eq!(
        a.store().get("app", "k").await.unwrap(),
        Some(b"v".to_vec())
    );
    assert!(a.members().iter().any(|m| m.id == id && m.effective));
    timeout(T, a.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_without_mesh_is_not_initialised() {
    let d = tempfile::tempdir().unwrap();
    let r = Node::open(test_cfg(d.path(), "a").0).await;
    assert!(matches!(r, Err(NodeError::NotInitialised)), "{:?}", r.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_twice_is_already_in_mesh() {
    let d = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(d.path(), "a").0).await.unwrap();
    timeout(T, a.shutdown()).await.unwrap();
    let r = Node::create_mesh(test_cfg(d.path(), "a").0).await;
    assert!(matches!(r, Err(NodeError::AlreadyInMesh)), "{:?}", r.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_connect_and_replicate() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Node::create_mesh(test_cfg(da.path(), "a").0).await.unwrap();
    let mut ea = a.subscribe();
    let b_id = id_of_dir(db.path());
    a.store()
        .set_add(MESH_NS, MEMBERS, member_bytes(&b_id, Role::Host).to_vec())
        .await
        .unwrap();
    let genesis = read_genesis(da.path()).unwrap().unwrap();
    let b = Node::start_member_for_test(
        test_cfg(db.path(), "b").0,
        genesis,
        vec![(a.node_id(), own_addr(&a).await)],
    )
    .await
    .unwrap();

    wait_event(&mut ea, |e| *e == NodeEvent::PeerUp(b_id), T).await;
    wait_until("b sees a online", T, || async { online(&b, a.node_id()) }).await;
    wait_event(&mut ea, |e| *e == NodeEvent::FirstSyncComplete, T).await;

    a.store().set("app", "k", b"v".to_vec()).await.unwrap();
    wait_until("key replicated to b", T, || async {
        b.store().get("app", "k").await.unwrap() == Some(b"v".to_vec())
    })
    .await;
    converged(&[&a, &b], T).await;

    for n in [&a, &b] {
        wait_until("both members online on both", T, || async {
            let ms = n.members();
            ms.len() == 2 && ms.iter().all(|m| m.online && m.effective)
        })
        .await;
    }
    timeout(T, b.shutdown()).await.unwrap();
    timeout(T, a.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_dial_keeps_one_connection() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path(), false).await;
    let (mut ea, mut eb) = (a.subscribe(), b.subscribe());
    let (addr_a, addr_b) = (own_addr(&a).await, own_addr(&b).await);

    // Both learn the other's address at once, so both dial at once.
    tokio::join!(async { a.add_hint(b.node_id(), addr_b) }, async {
        b.add_hint(a.node_id(), addr_a)
    },);
    wait_event(&mut ea, |e| *e == NodeEvent::PeerUp(b.node_id()), T).await;
    wait_event(&mut eb, |e| *e == NodeEvent::PeerUp(a.node_id()), T).await;

    wait_until("one connection per side", T, || async {
        a.connection_count() == 1 && b.connection_count() == 1
    })
    .await;
    // The surviving connection carries replication both ways.
    a.store().set("app", "from_a", b"1".to_vec()).await.unwrap();
    b.store().set("app", "from_b", b"2".to_vec()).await.unwrap();
    converged(&[&a, &b], T).await;
    assert_eq!(a.connection_count(), 1);
    assert_eq!(b.connection_count(), 1);
    assert!(online(&a, b.node_id()) && online(&b, a.node_id()));

    timeout(T, b.shutdown()).await.unwrap();
    timeout(T, a.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_open_of_same_dir_errors() {
    let d = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(d.path(), "a").0).await.unwrap();
    let r = Node::open(test_cfg(d.path(), "a").0).await;
    assert!(r.is_err(), "a second open of a live data dir must fail");
    // The failed open left the running node intact.
    a.store().set("app", "k", b"v".to_vec()).await.unwrap();
    timeout(T, a.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_with_peers_is_prompt() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path(), true).await;
    let a_id = a.node_id();
    wait_until("connected", T, || async {
        online(&a, b.node_id()) && online(&b, a_id)
    })
    .await;
    let mut eb = b.subscribe();

    timeout(Duration::from_secs(5), a.shutdown())
        .await
        .expect("shutdown with a connected peer took over 5 s");
    wait_event(&mut eb, |e| *e == NodeEvent::PeerDown(a_id), T).await;

    let a = Node::open(test_cfg(da.path(), "a").0).await.unwrap();
    assert_eq!(a.node_id(), a_id);
    timeout(T, a.shutdown()).await.unwrap();
    timeout(T, b.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_mesh_refuses_start() {
    let d = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(d.path(), "a").0).await.unwrap();
    let founder = a.node_id();
    timeout(T, a.shutdown()).await.unwrap();

    // Same founder key, different nonce: another mesh.
    write_genesis(
        d.path(),
        &MeshGenesis {
            founder,
            nonce: [7; 32],
        },
    )
    .unwrap();
    let r = Node::open(test_cfg(d.path(), "a").0).await;
    assert!(matches!(r, Err(NodeError::WrongMesh)), "{:?}", r.err());
}
