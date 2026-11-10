//! `httpfwd` serving and the forwarder (spec §3.2, §3.3) on small meshes.

mod common;

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use common::{Mesh, Serve, T, dispatch, fixture, mesh, split, wait_for, wait_kv, wait_ready};
use mesh_core::frame::{read_frame, write_frame};
use mesh_core::{AcceptError, Connection, ProtocolHandler};
use mesh_proto::{
    ALPN_HTTPFWD, ForwardRequest, ForwardResponse, HTTPFWD_MAX_FRAME, NodeId, Placement, Role,
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

// ------------------------------------------------------------------- raw test handlers

/// What a [`Raw`] handler answers to every request.
#[derive(Debug)]
enum Answer {
    /// A length prefix just over `HTTPFWD_MAX_FRAME`, then a few bytes.
    Oversized,
    /// A short frame of bytes that do not decode.
    Garbage,
    /// Records the request; answers 200 with hop-by-hop headers and `x-custom`.
    Record(Arc<Mutex<Vec<ForwardRequest>>>),
}

#[derive(Debug)]
struct Raw(Answer);

impl ProtocolHandler for Raw {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let Ok(Some(req)) = read_frame::<_, ForwardRequest>(&mut recv, HTTPFWD_MAX_FRAME).await
            else {
                continue;
            };
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
    Serve::Raw(Arc::new(Raw(answer)))
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
    // B is still fine, and a good host next to a bad one serves.
    for bad in [a, c] {
        set_nodes(&mesh, vec![bad, d]).await;
        wait_active(&mesh, 3).await;
        let before = mesh.served(3);
        assert_eq!(dispatch(mesh.fwd(1), COUNTER, "/").await.0, 200);
        assert_eq!(mesh.served(3), before + 1);
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
