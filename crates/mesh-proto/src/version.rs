//! The `meshnode/version` probe's payload: what a node runs, readable by every version.
//!
//! The ALPN is deliberately unversioned and frozen; the payload is JSON so later versions can
//! add fields without breaking older readers (unknown fields are ignored).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ProtoError;

/// Frozen: never bumped. Future changes add JSON fields instead.
pub const ALPN_VERSION: &[u8] = b"meshnode/version";
/// The largest `PeerVersion` JSON a probe reads.
pub const VERSION_MAX_BYTES: usize = 4 * 1024;

/// A node's product version, the version of each wire protocol it speaks and its store schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerVersion {
    /// The product version (e.g. "0.1.0").
    pub product: String,
    /// Protocol name ("sync", "join", "httpfwd") to the `/N` of its ALPN.
    pub protocols: BTreeMap<String, u32>,
    /// The store's `SCHEMA_VERSION`.
    pub store_schema: u64,
}

impl PeerVersion {
    /// Whether two nodes can sync: the same `sync` protocol and the same store schema.
    pub fn compatible_with(&self, other: &PeerVersion) -> bool {
        self.protocols.get("sync") == other.protocols.get("sync")
            && self.store_schema == other.store_schema
    }

    pub fn to_json(&self) -> Vec<u8> {
        // Serializing a struct of strings and integers cannot fail.
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Decodes a probe reply, refusing one over [`VERSION_MAX_BYTES`].
    pub fn from_json(b: &[u8]) -> Result<PeerVersion, ProtoError> {
        if b.len() > VERSION_MAX_BYTES {
            return Err(ProtoError::Limit("version reply"));
        }
        serde_json::from_slice(b).map_err(|e| ProtoError::Decode(e.to_string()))
    }
}

/// The `/N` at the end of a versioned ALPN (`meshnode/sync/1` → 1); 0 if it has none.
pub fn alpn_version(alpn: &[u8]) -> u32 {
    alpn.rsplit(|&b| b == b'/')
        .next()
        .and_then(|n| std::str::from_utf8(n).ok())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(sync: u32, schema: u64) -> PeerVersion {
        PeerVersion {
            product: "0.1.0".into(),
            protocols: [("sync".to_owned(), sync), ("join".to_owned(), 1)].into(),
            store_schema: schema,
        }
    }

    #[test]
    fn peer_version_ignores_unknown_fields() {
        let json = br#"{"product":"0.2.0","protocols":{"sync":1,"join":1,"httpfwd":1},
            "store_schema":1,"future":1}"#;
        let p = PeerVersion::from_json(json).unwrap();
        assert_eq!(p.product, "0.2.0");
        assert_eq!(p.protocols["sync"], 1);
        assert_eq!(p.store_schema, 1);
        assert!(v(1, 1).compatible_with(&v(1, 1)));
        assert!(!v(1, 1).compatible_with(&v(2, 1)), "sync differs");
        assert!(!v(1, 1).compatible_with(&v(1, 2)), "store_schema differs");
        // Only sync and the schema decide: the product and other protocols do not.
        let mut other = v(1, 1);
        other.product = "9.9.9".into();
        other.protocols.insert("join".into(), 7);
        assert!(v(1, 1).compatible_with(&other));
    }

    #[test]
    fn version_json_roundtrips_and_is_bounded() {
        let p = v(2, 3);
        assert_eq!(PeerVersion::from_json(&p.to_json()).unwrap(), p);
        let big = vec![b' '; VERSION_MAX_BYTES + 1];
        assert!(matches!(
            PeerVersion::from_json(&big),
            Err(ProtoError::Limit(_))
        ));
        assert!(PeerVersion::from_json(b"not json").is_err());
    }

    #[test]
    fn alpn_version_reads_the_suffix() {
        assert_eq!(ALPN_VERSION, b"meshnode/version");
        assert_eq!(alpn_version(b"meshnode/sync/1"), 1);
        assert_eq!(alpn_version(b"meshnode/sync/2"), 2);
        assert_eq!(alpn_version(crate::ALPN_HTTPFWD), 1);
        assert_eq!(alpn_version(ALPN_VERSION), 0);
    }
}
