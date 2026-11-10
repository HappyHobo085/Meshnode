//! Admin wire types (spec §5.1). Every message is one `mesh_core::frame` frame.

use std::collections::HashSet;
use std::fmt;

use mesh_proto::{Limits, MeshId, NodeId, Placement, RefuseReason, Role, ServiceManifest};
use serde::{Deserialize, Serialize};

/// The admin protocol version both sides send in [`Hello`].
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest file in a static upload (one [`FileData`] frame).
pub const MAX_UPLOAD_FILE: u64 = 64 * 1024 * 1024;
/// Most files in a static upload.
pub const MAX_UPLOAD_FILES: usize = 100_000;
/// Largest total of a static upload.
pub const MAX_UPLOAD_TOTAL: u64 = 1024 * 1024 * 1024;
/// Largest frame: a 64 MiB file plus its envelope.
pub const MAX_FRAME: usize = MAX_UPLOAD_FILE as usize + 1024;
/// Most samples one `Probe` may ask for.
pub const MAX_PROBE_COUNT: u32 = 1000;
/// Most lines `LogsTail` may ask for.
pub const MAX_LOG_LINES: u32 = 10_000;

/// The first frame in each direction. The client sends `state: None`; the daemon answers
/// with its version and `state: Some(..)`. Its shape must never change, so that any two
/// versions can tell they differ.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub state: Option<DaemonState>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonState {
    /// No mesh yet (`mesh.json` missing): only `Init`, `Join` and a few local requests.
    Uninitialised,
    Running,
    Removed,
    Rebootstrapping,
}

impl fmt::Display for DaemonState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DaemonState::Uninitialised => "uninitialised",
            DaemonState::Running => "running",
            DaemonState::Removed => "removed",
            DaemonState::Rebootstrapping => "rebootstrapping",
        })
    }
}

/// What a deploy sets on a new manifest: the serializable form of
/// `mesh_services::DeployOpts` (which is not serde). `Default` is the spec defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployOpts {
    pub limits: Limits,
    pub placement: Placement,
    pub outbound: Vec<String>,
}

impl Default for DeployOpts {
    fn default() -> Self {
        DeployOpts {
            limits: Limits::default(),
            placement: Placement::AllHosts,
            outbound: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminRequest {
    Status,
    Members,
    Services,
    Init {
        name: String,
    },
    Join {
        ticket: String,
        name: String,
    },
    ResetIdentity {
        confirm: bool,
    },
    Invite {
        role: Role,
        ttl_secs: u32,
    },
    Remove {
        id: NodeId,
    },
    SetInfo {
        name: Option<String>,
        tags: Option<Vec<String>>,
    },
    SetTagsOf {
        id: NodeId,
        tags: Vec<String>,
    },
    DeployWasm {
        name: String,
        wasm: Vec<u8>,
        opts: DeployOpts,
    },
    /// Followed by one [`FileData`] frame per entry of `files`, in order.
    DeployStatic {
        name: String,
        opts: DeployOpts,
        files: Vec<FileMeta>,
    },
    Undeploy {
        name: String,
    },
    Purge {
        name: String,
    },
    SetPlacement {
        name: String,
        placement: Placement,
    },
    SetOutbound {
        name: String,
        outbound: Vec<String>,
    },
    SetLimits {
        name: String,
        limits: Limits,
    },
    ConfigGet {
        key: Option<String>,
    },
    ConfigSet {
        key: String,
        value: String,
    },
    /// Answered by `LogLines` frames, then `Ok` (or `Error`).
    LogsTail {
        lines: u32,
        follow: bool,
    },
    /// Sends `count` `_ping` requests to the member whose id starts with `node`, one every
    /// `interval_ms`. Answered by one `LogLines` frame per sample (`seq=<n> ms=<rtt>` or
    /// `seq=<n> error=<reason>`), then `Ok` (or `Error`). Only while running.
    Probe {
        node: String,
        count: u32,
        interval_ms: u32,
    },
}

/// One file of a static upload: a relative `/`-separated path and its size in bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub path: String,
    pub size: u64,
}

/// The contents of one uploaded file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileData {
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminResponse {
    Status(StatusInfo),
    Members(Vec<MemberInfoView>),
    Services(Vec<ServiceInfoView>),
    Ticket { text: String },
    Manifest(ServiceManifest),
    Config(Vec<(String, String)>),
    ConfigSet { restart_required: bool },
    LogLines(Vec<String>),
    Ok,
    Error { code: ErrorCode, message: String },
}

impl AdminResponse {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        AdminResponse::Error {
            code,
            message: message.into(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    NotRunning,
    WrongState,
    InvalidArgument,
    NotFound,
    JoinRefused(RefuseReason),
    LastHost,
    Busy,
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorCode::NotRunning => f.write_str("the node is not running"),
            ErrorCode::WrongState => f.write_str("not possible in the daemon's current state"),
            ErrorCode::InvalidArgument => f.write_str("invalid argument"),
            ErrorCode::NotFound => f.write_str("not found"),
            ErrorCode::JoinRefused(r) => write!(f, "join refused: {}", refuse_text(*r)),
            ErrorCode::LastHost => f.write_str("the mesh's last host cannot be removed"),
            ErrorCode::Busy => f.write_str("the daemon is busy, try again"),
            ErrorCode::Internal => f.write_str("internal error in the daemon"),
        }
    }
}

fn refuse_text(r: RefuseReason) -> &'static str {
    match r {
        RefuseReason::Expired => "the invite has expired",
        RefuseReason::Used => "the invite was already used",
        RefuseReason::Unknown => "the invite is not known to the inviter",
        RefuseReason::WrongMesh => "the invite belongs to another mesh",
        RefuseReason::Removed => "this node was removed from the mesh",
        RefuseReason::InviterUnavailable => "the inviting node could not be reached",
    }
}

/// `meshd status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub state: DaemonState,
    pub daemon_version: String,
    pub data_dir: String,
    /// `None` while uninitialised.
    pub node_id: Option<NodeId>,
    pub mesh_id: Option<MeshId>,
    pub name: Option<String>,
    pub role: Option<Role>,
    pub tags: Vec<String>,
    pub members_total: u32,
    pub members_online: u32,
    pub services: u32,
    /// The gateway's listen address, when it runs.
    pub gateway: Option<String>,
    /// The gateway's mapped ports that are listening, with the service each serves.
    pub ports: Vec<(u16, String)>,
}

/// One row of `meshd members`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfoView {
    pub id: NodeId,
    pub name: String,
    pub role: Role,
    pub platform: String,
    pub tags: Vec<String>,
    pub is_self: bool,
    pub effective: bool,
    pub online: bool,
    pub rtt_ms: Option<u64>,
    pub last_seen_ms: Option<u64>,
    pub lag: u64,
}

/// One row of `meshd services`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfoView {
    pub name: String,
    pub kind: ServiceKindView,
    pub version: u64,
    pub placement: Placement,
    pub placed_hosts: Vec<NodeId>,
    pub local: LocalStateView,
    pub limits: Limits,
    pub outbound: Vec<String>,
    pub data_bytes: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceKindView {
    Wasm,
    Static,
}

/// The service on the daemon's node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalStateView {
    NotPlaced,
    Fetching,
    Ready { version: u64 },
    Failed { error: String },
}

/// A static-upload path: relative and `/`-separated, with no empty, `.` or `..` segments,
/// no backslash, NUL or `:` (drives, NTFS streams), not absolute — the rules of
/// mesh-core's blob paths.
pub fn valid_upload_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|seg| {
            !seg.is_empty() && seg != "." && seg != ".." && !seg.contains(['\\', '\0', ':'])
        })
}

/// Checks a `DeployStatic` file list: at most 100 000 files with valid, distinct paths,
/// each ≤ 64 MiB and together ≤ 1 GiB. The error is a readable message.
pub fn validate_upload(files: &[FileMeta]) -> Result<(), String> {
    if files.len() > MAX_UPLOAD_FILES {
        return Err(format!("an upload has at most {MAX_UPLOAD_FILES} files"));
    }
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for f in files {
        if !valid_upload_path(&f.path) {
            return Err(format!("invalid file path {:?}", f.path));
        }
        if !seen.insert(f.path.as_str()) {
            return Err(format!("file {:?} is listed twice", f.path));
        }
        if f.size > MAX_UPLOAD_FILE {
            return Err(format!("file {:?} is larger than 64 MiB", f.path));
        }
        total = total.saturating_add(f.size);
    }
    if total > MAX_UPLOAD_TOTAL {
        return Err("the upload is larger than 1 GiB".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_paths() {
        for ok in [
            "index.html",
            "a/b/c.txt",
            "a b/c%2e",
            ".well-known/x",
            "a..b",
        ] {
            assert!(valid_upload_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/x",
            "../x",
            "a/../x",
            "./x",
            "a/./x",
            "a//b",
            "a/",
            "a\\b",
            "a\0b",
            "C:",
            "c:/x",
            "..",
            "a/b:c",
            "a:b/c",
            "x/s::$DATA",
        ] {
            assert!(!valid_upload_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn upload_limits() {
        let f = |p: &str, size| FileMeta {
            path: p.into(),
            size,
        };
        assert!(validate_upload(&[f("a", MAX_UPLOAD_FILE), f("b", 0)]).is_ok());
        assert!(validate_upload(&[f("a", MAX_UPLOAD_FILE + 1)]).is_err());
        assert!(validate_upload(&[f("a", 1), f("a", 1)]).is_err());
        let mut full: Vec<_> = (0..16)
            .map(|i| f(&format!("f{i}"), MAX_UPLOAD_FILE))
            .collect();
        assert!(validate_upload(&full).is_ok());
        full.push(f("one-more", 1));
        assert!(validate_upload(&full).is_err());
        let many: Vec<_> = (0..=MAX_UPLOAD_FILES)
            .map(|i| f(&format!("f{i}"), 0))
            .collect();
        assert!(validate_upload(&many[..MAX_UPLOAD_FILES]).is_ok());
        assert!(validate_upload(&many).is_err());
    }

    #[test]
    fn deploy_opts_default_is_spec_default() {
        let o = DeployOpts::default();
        assert_eq!(o.limits, Limits::default());
        assert_eq!(o.placement, Placement::AllHosts);
        assert!(o.outbound.is_empty());
    }
}
