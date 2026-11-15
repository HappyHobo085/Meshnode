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
//!
//! Role upgrade (sub-spec 8a §3): a client whose effective role becomes host (re-invited
//! as host) re-bootstraps through the same steps as Full, once a session met a Fresh Full
//! peer (ruling R8, like C1): its Partial store is reset and caught up in full, and only
//! then does its scope flip to Full and `role.json` say `host` (see [`run_rebootstrap`]).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mesh_proto::{NodeId, Payload, Role, Scope};
use mesh_store::SyncItem;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::time::{MissedTickBehavior, sleep};
use tracing::{debug, error, info, warn};

use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::local::{
    RebootstrapFile, append_expired_edits, read_rebootstrap, remove_rebootstrap, write_own_role,
    write_rebootstrap,
};
use crate::membership::{MEMBERS, MESH_NS, member_bytes};
use crate::node::{Inner, NodeStatus, publish_addr};

/// How often step 4 re-checks `vv[self]` besides each `RoundComplete` (events are lossy).
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Runs the re-bootstrap at start (if `due`: an interrupted run) and whenever a session
/// reports a stale store, or a due role upgrade, next to a fresh Full peer. When a
/// membership change makes a role upgrade due, it only starts rounds with every peer, so
/// that a Fresh Full peer's `Hello` (the reset trigger) comes soon. A failed run is
/// retried after `backoff_max` (also one that failed after an upgrade flipped the scope:
/// the retry resumes from `rebootstrap.bin`).
pub(crate) async fn driver(
    inner: Arc<Inner>,
    stale: Arc<Mutex<mpsc::Receiver<NodeId>>>,
    due: bool,
) {
    let mut stale = stale.lock().await;
    let mut members = inner.membership.subscribe();
    let cancel = inner.cancel.clone();
    let mut due = due;
    // A stale store is reset only right after a session met a fresh Full peer.
    let mut fresh_peer = false;
    loop {
        if due {
            match run_rebootstrap(&inner, fresh_peer).await {
                Ok(()) => (due, fresh_peer) = (false, false),
                Err(e) => {
                    error!(error = %e, "re-bootstrap failed; retrying");
                    due = true;
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = sleep(inner.cfg.timings.backoff_max) => continue,
                    }
                }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = stale.recv() => match r {
                Some(peer) => {
                    debug!(peer = %peer.short(), "a session found the store stale");
                    (due, fresh_peer) = (true, true);
                }
                None => return,
            },
            r = members.changed() => {
                if r.is_err() {
                    return;
                }
                if upgrade_due(&inner) {
                    debug!("role upgrade due; starting rounds to meet a Fresh Full peer");
                    inner.peers.start_round_all();
                }
            }
        }
    }
}

/// Whether this node must re-bootstrap as Full (sub-spec 8a §3 role upgrade): it is an
/// effective host in `_mesh` (re-invited as host; host outranks client) but its store is
/// still Partial, so it holds only `_mesh` and its vector moved past everything else.
fn upgrade_due(inner: &Inner) -> bool {
    let host = {
        let snap = inner.membership.borrow();
        snap.is_effective(&inner.me) && snap.role(&inner.me) == Some(Role::Host)
    };
    host && inner.store.raw().scope() != Scope::Full
}

/// Spec §7 steps 1–7. Does nothing (and lets sessions serve again) if no re-bootstrap is
/// pending, no role upgrade is due ([`upgrade_due`]) and the store is not stale, or is
/// stale but `fresh_peer` is false (no Full, non-stale peer was just met to restore from;
/// ruling C1). Returns early, leaving `rebootstrap.bin` for the next start, when the node
/// is shut down meanwhile.
///
/// A role upgrade, like a stale reset, starts only right after a session met a Fresh Full
/// peer (`fresh_peer`, ruling R8): with no source to rebuild from, the node stays a
/// working Partial client (it syncs, and writes `_mesh`) instead of an empty, paused
/// store. Its rebuild announces `Full` from the reset on
/// ([`crate::sync::FullPulls::set_rebuild_full`]), takes data only from Fresh Full peers
/// (and `_mesh` from clients) like any rebuild, and only once done flips the store's scope
/// to Full and rewrites `role.json` as `host`. A Partial node's re-bootstrap for any other
/// reason stays Partial (a scoped catch-up).
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
            let upgrade = fresh_peer && upgrade_due(inner);
            if !upgrade && (!fresh_peer || !inner.store.call_raw(|s| s.is_stale()).await?) {
                resume_serving(inner);
                return Ok(());
            }
            // From here no own op can be written until step 7.
            pause(inner).await;
            // 1.
            if upgrade {
                warn!("this client is a host now; re-bootstrapping as a full replica");
            } else {
                warn!("store is stale (no full sync for 80 days); re-bootstrapping");
            }
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
                full: upgrade,
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
    // The reset and the new epoch are one step under `decide` (ruling R9): a round that
    // records itself (`maybe_complete`) checks its epoch under the same lock, so it either
    // writes into the old store (which the reset then clears) or sees the new epoch and
    // records nothing.
    // A role upgrade asks for everything from here on: every Hello of the new epoch
    // announces `Full` (with the emptied vector), so the pull credit below is a full one.
    // Only a pull answering a `Hello` sent from now on counts (ruling I2); new rounds send
    // one with the emptied vector (a round in progress counts once the next one starts).
    let (pulls, full) = (inner.pulls.clone(), file.full);
    let (next, epoch) = inner
        .store
        .call_raw(move |st| {
            // Must hold `decide` from the reset through `begin_epoch`: `record_round` and the
            // `Covered` apply check the epoch under it, which is what keeps a pre-reset round
            // or marker out of the emptied store (rulings R9, R10).
            let _decide = pulls.decide();
            st.reset_to_empty()?;
            pulls.set_rebuild_full(full);
            let epoch = pulls.begin_epoch();
            Ok((st.next_seq()?, epoch))
        })
        .await?;
    inner.peers.start_round_all();
    // Every writer runs once more, so a session that spans the reset ends now (R10).
    inner.peers.wake_all();
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
                inner.pulls.end_rebuild();
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

    // 7. The store was rebuilt from a Fresh Full peer only (`check_rebuild_source`): it is a
    // fresh copy now (sub-spec 8a C1c), so it cannot inherit staleness and re-bootstrap
    // again.
    inner
        .store
        .call_raw(|st| st.refresh_full_sync_time())
        .await?;
    // A role upgrade is done: the store is a full replica now (sub-spec 8a §3). The scope
    // flips before serving resumes, and `role.json` is rewritten before `rebootstrap.bin`
    // goes, so a restart in between still resumes as Full.
    if file.full {
        inner.store.raw().set_scope(Scope::Full);
        let d = dir.clone();
        blocking(move || write_own_role(&d, Role::Host)).await?;
        inner.pulls.set_rebuild_full(false);
        info!("role upgrade done: this node is a full replica now");
    }
    blocking(move || remove_rebootstrap(&dir)).await?;
    inner.pulls.end_rebuild();
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
