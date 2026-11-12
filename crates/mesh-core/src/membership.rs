//! The `_mesh` membership schema (spec §4), the acceptor that judges ops by it, and a
//! point-in-time snapshot of the derived values.

use std::collections::BTreeMap;

use mesh_proto::{
    MemberInfo, MeshGenesis, NodeAddrBytes, NodeId, OpBody, RemovalRecord, Role, Scope,
};
use mesh_store::{Acceptor, ReadView, StoreError, Verdict};
use tracing::warn;

use crate::db::CoreStore;

pub const MESH_NS: &str = "_mesh";
/// Grow-only set of `node_id[32] ‖ role[1]` (`0` = host, `1` = client).
pub const MEMBERS: &str = "members";
/// Grow-only set of `postcard(RemovalRecord)`; exempt from cutoff retraction.
pub const REMOVED: &str = "removed";
/// Grow-only set of `blake3(token)`.
pub const CONSUMED: &str = "consumed_invites";

const MEMBER_LEN: usize = 33;
const PAGE: u32 = 256;

/// Register key of a member's `MemberInfo`.
pub fn info_key(id: &NodeId) -> String {
    format!("info/{}", hex::encode(id.0))
}

/// Register key of a member's endpoint address.
pub fn addr_key(id: &NodeId) -> String {
    format!("addr/{}", hex::encode(id.0))
}

/// The `members` set element for `id` with `role`.
pub fn member_bytes(id: &NodeId, role: Role) -> [u8; MEMBER_LEN] {
    let mut b = [0u8; MEMBER_LEN];
    b[..32].copy_from_slice(&id.0);
    b[32] = match role {
        Role::Host => 0,
        Role::Client => 1,
    };
    b
}

/// `_mesh` register value of `info/<hex>`: raw postcard, no version byte.
pub fn encode_info(info: &MemberInfo) -> Vec<u8> {
    // Serializing into a Vec cannot fail for this type.
    postcard::to_allocvec(info).unwrap_or_default()
}

/// `_mesh` register value of `addr/<hex>`: the address bytes as they are.
pub fn encode_addr(addr: &NodeAddrBytes) -> Vec<u8> {
    addr.0.clone()
}

/// `_mesh` set `removed` element: raw postcard, no version byte.
pub fn encode_removal(rec: &RemovalRecord) -> Vec<u8> {
    postcard::to_allocvec(rec).unwrap_or_default()
}

/// Judges membership only; the removal cutoff is enforced inside the store.
pub struct MeshAcceptor {
    genesis: MeshGenesis,
}

impl MeshAcceptor {
    pub fn new(genesis: MeshGenesis) -> Self {
        MeshAcceptor { genesis }
    }

    /// `id`'s role in `_mesh` as [`MembershipSnapshot::role`] derives it (the founder is
    /// a host; host outranks client), without loading the whole snapshot. `None` if `id`
    /// is not listed (yet). Blocking.
    pub fn role_of(&self, store: &CoreStore, id: &NodeId) -> Result<Option<Role>, StoreError> {
        if *id == self.genesis.founder {
            return Ok(Some(Role::Host));
        }
        for role in [Role::Host, Role::Client] {
            if store.set_contains(MESH_NS, MEMBERS, &member_bytes(id, role))? {
                return Ok(Some(role));
            }
        }
        Ok(None)
    }
}

/// The replication scope of a member with `role` (sub-spec 8a §3): a client replicates
/// only `_mesh`, a host everything.
pub fn scope_of_role(role: Role) -> Scope {
    match role {
        Role::Host => Scope::Full,
        Role::Client => Scope::client(),
    }
}

impl Acceptor for MeshAcceptor {
    fn judge(&self, op: &OpBody, view: &ReadView<'_>) -> Verdict {
        if op.origin == self.genesis.founder {
            return Verdict::Valid;
        }
        for role in [Role::Host, Role::Client] {
            match view.set_contains(MESH_NS, MEMBERS, &member_bytes(&op.origin, role)) {
                Ok(true) => return Verdict::Valid,
                Ok(false) => {}
                Err(e) => {
                    warn!(error = %e, "membership read failed; holding op back");
                    return Verdict::UnknownOrigin;
                }
            }
        }
        Verdict::UnknownOrigin
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberEntry {
    pub role: Role,
    pub info: Option<MemberInfo>,
    pub addr: Option<NodeAddrBytes>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    /// Highest `cutoff_seq` over the records naming the node.
    pub cutoff: u64,
    /// Origin of the highest-cutoff record (ties: lowest origin).
    pub by: NodeId,
}

/// Membership as of one read of the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MembershipSnapshot {
    members: BTreeMap<NodeId, MemberEntry>,
    removed: BTreeMap<NodeId, Removal>,
}

/// All members of `ns/set`, paged.
fn all_members(store: &CoreStore, set: &str) -> Result<Vec<Vec<u8>>, StoreError> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    loop {
        let page = store.set_members(MESH_NS, set, out.last().map(Vec::as_slice), PAGE)?;
        let done = page.len() < PAGE as usize;
        out.extend(page);
        if done {
            return Ok(out);
        }
    }
}

impl MembershipSnapshot {
    /// Blocking; reads the `members` and `removed` sets and each member's `info` / `addr`.
    pub fn load(store: &CoreStore, genesis: &MeshGenesis) -> Result<Self, StoreError> {
        // Host wins over client; the founder is always a host.
        let mut roles: BTreeMap<NodeId, Role> = BTreeMap::new();
        roles.insert(genesis.founder, Role::Host);
        for m in all_members(store, MEMBERS)? {
            let Ok(raw) = <[u8; MEMBER_LEN]>::try_from(m.as_slice()) else {
                warn!(len = m.len(), "ignoring malformed members element");
                continue;
            };
            let id = NodeId(<[u8; 32]>::try_from(&raw[..32]).unwrap_or_default());
            match raw[32] {
                0 => {
                    roles.insert(id, Role::Host);
                }
                1 => {
                    roles.entry(id).or_insert(Role::Client);
                }
                other => warn!(role = other, "ignoring members element with unknown role"),
            }
        }

        let mut members = BTreeMap::new();
        for (id, role) in roles {
            let info = match store.get(MESH_NS, &info_key(&id))? {
                Some(b) => postcard::from_bytes::<MemberInfo>(&b)
                    .map_err(|e| warn!(node = %id.short(), error = %e, "bad member info"))
                    .ok(),
                None => None,
            };
            let addr = store.get(MESH_NS, &addr_key(&id))?.map(NodeAddrBytes);
            members.insert(id, MemberEntry { role, info, addr });
        }

        // Per node: the highest cutoff, with every element that decodes to it. Postcard
        // ignores trailing bytes, so one record can exist as several byte variants (one
        // element each); they are the same record, and its remover is the lowest origin
        // of any of them, so adding a variant cannot change `by`.
        let mut best: BTreeMap<NodeId, (u64, Vec<Vec<u8>>)> = BTreeMap::new();
        for raw in all_members(store, REMOVED)? {
            let rec = match postcard::from_bytes::<RemovalRecord>(&raw) {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "ignoring malformed removal record");
                    continue;
                }
            };
            match best.get_mut(&rec.node_id) {
                Some((c, raws)) if rec.cutoff_seq == *c => raws.push(raw),
                Some((c, _)) if rec.cutoff_seq < *c => {}
                _ => {
                    best.insert(rec.node_id, (rec.cutoff_seq, vec![raw]));
                }
            }
        }
        let mut removed = BTreeMap::new();
        for (id, (cutoff, raws)) in best {
            // Identical records from several removers are one element; take the lowest origin.
            let mut by: Option<NodeId> = None;
            for raw in &raws {
                for origin in store.set_member_origins(MESH_NS, REMOVED, raw)? {
                    by = Some(by.map_or(origin, |b| b.min(origin)));
                }
            }
            let Some(by) = by else {
                continue;
            };
            removed.insert(id, Removal { cutoff, by });
        }
        Ok(MembershipSnapshot { members, removed })
    }

    pub fn is_member(&self, id: &NodeId) -> bool {
        self.members.contains_key(id)
    }

    pub fn is_effective(&self, id: &NodeId) -> bool {
        self.is_member(id) && !self.removed.contains_key(id)
    }

    pub fn role(&self, id: &NodeId) -> Option<Role> {
        self.members.get(id).map(|m| m.role)
    }

    pub fn cutoff(&self, id: &NodeId) -> Option<u64> {
        self.removed.get(id).map(|r| r.cutoff)
    }

    pub fn removed_by(&self, id: &NodeId) -> Option<NodeId> {
        self.removed.get(id).map(|r| r.by)
    }

    pub fn effective_hosts(&self) -> Vec<NodeId> {
        self.members
            .iter()
            .filter(|(id, m)| m.role == Role::Host && !self.removed.contains_key(id))
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn effective_members(&self) -> Vec<NodeId> {
        self.members
            .keys()
            .filter(|id| !self.removed.contains_key(id))
            .copied()
            .collect()
    }

    /// Every member (effective or not), in `NodeId` order.
    pub fn members(&self) -> impl Iterator<Item = (&NodeId, &MemberEntry)> {
        self.members.iter()
    }

    /// Every node with at least one removal record, with its effective removal.
    pub fn removals(&self) -> impl Iterator<Item = (&NodeId, &Removal)> {
        self.removed.iter()
    }

    pub fn info(&self, id: &NodeId) -> Option<&MemberInfo> {
        self.members.get(id)?.info.as_ref()
    }

    pub fn addr(&self, id: &NodeId) -> Option<&NodeAddrBytes> {
        self.members.get(id)?.addr.as_ref()
    }
}
