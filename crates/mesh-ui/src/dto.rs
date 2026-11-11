//! UI-shaped data (camelCase JSON, ids as lowercase hex), exported to TypeScript.

use mesh_admin::{
    DaemonState, LocalStateView, MemberInfoView, ServiceInfoView, ServiceKindView, StatusInfo,
};
use mesh_proto::{Limits, NodeId, Placement, Role};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::UiError;

/// A stable colour for a node: the first two id bytes modulo 360.
pub fn accent_hue(id: &NodeId) -> u16 {
    u16::from_be_bytes([id.0[0], id.0[1]]) % 360
}

pub fn node_hex(id: &NodeId) -> String {
    hex::encode(id.0)
}

pub fn parse_node_id(s: &str) -> Result<NodeId, UiError> {
    let bytes = hex::decode(s.trim()).map_err(|_| UiError::invalid("not a valid node id"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| UiError::invalid("a node id is 64 hex characters"))?;
    Ok(NodeId(arr))
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum DaemonStateDto {
    Uninitialised,
    Running,
    Removed,
    Rebootstrapping,
}

impl From<DaemonState> for DaemonStateDto {
    fn from(s: DaemonState) -> Self {
        match s {
            DaemonState::Uninitialised => DaemonStateDto::Uninitialised,
            DaemonState::Running => DaemonStateDto::Running,
            DaemonState::Removed => DaemonStateDto::Removed,
            DaemonState::Rebootstrapping => DaemonStateDto::Rebootstrapping,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum RoleDto {
    Host,
    Client,
}

impl From<Role> for RoleDto {
    fn from(r: Role) -> Self {
        match r {
            Role::Host => RoleDto::Host,
            Role::Client => RoleDto::Client,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PortDto {
    pub port: u16,
    pub service: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub state: DaemonStateDto,
    pub node_id: Option<String>,
    pub mesh_id: Option<String>,
    pub name: Option<String>,
    pub role: Option<RoleDto>,
    pub tags: Vec<String>,
    pub members_total: u32,
    pub members_online: u32,
    pub services: u32,
    pub gateway_url: Option<String>,
    pub ports: Vec<PortDto>,
    pub daemon_version: String,
    pub data_dir: String,
}

impl From<StatusInfo> for StatusDto {
    fn from(s: StatusInfo) -> Self {
        StatusDto {
            state: s.state.into(),
            node_id: s.node_id.as_ref().map(node_hex),
            mesh_id: s.mesh_id.map(|m| hex::encode(m.0)),
            name: s.name,
            role: s.role.map(Into::into),
            tags: s.tags,
            members_total: s.members_total,
            members_online: s.members_online,
            services: s.services,
            gateway_url: s.gateway.map(|g| format!("http://{g}")),
            ports: s
                .ports
                .into_iter()
                .map(|(port, service)| PortDto { port, service })
                .collect(),
            daemon_version: s.daemon_version,
            data_dir: s.data_dir,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct MemberDto {
    pub id: String,
    pub name: String,
    pub role: RoleDto,
    pub platform: String,
    pub tags: Vec<String>,
    pub is_self: bool,
    pub effective: bool,
    pub online: bool,
    #[ts(type = "number | null")]
    pub rtt_ms: Option<u64>,
    #[ts(type = "number | null")]
    pub last_seen_ms: Option<u64>,
    #[ts(type = "number")]
    pub lag: u64,
    pub accent_hue: u16,
}

impl From<MemberInfoView> for MemberDto {
    fn from(m: MemberInfoView) -> Self {
        MemberDto {
            id: node_hex(&m.id),
            name: m.name,
            role: m.role.into(),
            platform: m.platform,
            tags: m.tags,
            is_self: m.is_self,
            effective: m.effective,
            online: m.online,
            rtt_ms: m.rtt_ms,
            last_seen_ms: m.last_seen_ms,
            lag: m.lag,
            accent_hue: accent_hue(&m.id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PlacementDto {
    AllHosts,
    Tags { tags: Vec<String> },
    Nodes { ids: Vec<String> },
}

impl From<Placement> for PlacementDto {
    fn from(p: Placement) -> Self {
        match p {
            Placement::AllHosts => PlacementDto::AllHosts,
            Placement::Tags(tags) => PlacementDto::Tags { tags },
            Placement::Nodes(ids) => PlacementDto::Nodes {
                ids: ids.iter().map(node_hex).collect(),
            },
        }
    }
}

impl PlacementDto {
    pub fn into_placement(self) -> Result<Placement, UiError> {
        Ok(match self {
            PlacementDto::AllHosts => Placement::AllHosts,
            PlacementDto::Tags { tags } => Placement::Tags(tags),
            PlacementDto::Nodes { ids } => Placement::Nodes(
                ids.iter()
                    .map(|s| parse_node_id(s))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct LimitsDto {
    pub memory_mb: u32,
    #[ts(type = "number")]
    pub request_timeout_ms: u64,
    #[ts(type = "number")]
    pub max_body_bytes: u64,
}

impl From<Limits> for LimitsDto {
    fn from(l: Limits) -> Self {
        LimitsDto {
            memory_mb: l.memory_mb,
            request_timeout_ms: l.request_timeout_ms,
            max_body_bytes: l.max_body_bytes,
        }
    }
}

impl From<LimitsDto> for Limits {
    fn from(l: LimitsDto) -> Self {
        Limits {
            memory_mb: l.memory_mb,
            request_timeout_ms: l.request_timeout_ms,
            max_body_bytes: l.max_body_bytes,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum ServiceKindDto {
    Wasm,
    Static,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LocalStateDto {
    NotPlaced,
    Fetching,
    Ready {
        #[ts(type = "number")]
        version: u64,
    },
    Failed {
        error: String,
    },
}

impl From<LocalStateView> for LocalStateDto {
    fn from(s: LocalStateView) -> Self {
        match s {
            LocalStateView::NotPlaced => LocalStateDto::NotPlaced,
            LocalStateView::Fetching => LocalStateDto::Fetching,
            LocalStateView::Ready { version } => LocalStateDto::Ready { version },
            LocalStateView::Failed { error } => LocalStateDto::Failed { error },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDto {
    pub name: String,
    pub kind: ServiceKindDto,
    #[ts(type = "number")]
    pub version: u64,
    pub placement: PlacementDto,
    pub placed_hosts: Vec<String>,
    pub local: LocalStateDto,
    pub limits: LimitsDto,
    pub outbound: Vec<String>,
    #[ts(type = "number")]
    pub data_bytes: u64,
}

impl From<ServiceInfoView> for ServiceDto {
    fn from(s: ServiceInfoView) -> Self {
        ServiceDto {
            name: s.name,
            kind: match s.kind {
                ServiceKindView::Wasm => ServiceKindDto::Wasm,
                ServiceKindView::Static => ServiceKindDto::Static,
            },
            version: s.version,
            placement: s.placement.into(),
            placed_hosts: s.placed_hosts.iter().map(node_hex).collect(),
            local: s.local.into(),
            limits: s.limits.into(),
            outbound: s.outbound,
            data_bytes: s.data_bytes,
        }
    }
}

/// What a deploy sets on the new manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct DeployOptsDto {
    pub limits: LimitsDto,
    pub placement: PlacementDto,
    pub outbound: Vec<String>,
}

impl DeployOptsDto {
    pub fn into_opts(self) -> Result<mesh_admin::DeployOpts, UiError> {
        Ok(mesh_admin::DeployOpts {
            limits: self.limits.into(),
            placement: self.placement.into_placement()?,
            outbound: self.outbound,
        })
    }
}

/// An invite for another device (filled in by the pairing commands).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InviteDto {
    pub text: String,
    pub qr_svg: String,
    #[ts(type = "number")]
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ConfigEntry {
    pub key: String,
    pub value: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSetDto {
    pub restart_required: bool,
}
