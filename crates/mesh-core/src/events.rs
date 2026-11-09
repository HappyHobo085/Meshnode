//! Events a node publishes to its subscribers (spec §3.2).

use mesh_proto::NodeId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeEvent {
    PeerUp(NodeId),
    PeerDown(NodeId),
    MembersChanged,
    /// A `sync/1` round with this peer completed on this side (spec §6.3).
    RoundComplete(NodeId),
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
