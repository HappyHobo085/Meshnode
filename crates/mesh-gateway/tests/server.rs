//! The localhost server (spec §4.1-§4.3): routing, port mappings, limits, status page.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::{Mesh, Serve, T, fixture, mesh, mesh_named, wait_for};
use mesh_gateway::{Gateway, GatewayConfig, GatewayError};
use mesh_proto::{Placement, Role};
use mesh_services::{DeployOpts, LocalState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const COUNTER: &str = "counter";
const LOOPBACK0: &str = "127.0.0.1:0";

/// A parsed HTTP/1.1 response.
struct Resp {
    status: u16,
    head: String,
    body: String,
}

/// Sends `raw` and reads the response until the server closes.
async fn raw(addr: SocketAddr, raw: &str) -> Resp {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    read_resp(&mut s).await
}

async fn read_resp(s: &mut TcpStream) -> Resp {
    let mut buf = Vec::new();
    let _ = timeout(T, s.read_to_end(&mut buf))
        .await
        .expect("response timed out");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a response head");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Resp {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

async fn request(addr: SocketAddr, method: &str, host: &str, path: &str) -> Resp {
    raw(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

fn cfg(ports: &[(u16, &str)]) -> GatewayConfig {
    GatewayConfig {
        listen: LOOPBACK0.parse().unwrap(),
        ports: ports
            .iter()
            .map(|(p, s)| (*p, s.to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

async fn start_with(mesh: &Mesh, cfg: GatewayConfig) -> Result<Gateway, GatewayError> {
    Gateway::start(
        cfg,
        mesh.fwd(0).clone(),
        mesh.m[0].node.handle(),
        mesh.m[0].svcs.clone(),
    )
    .await
}

async fn start(mesh: &Mesh, cfg: GatewayConfig) -> Gateway {
    start_with(mesh, cfg).await.unwrap()
}

fn main_addr(g: &Gateway) -> SocketAddr {
    g.local_addrs().iter().find(|(_, s)| s.is_none()).unwrap().0
}

fn mapped_addr(g: &Gateway) -> SocketAddr {
    g.local_addrs().iter().find(|(_, s)| s.is_some()).unwrap().0
}

async fn with_counter(founder: &str) -> Mesh {
    let mesh = mesh_named(founder, vec![(Role::Host, Serve::Attached)]).await;
    mesh.svcs(0)
        .deploy_wasm(
            COUNTER,
            Bytes::from(fixture(COUNTER)),
            DeployOpts {
                placement: Placement::Nodes(vec![mesh.id(0)]),
                ..DeployOpts::default()
            },
        )
        .await
        .unwrap();
    let svcs = mesh.m[0].svcs.clone();
    wait_for("service active", || async {
        svcs.local_state(COUNTER).await == LocalState::Active
    })
    .await;
    mesh
}

async fn finish(g: Gateway, mesh: Mesh) {
    timeout(T, g.shutdown()).await.expect("gateway shutdown");
    mesh.shutdown().await;
}

#[tokio::test]
async fn host_routing_to_local_service() {
    let mesh = with_counter("n0").await;
    let g = start(&mesh, cfg(&[])).await;
    let a = main_addr(&g);
    let r = request(a, "GET", "counter.mesh.localhost", "/").await;
    assert_eq!((r.status, r.body.as_str()), (200, "1"));
    // Case and port are ignored.
    let r = request(a, "GET", "COUNTER.Mesh.Localhost:7777", "/").await;
    assert_eq!((r.status, r.body.as_str()), (200, "2"));
    // A service that is not deployed is the forwarder's 404.
    let r = request(a, "GET", "nope.mesh.localhost", "/").await;
    assert_eq!((r.status, r.body.as_str()), (404, "Service not deployed"));
    finish(g, mesh).await;
}

#[tokio::test]
async fn unknown_host_404() {
    let mesh = mesh(vec![(Role::Host, Serve::Attached)]).await;
    let g = start(&mesh, cfg(&[])).await;
    let a = main_addr(&g);
    for host in ["example.com", "evil.localhost", "counter.example.com"] {
        let r = request(a, "GET", host, "/").await;
        assert_eq!((r.status, r.body.as_str()), (404, "Unknown host"), "{host}");
    }
    // No Host header at all.
    let r = raw(a, "GET / HTTP/1.1\r\nConnection: close\r\n\r\n").await;
    assert_eq!(r.status, 400);
    finish(g, mesh).await;
}

#[tokio::test]
async fn invalid_service_host_404() {
    let mesh = mesh(vec![(Role::Host, Serve::Attached)]).await;
    let g = start(&mesh, cfg(&[])).await;
    let a = main_addr(&g);
    let long = format!("{}.mesh.localhost", "a".repeat(300));
    let thirty_three = format!("{}.mesh.localhost", "a".repeat(33));
    for host in [
        "Foo_Bar.mesh.localhost",
        "a..mesh.localhost",
        ".mesh.localhost",
        "a.b.mesh.localhost",
        "a.mesh.localhost:80x",
        long.as_str(),
        thirty_three.as_str(),
    ] {
        let r = request(a, "GET", host, "/").await;
        assert_eq!((r.status, r.body.as_str()), (404, "Unknown host"), "{host}");
    }
    finish(g, mesh).await;
}

#[tokio::test]
async fn status_page_and_json() {
    let mesh = with_counter("<script>alert(1)</script>").await;
    let g = start(&mesh, cfg(&[(0, COUNTER)])).await;
    let a = main_addr(&g);
    for host in [
        "mesh.localhost",
        "localhost:7777",
        "127.0.0.1",
        "[::1]:7777",
    ] {
        let r = request(a, "GET", host, "/").await;
        assert_eq!(r.status, 200, "{host}");
        assert!(r.head.to_lowercase().contains("text/html"));
        assert!(!r.head.to_lowercase().contains("access-control"));
        assert!(!r.body.contains("<script>alert"), "unescaped name");
        assert!(r.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(r.body.contains(COUNTER));
    }
    let r = request(a, "GET", "mesh.localhost", "/status.json").await;
    assert_eq!(r.status, 200);
    assert!(r.head.to_lowercase().contains("application/json"));
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["members"][0]["name"], "<script>alert(1)</script>");
    assert_eq!(v["services"][0]["name"], COUNTER);
    assert_eq!(v["node"]["status"], "Running");
    // Other paths 404; writes are refused and change nothing.
    assert_eq!(
        request(a, "GET", "mesh.localhost", "/other").await.status,
        404
    );
    for m in ["POST", "PUT", "DELETE"] {
        let r = request(a, m, "mesh.localhost", "/").await;
        assert_eq!(r.status, 405, "{m}");
    }
    assert_eq!(request(a, "HEAD", "mesh.localhost", "/").await.status, 200);
    // The counter was not touched by any of this.
    let r = request(a, "GET", "counter.mesh.localhost", "/").await;
    assert_eq!(r.body, "1");
    finish(g, mesh).await;
}

#[tokio::test]
async fn mapped_port_serves_service() {
    let mesh = with_counter("n0").await;
    let g = start(&mesh, cfg(&[(0, COUNTER)])).await;
    let m = mapped_addr(&g);
    for (i, host) in ["127.0.0.1", "localhost:9", "[::1]", "LOCALHOST"]
        .iter()
        .enumerate()
    {
        let r = request(m, "GET", host, "/").await;
        assert_eq!((r.status, r.body), (200, (i + 1).to_string()), "{host}");
    }
    finish(g, mesh).await;
}

#[tokio::test]
async fn mapped_port_rejects_foreign_host_421() {
    let mesh = with_counter("n0").await;
    let g = start(&mesh, cfg(&[(0, COUNTER)])).await;
    let m = mapped_addr(&g);
    for host in [
        "evil.example.com",
        "counter.mesh.localhost",
        "mesh.localhost",
    ] {
        let r = request(m, "GET", host, "/").await;
        assert_eq!(r.status, 421, "{host}");
    }
    // The service was never reached.
    assert_eq!(request(m, "GET", "127.0.0.1", "/").await.body, "1");
    finish(g, mesh).await;
}

#[tokio::test]
async fn oversized_body_413() {
    let mesh = with_counter("n0").await;
    let g = start(&mesh, cfg(&[])).await;
    let a = main_addr(&g);
    let over = 64 * 1024 * 1024 + 1;
    // Declared too large: refused without reading the body.
    let r = raw(
        a,
        &format!(
            "POST / HTTP/1.1\r\nHost: counter.mesh.localhost\r\nContent-Length: {over}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(r.status, 413);
    // Chunked, no declared length: cut off while reading.
    let mut s = TcpStream::connect(a).await.unwrap();
    s.write_all(
        b"POST / HTTP/1.1\r\nHost: counter.mesh.localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let (mut rd, mut wr) = s.into_split();
    let writer = tokio::spawn(async move {
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..65 {
            if wr.write_all(b"100000\r\n").await.is_err()
                || wr.write_all(&chunk).await.is_err()
                || wr.write_all(b"\r\n").await.is_err()
            {
                return;
            }
        }
        let _ = wr.write_all(b"0\r\n\r\n").await;
        // Keep the write half open until the reader is done.
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut buf = Vec::new();
    let _ = timeout(T, async {
        let mut tmp = [0u8; 1024];
        loop {
            match rd.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
    })
    .await;
    writer.abort();
    let head = String::from_utf8_lossy(&buf);
    assert!(head.starts_with("HTTP/1.1 413"), "got {head:?}");
    // The server is still fine.
    let r = request(a, "GET", "counter.mesh.localhost", "/").await;
    assert_eq!(r.status, 200);
    finish(g, mesh).await;
}

#[tokio::test]
async fn non_loopback_listen_rejected() {
    let mesh = mesh(vec![(Role::Host, Serve::Attached)]).await;
    for bad in ["0.0.0.0:0", "192.168.1.5:7777", "[::]:0"] {
        let c = GatewayConfig {
            listen: bad.parse().unwrap(),
            ports: BTreeMap::new(),
        };
        assert!(c.validate().is_err(), "{bad}");
        assert!(start_with(&mesh, c).await.is_err(), "{bad}");
    }
    assert!(cfg(&[(7801, "Bad_Name")]).validate().is_err());
    assert!(cfg(&[(7801, "")]).validate().is_err());
    assert!(cfg(&[(7801, "sync")]).validate().is_ok());
    assert!(GatewayConfig::default().validate().is_ok());
    assert_eq!(
        GatewayConfig::default().listen.to_string(),
        "127.0.0.1:7777"
    );
    assert_eq!(GatewayConfig::default().ports.get(&7801).unwrap(), "sync");
    mesh.shutdown().await;
}

#[tokio::test]
async fn mapped_port_bind_failure_skipped() {
    let mesh = with_counter("n0").await;
    let taken = TcpListener::bind(LOOPBACK0).await.unwrap();
    let port = taken.local_addr().unwrap().port();
    let g = start(&mesh, cfg(&[(port, COUNTER)])).await;
    assert_eq!(g.local_addrs().len(), 1);
    let r = request(main_addr(&g), "GET", "counter.mesh.localhost", "/").await;
    assert_eq!(r.status, 200);
    finish(g, mesh).await;
    drop(taken);
}

#[tokio::test]
async fn main_port_in_use_fails() {
    let mesh = mesh(vec![(Role::Host, Serve::Attached)]).await;
    let taken = TcpListener::bind(LOOPBACK0).await.unwrap();
    let addr = taken.local_addr().unwrap();
    let c = GatewayConfig {
        listen: addr,
        ports: BTreeMap::new(),
    };
    let err = start_with(&mesh, c).await.err().expect("bind must fail");
    assert!(matches!(&err, GatewayError::Bind { addr: a, .. } if *a == addr));
    assert!(err.to_string().contains(&addr.to_string()), "{err}");
    mesh.shutdown().await;
}

#[tokio::test]
async fn shutdown_releases_services() {
    let mesh = with_counter("n0").await;
    let before = Arc::strong_count(&mesh.m[0].svcs);
    let g = start(&mesh, cfg(&[(0, COUNTER)])).await;
    // An idle keep-alive connection must not hold shutdown up.
    let _idle = TcpStream::connect(main_addr(&g)).await.unwrap();
    timeout(T, g.shutdown()).await.expect("gateway shutdown");
    assert_eq!(Arc::strong_count(&mesh.m[0].svcs), before);
    mesh.shutdown().await;
}
