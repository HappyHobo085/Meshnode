//! What a client tells its subscribers, and the task that derives it from the node's events
//! and store changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use mesh_core::membership::MESH_NS;
use mesh_core::{Change, ChangeKind, NodeEvent, NodeHandle, NodeStatus};
use mesh_forward::list_manifests;
use mesh_proto::{Role, ServiceManifest};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

/// Events published to [`crate::MeshClient::subscribe`] receivers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// The membership or a member's link (online, RTT) changed: read `members()` again.
    MembersChanged,
    /// A service was deployed, changed or undeployed, or the hosts that may run one
    /// changed: read `services()` again.
    ServicesChanged,
    /// This client was removed from the mesh. Sent once; every call fails with
    /// [`crate::ClientError::Removed`] from now on.
    Removed,
    /// At least one host is online again (after none was, or at start).
    Online,
    /// No host is online any more.
    Offline,
}

const CAPACITY: usize = 256;
/// `_mesh` registers holding service manifests.
const MANIFEST_PREFIX: &str = "service/";

/// State shared by the client and its event task.
pub(crate) struct Shared {
    tx: broadcast::Sender<ClientEvent>,
    removed: AtomicBool,
    /// Every readable manifest, as of the last refresh (the sync `services()` reads it).
    manifests: Mutex<Vec<ServiceManifest>>,
}

impl Shared {
    pub(crate) fn new() -> Shared {
        Shared {
            tx: broadcast::channel(CAPACITY).0,
            removed: AtomicBool::new(false),
            manifests: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.tx.subscribe()
    }

    fn send(&self, e: ClientEvent) {
        // No receiver is no error.
        let _ = self.tx.send(e);
    }

    /// Whether this client has been removed, from the flag or the node's status (sending
    /// `Removed` the first time it is seen).
    pub(crate) fn is_removed(&self, node: &NodeHandle) -> bool {
        if self.removed.load(Ordering::SeqCst) {
            return true;
        }
        let removed = matches!(
            node.status(),
            NodeStatus::Removed { .. } | NodeStatus::RefusedAsRemoved { .. }
        );
        if removed {
            self.mark_removed();
        }
        removed
    }

    fn mark_removed(&self) {
        if !self.removed.swap(true, Ordering::SeqCst) {
            self.send(ClientEvent::Removed);
        }
    }

    pub(crate) fn manifests(&self) -> MutexGuard<'_, Vec<ServiceManifest>> {
        self.manifests.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) async fn refresh_manifests(&self, node: &NodeHandle) {
        let list = list_manifests(node).await;
        *self.manifests() = list;
    }
}

/// Online effective hosts other than this node.
pub(crate) fn online_hosts(node: &NodeHandle) -> usize {
    let me = node.node_id();
    node.members()
        .iter()
        .filter(|m| m.id != me && m.role == Role::Host && m.effective && m.online)
        .count()
}

/// The node's receivers, taken before the first manifest read so no change is missed.
pub(crate) struct Receivers {
    events: broadcast::Receiver<NodeEvent>,
    changes: broadcast::Receiver<Change>,
}

impl Receivers {
    pub(crate) fn new(node: &NodeHandle) -> Receivers {
        Receivers {
            events: node.subscribe(),
            changes: node.store().subscribe(),
        }
    }
}

fn is_manifest(c: &Change) -> bool {
    c.ns == MESH_NS && matches!(&c.kind, ChangeKind::Key(k) if k.starts_with(MANIFEST_PREFIX))
}

/// Runs until the node's channels close (or the task is aborted at shutdown).
pub(crate) async fn run(node: NodeHandle, shared: Arc<Shared>, rx: Receivers) {
    let Receivers {
        mut events,
        mut changes,
    } = rx;
    let mut online = false;
    loop {
        let now = online_hosts(&node) > 0;
        if now != online {
            online = now;
            shared.send(if now {
                ClientEvent::Online
            } else {
                ClientEvent::Offline
            });
        }
        shared.is_removed(&node);
        tokio::select! {
            r = events.recv() => match r {
                Ok(NodeEvent::MembersChanged) => {
                    shared.send(ClientEvent::MembersChanged);
                    // Placement depends on the membership (hosts, their tags).
                    shared.send(ClientEvent::ServicesChanged);
                }
                Ok(NodeEvent::PeerUp(_) | NodeEvent::PeerDown(_)) => {
                    shared.send(ClientEvent::MembersChanged);
                }
                Ok(NodeEvent::RemovedSelf { .. } | NodeEvent::RefusedAsRemoved(_)) => {
                    shared.mark_removed();
                }
                Ok(NodeEvent::RebootstrapDone) | Err(RecvError::Lagged(_)) => {
                    shared.refresh_manifests(&node).await;
                    shared.send(ClientEvent::MembersChanged);
                    shared.send(ClientEvent::ServicesChanged);
                }
                Ok(_) => {}
                Err(RecvError::Closed) => return,
            },
            r = changes.recv() => match r {
                Ok(c) if !is_manifest(&c) => {}
                Ok(_) | Err(RecvError::Lagged(_)) => {
                    // Take every change already queued: one refresh covers them.
                    while let Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) =
                        changes.try_recv()
                    {}
                    shared.refresh_manifests(&node).await;
                    shared.send(ClientEvent::ServicesChanged);
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}
