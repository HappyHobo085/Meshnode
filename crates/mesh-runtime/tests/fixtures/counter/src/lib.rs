//! Exercises every `meshnode:kv/store` call through HTTP routes. Values come from the
//! query string (`v`, `m`, ...) or, for `/set`, from the request body when it is not empty.
use wstd::http::{Body, Error, Request, Response, StatusCode};

wit_bindgen::generate!({
    inline: "package local:counter; world counter { import meshnode:kv/store; }",
    path: "../../../wit",
    generate_all,
});

use meshnode::kv::store;

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

fn limit(query: &str) -> u32 {
    param(query, "limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(100)
}

#[wstd::http_server]
async fn main(mut req: Request<Body>) -> Result<Response<Body>, Error> {
    let path = req.uri().path().to_string();
    let q = req.uri().query().unwrap_or("").to_string();
    let p = |name: &str| param(&q, name).unwrap_or("").to_string();
    match path.as_str() {
        "/" => {
            let n = store::get("n")
                .map(|v| String::from_utf8_lossy(&v).parse::<u64>().unwrap_or(0))
                .unwrap_or(0)
                + 1;
            store::set("n", n.to_string().as_bytes());
            text(n.to_string())
        }
        "/set" => {
            let body = req.body_mut().contents().await?.to_vec();
            let value = if body.is_empty() { p("v").into_bytes() } else { body };
            store::set(&p("k"), &value);
            text("ok")
        }
        "/get" => match store::get(&p("k")) {
            Some(v) => Ok(Response::new(Body::from(v))),
            None => status(StatusCode::NOT_FOUND),
        },
        "/del" => {
            store::delete(&p("k"));
            text("ok")
        }
        "/list" => {
            let after = param(&q, "after");
            text(store::list_keys(&p("prefix"), after, limit(&q)).join("\n"))
        }
        "/ordered" => {
            let value = param(&q, "v").map(|v| v.as_bytes().to_vec());
            store::set_ordered(&p("k"), &[p("o").parse::<u8>().unwrap_or(0)], value.as_deref());
            text("ok")
        }
        "/setadd" => {
            store::set_add(&p("s"), p("m").as_bytes());
            text("ok")
        }
        "/contains" => text(store::set_contains(&p("s"), p("m").as_bytes()).to_string()),
        "/members" => {
            let after = param(&q, "after").map(|a| a.as_bytes().to_vec());
            let members = store::set_members(&p("s"), after.as_deref(), limit(&q));
            let lines: Vec<String> = members
                .iter()
                .map(|m| String::from_utf8_lossy(m).into_owned())
                .collect();
            text(lines.join("\n"))
        }
        "/nodeid" => text(
            store::node_id()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        ),
        _ => status(StatusCode::NOT_FOUND),
    }
}
