//! [`MeshClient`]: join or open, request, inspect, invite, remove, shut down.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode, header};
use mesh_core::local::{read_genesis, read_own_role};
use mesh_core::{Node, NodeConfig};
use mesh_forward::{ForwardError, RemoteForwarder, placed_hosts};
use mesh_proto::{InviteTicket, NodeId, Role, ServiceKind};
use tokio::task::JoinHandle;

use crate::error::ClientError;
use crate::events::{self, ClientEvent, Receivers, Shared};

/// The node identity file a joined directory holds.
const KEY_FILE: &str = "node.key";
/// One client per data directory: held (an exclusive OS file lock) while it is open.
const LOCK_FILE: &str = "client.lock";
/// Largest request body sent; a larger one gets a local `413`.
const MAX_BODY: usize = 16 * 1024 * 1024;
/// Added to the service's own `request_timeout_ms` for each attempt (as the gateway does).
const ATTEMPT_SLACK: Duration = Duration::from_secs(5);

/// How a client presents itself, and how long a request may take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientOptions {
    /// The member name others see.
    pub name: String,
    /// The platform others see (`"android"`, `"windows"`, ...).
    pub platform: String,
    /// Bound on a whole [`MeshClient::request`] call, every attempt included (30 s by
    /// default); past it the call fails with [`ClientError::Timeout`].
    pub request_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            name: "mesh-client".into(),
            platform: std::env::consts::OS.into(),
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// A snapshot of the client's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientStatus {
    /// The mesh id, in hex.
    pub mesh: String,
    /// This client's node id, in hex.
    pub id: String,
    /// Hosts this client has a live session with.
    pub online_hosts: usize,
    /// A sync round has completed since this client started.
    pub first_sync_done: bool,
    /// This client has been removed from the mesh.
    pub removed: bool,
}

/// A mesh member, as this client sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: NodeId,
    /// Empty until the member's info has replicated.
    pub name: String,
    pub role: Role,
    /// Empty until the member's info has replicated.
    pub platform: String,
    /// A session runs with it (always true for this client itself).
    pub online: bool,
    /// Recent round-trip time, when known.
    pub rtt_ms: Option<u64>,
}

/// A deployed service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceInfo {
    pub name: String,
    /// `"wasm"` or `"static"`.
    pub kind: String,
    /// Node ids (hex) of the hosts its placement selects, online or not; empty when no
    /// host runs it.
    pub hosts: Vec<String>,
}

/// A client-role member of a mesh. See the crate docs.
///
/// Call [`MeshClient::shutdown`] to stop it. Dropping it stops it too, in the background
/// (on the Tokio runtime it is dropped in): the directory stays locked until the node has
/// let go of it, so reopening it meanwhile is refused with [`ClientError::AlreadyOpen`].
/// Only `shutdown` guarantees an immediate reopen.
pub struct MeshClient {
    /// `None` only once shut down or dropped.
    owned: Option<Owned>,
    shared: Arc<Shared>,
    request_timeout: Duration,
}

/// What holds the data directory, torn down in order.
struct Owned {
    node: Node,
    fwd: RemoteForwarder,
    task: JoinHandle<()>,
    /// Released last, once the node has let go of the directory.
    lock: File,
}

impl Owned {
    async fn teardown(self) {
        let Owned {
            node,
            fwd,
            task,
            lock,
        } = self;
        task.abort();
        let _ = task.await;
        // Its cached connections and node handle.
        drop(fwd);
        node.shutdown().await;
        drop(lock);
    }
}

impl Drop for MeshClient {
    fn drop(&mut self) {
        let Some(owned) = self.owned.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(owned.teardown());
            }
            // No runtime to finish on: the node only cancels its tasks, and the lock is
            // released at once.
            Err(_) => owned.task.abort(),
        }
    }
}

/// How the client's node reaches the network.
#[derive(Clone, Copy)]
enum Net {
    /// Relays, discovery, every interface: the real thing.
    Normal,
    /// Loopback only, no relay or discovery, fast timings (tests).
    #[cfg(feature = "test-hooks")]
    LoopbackForTest,
}

impl MeshClient {
    /// Joins the mesh of a client invite `ticket`, keeping the new identity in `dir`
    /// (created if needed). Fails with [`ClientError::InvalidTicket`] for a ticket that is
    /// unreadable or not for the client role (before touching `dir`), or that the inviter
    /// refuses; [`ClientError::AlreadyJoined`] if `dir` holds a joined node (a `node.key`
    /// alone, left by an interrupted or failed join, is reused: retrying with the same
    /// ticket is how such a join completes);
    /// [`ClientError::AlreadyOpen`] if another client has `dir` open;
    /// [`ClientError::NoHost`] if the inviter cannot be reached.
    ///
    /// Returns once joined; the first sync completes in the background
    /// ([`ClientStatus::first_sync_done`]).
    pub async fn join(
        dir: &Path,
        ticket: &str,
        opts: ClientOptions,
    ) -> Result<MeshClient, ClientError> {
        Self::join_with(dir, ticket, opts, Net::Normal).await
    }

    /// Opens the client joined earlier in `dir`: [`ClientError::NotJoined`] if there is
    /// none (a host's data directory included), [`ClientError::AlreadyOpen`] if another
    /// client has it open.
    ///
    /// A client re-invited as a host while running (role upgrade) is out of scope for
    /// sub-spec 8a; once its role has changed, `open` refuses its directory.
    pub async fn open(dir: &Path, opts: ClientOptions) -> Result<MeshClient, ClientError> {
        Self::open_with(dir, opts, Net::Normal).await
    }

    /// [`MeshClient::join`] on loopback only, without relay or discovery, with fast
    /// timings (in-process tests).
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub async fn join_for_test(
        dir: &Path,
        ticket: &str,
        opts: ClientOptions,
    ) -> Result<MeshClient, ClientError> {
        Self::join_with(dir, ticket, opts, Net::LoopbackForTest).await
    }

    /// [`MeshClient::open`] as [`MeshClient::join_for_test`] joins.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub async fn open_for_test(dir: &Path, opts: ClientOptions) -> Result<MeshClient, ClientError> {
        Self::open_with(dir, opts, Net::LoopbackForTest).await
    }

    async fn join_with(
        dir: &Path,
        ticket: &str,
        opts: ClientOptions,
        net: Net,
    ) -> Result<MeshClient, ClientError> {
        let ticket = InviteTicket::from_text(ticket).map_err(|_| ClientError::InvalidTicket)?;
        if ticket.role != Role::Client {
            return Err(ClientError::InvalidTicket);
        }
        std::fs::create_dir_all(dir).map_err(ClientError::io)?;
        let lock = lock_dir(dir)?;
        // Joined means `mesh.json`. A failed join leaves only `node.key`, which the next
        // join reuses: the inviter accepts a retry from the same key (ruling R15).
        if read_genesis(dir).map_err(ClientError::node)?.is_some() {
            return Err(ClientError::AlreadyJoined);
        }
        let node = Node::join(node_config(dir, &opts, net), ticket)
            .await
            .map_err(ClientError::join)?;
        Ok(Self::start(node, &opts, lock).await)
    }

    async fn open_with(
        dir: &Path,
        opts: ClientOptions,
        net: Net,
    ) -> Result<MeshClient, ClientError> {
        let joined = || {
            dir.join(KEY_FILE).is_file()
                && matches!(read_genesis(dir), Ok(Some(_)))
                && matches!(read_own_role(dir), Ok(Some(Role::Client)))
        };
        if !joined() {
            return Err(ClientError::NotJoined);
        }
        let lock = lock_dir(dir)?;
        let node = Node::open(node_config(dir, &opts, net))
            .await
            .map_err(ClientError::node)?;
        Ok(Self::start(node, &opts, lock).await)
    }

    async fn start(node: Node, opts: &ClientOptions, lock: File) -> MeshClient {
        let handle = node.handle();
        let shared = Arc::new(Shared::new());
        let rx = Receivers::new(&handle);
        shared.refresh_manifests(&handle).await;
        let task = tokio::spawn(events::run(handle.clone(), shared.clone(), rx));
        MeshClient {
            owned: Some(Owned {
                fwd: RemoteForwarder::new(handle),
                node,
                task,
                lock,
            }),
            shared,
            request_timeout: opts.request_timeout,
        }
    }

    /// Sends `req` to `service` on the nearest online host that runs it, trying the next
    /// on failure (up to three hosts), within [`ClientOptions::request_timeout`]. The
    /// method, path and query, headers (hop-by-hop ones dropped) and body are forwarded;
    /// the URI's scheme and authority are not.
    ///
    /// The host's response is returned as it is, `4xx` and `5xx` included. Answered
    /// locally: `400` for a request whose path does not start with `/`, `413` for a body
    /// over 16 MiB, and `431`/`414` for headers or a path too large to forward.
    pub async fn request(
        &self,
        service: &str,
        req: Request<Bytes>,
    ) -> Result<Response<Bytes>, ClientError> {
        self.check_removed()?;
        let path_ok = req
            .uri()
            .path_and_query()
            .is_some_and(|p| p.as_str().starts_with('/'));
        if !path_ok {
            return Ok(local(StatusCode::BAD_REQUEST));
        }
        if req.body().len() > MAX_BODY {
            return Ok(local(StatusCode::PAYLOAD_TOO_LARGE));
        }
        let sent = self.fwd().forward(service, req, ATTEMPT_SLACK);
        match tokio::time::timeout(self.request_timeout, sent).await {
            Err(_) => Err(ClientError::Timeout),
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(ForwardError::Unframeable(e))) => Ok(local(e.status())),
            Ok(Err(ForwardError::Timeout(_))) => Err(ClientError::Timeout),
            Ok(Err(ForwardError::NoHost | ForwardError::NotDeployed)) => {
                // Removal shows up as no host: say which.
                self.check_removed()?;
                Err(ClientError::NoHost)
            }
            Ok(Err(e)) => Err(ClientError::Internal(e.to_string())),
        }
    }

    pub fn status(&self) -> ClientStatus {
        let handle = self.node().handle();
        ClientStatus {
            mesh: hex::encode(self.node().mesh().0),
            id: hex::encode(self.node().node_id().0),
            online_hosts: events::online_hosts(&handle),
            first_sync_done: self.node().first_sync_complete(),
            removed: self.shared.is_removed(&handle),
        }
    }

    /// Every member, this client included.
    pub fn members(&self) -> Vec<Member> {
        self.node()
            .members()
            .into_iter()
            .map(|m| {
                let (name, platform) = m.info.map(|i| (i.name, i.platform)).unwrap_or_default();
                Member {
                    id: m.id,
                    name,
                    role: m.role,
                    platform,
                    online: m.online,
                    rtt_ms: m.rtt_ms,
                }
            })
            .collect()
    }

    /// Every deployed service, by name, with the hosts its placement selects.
    pub fn services(&self) -> Vec<ServiceInfo> {
        let handle = self.node().handle();
        self.shared
            .manifests()
            .iter()
            .map(|m| ServiceInfo {
                name: m.name.clone(),
                kind: match m.kind {
                    ServiceKind::Wasm { .. } => "wasm",
                    ServiceKind::Static { .. } => "static",
                }
                .into(),
                hosts: placed_hosts(m, &handle)
                    .into_iter()
                    .map(|id| hex::encode(id.0))
                    .collect(),
            })
            .collect()
    }

    /// Membership, service, removal and online/offline events from now on.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ClientEvent> {
        self.shared.subscribe()
    }

    /// Issues a single-use invite ticket for `role`, valid for `ttl` (any member may
    /// invite). The new member joins through this client, so it must be running then.
    pub async fn invite(&self, role: Role, ttl: Duration) -> Result<String, ClientError> {
        self.check_removed()?;
        let ticket = self
            .node()
            .invite(role, ttl)
            .await
            .map_err(ClientError::node)?;
        Ok(ticket.to_text())
    }

    /// Removes member `id` from the mesh (any member may; the last host cannot be).
    pub async fn remove(&self, id: NodeId) -> Result<(), ClientError> {
        self.check_removed()?;
        self.node().remove(id).await.map_err(ClientError::node)
    }

    /// Stops the client and releases its directory, which can be opened again once this
    /// returns.
    pub async fn shutdown(mut self) {
        if let Some(owned) = self.owned.take() {
            owned.teardown().await;
        }
    }

    fn owned(&self) -> &Owned {
        // Taken only by `shutdown(self)` and `drop`, after which no method runs.
        self.owned.as_ref().expect("client not shut down")
    }

    fn node(&self) -> &Node {
        &self.owned().node
    }

    fn fwd(&self) -> &RemoteForwarder {
        &self.owned().fwd
    }

    /// Tries the hosts among `ids` first, in this order, instead of by RTT.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn try_first(&self, ids: Vec<NodeId>) {
        self.fwd().try_first(ids);
    }

    /// Forward attempts made so far.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn attempts(&self) -> u64 {
        self.fwd().attempts()
    }

    fn check_removed(&self) -> Result<(), ClientError> {
        if self.shared.is_removed(&self.node().handle()) {
            Err(ClientError::Removed)
        } else {
            Ok(())
        }
    }
}

/// A client node: never a gateway, never `httpfwd`, never services.
fn node_config(dir: &Path, opts: &ClientOptions, net: Net) -> NodeConfig {
    let mut cfg = NodeConfig::new(dir, opts.name.clone());
    cfg.platform = opts.platform.clone();
    match net {
        Net::Normal => {}
        #[cfg(feature = "test-hooks")]
        Net::LoopbackForTest => {
            cfg.relay = mesh_core::RelayChoice::Disabled;
            cfg.discovery = false;
            cfg.bind_addr = Some(std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
            cfg.timings = mesh_core::Timings::fast_for_tests();
        }
    }
    cfg
}

/// Takes `<dir>/client.lock` without waiting.
fn lock_dir(dir: &Path) -> Result<File, ClientError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .map_err(ClientError::io)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(ClientError::AlreadyOpen),
        Err(TryLockError::Error(e)) => Err(ClientError::io(e)),
    }
}

/// A response made here, not by a host.
fn local(code: StatusCode) -> Response<Bytes> {
    let mut resp = Response::new(Bytes::from(code.canonical_reason().unwrap_or("")));
    *resp.status_mut() = code;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}
