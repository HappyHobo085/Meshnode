//! Scheduled maintenance (spec §9): one job runner shared by `Node::maint_now` and the
//! periodic scheduler.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tracing::{debug, warn};

use crate::error::NodeError;
use crate::node::{Inner, NodeStatus, join_err, lock};

/// Scheduled maintenance jobs (spec §9). `Node::maint_now` runs one at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintJob {
    RetryPending,
    Compact,
    Gc,
    PruneInvites,
}

/// Runs one job to completion.
pub(crate) async fn run_job(inner: &Inner, job: MaintJob) -> Result<(), NodeError> {
    let store = &inner.store;
    match job {
        MaintJob::RetryPending => {
            let acc = inner.acc.clone();
            store.call(move |s| s.retry_pending(&*acc)).await.map(drop)
        }
        MaintJob::Compact => store.call(|s| s.compact()).await.map(drop),
        MaintJob::Gc => store.call(|s| s.gc()).await.map(drop),
        MaintJob::PruneInvites => {
            let invites = inner.invites.clone();
            let now = inner.cfg.wall.now_ms();
            tokio::task::spawn_blocking(move || lock(&invites).prune(now))
                .await
                .map_err(join_err)?
        }
    }
}

/// Whether the store is being re-bootstrapped (status, or serving paused before the
/// status flips at startup).
fn rebootstrapping(inner: &Inner) -> bool {
    matches!(*inner.status.borrow(), NodeStatus::Rebootstrapping { .. })
        || inner.serve_paused.load(Ordering::SeqCst)
}

/// The periodic scheduler: one timer per job, the first run one full interval after start.
/// `compact` and `gc` are skipped while the store is being re-bootstrapped. A late timer
/// (deep sleep) simply fires late, and missed ticks are not replayed.
pub(crate) async fn run(inner: std::sync::Arc<Inner>) {
    let t = &inner.cfg.timings;
    let start = Instant::now();
    let timer = |every: Duration| {
        let every = every.max(Duration::from_millis(1));
        let mut tm = interval_at(start + every, every);
        tm.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tm
    };
    let mut retry = timer(t.retry_pending);
    let mut compact = timer(t.compact);
    let mut gc = timer(t.gc);
    let mut prune = timer(t.invite_prune);
    loop {
        let job = tokio::select! {
            _ = inner.cancel.cancelled() => return,
            _ = retry.tick() => MaintJob::RetryPending,
            _ = compact.tick() => MaintJob::Compact,
            _ = gc.tick() => MaintJob::Gc,
            _ = prune.tick() => MaintJob::PruneInvites,
        };
        if matches!(job, MaintJob::Compact | MaintJob::Gc) && rebootstrapping(&inner) {
            debug!(?job, "skipped: store is being re-bootstrapped");
            continue;
        }
        if let Err(e) = run_job(&inner, job).await {
            warn!(?job, error = %e, "maintenance job failed");
        }
    }
}
