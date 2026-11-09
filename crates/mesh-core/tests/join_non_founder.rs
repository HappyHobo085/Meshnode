//! A joiner admitted by a member other than the founder (spec §5.3 step 5): it must dial the
//! peers the reply lists, not only the founder its empty store knows of.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{T, converged, test_cfg, wait_until};
use mesh_core::{Node, NodeError};
use mesh_proto::{InviteTicket, NodeId, Role};
use tokio::time::timeout;

const TTL: Duration = Duration::from_secs(900);
const FIRST_SYNC: Duration = Duration::from_secs(10);

async fn join(dir: &Path, name: &str, ticket: InviteTicket) -> Result<Node, NodeError> {
    timeout(T, Node::join(test_cfg(dir, name).0, ticket))
        .await
        .expect("join timed out")
}

fn is_member(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id)
}

fn effective(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id && m.effective)
}

/// A founds the mesh and B (a host) joins through A and syncs; A then shuts down.
async fn mesh_of_two(da: &Path, db: &Path) -> (NodeId, Node) {
    let a = Node::create_mesh(test_cfg(da, "a").0).await.unwrap();
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    let b = join(db, "b", ticket).await.unwrap();
    assert!(b.wait_first_sync(FIRST_SYNC).await, "B's first sync");
    converged(&[&a, &b], T).await;
    let ia = a.node_id();
    a.shutdown().await;
    (ia, b)
}

async fn key_reaches(from: &Node, to: &Node, key: &str) {
    from.store()
        .set("app", key, b"from b".to_vec())
        .await
        .unwrap();
    wait_until("the key reaches the joiner", T, || async {
        to.store().get("app", key).await.unwrap().as_deref() == Some(b"from b".as_slice())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_via_non_founder_while_founder_offline() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (ia, b) = mesh_of_two(dirs[0].path(), dirs[1].path()).await;

    let ticket = b.invite(Role::Host, TTL).await.unwrap();
    let c = join(dirs[2].path(), "c", ticket).await.unwrap();
    assert!(
        c.wait_first_sync(FIRST_SYNC).await,
        "C completes a first sync through B with the founder offline"
    );
    wait_until("C sees A, B and C as members", T, || async {
        [ia, b.node_id(), c.node_id()]
            .iter()
            .all(|id| effective(&c, *id))
    })
    .await;
    key_reaches(&b, &c, "k1").await;

    b.shutdown().await;
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_via_non_founder_after_founder_removed() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (ia, b) = mesh_of_two(dirs[0].path(), dirs[1].path()).await;
    // B is a second host, so removing the founder is allowed.
    b.remove(ia).await.unwrap();
    wait_until("B drops A", T, || async { !effective(&b, ia) }).await;

    let ticket = b.invite(Role::Client, TTL).await.unwrap();
    let d = join(dirs[2].path(), "d", ticket).await.unwrap();
    assert!(
        d.wait_first_sync(FIRST_SYNC).await,
        "D completes a first sync through B after the founder was removed"
    );
    wait_until("D sees B and D effective and A removed", T, || async {
        effective(&d, b.node_id())
            && effective(&d, d.node_id())
            && is_member(&d, ia)
            && !effective(&d, ia)
    })
    .await;
    key_reaches(&b, &d, "k2").await;

    b.shutdown().await;
    d.shutdown().await;
}
