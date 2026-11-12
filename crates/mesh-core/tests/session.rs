//! The `sync/1` session (spec §6.3–§6.6) over in-memory pipes: two `CoreStore`s joined by
//! `tokio::io::duplex`, or one real session driven by raw frames.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use mesh_core::events::NodeEvent;
use mesh_core::frame::{SYNC_MAX_FRAME, read_frame, write_frame};
use mesh_core::membership::{MEMBERS, MESH_NS, MeshAcceptor, member_bytes};
use mesh_core::sync::{SessionCmd, SessionCtx, SessionEnd, TestHandles, run_session};
use mesh_core::{CoreStore, DynClock, StoreHandle, Timings};
use mesh_hlc::ManualClock;
use mesh_proto::{MeshGenesis, MeshId, NodeId, Role, SignedOp, SyncMsg, VersionVector};
use mesh_store::SyncItem;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(30);
const PIPE: usize = 64 * 1024;
const DAY_MS: u64 = 86_400_000;
/// How long a session must stay silent to count as quiescent.
const QUIET: Duration = Duration::from_millis(500);

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

fn acc() -> Arc<MeshAcceptor> {
    Arc::new(MeshAcceptor::new(genesis()))
}

/// Rounds only when a test asks for one.
fn quiet() -> Timings {
    let hour = Duration::from_secs(3600);
    Timings {
        sync_interval: hour,
        silence: hour,
        ..Timings::fast_for_tests()
    }
}

struct TestNode {
    store: StoreHandle,
    clock: ManualClock,
    id: NodeId,
}

fn open(dir: &std::path::Path, n: u8, file: &str) -> TestNode {
    let clock = ManualClock::new(1_000_000);
    let store = CoreStore::open(
        &dir.join(file),
        genesis().mesh_id(),
        key(n),
        DynClock(Arc::new(clock.clone())),
    )
    .unwrap();
    TestNode {
        store: StoreHandle::new(Arc::new(store)),
        clock,
        id: id(n),
    }
}

/// Founder (node 1) and a second member (node 2). The founder's first two own ops add both
/// to `members`, so `own_a` counts them.
fn pair_nodes(dir: &std::path::Path, own_a: u64, own_b: u64) -> (TestNode, TestNode) {
    let a = open(dir, 1, "a.redb");
    let b = open(dir, 2, "b.redb");
    let s = a.store.inner();
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(1), Role::Host))
        .unwrap();
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Host))
        .unwrap();
    for i in 2..own_a {
        s.set("app", &format!("a{i}"), b"x").unwrap();
    }
    for i in 0..own_b {
        b.store.inner().set("app", &format!("b{i}"), b"y").unwrap();
    }
    (a, b)
}

struct Side {
    h: TestHandles,
    join: JoinHandle<SessionEnd>,
}

fn spawn_side<R, W>(n: &TestNode, peer: NodeId, timings: Timings, r: R, w: W, dialer: bool) -> Side
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (ctx, h) = SessionCtx::for_test(n.store.clone(), acc(), peer, timings);
    Side {
        h,
        join: tokio::spawn(run_session(ctx, r, w, dialer)),
    }
}

type Log = Arc<Mutex<Vec<SyncMsg>>>;

/// Forwards frames from `r` to `w`, recording each; ends (dropping `w`) at EOF or error.
fn tap(mut r: DuplexStream, mut w: DuplexStream, log: Log) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(Some(m)) = read_frame::<_, SyncMsg>(&mut r, SYNC_MAX_FRAME).await {
            log.lock().unwrap().push(m.clone());
            if write_frame(&mut w, &m).await.is_err() {
                break;
            }
        }
    })
}

struct Pair {
    a: Side,
    b: Side,
    /// Frames A sent to B and B sent to A (only filled when tapped).
    a2b: Log,
    b2a: Log,
    taps: Vec<JoinHandle<()>>,
}

/// Runs A (the dialer) and B against each other; every frame passes through a tap.
fn run_pair(a: &TestNode, b: &TestNode, timings: Timings) -> Pair {
    let (a_w, t1_r) = tokio::io::duplex(PIPE);
    let (t1_w, b_r) = tokio::io::duplex(PIPE);
    let (b_w, t2_r) = tokio::io::duplex(PIPE);
    let (t2_w, a_r) = tokio::io::duplex(PIPE);
    let a2b: Log = Arc::default();
    let b2a: Log = Arc::default();
    let taps = vec![tap(t1_r, t1_w, a2b.clone()), tap(t2_r, t2_w, b2a.clone())];
    Pair {
        a: spawn_side(a, b.id, timings.clone(), a_r, a_w, true),
        b: spawn_side(b, a.id, timings, b_r, b_w, false),
        a2b,
        b2a,
        taps,
    }
}

/// A raw peer: the test writes and reads frames itself.
struct Raw {
    r: DuplexStream,
    w: DuplexStream,
}

impl Raw {
    async fn send(&mut self, m: &SyncMsg) {
        write_frame(&mut self.w, m).await.unwrap();
    }

    fn hello(vv: VersionVector) -> SyncMsg {
        SyncMsg::Hello {
            mesh: mesh(),
            vv,
            scope: mesh_proto::Scope::Full,
            stale: false,
        }
    }

    /// Reads frames up to and including the session's next `Done`; returns how many
    /// `Hello`s came before it.
    async fn hellos_until_done(&mut self) -> usize {
        let mut hellos = 0;
        loop {
            match self.recv().await {
                Some(SyncMsg::Done) => return hellos,
                Some(SyncMsg::Hello { .. }) => hellos += 1,
                Some(_) => {}
                None => panic!("session closed"),
            }
        }
    }

    /// Nothing more arrives: the session has gone quiet.
    async fn assert_quiet(&mut self) {
        let next = timeout(QUIET, read_frame::<_, SyncMsg>(&mut self.r, SYNC_MAX_FRAME)).await;
        assert!(next.is_err(), "unexpected frame: {next:?}");
    }

    async fn recv(&mut self) -> Option<SyncMsg> {
        timeout(T, read_frame(&mut self.r, SYNC_MAX_FRAME))
            .await
            .expect("frame in time")
            .unwrap()
    }
}

/// A real, non-dialing session for `n` against a raw peer `peer`.
fn run_against_raw(n: &TestNode, peer: NodeId) -> (Side, Raw) {
    run_against_raw_with(n, peer, quiet(), false)
}

fn run_against_raw_with(n: &TestNode, peer: NodeId, timings: Timings, dialer: bool) -> (Side, Raw) {
    let (n_w, raw_r) = tokio::io::duplex(PIPE);
    let (raw_w, n_r) = tokio::io::duplex(PIPE);
    let side = spawn_side(n, peer, timings, n_r, n_w, dialer);
    (side, Raw { r: raw_r, w: raw_w })
}

async fn next_round(ev: &mut broadcast::Receiver<NodeEvent>) -> NodeId {
    timeout(T, async {
        loop {
            if let NodeEvent::RoundComplete(p) = ev.recv().await.unwrap() {
                return p;
            }
        }
    })
    .await
    .expect("round completes in time")
}

/// Polls `f` every 10 ms until it is true or `limit` passes.
async fn eventually(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    timeout(limit, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

fn has(n: &TestNode, k: &str) -> bool {
    n.store.inner().get("app", k).unwrap().is_some()
}

async fn end_of(side: Side) -> SessionEnd {
    timeout(T, side.join)
        .await
        .expect("session ends in time")
        .unwrap()
}

/// Signed ops of a throw-away node `k` (not necessarily a member), one per value length.
fn foreign_ops(dir: &std::path::Path, k: u8, value_lens: &[usize]) -> Vec<SignedOp> {
    let c = open(dir, k, &format!("c{k}.redb"));
    let s = c.store.inner();
    for (i, len) in value_lens.iter().enumerate() {
        s.set("app", &format!("c{i}"), &vec![0; *len]).unwrap();
    }
    s.diff_since(&VersionVector::default(), value_lens.len() + 10)
        .unwrap()
        .into_iter()
        .map(|i| match i {
            SyncItem::Op(op) => op,
            SyncItem::Covered { .. } => panic!("no compaction here"),
        })
        .collect()
}

fn mesh() -> MeshId {
    genesis().mesh_id()
}

fn vv(n: &TestNode) -> VersionVector {
    n.store.inner().version_vector().unwrap()
}

// ---------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread")]
async fn round_exchanges_and_marks_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 300, 10);
    let mut p = run_pair(&a, &b, quiet());
    assert_eq!(next_round(&mut p.a.h.events).await, b.id);
    assert_eq!(next_round(&mut p.b.h.events).await, a.id);
    assert_eq!(vv(&a), vv(&b));
    assert_eq!(vv(&a).get(&a.id), 300);
    assert_eq!(vv(&a).get(&b.id), 10);
    // B's Hello carried vv[A] = 0, A's carried vv[B] = 0.
    assert_eq!(a.store.inner().synced_self_seq().unwrap(), Some(0));
    assert_eq!(b.store.inner().synced_self_seq().unwrap(), Some(0));
    assert!(a.store.inner().last_full_sync_ms().unwrap().is_some());

    p.a.h.cmds.send(SessionCmd::StartRound).await.unwrap();
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;
    assert_eq!(a.store.inner().synced_self_seq().unwrap(), Some(300));
    assert_eq!(b.store.inner().synced_self_seq().unwrap(), Some(10));
    assert_eq!(p.a.h.metrics.rounds_completed.load(Ordering::SeqCst), 2);
    assert_eq!(p.b.h.metrics.rounds_completed.load(Ordering::SeqCst), 2);
    assert_eq!(p.a.h.metrics.lag.load(Ordering::SeqCst), 0);
    assert!(p.b.h.metrics.last_seen_ms.load(Ordering::SeqCst) > 0);
    assert!(!p.a.join.is_finished() && !p.b.join.is_finished());
}

#[tokio::test(flavor = "multi_thread")]
async fn live_push_without_round() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 5, 5);
    let mut p = run_pair(&a, &b, quiet());
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;

    a.store.set("app", "live", b"now".to_vec()).await.unwrap();
    p.a.h.wake.notify_one();
    assert!(eventually(Duration::from_millis(500), || has(&b, "live")).await);
    assert_eq!(p.b.h.metrics.rounds_completed.load(Ordering::SeqCst), 1);
    assert_eq!(p.a.h.metrics.rounds_completed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_echo_back() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 5, 5);
    let mut p = run_pair(&a, &b, quiet());
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;
    p.b2a.lock().unwrap().clear();

    a.store.set("app", "from-a", b"1".to_vec()).await.unwrap();
    p.a.h.wake.notify_one();
    assert!(eventually(T, || has(&b, "from-a")).await);
    // B's store changed: wake B's live push, then send a write of B's own behind it, so
    // everything B pushes after applying A's op reaches A before B's own key does.
    p.b.h.wake.notify_one();
    b.store.set("app", "from-b", b"2".to_vec()).await.unwrap();
    p.b.h.wake.notify_one();
    assert!(eventually(T, || has(&a, "from-b")).await);

    let log = p.b2a.lock().unwrap().clone();
    let mut b_ops = 0;
    for m in &log {
        if let SyncMsg::Ops { ops } = m {
            assert!(
                ops.iter().all(|o| o.body.origin != a.id),
                "B echoed A's op back"
            );
            b_ops += ops.len();
        }
    }
    assert_eq!(b_ops, 1, "only B's own new op travels B -> A: {log:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn covered_above_advertised_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 3, 0);
    let x = id(3);
    let (mut side, mut raw) = run_against_raw(&a, id(2));
    let mut hello_vv = VersionVector::default();
    hello_vv.set(x, 2);
    raw.send(&SyncMsg::Hello {
        mesh: mesh(),
        vv: hello_vv,
        scope: mesh_proto::Scope::Full,
        stale: false,
    })
    .await;
    raw.send(&SyncMsg::Covered { origin: x, upto: 7 }).await;
    raw.send(&SyncMsg::Covered { origin: x, upto: 2 }).await;
    raw.send(&SyncMsg::Done).await;
    // A replies with its Hello, its ops and Done.
    assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
    loop {
        match raw.recv().await {
            Some(SyncMsg::Done) => break,
            Some(_) => {}
            None => panic!("A closed"),
        }
    }
    next_round(&mut side.h.events).await;
    // The marker above the advertised 2 was ignored, the one at 2 applied.
    assert_eq!(vv(&a).get(&x), 2);
    assert!(!side.join.is_finished());
    drop(raw);
    assert!(matches!(end_of(side).await, SessionEnd::Closed));
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_batch_is_protocol_error() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 3, 0);
    let ops = foreign_ops(dir.path(), 3, &[1; 257]);
    let (side, mut raw) = run_against_raw(&a, id(2));
    raw.send(&SyncMsg::Hello {
        mesh: mesh(),
        vv: VersionVector::default(),
        scope: mesh_proto::Scope::Full,
        stale: false,
    })
    .await;
    raw.send(&SyncMsg::Ops { ops }).await;
    assert!(matches!(end_of(side).await, SessionEnd::Protocol(_)));
    assert_eq!(vv(&a).get(&id(3)), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_op_batch_over_1mib_is_protocol_error() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 3, 0);
    // Two ops summing just over 1 MiB in a frame still under the frame cap.
    let ops = foreign_ops(dir.path(), 3, &[1_048_000, 600]);
    let sum: usize = ops.iter().map(|o| mesh_proto::encode(o).len()).sum();
    let msg = SyncMsg::Ops { ops };
    assert!(sum > mesh_proto::MAX_BATCH_BYTES, "{sum}");
    assert!(mesh_proto::encode(&msg).len() <= SYNC_MAX_FRAME);
    let (side, mut raw) = run_against_raw(&a, id(2));
    raw.send(&msg).await;
    assert!(matches!(end_of(side).await, SessionEnd::Protocol(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_from_other_mesh_is_protocol_error() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 3, 0);
    let (side, mut raw) = run_against_raw(&a, id(2));
    raw.send(&SyncMsg::Hello {
        mesh: MeshId([0xEE; 32]),
        vv: VersionVector::default(),
        scope: mesh_proto::Scope::Full,
        stale: false,
    })
    .await;
    assert!(matches!(end_of(side).await, SessionEnd::Protocol(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn max_size_op_replicates() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 2, 0);
    let k = "k".repeat(512);
    let value = vec![0xAB; 1_048_576];
    a.store
        .inner()
        .set_ordered("app", &k, &[7; 64], Some(&value))
        .unwrap();
    let mut p = run_pair(&a, &b, quiet());
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;
    assert_eq!(b.store.inner().get("app", &k).unwrap(), Some(value));
    assert!(!p.a.join.is_finished() && !p.b.join.is_finished());
}

#[tokio::test(flavor = "multi_thread")]
async fn drop_mid_round_does_not_mark_sync() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 2000, 0);
    let (side, mut raw) = run_against_raw(&a, b.id);
    // B has nothing: it says so and sends Done at once; A's own Done never goes out.
    raw.send(&SyncMsg::Hello {
        mesh: mesh(),
        vv: VersionVector::default(),
        scope: mesh_proto::Scope::Full,
        stale: false,
    })
    .await;
    raw.send(&SyncMsg::Done).await;
    loop {
        match raw.recv().await {
            Some(SyncMsg::Ops { .. }) => break,
            Some(SyncMsg::Done) => panic!("A finished before the drop"),
            Some(_) => {}
            None => panic!("A closed"),
        }
    }
    drop(raw);
    assert!(matches!(end_of(side).await, SessionEnd::Closed));
    assert_eq!(a.store.inner().last_full_sync_ms().unwrap(), None);
    assert_eq!(b.store.inner().last_full_sync_ms().unwrap(), None);

    let mut p = run_pair(&a, &b, quiet());
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;
    assert!(a.store.inner().last_full_sync_ms().unwrap().is_some());
    assert!(b.store.inner().last_full_sync_ms().unwrap().is_some());
    assert_eq!(vv(&b).get(&a.id), 2000);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_store_ends_session() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 5, 5);
    let mut p = run_pair(&a, &b, quiet());
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;

    a.clock.advance(81 * DAY_MS);
    a.store.set("app", "late", b"z".to_vec()).await.unwrap();
    p.a2b.lock().unwrap().clear();
    p.a.h.cmds.send(SessionCmd::StartRound).await.unwrap();
    assert!(matches!(end_of(p.a).await, SessionEnd::Stale));
    // A's end closes its pipe; the tap drains and stops.
    timeout(T, p.taps.remove(0)).await.unwrap().unwrap();
    let log = p.a2b.lock().unwrap().clone();
    assert!(
        log.iter().all(|m| !matches!(m, SyncMsg::Ops { .. })),
        "stale A sent ops: {log:?}"
    );
    assert!(!has(&b, "late"));
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_serves_nothing_but_applies() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 5, 5);
    let mut p = {
        let (a_w, b_r) = tokio::io::duplex(PIPE);
        let (b_w, a_r) = tokio::io::duplex(PIPE);
        let (ctx, h) = SessionCtx::for_test(a.store.clone(), acc(), b.id, quiet());
        h.serve_paused.store(true, Ordering::SeqCst);
        Pair {
            a: Side {
                h,
                join: tokio::spawn(run_session(ctx, a_r, a_w, true)),
            },
            b: spawn_side(&b, a.id, quiet(), b_r, b_w, false),
            a2b: Arc::default(),
            b2a: Arc::default(),
            taps: Vec::new(),
        }
    };
    // B serves A in full; A applies it but sends nothing back, not even Done.
    assert!(eventually(T, || has(&a, "b4")).await);
    a.store
        .set("app", "while-paused", b"p".to_vec())
        .await
        .unwrap();
    p.a.h.wake.notify_one();
    assert!(p.b.h.events.try_recv().is_err());
    assert!(!has(&b, "a4"));
    assert_eq!(b.store.inner().last_full_sync_ms().unwrap(), None);

    p.a.h.serve_paused.store(false, Ordering::SeqCst);
    p.a.h.wake.notify_one();
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;
    assert!(has(&b, "a4") && has(&b, "while-paused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn message_after_silence_starts_round() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = pair_nodes(dir.path(), 5, 5);
    let timings = Timings {
        silence: Duration::from_millis(300),
        ..quiet()
    };
    let mut p = run_pair(&a, &b, timings);
    next_round(&mut p.a.h.events).await;
    next_round(&mut p.b.h.events).await;

    // Set up the silence: nothing crosses the link for longer than `silence`.
    tokio::time::sleep(Duration::from_millis(600)).await;
    a.store.set("app", "wake-up", b"!".to_vec()).await.unwrap();
    p.a.h.wake.notify_one();
    // B gets ops after the silence and starts a round itself.
    next_round(&mut p.b.h.events).await;
    next_round(&mut p.a.h.events).await;
    assert_eq!(p.b.h.metrics.rounds_completed.load(Ordering::SeqCst), 2);
}

// ------------------------------------------------- round-protocol edge cases (raw peer)

/// Fast silence, no interval rounds.
fn short_silence() -> Timings {
    Timings {
        silence: Duration::from_millis(300),
        ..quiet()
    }
}

/// Waits out `silence` so the next message counts as arriving after it.
async fn let_silence_pass() {
    tokio::time::sleep(Duration::from_millis(600)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn second_hello_mid_round_gets_one_reply_then_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 300, 0);
    let (mut side, mut raw) = run_against_raw(&a, id(2));
    raw.send(&Raw::hello(VersionVector::default())).await;
    assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
    // A is mid-round (streaming 300 ops); the peer starts over.
    raw.send(&Raw::hello(VersionVector::default())).await;
    let hellos = raw.hellos_until_done().await;
    // A's first Done may belong to the first round; read on to the Done after the reply.
    let hellos = if hellos == 0 {
        raw.hellos_until_done().await
    } else {
        hellos
    };
    assert_eq!(hellos, 1, "exactly one reply to the second Hello");
    raw.send(&SyncMsg::Done).await;
    next_round(&mut side.h.events).await;
    raw.assert_quiet().await;
    assert_eq!(side.h.metrics.rounds_completed.load(Ordering::SeqCst), 1);
    assert!(!side.join.is_finished());
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_after_silence_then_hello_completes_round() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 5, 0);
    let (mut side, mut raw) = run_against_raw_with(&a, id(2), short_silence(), false);
    raw.send(&Raw::hello(VersionVector::default())).await;
    assert_eq!(raw.hellos_until_done().await, 1);
    raw.send(&SyncMsg::Done).await;
    next_round(&mut side.h.events).await;

    // The Android wake-up: traffic after a silence, then the peer's own round start.
    let_silence_pass().await;
    raw.send(&SyncMsg::Ops { ops: Vec::new() }).await;
    raw.send(&Raw::hello(vv(&a))).await;
    // Whichever of A's start and the peer's Hello wins, A sends one Hello and a Done.
    assert_eq!(raw.hellos_until_done().await, 1);
    raw.send(&SyncMsg::Done).await;
    next_round(&mut side.h.events).await;
    raw.assert_quiet().await;
    assert_eq!(side.h.metrics.rounds_completed.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn crossing_hellos_make_one_round() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 5, 0);
    // A dials (starts on entry) while the peer starts too, without waiting.
    let (mut side, mut raw) = run_against_raw_with(&a, id(2), quiet(), true);
    raw.send(&Raw::hello(VersionVector::default())).await;
    assert_eq!(raw.hellos_until_done().await, 1);
    raw.send(&SyncMsg::Done).await;
    next_round(&mut side.h.events).await;
    raw.assert_quiet().await;
    assert_eq!(side.h.metrics.rounds_completed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn silence_restart_while_hello_unanswered_sends_no_second_hello() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 5, 0);
    let (mut side, mut raw) = run_against_raw_with(&a, id(2), short_silence(), true);
    // A's start; the peer does not answer yet.
    assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
    // Some earlier traffic, then a silence, then more traffic: a forced start while
    // A's Hello is unanswered must not put a second Hello in flight.
    raw.send(&SyncMsg::Ops { ops: Vec::new() }).await;
    let_silence_pass().await;
    raw.send(&SyncMsg::Ops { ops: Vec::new() }).await;
    raw.send(&Raw::hello(VersionVector::default())).await;
    assert_eq!(
        raw.hellos_until_done().await,
        0,
        "A answered its own round twice"
    );
    raw.send(&SyncMsg::Done).await;
    next_round(&mut side.h.events).await;
    raw.assert_quiet().await;
    assert_eq!(side.h.metrics.rounds_completed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn tick_restarts_a_stalled_round() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = pair_nodes(dir.path(), 5, 0);
    let timings = Timings {
        sync_interval: Duration::from_millis(300),
        ..quiet()
    };
    let (mut side, mut raw) = run_against_raw_with(&a, id(2), timings, true);
    // Round 1: the peer answers but never sends Done, so A's round stalls.
    assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
    raw.send(&Raw::hello(VersionVector::default())).await;
    assert_eq!(raw.hellos_until_done().await, 0);
    // After `sync_interval` the tick replaces it with a fresh start.
    assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
    raw.send(&Raw::hello(vv(&a))).await;
    raw.send(&SyncMsg::Done).await;
    next_round(&mut side.h.events).await;
}
