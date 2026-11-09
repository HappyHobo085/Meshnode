//! `join/1` protocol messages and the mesh genesis.

use crate::ProtoError;
use crate::ids::{MeshId, NodeAddrBytes, NodeId};
use crate::op::stash_error;
use serde::{Deserialize, Serialize};

pub const MAX_NAME: usize = 64;
pub const MAX_PLATFORM: usize = 32;
pub const MAX_TAGS: usize = 16;
pub const MAX_TAG: usize = 32;
pub const MAX_PEERS: usize = 64;
pub const MAX_ADDR: usize = 2048;

const MESH_DOMAIN: &[u8] = b"meshnode/mesh/1";

/// Founding record of a mesh; its hash is the [`MeshId`].
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct MeshGenesis {
    pub founder: NodeId,
    pub nonce: [u8; 32],
}

impl MeshGenesis {
    pub fn mesh_id(&self) -> MeshId {
        let mut h = blake3::Hasher::new();
        h.update(MESH_DOMAIN);
        h.update(&self.founder.0);
        h.update(&self.nonce);
        MeshId(*h.finalize().as_bytes())
    }
}

/// Self-reported member details. Deserialization validates limits.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawMemberInfo")]
pub struct MemberInfo {
    pub name: String,
    pub platform: String,
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct RawMemberInfo {
    name: String,
    platform: String,
    tags: Vec<String>,
}

impl MemberInfo {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.name.len() > MAX_NAME {
            return Err(ProtoError::Limit("name"));
        }
        if self.platform.len() > MAX_PLATFORM {
            return Err(ProtoError::Limit("platform"));
        }
        if self.tags.len() > MAX_TAGS {
            return Err(ProtoError::Limit("tags"));
        }
        if self.tags.iter().any(|t| t.len() > MAX_TAG) {
            return Err(ProtoError::Limit("tag"));
        }
        Ok(())
    }
}

impl TryFrom<RawMemberInfo> for MemberInfo {
    type Error = String;
    fn try_from(r: RawMemberInfo) -> Result<Self, String> {
        let info = MemberInfo {
            name: r.name,
            platform: r.platform,
            tags: r.tags,
        };
        info.validate().map_err(stash_error)?;
        Ok(info)
    }
}

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct RemovalRecord {
    pub node_id: NodeId,
    pub cutoff_seq: u64,
}

/// `info` is validated by its own deserialization, so decoding a request enforces its limits.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct JoinRequest {
    pub mesh: MeshId,
    pub token: [u8; 32],
    pub info: MemberInfo,
}

impl JoinRequest {
    pub fn validate(&self) -> Result<(), ProtoError> {
        self.info.validate()
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum RefuseReason {
    Expired,
    Used,
    Unknown,
    WrongMesh,
    Removed,
    InviterUnavailable,
}

/// Deserialization validates limits.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawJoinResponse")]
pub enum JoinResponse {
    Accepted {
        genesis: MeshGenesis,
        peers: Vec<(NodeId, NodeAddrBytes)>,
    },
    Refused(RefuseReason),
}

#[derive(Deserialize)]
enum RawJoinResponse {
    Accepted {
        genesis: MeshGenesis,
        peers: Vec<(NodeId, NodeAddrBytes)>,
    },
    Refused(RefuseReason),
}

impl JoinResponse {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if let JoinResponse::Accepted { peers, .. } = self {
            if peers.len() > MAX_PEERS {
                return Err(ProtoError::Limit("peers"));
            }
            if peers.iter().any(|(_, a)| a.0.len() > MAX_ADDR) {
                return Err(ProtoError::Limit("peer addr"));
            }
        }
        Ok(())
    }
}

impl TryFrom<RawJoinResponse> for JoinResponse {
    type Error = String;
    fn try_from(r: RawJoinResponse) -> Result<Self, String> {
        let resp = match r {
            RawJoinResponse::Accepted { genesis, peers } => {
                JoinResponse::Accepted { genesis, peers }
            }
            RawJoinResponse::Refused(why) => JoinResponse::Refused(why),
        };
        resp.validate().map_err(stash_error)?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProtoError, decode, encode};

    fn info() -> MemberInfo {
        MemberInfo {
            name: "laptop".into(),
            platform: "windows".into(),
            tags: vec!["a".into(), "b".into()],
        }
    }

    fn genesis() -> MeshGenesis {
        MeshGenesis {
            founder: NodeId([1; 32]),
            nonce: [2; 32],
        }
    }

    fn peers(n: usize) -> Vec<(NodeId, NodeAddrBytes)> {
        (0..n)
            .map(|i| (NodeId([i as u8; 32]), NodeAddrBytes(vec![i as u8; 4])))
            .collect()
    }

    fn rt<T>(v: T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        assert_eq!(decode::<T>(&encode(&v)).unwrap(), v);
    }

    #[test]
    fn roundtrips() {
        rt(genesis());
        rt(info());
        rt(RemovalRecord {
            node_id: NodeId([4; 32]),
            cutoff_seq: 77,
        });
        rt(JoinRequest {
            mesh: MeshId([5; 32]),
            token: [6; 32],
            info: info(),
        });
        rt(JoinResponse::Accepted {
            genesis: genesis(),
            peers: peers(3),
        });
        for r in [
            RefuseReason::Expired,
            RefuseReason::Used,
            RefuseReason::Unknown,
            RefuseReason::WrongMesh,
            RefuseReason::Removed,
            RefuseReason::InviterUnavailable,
        ] {
            rt(JoinResponse::Refused(r));
            rt(r);
        }
    }

    #[test]
    fn mesh_id_vector() {
        let mut h = blake3::Hasher::new();
        h.update(b"meshnode/mesh/1");
        h.update(&[1; 32]);
        h.update(&[2; 32]);
        assert_eq!(genesis().mesh_id(), MeshId(*h.finalize().as_bytes()));
    }

    #[test]
    fn member_info_limits() {
        let mut i = info();
        i.name = "n".repeat(64);
        i.platform = "p".repeat(32);
        i.tags = vec!["t".repeat(32); 16];
        assert!(i.validate().is_ok());
        let bad = [
            MemberInfo {
                name: "n".repeat(65),
                ..info()
            },
            MemberInfo {
                platform: "p".repeat(33),
                ..info()
            },
            MemberInfo {
                tags: vec!["t".into(); 17],
                ..info()
            },
            MemberInfo {
                tags: vec!["t".repeat(33)],
                ..info()
            },
        ];
        for b in bad {
            assert!(matches!(b.validate(), Err(ProtoError::Limit(_))));
            assert!(matches!(
                decode::<MemberInfo>(&encode(&b)),
                Err(ProtoError::Limit(_))
            ));
            let req = JoinRequest {
                mesh: MeshId([0; 32]),
                token: [0; 32],
                info: b,
            };
            assert!(matches!(req.validate(), Err(ProtoError::Limit(_))));
            assert!(matches!(
                decode::<JoinRequest>(&encode(&req)),
                Err(ProtoError::Limit(_))
            ));
        }
    }

    #[test]
    fn join_response_peer_limit() {
        let ok = JoinResponse::Accepted {
            genesis: genesis(),
            peers: peers(64),
        };
        assert!(ok.validate().is_ok());
        rt(ok);
        let bad = JoinResponse::Accepted {
            genesis: genesis(),
            peers: peers(65),
        };
        assert!(matches!(bad.validate(), Err(ProtoError::Limit(_))));
        assert!(matches!(
            decode::<JoinResponse>(&encode(&bad)),
            Err(ProtoError::Limit(_))
        ));
        let mut p = peers(1);
        p[0].1 = NodeAddrBytes(vec![0; 2049]);
        let bad = JoinResponse::Accepted {
            genesis: genesis(),
            peers: p,
        };
        assert!(matches!(
            decode::<JoinResponse>(&encode(&bad)),
            Err(ProtoError::Limit(_))
        ));
        let mut p = peers(1);
        p[0].1 = NodeAddrBytes(vec![0; 2048]);
        assert!(
            JoinResponse::Accepted {
                genesis: genesis(),
                peers: p
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn wrong_version_rejected() {
        let mut b = encode(&JoinResponse::Refused(RefuseReason::Used));
        b[0] = 2;
        assert!(matches!(
            decode::<JoinResponse>(&b),
            Err(ProtoError::Version(2))
        ));
    }
}
