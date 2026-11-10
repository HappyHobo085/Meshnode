//! The native static-site handler (spec §6): a collection's path → hash map, loaded once
//! when the site is swapped in, and a `GET`/`HEAD` server over it.

use std::collections::BTreeMap;

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use mesh_core::{BlobHash, Blobs, NodeError};
use tracing::warn;

use crate::error::ServicesError;

/// A file larger than this is not served.
const MAX_FILE: u64 = 64 << 20;
const INDEX: &str = "index.html";

/// A loaded static site.
#[derive(Debug)]
pub struct StaticSite {
    files: BTreeMap<String, BlobHash>,
}

impl StaticSite {
    /// Reads the index of the collection rooted at `root`; it must be stored here.
    pub async fn load(blobs: &Blobs, root: [u8; 32]) -> Result<StaticSite, ServicesError> {
        let entries = blobs
            .collection_entries(BlobHash::from(root))
            .await?
            .ok_or(NodeError::BlobUnavailable)?;
        Ok(StaticSite {
            files: entries.into_iter().collect(),
        })
    }

    /// Serves one request. Never fails: every outcome is an HTTP response.
    pub async fn serve(&self, blobs: &Blobs, req: &Request<Bytes>) -> Response<Bytes> {
        let head = match *req.method() {
            Method::GET => false,
            Method::HEAD => true,
            _ => {
                let mut r = plain(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
                r.headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
                return r;
            }
        };
        let raw = req.uri().path();
        let Some(segments) = normalise(raw) else {
            return plain(StatusCode::BAD_REQUEST, "Bad path");
        };
        let dir = raw.ends_with('/') || segments.is_empty();
        let key = if dir {
            let mut s = segments;
            s.push(INDEX.to_owned());
            s.join("/")
        } else {
            segments.join("/")
        };
        let Some(hash) = self.files.get(&key) else {
            if !dir && self.is_dir(&key) {
                return redirect(raw, req.uri().query());
            }
            return plain(StatusCode::NOT_FOUND, "Not found");
        };
        self.file(blobs, hash, &key, req, head).await
    }

    /// Whether some file lies below `key/`.
    fn is_dir(&self, key: &str) -> bool {
        let prefix = format!("{key}/");
        self.files
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    async fn file(
        &self,
        blobs: &Blobs,
        hash: &BlobHash,
        name: &str,
        req: &Request<Bytes>,
        head: bool,
    ) -> Response<Bytes> {
        match blobs.size(*hash).await {
            Ok(Some(n)) if n > MAX_FILE => {
                return plain(StatusCode::INTERNAL_SERVER_ERROR, "File too large");
            }
            Ok(Some(_)) => {}
            Ok(None) => return plain(StatusCode::SERVICE_UNAVAILABLE, "File not available"),
            Err(e) => return read_failed(name, &e),
        }
        let etag = format!("\"{}\"", hash.to_hex());
        let mut resp = Response::new(Bytes::new());
        let h = resp.headers_mut();
        h.insert(header::ETAG, header_value(&etag));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        if matches(req, &etag) {
            *resp.status_mut() = StatusCode::NOT_MODIFIED;
            return resp;
        }
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, header_value(&content_type(name)));
        if !head {
            match blobs.read(*hash).await {
                Ok(body) => *resp.body_mut() = body,
                Err(NodeError::BlobUnavailable) => {
                    return plain(StatusCode::SERVICE_UNAVAILABLE, "File not available");
                }
                Err(e) => return read_failed(name, &e),
            }
        }
        resp
    }
}

fn read_failed(name: &str, e: &NodeError) -> Response<Bytes> {
    warn!(file = name, error = %e, "cannot read a static file");
    plain(StatusCode::INTERNAL_SERVER_ERROR, "File unreadable")
}

/// Percent-decodes each segment once and returns the non-empty ones. `None` for anything
/// that is not a plain relative path: `.` or `..` segments, backslashes, NUL, an encoded
/// `/` (which would change the segmentation), a bad escape or non-UTF-8 bytes.
fn normalise(path: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for raw in path.split('/').filter(|s| !s.is_empty()) {
        let seg = decode(raw)?;
        if seg == "." || seg == ".." || seg.contains(['/', '\\', '\0']) {
            return None;
        }
        out.push(seg);
    }
    Some(out)
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: u8| char::from(c).to_digit(16);
            let hi = hex(*b.get(i + 1)?)?;
            let lo = hex(*b.get(i + 2)?)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `301` to the slashed path, keeping the query. `raw` is the request's own (encoded) path.
fn redirect(raw: &str, query: Option<&str>) -> Response<Bytes> {
    let mut to = format!("{raw}/");
    if let Some(q) = query {
        to.push('?');
        to.push_str(q);
    }
    let mut resp = plain(StatusCode::MOVED_PERMANENTLY, "Moved permanently");
    match HeaderValue::try_from(to) {
        Ok(v) => {
            resp.headers_mut().insert(header::LOCATION, v);
            resp
        }
        Err(_) => plain(StatusCode::BAD_REQUEST, "Bad path"),
    }
}

/// Whether `If-None-Match` names `etag` (or is `*`). Weak validators compare as equal.
fn matches(req: &Request<Bytes>, etag: &str) -> bool {
    req.headers()
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|t| t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag)
}

fn content_type(name: &str) -> String {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let mime = match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "pdf" => "application/pdf",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    };
    let text = mime.starts_with("text/") || matches!(mime, "application/json" | "image/svg+xml");
    if text {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_owned()
    }
}

fn header_value(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).expect("ASCII header value")
}

fn plain(code: StatusCode, body: &'static str) -> Response<Bytes> {
    let mut resp = Response::new(Bytes::from_static(body.as_bytes()));
    *resp.status_mut() = code;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}
