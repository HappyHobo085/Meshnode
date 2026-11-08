// THROWAWAY SPIKE GUEST: a wasi:http component that echoes a counter-ish payload.
// Purpose: prove the phone can run a standard wasi:http/incoming-handler component.
use wstd::http::{Body, Request, Response, StatusCode};

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
    let path = req.uri().path().to_string();
    // A little CPU work so JIT vs interpreter timings are visible.
    let mut acc: u64 = 0;
    for i in 0..2_000_000u64 {
        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(i);
    }
    let body = format!("hello from wasm; path={path}; acc={acc}\n");
    Ok(Response::builder().status(StatusCode::OK).body(Body::from(body))?)
}
