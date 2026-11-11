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

// ---------------------------------------------------------------- extra protocols

mod extra {
    use std::sync::Arc;

    use super::*;
    use mesh_core::transport::NOT_MEMBER;
    use mesh_core::{AcceptError, Connection, DynProtocolHandler, ProtocolHandler};

    const ECHO: &[u8] = b"test/echo";

    #[derive(Debug)]
    struct Echo;

    impl ProtocolHandler for Echo {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            let (mut s, mut r) = conn.accept_bi().await.map_err(AcceptError::from_err)?;
            let msg = r.read_to_end(1024).await.map_err(AcceptError::from_err)?;
            s.write_all(&msg).await.map_err(AcceptError::from_err)?;
            s.finish().map_err(AcceptError::from_err)?;
            conn.closed().await;
            Ok(())
        }
    }

    async fn echo_roundtrip(conn: &Connection, msg: &[u8]) -> Vec<u8> {
        let (mut s, mut r) = conn.open_bi().await.unwrap();
        s.write_all(msg).await.unwrap();
        s.finish().unwrap();
        r.read_to_end(1024).await.unwrap()
    }

    /// `a` (founder, serves `test/echo`) and `b` (member with a hint for `a`), both online.
    async fn echo_pair(da: &Path, db: &Path) -> (Node, Node) {
        let mut ca = test_cfg(da, "a").0;
        let handler: Arc<dyn DynProtocolHandler> = Arc::new(Echo);
        ca.extra_protocols.push((ECHO.to_vec(), handler));
        let a = Node::create_mesh(ca).await.unwrap();
        let b_id = id_of_dir(db);
        a.store()
            .set_add(MESH_NS, MEMBERS, member_bytes(&b_id, Role::Host).to_vec())
            .await
            .unwrap();
        let genesis = read_genesis(da).unwrap().unwrap();
        let hints = vec![(a.node_id(), own_addr(&a).await)];
        let b = Node::start_member_for_test(test_cfg(db, "b").0, genesis, hints)
            .await
            .unwrap();
        let (aid, bid) = (a.node_id(), b.node_id());
        wait_until("a and b online", T, || async {
            online(&a, bid) && online(&b, aid)
        })
        .await;
        (a, b)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn node_handle_connect_reaches_member() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = echo_pair(da.path(), db.path()).await;
        let conn = b.handle().connect(a.node_id(), ECHO).await.unwrap();
        assert_eq!(echo_roundtrip(&conn, b"hello").await, b"hello");
        timeout(T, a.shutdown()).await.unwrap();
        timeout(T, b.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn extra_protocols_are_gated() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = echo_pair(da.path(), db.path()).await;
        // A member is served.
        let conn = b.handle().connect(a.node_id(), ECHO).await.unwrap();
        assert_eq!(echo_roundtrip(&conn, b"ok").await, b"ok");

        // A bare endpoint that is no member is closed with NOT_MEMBER.
        let dx = tempfile::tempdir().unwrap();
        let mut cx = mesh_core::NodeConfig::new(dx.path(), "x");
        cx.relay = mesh_core::RelayChoice::Disabled;
        cx.discovery = false;
        cx.bind_addr = Some("127.0.0.1:0".parse().unwrap());
        let key = ed25519_dalek::SigningKey::from_bytes(&[77; 32]);
        let ep = mesh_core::transport::build_endpoint(&cx, &key)
            .await
            .unwrap();
        let conn = ep.connect(a.endpoint_addr_for_test(), ECHO).await.unwrap();
        let err = timeout(T, conn.closed()).await.unwrap();
        match err {
            iroh::endpoint::ConnectionError::ApplicationClosed(c) => {
                assert_eq!(c.error_code, NOT_MEMBER)
            }
            other => panic!("unexpected close: {other:?}"),
        }
        ep.close().await;
        timeout(T, a.shutdown()).await.unwrap();
        timeout(T, b.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn connect_to_non_member_refused() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = echo_pair(da.path(), db.path()).await;
        let stranger = NodeId([5; 32]);
        let r = a.handle().connect(stranger, ECHO).await;
        assert!(matches!(r, Err(NodeError::NotEffective)), "{:?}", r.err());
        // A removed member is refused too.
        let bid = b.node_id();
        a.remove(bid).await.unwrap();
        wait_until("b no longer effective on a", T, || async {
            a.members().iter().any(|m| m.id == bid && !m.effective)
        })
        .await;
        let r = a.handle().connect(bid, ECHO).await;
        assert!(matches!(r, Err(NodeError::NotEffective)), "{:?}", r.err());
        timeout(T, a.shutdown()).await.unwrap();
        timeout(T, b.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn colliding_extra_alpns_are_rejected() {
        let cases: [(&str, Vec<&[u8]>); 4] = [
            ("sync", vec![b"meshnode/sync/1"]),
            ("join", vec![b"meshnode/join/1"]),
            ("blobs", vec![iroh_blobs::ALPN]),
            ("extra", vec![b"test/echo", b"test/echo"]),
        ];
        for (name, alpns) in cases {
            let d = tempfile::tempdir().unwrap();
            let mut cfg = test_cfg(d.path(), "a").0;
            for alpn in alpns {
                let handler: Arc<dyn DynProtocolHandler> = Arc::new(Echo);
                cfg.extra_protocols.push((alpn.to_vec(), handler));
            }
            let r = Node::create_mesh(cfg).await;
            assert!(
                matches!(r, Err(NodeError::ProtocolConflict(_))),
                "{name}: {:?}",
                r.err()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn connect_to_self_refused() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = echo_pair(da.path(), db.path()).await;
        let r = a.handle().connect(a.node_id(), ECHO).await;
        assert!(matches!(r, Err(NodeError::ConnectToSelf)), "{:?}", r.err());
        timeout(T, a.shutdown()).await.unwrap();
        timeout(T, b.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn extra_connections_close_on_removal() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // Both nodes serve the echo protocol.
        let mut ca = test_cfg(da.path(), "a").0;
        let mut cb = test_cfg(db.path(), "b").0;
        for c in [&mut ca, &mut cb] {
            let handler: Arc<dyn DynProtocolHandler> = Arc::new(Echo);
            c.extra_protocols.push((ECHO.to_vec(), handler));
        }
        let a = Node::create_mesh(ca).await.unwrap();
        let b_id = id_of_dir(db.path());
        a.store()
            .set_add(MESH_NS, MEMBERS, member_bytes(&b_id, Role::Host).to_vec())
            .await
            .unwrap();
        let genesis = read_genesis(da.path()).unwrap().unwrap();
        let hints = vec![(a.node_id(), own_addr(&a).await)];
        let b = Node::start_member_for_test(cb, genesis, hints)
            .await
            .unwrap();
        let (aid, bid) = (a.node_id(), b.node_id());
        wait_until("a and b online", T, || async {
            online(&a, bid) && online(&b, aid)
        })
        .await;

        // a only knows b through the address b published; give it that address now.
        a.add_hint(bid, own_addr(&b).await);
        // a learns b's address from b's record only after a sync; hand it over now.
        a.add_hint(bid, own_addr(&b).await);
        // b -> a and a -> b, one connection each way, both used once.
        let b_to_a = b.handle().connect(aid, ECHO).await.unwrap();
        assert_eq!(echo_roundtrip(&b_to_a, b"x").await, b"x");
        let a_to_b = a.handle().connect(bid, ECHO).await.unwrap();
        assert_eq!(echo_roundtrip(&a_to_b, b"y").await, b"y");

        a.remove(bid).await.unwrap();
        // a closed both with REMOVED: b sees the code on the connection it dialled, a sees
        // its own close on the one it dialled.
        let err = timeout(T, b_to_a.closed()).await.expect("b->a stayed open");
        match err {
            iroh::endpoint::ConnectionError::ApplicationClosed(c) => {
                assert_eq!(c.error_code, mesh_core::transport::REMOVED)
            }
            other => panic!("b->a: unexpected close {other:?}"),
        }
        let err = timeout(T, a_to_b.closed()).await.expect("a->b stayed open");
        assert!(
            matches!(err, iroh::endpoint::ConnectionError::LocallyClosed),
            "a->b: {err:?}"
        );
        timeout(T, a.shutdown()).await.unwrap();
        timeout(T, b.shutdown()).await.unwrap();
    }
}

// ---------------------------------------------------------------- catch-up and wake

mod catch_up {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    use mesh_core::{CatchUpHook, NodeHandle, Timings};
    use mesh_hlc::ManualClock;
    use tokio::sync::broadcast;

    const MINUTE: u64 = 60_000;

    /// Connected `a` and `b` with only the incoming-traffic catch-up able to start a round
    /// once the first one is over: the periodic round and the session's own silence rule
    /// are an hour away. Returns `b`'s wall clock.
    async fn quiet_pair(
        da: &Path,
        db: &Path,
        after: Option<Duration>,
    ) -> (Node, Node, ManualClock) {
        quiet_pair_with(da, db, after, None).await
    }

    /// [`quiet_pair`] with `b`'s catch-up hook.
    async fn quiet_pair_with(
        da: &Path,
        db: &Path,
        after: Option<Duration>,
        hook_b: Option<CatchUpHook>,
    ) -> (Node, Node, ManualClock) {
        let hour = Duration::from_secs(3600);
        let timings = Timings {
            sync_interval: hour,
            silence: hour,
            ..Timings::fast_for_tests()
        };
        let mut ca = test_cfg(da, "a").0;
        ca.timings = timings.clone();
        ca.catch_up_on_incoming_after = after;
        let (mut cb, clock_b) = test_cfg(db, "b");
        cb.timings = timings;
        cb.catch_up_on_incoming_after = after;
        cb.on_catch_up = hook_b;

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
        assert!(a.wait_first_sync(T).await && b.wait_first_sync(T).await);
        wait_until("connected", T, || async {
            online(&a, b_id) && online(&b, a.node_id())
        })
        .await;
        converged(&[&a, &b], T).await;
        (a, b, clock_b)
    }

    /// Whether a `RoundComplete` arrives on `rx` within `within`.
    async fn round_within(rx: &mut broadcast::Receiver<NodeEvent>, within: Duration) -> bool {
        let fut = async {
            loop {
                match rx.recv().await {
                    Ok(NodeEvent::RoundComplete(_)) => return,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
                }
            }
        };
        timeout(within, fut).await.is_ok()
    }

    /// `a` writes a key and `b` receives it (inbound `sync/1` traffic on `b`).
    async fn traffic_to_b(a: &Node, b: &Node, key: &str) {
        a.store().set("app", key, b"v".to_vec()).await.unwrap();
        wait_until("key replicated to b", T, || async {
            b.store().get("app", key).await.unwrap().is_some()
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn incoming_after_silence_triggers_catch_up() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b, clock_b) = quiet_pair(da.path(), db.path(), Some(Duration::from_secs(60))).await;
        let mut eb = b.subscribe();
        clock_b.advance(MINUTE + 1_000);
        a.store().set("app", "k", b"v".to_vec()).await.unwrap();
        assert!(
            round_within(&mut eb, Duration::from_secs(10)).await,
            "no catch-up round after 61 s of silence"
        );
        timeout(T, b.shutdown()).await.unwrap();
        timeout(T, a.shutdown()).await.unwrap();
    }

    /// Records the start of each catch-up, the drop of the guard its hook returned, and
    /// how many rounds `b` had completed with `a` at that drop.
    #[derive(Clone, Default)]
    struct Probe {
        log: Arc<StdMutex<Vec<&'static str>>>,
        /// `b`, once it exists. Taken out before shutdown: the hook lives inside `b`.
        b: Arc<StdMutex<Option<NodeHandle>>>,
        rounds_at_end: Arc<StdMutex<Vec<u64>>>,
    }

    impl Probe {
        fn hook(&self, a: NodeId) -> CatchUpHook {
            let p = self.clone();
            Arc::new(move || {
                p.log.lock().unwrap().push("start");
                Box::new(Phase(p.clone(), a))
            })
        }

        fn log(&self) -> Vec<&'static str> {
            self.log.lock().unwrap().clone()
        }

        fn rounds_with(&self, a: NodeId) -> u64 {
            self.b
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .rounds_with_for_test(a)
        }
    }

    struct Phase(Probe, NodeId);

    impl Drop for Phase {
        fn drop(&mut self) {
            if let Some(b) = self.0.b.lock().unwrap().as_ref() {
                let n = b.rounds_with_for_test(self.1);
                self.0.rounds_at_end.lock().unwrap().push(n);
            }
            self.0.log.lock().unwrap().push("end");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn incoming_catch_up_holds_the_hook_guard_until_rounds_complete() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a_id = id_of_dir(da.path());
        let probe = Probe::default();
        let (a, b, clock_b) = quiet_pair_with(
            da.path(),
            db.path(),
            Some(Duration::from_secs(60)),
            Some(probe.hook(a_id)),
        )
        .await;
        assert!(probe.log().is_empty(), "{:?}", probe.log());
        *probe.b.lock().unwrap() = Some(b.handle());
        let before = probe.rounds_with(a_id);
        let mut eb = b.subscribe();
        clock_b.advance(MINUTE + 1_000);
        a.store().set("app", "k", b"v".to_vec()).await.unwrap();
        assert!(
            round_within(&mut eb, Duration::from_secs(10)).await,
            "no catch-up round after 61 s of silence"
        );
        wait_until("the catch-up guard dropped", T, || async {
            probe.log().len() == 2
        })
        .await;
        assert_eq!(probe.log(), ["start", "end"]);
        // Held until the round, not dropped at once.
        let at_end = probe.rounds_at_end.lock().unwrap().clone();
        assert!(
            at_end[0] > before,
            "guard dropped before a round: {at_end:?} vs {before}"
        );
        probe.b.lock().unwrap().take();
        timeout(T, b.shutdown()).await.unwrap();
        timeout(T, a.shutdown()).await.unwrap();
    }

    /// The other trigger: after the link died, the peer redials the silent node. That peer
    /// has no session yet when the connection is accepted; the guard must still be held
    /// until its first round.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn incoming_connection_after_silence_holds_the_guard_until_its_first_round() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a_id = id_of_dir(da.path());
        let probe = Probe::default();
        let (a, b, clock_b) = quiet_pair_with(
            da.path(),
            db.path(),
            Some(Duration::from_secs(60)),
            Some(probe.hook(a_id)),
        )
        .await;
        let b_id = b.node_id();
        // `a` drops the link and refuses `b`'s redials: nothing reaches `b`.
        a.partition(&[b_id]);
        wait_until("link down", T, || async {
            !online(&a, b_id) && !online(&b, a_id)
        })
        .await;
        *probe.b.lock().unwrap() = Some(b.handle());
        let before = probe.rounds_with(a_id);
        clock_b.advance(MINUTE + 1_000);
        // `b`'s redial backoff reaches its 1 s cap, so `a` dials first after the heal.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(probe.log().is_empty(), "{:?}", probe.log());
        a.heal();
        wait_until("the catch-up guard dropped", T, || async {
            probe.log().len() == 2
        })
        .await;
        assert_eq!(probe.log(), ["start", "end"]);
        let at_end = probe.rounds_at_end.lock().unwrap().clone();
        assert!(
            at_end[0] > before,
            "guard dropped before the redialling peer's first round: {at_end:?} vs {before}"
        );
        probe.b.lock().unwrap().take();
        timeout(T, b.shutdown()).await.unwrap();
        timeout(T, a.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn incoming_within_silence_does_not() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b, clock_b) = quiet_pair(da.path(), db.path(), Some(Duration::from_secs(60))).await;
        let mut eb = b.subscribe();
        clock_b.advance(MINUTE / 2);
        traffic_to_b(&a, &b, "k").await;
        assert!(
            !round_within(&mut eb, Duration::from_secs(2)).await,
            "a round after only 30 s of silence"
        );
        timeout(T, b.shutdown()).await.unwrap();
        timeout(T, a.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn catch_up_toggle_off() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b, clock_b) = quiet_pair(da.path(), db.path(), Some(Duration::from_secs(60))).await;
        b.set_catch_up_on_incoming(None);
        let mut eb = b.subscribe();
        clock_b.advance(MINUTE + 1_000);
        traffic_to_b(&a, &b, "k").await;
        assert!(
            !round_within(&mut eb, Duration::from_secs(2)).await,
            "a catch-up round although it was turned off"
        );
        timeout(T, b.shutdown()).await.unwrap();
        timeout(T, a.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wake_completes_rounds_with_connected_peers() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // No periodic round and no catch-up: only `wake` can start one.
        let (a, b, _) = quiet_pair(da.path(), db.path(), None).await;
        let out = timeout(T, b.wake(Duration::from_secs(10))).await.unwrap();
        assert_eq!(out.peers, 1, "{out:?}");
        assert_eq!(out.rounds_completed, 1, "{out:?}");
        assert!(!out.timed_out, "{out:?}");
        timeout(T, b.shutdown()).await.unwrap();
        timeout(T, a.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wake_times_out_when_rounds_cannot_finish_in_time() {
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b, _) = quiet_pair(da.path(), db.path(), None).await;
        // No round is in progress (quiet timings) and none can cross the network in a
        // zero limit, while the peer stays connected: wake must give up, not wait.
        let out = timeout(Duration::from_secs(1), b.wake(Duration::ZERO))
            .await
            .expect("wake waited past its limit");
        assert_eq!(out.peers, 1, "{out:?}");
        assert_eq!(out.rounds_completed, 0, "{out:?}");
        assert!(out.timed_out, "{out:?}");
        // A huge limit does not overflow, and the round completes.
        let out = timeout(T, b.wake(Duration::MAX)).await.unwrap();
        assert!(!out.timed_out, "{out:?}");
        timeout(T, b.shutdown()).await.unwrap();
        timeout(T, a.shutdown()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wake_with_no_peers_returns_immediately() {
        let d = tempfile::tempdir().unwrap();
        let a = Node::create_mesh(test_cfg(d.path(), "a").0).await.unwrap();
        let out = timeout(Duration::from_secs(1), a.wake(Duration::from_secs(10)))
            .await
            .expect("wake without peers waited");
        assert_eq!(out.peers, 0, "{out:?}");
        assert_eq!(out.rounds_completed, 0, "{out:?}");
        assert!(!out.timed_out, "{out:?}");
        timeout(T, a.shutdown()).await.unwrap();
    }
}
