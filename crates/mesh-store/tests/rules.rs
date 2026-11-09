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
    assert_eq!(a.version_vector_local_seq(), 1);
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
