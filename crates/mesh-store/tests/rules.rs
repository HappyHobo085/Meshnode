//! One test per store rule.

mod common;

use common::{MESH, db_path, key, store};
use mesh_hlc::ManualClock;
use mesh_proto::{MeshId, Payload};
use mesh_store::{Store, StoreError};

const T0: u64 = 1_700_000_000_000;

fn setup() -> (tempfile::TempDir, ManualClock) {
    (tempfile::tempdir().unwrap(), ManualClock::new(T0))
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn local_set_get_delete() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("s", "a", b"1").unwrap();
    assert_eq!(s.get("s", "a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(s.list_keys("s", "", None, 10).unwrap(), strings(&["a"]));
    s.delete("s", "a").unwrap();
    assert_eq!(s.get("s", "a").unwrap(), None);
    assert!(s.list_keys("s", "", None, 10).unwrap().is_empty());
}

#[test]
fn local_ops_have_increasing_seq_and_hlc() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    let ops = [
        s.set("s", "a", b"1").unwrap(),
        s.delete("s", "a").unwrap(),
        s.set_add("s", "m", b"x").unwrap(),
    ];
    for (i, op) in ops.iter().enumerate() {
        assert_eq!(op.body.seq, i as u64 + 1);
        assert_eq!(op.body.origin, s.node_id());
        assert_eq!(op.body.mesh, s.mesh());
        op.verify().unwrap();
    }
    assert!(ops[0].body.hlc < ops[1].body.hlc);
    assert!(ops[1].body.hlc < ops[2].body.hlc);
    assert_eq!(s.version_vector_local_seq(), 3);
}

#[test]
fn ordered_write_beats_newer_plain_hlc_only_when_order_higher() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set_ordered("s", "k", &[5], Some(b"old")).unwrap();
    clock.advance(1000);
    s.set_ordered("s", "k", &[3], Some(b"new")).unwrap();
    assert_eq!(s.get("s", "k").unwrap(), Some(b"old".to_vec()));
    let e = s.get_entry("s", "k").unwrap().unwrap();
    assert_eq!(e.order, vec![5]);
    assert_eq!(e.seq, 1);
    s.set_ordered("s", "k", &[6], Some(b"newest")).unwrap();
    assert_eq!(s.get("s", "k").unwrap(), Some(b"newest".to_vec()));
    // A plain write has the empty order, which loses to any non-empty order.
    s.set("s", "k", b"plain").unwrap();
    assert_eq!(s.get("s", "k").unwrap(), Some(b"newest".to_vec()));
}

#[test]
fn tombstone_wins_over_older_value() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("s", "k", b"v").unwrap();
    let del = s.delete("s", "k").unwrap();
    assert!(matches!(
        &del.body.payload,
        Payload::Set { value: None, .. }
    ));
    assert_eq!(s.get("s", "k").unwrap(), None);
    let e = s.get_entry("s", "k").unwrap().unwrap();
    assert_eq!(e.value, None);
    assert_eq!(e.seq, 2);
    assert_eq!(e.origin, s.node_id());
    assert_eq!(e.hlc, del.body.hlc);
}

#[test]
fn set_add_is_idempotent_and_members_sorted() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set_add("s", "m", b"b").unwrap();
    s.set_add("s", "m", b"a").unwrap();
    s.set_add("s", "m", b"a").unwrap();
    assert_eq!(
        s.set_members("s", "m", None, 10).unwrap(),
        vec![b"a".to_vec(), b"b".to_vec()]
    );
    assert!(s.set_contains("s", "m", b"a").unwrap());
    assert!(!s.set_contains("s", "m", b"c").unwrap());
    assert!(!s.set_contains("s", "other", b"a").unwrap());
    assert_eq!(
        s.set_members("s", "m", Some(b"a"), 10).unwrap(),
        vec![b"b".to_vec()]
    );
    assert_eq!(
        s.set_members("s", "m", None, 1).unwrap(),
        vec![b"a".to_vec()]
    );
}

#[test]
fn set_members_sorted_when_one_member_prefixes_another() {
    // The encoding is member ‖ origin, so a member's rows can sort after a longer
    // member that extends it; set_members must still return byte-sorted order.
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    let members: [&[u8]; 5] = [b"ab", b"a", b"", b"a\x00", b"b"];
    for m in members {
        s.set_add("s", "m", m).unwrap();
    }
    let all = s.set_members("s", "m", None, 100).unwrap();
    let mut expected: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
    expected.sort();
    assert_eq!(all, expected);
    // Paging through yields the same sequence.
    let mut paged = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = s.set_members("s", "m", after.as_deref(), 2).unwrap();
        if page.is_empty() {
            break;
        }
        after = page.last().cloned();
        paged.extend(page);
    }
    assert_eq!(paged, all);
    assert!(s.set_contains("s", "m", b"").unwrap());
    assert!(s.set_contains("s", "m", b"a").unwrap());
    assert!(!s.set_contains("s", "m", b"a\x00\x00").unwrap());
}

#[test]
fn list_keys_pages_with_start_after_even_if_deleted() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    for k in ["a", "b", "c", "d"] {
        s.set("s", k, b"v").unwrap();
    }
    s.delete("s", "b").unwrap();
    assert_eq!(
        s.list_keys("s", "", Some("b"), 2).unwrap(),
        strings(&["c", "d"])
    );
    assert_eq!(s.list_keys("s", "", None, 2).unwrap(), strings(&["a", "c"]));
    assert!(s.list_keys("s", "", Some("d"), 2).unwrap().is_empty());
}

#[test]
fn list_keys_respects_prefix_and_key_prefixes() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    for k in ["k", "k2", "ka", "l"] {
        s.set("s", k, b"v").unwrap();
    }
    assert_eq!(
        s.list_keys("s", "k", None, 10).unwrap(),
        strings(&["k", "k2", "ka"])
    );
    assert_eq!(
        s.list_keys("s", "k", Some("k"), 10).unwrap(),
        strings(&["k2", "ka"])
    );
    s.delete("s", "k").unwrap();
    assert_eq!(s.get("s", "k").unwrap(), None);
    assert_eq!(s.get("s", "k2").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn list_keys_does_not_bleed_across_namespaces() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("a", "x", b"1").unwrap();
    s.set("ab", "y", b"2").unwrap();
    assert_eq!(s.list_keys("a", "", None, 10).unwrap(), strings(&["x"]));
    assert!(s.list_keys("ab", "x", None, 10).unwrap().is_empty());
    assert_eq!(s.get("a", "y").unwrap(), None);
}

#[test]
fn namespace_sizes_counts_live_values_and_members() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("a", "x", b"123").unwrap();
    s.set("a", "y", b"45").unwrap();
    s.delete("a", "y").unwrap();
    s.set_add("b", "m", b"zz").unwrap();
    s.set_add("b", "m", b"zz").unwrap();
    let sizes = s.namespace_sizes().unwrap();
    assert_eq!(sizes.get("a"), Some(&3));
    assert_eq!(sizes.get("b"), Some(&2));
    assert_eq!(sizes.len(), 2);
}

#[test]
fn local_write_over_limit_is_rejected() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    assert!(matches!(
        s.set("s", "k", &vec![0; 1_048_577]),
        Err(StoreError::Proto(_))
    ));
    assert!(matches!(
        s.set("s", "a\0b", b"v"),
        Err(StoreError::Proto(_))
    ));
    assert!(matches!(
        s.set_add("s\0", "m", b"v"),
        Err(StoreError::Proto(_))
    ));
    assert_eq!(s.version_vector_local_seq(), 0); // nothing was consumed (helper: next_seq - 1)
    let op = s.set("s", "k", b"ok").unwrap();
    assert_eq!(op.body.seq, 1);
}

#[test]
fn reopen_continues_seq_and_hlc() {
    let (dir, clock) = setup();
    let op2 = {
        let s = store(dir.path(), 1, &clock);
        s.set("s", "a", b"1").unwrap();
        s.set("s", "b", b"2").unwrap()
    };
    clock.set(T0 - 3_600_000);
    let s = store(dir.path(), 1, &clock);
    let op3 = s.set("s", "c", b"3").unwrap();
    assert_eq!(op3.body.seq, 3);
    assert!(op3.body.hlc > op2.body.hlc);
    assert_eq!(s.get("s", "a").unwrap(), Some(b"1".to_vec()));
}

#[test]
fn second_open_fails_cleanly() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    let second = Store::open(&db_path(dir.path(), 1), MESH, key(1), clock.clone());
    assert!(matches!(second, Err(StoreError::Io(_))));
    s.set("s", "a", b"1").unwrap();
    assert_eq!(s.get("s", "a").unwrap(), Some(b"1".to_vec()));
}

#[test]
fn wrong_mesh_on_open() {
    let (dir, clock) = setup();
    drop(store(dir.path(), 1, &clock));
    let r = Store::open(
        &db_path(dir.path(), 1),
        MeshId([8; 32]),
        key(1),
        clock.clone(),
    );
    assert!(matches!(r, Err(StoreError::WrongMesh)));
}

#[test]
fn other_node_key_on_open_is_corrupt() {
    let (dir, clock) = setup();
    drop(store(dir.path(), 1, &clock));
    let r = Store::open(&db_path(dir.path(), 1), MESH, key(2), clock.clone());
    assert!(matches!(r, Err(StoreError::Corrupt { table: "meta", .. })));
}

#[test]
fn store_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store<ManualClock>>();
    assert_send_sync::<Store>();
}
