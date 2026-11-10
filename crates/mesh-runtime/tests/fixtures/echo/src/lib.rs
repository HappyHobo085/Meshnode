//! Returns the request body; `?size=N` returns N bytes of `a` instead.
//! Prints one line to stdout and one to stderr per request. `?lines=N` also prints N more
//! stdout lines; `?partial` ends by writing the marker PARTIAL-MARK-7f3a to stdout without a newline.
use wstd::http::{Body, Error, Request, Response};

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    println!("echo: {} {}", req.method(), req.uri());
    eprintln!("echo: stderr line");
    let param = |name: &str| {
        req.uri()
            .query()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix(name)))
            .map(str::to_owned)
    };
    for i in 0..param("lines=").and_then(|n| n.parse().ok()).unwrap_or(0) {
        println!("loop line {i}");
    }
    if req.uri().query().is_some_and(|q| q.split('&').any(|kv| kv == "partial")) {
        use std::io::Write;
        print!("PARTIAL-MARK-7f3a");
        std::io::stdout().flush().unwrap();
    }
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
