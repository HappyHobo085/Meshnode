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
    let mut events = a.subscribe();
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

    // A founder-only mesh has a host: the reactor never reports `NoEffectiveHost`, neither
    // for the founding writes nor for a later `_mesh` change.
    a.set_info(None, Some(vec!["t".into()])).await.unwrap();
    let mut seen = Vec::new();
    let has_tag = |a: &Node| {
        a.members()[0]
            .info
            .as_ref()
            .is_some_and(|i| i.tags == ["t"])
    };
    while !has_tag(&a) {
        // Record every event, not only the one waited for.
        let pred = |e: &NodeEvent| {
            seen.push(e.clone());
            *e == NodeEvent::MembersChanged
        };
        wait_event(&mut events, pred, T).await;
    }
    while let Ok(e) = events.try_recv() {
        seen.push(e);
    }
    assert!(!seen.contains(&NodeEvent::NoEffectiveHost), "{seen:?}");
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

/// Runs a member node on its own runtime in its own thread, so that dropping the runtime
/// kills it like a process: every task (iroh's connection drivers included) stops at once
/// and nothing more reaches the wire, not even a stream FIN or a connection close.
struct CrashableNode {
    id: NodeId,
    addr: NodeAddrBytes,
    ready: std::sync::mpsc::Receiver<()>,
    crash: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl CrashableNode {
    /// Starts the member in `dir`; it becomes ready once connected to `peer` and holding
    /// `peer`'s address register.
    fn start(dir: std::path::PathBuf, genesis: MeshGenesis, peer: NodeId) -> CrashableNode {
        let (info_tx, info_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready) = std::sync::mpsc::channel::<()>();
        let (crash, crash_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let node = rt.block_on(async {
                let cfg = test_cfg(&dir, "crasher").0;
                let n = Node::start_member_for_test(cfg, genesis, vec![])
                    .await
                    .unwrap();
                info_tx.send((n.node_id(), own_addr(&n).await)).unwrap();
                wait_until("crasher connected with the peer's address", T, || async {
                    online(&n, peer)
                        && n.store()
                            .get(MESH_NS, &addr_key(&peer))
                            .await
                            .unwrap()
                            .is_some()
                })
                .await;
                n
            });
            ready_tx.send(()).unwrap();
            let _ = crash_rx.recv();
            // Kill every task first; only then let the node's handles go.
            drop(rt);
            drop(node);
        });
        let (id, addr) = info_rx.recv_timeout(T).expect("crasher did not start");
        CrashableNode {
            id,
            addr,
            ready,
            crash,
            thread,
        }
    }

    fn wait_ready(&self) {
        tokio::task::block_in_place(|| self.ready.recv_timeout(T).expect("crasher not ready"));
    }

    fn crash(self) {
        self.crash.send(()).unwrap();
        tokio::task::block_in_place(|| self.thread.join().expect("crasher thread panicked"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashed_peer_reconnects_quickly() {
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // The survivor has the lower id and dials; the higher id crashes. The survivor is then
    // left holding a dead connection that it dialled itself (spec §6.1 duplicate rule).
    let (sdir, cdir) = if id_of_dir(d1.path()) < id_of_dir(d2.path()) {
        (d1.path(), d2.path())
    } else {
        (d2.path(), d1.path())
    };
    let survivor = Node::create_mesh(test_cfg(sdir, "survivor").0)
        .await
        .unwrap();
    let crasher_id = id_of_dir(cdir);
    assert!(survivor.node_id() < crasher_id);
    survivor
        .store()
        .set_add(
            MESH_NS,
            MEMBERS,
            member_bytes(&crasher_id, Role::Host).to_vec(),
        )
        .await
        .unwrap();
    let genesis = read_genesis(sdir).unwrap().unwrap();
    let crasher = CrashableNode::start(cdir.to_path_buf(), genesis, survivor.node_id());
    assert_eq!(crasher.id, crasher_id);
    survivor.add_hint(crasher.id, crasher.addr.clone());
    crasher.wait_ready();
    wait_until("survivor sees the crasher", T, || async {
        online(&survivor, crasher_id)
    })
    .await;

    crasher.crash();
    let restarted = Node::open(test_cfg(cdir, "restarted").0).await.unwrap();
    assert_eq!(restarted.node_id(), crasher_id);

    let quick = Duration::from_secs(5);
    survivor
        .store()
        .set("app", "after_crash", b"1".to_vec())
        .await
        .unwrap();
    wait_until(
        "survivor's write reaches the restarted node",
        quick,
        || async {
            restarted.store().get("app", "after_crash").await.unwrap() == Some(b"1".to_vec())
        },
    )
    .await;
    wait_until("one live connection on the survivor", quick, || async {
        online(&survivor, crasher_id) && survivor.connection_count() == 1
    })
    .await;

    timeout(T, restarted.shutdown()).await.unwrap();
    timeout(T, survivor.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_handle_drop_does_not_cancel() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path(), true).await;
    let h = a.handle();
    assert_eq!(h.node_id(), a.node_id());
    assert_eq!(h.mesh(), a.mesh());
    assert_eq!(h.status(), NodeStatus::Running);
    assert_eq!(h.members().len(), a.members().len());
    let copy = h.clone();
    drop(h);
    drop(copy);
    assert!(!a.is_stopped());
    // The node still replicates.
    a.store().set("app", "k", b"v".to_vec()).await.unwrap();
    wait_until("b sees the write", T, || async {
        b.store().get("app", "k").await.unwrap().as_deref() == Some(&b"v"[..])
    })
    .await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_handle_cancelled_resolves_on_shutdown() {
    let d = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(d.path(), "a").0).await.unwrap();
    let h = a.handle();
    let mut cancelled = Box::pin(h.cancelled());
    assert!(
        timeout(Duration::from_millis(200), &mut cancelled)
            .await
            .is_err(),
        "not cancelled while the node runs"
    );
    a.shutdown().await;
    timeout(T, cancelled)
        .await
        .expect("cancelled never resolved");
    // Also after the fact, and for a clone.
    timeout(T, h.clone().cancelled()).await.unwrap();
}
