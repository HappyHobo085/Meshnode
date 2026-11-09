use std::sync::Arc;

use ed25519_dalek::SigningKey;
use mesh_core::membership::{
    CONSUMED, MEMBERS, MESH_NS, MembershipSnapshot, MeshAcceptor, REMOVED, addr_key, info_key,
    member_bytes,
};
use mesh_core::{CoreStore, DynClock};
use mesh_hlc::ManualClock;
use mesh_proto::{MemberInfo, MeshGenesis, NodeAddrBytes, NodeId, RemovalRecord, Role};
use mesh_store::{DeferReason, Outcome, SyncItem};

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn id(n: u8) -> NodeId {
    NodeId(key(n).verifying_key().to_bytes())
}

fn genesis() -> MeshGenesis {
    MeshGenesis {
        founder: id(1),
        nonce: [9; 32],
    }
}

fn open(dir: &std::path::Path, n: u8, g: &MeshGenesis) -> CoreStore {
    let clock = ManualClock::new(1_000_000);
    CoreStore::open(
        &dir.join(format!("n{n}.redb")),
        g.mesh_id(),
        key(n),
        DynClock(Arc::new(clock)),
    )
    .unwrap()
}

fn removal(x: u8, cutoff: u64) -> Vec<u8> {
    postcard::to_allocvec(&RemovalRecord {
        node_id: id(x),
        cutoff_seq: cutoff,
    })
    .unwrap()
}

/// Copies every op `from` has and `to` lacks.
fn apply_all(to: &CoreStore, from: &CoreStore, acc: &MeshAcceptor) {
    let vv = to.version_vector().unwrap();
    for item in from.diff_since(&vv, 1000).unwrap() {
        if let SyncItem::Op(op) = item {
            to.apply(op, acc).unwrap();
        }
    }
}

#[test]
fn constants_and_keys() {
    assert_eq!(MESH_NS, "_mesh");
    assert_eq!(
        (MEMBERS, REMOVED, CONSUMED),
        ("members", "removed", "consumed_invites")
    );
    assert_eq!(info_key(&id(1)), format!("info/{}", hex::encode(id(1).0)));
    assert_eq!(addr_key(&id(1)), format!("addr/{}", hex::encode(id(1).0)));
    let b = member_bytes(&id(2), Role::Client);
    assert_eq!(&b[..32], &id(2).0);
    assert_eq!(b[32], 1);
    assert_eq!(member_bytes(&id(2), Role::Host)[32], 0);
}

#[test]
fn founder_is_member_without_record() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let s = open(dir.path(), 1, &g);
    let snap = MembershipSnapshot::load(&s, &g).unwrap();
    assert!(snap.is_member(&id(1)));
    assert!(snap.is_effective(&id(1)));
    assert_eq!(snap.role(&id(1)), Some(Role::Host));
    assert_eq!(snap.effective_hosts(), vec![id(1)]);
    assert!(!snap.is_member(&id(2)));
    assert_eq!(snap.role(&id(2)), None);
}

#[test]
fn acceptor_unknown_then_valid() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let founder = open(dir.path(), 1, &g);
    let b = open(dir.path(), 2, &g);
    let acc = MeshAcceptor::new(g.clone());

    let op = b.set("app", "k", b"v").unwrap();
    assert_eq!(
        founder.apply(op, &acc).unwrap(),
        Outcome::Deferred(DeferReason::UnknownOrigin)
    );
    assert_eq!(founder.get("app", "k").unwrap(), None);

    founder
        .set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Host))
        .unwrap();
    founder.retry_pending(&acc).unwrap();
    assert_eq!(founder.get("app", "k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn founder_ops_accepted_without_record() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let founder = open(dir.path(), 1, &g);
    let b = open(dir.path(), 2, &g);
    let acc = MeshAcceptor::new(g.clone());
    let op = founder.set("app", "k", b"v").unwrap();
    assert_eq!(b.apply(op, &acc).unwrap(), Outcome::Applied);
}

#[test]
fn role_host_wins_over_client() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let s = open(dir.path(), 1, &g);
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Client))
        .unwrap();
    let snap = MembershipSnapshot::load(&s, &g).unwrap();
    assert_eq!(snap.role(&id(2)), Some(Role::Client));
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Host))
        .unwrap();
    let snap = MembershipSnapshot::load(&s, &g).unwrap();
    assert_eq!(snap.role(&id(2)), Some(Role::Host));
    assert!(snap.effective_hosts().contains(&id(2)));
}

#[test]
fn max_cutoff_and_removed_by() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let a = open(dir.path(), 1, &g); // founder A
    let b = open(dir.path(), 2, &g);
    let acc = MeshAcceptor::new(g.clone());
    a.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Host))
        .unwrap();
    a.set_add(MESH_NS, MEMBERS, &member_bytes(&id(3), Role::Client))
        .unwrap();
    apply_all(&b, &a, &acc);
    a.set_add(MESH_NS, REMOVED, &removal(3, 3)).unwrap();
    b.set_add(MESH_NS, REMOVED, &removal(3, 7)).unwrap();
    apply_all(&a, &b, &acc);

    let snap = MembershipSnapshot::load(&a, &g).unwrap();
    assert_eq!(snap.cutoff(&id(3)), Some(7));
    assert_eq!(snap.removed_by(&id(3)), Some(id(2)));
    assert_eq!(snap.cutoff(&id(2)), None);
    assert_eq!(snap.removed_by(&id(2)), None);
}

#[test]
fn removed_by_tie_goes_to_lowest_origin() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let a = open(dir.path(), 1, &g);
    let b = open(dir.path(), 2, &g);
    let acc = MeshAcceptor::new(g.clone());
    a.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Host))
        .unwrap();
    apply_all(&b, &a, &acc);
    a.set_add(MESH_NS, REMOVED, &removal(3, 5)).unwrap();
    b.set_add(MESH_NS, REMOVED, &removal(3, 5)).unwrap();
    apply_all(&a, &b, &acc);
    let snap = MembershipSnapshot::load(&a, &g).unwrap();
    assert_eq!(snap.cutoff(&id(3)), Some(5));
    assert_eq!(snap.removed_by(&id(3)), Some(id(1).min(id(2))));
}

#[test]
fn effective_excludes_removed() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let s = open(dir.path(), 1, &g);
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Host))
        .unwrap();
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(3), Role::Client))
        .unwrap();
    s.set_add(MESH_NS, REMOVED, &removal(2, 4)).unwrap();
    let snap = MembershipSnapshot::load(&s, &g).unwrap();
    assert!(snap.is_member(&id(2)));
    assert!(!snap.is_effective(&id(2)));
    assert_eq!(snap.effective_hosts(), vec![id(1)]);
    let mut m = snap.effective_members();
    m.sort();
    let mut want = vec![id(1), id(3)];
    want.sort();
    assert_eq!(m, want);
}

#[test]
fn info_and_addr_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let s = open(dir.path(), 1, &g);
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Client))
        .unwrap();
    let info = MemberInfo {
        name: "phone".into(),
        platform: "android".into(),
        tags: vec!["a".into()],
    };
    s.set(
        MESH_NS,
        &info_key(&id(2)),
        &postcard::to_allocvec(&info).unwrap(),
    )
    .unwrap();
    s.set(MESH_NS, &addr_key(&id(2)), &[1, 2, 3]).unwrap();
    s.set(MESH_NS, &info_key(&id(1)), &[0xff, 0xff]).unwrap(); // garbage
    let snap = MembershipSnapshot::load(&s, &g).unwrap();
    assert_eq!(snap.info(&id(2)), Some(&info));
    assert_eq!(snap.addr(&id(2)), Some(&NodeAddrBytes(vec![1, 2, 3])));
    assert_eq!(snap.info(&id(1)), None);
    assert_eq!(snap.addr(&id(1)), None);
}

#[test]
fn snapshot_pages_over_many_members() {
    let dir = tempfile::tempdir().unwrap();
    let g = genesis();
    let s = open(dir.path(), 1, &g);
    for i in 0..300u16 {
        let mut b = [0u8; 32];
        b[..2].copy_from_slice(&i.to_be_bytes());
        b[31] = 0xee;
        s.set_add(MESH_NS, MEMBERS, &member_bytes(&NodeId(b), Role::Client))
            .unwrap();
    }
    let snap = MembershipSnapshot::load(&s, &g).unwrap();
    assert_eq!(snap.effective_members().len(), 301);
}
