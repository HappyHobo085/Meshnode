//! The records, enums and error the bindings expose, and their conversions from
//! `mesh-client`'s types.

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use mesh_client::{
    ClientError, ClientEvent, ClientOptions, ClientStatus, Member, Role, ServiceInfo,
};

/// Why a [`crate::MeshClient`] call failed: `mesh_client::ClientError`, plus the
/// FFI's own `InvalidArgument`, `Closed` and `NotInitialised`. Kotlin gets the sealed
/// `MeshClientException` (`MeshClientException.InvalidTicket` etc.) whose `message` is
/// this error's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum MeshClientError {
    /// The ticket is not a valid client invite: unreadable, for another role, expired,
    /// already used, or refused by the inviter.
    #[error("not a valid client invite ticket")]
    InvalidTicket,
    /// `join` into a directory that already holds a joined node.
    #[error("this directory already holds a joined node")]
    AlreadyJoined,
    /// `open` on a directory that holds no joined client.
    #[error("this directory holds no joined client")]
    NotJoined,
    /// Another client has this directory open (or one dropped and still stopping).
    #[error("another client has this directory open")]
    AlreadyOpen,
    /// This client has been removed from the mesh.
    #[error("this client has been removed from the mesh")]
    Removed,
    /// No host could be reached (for `join`: the inviter).
    #[error("no host could serve the request")]
    NoHost,
    /// A host was reached but did not answer in time, or the request took longer than
    /// the options' `requestTimeoutMs`.
    #[error("the request timed out")]
    Timeout,
    #[error("i/o error: {message}")]
    Io { message: String },
    /// Anything else, a panic in the library included.
    #[error("internal error: {message}")]
    Internal { message: String },
    /// An argument that cannot be used (a node id that is not 64 hex digits).
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
    /// The client has been shut down (or is shutting down).
    #[error("the client has been shut down")]
    Closed,
    /// Android: `MeshClientAndroid.init(context)` has not run.
    #[error("the Android context is not initialised (call MeshClientAndroid.init first)")]
    NotInitialised,
}

impl From<ClientError> for MeshClientError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::InvalidTicket => MeshClientError::InvalidTicket,
            ClientError::AlreadyJoined => MeshClientError::AlreadyJoined,
            ClientError::NotJoined => MeshClientError::NotJoined,
            ClientError::AlreadyOpen => MeshClientError::AlreadyOpen,
            ClientError::Removed => MeshClientError::Removed,
            ClientError::NoHost => MeshClientError::NoHost,
            ClientError::Timeout => MeshClientError::Timeout,
            ClientError::Io(message) => MeshClientError::Io { message },
            ClientError::Internal(message) => MeshClientError::Internal { message },
        }
    }
}

/// How a client presents itself, and how long a request may take.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MeshClientOptions {
    /// The member name others see.
    #[uniffi(default = "mesh-client")]
    pub name: String,
    /// The platform others see; empty means this OS (`"android"` on Android).
    #[uniffi(default = "")]
    pub platform: String,
    /// Bound on a whole `request` call, every attempt included.
    #[uniffi(default = 30000)]
    pub request_timeout_ms: u64,
}

impl From<MeshClientOptions> for ClientOptions {
    fn from(o: MeshClientOptions) -> Self {
        let def = ClientOptions::default();
        ClientOptions {
            name: o.name,
            platform: if o.platform.is_empty() {
                def.platform
            } else {
                o.platform
            },
            request_timeout: std::time::Duration::from_millis(o.request_timeout_ms),
        }
    }
}

/// One HTTP header. A response header value that is not UTF-8 is converted lossily.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MeshHeader {
    pub name: String,
    pub value: String,
}

/// An HTTP request for a service. `path` is the path and query (`/a/b?c=d`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MeshRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<MeshHeader>,
    pub body: Vec<u8>,
}

/// A host's HTTP response (or one answered locally: `400`, `413`, `414`, `431`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MeshResponse {
    pub status: u16,
    pub headers: Vec<MeshHeader>,
    pub body: Vec<u8>,
}

/// A snapshot of the client's state.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MeshStatus {
    /// The mesh id, in hex.
    pub mesh: String,
    /// This client's node id, in hex.
    pub id: String,
    /// Hosts this client has a live session with.
    pub online_hosts: u32,
    /// A sync round has completed since this client started.
    pub first_sync_done: bool,
    /// This client has been removed from the mesh.
    pub removed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MeshRole {
    Host,
    Client,
}

/// A mesh member, as this client sees it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MeshMember {
    /// The node id, in hex (what `remove` takes).
    pub id: String,
    /// Empty until the member's info has replicated.
    pub name: String,
    pub role: MeshRole,
    /// Empty until the member's info has replicated.
    pub platform: String,
    /// A session runs with it (always true for this client itself).
    pub online: bool,
    pub rtt_ms: Option<u64>,
}

/// A deployed service.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MeshService {
    pub name: String,
    /// `"wasm"` or `"static"`.
    pub kind: String,
    /// Node ids (hex) of the hosts its placement selects, online or not.
    pub hosts: Vec<String>,
}

/// What a `MeshClientListener` is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MeshEvent {
    /// Read `members()` again.
    MembersChanged,
    /// Read `services()` again.
    ServicesChanged,
    /// This client was removed from the mesh; every call fails with `Removed` from now on.
    Removed,
    /// At least one host is online again.
    Online,
    /// No host is online any more.
    Offline,
}

impl From<ClientEvent> for MeshEvent {
    fn from(e: ClientEvent) -> Self {
        match e {
            ClientEvent::MembersChanged => MeshEvent::MembersChanged,
            ClientEvent::ServicesChanged => MeshEvent::ServicesChanged,
            ClientEvent::Removed => MeshEvent::Removed,
            ClientEvent::Online => MeshEvent::Online,
            ClientEvent::Offline => MeshEvent::Offline,
        }
    }
}

impl From<MeshRole> for Role {
    fn from(r: MeshRole) -> Self {
        match r {
            MeshRole::Host => Role::Host,
            MeshRole::Client => Role::Client,
        }
    }
}

impl From<Role> for MeshRole {
    fn from(r: Role) -> Self {
        match r {
            Role::Host => MeshRole::Host,
            Role::Client => MeshRole::Client,
        }
    }
}

impl From<ClientStatus> for MeshStatus {
    fn from(s: ClientStatus) -> Self {
        MeshStatus {
            mesh: s.mesh,
            id: s.id,
            online_hosts: u32::try_from(s.online_hosts).unwrap_or(u32::MAX),
            first_sync_done: s.first_sync_done,
            removed: s.removed,
        }
    }
}

impl From<Member> for MeshMember {
    fn from(m: Member) -> Self {
        MeshMember {
            id: hex::encode(m.id.0),
            name: m.name,
            role: m.role.into(),
            platform: m.platform,
            online: m.online,
            rtt_ms: m.rtt_ms,
        }
    }
}

impl From<ServiceInfo> for MeshService {
    fn from(s: ServiceInfo) -> Self {
        MeshService {
            name: s.name,
            kind: s.kind,
            hosts: s.hosts,
        }
    }
}

/// The request as `mesh-client` takes it; `None` if it cannot be built (a bad method,
/// header name or value, or path).
pub(crate) fn to_http(r: MeshRequest) -> Option<Request<Bytes>> {
    let mut req = Request::builder()
        .method(Method::from_bytes(r.method.as_bytes()).ok()?)
        .uri(r.path)
        .body(Bytes::from(r.body))
        .ok()?;
    for h in r.headers {
        let name = HeaderName::from_bytes(h.name.as_bytes()).ok()?;
        let value = HeaderValue::from_str(&h.value).ok()?;
        req.headers_mut().append(name, value);
    }
    Some(req)
}

pub(crate) fn from_http(resp: Response<Bytes>) -> MeshResponse {
    let (parts, body) = resp.into_parts();
    MeshResponse {
        status: parts.status.as_u16(),
        headers: parts
            .headers
            .iter()
            .map(|(n, v)| MeshHeader {
                name: n.as_str().to_owned(),
                value: String::from_utf8_lossy(v.as_bytes()).into_owned(),
            })
            .collect(),
        body: body.to_vec(),
    }
}

/// The local `400` for a request that cannot be built (as `mesh-client` answers a bad
/// path).
pub(crate) fn bad_request() -> MeshResponse {
    let code = StatusCode::BAD_REQUEST;
    MeshResponse {
        status: code.as_u16(),
        headers: vec![MeshHeader {
            name: "content-type".into(),
            value: "text/plain; charset=utf-8".into(),
        }],
        body: code.canonical_reason().unwrap_or("").as_bytes().to_vec(),
    }
}
