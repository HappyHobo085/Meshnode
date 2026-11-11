//! `httpfwd/1` request-forwarding messages (one request frame, one response frame per stream).

use crate::ProtoError;
use crate::op::stash_error;
use crate::service::valid_service_name;
use serde::{Deserialize, Serialize};

pub const ALPN_HTTPFWD: &[u8] = b"meshnode/httpfwd/1";
/// Reserved service name answered by every node (204) without consulting its services; not a
/// valid user service name (see `valid_service_name`).
pub const PING_SERVICE: &str = "_ping";
pub const HTTPFWD_MAX_FRAME: usize = 64 * 1024 * 1024 + 64 * 1024;
pub const MAX_HEADERS: usize = 128;
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_PATH: usize = 8 * 1024;

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
];

/// Case-insensitive: headers that must not be forwarded in either direction.
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// RFC 9110 token: one or more `tchar`.
fn valid_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn validate_headers(headers: &[(String, Vec<u8>)]) -> Result<(), ProtoError> {
    if headers.len() > MAX_HEADERS {
        return Err(ProtoError::Limit("headers"));
    }
    let total: usize = headers.iter().map(|(n, v)| n.len() + v.len()).sum();
    if total > MAX_HEADER_BYTES {
        return Err(ProtoError::Limit("header bytes"));
    }
    Ok(())
}

/// Deserialization validates limits.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawForwardRequest")]
pub struct ForwardRequest {
    pub service: String,
    pub method: String,
    pub path_and_query: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

#[derive(Deserialize)]
struct RawForwardRequest {
    service: String,
    method: String,
    path_and_query: String,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

impl ForwardRequest {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.service != PING_SERVICE && !valid_service_name(&self.service) {
            return Err(ProtoError::Limit("service name"));
        }
        if !valid_token(&self.method) {
            return Err(ProtoError::Limit("method"));
        }
        if !self.path_and_query.starts_with('/') || self.path_and_query.len() > MAX_PATH {
            return Err(ProtoError::Limit("path_and_query"));
        }
        validate_headers(&self.headers)
    }
}

impl TryFrom<RawForwardRequest> for ForwardRequest {
    type Error = String;
    fn try_from(r: RawForwardRequest) -> Result<Self, String> {
        let req = ForwardRequest {
            service: r.service,
            method: r.method,
            path_and_query: r.path_and_query,
            headers: r.headers,
            body: r.body,
        };
        req.validate().map_err(stash_error)?;
        Ok(req)
    }
}

/// Deserialization validates limits.
#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawForwardResponse")]
pub struct ForwardResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

#[derive(Deserialize)]
struct RawForwardResponse {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

impl ForwardResponse {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if !(100..=999).contains(&self.status) {
            return Err(ProtoError::Limit("status"));
        }
        validate_headers(&self.headers)
    }
}

impl TryFrom<RawForwardResponse> for ForwardResponse {
    type Error = String;
    fn try_from(r: RawForwardResponse) -> Result<Self, String> {
        let resp = ForwardResponse {
            status: r.status,
            headers: r.headers,
            body: r.body,
        };
        resp.validate().map_err(stash_error)?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    fn req() -> ForwardRequest {
        ForwardRequest {
            service: "web".into(),
            method: "GET".into(),
            path_and_query: "/a?b=1".into(),
            headers: vec![("host".into(), b"web.mesh.localhost".to_vec())],
            body: vec![1, 2, 3],
        }
    }

    #[test]
    fn ping_service_request_validates() {
        let mut r = req();
        r.service = PING_SERVICE.into();
        r.validate().unwrap();
        r.service = "_other".into();
        assert!(r.validate().is_err());
    }

    fn resp() -> ForwardResponse {
        ForwardResponse {
            status: 200,
            headers: vec![("content-type".into(), b"text/plain".to_vec())],
            body: b"ok".to_vec(),
        }
    }

    fn limit_req(r: ForwardRequest) {
        assert!(matches!(
            decode::<ForwardRequest>(&encode(&r)),
            Err(ProtoError::Limit(_))
        ));
    }

    #[test]
    fn roundtrips() {
        assert_eq!(decode::<ForwardRequest>(&encode(&req())).unwrap(), req());
        assert_eq!(decode::<ForwardResponse>(&encode(&resp())).unwrap(), resp());
    }

    #[test]
    fn constants() {
        assert_eq!(ALPN_HTTPFWD, b"meshnode/httpfwd/1");
        assert_eq!(HTTPFWD_MAX_FRAME, 64 * 1024 * 1024 + 64 * 1024);
    }

    #[test]
    fn rejects_bad_requests() {
        for s in ["", "Bad", "_mesh", &"a".repeat(33)] {
            limit_req(ForwardRequest {
                service: s.into(),
                ..req()
            });
        }
        for m in ["GE T", "", "G\u{e9}T", "GET\n"] {
            limit_req(ForwardRequest {
                method: m.into(),
                ..req()
            });
        }
        limit_req(ForwardRequest {
            headers: vec![("a".into(), vec![]); 129],
            ..req()
        });
        limit_req(ForwardRequest {
            headers: vec![("a".into(), vec![0; 65 * 1024])],
            ..req()
        });
        limit_req(ForwardRequest {
            path_and_query: "a".into(),
            ..req()
        });
        limit_req(ForwardRequest {
            path_and_query: String::new(),
            ..req()
        });
        limit_req(ForwardRequest {
            path_and_query: format!("/{}", "a".repeat(8 * 1024)),
            ..req()
        });
    }

    #[test]
    fn accepts_edges() {
        let r = ForwardRequest {
            headers: vec![("a".into(), vec![]); 128],
            path_and_query: format!("/{}", "a".repeat(8 * 1024 - 1)),
            method: "M-SEARCH".into(),
            ..req()
        };
        assert_eq!(decode::<ForwardRequest>(&encode(&r)).unwrap(), r);
        let r = ForwardRequest {
            headers: vec![("ab".into(), vec![0; 64 * 1024 - 2])],
            ..req()
        };
        assert_eq!(decode::<ForwardRequest>(&encode(&r)).unwrap(), r);
    }

    #[test]
    fn rejects_bad_responses() {
        for s in [0, 99, 1000] {
            let r = ForwardResponse {
                status: s,
                ..resp()
            };
            assert!(matches!(
                decode::<ForwardResponse>(&encode(&r)),
                Err(ProtoError::Limit(_))
            ));
        }
        for s in [100, 999] {
            let r = ForwardResponse {
                status: s,
                ..resp()
            };
            assert_eq!(decode::<ForwardResponse>(&encode(&r)).unwrap(), r);
        }
        let r = ForwardResponse {
            headers: vec![("a".into(), vec![]); 129],
            ..resp()
        };
        assert!(matches!(
            decode::<ForwardResponse>(&encode(&r)),
            Err(ProtoError::Limit(_))
        ));
    }

    #[test]
    fn hop_by_hop() {
        for h in [
            "connection",
            "Keep-Alive",
            "transfer-encoding",
            "TE",
            "upgrade",
            "Proxy-Authorization",
            "proxy-authenticate",
            "Proxy-Connection",
        ] {
            assert!(is_hop_by_hop(h), "{h}");
        }
        for h in ["host", "content-type", "trailer", ""] {
            assert!(!is_hop_by_hop(h), "{h}");
        }
    }

    #[test]
    fn wrong_version_rejected() {
        let mut b = encode(&req());
        b[0] = 2;
        assert!(matches!(
            decode::<ForwardRequest>(&b),
            Err(ProtoError::Version(2))
        ));
        let mut b = encode(&resp());
        b[0] = 2;
        assert!(matches!(
            decode::<ForwardResponse>(&b),
            Err(ProtoError::Version(2))
        ));
    }
}
