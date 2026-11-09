use std::sync::Arc;

use mesh_core::local::{self, InviteEntry, Invites};
use mesh_core::{CoreStore, DynClock, NodeError, StoreHandle};
use mesh_hlc::SystemClock;
use mesh_proto::{MeshGenesis, NodeId, Role};

#[test]
fn key_created_then_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let a = local::load_or_create_key(dir.path()).unwrap();
    let b = local::load_or_create_key(dir.path()).unwrap();
    assert_eq!(a.verifying_key(), b.verifying_key());
}

#[test]
fn bad_key_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("node.key"), [1u8; 5]).unwrap();
    assert!(matches!(
        local::load_or_create_key(dir.path()),
        Err(NodeError::BadKey)
    ));
}

#[test]
fn genesis_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(local::read_genesis(dir.path()).unwrap(), None);
    let g = MeshGenesis {
        founder: NodeId([7; 32]),
        nonce: [9; 32],
    };
    local::write_genesis(dir.path(), &g).unwrap();
    assert_eq!(local::read_genesis(dir.path()).unwrap(), Some(g));
}

#[test]
fn invites_persist_and_prune() {
    let dir = tempfile::tempdir().unwrap();
    let mut inv = Invites::load(dir.path()).unwrap();
    let live = InviteEntry {
        role: Role::Client,
        expires_ms: 5_000,
        redeemed_by: None,
    };
    let dead = InviteEntry {
        role: Role::Host,
        expires_ms: 500,
        redeemed_by: None,
    };
    inv.insert([1; 32], live.clone()).unwrap();
    inv.insert([2; 32], dead).unwrap();
    let mut inv = Invites::load(dir.path()).unwrap();
    assert_eq!(inv.get(&[1; 32]), Some(&live));
    assert!(inv.get(&[2; 32]).is_some());
    inv.prune(1_000).unwrap();
    assert!(inv.get(&[2; 32]).is_none());
    inv.set_redeemed([1; 32], NodeId([3; 32])).unwrap();
    let inv = Invites::load(dir.path()).unwrap();
    assert_eq!(
        inv.get(&[1; 32]).unwrap().redeemed_by,
        Some(NodeId([3; 32]))
    );
}

#[test]
fn atomic_write_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.bin");
    local::atomic_write(&p, b"one").unwrap();
    local::atomic_write(&p, b"two-two").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two-two");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn store_handle_runs_off_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let key = local::load_or_create_key(dir.path()).unwrap();
    let mesh = mesh_proto::MeshId([5; 32]);
    let wall = DynClock(Arc::new(SystemClock));
    let store: CoreStore =
        mesh_store::Store::open(&dir.path().join("store.redb"), mesh, key, wall).unwrap();
    let handle = StoreHandle::new(Arc::new(store));
    handle.set("app", "k", b"v".to_vec()).await.unwrap();
    assert_eq!(handle.get("app", "k").await.unwrap(), Some(b"v".to_vec()));
    handle.delete("app", "k").await.unwrap();
    assert_eq!(handle.get("app", "k").await.unwrap(), None);
}
