mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::fixture;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::{MemKv, RequestCtx, Runtime, RuntimeConfig, RuntimeError};

fn ctx(service: &str) -> RequestCtx {
    RequestCtx {
        service: service.to_string(),
        node_id: NodeId([7; 32]),
        kv: Arc::new(MemKv::default()),
        limits: Limits::default(),
        outbound: Vec::new(),
    }
}

fn post(uri: &str, body: &'static [u8]) -> http::Request<Bytes> {
    http::Request::post(uri)
        .body(Bytes::from_static(body))
        .unwrap()
}

fn get(uri: &str) -> http::Request<Bytes> {
    http::Request::get(uri).body(Bytes::new()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_round_trip() {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let echo = rt.load([1; 32], &fixture("echo")).await.unwrap();

    let resp = rt
        .handle(
            &echo,
            post("http://echo.mesh.localhost/", b"hello, mesh"),
            ctx("echo"),
        )
        .await;
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(resp.body().as_ref(), b"hello, mesh");

    // A second request runs in a fresh instance from the same `Loaded`.
    let resp = rt
        .handle(
            &echo,
            get("http://echo.mesh.localhost/?size=100000"),
            ctx("echo"),
        )
        .await;
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(resp.body().len(), 100_000);
    assert!(resp.body().iter().all(|&b| b == b'a'));

    // A path-only request without a Host header is served too.
    let resp = rt.handle(&echo, post("/", b"no host"), ctx("echo")).await;
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(resp.body().as_ref(), b"no host");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_component_rejected_at_load() {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    match rt.load([2; 32], &fixture("bad")).await {
        Err(RuntimeError::Invalid(msg)) => assert!(msg.contains("wasi:filesystem"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn garbage_rejected_at_load() {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let core_module: &[u8] = b"\0asm\x01\0\0\0";
    for (i, bytes) in [&b""[..], core_module].into_iter().enumerate() {
        match rt.load([3 + i as u8; 32], bytes).await {
            Err(RuntimeError::Invalid(_) | RuntimeError::Compile(_)) => {}
            other => panic!("input {i}: expected Invalid or Compile, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn component_without_handler_rejected_at_load() {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    match rt.load([5; 32], b"(component)").await {
        Err(RuntimeError::Invalid(msg)) => assert!(msg.contains("incoming-handler"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}
