//! iroh endpoint construction, ALPNs, close codes, id/address conversion and the membership gate.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use iroh::endpoint::{Connection, VarInt, presets};
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use mesh_proto::{NodeAddrBytes, NodeId, ProtoError};
use tokio::sync::watch;
use tracing::info;

use crate::config::{NodeConfig, RelayChoice};
use crate::error::NodeError;
use crate::membership::MembershipSnapshot;

pub const ALPN_SYNC: &[u8] = b"meshnode/sync/1";
pub const ALPN_JOIN: &[u8] = b"meshnode/join/1";
/// The version probe (frozen, unversioned; see [`crate::version`]).
pub use mesh_proto::ALPN_VERSION;

pub const NOT_MEMBER: VarInt = VarInt::from_u32(1);
pub const REMOVED: VarInt = VarInt::from_u32(2);
pub const PROTOCOL: VarInt = VarInt::from_u32(3);
pub const SHUTDOWN: VarInt = VarInt::from_u32(4);

pub fn node_id_of(id: EndpointId) -> NodeId {
    NodeId(*id.as_bytes())
}

pub fn endpoint_id_of(id: &NodeId) -> Result<EndpointId, NodeError> {
    EndpointId::from_bytes(&id.0).map_err(|e| NodeError::Iroh(e.to_string()))
}

pub fn addr_to_bytes(addr: &EndpointAddr) -> NodeAddrBytes {
    // Serializing an address into a Vec cannot fail; an empty value decodes to an error anyway.
    NodeAddrBytes(postcard::to_allocvec(addr).unwrap_or_default())
}

pub fn addr_from_bytes(b: &NodeAddrBytes) -> Result<EndpointAddr, ProtoError> {
    postcard::from_bytes(&b.0).map_err(|e| ProtoError::Decode(e.to_string()))
}

/// Binds the node's endpoint. The caller registers ALPNs through the `Router`.
pub async fn build_endpoint(cfg: &NodeConfig, key: &SigningKey) -> Result<Endpoint, NodeError> {
    let iroh_err = |e: &dyn std::fmt::Display| NodeError::Iroh(e.to_string());
    let mut b = Endpoint::builder(presets::N0).secret_key(SecretKey::from_bytes(&key.to_bytes()));
    b = match &cfg.relay {
        RelayChoice::Default => b,
        RelayChoice::Disabled => b.relay_mode(RelayMode::Disabled),
        RelayChoice::Custom(url) => {
            let url: RelayUrl = url.parse().map_err(|e| iroh_err(&e))?;
            b.relay_mode(RelayMode::custom([url]))
        }
    };
    if !cfg.discovery {
        b = b.clear_address_lookup();
    }
    if let Some(addr) = cfg.bind_addr {
        b = b.bind_addr(addr).map_err(|e| iroh_err(&e))?;
    }
    b.bind().await.map_err(|e| iroh_err(&e))
}

/// Peers a joiner admits and dials before its membership lists them: the inviter and the
/// peers its join reply listed (spec §5.3 step 5). The joiner's store starts empty, so its
/// snapshot knows only the founder; these peers are admitted until the first sync completes,
/// after which the snapshot is complete and alone decides. Shared by the `sync/1` gate and
/// the peer manager.
#[derive(Debug, Default, Clone)]
pub(crate) struct Provisional(Arc<Mutex<ProvisionalState>>);

#[derive(Debug, Default)]
struct ProvisionalState {
    ids: HashSet<NodeId>,
    /// The first sync completed: nothing is admitted provisionally any more.
    over: bool,
}

impl Provisional {
    fn lock(&self) -> std::sync::MutexGuard<'_, ProvisionalState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admits `ids` until [`Provisional::end`]; returns false (and admits nothing) once it
    /// has ended.
    pub fn add(&self, ids: &[NodeId]) -> bool {
        let mut st = self.lock();
        if !st.over {
            st.ids.extend(ids.iter().copied());
        }
        !st.over
    }

    pub fn contains(&self, id: &NodeId) -> bool {
        self.lock().ids.contains(id)
    }

    /// The first sync completed: admits nothing more, for good.
    pub fn end(&self) {
        let mut st = self.lock();
        st.over = true;
        st.ids.clear();
    }
}

/// Connections of extra protocols, inbound and outbound. A connection whose peer stops
/// being an effective member is closed with `REMOVED` (like the `sync/1` connections).
#[derive(Debug)]
pub(crate) struct ExtraConns {
    conns: Mutex<Vec<Connection>>,
    membership: watch::Receiver<Arc<MembershipSnapshot>>,
}

impl ExtraConns {
    pub fn new(membership: watch::Receiver<Arc<MembershipSnapshot>>) -> Arc<Self> {
        Arc::new(ExtraConns {
            conns: Mutex::new(Vec::new()),
            membership,
        })
    }

    /// Tracks `conn` and closes it at once if its peer is no longer effective.
    pub fn track(&self, conn: Connection) {
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(conn);
        self.sweep();
    }

    /// Closes the connections of non-effective peers and forgets closed ones.
    pub fn sweep(&self) {
        let snap = self.membership.borrow().clone();
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|c| {
            if c.close_reason().is_some() {
                return false;
            }
            if snap.is_effective(&node_id_of(c.remote_id())) {
                return true;
            }
            c.close(REMOVED, b"removed");
            false
        });
    }

    /// Sweeps on every membership change until `cancel` fires.
    pub async fn watch(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        let mut rx = self.membership.clone();
        loop {
            self.sweep();
            tokio::select! {
                () = cancel.cancelled() => return,
                r = rx.changed() => if r.is_err() { return },
            }
        }
    }
}

/// Adapts a shared dyn handler (from `NodeConfig::extra_protocols`) so it can sit behind a
/// [`Gate`]; accepted connections are tracked for removal.
#[derive(Debug, Clone)]
pub(crate) struct Extra {
    pub handler: Arc<dyn DynProtocolHandler>,
    pub conns: Arc<ExtraConns>,
}

impl ProtocolHandler for Extra {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        self.conns.track(conn.clone());
        self.handler.accept(conn).await
    }

    async fn shutdown(&self) {
        self.handler.shutdown().await
    }
}

/// Refuses connections from non-members and removed nodes before the inner handler sees them.
#[derive(Debug)]
pub struct Gate<H> {
    inner: H,
    membership: watch::Receiver<Arc<MembershipSnapshot>>,
    provisional: Option<Provisional>,
}

impl<H> Gate<H> {
    pub fn new(inner: H, membership: watch::Receiver<Arc<MembershipSnapshot>>) -> Self {
        Gate {
            inner,
            membership,
            provisional: None,
        }
    }

    /// Also admits the provisional peers of a joiner that are not members in its snapshot
    /// yet (they are not known to be removed either: a removed member is still refused).
    pub(crate) fn with_provisional(mut self, p: Provisional) -> Self {
        self.provisional = Some(p);
        self
    }
}

impl<H: ProtocolHandler> ProtocolHandler for Gate<H> {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = node_id_of(conn.remote_id());
        let snap = self.membership.borrow().clone();
        let provisional = || self.provisional.as_ref().is_some_and(|p| p.contains(&peer));
        if !snap.is_member(&peer) && !provisional() {
            info!(peer = %peer.short(), "refusing connection: not a member");
            conn.close(NOT_MEMBER, b"not a member");
            return Ok(());
        }
        if snap.is_member(&peer) && !snap.is_effective(&peer) {
            info!(peer = %peer.short(), "refusing connection: removed");
            conn.close(REMOVED, b"removed");
            return Ok(());
        }
        self.inner.accept(conn).await
    }

    async fn shutdown(&self) {
        self.inner.shutdown().await
    }
}
