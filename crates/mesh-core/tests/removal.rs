//! Member removal, learning of one's own removal, and the removal-aware gate (spec §4, §3.3).

mod common;

use std::path::Path;
use std::time::Duration;

use common::{T, converged, test_cfg, wait_event, wait_until};
use mesh_core::{Node, NodeError, NodeEvent, NodeStatus};
use mesh_proto::{NodeId, RefuseReason, Role};
use tempfile::TempDir;
use tokio::time::timeout;

const TTL: Duration = Duration::from_secs(900);
const NS: &str = "app";

fn online(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id && m.online)
}

fn effective(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id && m.effective)
}

async fn val(n: &Node, key: &str) -> Option<Vec<u8>> {
    n.store().get(NS, key).await.unwrap()
}

async fn put(n: &Node, key: &str, v: &str) {
    n.store().set(NS, key, v.as_bytes().to_vec()).await.unwrap();
}

async fn invited(inviter: &Node, dir: &Path, name: &str, role: Role) -> Node {
    let ticket = inviter.invite(role, TTL).await.unwrap();
    let n = timeout(T, Node::join(test_cfg(dir, name).0, ticket))
        .await
        .expect("join timed out")
        .unwrap();
    assert!(n.wait_first_sync(T).await, "first sync");
    n
}

struct Mesh3 {
    a: Node,
    b: Node,
    c: Node,
    dirs: Vec<TempDir>,
}

/// Three nodes joined through invites (`a` founds; `b` is a host, `c` has `role_c`), all
/// connected to each other and converged.
async fn mesh3(role_c: Role) -> Mesh3 {
    let dirs: Vec<TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = Node::create_mesh(test_cfg(dirs[0].path(), "a").0)
        .await
        .unwrap();
    let b = invited(&a, dirs[1].path(), "b", Role::Host).await;
    let c = invited(&a, dirs[2].path(), "c", role_c).await;
    let ids = [a.node_id(), b.node_id(), c.node_id()];
    for n in [&a, &b, &c] {
        wait_until("full mesh", T, || async {
            ids.iter().all(|id| *id == n.node_id() || online(n, *id)) && n.members().len() == 3
        })
        .await;
    }
    converged(&[&a, &b, &c], T).await;
    Mesh3 { a, b, c, dirs }
}

async fn shutdown_all(nodes: Vec<Node>) {
    for n in nodes {
        n.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removal_closes_and_refuses() {
    let Mesh3 { a, b, c, dirs: _d } = mesh3(Role::Host).await;
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());
    put(&c, "k", "c1").await;
    converged(&[&a, &b, &c], T).await;

    // C writes again while cut off from everyone, then B removes it.
    a.partition(&[ic]);
    b.partition(&[ic]);
    c.partition(&[ia, ib]);
    wait_until("C isolated", T, || async {
        !online(&a, ic) && !online(&b, ic) && !online(&c, ia) && !online(&c, ib)
    })
    .await;
    put(&c, "k", "c2").await;
    let mut c_events = c.subscribe();
    b.remove(ic).await.unwrap();
    wait_until("A and B drop C", T, || async {
        !effective(&a, ic) && !effective(&b, ic)
    })
    .await;

    a.heal();
    b.heal();
    c.heal();
    wait_until("C refused", T, || async {
        matches!(c.status(), NodeStatus::RefusedAsRemoved { ref by } if !by.is_empty())
    })
    .await;
    wait_event(
        &mut c_events,
        |e| matches!(e, NodeEvent::RefusedAsRemoved(_)),
        T,
    )
    .await;
    assert!(!online(&a, ic) && !online(&b, ic));
    assert_eq!(val(&a, "k").await.as_deref(), Some(&b"c1"[..]));
    assert_eq!(val(&b, "k").await.as_deref(), Some(&b"c1"[..]));
    // C is only refused, not stopped: its own writes still work.
    put(&c, "other", "x").await;
    shutdown_all(vec![a, b, c]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_cutoff_write_retracted() {
    let Mesh3 { a, b, c, dirs: _d } = mesh3(Role::Host).await;
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());
    put(&c, "k", "c1").await;
    converged(&[&a, &b, &c], T).await;

    // B is cut off from A and C. C's next write reaches only A; B's cutoff for C excludes it.
    b.partition(&[ia, ic]);
    a.partition(&[ib]);
    c.partition(&[ib]);
    wait_until("B isolated", T, || async {
        !online(&b, ia) && !online(&b, ic) && !online(&a, ib) && !online(&c, ib)
    })
    .await;
    b.remove(ic).await.unwrap();
    put(&c, "k", "c2").await;
    wait_until("A holds c2", T, || async {
        val(&a, "k").await.as_deref() == Some(&b"c2"[..])
    })
    .await;

    a.heal();
    b.heal();
    c.heal();
    wait_until("k retracted to c1 on A and B", T, || async {
        !effective(&a, ic)
            && val(&a, "k").await.as_deref() == Some(&b"c1"[..])
            && val(&b, "k").await.as_deref() == Some(&b"c1"[..])
    })
    .await;
    shutdown_all(vec![a, b, c]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_node_cannot_rejoin() {
    let Mesh3 { a, b, c, dirs } = mesh3(Role::Host).await;
    let ic = c.node_id();
    b.remove(ic).await.unwrap();
    wait_until("A drops C", T, || async { !effective(&a, ic) }).await;
    c.shutdown().await;

    // A new data directory with C's key: the same node id asks for a fresh ticket.
    let fresh = tempfile::tempdir().unwrap();
    std::fs::copy(
        dirs[2].path().join("node.key"),
        fresh.path().join("node.key"),
    )
    .unwrap();
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    let r = timeout(T, Node::join(test_cfg(fresh.path(), "c").0, ticket))
        .await
        .expect("join timed out");
    assert!(matches!(
        r,
        Err(NodeError::JoinRefused(RefuseReason::Removed))
    ));
    shutdown_all(vec![a, b]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn last_host_cannot_be_removed() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Node::create_mesh(test_cfg(da.path(), "a").0).await.unwrap();
    let b = invited(&a, db.path(), "b", Role::Client).await;
    let ia = a.node_id();
    assert!(matches!(b.remove(ia).await, Err(NodeError::LastHost)));
    // A cannot sign itself out either: it is the only host.
    assert!(matches!(a.remove(ia).await, Err(NodeError::LastHost)));
    // A removed member is no longer effective, so removing it again is refused.
    let ib = b.node_id();
    a.remove(ib).await.unwrap();
    assert!(matches!(a.remove(ib).await, Err(NodeError::NotEffective)));
    shutdown_all(vec![a, b]).await;
}

/// Cuts every pair of the three nodes apart, so nothing moves except by `deliver_from`.
async fn isolate_all(m: &Mesh3) {
    let ids = [m.a.node_id(), m.b.node_id(), m.c.node_id()];
    for n in [&m.a, &m.b, &m.c] {
        let others: Vec<NodeId> = ids.iter().copied().filter(|i| *i != n.node_id()).collect();
        n.partition(&others);
    }
    wait_until("all connections closed", T, || async {
        [&m.a, &m.b, &m.c].iter().all(|n| n.connection_count() == 0)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_self_stops() {
    let m = mesh3(Role::Host).await;
    isolate_all(&m).await;
    let Mesh3 { a, b, c, dirs } = m;
    let (ib, ic) = (b.node_id(), c.node_id());
    let mut c_events = c.subscribe();
    b.remove(ic).await.unwrap();
    // The record reaches C by hand, so no REMOVED close can come first.
    c.deliver_from(&b).await.unwrap();
    wait_event(
        &mut c_events,
        |e| matches!(e, NodeEvent::RemovedSelf { by } if *by == ib),
        T,
    )
    .await;
    assert_eq!(c.status(), NodeStatus::Removed { by: ib });
    assert!(matches!(
        c.store().set(NS, "k", b"x".to_vec()).await,
        Err(NodeError::Removed)
    ));
    assert!(matches!(
        c.store().set_add(NS, "s", b"x".to_vec()).await,
        Err(NodeError::Removed)
    ));
    assert!(matches!(
        c.store().delete(NS, "k").await,
        Err(NodeError::Removed)
    ));
    // Every local write of the public API is gated, the ordered and bulk ones too.
    assert!(matches!(
        c.store()
            .set_ordered(NS, "k", vec![1], Some(b"x".to_vec()))
            .await,
        Err(NodeError::Removed)
    ));
    assert!(matches!(
        c.store().purge_namespace(NS).await,
        Err(NodeError::Removed)
    ));

    // The state is derived from the store, so it survives a restart.
    c.shutdown().await;
    let c = timeout(T, Node::open(test_cfg(dirs[2].path(), "c").0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.status(), NodeStatus::Removed { by: ib });
    assert!(matches!(
        c.store().set(NS, "k", b"x".to_vec()).await,
        Err(NodeError::Removed)
    ));
    assert_eq!(c.connection_count(), 0);
    shutdown_all(vec![a, b, c]).await;
}

/// Connected end to end: whether C hears the record or only the REMOVED close depends on
/// timing, so only what holds in every interleaving is asserted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removal_end_to_end() {
    let Mesh3 { a, b, c, dirs: _d } = mesh3(Role::Host).await;
    let (ib, ic) = (b.node_id(), c.node_id());
    b.remove(ic).await.unwrap();
    wait_until("A and B drop C", T, || async {
        !effective(&a, ic) && !effective(&b, ic) && !online(&a, ic) && !online(&b, ic)
    })
    .await;
    wait_until("C knows it is out", T, || async {
        match c.status() {
            NodeStatus::Removed { by } => by == ib,
            NodeStatus::RefusedAsRemoved { by } => !by.is_empty(),
            _ => false,
        }
    })
    .await;
    shutdown_all(vec![a, b, c]).await;
}

/// A and B remove each other while all three are cut apart; the records then reach C by
/// hand in the chosen order, and C hands both on. Also returns the events of C from before
/// the deliveries.
async fn staged_mutual(
    role_c: Role,
    a_first: bool,
) -> (Mesh3, tokio::sync::broadcast::Receiver<NodeEvent>) {
    let m = mesh3(role_c).await;
    let (ia, ib) = (m.a.node_id(), m.b.node_id());
    isolate_all(&m).await;
    let c_events = m.c.subscribe();
    m.a.remove(ib).await.unwrap();
    m.b.remove(ia).await.unwrap();
    if a_first {
        m.c.deliver_from(&m.a).await.unwrap();
        m.c.deliver_from(&m.b).await.unwrap();
    } else {
        m.c.deliver_from(&m.b).await.unwrap();
        m.c.deliver_from(&m.a).await.unwrap();
    }
    // Whatever the order, C ends with both removal records in force.
    wait_until("C shows A and B removed", T, || async {
        !effective(&m.c, ia) && !effective(&m.c, ib)
    })
    .await;
    m.a.deliver_from(&m.c).await.unwrap();
    m.b.deliver_from(&m.c).await.unwrap();
    (m, c_events)
}

async fn both_removed_everywhere(m: &Mesh3) {
    let (ia, ib) = (m.a.node_id(), m.b.node_id());
    for (n, name) in [(&m.a, "A"), (&m.b, "B"), (&m.c, "C")] {
        wait_until(&format!("{name} shows A and B removed"), T, || async {
            !effective(n, ia) && !effective(n, ib)
        })
        .await;
    }
    wait_until("A and B know they are removed", T, || async {
        m.a.status() == NodeStatus::Removed { by: ib }
            && m.b.status() == NodeStatus::Removed { by: ia }
    })
    .await;
    converged(&[&m.a, &m.b, &m.c], T).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutual_removal_converges() {
    for a_first in [true, false] {
        let (m, _events) = staged_mutual(Role::Host, a_first).await;
        both_removed_everywhere(&m).await;
        shutdown_all(vec![m.a, m.b, m.c]).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutual_removal_of_last_two_hosts() {
    let (m, mut c_events) = staged_mutual(Role::Client, true).await;
    wait_event(
        &mut c_events,
        |e| matches!(e, NodeEvent::NoEffectiveHost),
        T,
    )
    .await;
    both_removed_everywhere(&m).await;
    shutdown_all(vec![m.a, m.b, m.c]).await;
}

/// The change event for the removal is lost; a `Lagged` rescan (cache cleared) alone must
/// apply it: C ends non-effective on A and its post-cutoff write is retracted there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lagged_events_rescan() {
    let m = mesh3(Role::Host).await;
    let ic = m.c.node_id();
    put(&m.c, "k", "c1").await;
    converged(&[&m.a, &m.b, &m.c], T).await;
    isolate_all(&m).await;
    let Mesh3 { a, b, c, dirs: _d } = m;

    put(&c, "k", "c2").await;
    a.deliver_from(&c).await.unwrap();
    assert_eq!(val(&a, "k").await.as_deref(), Some(&b"c2"[..]));
    b.remove(ic).await.unwrap();

    a.lose_events(true);
    a.deliver_from(&b).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(effective(&a, ic), "the lost event must not rescan");
    assert_eq!(val(&a, "k").await.as_deref(), Some(&b"c2"[..]));

    a.lose_events(false);
    a.force_lagged();
    wait_until("A applies the removal", T, || async {
        !effective(&a, ic) && val(&a, "k").await.as_deref() == Some(&b"c1"[..])
    })
    .await;
    shutdown_all(vec![a, b, c]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sign_out_propagates() {
    let Mesh3 { a, b, c, dirs: _d } = mesh3(Role::Host).await;
    let ic = c.node_id();
    c.remove(ic).await.unwrap();
    wait_until("A and B show C removed", T, || async {
        !effective(&a, ic) && !effective(&b, ic)
    })
    .await;
    wait_until("C stops once a peer holds the record", T, || async {
        c.connection_count() == 0
    })
    .await;
    // A and B can close with `REMOVED` before C's own rescan applies the record (seen on a
    // slow Windows runner); C then shows `RefusedAsRemoved` until that rescan runs.
    wait_until("C shows its own sign-out", T, || async {
        c.status() == NodeStatus::Removed { by: ic }
    })
    .await;
    assert!(matches!(
        c.store().set(NS, "k", b"x".to_vec()).await,
        Err(NodeError::Removed)
    ));
    shutdown_all(vec![a, b, c]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sign_out_while_offline_reaches_peers_after_heal() {
    let m = mesh3(Role::Host).await;
    isolate_all(&m).await;
    let Mesh3 { a, b, c, dirs: _d } = m;
    let ic = c.node_id();
    c.remove(ic).await.unwrap();
    wait_until("C signed out", T, || async {
        c.status() == NodeStatus::Removed { by: ic }
    })
    .await;
    // Nobody has the record yet, so C keeps trying rather than stopping.
    assert!(effective(&a, ic) && effective(&b, ic));

    a.heal();
    b.heal();
    c.heal();
    wait_until("A and B learn the sign-out", T, || async {
        !effective(&a, ic) && !effective(&b, ic)
    })
    .await;
    wait_until("C stops", T, || async { c.connection_count() == 0 }).await;
    shutdown_all(vec![a, b, c]).await;
}
