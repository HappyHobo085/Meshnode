//! Which hosts run a service (spec §5.1), and where manifests live in `_mesh`.

use mesh_core::membership::MESH_NS;
use mesh_core::{MemberStatus, NodeHandle};
use mesh_proto::{NodeId, Placement, Role, ServiceManifest, decode_raw};
use tracing::warn;

const PAGE: u32 = 256;
const MANIFEST_PREFIX: &str = "service/";

/// `_mesh` register holding the manifest of `name`.
pub fn manifest_key(name: &str) -> String {
    format!("{MANIFEST_PREFIX}{name}")
}

/// The live manifest of `name` as `node` sees it: `None` if it is undeployed (or never
/// deployed), or cannot be read (logged).
pub async fn read_manifest(node: &NodeHandle, name: &str) -> Option<ServiceManifest> {
    fetch_manifest(node, name).await.ok().flatten()
}

/// Like [`read_manifest`], but tells undeployed (`Ok(None)`) from unreadable (`Err`, logged).
pub(crate) async fn fetch_manifest(
    node: &NodeHandle,
    name: &str,
) -> Result<Option<ServiceManifest>, ()> {
    match node.store().get(MESH_NS, &manifest_key(name)).await {
        Ok(Some(raw)) => decode_raw(&raw).map(Some).map_err(|e| {
            warn!(service = name, error = %e, "cannot read the service manifest");
        }),
        Ok(None) => Ok(None),
        Err(e) => {
            warn!(service = name, error = %e, "cannot read the service manifest");
            Err(())
        }
    }
}

/// Every readable manifest as `node` sees `_mesh`, sorted by service name. Unreadable ones
/// are skipped (logged); a store error ends the listing with what was read.
pub async fn list_manifests(node: &NodeHandle) -> Vec<ServiceManifest> {
    let store = node.store();
    let mut names = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = match store
            .list_keys(MESH_NS, MANIFEST_PREFIX, after.take(), PAGE)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "cannot list the service manifests");
                break;
            }
        };
        let full = page.len() == PAGE as usize;
        after = page.last().cloned();
        names.extend(
            page.iter()
                .filter_map(|k| k.strip_prefix(MANIFEST_PREFIX).map(str::to_owned)),
        );
        if !full {
            break;
        }
    }
    names.sort();
    names.dedup();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if let Some(m) = read_manifest(node, &name).await {
            out.push(m);
        }
    }
    out
}

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

/// [`placed`] over an explicit membership snapshot.
pub fn placed_among(m: &ServiceManifest, me: NodeId, members: &[MemberStatus]) -> bool {
    members.iter().any(|s| s.id == me && selects(m, s))
}

/// [`placed_hosts`] over an explicit membership snapshot.
pub fn hosts_among(m: &ServiceManifest, members: &[MemberStatus]) -> Vec<NodeId> {
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
