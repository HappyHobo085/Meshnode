//! Sync protocol messages.

use crate::ids::{MeshId, NodeId};
use crate::op::SignedOp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_BATCH_OPS: usize = 256;
pub const MAX_BATCH_BYTES: usize = 1_048_576;

#[derive(Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct VersionVector(pub BTreeMap<NodeId, u64>);

impl VersionVector {
    pub fn get(&self, id: &NodeId) -> u64 {
        self.0.get(id).copied().unwrap_or(0)
    }

    pub fn set(&mut self, id: NodeId, seq: u64) {
        self.0.insert(id, seq);
    }
}

/// Which namespaces a sync peer replicates.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum Scope {
    /// Everything (a full node).
    Full,
    /// Only the listed namespaces.
    Partial(Vec<String>),
}

impl Scope {
    /// The scope of a mesh client: only the `_mesh` namespace.
    pub fn client() -> Scope {
        Scope::Partial(vec!["_mesh".to_string()])
    }

    /// Whether namespace `ns` is replicated under this scope.
    pub fn admits(&self, ns: &str) -> bool {
        match self {
            Scope::Full => true,
            Scope::Partial(list) => list.iter().any(|n| n == ns),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum SyncMsg {
    Hello {
        mesh: MeshId,
        vv: VersionVector,
        scope: Scope,
    },
    Ops {
        ops: Vec<SignedOp>,
    },
    Covered {
        origin: NodeId,
        upto: u64,
    },
    Done,
}

/// Splits ops into `SyncMsg::Ops` batches of at most 256 ops and 1 MiB (sum of encoded op sizes).
/// A single op larger than 1 MiB still gets its own batch.
pub fn batch_ops(ops: Vec<SignedOp>) -> Vec<SyncMsg> {
    let mut out = Vec::new();
    let mut cur: Vec<SignedOp> = Vec::new();
    let mut cur_bytes = 0usize;
    for op in ops {
        let size = crate::encode(&op).len();
        if !cur.is_empty() && (cur.len() >= MAX_BATCH_OPS || cur_bytes + size > MAX_BATCH_BYTES) {
            out.push(SyncMsg::Ops {
                ops: std::mem::take(&mut cur),
            });
            cur_bytes = 0;
        }
        cur_bytes += size;
        cur.push(op);
    }
    if !cur.is_empty() {
        out.push(SyncMsg::Ops { ops: cur });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{OpBody, Payload};
    use ed25519_dalek::SigningKey;
    use mesh_hlc::Hlc;

    fn op(k: &SigningKey, seq: u64, len: usize) -> SignedOp {
        SignedOp::sign(
            OpBody {
                mesh: MeshId([7; 32]),
                origin: NodeId(k.verifying_key().to_bytes()),
                seq,
                hlc: Hlc::new(1, 0),
                payload: Payload::Set {
                    ns: "n".into(),
                    key: "k".into(),
                    order: vec![],
                    value: Some(vec![0; len]),
                },
            },
            k,
        )
    }

    #[test]
    fn hello_with_scope_roundtrips() {
        for scope in [Scope::Full, Scope::client()] {
            let m = SyncMsg::Hello {
                mesh: MeshId([7; 32]),
                vv: VersionVector::default(),
                scope,
            };
            assert_eq!(crate::decode::<SyncMsg>(&crate::encode(&m)).unwrap(), m);
        }
    }

    #[test]
    fn scope_admits() {
        assert!(Scope::Full.admits("anything"));
        assert!(Scope::client().admits("_mesh"));
        assert!(!Scope::client().admits("other"));
    }

    fn sizes(v: &[SyncMsg]) -> Vec<usize> {
        v.iter()
            .map(|m| match m {
                SyncMsg::Ops { ops } => ops.len(),
                _ => 0,
            })
            .collect()
    }

    #[test]
    fn batch_ops_respects_count_and_size() {
        let k = SigningKey::from_bytes(&[1; 32]);
        let ops: Vec<_> = (1..=600).map(|i| op(&k, i, 10)).collect();
        assert_eq!(sizes(&batch_ops(ops)), vec![256, 256, 88]);
        let big: Vec<_> = (1..=3).map(|i| op(&k, i, 600 * 1024)).collect();
        assert_eq!(sizes(&batch_ops(big)), vec![1, 1, 1]);
        assert!(batch_ops(vec![]).is_empty());
    }

    #[test]
    fn oversized_single_op_gets_own_batch() {
        let k = SigningKey::from_bytes(&[1; 32]);
        let ops = vec![op(&k, 1, 10), op(&k, 2, 1_048_576), op(&k, 3, 10)];
        assert_eq!(sizes(&batch_ops(ops)), vec![1, 1, 1]);
    }

    #[test]
    fn vv_get_set() {
        let mut vv = VersionVector::default();
        let n = NodeId([1; 32]);
        assert_eq!(vv.get(&n), 0);
        vv.set(n, 5);
        assert_eq!(vv.get(&n), 5);
    }
}
