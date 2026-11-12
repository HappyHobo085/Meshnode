//! Partial replication in the store (sub-spec 8a §3): scoped catch-up diffs and the
//! refusal of out-of-scope local writes.

mod common;

use common::{AcceptAll, node, store};
use mesh_hlc::ManualClock;
use mesh_proto::{Scope, VersionVector};
use mesh_store::{StoreError, SyncItem};

const T0: u64 = 1_700_000_000_000;

fn setup() -> (tempfile::TempDir, ManualClock) {
    (tempfile::tempdir().unwrap(), ManualClock::new(T0))
}

/// Seqs of the ops in `items`, with covered markers as `(0, upto)`.
fn shape(items: &[SyncItem]) -> Vec<(u64, u64)> {
    items
        .iter()
        .map(|i| match i {
            SyncItem::Op(op) => (op.body.seq, 0),
            SyncItem::Covered { upto, .. } => (0, *upto),
        })
        .collect()
}

#[test]
fn scoped_full_equals_diff_since() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    for i in 0..6 {
        let ns = if i % 2 == 0 { "_mesh" } else { "svc" };
        a.set(ns, &format!("k{i}"), b"v").unwrap();
    }
    a.__test_drop_op(node(1), 3).unwrap();
    a.__test_drop_op(node(1), 6).unwrap();
    // Full sends every op whatever its namespace; only compacted seqs are covered.
    let full = a
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::Full)
        .unwrap();
    assert_eq!(
        shape(&full),
        vec![(1, 0), (2, 0), (0, 3), (4, 0), (5, 0), (0, 6)]
    );
    let mut theirs = VersionVector::default();
    for have in [0, 1, 2] {
        theirs.set(node(1), have);
        for limit in [0, 1, 2, 3, 100] {
            assert_eq!(
                a.diff_since_scoped(&theirs, limit, &Scope::Full).unwrap(),
                a.diff_since(&theirs, limit).unwrap(),
                "have {have}, limit {limit}"
            );
        }
    }
}

#[test]
fn out_of_scope_run_becomes_one_covered() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("_mesh", "a", b"1").unwrap();
    a.set("svc", "b", b"2").unwrap();
    a.set("svc", "c", b"3").unwrap();
    a.set("_mesh", "d", b"4").unwrap();
    let items = a
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::client())
        .unwrap();
    assert_eq!(shape(&items), vec![(1, 0), (0, 3), (4, 0)]);
    match &items[1] {
        SyncItem::Covered { origin, upto } => {
            assert_eq!(*origin, node(1));
            assert_eq!(*upto, 3);
        }
        other => panic!("expected Covered, got {other:?}"),
    }
}

#[test]
fn out_of_scope_run_merges_with_compacted_gap() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("_mesh", "a", b"1").unwrap(); // 1
    a.set("svc", "b", b"2").unwrap(); // 2
    a.set("_mesh", "c", b"3").unwrap(); // 3 (compacted)
    a.set("svc", "d", b"4").unwrap(); // 4
    a.set("_mesh", "e", b"5").unwrap(); // 5 (compacted)
    a.set("_mesh", "f", b"6").unwrap(); // 6
    a.__test_drop_op(node(1), 3).unwrap();
    a.__test_drop_op(node(1), 5).unwrap();
    let items = a
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::client())
        .unwrap();
    assert_eq!(shape(&items), vec![(1, 0), (0, 5), (6, 0)]);

    // A compacted gap before an out-of-scope run merges too.
    let b = store(dir.path(), 2, &clock);
    b.set("_mesh", "a", b"1").unwrap(); // 1 (compacted)
    b.set("svc", "b", b"2").unwrap(); // 2
    b.set("_mesh", "c", b"3").unwrap(); // 3
    b.__test_drop_op(node(2), 1).unwrap();
    let items = b
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::client())
        .unwrap();
    assert_eq!(shape(&items), vec![(0, 2), (3, 0)]);
}

#[test]
fn trailing_out_of_scope_covered_upto_vv() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("_mesh", "a", b"1").unwrap(); // 1
    a.set("svc", "b", b"2").unwrap(); // 2
    a.set("svc", "c", b"3").unwrap(); // 3
    a.set("svc", "d", b"4").unwrap(); // 4 (compacted)
    a.__test_drop_op(node(1), 4).unwrap();
    let items = a
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::client())
        .unwrap();
    assert_eq!(shape(&items), vec![(1, 0), (0, 4)]);
    assert_eq!(a.version_vector().unwrap().get(&node(1)), 4);

    // Only out-of-scope ops past what the peer holds: one marker.
    let mut theirs = VersionVector::default();
    theirs.set(node(1), 1);
    let items = a.diff_since_scoped(&theirs, 100, &Scope::client()).unwrap();
    assert_eq!(shape(&items), vec![(0, 4)]);
}

#[test]
fn scoped_respects_limit() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set("_mesh", "a", b"1").unwrap(); // 1
    a.set("svc", "b", b"2").unwrap(); // 2
    a.set("_mesh", "c", b"3").unwrap(); // 3
    a.set("svc", "d", b"4").unwrap(); // 4
    let b = store(dir.path(), 2, &clock);
    b.set("svc", "x", b"1").unwrap();
    b.set("_mesh", "y", b"2").unwrap();
    let full = a
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::client())
        .unwrap();
    // `a` only knows its own origin here.
    assert_eq!(shape(&full), vec![(1, 0), (0, 2), (3, 0), (0, 4)]);
    for limit in 0..=5 {
        let items = a
            .diff_since_scoped(&VersionVector::default(), limit, &Scope::client())
            .unwrap();
        assert_eq!(items.len(), limit.min(full.len()), "limit {limit}");
        assert_eq!(items[..], full[..items.len()], "limit {limit}");
    }

    // Across origins: items from the second origin count toward the same limit.
    for item in b
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::Full)
        .unwrap()
    {
        if let SyncItem::Op(op) = item {
            a.apply(op, &AcceptAll).unwrap();
        }
    }
    let all = a
        .diff_since_scoped(&VersionVector::default(), 100, &Scope::client())
        .unwrap();
    assert_eq!(all.len(), 6);
    for limit in 0..=7 {
        let items = a
            .diff_since_scoped(&VersionVector::default(), limit, &Scope::client())
            .unwrap();
        assert_eq!(items[..], all[..limit.min(all.len())], "limit {limit}");
    }
}

#[test]
fn scope_defaults_to_full_and_is_settable() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    assert_eq!(a.scope(), Scope::Full);
    a.set_scope(Scope::client());
    assert_eq!(a.scope(), Scope::client());
    a.set_scope(Scope::Full);
    a.set("svc", "k", b"v").unwrap();
}

#[test]
fn partial_store_refuses_out_of_scope_write() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    // A live key to purge, written while Full.
    a.set("svc", "live", b"v").unwrap();
    a.set_scope(Scope::client());
    let seq = a.next_seq().unwrap();
    let vv = a.version_vector().unwrap();

    let is_oos = |r: Result<_, StoreError>| match r {
        Err(StoreError::OutOfScope { ns }) => assert_eq!(ns, "svc"),
        Err(e) => panic!("expected OutOfScope, got {e:?}"),
        Ok(_) => panic!("expected OutOfScope, got Ok"),
    };
    is_oos(a.set("svc", "k", b"v").map(|_| ()));
    is_oos(a.delete("svc", "k").map(|_| ()));
    is_oos(a.set_ordered("svc", "k", b"o", Some(b"v")).map(|_| ()));
    is_oos(a.set_ordered("svc", "k", b"o", None).map(|_| ()));
    is_oos(a.set_add("svc", "s", b"m").map(|_| ()));
    is_oos(a.purge_namespace("svc").map(|_| ()));

    // Nothing was written: no seq consumed, the live key survives.
    assert_eq!(a.next_seq().unwrap(), seq);
    assert_eq!(a.version_vector().unwrap(), vv);
    assert_eq!(a.get("svc", "live").unwrap(), Some(b"v".to_vec()));
    assert!(!a.set_contains("svc", "s", b"m").unwrap());

    // Node-local entries are not replicated, so the scope does not apply to them.
    a.local_set("svc", "nonce", b"n", 0).unwrap().unwrap();
    assert_eq!(a.local_get("svc", "nonce").unwrap(), Some(b"n".to_vec()));
    a.local_delete("svc", "nonce").unwrap();
}

#[test]
fn partial_store_accepts_mesh_write() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    a.set_scope(Scope::client());
    a.set("_mesh", "k", b"v").unwrap();
    a.set_ordered("_mesh", "o", b"x", Some(b"v")).unwrap();
    a.set_add("_mesh", "s", b"m").unwrap();
    a.delete("_mesh", "k").unwrap();
    assert_eq!(a.get("_mesh", "k").unwrap(), None);
    assert!(a.set_contains("_mesh", "s", b"m").unwrap());
    assert_eq!(a.purge_namespace("_mesh").unwrap().len(), 1);
    assert_eq!(a.next_seq().unwrap(), 6);
}

/// The store does not filter remote ops by scope: the sync session decides what a
/// partial node asks for and receives. A remote op in another namespace still applies.
#[test]
fn remote_ops_unaffected_by_scope() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    b.set_scope(Scope::client());
    let op = a.set("svc", "k", b"v").unwrap();
    let op2 = a.set_add("svc", "s", b"m").unwrap();
    b.apply(op, &AcceptAll).unwrap();
    b.apply_batch(vec![op2], &AcceptAll).unwrap();
    assert_eq!(b.get("svc", "k").unwrap(), Some(b"v".to_vec()));
    assert!(b.set_contains("svc", "s", b"m").unwrap());
    assert_eq!(b.version_vector().unwrap().get(&node(1)), 2);
}
