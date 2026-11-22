//! The `meshnode/version` probe (8b spec §5.4): a member learns another member's product and
//! protocol versions without a `sync/1` session, so a peer on an incompatible version shows
//! as such instead of just offline.
//!
//! Wire: the dialer opens a bi-stream (sending nothing but its end) and the acceptor writes
//! its [`PeerVersion`] as JSON (at most [`VERSION_MAX_BYTES`]) and finishes; the dialer reads
//! it and closes. Mounted behind the membership [`crate::transport::Gate`] like `sync/1`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{
    ConnectError, ConnectingError, Connection, ConnectionError, TransportErrorCode,
};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use mesh_proto::{ALPN_HTTPFWD, ALPN_VERSION, PeerVersion, VERSION_MAX_BYTES, alpn_version};
use tokio::time::timeout;
use tracing::debug;

use crate::config::NodeConfig;
use crate::transport::{ALPN_JOIN, ALPN_SYNC, SHUTDOWN};

/// How long one probe exchange (after the dial) may take, on either side.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// The TLS alert `no_application_protocol` (RFC 8446 §6; rustls `AlertDescription` 0x78).
const TLS_NO_APPLICATION_PROTOCOL: u8 = 0x78;

/// This build's version, as [`PeerVersion::current`] (a trait: `PeerVersion` lives in
/// mesh-proto, which cannot see the store's schema).
pub trait CurrentVersion {
    /// `product` (from `NodeConfig::product_version`), the protocol versions of this build's
    /// ALPN constants and `mesh_store::SCHEMA_VERSION`.
    fn current(product: &str) -> Self;
}

impl CurrentVersion for PeerVersion {
    fn current(product: &str) -> PeerVersion {
        let protocols = [
            ("sync", ALPN_SYNC),
            ("join", ALPN_JOIN),
            ("httpfwd", ALPN_HTTPFWD),
        ]
        .into_iter()
        .map(|(name, alpn)| (name.to_owned(), alpn_version(alpn)))
        .collect::<BTreeMap<_, _>>();
        PeerVersion {
            product: product.to_owned(),
            protocols,
            store_schema: mesh_store::SCHEMA_VERSION,
        }
    }
}

/// What this node reports: [`PeerVersion::current`], or a test's override.
pub(crate) fn local_version(cfg: &NodeConfig) -> PeerVersion {
    #[allow(unused_mut)]
    let mut v = PeerVersion::current(&cfg.product_version);
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(p) = &cfg.protocols_override {
        v.protocols = p.clone();
    }
    v
}

/// The ALPN this node serves and dials sync on: `ALPN_SYNC`, or a test's override.
pub(crate) fn sync_alpn(cfg: &NodeConfig) -> Vec<u8> {
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(a) = &cfg.sync_alpn_override {
        return a.clone();
    }
    let _ = cfg;
    ALPN_SYNC.to_vec()
}

/// Whether a dial failed because the peer offers none of the ALPNs asked for: its TLS stack
/// refused the handshake with the `no_application_protocol` alert, which QUIC carries as the
/// transport error code `0x100 | alert` (RFC 9001 §4.8) in the peer's CONNECTION_CLOSE.
pub(crate) fn alpn_refused(e: &ConnectError) -> bool {
    let closed = match e {
        ConnectError::Connecting {
            source: ConnectingError::ConnectionError { source, .. },
            ..
        } => source,
        ConnectError::Connection { source, .. } => source,
        _ => return false,
    };
    matches!(
        closed,
        ConnectionError::ConnectionClosed(c)
            if c.error_code == TransportErrorCode::crypto(TLS_NO_APPLICATION_PROTOCOL)
    )
}

/// Asks the member at `addr` for its version.
pub(crate) async fn fetch(
    ep: &Endpoint,
    addr: EndpointAddr,
    dial: Duration,
) -> Result<PeerVersion, String> {
    let conn = match timeout(dial, ep.connect(addr, ALPN_VERSION)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("dial timed out".into()),
    };
    let talk = async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        // An empty, finished stream: the acceptor sees it and answers.
        send.finish().map_err(|e| e.to_string())?;
        let body = recv
            .read_to_end(VERSION_MAX_BYTES)
            .await
            .map_err(|e| e.to_string())?;
        PeerVersion::from_json(&body).map_err(|e| e.to_string())
    };
    let r = match timeout(EXCHANGE_TIMEOUT, talk).await {
        Ok(r) => r,
        Err(_) => Err("version exchange timed out".into()),
    };
    conn.close(SHUTDOWN, b"done");
    r
}

/// Serves this node's version to members (behind the gate).
#[derive(Debug, Clone)]
pub(crate) struct VersionHandler {
    json: Arc<Vec<u8>>,
}

impl VersionHandler {
    pub fn new(v: &PeerVersion) -> Self {
        VersionHandler {
            json: Arc::new(v.to_json()),
        }
    }

    async fn serve(&self, conn: &Connection) -> Result<(), String> {
        let (mut send, _recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
        send.write_all(&self.json)
            .await
            .map_err(|e| e.to_string())?;
        send.finish().map_err(|e| e.to_string())?;
        // The dialer closes once it has read the reply; dropping the connection earlier
        // could lose it.
        conn.closed().await;
        Ok(())
    }
}

impl ProtocolHandler for VersionHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        match timeout(EXCHANGE_TIMEOUT, self.serve(&conn)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => debug!(error = %e, "version probe not served"),
            Err(_) => debug!("version probe timed out"),
        }
        conn.close(SHUTDOWN, b"done");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_reports_the_alpn_versions_and_schema() {
        let v = PeerVersion::current("0.3.0");
        assert_eq!(v.product, "0.3.0");
        assert_eq!(
            v.protocols,
            BTreeMap::from([
                ("httpfwd".to_owned(), 1),
                ("join".to_owned(), 1),
                ("sync".to_owned(), 1),
            ])
        );
        assert_eq!(v.store_schema, mesh_store::SCHEMA_VERSION);
    }
}
