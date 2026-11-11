//! Exercises `meshnode:kv/local` through HTTP routes, next to `meshnode:kv/store` so a test
//! can tell the two apart. Values come from the query string (`v`) or, for `/lset`, from
//! the request body when it is not empty.
use wstd::http::{Body, Error, Request, Response, StatusCode};

wit_bindgen::generate!({
    inline: "package fixture:local; world local-kv { import meshnode:kv/local; import meshnode:kv/store; }",
    path: "../../../wit",
    generate_all,
});

use meshnode::kv::{local, store};

fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
}

fn text(s: impl Into<String>) -> Result<Response<Body>, Error> {
    Ok(Response::new(Body::from(s.into())))
}

fn status(code: StatusCode) -> Result<Response<Body>, Error> {
    Ok(Response::builder().status(code).body(Body::empty())?)
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let path = req.uri().path().to_string();
    let q = req.uri().query().unwrap_or("").to_string();
    let p = |name: &str| param(&q, name).unwrap_or("").to_string();
    match path.as_str() {
        "/lset" => {
            let body = req.body_mut().contents().await?.to_vec();
            let value = if body.is_empty() { p("v").into_bytes() } else { body };
            let ttl = p("ttl").parse::<u64>().unwrap_or(0);
            match local::set(&p("k"), &value, ttl) {
                Ok(()) => text("ok"),
                Err(e) => text(format!("err: {e}")),
            }
        }
        "/lget" => match local::get(&p("k")) {
            Some(v) => Ok(Response::new(Body::from(v))),
            None => status(StatusCode::NOT_FOUND),
        },
        "/ldel" => {
            local::delete(&p("k"));
            text("ok")
        }
        "/get" => match store::get(&p("k")) {
            Some(v) => Ok(Response::new(Body::from(v))),
            None => status(StatusCode::NOT_FOUND),
        },
        _ => status(StatusCode::NOT_FOUND),
    }
}
