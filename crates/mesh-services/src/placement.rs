//! Which hosts run a service (spec §5.1).

use mesh_core::{MemberStatus, NodeHandle};
use mesh_proto::{NodeId, Placement, Role, ServiceManifest};

/// Whether `node` runs `m`: it must be an effective host, then match the placement
/// (`AllHosts`; its own `info/<me>` tags intersect `Tags`; it is listed in `Nodes`).
pub fn placed(m: &ServiceManifest, node: &NodeHandle) -> bool {
    placed_among(m, node.node_id(), &node.members())
}

/// Every effective host that `m`'s placement selects, as `node` sees the membership.
/// Empty means no host runs the service (a warning for the UI).
pub fn placed_hosts(m: &ServiceManifest, node: &NodeHandle) -> Vec<NodeId> {
    hosts_among(m, &node.members())
}

pub(crate) fn placed_among(m: &ServiceManifest, me: NodeId, members: &[MemberStatus]) -> bool {
    members.iter().any(|s| s.id == me && selects(m, s))
}

pub(crate) fn hosts_among(m: &ServiceManifest, members: &[MemberStatus]) -> Vec<NodeId> {
    members
        .iter()
        .filter(|s| selects(m, s))
        .map(|s| s.id)
        .collect()
}

fn selects(m: &ServiceManifest, s: &MemberStatus) -> bool {
    if !s.effective || s.role != Role::Host {
        return false;
    }
    match &m.placement {
        Placement::AllHosts => true,
        Placement::Tags(want) => s
            .info
            .as_ref()
            .is_some_and(|i| i.tags.iter().any(|t| want.contains(t))),
        Placement::Nodes(ids) => ids.contains(&s.id),
    }
}
