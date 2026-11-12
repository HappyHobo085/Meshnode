//! Staleness and re-bootstrap with client-role members (sub-spec 8a §3 with spec §7): a
//! round with a client is no full sync, but a stale host never wipes its store unless a
//! fresh Full peer is there to restore from, and a re-bootstrap completes only after a
//! round with a Full peer.

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

    /// A loses its hosts (keeps the client) and 81 days pass; B and D keep full-syncing,
    /// so only A is stale.
    async fn age_a(&self) {
        let (ib, id) = (self.b.node_id(), self.d.node_id());
        let ia = self.a.node_id();
        // A has had a full sync with a host before it is cut off.
        full_sync_since(&self.a, self.day0).await;
        self.a.partition(&[ib, id]);
        self.b.partition(&[ia]);
        self.d.partition(&[ia]);
        for day in [40, 81] {
            self.set_day(day);
            full_sync_since(&self.b, self.day0 + day * DAY).await;
            full_sync_since(&self.d, self.day0 + day * DAY).await;
        }
        assert!(is_stale(&self.a).await, "A is stale");
        assert!(!is_stale(&self.b).await && !is_stale(&self.d).await);
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

/// Ruling C1: a stale host with only clients keeps its data and keeps serving (also across
/// a restart); once a fresh Full peer is back it re-bootstraps from it and ends whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_host_with_only_clients_keeps_data_until_a_fresh_host() {
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

    // Rounds with the client go on; nothing is wiped, writes are taken.
    more_rounds(&a, c.node_id(), 3).await;
    assert!(is_stale(&a).await, "a client round is no full sync");
    assert_eq!(a.status(), NodeStatus::Running);
    assert_eq!(val(&a, "a-synced").await, Some(b"1".to_vec()));
    assert_eq!(val(&a, "b-data").await, Some(b"2".to_vec()));
    a.store().set(NS, "a-late", b"3".to_vec()).await.unwrap();
    a.set_info(Some("a-stale".into()), None).await.unwrap();
    wait_until("C sees A's info", T, || async {
        c.members()
            .iter()
            .any(|m| m.id == a.node_id() && m.info.as_ref().is_some_and(|i| i.name == "a-stale"))
    })
    .await;
    assert!(
        !saw_rebootstrap(&mut events),
        "no re-bootstrap without a fresh host"
    );

    // A restart while stale does not wipe either (B and D still shut A out).
    let now = now_of(&clocks[0]);
    timeout(T, a.shutdown()).await.unwrap();
    let (cfg, clock_a) = test_cfg(dirs[0].path(), "a");
    clock_a.set(now);
    let a = Node::open(cfg).await.unwrap();
    let mut events = a.subscribe();
    assert_eq!(a.status(), NodeStatus::Running);
    more_rounds(&a, c.node_id(), 2).await;
    assert!(is_stale(&a).await);
    assert_eq!(a.status(), NodeStatus::Running);
    for k in ["a-synced", "b-data", "a-late"] {
        assert!(val(&a, k).await.is_some(), "{k} kept");
    }
    assert!(!saw_rebootstrap(&mut events));

    // A fresh host comes back: A re-bootstraps from it and ends with everything.
    a.heal();
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

/// Ruling C1: two stale hosts that meet never wipe each other (nor deadlock in a reset).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_stale_hosts_do_not_wipe() {
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
    for c in [&ca, &cb] {
        c.set(day0 + 81 * DAY);
    }
    assert!(is_stale(&a).await && is_stale(&b).await);
    let (mut ea, mut eb) = (a.subscribe(), b.subscribe());
    a.heal();
    b.heal();
    // Let them meet several times.
    for _ in 0..20 {
        a.sync_now();
        b.sync_now();
        sleep(Duration::from_millis(100)).await;
    }
    for (n, e) in [(&a, &mut ea), (&b, &mut eb)] {
        assert!(
            !saw_rebootstrap(e),
            "a stale host must not reset from a stale host"
        );
        assert_eq!(n.status(), NodeStatus::Running);
        assert_eq!(val(n, "from-a").await, Some(b"1".to_vec()));
        assert_eq!(val(n, "from-b").await, Some(b"2".to_vec()));
    }
    a.shutdown().await;
    b.shutdown().await;
}

/// Ruling I2: a re-bootstrap that got its own ops back from a client alone does not
/// complete; it waits for a completed round with a Full peer.
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
    a.heal();
    b.heal();
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
    // ...but without a Full round A stays re-bootstrapping (and does not serve).
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
    let rounds = (
        a.handle().rounds_with_for_test(c.node_id()),
        c.handle().rounds_with_for_test(ia),
    );
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
