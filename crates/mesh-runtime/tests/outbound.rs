mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::fixture;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::{Loaded, MemKv, RequestCtx, Runtime, RuntimeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A minimal HTTP/1.1 responder: every request gets `200` with body `pong`.
async fn responder() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\npong",
                    )
                    .await;
                // Close politely: unread request bytes would turn the close into a reset.
                let _ = sock.shutdown().await;
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    tokio::io::copy(&mut sock, &mut tokio::io::sink()),
                )
                .await;
            });
        }
    });
    port
}

async fn fetch(rt: &Runtime, svc: &Loaded, target: &str, allow: &[&str]) -> String {
    let ctx = RequestCtx {
        service: "outbound".to_string(),
        node_id: NodeId([7; 32]),
        kv: Arc::new(MemKv::default()),
        limits: Limits::default(),
        outbound: allow.iter().map(|s| s.to_string()).collect(),
    };
    let req = http::Request::get(format!("http://outbound.mesh.localhost/?url={target}"))
        .body(Bytes::new())
        .unwrap();
    let resp = rt.handle(svc, req, ctx).await;
    assert_eq!(resp.status(), 200);
    String::from_utf8_lossy(resp.body()).into_owned()
}

fn assert_denied(text: &str) {
    assert!(text.starts_with("error:"), "{text}");
    assert!(text.to_lowercase().contains("denied"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn outbound_allowed_vs_denied() {
    let port = responder().await;
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let svc = rt.load([5; 32], &fixture("outbound")).await.unwrap();
    let ip = format!("http://127.0.0.1:{port}/");
    let localhost = format!("http://localhost:{port}/");

    // Allowed: the upstream response is relayed.
    let allow = format!("127.0.0.1:{port}");
    assert_eq!(fetch(&rt, &svc, &ip, &[&allow]).await, "200 pong");
    // An empty allowlist denies everything.
    assert_denied(&fetch(&rt, &svc, &ip, &[]).await);
    // The name and the address are different destinations.
    assert_denied(&fetch(&rt, &svc, &localhost, &[&allow]).await);
    // So are different ports, and an entry without a port never matches an explicit one.
    let other = format!("127.0.0.1:{}", port.wrapping_add(1));
    assert_denied(&fetch(&rt, &svc, &ip, &[&other]).await);
    assert_denied(&fetch(&rt, &svc, &ip, &["127.0.0.1"]).await);
}
