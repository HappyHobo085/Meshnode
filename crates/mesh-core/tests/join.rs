//! Invite tickets and the `join/1` protocol (spec §5).

mod common;

use std::path::Path;
use std::time::Duration;

use common::{T, converged, test_cfg, wait_until};
use mesh_core::membership::{MEMBERS, MESH_NS};
use mesh_core::{Node, NodeError};
use mesh_proto::{InviteTicket, MeshId, NodeAddrBytes, RefuseReason, Role};
use tokio::time::timeout;

const TTL: Duration = Duration::from_secs(900);

async fn founder(dir: &Path) -> (Node, mesh_hlc::ManualClock) {
    let (cfg, clock) = test_cfg(dir, "a");
    (Node::create_mesh(cfg).await.unwrap(), clock)
}

async fn join(dir: &Path, name: &str, ticket: &InviteTicket) -> Result<Node, NodeError> {
    timeout(T, Node::join(test_cfg(dir, name).0, ticket.clone()))
        .await
        .expect("join timed out")
}

fn refused(r: Result<Node, NodeError>) -> RefuseReason {
    match r {
        Err(NodeError::JoinRefused(reason)) => reason,
        Err(e) => panic!("expected JoinRefused, got {e:?}"),
        Ok(_) => panic!("expected JoinRefused, got a node"),
    }
}

fn files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

async fn member_records(n: &Node, who: &mesh_proto::NodeId) -> Vec<Vec<u8>> {
    n.store()
        .set_members(MESH_NS, MEMBERS, None, 256)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.starts_with(&who.0))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_then_invite_join() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    assert_eq!(ticket.mesh, a.mesh());
    // The text form round-trips (what a QR code / paste carries).
    assert_eq!(InviteTicket::from_text(&ticket.to_text()).unwrap(), ticket);

    let (cfg, _) = test_cfg(db.path(), "b");
    let mut cfg = cfg;
    cfg.tags = vec!["laptop".into()];
    let b = timeout(T, Node::join(cfg, ticket)).await.unwrap().unwrap();
    assert!(
        b.wait_first_sync(Duration::from_secs(5)).await,
        "FirstSyncComplete"
    );
    assert!(b.first_sync_complete());
    assert_eq!(b.mesh(), a.mesh());
    converged(&[&a, &b], T).await;
    wait_until("B's info in A's members()", T, || async {
        a.members().iter().any(|m| {
            m.id == b.node_id()
                && m.role == Role::Host
                && m.info
                    .as_ref()
                    .is_some_and(|i| i.name == "b" && i.tags == ["laptop"])
        })
    })
    .await;
    assert!(db.path().join("mesh.json").exists());
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticket_reuse_refused() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, _) = founder(dirs[0].path()).await;
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    let b = join(dirs[1].path(), "b", &ticket).await.unwrap();
    let reason = refused(join(dirs[2].path(), "c", &ticket).await);
    assert_eq!(reason, RefuseReason::Used);
    assert!(!dirs[2].path().join("mesh.json").exists());
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_ticket_refused() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, clock) = founder(da.path()).await;
    let ticket = a.invite(Role::Host, Duration::from_secs(60)).await.unwrap();
    clock.advance(61_000);
    assert_eq!(
        refused(join(db.path(), "b", &ticket).await),
        RefuseReason::Expired
    );
    assert!(!db.path().join("mesh.json").exists());
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_token_refused() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let mut ticket = a.invite(Role::Host, TTL).await.unwrap();
    ticket.token[0] ^= 0xff;
    assert_eq!(
        refused(join(db.path(), "b", &ticket).await),
        RefuseReason::Unknown
    );
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_retry_same_node_idempotent() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    a.drop_next_join_reply();
    let first = join(db.path(), "b", &ticket).await;
    assert!(first.is_err(), "the reply was dropped");
    assert!(!db.path().join("mesh.json").exists());

    let b = join(db.path(), "b", &ticket).await.unwrap();
    converged(&[&a, &b], T).await;
    assert_eq!(member_records(&a, &b.node_id()).await.len(), 1);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_start_after_accept_leaves_dir_retryable() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    // A directory where the store file goes: the join is accepted but the node cannot start.
    std::fs::create_dir(db.path().join("store.redb")).unwrap();
    assert!(join(db.path(), "b", &ticket).await.is_err());
    assert!(
        !db.path().join("mesh.json").exists(),
        "mesh.json is removed when the start fails"
    );

    std::fs::remove_dir(db.path().join("store.redb")).unwrap();
    let b = join(db.path(), "b", &ticket).await.unwrap();
    assert!(b.wait_first_sync(T).await);
    converged(&[&a, &b], T).await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_ticket_wrong_genesis() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let mut ticket = a.invite(Role::Host, TTL).await.unwrap();
    ticket.mesh = MeshId([7; 32]);
    assert_eq!(
        refused(join(db.path(), "b", &ticket).await),
        RefuseReason::WrongMesh
    );
    assert!(!db.path().join("mesh.json").exists());
    assert!(!db.path().join("store.redb").exists());
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undecodable_inviter_addr_errors() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let mut ticket = a.invite(Role::Host, TTL).await.unwrap();
    ticket.inviter = NodeAddrBytes(vec![0xff; 7]);
    let r = join(db.path(), "b", &ticket).await;
    assert!(matches!(r, Err(NodeError::Proto(_))), "{:?}", r.err());
    assert!(
        files(db.path()).iter().all(|f| f == "node.key"),
        "{:?}",
        files(db.path())
    );
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_role_invite_records_client() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let ticket = a.invite(Role::Client, TTL).await.unwrap();
    let b = join(db.path(), "b", &ticket).await.unwrap();
    let recs = member_records(&a, &b.node_id()).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0][32], 1);
    wait_until("B listed as client", T, || async {
        a.members()
            .iter()
            .any(|m| m.id == b.node_id() && m.role == Role::Client)
    })
    .await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_after_inviter_crash_before_redeemed() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = founder(da.path()).await;
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    // The joiner's identity is its node.key, which exists before it joins.
    let b_id = mesh_proto::NodeId(
        mesh_core::local::load_or_create_key(db.path())
            .unwrap()
            .verifying_key()
            .to_bytes(),
    );
    // The inviter wrote members + consumed, then crashed before `redeemed_by`.
    a.store()
        .set_add(
            MESH_NS,
            MEMBERS,
            mesh_core::membership::member_bytes(&b_id, Role::Host).to_vec(),
        )
        .await
        .unwrap();
    a.store()
        .set_add(
            MESH_NS,
            mesh_core::membership::CONSUMED,
            blake3::hash(&ticket.token).as_bytes().to_vec(),
        )
        .await
        .unwrap();
    let b = join(db.path(), "b", &ticket).await.unwrap();
    assert_eq!(b.node_id(), b_id);
    converged(&[&a, &b], T).await;
    assert_eq!(member_records(&a, &b_id).await.len(), 1);
    a.shutdown().await;
    b.shutdown().await;
}
