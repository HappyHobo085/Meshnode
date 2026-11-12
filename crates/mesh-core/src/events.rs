//! Events a node publishes to its subscribers (spec §3.2).

use mesh_proto::NodeId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeEvent {
    PeerUp(NodeId),
    PeerDown(NodeId),
    MembersChanged,
    /// A `sync/1` round with this peer completed on this side (spec §6.3).
    RoundComplete(NodeId),
    /// The first round since start completed, with any peer. If that peer is a client
    /// (sub-spec 8a), this node has the membership (`_mesh`) but not necessarily any
    /// service data: it does not mean "holds the mesh's data".
    FirstSyncComplete,
    RemovedSelf {
        by: NodeId,
    },
    RefusedAsRemoved(NodeId),
    NoEffectiveHost,
    RebootstrapStarting,
    RebootstrapDone,
    ExpiredOfflineEdits {
        count: usize,
        keys: Vec<(String, String)>,
    },
    ClockFloorAhead {
        ms: u64,
    },
    /// Diagnostics only; over-reports and must not be shown to users as data loss.
    RetractionUnrestorable {
        origin: NodeId,
        keys: usize,
    },
}
