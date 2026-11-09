//! Scheduled maintenance and diagnostic events (spec §9, §3.2).

mod common;

use std::time::Duration;

use common::{T, test_cfg, wait_until};
use mesh_core::{MaintJob, Node};
use mesh_proto::{Role, VersionVector};
use mesh_store::SyncItem;

const NS: &str = "app";
const TEN_MIN_MS: u64 = 10 * 60 * 1000;
const DAY_MS: u64 = 24 * 3600 * 1000;

async fn op_count(n: &Node) -> usize {
    n.store()
        .call(|s| s.diff_since(&VersionVector::default(), 100_000))
        .await
        .unwrap()
        .iter()
        .filter(|i| matches!(i, SyncItem::Op(_)))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_job_runs() {
    let d = tempfile::tempdir().unwrap();
    let (cfg, clock) = test_cfg(d.path(), "a");
    let a = Node::create_mesh(cfg).await.unwrap();
    a.store().set(NS, "k", b"v1".to_vec()).await.unwrap();
    a.store().set(NS, "k", b"v2".to_vec()).await.unwrap();
    let before = op_count(&a).await;

    // Not yet old enough: compaction keeps the superseded op.
    a.maint_now(MaintJob::Compact).await.unwrap();
    assert_eq!(op_count(&a).await, before, "fresh superseded op is kept");

    clock.advance(8 * DAY_MS);
    a.maint_now(MaintJob::Compact).await.unwrap();
    assert_eq!(op_count(&a).await, before - 1, "old superseded op is gone");
    assert_eq!(
        a.store().get(NS, "k").await.unwrap().as_deref(),
        Some(&b"v2"[..])
    );
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prune_invites_job() {
    let d = tempfile::tempdir().unwrap();
    let (cfg, clock) = test_cfg(d.path(), "a");
    let a = Node::create_mesh(cfg).await.unwrap();
    a.invite(Role::Host, Duration::from_secs(60)).await.unwrap();
    let rows = || -> usize {
        let bytes = std::fs::read(d.path().join("invites.json")).unwrap();
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
            .unwrap()
            .len()
    };
    assert_eq!(rows(), 1);

    a.maint_now(MaintJob::PruneInvites).await.unwrap();
    assert_eq!(rows(), 1, "unexpired invite is kept");

    clock.advance(120_000);
    a.maint_now(MaintJob::PruneInvites).await.unwrap();
    assert_eq!(rows(), 0, "expired unredeemed invite is gone");
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clock_floor_ahead_event() {
    let d = tempfile::tempdir().unwrap();
    let (cfg, clock) = test_cfg(d.path(), "a");
    let a = Node::create_mesh(cfg).await.unwrap();
    clock.advance(TEN_MIN_MS);
    a.store().set(NS, "k", b"v".to_vec()).await.unwrap();
    a.shutdown().await;

    // Reopened with the wall clock back where it was: the stored floor is 10 min ahead.
    let (cfg, _clock) = test_cfg(d.path(), "a");
    let a = Node::open(cfg).await.unwrap();
    let ms = a.clock_floor_ahead_ms().await.expect("floor is ahead");
    assert!(ms >= TEN_MIN_MS - 5_000, "ms = {ms}");
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_tick_on_interval() {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let (mut cfg_a, clock_a) = test_cfg(da.path(), "a");
    cfg_a.timings.retry_pending = Duration::from_millis(100);
    let a = Node::create_mesh(cfg_a).await.unwrap();
    let ticket = a
        .invite(Role::Host, Duration::from_secs(900))
        .await
        .unwrap();
    let (cfg_b, clock_b) = test_cfg(db.path(), "b");
    let b = Node::join(cfg_b, ticket).await.unwrap();
    assert!(b.wait_first_sync(T).await);

    clock_b.advance(TEN_MIN_MS);
    b.store().set(NS, "skewed", b"v".to_vec()).await.unwrap();
    wait_until("A defers B's op", T, || async {
        a.store().call(|s| s.pending_len()).await.unwrap() >= 1
    })
    .await;
    assert_eq!(a.store().get(NS, "skewed").await.unwrap(), None);

    // No maint_now: the scheduled retry applies it once A's clock caught up.
    clock_a.advance(TEN_MIN_MS);
    wait_until("scheduled retry applies the op", T, || async {
        a.store().get(NS, "skewed").await.unwrap().as_deref() == Some(&b"v"[..])
    })
    .await;
    b.shutdown().await;
    a.shutdown().await;
}
