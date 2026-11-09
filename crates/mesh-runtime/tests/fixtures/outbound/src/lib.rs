//! Fetches `?url=<uri>` with an outgoing GET. Answers (always 200) with `<status> <body>`,
//! or `error: <code>` when the request is refused or fails.
//!
//! Uses the raw `wasi:http` calls rather than `wstd`'s client: that one sends the request
//! body concurrently and reports its I/O error instead of the host's refusal.
use wasip2::http::outgoing_handler;
use wasip2::http::types::{Fields, IncomingResponse, OutgoingRequest, Scheme};
use wasip2::io::streams::StreamError;
use wstd::http::{Body, Error, Request, Response, Uri};

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    let url = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("url=")))
        .unwrap_or_default()
        .to_string();
    let text = match fetch(&url) {
        Ok(text) => text,
        Err(e) => format!("error: {e}"),
    };
    Ok(Response::new(Body::from(text)))
}

fn fetch(url: &str) -> Result<String, String> {
    let uri: Uri = url.parse().map_err(|e| format!("bad url: {e}"))?;
    let request = OutgoingRequest::new(Fields::new());
    let scheme = match uri.scheme_str() {
        Some("http") => Scheme::Http,
        _ => Scheme::Https,
    };
    request.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    request
        .set_authority(uri.authority().map(|a| a.as_str()))
        .map_err(|()| "authority")?;
    request
        .set_path_with_query(uri.path_and_query().map(|p| p.as_str()))
        .map_err(|()| "path")?;
    let future = outgoing_handler::handle(request, None).map_err(|e| format!("{e:?}"))?;
    future.subscribe().block();
    let response: IncomingResponse = future
        .get()
        .ok_or("no response")?
        .map_err(|()| "response taken")?
        .map_err(|e| format!("{e:?}"))?;
    let status = response.status();
    let body = response.consume().map_err(|()| "consume")?;
    let stream = body.stream().map_err(|()| "stream")?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(4096) {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("{e:?}")),
        }
    }
    Ok(format!("{status} {}", String::from_utf8_lossy(&bytes)))
}
