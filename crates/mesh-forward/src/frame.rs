//! Conversion between `http` messages and `httpfwd` frames (spec §3.2), and one exchange on
//! a stream.

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use mesh_core::Connection;
use mesh_core::frame::read_frame;
use mesh_proto::{
    ForwardRequest, ForwardResponse, HTTPFWD_MAX_FRAME, MAX_HEADER_BYTES, MAX_HEADERS, MAX_PATH,
    is_hop_by_hop,
};

use crate::remote::ForwardError;

/// A received `httpfwd` frame does not make a valid `http` request or response.
#[derive(Debug, thiserror::Error)]
pub enum FrameConvError {
    #[error("invalid method")]
    Method,
    #[error("invalid path")]
    Path,
    #[error("invalid status")]
    Status,
    #[error("invalid header name {0:?}")]
    HeaderName(String),
    #[error("invalid header value")]
    HeaderValue,
}

/// A request that cannot be put in an `httpfwd` frame; answered with [`Unframeable::status`]
/// instead of being forwarded.
#[derive(Debug, thiserror::Error)]
pub enum Unframeable {
    #[error(
        "more than {} headers or {} bytes of headers",
        mesh_proto::MAX_HEADERS,
        mesh_proto::MAX_HEADER_BYTES
    )]
    HeaderFields,
    #[error("path and query longer than {} bytes", mesh_proto::MAX_PATH)]
    Uri,
    #[error("request frame larger than {} bytes", mesh_proto::HTTPFWD_MAX_FRAME)]
    TooLarge,
    #[error("invalid request: {0}")]
    Invalid(mesh_proto::ProtoError),
}

impl Unframeable {
    pub fn status(&self) -> StatusCode {
        match self {
            Unframeable::HeaderFields => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Unframeable::Uri => StatusCode::URI_TOO_LONG,
            Unframeable::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Unframeable::Invalid(_) => StatusCode::BAD_REQUEST,
        }
    }
}

/// Appends `headers` to `map`, skipping hop-by-hop ones.
pub fn copy_headers(
    headers: Vec<(String, Vec<u8>)>,
    map: &mut http::HeaderMap,
) -> Result<(), FrameConvError> {
    for (name, value) in headers {
        if is_hop_by_hop(&name) {
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| FrameConvError::HeaderName(name))?;
        let value = HeaderValue::from_bytes(&value).map_err(|_| FrameConvError::HeaderValue)?;
        map.append(name, value);
    }
    Ok(())
}

/// `headers` as frame headers, without hop-by-hop ones.
pub fn frame_headers(headers: &http::HeaderMap) -> Vec<(String, Vec<u8>)> {
    headers
        .iter()
        .filter(|(n, _)| !is_hop_by_hop(n.as_str()))
        .map(|(n, v)| (n.as_str().to_owned(), v.as_bytes().to_vec()))
        .collect()
}

/// Sends one request frame on a new stream and reads the response frame (at most
/// `HTTPFWD_MAX_FRAME`; a larger length prefix fails at once).
pub(crate) async fn exchange(
    conn: &Connection,
    frame: &[u8],
) -> Result<ForwardResponse, ForwardError> {
    let stream = |e: &dyn std::fmt::Display| ForwardError::Stream(e.to_string());
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| stream(&e))?;
    send.write_all(frame).await.map_err(|e| stream(&e))?;
    send.finish().map_err(|e| stream(&e))?;
    read_frame::<_, ForwardResponse>(&mut recv, HTTPFWD_MAX_FRAME)
        .await?
        .ok_or(ForwardError::NoResponse)
}

/// The whole `httpfwd` frame (length prefix included) for `req`, without hop-by-hop
/// headers.
pub(crate) fn encode_request(name: &str, req: Request<Bytes>) -> Result<Vec<u8>, Unframeable> {
    let (parts, body) = req.into_parts();
    if body.len() > HTTPFWD_MAX_FRAME {
        return Err(Unframeable::TooLarge);
    }
    let fr = ForwardRequest {
        service: name.to_owned(),
        method: parts.method.as_str().to_owned(),
        path_and_query: parts
            .uri
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned(),
        headers: frame_headers(&parts.headers),
        body: body.to_vec(),
    };
    let header_bytes: usize = fr.headers.iter().map(|(n, v)| n.len() + v.len()).sum();
    if fr.headers.len() > MAX_HEADERS || header_bytes > MAX_HEADER_BYTES {
        return Err(Unframeable::HeaderFields);
    }
    if fr.path_and_query.len() > MAX_PATH {
        return Err(Unframeable::Uri);
    }
    fr.validate().map_err(Unframeable::Invalid)?;
    let body = mesh_proto::encode(&fr);
    if body.len() > HTTPFWD_MAX_FRAME {
        return Err(Unframeable::TooLarge);
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// The response a frame describes, without hop-by-hop headers.
pub(crate) fn to_response(fr: ForwardResponse) -> Result<Response<Bytes>, ForwardError> {
    // An informational status is no final answer to a forwarded request.
    if fr.status < 200 {
        return Err(FrameConvError::Status.into());
    }
    let status = StatusCode::from_u16(fr.status).map_err(|_| FrameConvError::Status)?;
    let mut resp = Response::new(Bytes::from(fr.body));
    *resp.status_mut() = status;
    copy_headers(fr.headers, resp.headers_mut())?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frame_strips_hop_by_hop() {
        let req = Request::post("http://web.mesh.localhost/p?q=1")
            .header("host", "web.mesh.localhost")
            .header("connection", "close")
            .header("transfer-encoding", "chunked")
            .header("x-a", "1")
            .body(Bytes::from_static(b"b"))
            .unwrap();
        let frame = encode_request("web", req).unwrap();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let fr: ForwardRequest = mesh_proto::decode(&frame[4..]).unwrap();
        assert_eq!(fr.service, "web");
        assert_eq!(fr.method, "POST");
        assert_eq!(fr.path_and_query, "/p?q=1");
        assert_eq!(fr.body, b"b");
        assert_eq!(
            fr.headers,
            [
                ("host".to_string(), b"web.mesh.localhost".to_vec()),
                ("x-a".to_string(), b"1".to_vec()),
            ]
        );
    }

    #[test]
    fn response_from_frame_rejects_bad_headers() {
        let fr = |headers: Vec<(String, Vec<u8>)>| ForwardResponse {
            status: 200,
            headers,
            body: Vec::new(),
        };
        for h in [
            ("x-a".to_string(), b"a\r\nb".to_vec()),
            ("bad name".to_string(), b"v".to_vec()),
        ] {
            assert!(to_response(fr(vec![h])).is_err());
        }
        let ok = to_response(fr(vec![
            ("upgrade".into(), b"h2c".to_vec()),
            ("x-a".into(), b"1".to_vec()),
        ]))
        .unwrap();
        assert_eq!(ok.headers().len(), 1);
        assert_eq!(ok.headers()["x-a"], "1");
    }

    #[test]
    fn unframeable_requests_get_their_status() {
        let get = |path: &str| Request::get(path.to_owned());
        let mut many = get("/");
        for i in 0..=MAX_HEADERS {
            many = many.header(format!("x-{i}"), "v");
        }
        let cases = [
            (many.body(Bytes::new()).unwrap(), 431),
            (
                get("/")
                    .header("x-big", "a".repeat(MAX_HEADER_BYTES))
                    .body(Bytes::new())
                    .unwrap(),
                431,
            ),
            (
                get(&format!("/{}", "a".repeat(MAX_PATH)))
                    .body(Bytes::new())
                    .unwrap(),
                414,
            ),
            (
                get("/")
                    .body(Bytes::from(vec![0u8; HTTPFWD_MAX_FRAME + 1]))
                    .unwrap(),
                413,
            ),
            // The body fits, but not with the rest of the frame.
            (
                get("/")
                    .body(Bytes::from(vec![0u8; HTTPFWD_MAX_FRAME - 8]))
                    .unwrap(),
                413,
            ),
        ];
        for (req, want) in cases {
            let err = encode_request("web", req).unwrap_err();
            assert_eq!(err.status().as_u16(), want, "{err}");
        }
        // At the limits it still frames.
        let mut most = get(&format!("/{}", "a".repeat(MAX_PATH - 1)));
        for i in 0..MAX_HEADERS {
            most = most.header(format!("x-{i}"), "v");
        }
        assert!(encode_request("web", most.body(Bytes::new()).unwrap()).is_ok());
    }

    #[test]
    fn informational_peer_status_rejected() {
        for status in [100, 101, 199] {
            let fr = ForwardResponse {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            };
            assert!(to_response(fr).is_err(), "{status}");
        }
    }
}
