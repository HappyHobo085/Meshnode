//! Node / mesh identifiers.

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
pub struct MeshId(pub [u8; 32]);

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// First 10 lowercase hex chars of the id.
    pub fn short(&self) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(10);
        for b in &self.0[..5] {
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

/// Opaque serialized transport address of a node.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
pub struct NodeAddrBytes(pub Vec<u8>);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub enum Role {
    Host,
    Client,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_is_10_hex_chars() {
        let mut b = [0u8; 32];
        b[0] = 0xab;
        b[4] = 0x0f;
        assert_eq!(NodeId(b).short(), "ab0000000f");
    }
}
