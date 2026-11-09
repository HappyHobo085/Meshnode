//! Per-request limits: CPU and wall time, memory, request and response bodies, and the
//! in-flight cap. Timing assertions are generous upper bounds, never exact durations.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::fixture;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::{KvBackend, KvError, Loaded, MemKv, RequestCtx, Runtime, RuntimeConfig};

fn ctx(limits: Limits) -> RequestCtx {
    ctx_with_kv(limits, Arc::new(MemKv::default()))
}

fn ctx_with_kv(limits: Limits, kv: Arc<dyn KvBackend>) -> RequestCtx {
    RequestCtx {
        service: "limits".to_string(),
        node_id: NodeId([7; 32]),
        kv,
        limits,
        outbound: Vec::new(),
    }
}

fn timeout_ms(ms: u64) -> Limits {
    Limits {
        request_timeout_ms: ms,
        ..Limits::default()
    }
}

fn get(path: &str) -> http::Request<Bytes> {
    http::Request::get(format!("http://limits.mesh.localhost{path}"))
        .body(Bytes::new())
        .unwrap()
}

fn post(body: Vec<u8>) -> http::Request<Bytes> {
    http::Request::post("http://limits.mesh.localhost/")
        .body(Bytes::from(body))
        .unwrap()
}

fn runtime(max_in_flight: usize) -> Runtime {
    Runtime::new(RuntimeConfig {
        max_in_flight,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

async fn load(rt: &Runtime, name: &str, id: u8) -> Loaded {
    rt.load([id; 32], &fixture(name)).await.unwrap()
}

/// Serves one request, failing the test (instead of hanging it) if no response arrives
/// within `bound`. Returns the status and how long it took.
async fn serve_within(
    rt: &Runtime,
    svc: &Loaded,
    req: http::Request<Bytes>,
    ctx: RequestCtx,
    bound: Duration,
) -> (http::StatusCode, Duration) {
    let start = Instant::now();
    let resp = tokio::time::timeout(bound, rt.handle(svc, req, ctx))
        .await
        .unwrap_or_else(|_| panic!("no response within {bound:?}"));
    (resp.status(), start.elapsed())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_gives_504_and_node_survives() {
    // One slot: the follow-up request is only served once the spinning guest has stopped.
    let rt = runtime(1);
    let spin = load(&rt, "spin", 1).await;
    let echo = load(&rt, "echo", 2).await;

    let (status, took) = serve_within(
        &rt,
        &spin,
        get("/"),
        ctx(timeout_ms(200)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    assert!(took < Duration::from_secs(1), "504 took {took:?}");

    let (status, _) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(timeout_ms(2_000)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}

/// Epoch interruption alone stops a spinning guest: on a single-threaded runtime the
/// spinning guest holds the only thread, so the outer `tokio::time::timeout` cannot fire.
#[test]
fn epoch_deadline_interrupts_spin_on_one_thread() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let status = tokio.block_on(async {
            let rt = runtime(4);
            let spin = load(&rt, "spin", 1).await;
            rt.handle(&spin, get("/"), ctx(timeout_ms(200)))
                .await
                .status()
        });
        let _ = tx.send(status);
    });
    let status = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("spinning guest was not interrupted");
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
}

/// A backend whose every call never completes: the guest waits in a host call, where
/// epoch interruption cannot reach it, so only the outer timeout ends the request.
struct StalledKv;

#[async_trait::async_trait]
impl KvBackend for StalledKv {
    async fn get(&self, _: &str) -> Result<Option<Vec<u8>>, KvError> {
        std::future::pending().await
    }
    async fn set(&self, _: &str, _: Vec<u8>) -> Result<(), KvError> {
        std::future::pending().await
    }
    async fn set_ordered(&self, _: &str, _: Vec<u8>, _: Option<Vec<u8>>) -> Result<(), KvError> {
        std::future::pending().await
    }
    async fn delete(&self, _: &str) -> Result<(), KvError> {
        std::future::pending().await
    }
    async fn list_keys(&self, _: &str, _: Option<&str>, _: u32) -> Result<Vec<String>, KvError> {
        std::future::pending().await
    }
    async fn set_add(&self, _: &str, _: Vec<u8>) -> Result<(), KvError> {
        std::future::pending().await
    }
    async fn set_contains(&self, _: &str, _: &[u8]) -> Result<bool, KvError> {
        std::future::pending().await
    }
    async fn set_members(
        &self,
        _: &str,
        _: Option<&[u8]>,
        _: u32,
    ) -> Result<Vec<Vec<u8>>, KvError> {
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wall_timeout_during_host_call_gives_504_and_frees_slot() {
    let rt = runtime(1);
    let counter = load(&rt, "counter", 1).await;
    let echo = load(&rt, "echo", 2).await;

    let (status, took) = serve_within(
        &rt,
        &counter,
        get("/"),
        ctx_with_kv(timeout_ms(200), Arc::new(StalledKv)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    assert!(took < Duration::from_secs(1), "504 took {took:?}");

    // The stalled guest was stopped and gave its slot back.
    let (status, _) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(timeout_ms(2_000)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_cap_gives_500() {
    let rt = runtime(4);
    let hog = load(&rt, "hog", 1).await;
    let echo = load(&rt, "echo", 2).await;
    let limits = Limits {
        memory_mb: 16,
        ..Limits::default()
    };
    let kv = Arc::new(MemKv::default());

    let (status, _) = serve_within(
        &rt,
        &hog,
        get("/"),
        ctx_with_kv(limits, kv.clone()),
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR);
    // The guest got some memory but never more than the cap.
    let held: u32 = String::from_utf8(kv.get("mib").await.unwrap().expect("hog recorded nothing"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (1..16).contains(&held),
        "hog held {held} MiB under a 16 MiB cap"
    );

    let (status, _) = serve_within(
        &rt,
        &echo,
        get("/?size=1000"),
        ctx(limits),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn body_cap_gives_413_before_instantiation() {
    let rt = runtime(4);
    let echo = load(&rt, "echo", 1).await;
    let cap = 1024;
    let limits = Limits {
        max_body_bytes: cap,
        ..Limits::default()
    };
    let before = rt.instantiations();

    let (status, _) = serve_within(
        &rt,
        &echo,
        post(vec![b'x'; cap as usize + 1]),
        ctx(limits),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        rt.instantiations(),
        before,
        "an oversized request was instantiated"
    );

    // Exactly at the cap is served.
    let (status, _) = serve_within(
        &rt,
        &echo,
        post(vec![b'x'; cap as usize]),
        ctx(limits),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(rt.instantiations(), before + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_response_gives_500() {
    let rt = runtime(4);
    let echo = load(&rt, "echo", 1).await;
    let cap: u64 = 64 * 1024;
    let limits = Limits {
        max_body_bytes: cap,
        ..Limits::default()
    };

    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        rt.handle(&echo, get(&format!("/?size={cap}")), ctx(limits)),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(resp.body().len() as u64, cap);

    for size in [cap + 1, 8 * 1024 * 1024] {
        let (status, _) = serve_within(
            &rt,
            &echo,
            get(&format!("/?size={size}")),
            ctx(limits),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(
            status,
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "size {size}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_cap_queues_then_times_out() {
    let rt = Arc::new(runtime(1));
    let spin = load(&rt, "spin", 1).await;
    let echo = load(&rt, "echo", 2).await;
    let before = rt.instantiations();

    // Occupy the only slot with a guest that runs until its 2 s deadline.
    let first = tokio::spawn({
        let rt = rt.clone();
        async move {
            serve_within(
                &rt,
                &spin,
                get("/"),
                ctx(timeout_ms(2_000)),
                Duration::from_secs(20),
            )
            .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.instantiations() == before {
        assert!(Instant::now() < deadline, "spin was never instantiated");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The second request waits for the slot; the wait counts against its 300 ms.
    let (status, took) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(timeout_ms(300)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    assert!(took < Duration::from_millis(1_500), "504 took {took:?}");
    assert_eq!(
        rt.instantiations(),
        before + 1,
        "the queued request was instantiated"
    );

    let (status, _) = first.await.unwrap();
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);

    // The slot is free again.
    let (status, _) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(timeout_ms(2_000)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}
