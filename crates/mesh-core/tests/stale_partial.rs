//! Staleness and re-bootstrap with client-role members (spec §7 with sub-spec 8a §3,
//! ruling C1c): a round with a Fresh peer (host or client) refreshes a node's last full
//! sync; staleness never blocks syncing; the only reset is a stale node meeting a Fresh
//! Full peer; an Empty (never synced) node turns Fresh only through a Fresh peer and
//! inherits staleness from a Stale one. A re-bootstrap completes only after a pull from
//! a Full peer.

mod common;

use std::time::Duration;

use common::{T, converged, test_cfg, wait_until};
use mesh_core::{Node, NodeEvent, NodeStatus};
use mesh_hlc::ManualClock;
use mesh_proto::{InviteTicket, NodeId, Role, VersionVector};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};

const TTL: Duration = Duration::from_secs(900);
const DAY: u64 = 86_400_000;
const NS: &str = "svc";

fn now_of(c: &ManualClock) -> u64 {
    mesh_hlc::WallClock::now_ms(c)
}

async fn vv_of(n: &Node) -> VersionVector {
    n.store().call(|s| s.version_vector()).await.unwrap()
}

async fn val(n: &Node, key: &str) -> Option<Vec<u8>> {
    n.store().get(NS, key).await.unwrap()
}

async fn last_full_sync(n: &Node) -> Option<u64> {
    n.store().call(|s| s.last_full_sync_ms()).await.unwrap()
}

async fn is_stale(n: &Node) -> bool {
    n.store().call(|s| s.is_stale()).await.unwrap()
}

async fn full_sync_since(n: &Node, ms: u64) {
    wait_until("a full sync", T, || async {
        last_full_sync(n).await.is_some_and(|t| t >= ms)
    })
    .await;
}

async fn join(
    dir: &TempDir,
    name: &str,
    clock_at: u64,
    ticket: InviteTicket,
) -> (Node, ManualClock) {
    let (cfg, clock) = test_cfg(dir.path(), name);
    clock.set(clock_at);
    let n = timeout(T, Node::join(cfg, ticket))
        .await
        .expect("join timed out")
        .expect("join");
    assert!(n.wait_first_sync(T).await);
    (n, clock)
}

fn id_of(dir: &TempDir) -> NodeId {
    NodeId(
        mesh_core::local::load_or_create_key(dir.path())
            .unwrap()
            .verifying_key()
            .to_bytes(),
    )
}

/// Hosts A (founder), B and D, client C, on one shared clock ("day 0"), converged.
struct Quad {
    a: Node,
    b: Node,
    c: Node,
    d: Node,
    /// a, b, c, d
    clocks: [ManualClock; 4],
    dirs: Vec<TempDir>,
    day0: u64,
}

async fn quad() -> Quad {
    let dirs: Vec<TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (cfg_a, ca) = test_cfg(dirs[0].path(), "a");
    let day0 = now_of(&ca);
    let a = Node::create_mesh(cfg_a).await.unwrap();
    let (b, cb) = join(
        &dirs[1],
        "b",
        day0,
        a.invite(Role::Host, TTL).await.unwrap(),
    )
    .await;
    let (d, cd) = join(
        &dirs[3],
        "d",
        day0,
        a.invite(Role::Host, TTL).await.unwrap(),
    )
    .await;
    let (c, cc) = join(
        &dirs[2],
        "c",
        day0,
        a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    converged(&[&a, &b, &d], T).await;
    Quad {
        a,
        b,
        c,
        d,
        clocks: [ca, cb, cc, cd],
        dirs,
        day0,
    }
}

impl Quad {
    fn set_day(&self, day: u64) {
        for c in &self.clocks {
            c.set(self.day0 + day * DAY);
        }
    }

    /// A and its client C lose the hosts B and D, and 81 days pass with B and D syncing
    /// (fresh). A and C are kept apart while the clock moves (a round between them would
    /// refresh both), then reconnected: both stale. C keeps refusing B and D, so the only
    /// way back to a Fresh peer is through B or D healing (they shut A out until then).
    async fn age_a(&self) {
        let (ia, ib, ic, id) = (
            self.a.node_id(),
            self.b.node_id(),
            self.c.node_id(),
            self.d.node_id(),
        );
        full_sync_since(&self.a, self.day0).await;
        self.a.partition(&[ib, id, ic]);
        self.c.partition(&[ia, ib, id]);
        self.b.partition(&[ia, ic]);
        self.d.partition(&[ia, ic]);
        wait_until("A and C cut off", T, || async {
            !self.a.members().iter().any(|m| m.id != ia && m.online)
                && !self.c.members().iter().any(|m| m.id != ic && m.online)
        })
        .await;
        // A round that completed just before the cut-off may still be recording its full
        // sync: let that land before the clock moves.
        sleep(Duration::from_millis(500)).await;
        let (sa, sc) = (last_full_sync(&self.a).await, last_full_sync(&self.c).await);
        for day in [40, 81] {
            self.set_day(day);
            full_sync_since(&self.b, self.day0 + day * DAY).await;
            full_sync_since(&self.d, self.day0 + day * DAY).await;
        }
        assert_eq!(
            last_full_sync(&self.a).await,
            sa,
            "A refreshed while cut off"
        );
        assert_eq!(
            last_full_sync(&self.c).await,
            sc,
            "C refreshed while cut off"
        );
        assert!(is_stale(&self.a).await, "A stale");
        assert!(is_stale(&self.c).await, "C stale");
        assert!(!is_stale(&self.b).await && !is_stale(&self.d).await);
        self.a.heal();
        self.c.heal();
        self.c.partition(&[ib, id]);
    }
}

/// Some more completed rounds between `n` and `peer`.
async fn more_rounds(n: &Node, peer: NodeId, k: u64) {
    let r0 = n.handle().rounds_with_for_test(peer);
    wait_until("more rounds", T, || async {
        n.sync_now();
        n.handle().rounds_with_for_test(peer) >= r0 + k
    })
    .await;
}

fn saw_rebootstrap(rx: &mut broadcast::Receiver<NodeEvent>) -> bool {
    let mut seen = false;
    while let Ok(ev) = rx.try_recv() {
        seen |= matches!(ev, NodeEvent::RebootstrapStarting);
    }
    seen
}

async fn wait_rebootstrap_done(rx: &mut broadcast::Receiver<NodeEvent>) {
    timeout(T, async {
        loop {
            match rx.recv().await {
                Ok(NodeEvent::RebootstrapDone) => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("{e:?}"),
            }
        }
    })
    .await
    .expect("RebootstrapDone");
}

async fn sees_name(n: &Node, who: NodeId, name: &str) -> bool {
    n.members()
        .iter()
        .any(|m| m.id == who && m.info.as_ref().is_some_and(|i| i.name == name))
}

/// Ruling C1c: a lone host with clients stays Fresh through them (each round with a
/// Fresh peer refreshes), however long it runs; nothing resets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lone_host_with_clients_stays_fresh() {
    let dirs: Vec<TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (cfg_a, ca) = test_cfg(dirs[0].path(), "a");
    let day0 = now_of(&ca);
    let a = Node::create_mesh(cfg_a).await.unwrap();
    let (c1, cc1) = join(
        &dirs[1],
        "c1",
        day0,
        a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    let (c2, cc2) = join(
        &dirs[2],
        "c2",
        day0,
        a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    a.store().set(NS, "k", b"v".to_vec()).await.unwrap();
    let mut ea = a.subscribe();
    for day in (10..=90).step_by(10) {
        for c in [&ca, &cc1, &cc2] {
            c.set(day0 + day * DAY);
        }
        more_rounds(&a, c1.node_id(), 2).await;
        more_rounds(&a, c2.node_id(), 2).await;
    }
    assert!(!is_stale(&a).await, "A stays fresh through its clients");
    assert!(last_full_sync(&a).await.unwrap() >= day0 + 90 * DAY);
    assert!(!is_stale(&c1).await && !is_stale(&c2).await);
    assert!(!saw_rebootstrap(&mut ea));
    assert_eq!(val(&a, "k").await, Some(b"v".to_vec()));
    for n in [a, c1, c2] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

/// Ruling C1c: a stale host A with a (stale) client keeps its data and syncs on, also
/// across a restart; when a Fresh Full host is back, A re-bootstraps from it and ends with
/// all the data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_host_resets_only_on_a_fresh_host_and_keeps_all_data() {
    let t = quad().await;
    t.a.store()
        .set(NS, "a-synced", b"1".to_vec())
        .await
        .unwrap();
    t.b.store().set(NS, "b-data", b"2".to_vec()).await.unwrap();
    converged(&[&t.a, &t.b, &t.d], T).await;
    t.age_a().await;
    let Quad {
        a,
        b,
        c,
        d,
        clocks,
        dirs,
        ..
    } = t;
    let mut events = a.subscribe();

    more_rounds(&a, c.node_id(), 3).await;
    assert!(
        is_stale(&a).await,
        "a round with a stale client refreshes nothing"
    );
    assert_eq!(a.status(), NodeStatus::Running);
    assert_eq!(val(&a, "b-data").await, Some(b"2".to_vec()));
    a.store().set(NS, "a-late", b"3".to_vec()).await.unwrap();
    a.set_info(Some("a-stale".into()), None).await.unwrap();
    wait_until("C sees A's info", T, || async {
        sees_name(&c, a.node_id(), "a-stale").await
    })
    .await;
    assert!(!saw_rebootstrap(&mut events));

    // A restart while stale does not reset either (B and D still shut A out).
    let now = now_of(&clocks[0]);
    timeout(T, a.shutdown()).await.unwrap();
    let (cfg, clock_a) = test_cfg(dirs[0].path(), "a");
    clock_a.set(now);
    let a = Node::open(cfg).await.unwrap();
    let mut events = a.subscribe();
    more_rounds(&a, c.node_id(), 2).await;
    assert!(is_stale(&a).await);
    assert_eq!(a.status(), NodeStatus::Running);
    assert!(!saw_rebootstrap(&mut events));

    // A Fresh host comes back: A re-bootstraps from it and keeps everything.
    b.heal();
    d.heal();
    wait_rebootstrap_done(&mut events).await;
    assert_eq!(a.status(), NodeStatus::Running);
    converged(&[&a, &b, &d], T).await;
    for k in ["a-synced", "b-data", "a-late"] {
        assert!(val(&a, k).await.is_some(), "{k} after the re-bootstrap");
        assert!(val(&b, k).await.is_some(), "{k} on B");
    }
    assert!(!is_stale(&a).await);
    for n in [a, b, c, d] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

/// Ruling C1c: two stale hosts sync with each other as usual; both stay stale and
/// neither resets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_stale_hosts_sync_and_neither_resets() {
    let dirs: Vec<TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (cfg_a, ca) = test_cfg(dirs[0].path(), "a");
    let day0 = now_of(&ca);
    let a = Node::create_mesh(cfg_a).await.unwrap();
    let (b, cb) = join(
        &dirs[1],
        "b",
        day0,
        a.invite(Role::Host, TTL).await.unwrap(),
    )
    .await;
    a.store().set(NS, "from-a", b"1".to_vec()).await.unwrap();
    b.store().set(NS, "from-b", b"2".to_vec()).await.unwrap();
    converged(&[&a, &b], T).await;
    full_sync_since(&a, day0).await;
    full_sync_since(&b, day0).await;
    let (ia, ib) = (a.node_id(), b.node_id());
    a.partition(&[ib]);
    b.partition(&[ia]);
    wait_until("apart", T, || async {
        !a.members().iter().any(|m| m.id == ib && m.online)
    })
    .await;
    for c in [&ca, &cb] {
        c.set(day0 + 81 * DAY);
    }
    assert!(is_stale(&a).await && is_stale(&b).await);
    let (mut ea, mut eb) = (a.subscribe(), b.subscribe());
    a.heal();
    b.heal();
    a.store().set(NS, "a-later", b"3".to_vec()).await.unwrap();
    b.store().set(NS, "b-later", b"4".to_vec()).await.unwrap();
    converged(&[&a, &b], T).await;
    more_rounds(&a, ib, 2).await;
    for (n, e) in [(&a, &mut ea), (&b, &mut eb)] {
        assert!(
            !saw_rebootstrap(e),
            "a stale host must not reset from a stale host"
        );
        assert!(is_stale(n).await, "a stale peer refreshes nothing");
        assert_eq!(n.status(), NodeStatus::Running);
        for k in ["from-a", "from-b", "a-later", "b-later"] {
            assert!(val(n, k).await.is_some(), "{k}");
        }
    }
    a.shutdown().await;
    b.shutdown().await;
}

/// Ruling C1c: two new (Empty) hosts join a stale host A in parallel. A never resets, both
/// new hosts end Stale (an Empty pair refreshes nothing), and all three keep syncing.
/// When a Fresh host is back, all re-bootstrap from it and keep their data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_host_and_two_new_hosts_joining_in_parallel() {
    let t = quad().await;
    t.a.store()
        .set(NS, "a-synced", b"1".to_vec())
        .await
        .unwrap();
    t.b.store().set(NS, "b-data", b"2".to_vec()).await.unwrap();
    converged(&[&t.a, &t.b, &t.d], T).await;
    t.age_a().await;
    let now = now_of(&t.clocks[0]);
    let Quad { a, b, c, d, .. } = t;
    let mut ea = a.subscribe();

    let (de, de2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (ie, ie2) = (id_of(&de), id_of(&de2));
    b.partition(&[a.node_id(), c.node_id(), ie, ie2]);
    d.partition(&[a.node_id(), c.node_id(), ie, ie2]);
    let (t1, t2) = (
        a.invite(Role::Host, TTL).await.unwrap(),
        a.invite(Role::Host, TTL).await.unwrap(),
    );
    let ((e, _), (e2, _)) = tokio::join!(join(&de, "e", now, t1), join(&de2, "e2", now, t2));
    let (mut ee, mut ee2) = (e.subscribe(), e2.subscribe());
    for n in [&e, &e2] {
        wait_until("a new host has A's data", T, || async {
            val(n, "a-synced").await.is_some() && val(n, "b-data").await.is_some()
        })
        .await;
        wait_until("a new host reports stale", T, || async {
            is_stale(n).await
        })
        .await;
    }
    // All three keep syncing.
    e.store().set(NS, "e-data", b"5".to_vec()).await.unwrap();
    e2.store().set(NS, "e2-data", b"6".to_vec()).await.unwrap();
    a.store().set(NS, "a-late", b"7".to_vec()).await.unwrap();
    converged(&[&a, &e, &e2], T).await;
    more_rounds(&e, ie2, 2).await;
    assert!(is_stale(&a).await && is_stale(&e).await && is_stale(&e2).await);
    assert!(
        !saw_rebootstrap(&mut ea),
        "A must never reset from a new host"
    );
    assert!(!saw_rebootstrap(&mut ee) && !saw_rebootstrap(&mut ee2));
    assert_eq!(a.status(), NodeStatus::Running);

    // A Fresh host is back: everyone re-bootstraps from it and nothing is lost.
    b.heal();
    d.heal();
    for ev in [&mut ea, &mut ee, &mut ee2] {
        wait_rebootstrap_done(ev).await;
    }
    // A host that re-bootstrapped is Empty until a round with a Fresh peer; a round with a
    // still-stale peer first (the client C, or a host not yet reset) makes it inherit
    // staleness and re-bootstrap again. That settles once every stale node has reset.
    wait_until("everyone fresh and running", T, || async {
        let mut ok = true;
        for n in [&a, &c, &e, &e2] {
            ok &= n.status() == NodeStatus::Running && !is_stale(n).await;
        }
        ok
    })
    .await;
    converged(&[&a, &b, &d, &e, &e2], T).await;
    for n in [&a, &b, &e, &e2] {
        for k in ["a-synced", "b-data", "e-data", "e2-data", "a-late"] {
            assert!(val(n, k).await.is_some(), "{k} missing");
        }
    }
    for n in [a, b, c, d, e, e2] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

/// Ruling I2: a re-bootstrap that got its own ops back from a client alone does not
/// complete; it waits for a pull from a Full peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebootstrap_completes_only_after_a_full_round() {
    let t = quad().await;
    // A writes only `_mesh` (its own ops all come back from the client); B has service data.
    t.b.store().set(NS, "b-data", b"2".to_vec()).await.unwrap();
    converged(&[&t.a, &t.b, &t.d], T).await;
    t.age_a().await;
    let Quad { a, b, c, d, .. } = t;
    let (ia, ib) = (a.node_id(), b.node_id());
    let mut events = a.subscribe();

    // B (fresh) triggers the re-bootstrap, then vanishes before the reset.
    a.hold_rebootstrap_before_reset(true);
    b.heal();
    b.partition(&[c.node_id()]);
    wait_until("A is re-bootstrapping", T, || async {
        matches!(a.status(), NodeStatus::Rebootstrapping { .. })
    })
    .await;
    a.partition(&[ib]);
    b.partition(&[ia]);
    let NodeStatus::Rebootstrapping {
        waiting_for_self_seq: s,
    } = a.status()
    else {
        panic!("not re-bootstrapping");
    };
    a.hold_rebootstrap_before_reset(false);
    // The client gives A's own (`_mesh`) ops back...
    wait_until("A's own ops back from C", T, || async {
        vv_of(&a).await.get(&ia) >= s && a.members().iter().any(|m| m.id == ia)
    })
    .await;
    // ...but without a pull from a Full peer A stays re-bootstrapping.
    for _ in 0..10 {
        a.sync_now();
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        matches!(a.status(), NodeStatus::Rebootstrapping { .. }),
        "completed without a Full peer: {:?}",
        a.status()
    );
    assert_eq!(val(&a, "b-data").await, None);

    a.heal();
    b.heal();
    wait_rebootstrap_done(&mut events).await;
    assert_eq!(val(&a, "b-data").await, Some(b"2".to_vec()));
    for n in [a, b, c, d] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

/// Ruling I3: a live `_mesh` write after out-of-scope writes reaches a client at once, in
/// live mode (no round), with nothing parked in the client's pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_mesh_write_after_svc_writes_reaches_client_at_once() {
    let dirs: Vec<TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let hour = Duration::from_secs(3600);
    let quiet = |dir: &std::path::Path, name: &str| {
        let (mut cfg, clock) = test_cfg(dir, name);
        cfg.timings.sync_interval = hour;
        cfg.timings.silence = hour;
        (cfg, clock)
    };
    let a = Node::create_mesh(quiet(dirs[0].path(), "a").0)
        .await
        .unwrap();
    let ticket = a.invite(Role::Client, TTL).await.unwrap();
    let c = timeout(T, Node::join(quiet(dirs[1].path(), "c").0, ticket))
        .await
        .unwrap()
        .unwrap();
    assert!(c.wait_first_sync(T).await);
    let ia = a.node_id();
    wait_until("C caught up", T, || async {
        vv_of(&c).await.get(&ia) >= vv_of(&a).await.get(&ia)
    })
    .await;
    // Rounds from the join (first sync, the end of the provisional phase) settle first.
    let count = || {
        (
            a.handle().rounds_with_for_test(c.node_id()),
            c.handle().rounds_with_for_test(ia),
        )
    };
    let mut rounds = count();
    loop {
        sleep(Duration::from_millis(500)).await;
        let now = count();
        if now == rounds {
            break;
        }
        rounds = now;
    }
    for i in 0..3 {
        for j in 0..3 {
            a.store()
                .set(NS, &format!("k{i}{j}"), b"v".to_vec())
                .await
                .unwrap();
        }
        a.set_info(Some(format!("a{i}")), None).await.unwrap();
        let want = format!("a{i}");
        wait_until(
            "C sees the live `_mesh` write",
            Duration::from_secs(5),
            || async {
                c.members()
                    .iter()
                    .any(|m| m.id == ia && m.info.as_ref().is_some_and(|x| x.name == want))
            },
        )
        .await;
    }
    // Nothing waits in C's pending: its vector has every op of A.
    assert_eq!(vv_of(&c).await.get(&ia), vv_of(&a).await.get(&ia));
    assert_eq!(
        rounds,
        (
            a.handle().rounds_with_for_test(c.node_id()),
            c.handle().rounds_with_for_test(ia)
        ),
        "no round ran: this was live push"
    );
    a.shutdown().await;
    c.shutdown().await;
}

/// Ruling A2: a stale client and a stale host (its only host) keep syncing `_mesh`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_client_and_stale_host_keep_syncing() {
    let dirs: Vec<TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (cfg_a, ca) = test_cfg(dirs[0].path(), "a");
    let day0 = now_of(&ca);
    let a = Node::create_mesh(cfg_a).await.unwrap();
    let (b, _cb) = join(
        &dirs[1],
        "b",
        day0,
        a.invite(Role::Host, TTL).await.unwrap(),
    )
    .await;
    let (c, cc) = join(
        &dirs[2],
        "c",
        day0,
        a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    full_sync_since(&a, day0).await;
    full_sync_since(&c, day0).await;
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());
    // Apart while the clock jumps (no round may straddle it), B gone for good.
    a.partition(&[ib, ic]);
    c.partition(&[ib, ia]);
    b.partition(&[ia, ic]);
    wait_until("A and C apart", T, || async {
        !a.members().iter().any(|m| m.id == ic && m.online)
            && !c.members().iter().any(|m| m.id == ia && m.online)
    })
    .await;
    for clock in [&ca, &cc] {
        clock.set(day0 + 81 * DAY);
    }
    assert!(is_stale(&a).await && is_stale(&c).await);
    a.heal();
    c.heal();
    a.partition(&[ib]);
    c.partition(&[ib]);
    // Rounds between them: neither refreshes the other.
    more_rounds(&a, ic, 1).await;
    assert!(is_stale(&a).await && is_stale(&c).await);
    let (mut ea, mut ec) = (a.subscribe(), c.subscribe());
    a.set_info(Some("a-later".into()), None).await.unwrap();
    wait_until("C sees A's write", T, || async {
        c.members()
            .iter()
            .any(|m| m.id == ia && m.info.as_ref().is_some_and(|i| i.name == "a-later"))
    })
    .await;
    c.set_info(Some("c-later".into()), None).await.unwrap();
    wait_until("A sees C's write", T, || async {
        a.members()
            .iter()
            .any(|m| m.id == ic && m.info.as_ref().is_some_and(|i| i.name == "c-later"))
    })
    .await;
    more_rounds(&c, ia, 2).await;
    assert!(!saw_rebootstrap(&mut ea) && !saw_rebootstrap(&mut ec));
    for n in [a, b, c] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}

/// Ruling C1d: a stale host is not refreshed by a client that is Fresh only through
/// another host (whose GC'd deletions the stale data could bring back); it stays Stale
/// until it resets from that Fresh host, and then has all the data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_host_not_refreshed_by_client_fresh_via_other_host() {
    let t = quad().await;
    t.a.store()
        .set(NS, "a-synced", b"1".to_vec())
        .await
        .unwrap();
    t.b.store().set(NS, "b-data", b"2".to_vec()).await.unwrap();
    converged(&[&t.a, &t.b, &t.d], T).await;
    t.age_a().await;
    let Quad { a, b, c, d, .. } = t;
    let (ia, ic) = (a.node_id(), c.node_id());
    let mut ea = a.subscribe();

    // C reaches the Fresh hosts (they keep shutting A out) and turns Fresh through them.
    b.heal();
    b.partition(&[ia]);
    d.heal();
    d.partition(&[ia]);
    c.heal();
    wait_until("C fresh through B", T, || async { !is_stale(&c).await }).await;
    // A keeps syncing with that Fresh client, and stays stale.
    more_rounds(&a, ic, 3).await;
    assert!(is_stale(&a).await, "a client cannot rescue a stale host");
    assert!(!saw_rebootstrap(&mut ea));
    assert_eq!(val(&a, "b-data").await, Some(b"2".to_vec()));
    a.store().set(NS, "a-late", b"3".to_vec()).await.unwrap();

    // A meets B: it resets from it and ends with all the data.
    b.heal();
    d.heal();
    wait_rebootstrap_done(&mut ea).await;
    converged(&[&a, &b, &d], T).await;
    for k in ["a-synced", "b-data", "a-late"] {
        assert!(val(&a, k).await.is_some(), "{k} on A");
        assert!(val(&b, k).await.is_some(), "{k} on B");
    }
    assert!(!is_stale(&a).await);
    for n in [a, b, c, d] {
        timeout(T, n.shutdown()).await.unwrap();
    }
}
