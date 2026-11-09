//! Returns the request body; `?size=N` returns N bytes of `a` instead.
//! Prints one line to stdout and one to stderr per request.
use wstd::http::{Body, Error, Request, Response};

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    println!("echo: {} {}", req.method(), req.uri());
    eprintln!("echo: stderr line");
    let size = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("size=")))
        .and_then(|n| n.parse::<usize>().ok());
    let body = match size {
        Some(n) => vec![b'a'; n],
        None => req.body_mut().contents().await?.to_vec(),
    };
    Ok(Response::new(Body::from(body)))
}
