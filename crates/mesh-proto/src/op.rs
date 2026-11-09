//! Signed replication operations.

use crate::ProtoError;
use crate::ids::{MeshId, NodeId};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use mesh_hlc::Hlc;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

pub const MAX_NS: usize = 64;
pub const MAX_KEY: usize = 512;
pub const MAX_ORDER: usize = 64;
pub const MAX_VALUE: usize = 1_048_576;

const SIG_DOMAIN: &[u8] = b"meshnode/op/1";

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum Payload {
    Set {
        ns: String,
        key: String,
        order: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    SetAdd {
        ns: String,
        set: String,
        member: Vec<u8>,
    },
}

impl Payload {
    pub fn ns(&self) -> &str {
        match self {
            Payload::Set { ns, .. } | Payload::SetAdd { ns, .. } => ns,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct OpBody {
    pub mesh: MeshId,
    pub origin: NodeId,
    pub seq: u64,
    pub hlc: Hlc,
    pub payload: Payload,
}

fn check_str(s: &str, max: usize, name: &'static str) -> Result<(), ProtoError> {
    if s.len() > max {
        return Err(ProtoError::Limit(name));
    }
    if s.as_bytes().contains(&0) {
        return Err(ProtoError::Nul(name));
    }
    Ok(())
}

impl OpBody {
    pub fn validate(&self) -> Result<(), ProtoError> {
        match &self.payload {
            Payload::Set {
                ns,
                key,
                order,
                value,
            } => {
                check_str(ns, MAX_NS, "ns")?;
                check_str(key, MAX_KEY, "key")?;
                if order.len() > MAX_ORDER {
                    return Err(ProtoError::Limit("order"));
                }
                if value.as_ref().is_some_and(|v| v.len() > MAX_VALUE) {
                    return Err(ProtoError::Limit("value"));
                }
            }
            Payload::SetAdd { ns, set, member } => {
                check_str(ns, MAX_NS, "ns")?;
                check_str(set, MAX_KEY, "set")?;
                if member.len() > MAX_VALUE {
                    return Err(ProtoError::Limit("member"));
                }
            }
        }
        Ok(())
    }

    fn signing_message(&self) -> Result<Vec<u8>, ProtoError> {
        let body = postcard::to_allocvec(self).map_err(|e| ProtoError::Decode(e.to_string()))?;
        let mut m = Vec::with_capacity(SIG_DOMAIN.len() + body.len());
        m.extend_from_slice(SIG_DOMAIN);
        m.extend_from_slice(&body);
        Ok(m)
    }
}

mod sig_serde {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::ser::SerializeTuple;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(sig: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        let mut t = s.serialize_tuple(64)?;
        for b in sig {
            t.serialize_element(b)?;
        }
        t.end()
    }

    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = [u8; 64];
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("64 bytes")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; 64], A::Error> {
            let mut out = [0u8; 64];
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(i, &self))?;
            }
            Ok(out)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        d.deserialize_tuple(64, V)
    }
}

#[derive(Deserialize)]
struct RawSignedOp {
    body: OpBody,
    #[serde(with = "sig_serde")]
    sig: [u8; 64],
}

thread_local! {
    static STASHED: RefCell<Option<ProtoError>> = const { RefCell::new(None) };
}

/// Takes the validation error recorded by the last failed `SignedOp` deserialization on this thread.
pub(crate) fn take_stashed_error() -> Option<ProtoError> {
    STASHED.with(|s| s.borrow_mut().take())
}

/// Records `e` for [`take_stashed_error`] and returns its message for the serde error.
pub(crate) fn stash_error(e: ProtoError) -> String {
    let msg = e.to_string();
    STASHED.with(|s| *s.borrow_mut() = Some(e));
    msg
}

impl TryFrom<RawSignedOp> for SignedOp {
    type Error = String;
    fn try_from(r: RawSignedOp) -> Result<Self, String> {
        if let Err(e) = r.body.validate() {
            return Err(stash_error(e));
        }
        Ok(SignedOp {
            body: r.body,
            sig: r.sig,
        })
    }
}

/// An op plus Ed25519 signature by `body.origin`. Deserialization validates the body.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawSignedOp")]
pub struct SignedOp {
    pub body: OpBody,
    #[serde(with = "sig_serde")]
    pub sig: [u8; 64],
}

impl SignedOp {
    /// Signs `body`. (Serializing an `OpBody` cannot fail; a zero signature would fail verify.)
    pub fn sign(body: OpBody, key: &SigningKey) -> SignedOp {
        let sig = match body.signing_message() {
            Ok(m) => key.sign(&m).to_bytes(),
            Err(_) => [0u8; 64],
        };
        SignedOp { body, sig }
    }

    pub fn verify(&self) -> Result<(), ProtoError> {
        let vk = VerifyingKey::from_bytes(&self.body.origin.0).map_err(|_| ProtoError::BadSig)?;
        let msg = self.body.signing_message()?;
        vk.verify_strict(&msg, &Signature::from_bytes(&self.sig))
            .map_err(|_| ProtoError::BadSig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn body(k: &SigningKey, seq: u64) -> OpBody {
        OpBody {
            mesh: MeshId([7; 32]),
            origin: NodeId(k.verifying_key().to_bytes()),
            seq,
            hlc: Hlc::new(1000, 0),
            payload: Payload::Set {
                ns: "ns".into(),
                key: "k".into(),
                order: vec![],
                value: Some(b"v".to_vec()),
            },
        }
    }

    fn set_op(ns: impl Into<String>, key_: impl Into<String>, value: Vec<u8>) -> OpBody {
        OpBody {
            payload: Payload::Set {
                ns: ns.into(),
                key: key_.into(),
                order: vec![],
                value: Some(value),
            },
            ..body(&key(1), 1)
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        let k = key(1);
        let op = SignedOp::sign(body(&k, 1), &k);
        assert!(op.verify().is_ok());
    }

    #[test]
    fn tampered_payload_fails_verify() {
        let k = key(1);
        let mut op = SignedOp::sign(body(&k, 1), &k);
        if let Payload::Set { value, .. } = &mut op.body.payload {
            *value = Some(b"x".to_vec())
        }
        assert!(matches!(op.verify(), Err(ProtoError::BadSig)));
    }

    #[test]
    fn signature_covers_mesh_id() {
        let k = key(1);
        let mut op = SignedOp::sign(body(&k, 1), &k);
        op.body.mesh = MeshId([9; 32]);
        assert!(matches!(op.verify(), Err(ProtoError::BadSig)));
    }

    #[test]
    fn limits() {
        assert!(
            set_op("n".repeat(64), "k", vec![0; 1_048_576])
                .validate()
                .is_ok()
        );
        assert!(matches!(
            set_op("n".repeat(65), "k", vec![]).validate(),
            Err(ProtoError::Limit(_))
        ));
        assert!(matches!(
            set_op("n", "k".repeat(513), vec![]).validate(),
            Err(ProtoError::Limit(_))
        ));
        assert!(matches!(
            set_op("n", "k", vec![0; 1_048_577]).validate(),
            Err(ProtoError::Limit(_))
        ));
        assert!(matches!(
            set_op("n", "a\0b", vec![]).validate(),
            Err(ProtoError::Nul(_))
        ));
    }

    #[test]
    fn setadd_and_order_limits() {
        let k = key(1);
        let mut b = body(&k, 1);
        b.payload = Payload::SetAdd {
            ns: "n".into(),
            set: "s\0".into(),
            member: vec![],
        };
        assert!(matches!(b.validate(), Err(ProtoError::Nul("set"))));
        b.payload = Payload::Set {
            ns: "n".into(),
            key: "k".into(),
            order: vec![0; 65],
            value: None,
        };
        assert!(matches!(b.validate(), Err(ProtoError::Limit("order"))));
    }

    #[test]
    fn decode_validates_signed_op() {
        let k = key(1);
        let op = SignedOp::sign(set_op("n", "a\0b", vec![]), &k);
        let bytes = crate::encode(&op);
        assert!(matches!(
            crate::decode::<SignedOp>(&bytes),
            Err(ProtoError::Nul("key"))
        ));
        let big = SignedOp::sign(set_op("n", "k", vec![0; 1_048_577]), &k);
        assert!(matches!(
            crate::decode::<SignedOp>(&crate::encode(&big)),
            Err(ProtoError::Limit("value"))
        ));
        let ok = SignedOp::sign(body(&k, 1), &k);
        assert_eq!(crate::decode::<SignedOp>(&crate::encode(&ok)).unwrap(), ok);
    }
}
