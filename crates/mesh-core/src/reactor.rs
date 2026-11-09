//! The change reactor (spec §3.3): store `Change` events are only wake-up signals. Every
//! event wakes every session's live push; a `_mesh` event or a `Lagged` error runs a full
//! membership rescan, so a lost event costs nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mesh_proto::{MeshGenesis, NodeId};
use mesh_store::Change;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{Notify, broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::db::StoreHandle;
use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::membership::{MESH_NS, MembershipSnapshot, MeshAcceptor};
use crate::node::NodeStatus;
use crate::peers::{PeerManager, SignOut};
use crate::transport::REMOVED;

/// Everything a rescan reads or updates; cloned into each reactor run.
#[derive(Clone)]
pub(crate) struct ReactorCtx {
    pub store: StoreHandle,
    pub acc: Arc<MeshAcceptor>,
    pub genesis: MeshGenesis,
    pub me: NodeId,
    pub membership: Arc<watch::Sender<Arc<MembershipSnapshot>>>,
    pub status: Arc<watch::Sender<NodeStatus>>,
    pub peers: PeerManager,
    pub events: broadcast::Sender<NodeEvent>,
    /// Test hook: behave as if the change subscription lagged.
    pub force_lagged: Arc<Notify>,
    /// Test hook: discard change events (see `Node::lose_events`).
    pub lose_events: Arc<AtomicBool>,
    pub signout: Arc<SignOut>,
}

pub(crate) struct Reactor {
    ctx: ReactorCtx,
    /// Subscribed at construction, so a rescan run before `run` misses no event.
    rx: Receiver<Change>,
    /// Whether a rescan already ran since `rx` was subscribed.
    scanned: bool,
    /// Cutoffs already passed to `on_removal`; cleared on `Lagged` and on restart.
    applied: HashMap<NodeId, u64>,
    /// Whether `NoEffectiveHost` was emitted for the current state.
    no_host: bool,
}

impl Reactor {
    pub fn new(ctx: ReactorCtx) -> Reactor {
        Reactor {
            rx: ctx.store.inner().subscribe(),
            scanned: false,
            ctx,
            applied: HashMap::new(),
            no_host: false,
        }
    }

    /// Rescans once unless that already happened since the subscription (a restart after
    /// a panic gets a fresh reactor), then reacts to events until cancelled.
    pub async fn run(mut self, cancel: CancellationToken) {
        if !self.scanned {
            self.rescan_logged(true).await;
        }
        loop {
            let mut rescan = false;
            let mut lagged = false;
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.ctx.force_lagged.notified() => lagged = true,
                _ = self.ctx.signout.proved() => {
                    info!("sign-out reached a peer; stopping");
                    self.ctx.peers.stop();
                    continue;
                }
                r = self.rx.recv() => match r {
                    Ok(ch) => rescan |= is_mesh(&ch) && !self.lost(),
                    Err(RecvError::Lagged(n)) => {
                        warn!(missed = n, "change events lagged; rescanning");
                        lagged = true;
                    }
                    Err(RecvError::Closed) => return,
                },
            }
            // Coalesce a burst into one wake-up and at most one rescan.
            loop {
                match self.rx.try_recv() {
                    Ok(ch) => rescan |= is_mesh(&ch) && !self.lost(),
                    Err(TryRecvError::Lagged(_)) => lagged = true,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => return,
                }
            }
            self.ctx.peers.wake_all();
            if rescan || lagged {
                self.rescan_logged(lagged).await;
            }
        }
    }

    fn lost(&self) -> bool {
        self.ctx.lose_events.load(Ordering::SeqCst)
    }

    pub async fn rescan_logged(&mut self, lagged: bool) {
        self.scanned = true;
        if let Err(e) = self.rescan(lagged).await {
            error!(error = %e, "membership rescan failed");
        }
    }

    /// One full membership rescan (spec §3.3 steps 1–7).
    pub async fn rescan(&mut self, lagged: bool) -> Result<(), NodeError> {
        let c = &self.ctx;
        if lagged {
            self.applied.clear();
        }
        // 1. Rebuild the snapshot.
        let g = c.genesis.clone();
        let snap = c
            .store
            .call(move |s| MembershipSnapshot::load(s, &g))
            .await?;

        // 2. Apply raised cutoffs.
        for (id, removal) in snap.removals() {
            if self.applied.get(id).is_some_and(|&c| c >= removal.cutoff) {
                continue;
            }
            let (origin, cutoff) = (*id, removal.cutoff);
            let report = c.store.call(move |s| s.on_removal(origin, cutoff)).await?;
            self.applied.insert(origin, cutoff);
            if !report.unrestorable_keys.is_empty() {
                info!(
                    origin = %origin.short(),
                    keys = report.unrestorable_keys.len(),
                    "removal could not restore some keys (diagnostic, over-reports)"
                );
                let _ = c.events.send(NodeEvent::RetractionUnrestorable {
                    origin,
                    keys: report.unrestorable_keys.len(),
                });
            }
        }

        // 3. Ops held back for an unknown origin may now be acceptable.
        let acc = c.acc.clone();
        c.store.call(move |s| s.retry_pending(&*acc)).await?;

        let snap = Arc::new(snap);
        let changed = **c.membership.borrow() != *snap;
        c.membership.send_replace(snap.clone());
        if changed {
            let _ = c.events.send(NodeEvent::MembersChanged);
        }

        // 4. Close connections to peers that are no longer effective.
        for id in c.peers.target_ids() {
            if !snap.is_effective(&id) {
                c.peers.close(id, REMOVED);
            }
        }

        // 5. Dial every effective member but this node (none once this node is removed).
        let targets = snap
            .effective_members()
            .into_iter()
            .filter(|id| *id != c.me)
            .map(|id| (id, snap.addr(&id).cloned()))
            .collect();
        // (a sign-out keeps its targets until a peer has the record, see step 6)
        if snap.removed_by(&c.me).is_none_or(|by| by == c.me) {
            c.peers.set_targets(targets);
        }

        // 6. This node was removed: stop for good (also across restarts).
        if let Some(by) = snap.removed_by(&c.me) {
            let already = matches!(*c.status.borrow(), NodeStatus::Removed { .. });
            if !already {
                warn!(by = %by.short(), "this node was removed from the mesh");
                c.store.refuse_writes_removed();
                c.status.send_replace(NodeStatus::Removed { by });
                let _ = c.events.send(NodeEvent::RemovedSelf { by });
            }
            if by != c.me {
                c.peers.stop();
            } else if !c.signout.begun() {
                // Sign-out: the record may not have left this node yet, so the sessions stay
                // until a peer proves it holds the record (`SignOut`).
                let me = c.me;
                let seq = c
                    .store
                    .call(move |s| Ok(s.version_vector()?.get(&me)))
                    .await?;
                c.signout.begin(seq);
            }
        }

        // 7. No effective host left.
        let no_host = snap.effective_hosts().is_empty();
        if no_host && !self.no_host {
            warn!("no effective host remains in the mesh");
            let _ = c.events.send(NodeEvent::NoEffectiveHost);
        }
        self.no_host = no_host;
        Ok(())
    }
}

fn is_mesh(ch: &Change) -> bool {
    ch.ns == MESH_NS
}
