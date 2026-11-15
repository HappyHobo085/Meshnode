//! `httpfwd` serving and the forwarder (spec §3.2, §3.3) on small meshes.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::{Mesh, Serve, T, dispatch, fixture, mesh, split, wait_for, wait_kv, wait_ready};
use mesh_core::frame::{read_frame, write_frame};
use mesh_core::{AcceptError, Connection, ProtocolHandler};
use mesh_gateway::{ForwardError, RemoteForwarder};
use mesh_proto::{
    ALPN_HTTPFWD, ForwardRequest, ForwardResponse, HTTPFWD_MAX_FRAME, Limits, NodeId, Placement,
    Role,
};
use mesh_services::{DeployOpts, LocalState};
use tokio::time::timeout;

const COUNTER: &str = "counter";

async fn deploy_counter(mesh: &Mesh, placement: Placement) {
    mesh.svcs(0)
        .deploy_wasm(
            COUNTER,
            Bytes::from(fixture(COUNTER)),
            DeployOpts {
                placement,
                ..DeployOpts::default()
            },
        )
        .await
        .unwrap();
}

/// Waits until node `i` has the manifest (any state but `NotDeployed`).
async fn wait_manifest(mesh: &Mesh, i: usize) {
    let svcs = mesh.svcs(i);
    wait_for("manifest replicated", || async {
        svcs.local_state(COUNTER).await != LocalState::NotDeployed
    })
    .await;
}

/// Waits until node `i` serves the service itself.
async fn wait_active(mesh: &Mesh, i: usize) {
    let svcs = mesh.svcs(i);
    wait_for("service active", || async {
        svcs.local_state(COUNTER).await == LocalState::Active
    })
    .await;
}

fn fwd_req(service: &str) -> ForwardRequest {
    ForwardRequest {
        service: service.into(),
        method: "GET".into(),
        path_and_query: "/".into(),
        headers: vec![(
            "host".into(),
            format!("{service}.mesh.localhost").into_bytes(),
        )],
        body: Vec::new(),
    }
}

/// Sends one raw `httpfwd` request from node `from` to node `to`.
async fn raw_forward(mesh: &Mesh, from: usize, to: usize, req: &ForwardRequest) -> ForwardResponse {
    let conn = mesh.m[from]
        .node
        .handle()
        .connect(mesh.id(to), ALPN_HTTPFWD)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, req).await.unwrap();
    send.finish().unwrap();
    timeout(
        T,
        read_frame::<_, ForwardResponse>(&mut recv, HTTPFWD_MAX_FRAME),
    )
    .await
    .expect("response timed out")
    .unwrap()
    .expect("a response frame")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_to_placed_host() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    deploy_counter(&mesh, Placement::Nodes(vec![mesh.id(0)])).await;
    wait_ready(mesh.svcs(0), COUNTER, 1).await;
    wait_manifest(&mesh, 1).await;
    assert_eq!(
        mesh.svcs(1).local_state(COUNTER).await,
        LocalState::NotPlaced
    );

    // B forwards to A, where the count lives.
    assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await, (200, "1".into()));
    wait_kv(&mesh.m[0].node, COUNTER, "n", "1").await;
    assert_eq!(mesh.served(0), 1);
    assert_eq!(mesh.served(1), 0);
    assert_eq!(mesh.fwd(1).attempts(), 1);
    // The cached connection serves the next forward.
    assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await, (200, "2".into()));
    assert_eq!(mesh.fwd(1).connects(), 1);

    // A serves locally, without forwarding.
    assert_eq!(dispatch(mesh.fwd(0), COUNTER, "/").await, (200, "3".into()));
    assert_eq!(mesh.fwd(0).attempts(), 0);

    // An unknown service is not forwarded.
    assert_eq!(
        dispatch(mesh.fwd(1), "nope", "/").await,
        (404, "Service not deployed".into())
    );
    assert_eq!(mesh.fwd(1).attempts(), 2);

    // Requests that cannot be framed are refused with their status, not forwarded.
    let mut headers = http::Request::get("/");
    for i in 0..129 {
        headers = headers.header(format!("x-{i}"), "v");
    }
    let long = http::Request::get(format!("/{}", "a".repeat(8 * 1024)));
    let big = http::Request::post("/");
    for (req, want) in [
        (headers.body(Bytes::new()).unwrap(), 431),
        (long.body(Bytes::new()).unwrap(), 414),
        (
            big.body(Bytes::from(vec![0u8; HTTPFWD_MAX_FRAME + 1]))
                .unwrap(),
            413,
        ),
    ] {
        let resp = timeout(T, mesh.fwd(1).dispatch(COUNTER, req))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), want);
    }
    assert_eq!(mesh.fwd(1).attempts(), 2);
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_skips_unreachable_then_succeeds() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    deploy_counter(&mesh, Placement::Nodes(vec![mesh.id(0), mesh.id(2)])).await;
    wait_ready(mesh.svcs(0), COUNTER, 1).await;
    wait_ready(mesh.svcs(2), COUNTER, 1).await;
    wait_manifest(&mesh, 1).await;
    split(&mesh, 0, 1).await;

    assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await, (200, "1".into()));
    assert_eq!(mesh.served(0), 0);
    assert_eq!(mesh.served(2), 1);
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_all_fail_502() {
    // A and C are placed but answer the retryable `503 Service not yet replicated`.
    let mesh = mesh(vec![
        (Role::Host, Serve::Unattached),
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Unattached),
    ])
    .await;
    deploy_counter(&mesh, Placement::Nodes(vec![mesh.id(0), mesh.id(2)])).await;
    wait_manifest(&mesh, 1).await;
    assert_eq!(
        dispatch(mesh.fwd(1), COUNTER, "/").await,
        (502, "No host could serve counter".into())
    );
    assert_eq!(mesh.fwd(1).attempts(), 2);
    assert_eq!(mesh.served(0), 1);
    assert_eq!(mesh.served(2), 1);

    // No candidate at all: no host has the tag.
    mesh.svcs(0)
        .set_placement(COUNTER, Placement::Tags(vec!["nowhere".into()]))
        .await
        .unwrap();
    let svcs = mesh.svcs(1);
    wait_for("placement replicated", || async {
        svcs.services()
            .iter()
            .any(|s| s.manifest.placement == Placement::Tags(vec!["nowhere".into()]))
    })
    .await;
    assert_eq!(
        dispatch(mesh.fwd(1), COUNTER, "/").await,
        (502, "No host could serve counter".into())
    );
    assert_eq!(mesh.fwd(1).attempts(), 2);
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarded_request_never_reforwarded() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    deploy_counter(&mesh, Placement::Nodes(vec![mesh.id(2)])).await;
    wait_ready(mesh.svcs(2), COUNTER, 1).await;
    wait_manifest(&mesh, 0).await;

    // A does not run the service: it answers the retryable 503 itself.
    let resp = raw_forward(&mesh, 1, 0, &fwd_req(COUNTER)).await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body, b"Service not placed on this node");
    assert_eq!(mesh.served(0), 1);
    // Nothing went on to C.
    assert_eq!(mesh.served(2), 0);
    assert_eq!(mesh.fwd(0).attempts(), 0);
    assert_eq!(mesh.fwd(0).connects(), 0);
    assert_eq!(
        mesh.m[2].node.store().get(COUNTER, "n").await.unwrap(),
        None
    );
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_role_always_forwards() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Client, Serve::Attached),
    ])
    .await;
    deploy_counter(&mesh, Placement::AllHosts).await;
    wait_ready(mesh.svcs(0), COUNTER, 1).await;
    wait_manifest(&mesh, 1).await;
    assert_ne!(mesh.svcs(1).local_state(COUNTER).await, LocalState::Active);
    assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await, (200, "1".into()));
    assert_eq!(mesh.fwd(1).attempts(), 1);
    assert_eq!(mesh.served(0), 1);
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unattached_handler_answers_retryable_503() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Unattached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    let resp = raw_forward(&mesh, 1, 0, &fwd_req(COUNTER)).await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body, b"Service not yet replicated");
    assert_eq!(mesh.served(0), 1);
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_handler_answers_retryable_503() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    let before = raw_forward(&mesh, 1, 0, &fwd_req(COUNTER)).await;
    assert_ne!(before.body, b"Service not yet replicated");
    mesh.m[0].slot.as_ref().unwrap().detach();
    let resp = raw_forward(&mesh, 1, 0, &fwd_req(COUNTER)).await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body, b"Service not yet replicated");
    mesh.shutdown().await;
}

// ------------------------------------------------------------------- raw test handlers

/// What a [`Raw`] handler answers to every request.
#[derive(Debug)]
enum Answer {
    /// A length prefix just over `HTTPFWD_MAX_FRAME`, then a few bytes.
    Oversized,
    /// A short frame of bytes that do not decode.
    Garbage,
    /// Resets the stream without answering.
    Reset,
    /// Records the request; answers 200 with hop-by-hop headers and `x-custom`.
    Record(Arc<Mutex<Vec<ForwardRequest>>>),
    /// Reads the request and never answers (the stream stays open for a minute).
    Hang,
}

#[derive(Debug)]
struct Raw(Answer, Arc<AtomicU64>);

impl ProtocolHandler for Raw {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let Ok(Some(req)) = read_frame::<_, ForwardRequest>(&mut recv, HTTPFWD_MAX_FRAME).await
            else {
                continue;
            };
            self.1.fetch_add(1, Ordering::SeqCst);
            match &self.0 {
                Answer::Oversized => {
                    let len = u32::try_from(HTTPFWD_MAX_FRAME + 1).unwrap();
                    let _ = send.write_all(&len.to_be_bytes()).await;
                    let _ = send.write_all(&[0u8; 64]).await;
                }
                Answer::Garbage => {
                    let _ = send.write_all(&8u32.to_be_bytes()).await;
                    let _ = send.write_all(&[0xff; 8]).await;
                }
                Answer::Reset => {
                    let _ = send.reset(7u32.into());
                    continue;
                }
                Answer::Hang => {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        drop(send);
                    });
                    continue;
                }
                Answer::Record(log) => {
                    log.lock().unwrap().push(req);
                    let resp = ForwardResponse {
                        status: 200,
                        headers: vec![
                            ("connection".into(), b"close".to_vec()),
                            ("keep-alive".into(), b"timeout=5".to_vec()),
                            ("transfer-encoding".into(), b"chunked".to_vec()),
                            ("upgrade".into(), b"h2c".to_vec()),
                            ("proxy-authenticate".into(), b"Basic".to_vec()),
                            ("x-custom".into(), b"yes".to_vec()),
                        ],
                        body: b"recorded".to_vec(),
                    };
                    let _ = write_frame(&mut send, &resp).await;
                }
            }
            let _ = send.finish();
        }
        Ok(())
    }
}

fn raw(answer: Answer) -> Serve {
    raw_counted(answer).0
}

/// A raw handler and the count of requests it has read.
fn raw_counted(answer: Answer) -> (Serve, Arc<AtomicU64>) {
    let seen = Arc::new(AtomicU64::new(0));
    (Serve::Raw(Arc::new(Raw(answer, seen.clone()))), seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hop_by_hop_headers_stripped() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mesh = mesh(vec![
        (Role::Host, raw(Answer::Record(log.clone()))),
        (Role::Host, Serve::Attached),
    ])
    .await;
    deploy_counter(&mesh, Placement::Nodes(vec![mesh.id(0)])).await;
    wait_manifest(&mesh, 1).await;

    let req = http::Request::post("/x?y=1")
        .header("host", "counter.mesh.localhost")
        .header("connection", "keep-alive, x-trace")
        .header("keep-alive", "timeout=5")
        .header("te", "trailers")
        .header("upgrade", "websocket")
        .header("proxy-authorization", "Basic abc")
        .header("proxy-connection", "keep-alive")
        .header("x-trace", "1")
        .body(Bytes::from_static(b"payload"))
        .unwrap();
    let resp = timeout(T, mesh.fwd(1).dispatch(COUNTER, req))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body().as_ref(), b"recorded");
    for h in [
        "connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "proxy-authenticate",
    ] {
        assert!(resp.headers().get(h).is_none(), "response kept {h}");
    }
    assert_eq!(resp.headers().get("x-custom").unwrap(), "yes");

    let got = log.lock().unwrap().pop().expect("a recorded request");
    assert_eq!(got.service, COUNTER);
    assert_eq!(got.method, "POST");
    assert_eq!(got.path_and_query, "/x?y=1");
    assert_eq!(got.body, b"payload");
    let mut headers = got.headers.clone();
    headers.sort();
    assert_eq!(
        headers,
        [
            ("host".to_string(), b"counter.mesh.localhost".to_vec()),
            ("x-trace".to_string(), b"1".to_vec()),
        ],
        "forwarded headers"
    );
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_oversized_response_rejected() {
    // Review Focus 2: A answers with a length prefix over the cap, C with garbage.
    let mesh = mesh(vec![
        (Role::Host, raw(Answer::Oversized)),
        (Role::Host, Serve::Attached),
        (Role::Host, raw(Answer::Garbage)),
        (Role::Host, Serve::Attached),
    ])
    .await;
    let (a, c, d) = (mesh.id(0), mesh.id(2), mesh.id(3));
    deploy_counter(&mesh, Placement::Nodes(vec![a, c])).await;
    wait_manifest(&mesh, 1).await;
    assert_eq!(
        dispatch(mesh.fwd(1), COUNTER, "/").await,
        (502, "No host could serve counter".into())
    );
    assert_eq!(mesh.fwd(1).attempts(), 2);
    // B is still fine, and a good host tried after a bad one serves.
    for bad in [a, c] {
        set_nodes(&mesh, vec![bad, d]).await;
        wait_active(&mesh, 3).await;
        mesh.fwd(1).try_first(vec![bad, d]);
        let (served, attempts) = (mesh.served(3), mesh.fwd(1).attempts());
        assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await.0, 200);
        assert_eq!(mesh.served(3), served + 1);
        assert_eq!(mesh.fwd(1).attempts(), attempts + 2);
    }
    mesh.shutdown().await;
}

/// Places `counter` on `ids` and waits until node 1 sees it.
async fn set_nodes(mesh: &Mesh, ids: Vec<NodeId>) {
    let want = Placement::Nodes(ids);
    mesh.svcs(0)
        .set_placement(COUNTER, want.clone())
        .await
        .unwrap();
    let svcs = mesh.svcs(1);
    wait_for("placement replicated", || async {
        svcs.services().iter().any(|s| s.manifest.placement == want)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_host_failing_at_stream_level_then_next_serves() {
    // A is online and accepts the connection, then resets the request stream.
    let (bad, seen) = raw_counted(Answer::Reset);
    let mesh = mesh(vec![
        (Role::Host, bad),
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    let (a, c) = (mesh.id(0), mesh.id(2));
    deploy_counter(&mesh, Placement::Nodes(vec![a, c])).await;
    wait_active(&mesh, 2).await;
    wait_manifest(&mesh, 1).await;
    mesh.fwd(1).try_first(vec![a, c]);
    assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await, (200, "1".into()));
    assert_eq!(mesh.fwd(1).attempts(), 2);
    assert_eq!(seen.load(Ordering::SeqCst), 1, "A saw the request");
    assert_eq!(mesh.served(2), 1, "C served it");
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_host_failing_at_connect_then_next_serves() {
    // A is online (sync runs) but serves no `httpfwd`: dialing it fails.
    let mesh = mesh(vec![
        (Role::Host, Serve::Nothing),
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    let (a, c) = (mesh.id(0), mesh.id(2));
    deploy_counter(&mesh, Placement::Nodes(vec![a, c])).await;
    wait_active(&mesh, 2).await;
    wait_manifest(&mesh, 1).await;
    assert!(common::online(&mesh.m[1].node, a));
    mesh.fwd(1).try_first(vec![a, c]);
    assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await, (200, "1".into()));
    assert_eq!(mesh.fwd(1).attempts(), 2);
    assert_eq!(mesh.fwd(1).connects(), 1, "only C was connected");
    assert_eq!(mesh.served(2), 1, "C served it");
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_reports_timeout_once_a_reached_host_never_answers() {
    // Ruling R11: A accepts the request and never answers; C serves no `httpfwd`.
    let mesh = mesh(vec![
        (Role::Host, raw(Answer::Hang)),
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Nothing),
    ])
    .await;
    let (a, c) = (mesh.id(0), mesh.id(2));
    mesh.svcs(0)
        .deploy_wasm(
            COUNTER,
            Bytes::from(fixture(COUNTER)),
            DeployOpts {
                limits: Limits {
                    request_timeout_ms: 100,
                    ..Limits::default()
                },
                placement: Placement::Nodes(vec![a]),
                ..DeployOpts::default()
            },
        )
        .await
        .unwrap();
    wait_manifest(&mesh, 1).await;
    let remote = RemoteForwarder::new(mesh.m[1].node.handle());
    let forward = || async {
        timeout(
            T,
            remote.forward(COUNTER, common::get(COUNTER, "/"), Duration::ZERO),
        )
        .await
        .expect("forward timed out")
    };
    // Every attempt timed out.
    let r = forward().await;
    assert!(matches!(r, Err(ForwardError::Timeout(_))), "{r:?}");
    // The gateway still answers 502.
    assert_eq!(
        dispatch(mesh.fwd(1), COUNTER, "/").await,
        (502, "No host could serve counter".into())
    );

    // One attempt failed to connect, one timed out: a host was reached.
    set_nodes(&mesh, vec![a, c]).await;
    remote.try_first(vec![c, a]);
    let attempts = remote.attempts();
    let r = forward().await;
    assert!(matches!(r, Err(ForwardError::Timeout(_))), "{r:?}");
    assert_eq!(remote.attempts(), attempts + 2);

    // Every attempt failed to connect.
    set_nodes(&mesh, vec![c]).await;
    let r = forward().await;
    assert!(matches!(r, Err(ForwardError::NoHost)), "{r:?}");
    mesh.shutdown().await;
}

// ------------------------------------------------------------ _ping, pause, activity

use mesh_gateway::Activity;
use mesh_proto::PING_SERVICE;

#[test]
fn ping_name_is_not_a_valid_service_name() {
    assert_eq!(PING_SERVICE, "_ping");
    assert!(!mesh_proto::valid_service_name(PING_SERVICE));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_service_answers_204() {
    // No services attached on node 0.
    let mesh = mesh(vec![
        (Role::Host, Serve::Unattached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    let resp = raw_forward(&mesh, 1, 0, &fwd_req(PING_SERVICE)).await;
    assert_eq!(resp.status, 204);
    assert!(resp.body.is_empty());
    // Also detached.
    mesh.m[1].slot.as_ref().unwrap().detach();
    let resp = raw_forward(&mesh, 0, 1, &fwd_req(PING_SERVICE)).await;
    assert_eq!(resp.status, 204);
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_gated_for_non_members() {
    let mesh = mesh(vec![(Role::Host, Serve::Attached)]).await;
    let dx = tempfile::tempdir().unwrap();
    let mut cx = mesh_core::NodeConfig::new(dx.path(), "x");
    cx.relay = mesh_core::RelayChoice::Disabled;
    cx.discovery = false;
    cx.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    let key = ed25519_dalek::SigningKey::from_bytes(&[77; 32]);
    let ep = mesh_core::transport::build_endpoint(&cx, &key)
        .await
        .unwrap();
    let conn = ep
        .connect(mesh.m[0].node.endpoint_addr_for_test(), ALPN_HTTPFWD)
        .await
        .unwrap();
    // The gate closes the connection; no request is ever answered.
    let closed = timeout(T, conn.closed()).await.unwrap();
    match closed {
        iroh::endpoint::ConnectionError::ApplicationClosed(c) => {
            assert_eq!(c.error_code, mesh_core::transport::NOT_MEMBER)
        }
        other => panic!("unexpected close: {other:?}"),
    }
    ep.close().await;
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_answers_503_then_resumes() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    // Served on node 0, so that the request after the resume is answered 200.
    deploy_counter(&mesh, Placement::Nodes(vec![mesh.id(0)])).await;
    wait_ready(mesh.svcs(0), COUNTER, 1).await;
    let slot = mesh.m[0].slot.as_ref().unwrap();
    slot.set_paused(true);
    let resp = raw_forward(&mesh, 1, 0, &fwd_req(COUNTER)).await;
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body, b"Service not yet replicated");
    assert_eq!(
        raw_forward(&mesh, 1, 0, &fwd_req(PING_SERVICE))
            .await
            .status,
        204
    );
    slot.set_paused(false);
    let resp = raw_forward(&mesh, 1, 0, &fwd_req(COUNTER)).await;
    assert_eq!(
        resp.status,
        200,
        "{:?}",
        String::from_utf8_lossy(&resp.body)
    );
    mesh.shutdown().await;
}

/// Opens a stream to node 0, sends `bytes` raw, and finishes the stream.
async fn raw_bytes(mesh: &Mesh, bytes: &[u8]) {
    let conn = mesh.m[1]
        .node
        .handle()
        .connect(mesh.id(0), ALPN_HTTPFWD)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(bytes).await.unwrap();
    send.finish().unwrap();
    // The server answers or resets; either way the stream ends.
    let _ = timeout(T, recv.read_to_end(HTTPFWD_MAX_FRAME)).await;
}

/// The 30 s read/write timeouts are not exercised in real time here: a timed-out stream
/// leaves through the same `ActivityGuard` drop as the error and reset paths below, which
/// are, so `End` follows `Start` there too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_start_end_balanced_on_success_error_and_timeout() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    mesh.m[0]
        .slot
        .as_ref()
        .unwrap()
        .set_activity(Arc::new(move |a| ev.lock().unwrap().push(a)));
    let counts = || {
        let e = events.lock().unwrap();
        let n = |k| e.iter().filter(|a| **a == k).count();
        (n(Activity::Start), n(Activity::End))
    };

    // Success: Start then End.
    assert_eq!(
        raw_forward(&mesh, 1, 0, &fwd_req(PING_SERVICE))
            .await
            .status,
        204
    );
    wait_for("one balanced stream", || async { counts() == (1, 1) }).await;
    assert_eq!(events.lock().unwrap()[0], Activity::Start);

    // Paused 503 is also a stream served.
    mesh.m[0].slot.as_ref().unwrap().set_paused(true);
    raw_forward(&mesh, 1, 0, &fwd_req(COUNTER)).await;
    wait_for("two balanced streams", || async { counts() == (2, 2) }).await;
    mesh.m[0].slot.as_ref().unwrap().set_paused(false);

    // Error: a request that is not convertible resets the stream.
    let mut bad = fwd_req(COUNTER);
    bad.method = "BAD METHOD".into();
    {
        let conn = mesh.m[1]
            .node
            .handle()
            .connect(mesh.id(0), ALPN_HTTPFWD)
            .await
            .unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        write_frame(&mut send, &bad).await.unwrap();
        send.finish().unwrap();
        assert!(
            read_frame::<_, ForwardResponse>(&mut recv, HTTPFWD_MAX_FRAME)
                .await
                .is_err()
        );
    }
    wait_for("three balanced streams", || async { counts() == (3, 3) }).await;

    // Error: a malformed request frame (oversized length prefix).
    raw_bytes(&mesh, &[0xff; 8]).await;
    wait_for("four balanced streams", || async { counts() == (4, 4) }).await;

    // Reset by the peer before a request arrives (the read path).
    {
        let conn = mesh.m[1]
            .node
            .handle()
            .connect(mesh.id(0), ALPN_HTTPFWD)
            .await
            .unwrap();
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        send.write_all(&[0, 0, 0, 9]).await.unwrap();
        send.reset(7u32.into()).unwrap();
    }
    wait_for("five balanced streams", || async { counts() == (5, 5) }).await;
    mesh.shutdown().await;
}

/// Sub-spec 8a §3 / ruling R8 (M3): a host whose store is not a full replica yet (a client
/// upgrading to host, still Partial or re-bootstrapping) forwards instead of serving from
/// it, although it is placed and active.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_store_forwards_although_listed_as_host() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
    ])
    .await;
    deploy_counter(&mesh, Placement::AllHosts).await;
    wait_ready(mesh.svcs(0), COUNTER, 1).await;
    wait_active(&mesh, 1).await;
    // Test hook: B's store is Partial (as while an upgrade has not rebuilt it).
    mesh.m[1]
        .node
        .store()
        .inner()
        .set_scope(mesh_proto::Scope::client());
    assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await, (200, "1".into()));
    assert_eq!(mesh.fwd(1).attempts(), 1, "forwarded, not served from B");
    assert_eq!(mesh.served(0), 1);
    mesh.shutdown().await;
}

/// Ruling R9: a host whose store is not a full replica (here Partial, as for a client
/// re-invited as a host before its rebuild) answers forwarded requests with the retryable
/// `503 Node re-bootstrapping`, not from its store, and the forwarder fails over to the
/// next host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarder_fails_over_from_a_host_that_is_not_full() {
    let mesh = mesh(vec![
        (Role::Host, Serve::Attached),
        (Role::Host, Serve::Attached),
        (Role::Client, Serve::Attached),
    ])
    .await;
    deploy_counter(&mesh, Placement::AllHosts).await;
    wait_ready(mesh.svcs(0), COUNTER, 1).await;
    wait_active(&mesh, 1).await;
    wait_manifest(&mesh, 2).await;
    // Test hook: B's store is Partial.
    mesh.m[1]
        .node
        .store()
        .inner()
        .set_scope(mesh_proto::Scope::client());
    mesh.fwd(2).try_first(vec![mesh.id(1), mesh.id(0)]);
    assert_eq!(dispatch(mesh.fwd(2), COUNTER, "/").await, (200, "1".into()));
    assert_eq!(mesh.served(1), 1, "B answered (with the retryable 503)");
    assert_eq!(mesh.served(0), 1, "A served it");
    assert_eq!(mesh.fwd(2).attempts(), 2);
    wait_kv(&mesh.m[0].node, COUNTER, "n", "1").await;
    mesh.shutdown().await;
}
