//! The stale-node re-bootstrap (spec §7; handoff Must 1 and 2, R10, R18, R19, R20).
//!
//! A store whose last full sync is older than 80 days may hold keys whose tombstones the
//! other replicas already garbage-collected, so it must neither serve nor keep its data.
//! It exports its unsynced own ops (to `rebootstrap.bin`, before anything is deleted),
//! empties itself, pulls everything back from its peers, then re-applies the export.
//!
//! Detection: when the node opens (`rebootstrap.bin` exists, or the store is stale), and when
//! a session ends with `SessionEnd::Stale`. Serving is paused (`serve_paused`) from the
//! moment staleness is seen until step 7; sessions still receive and apply.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mesh_proto::{NodeId, Payload};
use mesh_store::SyncItem;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::time::{MissedTickBehavior, sleep};
use tracing::{debug, error, info, warn};

use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::local::{
    RebootstrapFile, append_expired_edits, read_rebootstrap, remove_rebootstrap, write_rebootstrap,
};
use crate::node::{Inner, NodeStatus, publish_addr};

/// How often step 4 re-checks `vv[self]` besides each `RoundComplete` (events are lossy).
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Runs the re-bootstrap at start (if `due`) and whenever a session reports a stale store.
/// A failed run is retried after `backoff_max`.
pub(crate) async fn driver(
    inner: Arc<Inner>,
    stale: Arc<Mutex<mpsc::Receiver<NodeId>>>,
    due: bool,
) {
    let mut stale = stale.lock().await;
    let cancel = inner.cancel.clone();
    let mut due = due;
    loop {
        if due && let Err(e) = run_rebootstrap(&inner).await {
            error!(error = %e, "re-bootstrap failed; retrying");
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = sleep(inner.cfg.timings.backoff_max) => continue,
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = stale.recv() => match r {
                Some(peer) => debug!(peer = %peer.short(), "a session found the store stale"),
                None => return,
            },
        }
        due = true;
    }
}

/// Spec §7 steps 1–7. Does nothing (and lets sessions serve again) if the store is not stale
/// and no re-bootstrap is pending. Returns early, leaving `rebootstrap.bin` for the next
/// start, when the node is shut down meanwhile.
pub(crate) async fn run_rebootstrap(inner: &Inner) -> Result<(), NodeError> {
    let _one_at_a_time = inner.rebootstrap.lock().await;
    let dir = inner.cfg.data_dir.clone();
    if matches!(*inner.status.borrow(), NodeStatus::Removed { .. }) {
        return Ok(());
    }
    let d = dir.clone();
    let pending = blocking(move || read_rebootstrap(&d)).await?;
    let file = match pending {
        Some(file) => {
            // Crash safety: resume at step 3; the export is never recomputed.
            warn!(s = file.s, "resuming an interrupted re-bootstrap");
            pause(inner);
            file
        }
        None => {
            if !inner.store.call(|s| s.is_stale()).await? {
                resume_serving(inner);
                return Ok(());
            }
            pause(inner);
            // 1.
            warn!("store is stale (no full sync for 80 days); re-bootstrapping");
            let _ = inner.events.send(NodeEvent::RebootstrapStarting);
            // 2. `synced_self_seq` before the reset, which deletes it.
            let (s, export) = inner
                .store
                .call(|st| {
                    let s = st.synced_self_seq()?.unwrap_or(0);
                    Ok((s, st.export_unsynced_local_ops()?))
                })
                .await?;
            let file = RebootstrapFile {
                s,
                items: export.items,
                expired: export.expired,
            };
            let (d, f) = (dir.clone(), file.clone());
            blocking(move || write_rebootstrap(&d, &f)).await?;
            file
        }
    };

    // 3.
    inner.store.call(|st| st.reset_to_empty()).await?;
    let s = file.s;
    inner.status.send_if_modified(|st| match st {
        NodeStatus::Running | NodeStatus::RefusedAsRemoved { .. } => {
            *st = NodeStatus::Rebootstrapping {
                waiting_for_self_seq: s,
            };
            true
        }
        _ => false,
    });
    info!(waiting_for_self_seq = s, "store reset; catching up");
    #[cfg(any(test, feature = "test-hooks"))]
    if inner.crash_after_reset.swap(false, Ordering::SeqCst) {
        warn!("test hook: crashing right after the reset");
        inner.cancel.cancel();
        return Ok(());
    }

    // 4. Rounds (a pull: nothing is served) until a peer gave back the synced own ops.
    if !wait_for_self_seq(inner, s).await? {
        return Ok(());
    }

    // 5. Covered markers and ops, in export order (a marker may follow ops).
    let acc = inner.acc.clone();
    let items = file.items;
    inner
        .store
        .call(move |st| {
            let mut ops = Vec::new();
            for item in items {
                match item {
                    SyncItem::Op(op) => ops.push(op),
                    SyncItem::Covered { origin, upto } => {
                        st.apply_batch(std::mem::take(&mut ops), &*acc)?;
                        // Trusted local data: the marker is its own bound.
                        st.apply_covered(origin, upto, upto, &*acc)?;
                    }
                }
            }
            st.apply_batch(ops, &*acc).map(drop)
        })
        .await?;

    // 6. Own edits too old to re-apply (R19).
    if !file.expired.is_empty() {
        let keys: Vec<(String, String)> = file
            .expired
            .iter()
            .filter_map(|op| match &op.body.payload {
                Payload::Set { ns, key, .. } => Some((ns.clone(), key.clone())),
                Payload::SetAdd { .. } => None,
            })
            .collect();
        let (d, ops) = (dir.clone(), file.expired);
        let count = ops.len();
        blocking(move || append_expired_edits(&d, &ops)).await?;
        warn!(count, "own offline edits were too old to re-apply");
        let _ = inner
            .events
            .send(NodeEvent::ExpiredOfflineEdits { count, keys });
    }

    // 7.
    blocking(move || remove_rebootstrap(&dir)).await?;
    inner.store.refuse_writes_rebootstrapping(false);
    inner.status.send_if_modified(|st| {
        let was = matches!(st, NodeStatus::Rebootstrapping { .. });
        if was {
            *st = NodeStatus::Running;
        }
        was
    });
    resume_serving(inner);
    // The membership rescans held back during the catch-up: one full rescan now.
    inner.force_lagged.notify_one();
    // A restart during the re-bootstrap may have moved this node's address.
    if let Err(e) = publish_addr(&inner.store, inner.me, &inner.endpoint.addr()).await {
        warn!(error = %e, "could not publish own address");
    }
    info!("re-bootstrap done");
    let _ = inner.events.send(NodeEvent::RebootstrapDone);
    Ok(())
}

/// Stops serving and refuses local writes until the re-bootstrap is done.
fn pause(inner: &Inner) {
    inner.serve_paused.store(true, Ordering::SeqCst);
    inner.store.refuse_writes_rebootstrapping(true);
}

/// Lets sessions serve again and wakes every one of them.
fn resume_serving(inner: &Inner) {
    if inner.serve_paused.swap(false, Ordering::SeqCst) {
        inner.peers.wake_all();
    }
}

/// Waits until `vv[self] >= s`, checking after each completed round and every
/// [`CHECK_EVERY`]. False if the node was stopped first.
async fn wait_for_self_seq(inner: &Inner, s: u64) -> Result<bool, NodeError> {
    let me = inner.me;
    let mut events = inner.events.subscribe();
    let mut poll = tokio::time::interval(CHECK_EVERY);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let have = inner
            .store
            .call(move |st| Ok(st.version_vector()?.get(&me)))
            .await?;
        if have >= s {
            return Ok(true);
        }
        debug!(have, s, "waiting for this node's synced ops");
        loop {
            tokio::select! {
                _ = inner.cancel.cancelled() => return Ok(false),
                _ = poll.tick() => break,
                ev = events.recv() => match ev {
                    Ok(NodeEvent::RoundComplete(_)) | Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(false),
                },
            }
        }
    }
}

/// Runs blocking file work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, NodeError> + Send + 'static,
) -> Result<T, NodeError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| NodeError::Io(std::io::Error::other(e)))?
}
