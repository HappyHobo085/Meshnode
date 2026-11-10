//! The native static-site handler (spec §6), end to end through `Services::handle`.

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::{T, test_cfg, wait_until};
use http::{Method, Response, header};
use mesh_core::Node;
use mesh_proto::Role;
use mesh_services::{DeployOpts, LocalStatus, Services, ServicesConfig};
use tempfile::TempDir;
use tokio::time::timeout;

const SITE: &str = "site";

fn write_site(dir: &std::path::Path) {
    let files: &[(&str, &[u8])] = &[
        ("index.html", b"<h1>root</h1>"),
        ("docs/index.html", b"<h1>docs</h1>"),
        ("app.js", b"console.log(1)"),
        ("img/x.png", b"\x89PNG"),
        ("data.json", b"{}"),
        ("logo.svg", b"<svg/>"),
        ("notes.txt", b"hi"),
        ("blob.bin", b"\x00\x01"),
        ("empty/readme.txt", b"r"),
        ("we ird%/x.txt", b"w"),
    ];
    for (name, data) in files {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
}

async fn start(node: &Node) -> Services {
    Services::start(node, ServicesConfig::default())
        .await
        .unwrap()
}

async fn ready(svc: &Services) {
    wait_until("site ready", T, || {
        svc.services()
            .iter()
            .any(|s| s.manifest.name == SITE && s.local == LocalStatus::Ready { version: 1 })
    })
    .await;
}

async fn req(
    svc: &Services,
    method: Method,
    target: &str,
    headers: &[(&str, &str)],
) -> Response<Bytes> {
    let mut b = http::Request::builder()
        .method(method)
        .uri(format!("http://{SITE}.mesh.localhost{target}"));
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    svc.handle(SITE, b.body(Bytes::new()).unwrap()).await
}

fn hdr<'a>(r: &'a Response<Bytes>, name: &str) -> Option<&'a str> {
    r.headers().get(name).map(|v| v.to_str().unwrap())
}

struct One {
    node: Node,
    svc: Services,
    _site: TempDir,
    _dir: TempDir,
}

async fn one() -> One {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::create_mesh(test_cfg(dir.path(), "n0")).await.unwrap();
    let svc = start(&node).await;
    let site = tempfile::tempdir().unwrap();
    write_site(site.path());
    svc.deploy_static(SITE, site.path(), DeployOpts::default())
        .await
        .unwrap();
    ready(&svc).await;
    One {
        node,
        svc,
        _site: site,
        _dir: dir,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn static_site_index_etag_304_404_traversal_405_redirect() {
    let o = one().await;
    let s = &o.svc;

    let r = req(s, Method::GET, "/", &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.body().as_ref(), b"<h1>root</h1>");
    assert_eq!(hdr(&r, "content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(hdr(&r, "cache-control"), Some("no-cache"));
    let etag = hdr(&r, "etag").unwrap().to_owned();
    assert!(etag.starts_with('"') && etag.ends_with('"') && etag.len() == 66);

    let r = req(s, Method::GET, "/docs/", &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.body().as_ref(), b"<h1>docs</h1>");
    assert_ne!(hdr(&r, "etag"), Some(etag.as_str()));

    // A directory without a trailing slash redirects, keeping the query.
    let r = req(s, Method::GET, "/docs", &[]).await;
    assert_eq!(r.status(), 301);
    assert_eq!(hdr(&r, "location"), Some("/docs/"));
    let r = req(s, Method::GET, "/img?v=1&a=b", &[]).await;
    assert_eq!(r.status(), 301);
    assert_eq!(hdr(&r, "location"), Some("/img/?v=1&a=b"));
    // The target is rebuilt from the normalised path: never protocol-relative.
    let r = req(s, Method::GET, "//docs", &[]).await;
    assert_eq!(r.status(), 301);
    assert_eq!(hdr(&r, "location"), Some("/docs/"));
    let r = req(s, Method::GET, "/we%20ird%25?q=1", &[]).await;
    assert_eq!(r.status(), 301);
    assert_eq!(hdr(&r, "location"), Some("/we%20ird%25/?q=1"));
    let r = req(s, Method::GET, "/we%20ird%25/x.txt", &[]).await;
    assert_eq!(r.body().as_ref(), b"w");
    // A directory with no index file is a 404.
    assert_eq!(req(s, Method::GET, "/empty/", &[]).await.status(), 404);

    for (path, ct) in [
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/img/x.png", "image/png"),
        ("/data.json", "application/json; charset=utf-8"),
        ("/logo.svg", "image/svg+xml; charset=utf-8"),
        ("/notes.txt", "text/plain; charset=utf-8"),
        ("/blob.bin", "application/octet-stream"),
    ] {
        let r = req(s, Method::GET, path, &[]).await;
        assert_eq!(r.status(), 200, "{path}");
        assert_eq!(hdr(&r, "content-type"), Some(ct), "{path}");
        assert!(hdr(&r, "etag").is_some(), "{path}");
    }

    // Percent-decoding happens once and `//` collapses.
    let r = req(s, Method::GET, "/img//x%2Epng", &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.body().as_ref(), b"\x89PNG");

    // Conditional requests.
    for inm in [
        etag.clone(),
        "*".to_owned(),
        format!("\"nope\", {etag}"),
        format!("W/{etag}"),
    ] {
        let r = req(s, Method::GET, "/", &[("if-none-match", &inm)]).await;
        assert_eq!(r.status(), 304, "{inm}");
        assert!(r.body().is_empty());
        assert_eq!(hdr(&r, "etag"), Some(etag.as_str()));
    }
    let r = req(s, Method::GET, "/", &[("if-none-match", "\"nope\"")]).await;
    assert_eq!(r.status(), 200);

    assert_eq!(req(s, Method::GET, "/nope", &[]).await.status(), 404);
    assert_eq!(
        req(s, Method::GET, "/docs/nope.html", &[]).await.status(),
        404
    );

    let r = req(s, Method::POST, "/", &[]).await;
    assert_eq!(r.status(), 405);
    assert_eq!(hdr(&r, "allow"), Some("GET, HEAD"));
    assert_eq!(req(s, Method::DELETE, "/app.js", &[]).await.status(), 405);

    let g = req(s, Method::GET, "/", &[]).await;
    let h = req(s, Method::HEAD, "/", &[]).await;
    assert_eq!(h.status(), 200);
    assert!(h.body().is_empty());
    for name in [header::CONTENT_TYPE, header::ETAG, header::CACHE_CONTROL] {
        assert_eq!(h.headers().get(&name), g.headers().get(&name));
    }
    o.svc.shutdown().await;
    o.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encoded_traversal_rejected() {
    let o = one().await;
    for path in [
        "/%2e%2e/x",
        "/%2E%2E/x",
        "/a/..%2Fb",
        "/a/%2e%2e%2fb",
        "/..%5cx",
        "/a%5Cb",
        "/a%00b",
        "/../x",
        "/a/./b",
        "/a/..",
        "/a%2Fb",
        "/%ff",
        "/a%zz",
    ] {
        let r = req(&o.svc, Method::GET, path, &[]).await;
        assert_eq!(r.status(), 400, "{path}");
    }
    // The method is checked before the path.
    assert_eq!(
        req(&o.svc, Method::POST, "/%2e%2e/x", &[]).await.status(),
        405
    );
    o.svc.shutdown().await;
    o.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_static_on_a_served_on_b() {
    let da = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(da.path(), "n0")).await.unwrap();
    let db = tempfile::tempdir().unwrap();
    let ticket = a
        .invite(Role::Host, Duration::from_secs(900))
        .await
        .unwrap();
    let b = timeout(T, Node::join(test_cfg(db.path(), "n1"), ticket))
        .await
        .expect("join timed out")
        .unwrap();
    assert!(b.wait_first_sync(T).await);
    let (sa, sb) = (start(&a).await, start(&b).await);

    let site = tempfile::tempdir().unwrap();
    write_site(site.path());
    sa.deploy_static(SITE, site.path(), DeployOpts::default())
        .await
        .unwrap();
    ready(&sa).await;
    ready(&sb).await;

    for svc in [&sa, &sb] {
        let r = req(svc, Method::GET, "/docs/", &[]).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.body().as_ref(), b"<h1>docs</h1>");
        let r = req(svc, Method::GET, "/app.js", &[]).await;
        assert_eq!(r.body().as_ref(), b"console.log(1)");
    }
    let (ra, rb) = (
        req(&sa, Method::GET, "/", &[]).await,
        req(&sb, Method::GET, "/", &[]).await,
    );
    assert_eq!(hdr(&ra, "etag"), hdr(&rb, "etag"));
    sa.shutdown().await;
    sb.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
}
