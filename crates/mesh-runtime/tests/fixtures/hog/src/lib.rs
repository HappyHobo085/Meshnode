//! Allocates 1 MiB blocks and keeps them until allocation fails (which aborts, a trap).
//! After each block it records the number of MiB held so far under KV key `mib`.
use wstd::http::{Body, Error, Request, Response};

wit_bindgen::generate!({
    inline: "package local:hog; world hog { import meshnode:kv/store; }",
    path: "../../../wit",
    generate_all,
});

use meshnode::kv::store;

#[wstd::http_server]
async fn main(_req: Request<Body>) -> Result<Response<Body>, Error> {
    let mut hoard: Vec<Vec<u8>> = Vec::new();
    loop {
        hoard.push(std::hint::black_box(vec![1u8; 1 << 20]));
        store::set("mib", hoard.len().to_string().as_bytes());
    }
}
