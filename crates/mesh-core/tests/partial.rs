//! Partial replication in the node (sub-spec 8a §3): a client-role member replicates only
//! `_mesh`, announces its scope in `Hello`, and Full nodes never take a Partial peer's
//! `Covered` markers as proof that anything was seen.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{T, test_cfg, wait_until};
use ed25519_dalek::SigningKey;
use mesh_core::events::NodeEvent;
use mesh_core::frame::{SYNC_MAX_FRAME, read_frame, write_frame};
use mesh_core::membership::{MEMBERS, MESH_NS, MeshAcceptor, member_bytes};
use mesh_core::sync::{SessionCtx, SessionEnd, TestHandles, run_session};
use mesh_core::{CoreStore, DynClock, Node, NodeError, StoreError, StoreHandle, Timings};
use mesh_hlc::ManualClock;
use mesh_proto::{
    Freshness, InviteTicket, MeshGenesis, MeshId, NodeId, Role, Scope, SyncMsg, VersionVector,
};
use tokio::io::DuplexStream;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const TTL: Duration = Duration::from_secs(900);
const PROTOCOL: u64 = 3;

// ---------------------------------------------------------------- node helpers

async fn founder(dir: &Path) -> Node {
    Node::create_mesh(test_cfg(dir, "a").0).await.unwrap()
}

async fn join(dir: &Path, name: &str, ticket: &InviteTicket) -> Node {
    timeout(T, Node::join(test_cfg(dir, name).0, ticket.clone()))
        .await
        .expect("join timed out")
        .expect("join")
}

async fn vv_of(n: &Node) -> VersionVector {
    n.store().call(|s| s.version_vector()).await.unwrap()
}

async fn get(n: &Node, ns: &str, k: &str) -> Option<Vec<u8>> {
    n.store().get(ns, k).await.unwrap()
}

/// Waits until `to` holds everything of `from`'s own origin (ops or covered markers).
async fn caught_up_with(to: &Node, from: &Node) {
    let id = from.node_id();
    let want = vv_of(from).await.get(&id);
    wait_until("caught up", T, || async {
        vv_of(to).await.get(&id) >= want
    })
    .await;
}

/// Hosts A (founder) and B, client C (invited by A).
async fn mesh_abc(dirs: &[tempfile::TempDir]) -> (Node, Node, Node) {
    let a = founder(dirs[0].path()).await;
    let b = join(
        dirs[1].path(),
        "b",
        &a.invite(Role::Host, TTL).await.unwrap(),
    )
    .await;
    let c = join(
        dirs[2].path(),
        "c",
        &a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    assert!(b.wait_first_sync(T).await);
    assert!(c.wait_first_sync(T).await);
    (a, b, c)
}

fn dirs(n: usize) -> Vec<tempfile::TempDir> {
    (0..n).map(|_| tempfile::tempdir().unwrap()).collect()
}

async fn assert_no_svc(c: &Node) {
    assert_eq!(get(c, "svc", "k").await, None);
    assert!(
        c.store()
            .list_keys("svc", "", None, 100)
            .await
            .unwrap()
            .is_empty()
    );
    let sizes = c.store().call(|s| s.namespace_sizes()).await.unwrap();
    assert!(
        sizes.keys().all(|ns| ns == MESH_NS),
        "client holds {sizes:?}"
    );
}

// ---------------------------------------------------------------- node tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_store_holds_only_mesh() {
    let d = dirs(3);
    let (a, b, c) = mesh_abc(&d).await;
    assert_eq!(a.scope(), Scope::Full);
    assert_eq!(b.scope(), Scope::Full);
    assert_eq!(c.scope(), Scope::client());
    assert_eq!(c.handle().scope(), Scope::client());

    a.store().set("svc", "k", b"v".to_vec()).await.unwrap();
    wait_until("B has svc/k", T, || async {
        get(&b, "svc", "k").await.is_some()
    })
    .await;
    // C's vector moves past A's svc op (a covered marker from a Full host).
    caught_up_with(&c, &a).await;
    assert_no_svc(&c).await;
    wait_until("C sees both hosts", T, || async {
        let m = c.members();
        [a.node_id(), b.node_id()]
            .iter()
            .all(|id| m.iter().any(|s| s.id == *id && s.role == Role::Host))
    })
    .await;
    for n in [a, b, c] {
        n.shutdown().await;
    }
}

/// Ruling R1: the catch-up a client joiner gets (its first round with the inviter, which
/// recorded it as a client before replying) is scoped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_join_catch_up_is_scoped() {
    let d = dirs(2);
    let a = founder(d[0].path()).await;
    for i in 0..20 {
        a.store()
            .set("svc", &format!("k{i}"), b"v".to_vec())
            .await
            .unwrap();
    }
    a.store().set("svc", "k", b"v".to_vec()).await.unwrap();
    let ticket = a.invite(Role::Client, TTL).await.unwrap();
    let c = join(d[1].path(), "c", &ticket).await;
    // Set before any session ran.
    assert_eq!(c.scope(), Scope::client());
    assert!(c.wait_first_sync(T).await);
    caught_up_with(&c, &a).await;
    assert_no_svc(&c).await;
    wait_until("C sees A", T, || async {
        c.members().iter().any(|m| m.id == a.node_id())
    })
    .await;
    // Reopened, it is still a client.
    c.shutdown().await;
    let c = Node::open(test_cfg(d[1].path(), "c").0).await.unwrap();
    assert_eq!(c.scope(), Scope::client());
    a.shutdown().await;
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_receives_mesh_writes() {
    let d = dirs(3);
    let (a, b, c) = mesh_abc(&d).await;
    a.set_info(Some("renamed".into()), None).await.unwrap();
    wait_until("C sees A's new name", T, || async {
        c.members()
            .iter()
            .any(|m| m.id == a.node_id() && m.info.as_ref().is_some_and(|i| i.name == "renamed"))
    })
    .await;
    // And a client's own `_mesh` write reaches the hosts.
    c.set_info(Some("phone".into()), None).await.unwrap();
    wait_until("A sees C's new name", T, || async {
        a.members()
            .iter()
            .any(|m| m.id == c.node_id() && m.info.as_ref().is_some_and(|i| i.name == "phone"))
    })
    .await;
    for n in [a, b, c] {
        n.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_syncing_only_from_client_gets_data_later() {
    let d = dirs(4);
    let (a, b, c) = mesh_abc(&d).await;
    let ia = a.node_id();
    let first_svc = vv_of(&a).await.get(&ia) + 1;
    for i in 0..5 {
        a.store()
            .set("svc", &format!("k{i}"), b"v".to_vec())
            .await
            .unwrap();
    }
    a.store().set("svc", "k", b"v".to_vec()).await.unwrap();
    // A `_mesh` op above the svc run.
    a.set_info(Some("a2".into()), None).await.unwrap();
    caught_up_with(&c, &a).await;
    caught_up_with(&b, &a).await;

    // D's id before it exists, so the hosts can shut it out from the start.
    let id_d = NodeId(
        mesh_core::local::load_or_create_key(d[3].path())
            .unwrap()
            .verifying_key()
            .to_bytes(),
    );
    a.partition(&[id_d]);
    b.partition(&[id_d]);
    let dn = join(d[3].path(), "d", &c.invite(Role::Host, TTL).await.unwrap()).await;
    assert_eq!(dn.scope(), Scope::Full);
    assert!(dn.wait_first_sync(T).await, "D synced with C");
    // Several more complete rounds with C: everything C would ever send has arrived.
    let r0 = dn.handle().rounds_with_for_test(c.node_id());
    dn.sync_now();
    wait_until("more rounds with C", T, || async {
        dn.sync_now();
        dn.handle().rounds_with_for_test(c.node_id()) >= r0 + 2
    })
    .await;
    let seen = vv_of(&dn).await.get(&ia);
    assert!(
        seen < first_svc,
        "D's vv[A] = {seen} covers A's svc ops from {first_svc}"
    );
    assert_eq!(seen, first_svc - 1, "D holds A's ops below the svc run");
    assert_eq!(get(&dn, "svc", "k").await, None);
    assert_eq!(dn.handle().rounds_with_for_test(ia), 0);

    a.heal();
    b.heal();
    wait_until("D gets A's svc data", T, || async {
        get(&dn, "svc", "k").await.is_some()
    })
    .await;
    caught_up_with(&dn, &a).await;
    for i in 0..5 {
        assert!(get(&dn, "svc", &format!("k{i}")).await.is_some());
    }
    for n in [a, b, c, dn] {
        n.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scope_role_mismatch_refused() {
    let d = dirs(2);
    let a = founder(d[0].path()).await;
    let c = join(
        d[1].path(),
        "c",
        &a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    assert!(c.wait_first_sync(T).await);
    // Test hook: C now announces Full although A lists it as a client.
    c.store().inner().set_scope(Scope::Full);
    wait_until("A refuses C with PROTOCOL", T, || async {
        c.sync_now();
        c.last_peer_close_code(a.node_id()) == Some(PROTOCOL)
    })
    .await;
    a.shutdown().await;
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_write_outside_mesh_refused() {
    let d = dirs(2);
    let a = founder(d[0].path()).await;
    let c = join(
        d[1].path(),
        "c",
        &a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    let r = c.store().set("svc", "k", b"v".to_vec()).await;
    assert!(
        matches!(&r, Err(NodeError::Store(StoreError::OutOfScope { ns })) if ns == "svc"),
        "{r:?}"
    );
    c.store()
        .set(MESH_NS, "test/k", b"v".to_vec())
        .await
        .unwrap();
    a.shutdown().await;
    c.shutdown().await;
}

// ---------------------------------------------------------------- session tests

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

fn mesh() -> MeshId {
    genesis().mesh_id()
}

fn quiet() -> Timings {
    let hour = Duration::from_secs(3600);
    Timings {
        sync_interval: hour,
        silence: hour,
        ..Timings::fast_for_tests()
    }
}

/// The founder's store (node 1) listing node 2 as a host and node 3 as a client, with some
/// `app` ops of its own.
fn founder_store(dir: &Path) -> StoreHandle {
    founder_store_with_clock(dir).0
}

fn founder_store_with_clock(dir: &Path) -> (StoreHandle, ManualClock) {
    let clock = ManualClock::new(1_000_000);
    let s = CoreStore::open(
        &dir.join("a.redb"),
        mesh(),
        key(1),
        DynClock(Arc::new(clock.clone())),
    )
    .unwrap();
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(1), Role::Host))
        .unwrap();
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Host))
        .unwrap();
    s.set_add(MESH_NS, MEMBERS, &member_bytes(&id(3), Role::Client))
        .unwrap();
    for i in 0..4 {
        s.set("app", &format!("a{i}"), b"x").unwrap();
    }
    s.set("_mesh", "test/last", b"x").unwrap();
    (StoreHandle::new(Arc::new(s)), clock)
}

/// The founder's store, stale: its last full sync was 81 days ago.
fn stale_founder_store(dir: &Path) -> StoreHandle {
    let (a, clock) = founder_store_with_clock(dir);
    a.inner().mark_full_sync(0).unwrap();
    clock.advance(81 * 86_400_000);
    assert!(a.inner().is_stale().unwrap());
    a
}

struct Raw {
    r: DuplexStream,
    w: DuplexStream,
}

impl Raw {
    async fn send(&mut self, m: &SyncMsg) {
        write_frame(&mut self.w, m).await.unwrap();
    }

    async fn recv(&mut self) -> Option<SyncMsg> {
        timeout(T, read_frame(&mut self.r, SYNC_MAX_FRAME))
            .await
            .expect("frame in time")
            .unwrap()
    }

    /// Every frame up to and including the next `Done`.
    async fn until_done(&mut self) -> Vec<SyncMsg> {
        let mut out = Vec::new();
        loop {
            match self.recv().await {
                Some(SyncMsg::Done) => return out,
                Some(m) => out.push(m),
                None => panic!("session closed"),
            }
        }
    }
}

fn run_against_raw(
    store: &StoreHandle,
    peer: NodeId,
) -> (JoinHandle<SessionEnd>, TestHandles, Raw) {
    let (n_w, raw_r) = tokio::io::duplex(64 * 1024);
    let (raw_w, n_r) = tokio::io::duplex(64 * 1024);
    let (ctx, h) = SessionCtx::for_test(
        store.clone(),
        Arc::new(MeshAcceptor::new(genesis())),
        peer,
        quiet(),
    );
    let join = tokio::spawn(run_session(ctx, n_r, n_w, false));
    (join, h, Raw { r: raw_r, w: raw_w })
}

fn hello(vv: VersionVector, scope: Scope) -> SyncMsg {
    hello_with(vv, scope, Freshness::Fresh)
}

fn hello_with(vv: VersionVector, scope: Scope, freshness: Freshness) -> SyncMsg {
    SyncMsg::Hello {
        mesh: mesh(),
        vv,
        scope,
        freshness,
    }
}

const STALE: Freshness = Freshness::Stale { since_ms: 1 };

async fn next_round(ev: &mut broadcast::Receiver<NodeEvent>) {
    timeout(T, async {
        loop {
            if let NodeEvent::RoundComplete(_) = ev.recv().await.unwrap() {
                return;
            }
        }
    })
    .await
    .expect("round completes in time");
}

async fn end_of(join: JoinHandle<SessionEnd>) -> SessionEnd {
    timeout(T, join).await.expect("session ends").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn covered_from_partial_peer_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let a = founder_store(dir.path());
    let x = id(4);
    let before = a.inner().version_vector().unwrap();
    let synced_before = a.inner().synced_self_seq().unwrap();
    let (join, mut h, mut raw) = run_against_raw(&a, id(3));
    let mut adv = VersionVector::default();
    adv.set(x, 5);
    adv.set(id(1), 100);
    raw.send(&hello(adv, Scope::client())).await;
    raw.send(&SyncMsg::Covered { origin: x, upto: 5 }).await;
    raw.send(&SyncMsg::Covered {
        origin: id(1),
        upto: 100,
    })
    .await;
    raw.send(&SyncMsg::Done).await;
    assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
    raw.until_done().await;
    next_round(&mut h.events).await;
    assert_eq!(a.inner().version_vector().unwrap(), before, "vv unchanged");
    assert_eq!(a.inner().next_seq().unwrap(), before.get(&id(1)) + 1);
    // A round with a partial peer is not a full sync: its vv[me] proves nothing.
    assert_eq!(a.inner().synced_self_seq().unwrap(), synced_before);
    assert!(!join.is_finished());
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_peer_served_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let a = founder_store(dir.path());
    let own = a.inner().version_vector().unwrap().get(&id(1));
    let (_join, _h, mut raw) = run_against_raw(&a, id(3));
    raw.send(&hello(VersionVector::default(), Scope::client()))
        .await;
    let msgs = raw.until_done().await;
    let mut covered = Vec::new();
    for m in &msgs {
        match m {
            SyncMsg::Ops { ops } => {
                for o in ops {
                    assert_eq!(o.body.payload.ns(), MESH_NS, "{m:?}");
                }
            }
            SyncMsg::Covered { upto, .. } => covered.push(*upto),
            _ => {}
        }
    }
    // members ×3 (seqs 1-3), app ×4 (4-7) covered, then `_mesh` 8.
    assert_eq!(covered, vec![7]);
    assert_eq!(own, 8);
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_scope_must_match_role() {
    let dir = tempfile::tempdir().unwrap();
    let a = founder_store(dir.path());
    // A host announcing Partial, a client announcing Full: refused.
    for (peer, scope) in [(id(2), Scope::client()), (id(3), Scope::Full)] {
        let (join, _h, mut raw) = run_against_raw(&a, peer);
        raw.send(&hello(VersionVector::default(), scope)).await;
        assert!(matches!(end_of(join).await, SessionEnd::Protocol(_)));
    }
    // Matching scopes, and a peer with no known role, are served.
    for (peer, scope) in [
        (id(2), Scope::Full),
        (id(3), Scope::client()),
        (id(9), Scope::client()),
        (id(9), Scope::Full),
    ] {
        let (join, _h, mut raw) = run_against_raw(&a, peer);
        raw.send(&hello(VersionVector::default(), scope)).await;
        assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
        assert!(!join.is_finished());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_announces_own_scope() {
    let dir = tempfile::tempdir().unwrap();
    let a = founder_store(dir.path());
    for scope in [Scope::Full, Scope::client()] {
        a.inner().set_scope(scope.clone());
        let (_join, _h, mut raw) = run_against_raw(&a, id(2));
        raw.send(&hello(VersionVector::default(), Scope::Full))
            .await;
        match raw.recv().await {
            Some(SyncMsg::Hello { scope: s, .. }) => assert_eq!(s, scope),
            other => panic!("{other:?}"),
        }
    }
}

/// Ruling C1: a stale store syncs on with a client (it serves it, says it is stale, and
/// the round marks no full sync).
#[tokio::test(flavor = "multi_thread")]
async fn stale_store_keeps_syncing_with_a_client() {
    let dir = tempfile::tempdir().unwrap();
    let a = stale_founder_store(dir.path());
    let synced = a.inner().last_full_sync_ms().unwrap();
    let (join, mut h, mut raw) = run_against_raw(&a, id(3));
    raw.send(&hello(VersionVector::default(), Scope::client()))
        .await;
    match raw.recv().await {
        Some(SyncMsg::Hello { freshness, .. }) => {
            assert!(
                matches!(freshness, Freshness::Stale { .. }),
                "says it is stale"
            )
        }
        other => panic!("{other:?}"),
    }
    let msgs = raw.until_done().await;
    assert!(
        msgs.iter().any(|m| matches!(m, SyncMsg::Ops { .. })),
        "served"
    );
    raw.send(&SyncMsg::Done).await;
    next_round(&mut h.events).await;
    assert_eq!(a.inner().last_full_sync_ms().unwrap(), synced);
    assert!(!join.is_finished());
}

/// Ruling C1: a stale store meeting a fresh Full peer ends the session to re-bootstrap
/// from it, without serving it.
#[tokio::test(flavor = "multi_thread")]
async fn stale_store_and_fresh_host_end_stale() {
    let dir = tempfile::tempdir().unwrap();
    let a = stale_founder_store(dir.path());
    let (join, _h, mut raw) = run_against_raw(&a, id(2));
    raw.send(&hello(VersionVector::default(), Scope::Full))
        .await;
    assert!(matches!(end_of(join).await, SessionEnd::Stale));
    // Nothing was sent to it.
    assert!(matches!(
        timeout(
            Duration::from_secs(1),
            read_frame::<_, SyncMsg>(&mut raw.r, SYNC_MAX_FRAME)
        )
        .await,
        Ok(Ok(None)) | Ok(Err(_))
    ));
}

/// Ruling C1: two stale stores do not sync, and neither is told to re-bootstrap.
#[tokio::test(flavor = "multi_thread")]
async fn stale_store_and_stale_host_end_peer_stale() {
    let dir = tempfile::tempdir().unwrap();
    let a = stale_founder_store(dir.path());
    let vv = a.inner().version_vector().unwrap();
    let (join, _h, mut raw) = run_against_raw(&a, id(2));
    raw.send(&hello_with(VersionVector::default(), Scope::Full, STALE))
        .await;
    assert!(matches!(end_of(join).await, SessionEnd::PeerStale));
    assert_eq!(a.inner().version_vector().unwrap(), vv);
}

/// Ruling I3: in live mode, a client never gets a `Covered` above what the host has
/// announced (`Hello` or `Advertise`), and the in-scope op behind it follows.
#[tokio::test(flavor = "multi_thread")]
async fn live_markers_never_above_announced() {
    let dir = tempfile::tempdir().unwrap();
    let a = founder_store(dir.path());
    let me = id(1);
    let vv = a.inner().version_vector().unwrap();
    let (_join, h, mut raw) = run_against_raw(&a, id(3));
    raw.send(&hello(vv.clone(), Scope::client())).await;
    let mut told = match raw.recv().await {
        Some(SyncMsg::Hello { vv, .. }) => vv,
        other => panic!("{other:?}"),
    };
    raw.until_done().await;
    raw.send(&SyncMsg::Done).await;

    // Live: out-of-scope writes, then a `_mesh` write.
    for i in 0..3 {
        a.set("app", &format!("live{i}"), b"x".to_vec())
            .await
            .unwrap();
    }
    a.set(MESH_NS, "test/live", b"x".to_vec()).await.unwrap();
    let want = a.inner().version_vector().unwrap().get(&me);
    h.wake.notify_one();
    let mut covered = 0;
    loop {
        match raw.recv().await {
            Some(SyncMsg::Advertise { vv }) => told = vv,
            Some(SyncMsg::Covered { origin, upto }) => {
                assert!(
                    upto <= told.get(&origin),
                    "Covered upto {upto} above the announced {}",
                    told.get(&origin)
                );
                covered += 1;
            }
            Some(SyncMsg::Ops { ops }) => {
                assert!(ops.iter().all(|o| o.body.payload.ns() == MESH_NS));
                if ops.iter().any(|o| o.body.seq == want) {
                    break;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(covered >= 1);
}

/// A client data dir without `role.json` (lost, or from before it existed) still opens as
/// a client once its `members` record is in the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_dir_without_role_json_opens_partial() {
    let d = dirs(2);
    let a = founder(d[0].path()).await;
    let c = join(
        d[1].path(),
        "c",
        &a.invite(Role::Client, TTL).await.unwrap(),
    )
    .await;
    assert!(c.wait_first_sync(T).await);
    let ic = c.node_id();
    wait_until("C's member record", T, || async {
        c.members()
            .iter()
            .any(|m| m.id == ic && m.role == Role::Client)
    })
    .await;
    c.shutdown().await;
    std::fs::remove_file(d[1].path().join("role.json")).unwrap();
    let c = Node::open(test_cfg(d[1].path(), "c").0).await.unwrap();
    assert_eq!(c.scope(), Scope::client());
    assert!(matches!(
        c.store().set("svc", "k", b"v".to_vec()).await,
        Err(NodeError::Store(StoreError::OutOfScope { .. }))
    ));
    a.shutdown().await;
    c.shutdown().await;
}

/// Ruling A1: an `Advertise` before the peer's `Hello` (scope and role unchecked) is a
/// protocol violation; a `Covered` after it raises nothing.
#[tokio::test(flavor = "multi_thread")]
async fn advertise_before_hello_is_protocol_error() {
    let dir = tempfile::tempdir().unwrap();
    let a = founder_store(dir.path());
    let before = a.inner().version_vector().unwrap();
    let x = id(4);
    let (join, _h, mut raw) = run_against_raw(&a, id(3));
    let mut huge = VersionVector::default();
    huge.set(x, 1_000);
    raw.send(&SyncMsg::Advertise { vv: huge }).await;
    raw.send(&SyncMsg::Covered {
        origin: x,
        upto: 500,
    })
    .await;
    assert!(matches!(end_of(join).await, SessionEnd::Protocol(_)));
    assert_eq!(a.inner().version_vector().unwrap(), before);
}

/// Ruling A2: a stale client syncs on with a stale host (and with an empty one); it resets
/// only for a Fresh host.
#[tokio::test(flavor = "multi_thread")]
async fn stale_client_syncs_with_stale_host() {
    for peer in [STALE, Freshness::Empty] {
        let dir = tempfile::tempdir().unwrap();
        let a = stale_founder_store(dir.path());
        a.inner().set_scope(Scope::client());
        let (join, _h, mut raw) = run_against_raw(&a, id(2));
        raw.send(&hello_with(VersionVector::default(), Scope::Full, peer))
            .await;
        assert!(matches!(raw.recv().await, Some(SyncMsg::Hello { .. })));
        let msgs = raw.until_done().await;
        assert!(
            msgs.iter().any(|m| matches!(m, SyncMsg::Ops { .. })),
            "served"
        );
        assert!(!join.is_finished(), "{peer:?}");
    }
    let dir = tempfile::tempdir().unwrap();
    let a = stale_founder_store(dir.path());
    a.inner().set_scope(Scope::client());
    let (join, _h, mut raw) = run_against_raw(&a, id(2));
    raw.send(&hello(VersionVector::default(), Scope::Full))
        .await;
    assert!(matches!(end_of(join).await, SessionEnd::Stale));
}

/// Ruling C1b: a stale host serves an Empty Full host, and marks no full sync from it.
#[tokio::test(flavor = "multi_thread")]
async fn stale_host_serves_empty_host() {
    let dir = tempfile::tempdir().unwrap();
    let a = stale_founder_store(dir.path());
    let synced = a.inner().last_full_sync_ms().unwrap();
    let (join, mut h, mut raw) = run_against_raw(&a, id(2));
    raw.send(&hello_with(
        VersionVector::default(),
        Scope::Full,
        Freshness::Empty,
    ))
    .await;
    match raw.recv().await {
        Some(SyncMsg::Hello { freshness, .. }) => {
            assert_eq!(
                freshness,
                Freshness::Stale {
                    since_ms: synced.unwrap()
                }
            )
        }
        other => panic!("{other:?}"),
    }
    let msgs = raw.until_done().await;
    let ops: usize = msgs
        .iter()
        .map(|m| match m {
            SyncMsg::Ops { ops } => ops.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(ops as u64, a.inner().version_vector().unwrap().get(&id(1)));
    raw.send(&SyncMsg::Done).await;
    next_round(&mut h.events).await;
    assert_eq!(
        a.inner().last_full_sync_ms().unwrap(),
        synced,
        "not refreshed"
    );
    assert!(a.inner().is_stale().unwrap());
    assert!(!join.is_finished());
}

/// Ruling C1b: an Empty store that completes a round with a stale Full peer inherits its
/// last full sync, so it is stale too (and keeps every own op unsynced).
#[tokio::test(flavor = "multi_thread")]
async fn empty_store_inherits_staleness_from_stale_peer() {
    let dir = tempfile::tempdir().unwrap();
    let (a, clock) = founder_store_with_clock(dir.path());
    assert_eq!(a.inner().last_full_sync_ms().unwrap(), None);
    clock.advance(81 * 86_400_000);
    let (join, mut h, mut raw) = run_against_raw(&a, id(2));
    raw.send(&hello_with(
        VersionVector::default(),
        Scope::Full,
        Freshness::Stale {
            since_ms: 1_000_000,
        },
    ))
    .await;
    match raw.recv().await {
        Some(SyncMsg::Hello { freshness, .. }) => assert_eq!(freshness, Freshness::Empty),
        other => panic!("{other:?}"),
    }
    raw.until_done().await;
    raw.send(&SyncMsg::Done).await;
    next_round(&mut h.events).await;
    assert_eq!(a.inner().last_full_sync_ms().unwrap(), Some(1_000_000));
    assert!(a.inner().is_stale().unwrap());
    assert_eq!(a.inner().synced_self_seq().unwrap(), None);
    assert!(!join.is_finished());
}
