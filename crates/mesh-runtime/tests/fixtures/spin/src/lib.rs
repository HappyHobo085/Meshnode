//! Loops forever without yielding: the request can only end by being interrupted.
use wstd::http::{Body, Error, Request, Response};

#[wstd::http_server]
async fn main(_req: Request<Body>) -> Result<Response<Body>, Error> {
    let mut n: u64 = 0;
    loop {
        n = std::hint::black_box(n.wrapping_add(1));
    }
}
