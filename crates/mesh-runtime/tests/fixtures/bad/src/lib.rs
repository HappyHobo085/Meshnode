//! Lists the guest's working directory. Rust std reaches the filesystem through
//! `wasi:filesystem/preopens` and `wasi:filesystem/types`, which are outside the
//! `meshnode:service/service` world, so the host must refuse to load this component.
use wstd::http::{Body, Error, Request, Response};

#[wstd::http_server]
async fn main(_req: Request<Body>) -> Result<Response<Body>, Error> {
    let entries = std::fs::read_dir(".").map(|d| d.count()).unwrap_or(0);
    Ok(Response::new(Body::from(format!("{entries}\n"))))
}
