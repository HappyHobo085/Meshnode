//! `svc_local`: node-local, non-replicated service entries with a TTL.

mod common;

use common::{AcceptAll, store};
use mesh_hlc::ManualClock;
use mesh_proto::VersionVector;
use mesh_store::{LOCAL_MAX_KEY, LOCAL_MAX_KEYS, LOCAL_MAX_VALUE, SyncItem};

const T0: u64 = 1_700_000_000_000;

fn setup() -> (tempfile::TempDir, ManualClock) {
    (tempfile::tempdir().unwrap(), ManualClock::new(T0))
}

#[test]
fn local_not_replicated() {
    let (dir, clock) = setup();
    let a = store(dir.path(), 1, &clock);
    let b = store(dir.path(), 2, &clock);
    a.set("svc", "shared", b"r").unwrap();
    let vv_before = a.version_vector().unwrap();
    let seq_before = a.next_seq().unwrap();

    a.local_set("svc", "nonce", b"n", 0).unwrap().unwrap();
    assert_eq!(a.local_get("svc", "nonce").unwrap(), Some(b"n".to_vec()));
    // No op, no seq, no version-vector change.
    assert_eq!(a.next_seq().unwrap(), seq_before);
    assert_eq!(a.version_vector().unwrap(), vv_before);

    // A full catch-up from A carries only the replicated write.
    let items = a.diff_since(&VersionVector::default(), 1000).unwrap();
    assert_eq!(items.len(), 1);
    for item in items {
        match item {
            SyncItem::Op(op) => {
                b.apply(op, &AcceptAll).unwrap();
            }
            SyncItem::Covered { .. } => panic!("unexpected covered marker"),
        }
    }
    assert_eq!(b.get("svc", "shared").unwrap(), Some(b"r".to_vec()));
    assert_eq!(b.local_get("svc", "nonce").unwrap(), None);
    // Nor is it visible through the replicated API on A.
    assert_eq!(a.get("svc", "nonce").unwrap(), None);
    assert!(a.list_keys("svc", "", None, 10).unwrap() == ["shared"]);
}

#[test]
fn local_ttl_expires() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.local_set("svc", "short", b"1", 1_000).unwrap().unwrap();
    s.local_set("svc", "forever", b"2", 0).unwrap().unwrap();
    clock.advance(999);
    assert_eq!(s.local_get("svc", "short").unwrap(), Some(b"1".to_vec()));
    clock.advance(1);
    assert_eq!(s.local_get("svc", "short").unwrap(), None, "expired at ttl");
    assert_eq!(s.local_get("svc", "forever").unwrap(), Some(b"2".to_vec()));
    assert_eq!(s.local_count("svc").unwrap(), 1);

    // An overwrite resets the expiry; delete removes.
    s.local_set("svc", "short", b"3", 500).unwrap().unwrap();
    assert_eq!(s.local_get("svc", "short").unwrap(), Some(b"3".to_vec()));
    s.local_delete("svc", "short").unwrap();
    assert_eq!(s.local_get("svc", "short").unwrap(), None);
    s.local_delete("svc", "absent").unwrap();
}

#[test]
fn local_namespaces_are_separate() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.local_set("a", "k", b"from-a", 0).unwrap().unwrap();
    assert_eq!(s.local_get("b", "k").unwrap(), None);
    // `a.1` (a later generation) shares no entries with `a`.
    assert_eq!(s.local_get("a.1", "k").unwrap(), None);
    assert_eq!(s.local_get("a", "k").unwrap(), Some(b"from-a".to_vec()));
}

#[test]
fn local_value_cap_err() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.local_set("svc", "max", &vec![1; LOCAL_MAX_VALUE], 0)
        .unwrap()
        .unwrap();
    let err = s
        .local_set("svc", "big", &vec![1; LOCAL_MAX_VALUE + 1], 0)
        .unwrap()
        .unwrap_err();
    assert!(err.contains("value"), "{err}");
    assert_eq!(s.local_get("svc", "big").unwrap(), None);
    let long_key = "k".repeat(LOCAL_MAX_KEY + 1);
    assert!(s.local_set("svc", &long_key, b"v", 0).unwrap().is_err());
}

#[test]
fn local_key_cap_err() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    // All but one key live forever; ten more expire after 1 s.
    s.local_fill_for_test("svc", "f", LOCAL_MAX_KEYS - 11, 0)
        .unwrap();
    s.local_fill_for_test("svc", "e", 10, T0 + 1_000).unwrap();
    s.local_set("svc", "last", b"v", 0).unwrap().unwrap();
    assert_eq!(s.local_count("svc").unwrap(), LOCAL_MAX_KEYS);

    let err = s
        .local_set("svc", "one-more", b"v", 0)
        .unwrap()
        .unwrap_err();
    assert!(err.contains("keys"), "{err}");
    // Overwriting an existing key is fine at the cap; other namespaces are unaffected.
    s.local_set("svc", "last", b"w", 0).unwrap().unwrap();
    s.local_set("other", "k", b"v", 0).unwrap().unwrap();

    // Expired entries do not count: once they lapse, there is room again.
    clock.advance(1_000);
    s.local_set("svc", "one-more", b"v", 0).unwrap().unwrap();
    // Deleting frees a slot too.
    s.local_delete("svc", "last").unwrap();
    s.local_set("svc", "again", b"v", 0).unwrap().unwrap();
}

#[test]
fn local_sweep_deletes_expired() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.local_set("a", "x", b"1", 1_000).unwrap().unwrap();
    s.local_set("a", "y", b"2", 5_000).unwrap().unwrap();
    s.local_set("b", "z", b"3", 1_000).unwrap().unwrap();
    s.local_set("b", "keep", b"4", 0).unwrap().unwrap();
    assert_eq!(s.local_sweep(T0 + 999).unwrap(), 0);
    assert_eq!(s.local_sweep(T0 + 1_000).unwrap(), 2);
    assert_eq!(s.local_sweep(T0 + 1_000).unwrap(), 0);
    assert_eq!(s.local_namespaces().unwrap(), ["a", "b"]);
    assert_eq!(s.local_sweep(T0 + 10_000).unwrap(), 1);
    assert_eq!(s.local_namespaces().unwrap(), ["b"]);
    assert_eq!(s.local_get("b", "keep").unwrap(), Some(b"4".to_vec()));
}

#[test]
fn local_drop_namespace_removes_everything_in_it() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.local_set("svc", "x", b"1", 0).unwrap().unwrap();
    s.local_set("svc", "y", b"2", 1_000).unwrap().unwrap();
    s.local_set("svc.1", "x", b"3", 0).unwrap().unwrap();
    assert_eq!(s.local_drop_namespace("svc").unwrap(), 2);
    assert_eq!(s.local_get("svc", "x").unwrap(), None);
    assert_eq!(s.local_get("svc.1", "x").unwrap(), Some(b"3".to_vec()));
    assert_eq!(s.local_namespaces().unwrap(), ["svc.1"]);
}

#[test]
fn local_survives_reopen_and_reset() {
    let (dir, clock) = setup();
    {
        let s = store(dir.path(), 1, &clock);
        s.local_set("svc", "n", b"1", 0).unwrap().unwrap();
    }
    let s = store(dir.path(), 1, &clock);
    assert_eq!(s.local_get("svc", "n").unwrap(), Some(b"1".to_vec()));
    // A re-bootstrap wipes replicated state only: replay bookkeeping must outlive it.
    s.set("svc", "r", b"x").unwrap();
    s.reset_to_empty().unwrap();
    assert_eq!(s.get("svc", "r").unwrap(), None);
    assert_eq!(s.local_get("svc", "n").unwrap(), Some(b"1".to_vec()));
    // Local writes work while replicated writes are refused (R18).
    assert!(s.set("svc", "r", b"y").is_err());
    s.local_set("svc", "m", b"2", 0).unwrap().unwrap();
}

#[test]
fn local_cap_refusal_does_not_scan() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.local_fill_for_test("svc", "f", LOCAL_MAX_KEYS - 1, 0)
        .unwrap();
    // The namespace is full; its earliest expiry is T0 + 5 s.
    s.local_set("svc", "t", b"v", 5_000).unwrap().unwrap();
    let before = s.local_rows_scanned();
    for i in 0..50 {
        assert!(
            s.local_set("svc", &format!("n{i}"), b"v", 0)
                .unwrap()
                .is_err()
        );
    }
    assert_eq!(
        s.local_rows_scanned(),
        before,
        "nothing can have expired yet: refusals read no rows"
    );

    // Once the earliest expiry passes, one scan frees its slot.
    clock.advance(5_000);
    s.local_set("svc", "n", b"v", 0).unwrap().unwrap();
    let after = s.local_rows_scanned();
    assert!(after > before);
    // Full again with nothing that can expire: refusals stay O(1).
    for i in 0..50 {
        assert!(
            s.local_set("svc", &format!("m{i}"), b"v", 0)
                .unwrap()
                .is_err()
        );
    }
    assert_eq!(s.local_rows_scanned(), after);
}

#[test]
fn local_cap_stale_due_costs_one_scan() {
    let (dir, clock) = setup();
    let s = store(dir.path(), 1, &clock);
    s.local_fill_for_test("svc", "f", LOCAL_MAX_KEYS - 1, 0)
        .unwrap();
    // An expiring key that is deleted leaves a stale (early) sweep-due time behind.
    s.local_set("svc", "gone", b"v", 1_000).unwrap().unwrap();
    s.local_delete("svc", "gone").unwrap();
    s.local_set("svc", "last", b"v", 0).unwrap().unwrap();
    clock.advance(1_000);
    let before = s.local_rows_scanned();
    assert!(s.local_set("svc", "x", b"v", 0).unwrap().is_err());
    let once = s.local_rows_scanned();
    assert!(once > before, "the stale bound triggers one scan");
    // That scan made the bound exact (and was kept despite the refusal): no more scans.
    for i in 0..20 {
        assert!(
            s.local_set("svc", &format!("y{i}"), b"v", 0)
                .unwrap()
                .is_err()
        );
    }
    assert_eq!(s.local_rows_scanned(), once);
}
