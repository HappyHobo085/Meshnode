//! iroh endpoint construction, ALPNs, close codes, id/address conversion and the membership gate.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use iroh::endpoint::{Connection, VarInt, presets};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use mesh_proto::{NodeAddrBytes, NodeId, ProtoError};
use tokio::sync::watch;
use tracing::info;

use crate::config::{NodeConfig, RelayChoice};
use crate::error::NodeError;
use crate::membership::MembershipSnapshot;

pub const ALPN_SYNC: &[u8] = b"meshnode/sync/1";
pub const ALPN_JOIN: &[u8] = b"meshnode/join/1";

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

/// Refuses connections from non-members and removed nodes before the inner handler sees them.
#[derive(Debug)]
pub struct Gate<H> {
    inner: H,
    membership: watch::Receiver<Arc<MembershipSnapshot>>,
}

impl<H> Gate<H> {
    pub fn new(inner: H, membership: watch::Receiver<Arc<MembershipSnapshot>>) -> Self {
        Gate { inner, membership }
    }
}

impl<H: ProtocolHandler> ProtocolHandler for Gate<H> {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = node_id_of(conn.remote_id());
        let snap = self.membership.borrow().clone();
        if !snap.is_member(&peer) {
            info!(peer = %peer.short(), "refusing connection: not a member");
            conn.close(NOT_MEMBER, b"not a member");
            return Ok(());
        }
        if !snap.is_effective(&peer) {
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
