//! One test per store rule.

mod common;

use common::{AcceptAll, MESH, MembersOnly, db_path, key, node, store};
use mesh_hlc::{Hlc, ManualClock};
use mesh_proto::{MeshId, NodeId, OpBody, Payload, SignedOp};
use mesh_store::{
    Acceptor, Change, ChangeKind, DeferReason, Outcome, ReadView, RejectReason, Store, StoreError,
    Verdict,
};
use std::collections::HashSet;

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
    assert_eq!(s.version_vector().unwrap().get(&s.node_id()), 3);
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
    assert_eq!(s.version_vector().unwrap().get(&s.node_id()), 0); // nothing was consumed (helper: next_seq - 1)
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

// ---------------------------------------------------------------------------
// Remote apply (Task 4)

const DAY_MS: u64 = 86_400_000;

/// A `Set` op signed by test node `n`, built by hand (for equivocation / gaps).
fn hand_op(n: u8, seq: u64, hlc_ms: u64, key_: &str, value: &[u8]) -> SignedOp {
    let body = OpBody {
        mesh: MESH,
        origin: node(n),
        seq,
        hlc: Hlc::new(hlc_ms, 0),
        payload: Payload::Set {
            ns: "s".into(),
            key: key_.into(),
            order: Vec::new(),
            value: Some(value.to_vec()),
        },
    };
    SignedOp::sign(body, &key(n))
}

fn members(ids: &[NodeId]) -> MembersOnly {
    MembersOnly(ids.iter().copied().collect::<HashSet<_>>())
}

#[test]
fn remote_op_applies_and_converges_with_local_reader() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let op = a.set("s", "k", b"v").unwrap();
    let add = a.set_add("s", "m", b"x").unwrap();
    assert_eq!(b.apply(op, &AcceptAll).unwrap(), Outcome::Applied);
    assert_eq!(b.apply(add, &AcceptAll).unwrap(), Outcome::Applied);
    assert_eq!(b.get("s", "k").unwrap(), a.get("s", "k").unwrap());
    assert_eq!(
        b.get_entry("s", "k").unwrap(),
        a.get_entry("s", "k").unwrap()
    );
    assert!(b.set_contains("s", "m", b"x").unwrap());
    assert_eq!(b.vv_of(a.node_id()).unwrap(), 2);
}

#[test]
fn own_op_echo_is_duplicate() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let op = a.set("s", "k", b"v").unwrap();
    assert_eq!(a.apply(op.clone(), &AcceptAll).unwrap(), Outcome::Duplicate);
    assert_eq!(a.version_vector().unwrap().get(&a.node_id()), 1);
    assert_eq!(a.vv_of(a.node_id()).unwrap(), 1);
    assert_eq!(a.pending_len().unwrap(), 0);
    assert_eq!(a.set("s", "k", b"w").unwrap().body.seq, 2);
}

#[test]
fn bad_signature_rejected_and_not_counted() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let mut op = a.set("s", "k", b"v").unwrap();
    op.sig[0] ^= 1;
    assert_eq!(
        b.apply(op.clone(), &AcceptAll).unwrap(),
        Outcome::Rejected(RejectReason::BadSig)
    );
    assert_eq!(b.vv_of(a.node_id()).unwrap(), 0);
    assert_eq!(b.get("s", "k").unwrap(), None);
    assert_eq!(b.pending_len().unwrap(), 0);
    // The genuine op still applies afterwards.
    op.sig[0] ^= 1;
    assert_eq!(b.apply(op, &AcceptAll).unwrap(), Outcome::Applied);
}

#[test]
fn wrong_mesh_rejected() {
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    let mut body = hand_op(1, 1, T0, "k", b"v").body;
    body.mesh = MeshId([8; 32]);
    let op = SignedOp::sign(body, &key(1));
    assert_eq!(
        b.apply(op, &AcceptAll).unwrap(),
        Outcome::Rejected(RejectReason::WrongMesh)
    );
    assert_eq!(b.vv_of(node(1)).unwrap(), 0);
}

#[test]
fn equivocation_keeps_first() {
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    let first = hand_op(1, 1, T0, "k", b"first");
    let second = hand_op(1, 1, T0, "k", b"second");
    assert_eq!(
        b.apply(first.clone(), &AcceptAll).unwrap(),
        Outcome::Applied
    );
    assert_eq!(
        b.apply(second, &AcceptAll).unwrap(),
        Outcome::Rejected(RejectReason::Equivocation)
    );
    assert_eq!(b.get("s", "k").unwrap(), Some(b"first".to_vec()));
    assert_eq!(b.apply(first, &AcceptAll).unwrap(), Outcome::Duplicate);
}

#[test]
fn equivocation_against_pending_keeps_first() {
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    let first = hand_op(1, 2, T0, "k", b"first");
    let second = hand_op(1, 2, T0, "k", b"second");
    assert_eq!(
        b.apply(first.clone(), &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    assert_eq!(
        b.apply(second, &AcceptAll).unwrap(),
        Outcome::Rejected(RejectReason::Equivocation)
    );
    assert_eq!(b.apply(first, &AcceptAll).unwrap(), Outcome::Duplicate);
    assert_eq!(b.pending_len().unwrap(), 1);
    assert_eq!(
        b.apply(hand_op(1, 1, T0, "j", b"one"), &AcceptAll).unwrap(),
        Outcome::Applied
    );
    assert_eq!(b.get("s", "k").unwrap(), Some(b"first".to_vec()));
    assert_eq!(b.vv_of(node(1)).unwrap(), 2);
    assert_eq!(b.pending_len().unwrap(), 0);
}

#[test]
fn gap_is_deferred_then_applied() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let o1 = a.set("s", "x", b"1").unwrap();
    let o2 = a.set("s", "y", b"2").unwrap();
    let o3 = a.set("s", "z", b"3").unwrap();
    assert_eq!(
        b.apply(o3, &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    assert_eq!(
        b.apply(o2, &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    assert_eq!(b.get("s", "z").unwrap(), None);
    assert_eq!(b.vv_of(a.node_id()).unwrap(), 0);
    assert_eq!(b.apply(o1, &AcceptAll).unwrap(), Outcome::Applied);
    for (k, v) in [("x", b"1"), ("y", b"2"), ("z", b"3")] {
        assert_eq!(b.get("s", k).unwrap(), Some(v.to_vec()));
    }
    assert_eq!(b.vv_of(a.node_id()).unwrap(), 3);
    assert_eq!(b.pending_len().unwrap(), 0);
}

#[test]
fn identical_pending_op_is_duplicate() {
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    let o2 = hand_op(1, 2, T0, "k", b"v");
    assert_eq!(
        b.apply(o2.clone(), &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    assert_eq!(b.apply(o2, &AcceptAll).unwrap(), Outcome::Duplicate);
    assert_eq!(b.pending_len().unwrap(), 1);
}

#[test]
fn unknown_origin_deferred_until_member() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let op = a.set("s", "k", b"v").unwrap();
    assert_eq!(
        b.apply(op, &members(&[])).unwrap(),
        Outcome::Deferred(DeferReason::UnknownOrigin)
    );
    assert_eq!(b.get("s", "k").unwrap(), None);
    assert_eq!(b.vv_of(a.node_id()).unwrap(), 0);
    assert_eq!(b.retry_pending(&members(&[])).unwrap(), 0);
    assert_eq!(b.pending_len().unwrap(), 1);
    assert_eq!(b.retry_pending(&members(&[a.node_id()])).unwrap(), 1);
    assert_eq!(b.get("s", "k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(b.vv_of(a.node_id()).unwrap(), 1);
    assert_eq!(b.pending_len().unwrap(), 0);
}

#[test]
fn drain_rechecks_origin_of_unblocked_ops() {
    // Op 2 waits on a gap; op 1 arrives under an acceptor that does not know the origin
    // yet, so both stay pending until a retry with the origin as a member.
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    let a = node(1);
    assert_eq!(
        b.apply(hand_op(1, 2, T0, "k2", b"2"), &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    assert_eq!(
        b.apply(hand_op(1, 1, T0, "k1", b"1"), &members(&[]))
            .unwrap(),
        Outcome::Deferred(DeferReason::UnknownOrigin)
    );
    assert_eq!(b.pending_len().unwrap(), 2);
    assert_eq!(b.retry_pending(&members(&[a])).unwrap(), 2);
    assert_eq!(b.get("s", "k2").unwrap(), Some(b"2".to_vec()));
    assert_eq!(b.vv_of(a).unwrap(), 2);
    assert_eq!(b.pending_len().unwrap(), 0);
}

#[test]
fn drained_op_is_judged_again() {
    // Op 2 was deferred for a gap under a permissive acceptor; when op 1 arrives under a
    // stricter acceptor that only admits op 1, op 2 stays pending (now as UnknownOrigin).
    struct OnlySeq1;
    impl Acceptor for OnlySeq1 {
        fn judge(&self, op: &OpBody, _view: &ReadView<'_>) -> Verdict {
            if op.seq == 1 {
                Verdict::Valid
            } else {
                Verdict::UnknownOrigin
            }
        }
    }
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    b.apply(hand_op(1, 2, T0, "k2", b"2"), &AcceptAll).unwrap();
    assert_eq!(
        b.apply(hand_op(1, 1, T0, "k1", b"1"), &OnlySeq1).unwrap(),
        Outcome::Applied
    );
    assert_eq!(b.vv_of(node(1)).unwrap(), 1);
    assert_eq!(b.get("s", "k2").unwrap(), None);
    assert_eq!(b.pending_len().unwrap(), 1);
    assert_eq!(b.retry_pending(&AcceptAll).unwrap(), 1);
    assert_eq!(b.vv_of(node(1)).unwrap(), 2);
}

#[test]
fn skew_deferred_then_applied_when_clock_catches_up() {
    let dir = tempfile::tempdir().unwrap();
    let clock_a = ManualClock::new(T0 + 300_001);
    let clock_b = ManualClock::new(T0);
    let a = store(dir.path(), 1, &clock_a);
    let b = store(dir.path(), 2, &clock_b);
    let op = a.set("s", "k", b"v").unwrap();
    assert_eq!(
        b.apply(op.clone(), &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Skew)
    );
    assert_eq!(b.retry_pending(&AcceptAll).unwrap(), 0);
    clock_b.advance(2);
    assert_eq!(b.retry_pending(&AcceptAll).unwrap(), 1);
    assert_eq!(b.get("s", "k").unwrap(), Some(b"v".to_vec()));
    // The observed HLC raised B's clock.
    assert!(b.set("s", "j", b"w").unwrap().body.hlc > op.body.hlc);
}

#[test]
fn observed_hlc_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let clock_a = ManualClock::new(T0 + 200_000);
    let clock_b = ManualClock::new(T0);
    let a = store(dir.path(), 1, &clock_a);
    let op = a.set("s", "k", b"v").unwrap();
    {
        let b = store(dir.path(), 2, &clock_b);
        assert_eq!(b.apply(op.clone(), &AcceptAll).unwrap(), Outcome::Applied);
    }
    let b = store(dir.path(), 2, &clock_b);
    assert!(b.set("s", "j", b"w").unwrap().body.hlc > op.body.hlc);
}

#[test]
fn pending_dropped_after_24h() {
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    assert_eq!(
        b.apply(hand_op(1, 2, T0, "k", b"v"), &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    clock.advance(DAY_MS + 1);
    assert_eq!(b.retry_pending(&AcceptAll).unwrap(), 0);
    assert_eq!(b.pending_len().unwrap(), 0);
    assert_eq!(b.vv_of(node(1)).unwrap(), 0);
}

#[test]
fn retry_keeps_first_seen() {
    let (dir, clock) = setup();
    let b = store(dir.path(), 2, &clock);
    assert_eq!(
        b.apply(hand_op(1, 2, T0, "k", b"v"), &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    clock.advance(DAY_MS / 2);
    assert_eq!(b.retry_pending(&AcceptAll).unwrap(), 0);
    assert_eq!(b.pending_len().unwrap(), 1);
    clock.advance(DAY_MS / 2);
    assert_eq!(b.retry_pending(&AcceptAll).unwrap(), 0);
    assert_eq!(b.pending_len().unwrap(), 1); // exactly 24 h old: kept
    clock.advance(1);
    assert_eq!(b.retry_pending(&AcceptAll).unwrap(), 0);
    assert_eq!(b.pending_len().unwrap(), 0);
}

#[test]
fn change_events_after_commit() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let mut rx = b.subscribe();
    b.set("s", "mine", b"1").unwrap();
    let op = a.set_add("t", "group", b"x").unwrap();
    assert_eq!(b.apply(op.clone(), &AcceptAll).unwrap(), Outcome::Applied);
    assert_eq!(
        rx.try_recv().unwrap(),
        Change {
            ns: "s".into(),
            kind: ChangeKind::Key("mine".into()),
            local: true
        }
    );
    assert_eq!(
        rx.try_recv().unwrap(),
        Change {
            ns: "t".into(),
            kind: ChangeKind::Set("group".into()),
            local: false
        }
    );
    // A duplicate changes nothing and publishes nothing.
    assert_eq!(b.apply(op, &AcceptAll).unwrap(), Outcome::Duplicate);
    assert!(rx.try_recv().is_err());
}

#[test]
fn drained_chain_emits_one_event_per_op() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let ops = [
        a.set("s", "x", b"1").unwrap(),
        a.set("s", "y", b"2").unwrap(),
        a.set("s", "z", b"3").unwrap(),
    ];
    let mut rx = b.subscribe();
    b.apply(ops[2].clone(), &AcceptAll).unwrap();
    b.apply(ops[1].clone(), &AcceptAll).unwrap();
    assert!(rx.try_recv().is_err());
    b.apply(ops[0].clone(), &AcceptAll).unwrap();
    let kinds: Vec<ChangeKind> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|c| c.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            ChangeKind::Key("x".into()),
            ChangeKind::Key("y".into()),
            ChangeKind::Key("z".into())
        ]
    );
}

#[test]
fn publishing_without_subscribers_is_fine() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let op = a.set("s", "k", b"v").unwrap();
    assert_eq!(b.apply(op, &AcceptAll).unwrap(), Outcome::Applied);
    drop(b.subscribe());
    b.set("s", "j", b"w").unwrap();
}

/// Admits an origin only if `_mesh/members` contains it (read through the view).
struct MembersFromStore;

impl Acceptor for MembersFromStore {
    fn judge(&self, op: &OpBody, view: &ReadView<'_>) -> Verdict {
        match view.set_contains("_mesh", "members", &op.origin.0) {
            Ok(true) => Verdict::Valid,
            _ => Verdict::UnknownOrigin,
        }
    }
}

#[test]
fn acceptor_reads_the_store_inside_the_transaction() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let op = a.set("s", "k", b"v").unwrap();
    assert_eq!(
        b.apply(op, &MembersFromStore).unwrap(),
        Outcome::Deferred(DeferReason::UnknownOrigin)
    );
    b.set_add("_mesh", "members", &a.node_id().0).unwrap();
    assert_eq!(b.retry_pending(&MembersFromStore).unwrap(), 1);
    assert_eq!(b.get("s", "k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn echoed_own_ops_raise_next_seq() {
    // Same node key, fresh DB (DB loss / re-bootstrap): peers echo our seqs 1..=3.
    let (dir, clock) = setup();
    let old_dir = tempfile::tempdir().unwrap();
    let old = store(old_dir.path(), 1, &clock);
    let echoed = [
        old.set("s", "k", b"1").unwrap(),
        old.set("s", "k", b"2").unwrap(),
        old.set("s", "k", b"3").unwrap(),
    ];
    let s = store(dir.path(), 1, &clock);
    // Out of order, so seqs 2 and 3 come in through the drain.
    for i in [2, 1, 0] {
        s.apply(echoed[i].clone(), &AcceptAll).unwrap();
    }
    assert_eq!(s.vv_of(s.node_id()).unwrap(), 3);
    assert_eq!(s.version_vector().unwrap().get(&s.node_id()), 3);
    let new = s.set("s", "k", b"4").unwrap();
    assert_eq!(new.body.seq, 4);
    assert_eq!(s.get("s", "k").unwrap(), Some(b"4".to_vec()));
    // The echoed ops' rows are untouched (identical op => Duplicate, not Equivocation).
    for op in echoed {
        assert_eq!(s.apply(op, &AcceptAll).unwrap(), Outcome::Duplicate);
    }
    assert_eq!(s.apply(new, &AcceptAll).unwrap(), Outcome::Duplicate);
}

#[test]
fn concurrent_writes_converge_in_any_order() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let c = store(dir.path(), 3, &clock);
    let oa = a.set("s", "k", b"from-a").unwrap();
    let oc = c.set("s", "k", b"from-c").unwrap();
    let b = store(dir.path(), 2, &clock);
    let d = store(dir.path(), 4, &clock);
    b.apply(oa.clone(), &AcceptAll).unwrap();
    b.apply(oc.clone(), &AcceptAll).unwrap();
    d.apply(oc, &AcceptAll).unwrap();
    d.apply(oa, &AcceptAll).unwrap();
    assert_eq!(
        b.get_entry("s", "k").unwrap(),
        d.get_entry("s", "k").unwrap()
    );
    assert!(b.get("s", "k").unwrap().is_some());
}

// ---- Task 5: catch-up ----

use mesh_proto::VersionVector;
use mesh_store::SyncItem;

fn op_ids(items: &[SyncItem]) -> Vec<(u8, u64)> {
    items
        .iter()
        .filter_map(|i| match i {
            SyncItem::Op(o) => Some((o.body.origin.0[0], o.body.seq)),
            SyncItem::Covered { .. } => None,
        })
        .collect()
}

#[test]
fn diff_since_returns_missing_ops_in_origin_seq_order() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let c = store(dir.path(), 3, &clock);
    let b = store(dir.path(), 2, &clock);
    let a_ops: Vec<_> = (0..3).map(|i| a.set("s", "k", &[i]).unwrap()).collect();
    let c_ops: Vec<_> = (0..2).map(|i| c.set("s", "j", &[i]).unwrap()).collect();
    for op in a_ops.iter().chain(c_ops.iter()) {
        b.apply(op.clone(), &AcceptAll).unwrap();
    }
    let vv = b.version_vector().unwrap();
    assert_eq!(vv.get(&node(1)), 3);
    assert_eq!(vv.get(&node(3)), 2);
    assert_eq!(vv.get(&b.node_id()), 0);
    let mut theirs = VersionVector::default();
    theirs.set(node(1), 1);
    // An origin the peer has but we don't is ignored.
    theirs.set(node(9), 4);
    let items = b.diff_since(&theirs, 100).unwrap();
    assert_eq!(
        op_ids(&items),
        vec![
            (node(1).0[0], 2),
            (node(1).0[0], 3),
            (node(3).0[0], 1),
            (node(3).0[0], 2)
        ]
    );
    assert_eq!(items.len(), 4);
}

#[test]
fn diff_since_respects_limit() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    for i in 0..5 {
        a.set("s", "k", &[i]).unwrap();
    }
    let items = a.diff_since(&VersionVector::default(), 2).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(a.diff_since(&VersionVector::default(), 0).unwrap().len(), 0);
}

#[test]
fn covered_marker_lets_peer_advance() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let c = store(dir.path(), 3, &clock);
    for i in 0..5u8 {
        let op = a.set("s", "k", &[i]).unwrap();
        b.apply(op, &AcceptAll).unwrap();
    }
    b.__test_drop_op(node(1), 2).unwrap();
    b.__test_drop_op(node(1), 3).unwrap();
    let items = b.diff_since(&VersionVector::default(), 100).unwrap();
    assert_eq!(items.len(), 4);
    assert!(matches!(&items[0], SyncItem::Op(o) if o.body.seq == 1));
    assert!(matches!(&items[1], SyncItem::Covered { origin, upto: 3 } if *origin == node(1)));
    assert!(matches!(&items[2], SyncItem::Op(o) if o.body.seq == 4));
    assert!(matches!(&items[3], SyncItem::Op(o) if o.body.seq == 5));
    // Limit counts markers too.
    assert_eq!(b.diff_since(&VersionVector::default(), 2).unwrap().len(), 2);
    for item in items {
        match item {
            SyncItem::Op(o) => {
                c.apply(o, &AcceptAll).unwrap();
            }
            SyncItem::Covered { origin, upto } => {
                c.apply_covered(origin, upto, upto, &AcceptAll).unwrap()
            }
        }
    }
    assert_eq!(c.version_vector().unwrap().get(&node(1)), 5);
    assert_eq!(c.get("s", "k").unwrap(), b.get("s", "k").unwrap());
    assert_eq!(
        c.get_entry("s", "k").unwrap(),
        b.get_entry("s", "k").unwrap()
    );
}

#[test]
fn trailing_hole_is_covered_and_invalid_ops_are_served() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    for i in 0..3u8 {
        b.apply(a.set("s", "k", &[i]).unwrap(), &AcceptAll).unwrap();
    }
    b.__test_drop_op(node(1), 3).unwrap();
    let items = b.diff_since(&VersionVector::default(), 100).unwrap();
    assert_eq!(items.len(), 3);
    assert!(matches!(&items[2], SyncItem::Covered { upto: 3, .. }));
    // Peer already has 1..=2: only the marker is sent.
    let mut theirs = VersionVector::default();
    theirs.set(node(1), 2);
    let items = b.diff_since(&theirs, 100).unwrap();
    assert!(matches!(
        items.as_slice(),
        [SyncItem::Covered { upto: 3, .. }]
    ));
}

#[test]
fn pending_ops_are_not_served() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let _a1 = a.set("s", "k", b"1").unwrap();
    let a2 = a.set("s", "k", b"2").unwrap();
    assert_eq!(
        b.apply(a2, &AcceptAll).unwrap(),
        Outcome::Deferred(DeferReason::Gap)
    );
    assert_eq!(b.pending_len().unwrap(), 1);
    assert!(
        b.diff_since(&VersionVector::default(), 100)
            .unwrap()
            .is_empty()
    );
    assert!(b.version_vector().unwrap().0.is_empty());
}

#[test]
fn apply_covered_drains_pending_and_publishes() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let _a1 = a.set("s", "k", b"1").unwrap();
    let a2 = a.set("s", "k", b"2").unwrap();
    let a3 = a.set("s", "other", b"3").unwrap();
    b.apply(a2, &AcceptAll).unwrap();
    b.apply(a3, &AcceptAll).unwrap();
    let mut rx = b.subscribe();
    b.apply_covered(node(1), 1, 3, &AcceptAll).unwrap();
    assert_eq!(b.version_vector().unwrap().get(&node(1)), 3);
    assert_eq!(b.pending_len().unwrap(), 0);
    assert_eq!(b.get("s", "k").unwrap(), Some(b"2".to_vec()));
    assert_eq!(rx.try_recv().unwrap().kind, ChangeKind::Key("k".into()));
    assert_eq!(rx.try_recv().unwrap().kind, ChangeKind::Key("other".into()));
    // Lower or equal upto is a no-op.
    b.apply_covered(node(1), 2, 3, &AcceptAll).unwrap();
    assert_eq!(b.version_vector().unwrap().get(&node(1)), 3);
}

#[test]
fn apply_covered_drops_covered_pending() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let _ = a.set("s", "k", b"1").unwrap();
    let _ = a.set("s", "k", b"2").unwrap();
    let a3 = a.set("s", "k", b"3").unwrap();
    b.apply(a3, &AcceptAll).unwrap();
    assert_eq!(b.pending_len().unwrap(), 1);
    b.apply_covered(node(1), 5, 5, &AcceptAll).unwrap();
    assert_eq!(b.pending_len().unwrap(), 0);
    assert_eq!(b.version_vector().unwrap().get(&node(1)), 5);
    // The covered op's data was never merged.
    assert_eq!(b.get("s", "k").unwrap(), None);
}

#[test]
fn apply_covered_for_own_origin_raises_next_seq() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("s", "k", b"1").unwrap();
    s.apply_covered(s.node_id(), 7, 7, &AcceptAll).unwrap();
    assert_eq!(s.version_vector().unwrap().get(&s.node_id()), 7);
    assert_eq!(s.set("s", "k", b"2").unwrap().body.seq, 8);
    // Not above next_seq: unchanged.
    s.apply_covered(s.node_id(), 3, 7, &AcceptAll).unwrap();
    assert_eq!(s.set("s", "k", b"3").unwrap().body.seq, 9);
}

#[test]
fn apply_covered_above_advertised_is_ignored() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let a1 = a.set("s", "k", b"1").unwrap();
    let _a2 = a.set("s", "k", b"2").unwrap();
    // The sender advertised vv[a] = 2 in its Hello but claims more is covered.
    b.apply_covered(node(1), 5, 2, &AcceptAll).unwrap();
    assert_eq!(b.version_vector().unwrap().get(&node(1)), 0);
    assert_eq!(b.apply(a1, &AcceptAll).unwrap(), Outcome::Applied);
    // Within the advertised bound it applies.
    b.apply_covered(node(1), 2, 2, &AcceptAll).unwrap();
    assert_eq!(b.version_vector().unwrap().get(&node(1)), 2);
}

#[test]
fn apply_covered_huge_own_upto_is_ignored() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("s", "k", b"1").unwrap();
    s.apply_covered(s.node_id(), u64::MAX, u64::MAX, &AcceptAll)
        .unwrap();
    s.apply_covered(s.node_id(), u64::MAX / 2 + 1, u64::MAX, &AcceptAll)
        .unwrap();
    assert_eq!(s.version_vector().unwrap().get(&s.node_id()), 1);
    // Local writes still work.
    assert_eq!(s.set("s", "k", b"2").unwrap().body.seq, 2);
    // The bound itself is accepted.
    s.apply_covered(s.node_id(), u64::MAX / 2, u64::MAX / 2, &AcceptAll)
        .unwrap();
    assert_eq!(s.version_vector().unwrap().get(&s.node_id()), u64::MAX / 2);
}

#[test]
fn version_vector_includes_self_and_full_sync_mark_roundtrips() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    assert!(s.version_vector().unwrap().0.is_empty());
    assert_eq!(s.last_full_sync_ms().unwrap(), None);
    s.set("s", "k", b"1").unwrap();
    assert_eq!(s.version_vector().unwrap().get(&s.node_id()), 1);
    assert_eq!(s.synced_self_seq().unwrap(), None);
    clock.set(T0 + 5);
    s.mark_full_sync(1).unwrap();
    assert_eq!(s.last_full_sync_ms().unwrap(), Some(T0 + 5));
    assert_eq!(s.synced_self_seq().unwrap(), Some(1));
    // A peer cannot confirm seqs this node never wrote; a lower one does not lower it.
    s.mark_full_sync(9).unwrap();
    assert_eq!(s.synced_self_seq().unwrap(), Some(1));
    s.mark_full_sync(0).unwrap();
    assert_eq!(s.synced_self_seq().unwrap(), Some(1));
    drop(s);
    let s = store(dir.path(), 1, &clock);
    assert_eq!(s.last_full_sync_ms().unwrap(), Some(T0 + 5));
    assert_eq!(s.synced_self_seq().unwrap(), Some(1));
}

// ---- Task 6: maintenance ----

use mesh_store::{SETTLE_MS, STALE_MS, TOMBSTONE_TTL_MS};

/// Feeds `items` (from `from.diff_since`) into `to`; `from`'s vector is the advertised one.
fn feed(to: &Store<ManualClock>, from: &Store<ManualClock>, items: Vec<SyncItem>) {
    let advertised = from.version_vector().unwrap();
    for item in items {
        match item {
            SyncItem::Op(o) => {
                to.apply(o, &AcceptAll).unwrap();
            }
            SyncItem::Covered { origin, upto } => to
                .apply_covered(origin, upto, advertised.get(&origin), &AcceptAll)
                .unwrap(),
        }
    }
}

#[test]
fn constants_match_spec() {
    assert_eq!(SETTLE_MS, 7 * DAY_MS);
    assert_eq!(TOMBSTONE_TTL_MS, 90 * DAY_MS);
    assert_eq!(STALE_MS, 80 * DAY_MS);
}

#[test]
fn compaction_waits_for_settle_period() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("s", "k", b"1").unwrap();
    let op2 = s.set("s", "k", b"2").unwrap();
    clock.advance(7 * DAY_MS - 1);
    assert_eq!(s.compact().unwrap(), 0);
    let items = s.diff_since(&VersionVector::default(), 100).unwrap();
    assert_eq!(op_ids(&items), vec![(node(1).0[0], 1), (node(1).0[0], 2)]);
    clock.advance(2);
    assert_eq!(s.compact().unwrap(), 1);
    assert_eq!(
        s.diff_since(&VersionVector::default(), 100).unwrap(),
        vec![
            SyncItem::Covered {
                origin: node(1),
                upto: 1
            },
            SyncItem::Op(op2)
        ]
    );
    assert_eq!(s.get("s", "k").unwrap(), Some(b"2".to_vec()));
    // Nothing left to compact; the superseded entry went with the row.
    assert_eq!(s.compact().unwrap(), 0);
}

#[test]
fn compaction_never_removes_set_add_ops() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set_add("s", "m", b"x").unwrap();
    s.set_add("s", "m", b"x").unwrap();
    clock.advance(8 * DAY_MS);
    assert_eq!(s.compact().unwrap(), 0);
    let items = s.diff_since(&VersionVector::default(), 100).unwrap();
    assert_eq!(op_ids(&items).len(), 2);
    assert!(s.set_contains("s", "m", b"x").unwrap());
}

#[test]
fn compacted_store_feeds_fresh_peer_to_same_reads() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let c = store(dir.path(), 3, &clock);
    // B's writes come first (older HLC).
    let b_ops = [
        b.set("s", "a", b"b-old").unwrap(),
        b.set("s", "d", b"1").unwrap(),
        b.set("s", "d", b"2").unwrap(),
        b.set_ordered("s", "o", &[9], Some(b"ordered")).unwrap(),
    ];
    for op in b_ops {
        a.apply(op, &AcceptAll).unwrap();
    }
    clock.advance(10);
    a.set("s", "a", b"1").unwrap();
    a.set("s", "a", b"2").unwrap();
    a.set("s", "b", b"1").unwrap();
    a.delete("s", "b").unwrap();
    a.set("s", "c", b"x").unwrap();
    a.set("s", "o", b"plain-loses").unwrap();
    a.set_add("s", "m", b"x").unwrap();
    a.set_add("s", "m", b"x").unwrap();
    clock.advance(8 * DAY_MS);
    assert!(a.compact().unwrap() > 0);
    let items = a.diff_since(&VersionVector::default(), 1000).unwrap();
    assert!(items.iter().any(|i| matches!(i, SyncItem::Covered { .. })));
    feed(&c, &a, items);
    for k in ["a", "b", "c", "d", "o", "never"] {
        assert_eq!(c.get("s", k).unwrap(), a.get("s", k).unwrap(), "{k}");
        assert_eq!(
            c.get_entry("s", k).unwrap(),
            a.get_entry("s", k).unwrap(),
            "{k}"
        );
    }
    assert_eq!(
        c.list_keys("s", "", None, 100).unwrap(),
        a.list_keys("s", "", None, 100).unwrap()
    );
    assert_eq!(
        a.list_keys("s", "", None, 100).unwrap(),
        strings(&["a", "c", "d", "o"])
    );
    assert!(c.set_contains("s", "m", b"x").unwrap());
    assert_eq!(c.version_vector().unwrap().get(&node(1)), 8);
    assert_eq!(c.version_vector().unwrap().get(&node(2)), 4);
}

#[test]
fn tombstone_gc_after_90_days() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    // B's older write of k, and B's old tombstone of `live`.
    a.apply(b.set("s", "k", b"w").unwrap(), &AcceptAll).unwrap();
    a.apply(b.delete("s", "live").unwrap(), &AcceptAll).unwrap();
    clock.advance(10);
    a.set("s", "k", b"v").unwrap();
    a.set("s", "live", b"yes").unwrap();
    a.set_add("s", "m", b"x").unwrap();
    a.delete("s", "k").unwrap();
    let mut rx = a.subscribe();
    clock.advance(TOMBSTONE_TTL_MS);
    // A's own ops of k must be covered by a full sync before they can be GC'd.
    a.mark_full_sync(4).unwrap();
    assert_eq!(a.gc().unwrap(), 0);
    clock.advance(1);
    assert_eq!(a.gc().unwrap(), 1);
    assert_eq!(a.get("s", "k").unwrap(), None);
    // No reg row of any origin remains (B's older "w" must not reappear).
    assert_eq!(a.get_entry("s", "k").unwrap(), None);
    assert_eq!(a.list_keys("s", "", None, 100).unwrap(), strings(&["live"]));
    // A key whose winner is live is untouched, even with an old tombstone row under it.
    assert_eq!(a.get("s", "live").unwrap(), Some(b"yes".to_vec()));
    assert!(a.set_contains("s", "m", b"x").unwrap());
    let ev = rx.try_recv().unwrap();
    assert_eq!(ev.ns, "s");
    assert_eq!(ev.kind, ChangeKind::Key("k".into()));
    assert!(rx.try_recv().is_err());
    // The GC'd ops are served as covered, never as ops.
    let items = a.diff_since(&VersionVector::default(), 100).unwrap();
    assert!(items.iter().all(|i| match i {
        SyncItem::Op(o) => !matches!(&o.body.payload, Payload::Set { key, .. } if key == "k"),
        SyncItem::Covered { .. } => true,
    }));
    assert_eq!(a.gc().unwrap(), 0);
}

#[test]
fn gc_without_compaction_does_not_resurrect_on_fresh_peer() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let c = store(dir.path(), 3, &clock);
    a.set("s", "k", b"v").unwrap();
    a.set("s", "k", b"v2").unwrap();
    a.delete("s", "k").unwrap();
    a.set("s", "other", b"o").unwrap();
    clock.advance(TOMBSTONE_TTL_MS + 1);
    a.mark_full_sync(4).unwrap();
    // No compact() ran: the superseded ops of k must not outlive the GC.
    assert_eq!(a.gc().unwrap(), 1);
    feed(
        &c,
        &a,
        a.diff_since(&VersionVector::default(), 100).unwrap(),
    );
    assert_eq!(c.get("s", "k").unwrap(), None);
    assert_eq!(c.get_entry("s", "k").unwrap(), None);
    assert_eq!(c.get("s", "other").unwrap(), Some(b"o".to_vec()));
    assert_eq!(c.version_vector().unwrap().get(&node(1)), 4);
    // Nothing of k is left for compaction either.
    assert_eq!(a.compact().unwrap(), 0);
}

#[test]
fn stale_after_80_days() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    assert!(!s.is_stale().unwrap());
    s.mark_full_sync(0).unwrap();
    clock.advance(STALE_MS);
    assert!(!s.is_stale().unwrap());
    clock.advance(1);
    assert!(s.is_stale().unwrap());
    let never = store(dir.path(), 2, &clock);
    clock.advance(1000 * DAY_MS);
    assert!(!never.is_stale().unwrap());
}

#[test]
fn export_unsynced_local_ops_after_last_full_sync() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let o1 = a.set("s", "k", b"1").unwrap();
    let o2 = a.set_add("s", "m", b"x").unwrap();
    a.apply(b.set("s", "j", b"b").unwrap(), &AcceptAll).unwrap();
    // Never synced: every own op.
    assert_eq!(
        a.export_unsynced_local_ops().unwrap(),
        vec![SyncItem::Op(o1), SyncItem::Op(o2)]
    );
    a.mark_full_sync(2).unwrap();
    assert!(a.export_unsynced_local_ops().unwrap().is_empty());
    clock.advance(1);
    let o3 = a.delete("s", "k").unwrap();
    let o4 = a.set("s", "z", b"z").unwrap();
    a.apply(b.set("s", "j", b"b2").unwrap(), &AcceptAll)
        .unwrap();
    // Seqs 1..=2 were synced: no marker below the boundary (catch-up brings them back).
    assert_eq!(
        a.export_unsynced_local_ops().unwrap(),
        vec![SyncItem::Op(o3), SyncItem::Op(o4)]
    );
}

#[test]
fn own_write_during_sync_session_is_not_counted_as_synced() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("s", "k", b"1").unwrap();
    // The peer's diff is computed here: it will hold seq 1. Then, still during the
    // session, A writes seq 2 at the same wall time as the sync completes.
    let o2 = a.set("s", "k", b"2").unwrap();
    a.mark_full_sync(1).unwrap();
    assert_eq!(
        a.export_unsynced_local_ops().unwrap(),
        vec![SyncItem::Op(o2)]
    );
}

#[test]
fn export_covers_only_holes_above_the_synced_seq() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("s", "k", b"1").unwrap(); // 1
    a.set("s", "j", b"1").unwrap(); // 2
    a.mark_full_sync(2).unwrap();
    a.set("s", "k", b"2").unwrap(); // 3 supersedes 1
    a.set("s", "j", b"2").unwrap(); // 4 supersedes 2
    a.set("s", "k", b"3").unwrap(); // 5 supersedes 3
    let o6 = a.set("s", "j", b"3").unwrap(); // 6 supersedes 4
    clock.advance(SETTLE_MS);
    assert_eq!(a.compact().unwrap(), 4);
    let items = a.export_unsynced_local_ops().unwrap();
    // Holes 3..=4 lie above the boundary 2: one marker up to 4, nothing for 1..=2.
    assert!(matches!(
        items.as_slice(),
        [
            SyncItem::Covered { upto: 4, .. },
            SyncItem::Op(o5),
            SyncItem::Op(o)
        ] if o5.body.seq == 5 && *o == o6
    ));
}

#[test]
fn reset_keeps_seq() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    a.set("s", "k", b"1").unwrap();
    a.set("s", "j", b"2").unwrap();
    let last = a.set_add("s", "m", b"x").unwrap();
    a.apply(b.set("s", "b", b"b").unwrap(), &AcceptAll).unwrap();
    // Something pending too (a gap from B).
    let _ = b.set("s", "b", b"b2").unwrap();
    a.apply(b.set("s", "b", b"b3").unwrap(), &AcceptAll)
        .unwrap();
    assert_eq!(a.pending_len().unwrap(), 1);
    a.mark_full_sync(3).unwrap();
    a.reset_to_empty().unwrap();
    assert_eq!(a.get("s", "k").unwrap(), None);
    assert_eq!(a.get_entry("s", "b").unwrap(), None);
    assert!(a.list_keys("s", "", None, 100).unwrap().is_empty());
    assert!(!a.set_contains("s", "m", b"x").unwrap());
    assert!(a.version_vector().unwrap().0.is_empty());
    assert_eq!(a.pending_len().unwrap(), 0);
    assert!(
        a.diff_since(&VersionVector::default(), 100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(a.last_full_sync_ms().unwrap(), None);
    assert!(!a.is_stale().unwrap());
    let next = a.set("s", "k", b"again").unwrap();
    assert_eq!(next.body.seq, 4);
    assert!(next.body.hlc > last.body.hlc);
    // Survives a reopen.
    drop(a);
    let a = store(dir.path(), 1, &clock);
    assert_eq!(a.set("s", "k", b"x").unwrap().body.seq, 5);
}

#[test]
fn purge_namespace_tombstones_live_keys() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.set("p", "c", b"3").unwrap();
    s.set("p", "a", b"1").unwrap();
    s.set("p", "b", b"2").unwrap();
    s.set("p", "gone", b"x").unwrap();
    s.delete("p", "gone").unwrap();
    s.set_add("p", "m", b"x").unwrap();
    s.set("q", "a", b"other").unwrap();
    // An ordered winner: a plain (empty-order) delete would lose to it.
    s.set_ordered("p", "o", &[9], Some(b"ordered")).unwrap();
    let ops = s.purge_namespace("p").unwrap();
    let keys: Vec<_> = ops
        .iter()
        .map(|o| match &o.body.payload {
            Payload::Set { ns, key, value, .. } => {
                assert_eq!(ns, "p");
                assert!(value.is_none());
                key.clone()
            }
            Payload::SetAdd { .. } => panic!("purge emitted a set op"),
        })
        .collect();
    assert_eq!(keys, strings(&["a", "b", "c", "o"]));
    assert_eq!(s.get_entry("p", "o").unwrap().unwrap().order, vec![9]);
    assert!(s.list_keys("p", "", None, 100).unwrap().is_empty());
    assert_eq!(s.get("q", "a").unwrap(), Some(b"other".to_vec()));
    // Sets are grow-only: members survive a purge.
    assert!(s.set_contains("p", "m", b"x").unwrap());
    assert!(s.purge_namespace("p").unwrap().is_empty());
}

/// Re-applies exported items (ops with `apply`, markers with `apply_covered`).
fn reapply(to: &Store<ManualClock>, items: Vec<SyncItem>) {
    for item in items {
        match item {
            SyncItem::Op(o) => {
                assert_eq!(to.apply(o, &AcceptAll).unwrap(), Outcome::Applied);
            }
            SyncItem::Covered { origin, upto } => {
                // Exported by this node itself: the marker is its own bound.
                to.apply_covered(origin, upto, upto, &AcceptAll).unwrap()
            }
        }
    }
}

#[test]
fn export_after_compaction_reapplies_without_gaps() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("s", "k", b"1").unwrap();
    a.set("s", "k", b"2").unwrap();
    a.set("s", "k2", b"x").unwrap();
    clock.advance(SETTLE_MS + 1);
    assert_eq!(a.compact().unwrap(), 1);
    let items = a.export_unsynced_local_ops().unwrap();
    assert!(matches!(
        items.as_slice(),
        [
            SyncItem::Covered { upto: 1, .. },
            SyncItem::Op(_),
            SyncItem::Op(_)
        ]
    ));

    // A fresh second store re-applying the export.
    let c = store(dir.path(), 3, &clock);
    reapply(&c, items.clone());
    assert_eq!(c.get("s", "k").unwrap(), Some(b"2".to_vec()));
    assert_eq!(c.get("s", "k2").unwrap(), Some(b"x".to_vec()));
    assert_eq!(c.version_vector().unwrap().get(&node(1)), 3);
    assert_eq!(c.pending_len().unwrap(), 0);

    // The same store after a reset.
    a.reset_to_empty().unwrap();
    reapply(&a, items);
    assert_eq!(a.get("s", "k").unwrap(), Some(b"2".to_vec()));
    assert_eq!(a.get("s", "k2").unwrap(), Some(b"x".to_vec()));
    assert_eq!(a.version_vector().unwrap().get(&node(1)), 3);
    assert_eq!(a.pending_len().unwrap(), 0);
    assert_eq!(a.set("s", "k", b"3").unwrap().body.seq, 4);
}

#[test]
fn export_covers_holes_between_exported_ops() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("s", "k", b"1").unwrap(); // 1
    a.set("s", "j", b"1").unwrap(); // 2
    a.set("s", "k", b"2").unwrap(); // 3 supersedes 1
    a.set("s", "j", b"2").unwrap(); // 4 supersedes 2
    clock.advance(SETTLE_MS);
    assert_eq!(a.compact().unwrap(), 2);
    let items = a.export_unsynced_local_ops().unwrap();
    assert!(matches!(
        items.as_slice(),
        [
            SyncItem::Covered { upto: 2, .. },
            SyncItem::Op(o3),
            SyncItem::Op(o4)
        ] if o3.body.seq == 3 && o4.body.seq == 4
    ));
}

#[test]
fn gc_does_nothing_while_stale() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("s", "k", b"v").unwrap();
    a.delete("s", "k").unwrap();
    a.mark_full_sync(2).unwrap();
    clock.advance(TOMBSTONE_TTL_MS + 1);
    assert!(a.is_stale().unwrap());
    assert_eq!(a.gc().unwrap(), 0);
    assert!(a.get_entry("s", "k").unwrap().is_some());
    a.mark_full_sync(2).unwrap();
    assert_eq!(a.gc().unwrap(), 1);
}

#[test]
fn gc_keeps_keys_with_own_unsynced_ops() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    a.set("s", "own", b"v").unwrap(); // 1
    a.delete("s", "own").unwrap(); // 2
    // A's write of k (seq 3), then B's newer tombstone of k.
    a.set("s", "k", b"late").unwrap();
    clock.advance(5);
    a.apply(b.delete("s", "k").unwrap(), &AcceptAll).unwrap();
    assert_eq!(a.get("s", "k").unwrap(), None);
    clock.advance(TOMBSTONE_TTL_MS + 1);
    // Never synced: every own op may be unknown to the others.
    assert_eq!(a.gc().unwrap(), 0);
    // The peer confirmed seqs 1..=2 only: `own` goes, `k` holds unsynced seq 3.
    a.mark_full_sync(2).unwrap();
    assert_eq!(a.gc().unwrap(), 1);
    assert!(a.get_entry("s", "own").unwrap().is_none());
    assert!(a.get_entry("s", "k").unwrap().is_some());
    a.mark_full_sync(3).unwrap();
    assert_eq!(a.gc().unwrap(), 1);
    assert!(a.get_entry("s", "k").unwrap().is_none());
}

#[test]
fn gc_never_runs_on_own_ops_of_a_never_synced_store() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("s", "k", b"v").unwrap();
    a.delete("s", "k").unwrap();
    clock.advance(TOMBSTONE_TTL_MS + 1);
    assert!(!a.is_stale().unwrap());
    assert_eq!(a.gc().unwrap(), 0);
    a.mark_full_sync(2).unwrap();
    assert_eq!(a.gc().unwrap(), 1);
}

#[test]
fn late_arriving_old_tombstone_is_kept_for_ttl_after_arrival() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    // X is an offline device with its own clock.
    let x_clock = ManualClock::new(T0);
    let x = store(dir.path(), 3, &x_clock);
    let v = x.set("s", "k", b"v").unwrap();
    a.apply(v.clone(), &AcceptAll).unwrap();
    b.apply(v, &AcceptAll).unwrap();
    // X deletes k, then stays offline for 100 days.
    x_clock.advance(1);
    let tomb = x.delete("s", "k").unwrap();
    clock.advance(100 * DAY_MS);
    // A receives the tombstone (old by its HLC, new by arrival); B has not yet.
    assert_eq!(a.apply(tomb, &AcceptAll).unwrap(), Outcome::Applied);
    assert_eq!(
        a.gc().unwrap(),
        0,
        "a tombstone that just arrived is not GC'd"
    );
    // B catches up from A and must learn the deletion.
    feed(
        &b,
        &a,
        a.diff_since(&b.version_vector().unwrap(), 100).unwrap(),
    );
    assert_eq!(b.get("s", "k").unwrap(), None);
    assert!(b.get_entry("s", "k").unwrap().is_some());
    // 90 days after its arrival, it goes.
    clock.advance(TOMBSTONE_TTL_MS);
    assert_eq!(a.gc().unwrap(), 0);
    clock.advance(1);
    assert_eq!(a.gc().unwrap(), 1);
    assert!(a.get_entry("s", "k").unwrap().is_none());
}

#[test]
fn ordered_tombstone_is_never_gcd() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let w = store(dir.path(), 3, &clock);
    let c = store(dir.path(), 4, &clock);
    let t = w.set_ordered("s", "k", &[5], None).unwrap();
    a.apply(t.clone(), &AcceptAll).unwrap();
    b.apply(t, &AcceptAll).unwrap();
    clock.advance(TOMBSTONE_TTL_MS + 1);
    assert_eq!(a.gc().unwrap(), 0);
    assert_eq!(
        a.get_entry("s", "k").unwrap().map(|e| e.order),
        Some(vec![5])
    );
    // A later plain write loses to the ordered tombstone on every replica alike.
    let late = c.set("s", "k", b"late").unwrap();
    a.apply(late.clone(), &AcceptAll).unwrap();
    b.apply(late, &AcceptAll).unwrap();
    assert_eq!(a.get("s", "k").unwrap(), None);
    assert_eq!(
        a.get_entry("s", "k").unwrap(),
        b.get_entry("s", "k").unwrap()
    );
}

// ---- Task 7: removal retraction ----

use mesh_store::RemovalReport;

fn no_change() -> RemovalReport {
    RemovalReport {
        retracted_ops: 0,
        restored_keys: 0,
        unrestorable_keys: Vec::new(),
    }
}

#[test]
fn removal_retracts_and_restores_older_value() {
    let (dir, clock) = setup();
    let x = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    b.apply(x.set("s", "k", b"1").unwrap(), &AcceptAll).unwrap();
    b.apply(x.set("s", "k", b"2").unwrap(), &AcceptAll).unwrap();
    assert_eq!(b.get("s", "k").unwrap(), Some(b"2".to_vec()));
    let mut rx = b.subscribe();
    let r = b.on_removal(node(1), 1).unwrap();
    assert_eq!(r.retracted_ops, 1);
    assert_eq!(r.restored_keys, 1);
    assert!(r.unrestorable_keys.is_empty());
    assert_eq!(b.get("s", "k").unwrap(), Some(b"1".to_vec()));
    assert_eq!(b.get_entry("s", "k").unwrap().unwrap().seq, 1);
    assert_eq!(
        rx.try_recv().unwrap(),
        Change {
            ns: "s".into(),
            kind: ChangeKind::Key("k".into()),
            local: false
        }
    );
    // Retracted ops still count as covered.
    assert_eq!(b.vv_of(node(1)).unwrap(), 2);
    // The restored winner survives compaction; the retracted op is compacted later.
    clock.advance(SETTLE_MS + 1);
    b.compact().unwrap();
    assert_eq!(b.get("s", "k").unwrap(), Some(b"1".to_vec()));
}

#[test]
fn ops_above_cutoff_arriving_later_are_ignored_but_covered() {
    let (dir, clock) = setup();
    let x = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let op1 = x.set("s", "k", b"1").unwrap();
    let op2 = x.set("s", "k", b"2").unwrap();
    assert_eq!(b.on_removal(node(1), 1).unwrap(), no_change());
    assert_eq!(b.apply(op1, &AcceptAll).unwrap(), Outcome::Applied);
    assert_eq!(b.apply(op2, &AcceptAll).unwrap(), Outcome::Ignored);
    assert_eq!(b.vv_of(node(1)).unwrap(), 2);
    assert_eq!(b.get("s", "k").unwrap(), Some(b"1".to_vec()));
}

#[test]
fn removal_before_or_after_ops_gives_same_result() {
    let (dir, clock) = setup();
    let x = store(dir.path(), 1, &clock);
    let y = store(dir.path(), 4, &clock);
    let b = store(dir.path(), 2, &clock);
    let c = store(dir.path(), 3, &clock);
    let mut ops = vec![
        y.set("s", "k", b"y").unwrap(),
        y.set_add("s", "m", b"shared").unwrap(),
    ];
    clock.advance(10);
    ops.extend([
        x.set("s", "k", b"x1").unwrap(),         // seq 1: wins over y
        x.set("s", "only-x", b"a").unwrap(),     // seq 2
        x.set_add("s", "m", b"early").unwrap(),  // seq 3
        x.set("s", "k", b"x2").unwrap(),         // seq 4: retracted
        x.set("s", "late", b"z").unwrap(),       // seq 5: retracted
        x.set_add("s", "m", b"late").unwrap(),   // seq 6: retracted
        x.set_add("s", "m", b"shared").unwrap(), // seq 7: retracted, y keeps it
        x.delete("s", "only-x").unwrap(),        // seq 8: retracted
    ]);
    for op in ops.clone() {
        b.apply(op, &AcceptAll).unwrap();
    }
    let r = b.on_removal(node(1), 3).unwrap();
    assert_eq!(r.retracted_ops, 5);
    assert_eq!(r.restored_keys, 2);
    assert!(r.unrestorable_keys.is_empty(), "plain delete of `late`");

    assert_eq!(c.on_removal(node(1), 3).unwrap(), no_change());
    for op in ops {
        c.apply(op, &AcceptAll).unwrap();
    }
    for k in ["k", "only-x", "late"] {
        assert_eq!(
            b.get_entry("s", k).unwrap(),
            c.get_entry("s", k).unwrap(),
            "{k}"
        );
    }
    assert_eq!(b.get("s", "k").unwrap(), Some(b"x1".to_vec()));
    assert_eq!(b.get("s", "only-x").unwrap(), Some(b"a".to_vec()));
    assert_eq!(b.get("s", "late").unwrap(), None);
    assert_eq!(
        b.list_keys("s", "", None, 100).unwrap(),
        c.list_keys("s", "", None, 100).unwrap()
    );
    let members = b.set_members("s", "m", None, 100).unwrap();
    assert_eq!(members, c.set_members("s", "m", None, 100).unwrap());
    assert_eq!(members, vec![b"early".to_vec(), b"shared".to_vec()]);
    assert_eq!(b.version_vector().unwrap(), c.version_vector().unwrap());
}

#[test]
fn max_cutoff_wins_across_removals() {
    let (dir, clock) = setup();
    let x = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    b.apply(x.set("s", "k", b"1").unwrap(), &AcceptAll).unwrap();
    b.apply(x.set("s", "k", b"2").unwrap(), &AcceptAll).unwrap();
    b.apply(x.set_add("s", "m", b"a").unwrap(), &AcceptAll)
        .unwrap();
    b.apply(x.set("s", "k", b"4").unwrap(), &AcceptAll).unwrap();
    b.on_removal(node(1), 1).unwrap();
    assert_eq!(b.get("s", "k").unwrap(), Some(b"1".to_vec()));
    assert!(!b.set_contains("s", "m", b"a").unwrap());

    // Raise: seqs 2 and 3 are valid again and re-merged; seq 4 stays retracted.
    let mut rx = b.subscribe();
    let r = b.on_removal(node(1), 3).unwrap();
    assert_eq!(r.retracted_ops, 0);
    assert_eq!(r.restored_keys, 1);
    assert_eq!(b.get("s", "k").unwrap(), Some(b"2".to_vec()));
    assert!(b.set_contains("s", "m", b"a").unwrap());
    let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|c| c.kind)
        .collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.contains(&ChangeKind::Key("k".into())));
    assert!(got.contains(&ChangeKind::Set("m".into())));

    // Equal or lower: no change.
    assert_eq!(b.on_removal(node(1), 1).unwrap(), no_change());
    assert_eq!(b.on_removal(node(1), 3).unwrap(), no_change());
    assert_eq!(b.get("s", "k").unwrap(), Some(b"2".to_vec()));

    // Raising past everything brings back the latest value.
    b.on_removal(node(1), 10).unwrap();
    assert_eq!(b.get("s", "k").unwrap(), Some(b"4".to_vec()));

    // Re-merged losers are superseded again and compact; the winner stays.
    clock.advance(SETTLE_MS + 1);
    assert_eq!(b.compact().unwrap(), 2);
    assert_eq!(b.get("s", "k").unwrap(), Some(b"4".to_vec()));
    assert!(b.set_contains("s", "m", b"a").unwrap());
}

#[test]
fn set_adds_above_cutoff_are_retracted() {
    let (dir, clock) = setup();
    let x = store(dir.path(), 1, &clock);
    let y = store(dir.path(), 4, &clock);
    let b = store(dir.path(), 2, &clock);
    b.apply(x.set_add("s", "m", b"keep").unwrap(), &AcceptAll)
        .unwrap();
    b.apply(x.set_add("s", "m", b"m").unwrap(), &AcceptAll)
        .unwrap();
    b.apply(x.set_add("s", "m", b"both").unwrap(), &AcceptAll)
        .unwrap();
    b.apply(y.set_add("s", "m", b"both").unwrap(), &AcceptAll)
        .unwrap();
    let mut rx = b.subscribe();
    let r = b.on_removal(node(1), 1).unwrap();
    assert_eq!(r.retracted_ops, 2);
    assert!(b.set_contains("s", "m", b"keep").unwrap());
    assert!(!b.set_contains("s", "m", b"m").unwrap());
    assert!(b.set_contains("s", "m", b"both").unwrap());
    assert_eq!(
        rx.try_recv().unwrap(),
        Change {
            ns: "s".into(),
            kind: ChangeKind::Set("m".into()),
            local: false
        }
    );
}

#[test]
fn removal_after_settle_cannot_restore_and_warns() {
    let (dir, clock) = setup();
    let x = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    b.apply(x.set("s", "k", b"1").unwrap(), &AcceptAll).unwrap();
    b.apply(x.set("s", "k", b"2").unwrap(), &AcceptAll).unwrap();
    clock.advance(7 * DAY_MS + 1);
    assert_eq!(b.compact().unwrap(), 1);
    let r = b.on_removal(node(1), 1).unwrap();
    assert_eq!(b.get("s", "k").unwrap(), None);
    assert!(b.get_entry("s", "k").unwrap().is_none());
    assert_eq!(r.restored_keys, 0);
    assert_eq!(
        r.unrestorable_keys,
        vec![("s".to_string(), "k".to_string())]
    );
}

// ---- Batched apply ----

/// Everything readable about the keys / sets used by `apply_batch_equals_sequential_apply`.
fn batch_dump(s: &Store<ManualClock>) -> impl PartialEq + std::fmt::Debug {
    let entries: Vec<_> = ["k", "j", "o", "x"]
        .iter()
        .map(|k| s.get_entry("s", k).unwrap())
        .collect();
    (
        entries,
        s.list_keys("s", "", None, 100).unwrap(),
        s.set_members("s", "m", None, 100).unwrap(),
        s.version_vector().unwrap(),
        s.pending_len().unwrap(),
        s.diff_since(&VersionVector::default(), 1000).unwrap(),
    )
}

#[test]
fn apply_batch_equals_sequential_apply() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    let a1 = a.set("s", "k", b"a1").unwrap();
    let a2 = a.set_add("s", "m", b"x").unwrap();
    let a3 = a.delete("s", "j").unwrap();
    let a4 = a.set_ordered("s", "o", &[2], Some(b"a4")).unwrap();
    let b1 = b.set("s", "k", b"b1").unwrap();
    let b2 = b.set("s", "j", b"b2").unwrap();
    let b3 = b.set_add("s", "m", b"y").unwrap();
    let mut bad = b.set("s", "x", b"bad").unwrap();
    bad.body.seq = 9; // signature no longer verifies
    let equiv = hand_op(2, 1, T0, "x", b"equivocating");
    let skewed = hand_op(3, 1, T0 + 10 * DAY_MS, "x", b"future");
    // Gaps (a3 before a1), drains, duplicates, an equivocation, a bad signature, a
    // skewed op and an unknown origin (node 4, judged by `members`).
    let c1 = hand_op(4, 1, T0, "x", b"c1");
    let batch = vec![
        a3.clone(),
        b2.clone(),
        a1.clone(),
        b1.clone(),
        a2.clone(),
        a1.clone(),
        equiv,
        bad,
        skewed,
        c1,
        b3.clone(),
        a4.clone(),
        b2.clone(),
    ];
    let acc = members(&[node(1), node(2), node(3)]);

    let seq_store = store(dir.path(), 5, &clock);
    let bat_store = store(dir.path(), 6, &clock);
    let mut seq_rx = seq_store.subscribe();
    let mut bat_rx = bat_store.subscribe();
    let sequential: Vec<Outcome> = batch
        .iter()
        .map(|op| seq_store.apply(op.clone(), &acc).unwrap())
        .collect();
    let batched = bat_store.apply_batch(batch, &acc).unwrap();
    assert_eq!(batched, sequential);
    assert!(sequential.contains(&Outcome::Deferred(DeferReason::Gap)));
    assert!(sequential.contains(&Outcome::Rejected(RejectReason::Equivocation)));
    assert!(sequential.contains(&Outcome::Rejected(RejectReason::BadSig)));
    assert!(sequential.contains(&Outcome::Deferred(DeferReason::Skew)));
    assert!(sequential.contains(&Outcome::Deferred(DeferReason::UnknownOrigin)));
    assert!(sequential.contains(&Outcome::Duplicate));
    assert_eq!(batch_dump(&bat_store), batch_dump(&seq_store));
    // Same change events, in the same order.
    let drain = |rx: &mut tokio::sync::broadcast::Receiver<Change>| {
        std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>()
    };
    let events = drain(&mut seq_rx);
    assert!(!events.is_empty());
    assert_eq!(drain(&mut bat_rx), events);
    // An empty batch and an all-duplicate batch are fine.
    assert!(bat_store.apply_batch(Vec::new(), &acc).unwrap().is_empty());
    assert_eq!(
        bat_store.apply_batch(vec![a1, b1], &acc).unwrap(),
        vec![Outcome::Duplicate, Outcome::Duplicate]
    );
}

// ---- Bench (informational) ----

#[test]
#[ignore]
fn bench_apply_100k() {
    use std::time::Instant;
    const N: u64 = 100_000;
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    let ops: Vec<SignedOp> = (1..=N)
        .map(|i| hand_op(2, i, T0, &format!("k{}", i % 10_000), b"value"))
        .collect();
    let t = Instant::now();
    for op in ops {
        assert_eq!(s.apply(op, &AcceptAll).unwrap(), Outcome::Applied);
    }
    let apply = t.elapsed();
    let t = Instant::now();
    for i in 0..10_000u64 {
        assert!(s.get("s", &format!("k{i}")).unwrap().is_some());
    }
    let get = t.elapsed();
    println!(
        "apply {N} remote ops: {apply:?} ({:.0} ops/s); 10000 gets: {get:?} ({:.1} us/get)",
        N as f64 / apply.as_secs_f64(),
        get.as_secs_f64() * 1e6 / 10_000.0
    );

    // The same ops through `apply_batch`, 256 per batch (one transaction each).
    const BATCH: usize = 256;
    let s = store(dir.path(), 3, &clock);
    let ops: Vec<SignedOp> = (1..=N)
        .map(|i| hand_op(2, i, T0, &format!("k{}", i % 10_000), b"value"))
        .collect();
    let t = Instant::now();
    for chunk in ops.chunks(BATCH) {
        let outs = s.apply_batch(chunk.to_vec(), &AcceptAll).unwrap();
        assert!(outs.iter().all(|o| *o == Outcome::Applied));
    }
    let batched = t.elapsed();
    println!(
        "apply_batch {N} remote ops in batches of {BATCH}: {batched:?} ({:.0} ops/s)",
        N as f64 / batched.as_secs_f64()
    );
}
