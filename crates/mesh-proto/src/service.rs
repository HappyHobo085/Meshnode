//! Service manifests stored in `_mesh` (`service/<name>`), raw postcard.

use crate::ProtoError;
use crate::ids::NodeId;
use crate::join::MAX_TAG;
use crate::op::stash_error;
use serde::{Deserialize, Serialize};

pub const MAX_SERVICE_NAME: usize = 32;
pub const MAX_OUTBOUND: usize = 64;
pub const MAX_PLACEMENT: usize = 64;
pub const MAX_MEMORY_MB: u32 = 1024;
pub const MIN_TIMEOUT_MS: u64 = 100;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
pub const MAX_BODY: u64 = 64 * 1024 * 1024;

/// `[a-z0-9-]{1,32}`.
pub fn valid_service_name(s: &str) -> bool {
    (1..=MAX_SERVICE_NAME).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Content hashes are iroh-blobs hashes.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum ServiceKind {
    Wasm { module: [u8; 32] },
    Static { root: [u8; 32] },
}

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum Placement {
    AllHosts,
    Tags(Vec<String>),
    Nodes(Vec<NodeId>),
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub memory_mb: u32,
    pub request_timeout_ms: u64,
    pub max_body_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory_mb: 64,
            request_timeout_ms: 10_000,
            max_body_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Deserialization validates limits.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawServiceManifest")]
pub struct ServiceManifest {
    pub name: String,
    pub kind: ServiceKind,
    pub version: u64,
    pub limits: Limits,
    pub placement: Placement,
    /// `host:port` entries the service may connect to.
    pub outbound: Vec<String>,
}

#[derive(Deserialize)]
struct RawServiceManifest {
    name: String,
    kind: ServiceKind,
    version: u64,
    limits: Limits,
    placement: Placement,
    outbound: Vec<String>,
}

fn valid_outbound(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

impl ServiceManifest {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if !valid_service_name(&self.name) {
            return Err(ProtoError::Limit("service name"));
        }
        let l = &self.limits;
        if !(1..=MAX_MEMORY_MB).contains(&l.memory_mb) {
            return Err(ProtoError::Limit("memory_mb"));
        }
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&l.request_timeout_ms) {
            return Err(ProtoError::Limit("request_timeout_ms"));
        }
        if l.max_body_bytes > MAX_BODY {
            return Err(ProtoError::Limit("max_body_bytes"));
        }
        if self.outbound.len() > MAX_OUTBOUND {
            return Err(ProtoError::Limit("outbound"));
        }
        if !self.outbound.iter().all(|o| valid_outbound(o)) {
            return Err(ProtoError::Limit("outbound entry"));
        }
        match &self.placement {
            Placement::AllHosts => {}
            Placement::Tags(t) => {
                if t.len() > MAX_PLACEMENT {
                    return Err(ProtoError::Limit("placement tags"));
                }
                if t.iter().any(|x| x.len() > MAX_TAG) {
                    return Err(ProtoError::Limit("placement tag"));
                }
            }
            Placement::Nodes(n) => {
                if n.len() > MAX_PLACEMENT {
                    return Err(ProtoError::Limit("placement nodes"));
                }
            }
        }
        Ok(())
    }
}

impl TryFrom<RawServiceManifest> for ServiceManifest {
    type Error = String;
    fn try_from(r: RawServiceManifest) -> Result<Self, String> {
        let m = ServiceManifest {
            name: r.name,
            kind: r.kind,
            version: r.version,
            limits: r.limits,
            placement: r.placement,
            outbound: r.outbound,
        };
        m.validate().map_err(stash_error)?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProtoError, decode_raw, encode_raw};

    fn m() -> ServiceManifest {
        ServiceManifest {
            name: "web".into(),
            kind: ServiceKind::Wasm { module: [1; 32] },
            version: 3,
            limits: Limits::default(),
            placement: Placement::Tags(vec!["a".into()]),
            outbound: vec!["example.com:443".into()],
        }
    }

    #[test]
    fn roundtrip_raw() {
        let a = m();
        assert_eq!(decode_raw::<ServiceManifest>(&encode_raw(&a)).unwrap(), a);
        let b = ServiceManifest {
            kind: ServiceKind::Static { root: [2; 32] },
            placement: Placement::Nodes(vec![NodeId([5; 32])]),
            ..m()
        };
        assert_eq!(decode_raw::<ServiceManifest>(&encode_raw(&b)).unwrap(), b);
        let c = ServiceManifest {
            placement: Placement::AllHosts,
            ..m()
        };
        assert_eq!(decode_raw::<ServiceManifest>(&encode_raw(&c)).unwrap(), c);
    }

    #[test]
    fn defaults() {
        assert_eq!(
            Limits::default(),
            Limits {
                memory_mb: 64,
                request_timeout_ms: 10_000,
                max_body_bytes: 8 * 1024 * 1024
            }
        );
        assert!(m().validate().is_ok());
    }

    #[test]
    fn names() {
        for ok in ["a", "my-svc-1", &"a".repeat(32)] {
            assert!(valid_service_name(ok), "{ok}");
        }
        for bad in ["", "A", "_mesh", "a.b", &"a".repeat(33)] {
            assert!(!valid_service_name(bad), "{bad}");
        }
    }

    #[test]
    fn bounds() {
        let lim = |f: fn(&mut Limits)| {
            let mut x = m();
            f(&mut x.limits);
            x
        };
        let mut bad = vec![
            lim(|l| l.memory_mb = 0),
            lim(|l| l.memory_mb = 1025),
            lim(|l| l.request_timeout_ms = 99),
            lim(|l| l.request_timeout_ms = 300_001),
            lim(|l| l.max_body_bytes = 64 * 1024 * 1024 + 1),
        ];
        for o in [
            vec!["h:1".to_string(); 65],
            vec!["host".into()],
            vec![":80".into()],
            vec!["h:99999".into()],
        ] {
            bad.push(ServiceManifest { outbound: o, ..m() });
        }
        for p in [
            Placement::Tags(vec!["t".into(); 65]),
            Placement::Tags(vec!["t".repeat(33)]),
            Placement::Nodes(vec![NodeId([0; 32]); 65]),
        ] {
            bad.push(ServiceManifest {
                placement: p,
                ..m()
            });
        }
        bad.push(ServiceManifest {
            name: "Bad".into(),
            ..m()
        });
        for b in bad {
            assert!(matches!(b.validate(), Err(ProtoError::Limit(_))), "{b:?}");
            assert!(matches!(
                decode_raw::<ServiceManifest>(&encode_raw(&b)),
                Err(ProtoError::Limit(_))
            ));
        }
    }

    #[test]
    fn edges_accepted() {
        let mut x = m();
        x.limits = Limits {
            memory_mb: 1024,
            request_timeout_ms: 100,
            max_body_bytes: 64 * 1024 * 1024,
        };
        x.outbound = vec!["h:65535".to_string(); 64];
        x.placement = Placement::Tags(vec!["t".repeat(32); 64]);
        assert!(x.validate().is_ok());
        assert!(decode_raw::<ServiceManifest>(&encode_raw(&x)).is_ok());
    }

    #[test]
    fn decode_rejects_trailing() {
        let mut b = encode_raw(&m());
        b.push(0);
        assert!(matches!(
            decode_raw::<ServiceManifest>(&b),
            Err(ProtoError::Decode(_))
        ));
    }
}
