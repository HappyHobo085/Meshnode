//! The stale-node re-bootstrap (spec §7; handoff Must 1 and 2, R10, R18, R19, R20).
//!
//! A store whose last full sync is older than 80 days may hold keys whose tombstones the
//! other replicas already garbage-collected, so it must neither serve nor keep its data.
//! It exports its unsynced own ops (to `rebootstrap.bin`, before anything is deleted),
//! empties itself, pulls everything back from its peers, then re-applies the export.
//!
//! Detection: when the node opens with `rebootstrap.bin` (an interrupted run resumes), and
//! when a session ends with `SessionEnd::Stale`, which it does only when this store is
//! stale and a **Fresh Full** peer announced itself (sub-spec 8a ruling C1c). A stale
//! store is never reset otherwise, and staleness never blocks syncing: a stale node syncs
//! with stale, Empty and Partial peers as usual. A round with a Fresh peer (host or
//! client) refreshes the last full sync; one with a Stale or Empty peer refreshes nothing,
//! and a never-synced (Empty) store inherits a Stale peer's staleness. So a mesh where
//! every node is stale stays stale but fully working (no owner action is needed): all its
//! nodes keep syncing, and they re-bootstrap once a Fresh Full peer appears. Serving is
//! paused (`serve_paused`) from the reset decision until step 7; sessions still receive
//! and apply. The run completes only after a completed pull from a Full peer since the
//! reset (ruling I2), besides getting its own ops and `members` record back.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mesh_proto::{NodeId, Payload, Role};
use mesh_store::SyncItem;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::time::{MissedTickBehavior, sleep};
use tracing::{debug, error, info, warn};

use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::local::{
    RebootstrapFile, append_expired_edits, read_rebootstrap, remove_rebootstrap, write_rebootstrap,
};
use crate::membership::{MEMBERS, MESH_NS, member_bytes};
use crate::node::{Inner, NodeStatus, publish_addr};

/// How often step 4 re-checks `vv[self]` besides each `RoundComplete` (events are lossy).
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Runs the re-bootstrap at start (if `due`: an interrupted run) and whenever a session
/// reports a stale store next to a fresh Full peer. A failed run is retried after
/// `backoff_max`.
pub(crate) async fn driver(
    inner: Arc<Inner>,
    stale: Arc<Mutex<mpsc::Receiver<NodeId>>>,
    due: bool,
) {
    let mut stale = stale.lock().await;
    let cancel = inner.cancel.clone();
    let mut due = due;
    // A stale store is reset only right after a session met a fresh Full peer.
    let mut fresh_peer = false;
    loop {
        if due && let Err(e) = run_rebootstrap(&inner, fresh_peer).await {
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
        fresh_peer = true;
    }
}

/// Spec §7 steps 1–7. Does nothing (and lets sessions serve again) if no re-bootstrap is
/// pending and the store is not stale, or is stale but `fresh_peer` is false (no Full,
/// non-stale peer was just met to restore from; ruling C1). Returns early, leaving
/// `rebootstrap.bin` for the next start, when the node is shut down meanwhile.
pub(crate) async fn run_rebootstrap(inner: &Inner, fresh_peer: bool) -> Result<(), NodeError> {
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
            pause(inner).await;
            file
        }
        None => {
            if !fresh_peer || !inner.store.call_raw(|s| s.is_stale()).await? {
                resume_serving(inner);
                return Ok(());
            }
            // From here no own op can be written until step 7.
            pause(inner).await;
            // 1.
            warn!("store is stale (no full sync for 80 days); re-bootstrapping");
            let _ = inner.events.send(NodeEvent::RebootstrapStarting);
            // 2. `synced_self_seq` before the reset, which deletes it.
            let (s, last_seq, export) = inner
                .store
                .call_raw(|st| {
                    let s = st.synced_self_seq()?.unwrap_or(0);
                    let last_seq = st.next_seq()?.saturating_sub(1);
                    Ok((s, last_seq, st.export_unsynced_local_ops()?))
                })
                .await?;
            let peers = {
                let snap = inner.membership.borrow();
                snap.effective_members()
                    .into_iter()
                    .filter(|id| *id != inner.me)
                    .filter_map(|id| snap.addr(&id).map(|a| (id, a.clone())))
                    .collect()
            };
            let file = RebootstrapFile {
                s,
                last_seq,
                items: export.items,
                expired: export.expired,
                peers,
            };
            let (d, f) = (dir.clone(), file.clone());
            blocking(move || write_rebootstrap(&d, &f)).await?;
            file
        }
    };

    // 3. The status changes before the reset, so whoever reads the store while the
    // status is `Running` reads it whole.
    let s = file.s;
    // `RefusedAsRemoved` stays visible.
    inner.status.send_if_modified(|st| {
        let running = matches!(st, NodeStatus::Running);
        if running {
            *st = NodeStatus::Rebootstrapping {
                waiting_for_self_seq: s,
            };
        }
        running
    });
    #[cfg(any(test, feature = "test-hooks"))]
    {
        let mut hold = inner.hold_before_reset.subscribe();
        tokio::select! {
            _ = inner.cancel.cancelled() => return Ok(()),
            _ = hold.wait_for(|on| !*on) => {}
        }
    }
    let next = inner
        .store
        .call_raw(|st| {
            st.reset_to_empty()?;
            st.next_seq()
        })
        .await?;
    // Only a pull answering a `Hello` sent from now on counts (ruling I2); new rounds send
    // one with the emptied vector (a round in progress counts once the next one starts).
    let epoch = inner.pulls.begin_epoch();
    inner.peers.start_round_all();
    if next.saturating_sub(1) != file.last_seq {
        // An own op was written after the export: it is gone, and the store would refuse
        // every later local write. Never carry on silently.
        error!(
            last_seq = file.last_seq,
            next_seq = next,
            "an own op was written between the re-bootstrap export and the reset; stopping"
        );
        return Err(NodeError::Io(std::io::Error::other(
            "own op written between the re-bootstrap export and the reset",
        )));
    }
    info!(waiting_for_self_seq = s, "store reset; catching up");
    #[cfg(any(test, feature = "test-hooks"))]
    if inner.crash_after_reset.swap(false, Ordering::SeqCst) {
        warn!("test hook: crashing right after the reset");
        inner.cancel.cancel();
        return Ok(());
    }

    // 4. Rounds (a pull: nothing is served) until a peer gave back the synced own ops and
    // this node's `members` record (without it the re-applied own ops would be held back).
    // 5. Then re-apply the export; retried until every exported op was accepted.
    let items = Arc::new(file.items);
    loop {
        match wait_for_self_seq(inner, s, epoch).await? {
            Wait::Reached => {}
            Wait::Stopped => return Ok(()),
            Wait::Removed => {
                // Nothing left to rebuild: the node is stopped for good (spec §3.3 step 6).
                warn!("this node was removed during its re-bootstrap; abandoning it");
                blocking(move || remove_rebootstrap(&dir)).await?;
                inner.store.refuse_writes_rebootstrapping(false);
                inner.serve_paused.store(false, Ordering::SeqCst);
                return Ok(());
            }
        }
        if reapply(inner, items.clone(), file.last_seq).await? {
            break;
        }
        // `rebootstrap.bin` stays: a crash now resumes and re-applies again.
        warn!("some re-applied own ops were held back; retrying");
        tokio::select! {
            _ = inner.cancel.cancelled() => return Ok(()),
            _ = sleep(CHECK_EVERY) => {}
        }
        let acc = inner.acc.clone();
        inner
            .store
            .call_raw(move |st| st.retry_pending(&*acc))
            .await?;
    }

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

/// Step 5: the export's ops and `Covered` markers in export order (a marker may follow
/// ops), then a marker up to the last own seq used (a trailing expired or compacted op
/// leaves a hole the export has no marker for, and local writes need
/// `vv[self] == next_seq - 1`). A self marker drops every pending own op below it, so one is
/// applied only once `vv[self]` covers every exported op before it; otherwise this stops
/// and returns false (re-applying again later is harmless).
async fn reapply(
    inner: &Inner,
    items: Arc<Vec<SyncItem>>,
    last_seq: u64,
) -> Result<bool, NodeError> {
    let (acc, me) = (inner.acc.clone(), inner.me);
    inner
        .store
        .call_raw(move |st| {
            let mut ops = Vec::new();
            // The highest exported op seq seen so far.
            let mut highest = 0;
            let covered = |st: &crate::db::CoreStore, ops: &mut Vec<_>, highest: u64| {
                st.apply_batch(std::mem::take(ops), &*acc)?;
                Ok::<_, mesh_store::StoreError>(st.version_vector()?.get(&me) >= highest)
            };
            for item in items.iter() {
                match item {
                    SyncItem::Op(op) => {
                        highest = highest.max(op.body.seq);
                        ops.push(op.clone());
                    }
                    SyncItem::Covered { origin, upto } => {
                        if !covered(st, &mut ops, highest)? {
                            return Ok(false);
                        }
                        // Trusted local data: the marker is its own bound.
                        st.apply_covered(*origin, *upto, *upto, &*acc)?;
                    }
                }
            }
            if !covered(st, &mut ops, highest)? {
                return Ok(false);
            }
            if last_seq > 0 {
                st.apply_covered(me, last_seq, last_seq, &*acc)?;
            }
            Ok(true)
        })
        .await
}

/// Stops serving, refuses local writes until the re-bootstrap is done, and waits for the
/// local writes already admitted to finish.
async fn pause(inner: &Inner) {
    inner.serve_paused.store(true, Ordering::SeqCst);
    inner.store.refuse_writes_rebootstrapping(true);
    inner.store.drain_writes().await;
}

/// How step 4 ended.
enum Wait {
    Reached,
    /// The node is shutting down.
    Stopped,
    /// This node's removal arrived.
    Removed,
}

/// Lets sessions serve again and wakes every one of them.
fn resume_serving(inner: &Inner) {
    if inner.serve_paused.swap(false, Ordering::SeqCst) {
        inner.peers.wake_all();
    }
}

/// Waits until `vv[self] >= s`, this node's `members` record is back and a Full peer has
/// answered a `Hello` sent in `epoch` with everything it holds (ruling I2: own ops back
/// from a client alone leave every other namespace missing), checking after each
/// completed round, each status change and every [`CHECK_EVERY`]; gives up if the node
/// stops or is removed meanwhile.
async fn wait_for_self_seq(inner: &Inner, s: u64, epoch: u64) -> Result<Wait, NodeError> {
    let (me, founder) = (inner.me, inner.genesis.founder);
    let mut events = inner.events.subscribe();
    let mut status = inner.status.subscribe();
    let mut poll = tokio::time::interval(CHECK_EVERY);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let (have, member) = inner
            .store
            .call_raw(move |st| {
                let member = me == founder
                    || [Role::Host, Role::Client]
                        .into_iter()
                        .try_fold(false, |found, role| {
                            Ok::<_, mesh_store::StoreError>(
                                found
                                    || st.set_contains(
                                        MESH_NS,
                                        MEMBERS,
                                        &member_bytes(&me, role),
                                    )?,
                            )
                        })?;
                Ok((st.version_vector()?.get(&me), member))
            })
            .await?;
        if matches!(*status.borrow_and_update(), NodeStatus::Removed { .. }) {
            return Ok(Wait::Removed);
        }
        let pulled = inner.pulls.reached(epoch);
        if have >= s && member && pulled {
            return Ok(Wait::Reached);
        }
        debug!(
            have,
            s, member, pulled, "waiting for own synced ops, members record and a Full pull"
        );
        loop {
            tokio::select! {
                _ = inner.cancel.cancelled() => return Ok(Wait::Stopped),
                _ = poll.tick() => break,
                r = status.changed() => match r {
                    Ok(()) => break,
                    Err(_) => return Ok(Wait::Stopped),
                },
                ev = events.recv() => match ev {
                    Ok(NodeEvent::RoundComplete(_)) | Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(Wait::Stopped),
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
