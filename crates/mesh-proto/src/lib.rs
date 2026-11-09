//! Wire/data types for the Meshnode mesh: ids, signed ops, sync messages, invite tickets.

mod ids;
mod join;
mod op;
mod sync;
mod ticket;

pub use ids::{MeshId, NodeAddrBytes, NodeId, Role};
pub use join::{JoinRequest, JoinResponse, MemberInfo, MeshGenesis, RefuseReason, RemovalRecord};
pub use op::{OpBody, Payload, SignedOp};
pub use sync::{MAX_BATCH_BYTES, MAX_BATCH_OPS, SyncMsg, VersionVector, batch_ops};
pub use ticket::InviteTicket;

use serde::{Serialize, de::DeserializeOwned};

/// Format version byte prefixed to every top-level encoded message.
pub const FORMAT_VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("unsupported format version {0}")]
    Version(u8),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("limit exceeded: {0}")]
    Limit(&'static str),
    #[error("NUL byte not allowed in {0}")]
    Nul(&'static str),
    #[error("bad signature")]
    BadSig,
    #[error("invalid ticket: {0}")]
    Ticket(&'static str),
}

/// Version byte followed by postcard.
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = vec![FORMAT_VERSION];
    // Serializing into a Vec only fails for types that cannot be serialized (a programming error);
    // in that case only the version byte is returned and decoding will fail.
    if let Ok(b) = postcard::to_allocvec(v) {
        out.extend_from_slice(&b);
    }
    out
}

/// Inverse of [`encode`]. Any `SignedOp` inside is validated (limits, no NUL).
pub fn decode<T: DeserializeOwned>(b: &[u8]) -> Result<T, ProtoError> {
    let (&ver, rest) = b
        .split_first()
        .ok_or_else(|| ProtoError::Decode("empty input".into()))?;
    if ver != FORMAT_VERSION {
        return Err(ProtoError::Version(ver));
    }
    let _ = op::take_stashed_error();
    match postcard::take_from_bytes::<T>(rest) {
        Ok((v, [])) => Ok(v),
        Ok(_) => Err(ProtoError::Decode("trailing bytes".into())),
        Err(e) => {
            Err(op::take_stashed_error().unwrap_or_else(|| ProtoError::Decode(e.to_string())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use mesh_hlc::Hlc;

    #[test]
    fn wrong_version_rejected() {
        let mut b = encode(&SyncMsg::Done);
        b[0] = 2;
        assert!(matches!(decode::<SyncMsg>(&b), Err(ProtoError::Version(2))));
    }

    #[test]
    fn empty_and_garbage_rejected() {
        assert!(matches!(decode::<SyncMsg>(&[]), Err(ProtoError::Decode(_))));
        assert!(matches!(
            decode::<SyncMsg>(&[1, 99]),
            Err(ProtoError::Decode(_))
        ));
        let mut b = encode(&SyncMsg::Done);
        b.push(0);
        assert!(matches!(decode::<SyncMsg>(&b), Err(ProtoError::Decode(_))));
    }

    #[test]
    fn every_syncmsg_roundtrips() {
        let k = SigningKey::from_bytes(&[1; 32]);
        let op = SignedOp::sign(
            OpBody {
                mesh: MeshId([7; 32]),
                origin: NodeId(k.verifying_key().to_bytes()),
                seq: 1,
                hlc: Hlc::new(5, 1),
                payload: Payload::SetAdd {
                    ns: "n".into(),
                    set: "s".into(),
                    member: vec![1, 2],
                },
            },
            &k,
        );
        let mut vv = VersionVector::default();
        vv.set(NodeId([2; 32]), 9);
        let msgs = vec![
            SyncMsg::Hello {
                mesh: MeshId([7; 32]),
                vv,
            },
            SyncMsg::Ops { ops: vec![op] },
            SyncMsg::Covered {
                origin: NodeId([3; 32]),
                upto: 42,
            },
            SyncMsg::Done,
        ];
        for m in msgs {
            assert_eq!(decode::<SyncMsg>(&encode(&m)).unwrap(), m);
        }
    }
}
