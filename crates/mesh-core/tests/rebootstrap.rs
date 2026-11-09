//! Stale-node re-bootstrap (spec §7, §11.2): crash-safe export, reset, catch-up, re-apply,
//! and own edits too old to re-apply.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{T, converged, test_cfg, wait_until};
use mesh_core::local::{load_or_create_key, read_expired_edits, read_genesis, rebootstrap_pending};
use mesh_core::membership::{MEMBERS, MESH_NS, addr_key, member_bytes};
use mesh_core::{MaintJob, Node, NodeError, NodeEvent, NodeStatus};
use mesh_hlc::ManualClock;
use mesh_proto::{NodeAddrBytes, NodeId, Payload, Role};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep, timeout};

const DAY: u64 = 86_400_000;
const NS: &str = "n";

fn id_of_dir(dir: &Path) -> NodeId {
    NodeId(load_or_create_key(dir).unwrap().verifying_key().to_bytes())
}

fn online(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id && m.online)
}

async fn own_addr(n: &Node) -> NodeAddrBytes {
    let raw = n
        .store()
        .get(MESH_NS, &addr_key(&n.node_id()))
        .await
        .unwrap()
        .expect("addr register written");
    NodeAddrBytes(raw)
}

async fn val(n: &Node, key: &str) -> Option<Vec<u8>> {
    n.store().get(NS, key).await.unwrap()
}

async fn vv_of(n: &Node, origin: NodeId) -> u64 {
    n.store()
        .call(move |s| Ok(s.version_vector()?.get(&origin)))
        .await
        .unwrap()
}

async fn synced_self_seq(n: &Node) -> Option<u64> {
    n.store().call(|s| s.synced_self_seq()).await.unwrap()
}

async fn last_full_sync(n: &Node) -> Option<u64> {
    n.store().call(|s| s.last_full_sync_ms()).await.unwrap()
}

/// Waits until a peer confirmed holding every own op of `n` (its `synced_self_seq` is its
/// own `vv`), so nothing written so far counts as unsynced.
async fn all_own_ops_synced(n: &Node) {
    let me = n.node_id();
    wait_until("own ops confirmed by a peer", T, || async {
        let seq = vv_of(n, me).await;
        seq > 0 && synced_self_seq(n).await == Some(seq)
    })
    .await;
}

/// Waits until `n` completed a full sync at or after wall time `ms`.
async fn full_sync_since(n: &Node, ms: u64) {
    wait_until("a full sync at the new time", T, || async {
        last_full_sync(n).await.is_some_and(|t| t >= ms)
    })
    .await;
}

/// The events of one re-bootstrap on `rx`, up to and including `RebootstrapDone`.
async fn rebootstrap_events(rx: &mut broadcast::Receiver<NodeEvent>) -> Vec<NodeEvent> {
    let mut seen = Vec::new();
    let collect = async {
        loop {
            match rx.recv().await {
                Ok(
                    e @ (NodeEvent::RebootstrapStarting
                    | NodeEvent::ExpiredOfflineEdits { .. }
                    | NodeEvent::RebootstrapDone),
                ) => {
                    let done = e == NodeEvent::RebootstrapDone;
                    seen.push(e);
                    if done {
                        return;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => panic!("lost {n} events"),
                Err(broadcast::error::RecvError::Closed) => panic!("event channel closed"),
            }
        }
    };
    timeout(T, collect)
        .await
        .unwrap_or_else(|_| panic!("no RebootstrapDone; saw {seen:?}"));
    seen
}

/// Founder `b` and members `a` and `c` (added by `b`), all connected and converged, with
/// every own op of `a` confirmed by a peer.
struct Trio {
    a: Node,
    b: Node,
    c: Node,
    clocks: [ManualClock; 3],
    dirs: [TempDir; 3],
}

async fn trio() -> Trio {
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let (cfg_a, ca) = test_cfg(dirs[0].path(), "a");
    let (cfg_b, cb) = test_cfg(dirs[1].path(), "b");
    let (cfg_c, cc) = test_cfg(dirs[2].path(), "c");
    // One shared starting time ("day 0").
    let day0 = cb.now();
    ca.set(day0);
    cc.set(day0);
    let b = Node::create_mesh(cfg_b).await.unwrap();
    for d in [&dirs[0], &dirs[2]] {
        b.store()
            .set_add(
                MESH_NS,
                MEMBERS,
                member_bytes(&id_of_dir(d.path()), Role::Host).to_vec(),
            )
            .await
            .unwrap();
    }
    let genesis = read_genesis(dirs[1].path()).unwrap().unwrap();
    let hint = vec![(b.node_id(), own_addr(&b).await)];
    let a = Node::start_member_for_test(cfg_a, genesis.clone(), hint.clone())
        .await
        .unwrap();
    let c = Node::start_member_for_test(cfg_c, genesis, hint)
        .await
        .unwrap();
    let ids = [a.node_id(), b.node_id(), c.node_id()];
    for n in [&a, &b, &c] {
        wait_until("full mesh", T, || async {
            ids.iter().all(|id| *id == n.node_id() || online(n, *id))
        })
        .await;
    }
    converged(&[&a, &b, &c], T).await;
    all_own_ops_synced(&a).await;
    Trio {
        a,
        b,
        c,
        clocks: [ca, cb, cc],
        dirs,
    }
}

trait Now {
    fn now(&self) -> u64;
}

impl Now for ManualClock {
    fn now(&self) -> u64 {
        mesh_hlc::WallClock::now_ms(self)
    }
}

/// Spec §11.2 `stale_rebootstrap` steps 1–4: A is cut off and writes `n/k` (soon too old to
/// re-apply); B deletes `n/k` and the tombstone is GC'd on B and C at day 92; A writes the
/// recent `n/recent`. Returns A's `synced_self_seq`. Nothing is healed yet.
async fn stale_scenario(t: &Trio) -> u64 {
    let (a, b, c) = (&t.a, &t.b, &t.c);
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());
    let day0 = t.clocks[1].now();
    let set_day = |d: u64| {
        for clock in &t.clocks {
            clock.set(day0 + d * DAY);
        }
    };

    // 1. B writes n/k = b; converged.
    b.store().set(NS, "k", b"b".to_vec()).await.unwrap();
    converged(&[a, b, c], T).await;
    all_own_ops_synced(a).await;

    // 2. A is cut off and writes n/k = a above its synced seq.
    a.partition(&[ib, ic]);
    b.partition(&[ia]);
    c.partition(&[ia]);
    wait_until("A cut off", T, || async {
        !online(a, ib) && !online(a, ic) && !online(b, ia) && !online(c, ia)
    })
    .await;
    let s = synced_self_seq(a).await.expect("A completed a full sync");
    a.store().set(NS, "k", b"a".to_vec()).await.unwrap();
    assert!(vv_of(a, ia).await > s, "A's edit is unsynced");

    // 3. Day 1: B deletes n/k. B and C keep syncing (never stale) up to day 92.
    set_day(1);
    b.store().delete(NS, "k").await.unwrap();
    wait_until("C holds the tombstone", T, || async {
        c.store()
            .call(|s| s.get_entry(NS, "k"))
            .await
            .unwrap()
            .is_some_and(|e| e.value.is_none())
    })
    .await;
    for d in [1, 50, 92] {
        set_day(d);
        full_sync_since(b, day0 + d * DAY).await;
        full_sync_since(c, day0 + d * DAY).await;
    }
    b.maint_now(MaintJob::Gc).await.unwrap();
    c.maint_now(MaintJob::Gc).await.unwrap();
    for n in [b, c] {
        let e = n.store().call(|s| s.get_entry(NS, "k")).await.unwrap();
        assert_eq!(e, None, "tombstone GC'd");
    }

    // 4. Day 92: A writes a recent edit.
    a.store().set(NS, "recent", b"1".to_vec()).await.unwrap();
    s
}

async fn heal_all(t: &Trio) {
    t.a.heal();
    t.b.heal();
    t.c.heal();
}

/// Spec §11.2 step 5, after the re-bootstrap: no resurrection, the recent edit everywhere.
async fn assert_outcome(a: &Node, b: &Node, c: &Node) {
    converged(&[a, b, c], T).await;
    for n in [a, b, c] {
        assert_eq!(val(n, "k").await, None, "n/k must not come back");
        assert_eq!(val(n, "recent").await, Some(b"1".to_vec()), "n/recent");
    }
    assert_eq!(a.status(), NodeStatus::Running);
}

fn expired_keys(dir: &Path) -> Vec<(String, String)> {
    read_expired_edits(dir)
        .unwrap()
        .iter()
        .map(|op| match &op.body.payload {
            Payload::Set { ns, key, .. } => (ns.clone(), key.clone()),
            other => panic!("expired SetAdd: {other:?}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_rebootstrap() {
    let t = trio().await;
    stale_scenario(&t).await;
    let mut events = t.a.subscribe();
    heal_all(&t).await;

    let seen = rebootstrap_events(&mut events).await;
    assert_eq!(
        seen,
        vec![
            NodeEvent::RebootstrapStarting,
            NodeEvent::ExpiredOfflineEdits {
                count: 1,
                keys: vec![(NS.to_owned(), "k".to_owned())],
            },
            NodeEvent::RebootstrapDone,
        ]
    );
    assert_outcome(&t.a, &t.b, &t.c).await;
    let dir = t.dirs[0].path();
    assert!(
        !rebootstrap_pending(dir).unwrap(),
        "rebootstrap.bin deleted"
    );
    assert_eq!(expired_keys(dir), vec![(NS.to_owned(), "k".to_owned())]);
    let Trio {
        a, b, c, dirs: _d, ..
    } = t;
    for n in [a, b, c] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebootstrap_resumes_after_crash() {
    let t = trio().await;
    let s = stale_scenario(&t).await;
    t.a.crash_after_reset();
    heal_all(&t).await;

    // The node stops itself right after step 3: export persisted, store reset.
    wait_until("A crashed after the reset", T, || async {
        t.a.is_stopped()
    })
    .await;
    assert_eq!(
        t.a.status(),
        NodeStatus::Rebootstrapping {
            waiting_for_self_seq: s
        }
    );
    let dir = t.dirs[0].path().to_path_buf();
    assert!(
        rebootstrap_pending(&dir).unwrap(),
        "rebootstrap.bin written before the reset"
    );

    let Trio {
        a,
        b,
        c,
        clocks,
        dirs: _d,
    } = t;
    timeout(T, a.shutdown()).await.expect("shutdown timed out");
    // Reopen on the same clock; retry while the store file is still being released.
    let deadline = Instant::now() + T;
    let a = loop {
        let mut cfg = test_cfg(&dir, "a").0;
        cfg.wall = Arc::new(clocks[0].clone());
        match Node::open(cfg).await {
            Ok(n) => break n,
            Err(e) if Instant::now() < deadline => {
                eprintln!("reopen failed, retrying: {e}");
                sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("reopen failed: {e}"),
        }
    };

    // No hints: the resumed run dials the peers remembered in rebootstrap.bin (discovery is
    // off and the reset store holds no addresses).
    wait_until("the resumed re-bootstrap completes", T, || async {
        a.status() == NodeStatus::Running && !rebootstrap_pending(&dir).unwrap()
    })
    .await;
    assert_outcome(&a, &b, &c).await;
    assert_eq!(expired_keys(&dir), vec![(NS.to_owned(), "k".to_owned())]);
    for n in [a, b, c] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

/// A stale and re-bootstrapping, connected only to C, which lacks A's synced ops (only B
/// holds them, and B is cut off from both). C knows A as a member.
struct StuckOnC {
    a: Node,
    b: Node,
    c: Node,
    /// A's `synced_self_seq`.
    s: u64,
    events: broadcast::Receiver<NodeEvent>,
    _dirs: [TempDir; 3],
}

async fn stuck_on_c() -> StuckOnC {
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let (cfg_a, clock_a) = test_cfg(dirs[0].path(), "a");
    let cfg_b = test_cfg(dirs[1].path(), "b").0;
    let cfg_c = test_cfg(dirs[2].path(), "c").0;
    let (ia, ic) = (id_of_dir(dirs[0].path()), id_of_dir(dirs[2].path()));

    // B founds the mesh and records A and C as members.
    let b = Node::create_mesh(cfg_b).await.unwrap();
    let ib = b.node_id();
    for id in [ia, ic] {
        b.store()
            .set_add(MESH_NS, MEMBERS, member_bytes(&id, Role::Host).to_vec())
            .await
            .unwrap();
    }
    let genesis = read_genesis(dirs[1].path()).unwrap().unwrap();
    let hint = vec![(ib, own_addr(&b).await)];

    // C syncs with B (so it knows A as a member), then loses B before A ever writes.
    let c = Node::start_member_for_test(cfg_c, genesis.clone(), hint.clone())
        .await
        .unwrap();
    wait_until("C has B's records", T, || async {
        vv_of(&c, ib).await == vv_of(&b, ib).await && vv_of(&b, ic).await > 0
    })
    .await;
    c.partition(&[ia, ib]);
    b.partition(&[ic]);
    wait_until("B and C apart", T, || async {
        !online(&b, ic) && !online(&c, ib)
    })
    .await;

    // A syncs with B only, then goes offline: only B holds A's synced ops.
    let a = Node::start_member_for_test(cfg_a, genesis, hint)
        .await
        .unwrap();
    wait_until("A knows C's address", T, || async {
        a.store()
            .get(MESH_NS, &addr_key(&ic))
            .await
            .unwrap()
            .is_some()
    })
    .await;
    all_own_ops_synced(&a).await;
    let s = synced_self_seq(&a).await.unwrap();
    a.partition(&[ib]);
    b.partition(&[ia]);
    wait_until("A offline", T, || async {
        !online(&a, ib) && !online(&b, ia)
    })
    .await;
    assert_eq!(vv_of(&c, ia).await, 0, "C holds no op of A");

    // 81 days later (A's clock only) A meets only C.
    clock_a.set(clock_a.now() + 81 * DAY);
    let events = a.subscribe();
    c.heal();
    wait_until("A is re-bootstrapping", T, || async {
        a.status()
            == NodeStatus::Rebootstrapping {
                waiting_for_self_seq: s,
            }
    })
    .await;
    wait_until("A caught up with C", T, || async {
        vv_of(&a, ib).await >= vv_of(&c, ib).await
            && vv_of(&a, ic).await >= vv_of(&c, ic).await
            && vv_of(&a, ic).await > 0
    })
    .await;
    // C lacks A's synced ops, so the re-bootstrap cannot finish.
    assert!(vv_of(&a, ia).await < s);
    assert_eq!(
        a.status(),
        NodeStatus::Rebootstrapping {
            waiting_for_self_seq: s
        }
    );
    StuckOnC {
        a,
        b,
        c,
        s,
        events,
        _dirs: dirs,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebootstrap_waits_for_self_seq() {
    let StuckOnC {
        a,
        b,
        c,
        s,
        mut events,
        _dirs,
    } = stuck_on_c().await;
    let r = a.store().set(NS, "x", b"1".to_vec()).await;
    assert!(matches!(r, Err(NodeError::Rebootstrapping)), "{r:?}");
    let r = a.store().set_ordered(NS, "x", vec![1], None).await;
    assert!(matches!(r, Err(NodeError::Rebootstrapping)), "{r:?}");
    let r = a.store().purge_namespace(NS).await;
    assert!(matches!(r, Err(NodeError::Rebootstrapping)), "{r:?}");

    // B comes back: A gets its own ops and finishes.
    b.heal();
    a.heal();
    common::wait_event(&mut events, |e| *e == NodeEvent::RebootstrapDone, T).await;
    assert_eq!(a.status(), NodeStatus::Running);
    assert!(vv_of(&a, a.node_id()).await >= s);
    converged(&[&a, &b, &c], T).await;
    // Local writes are accepted again (B and C hold this one back: A's clock is ahead).
    a.store().set(NS, "x", b"1".to_vec()).await.unwrap();
    for n in [a, b, c] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_while_stale_ends_removed() {
    let StuckOnC { a, b, c, _dirs, .. } = stuck_on_c().await;
    let (ia, ic) = (a.node_id(), c.node_id());
    let dir = _dirs[0].path().to_path_buf();
    assert!(rebootstrap_pending(&dir).unwrap());

    // C removes A; the record reaches A's half-refilled store (by hand: C also closes A).
    c.remove(ia).await.unwrap();
    a.deliver_from(&c).await.unwrap();

    wait_until("A ends Removed", T, || async {
        a.status() == NodeStatus::Removed { by: ic }
    })
    .await;
    wait_until("the abandoned run cleaned up", T, || async {
        !rebootstrap_pending(&dir).unwrap()
    })
    .await;
    let r = a.store().set(NS, "x", b"1".to_vec()).await;
    assert!(matches!(r, Err(NodeError::Removed)), "{r:?}");
    assert_eq!(a.status(), NodeStatus::Removed { by: ic });
    for n in [a, b, c] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_writes_race_rebootstrap_start() {
    let t = trio().await;
    let (a, b, c) = (&t.a, &t.b, &t.c);
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());
    a.partition(&[ib, ic]);
    b.partition(&[ia]);
    c.partition(&[ia]);
    wait_until("A cut off", T, || async {
        !online(a, ib) && !online(a, ic) && !online(b, ia) && !online(c, ia)
    })
    .await;
    // All clocks move together; B and C keep syncing so only A goes stale.
    let day0 = t.clocks[1].now();
    for d in [40, 81] {
        for clock in &t.clocks {
            clock.set(day0 + d * DAY);
        }
        full_sync_since(b, day0 + d * DAY).await;
        full_sync_since(c, day0 + d * DAY).await;
    }

    // Several writers hammer A while the re-bootstrap starts.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut writers = Vec::new();
    for w in 0..4 {
        let (store, stop) = (a.store().clone(), stop.clone());
        writers.push(tokio::spawn(async move {
            let (mut ok, mut refused) = (Vec::new(), 0usize);
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let key = format!("w{w}-{i}");
                match store.set(NS, &key, b"v".to_vec()).await {
                    Ok(()) => ok.push(key),
                    Err(NodeError::Rebootstrapping) => {
                        refused += 1;
                        tokio::task::yield_now().await;
                    }
                    Err(e) => panic!("unexpected write error: {e}"),
                }
                i += 1;
            }
            (ok, refused)
        }));
    }
    let mut events = a.subscribe();
    wait_until("writes are landing", T, || async {
        vv_of(a, ia).await > 20
    })
    .await;
    heal_all(&t).await;
    common::wait_event(&mut events, |e| *e == NodeEvent::RebootstrapDone, T).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let (mut ok, mut refused) = (Vec::new(), 0);
    for w in writers {
        let (o, r) = w.await.unwrap();
        ok.extend(o);
        refused += r;
    }
    assert!(refused > 0, "no write hit the re-bootstrap");

    // Writes work again; every accepted write survives everywhere.
    a.store().set(NS, "after", b"1".to_vec()).await.unwrap();
    ok.push("after".to_owned());
    converged(&[a, b, c], T).await;
    for n in [a, b, c] {
        for key in &ok {
            assert!(
                val(n, key).await.is_some(),
                "{key} lost on {}",
                n.node_id().short()
            );
        }
    }
    let Trio {
        a, b, c, dirs: _d, ..
    } = t;
    for n in [a, b, c] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebootstrap_waits_for_own_membership_before_reapply() {
    let t = trio().await;
    let (a, b, c) = (&t.a, &t.b, &t.c);
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());
    a.partition(&[ib, ic]);
    b.partition(&[ia]);
    c.partition(&[ia]);
    wait_until("A cut off", T, || async {
        !online(a, ib) && !online(a, ic) && !online(b, ia) && !online(c, ia)
    })
    .await;
    a.store().set(NS, "offline", b"1".to_vec()).await.unwrap();
    // s == 0: step 4 holds at once after the reset, before this node's `members` record
    // (written by B) is back, so the re-applied own ops are held back as `UnknownOrigin`.
    a.store()
        .call_unchecked(|s| s.__test_set_meta_u64("synced_self_seq", 0))
        .await
        .unwrap();
    let day0 = t.clocks[1].now();
    for d in [40, 81] {
        for clock in &t.clocks {
            clock.set(day0 + d * DAY);
        }
        full_sync_since(b, day0 + d * DAY).await;
        full_sync_since(c, day0 + d * DAY).await;
    }

    let mut events = a.subscribe();
    heal_all(&t).await;
    common::wait_event(&mut events, |e| *e == NodeEvent::RebootstrapDone, T).await;
    assert_eq!(a.status(), NodeStatus::Running);
    // The unsynced edit was neither dropped nor covered over: it reaches every node.
    converged(&[a, b, c], T).await;
    for n in [a, b, c] {
        assert_eq!(val(n, "offline").await, Some(b"1".to_vec()), "n/offline");
    }
    a.store().set(NS, "after", b"1".to_vec()).await.unwrap();
    wait_until("A's later write reaches B", T, || async {
        val(b, "after").await.is_some()
    })
    .await;
    let Trio {
        a, b, c, dirs: _d, ..
    } = t;
    for n in [a, b, c] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}
