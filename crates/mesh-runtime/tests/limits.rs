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

fn compact_runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        max_in_flight: 4,
        compact_memory: true,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

fn budget_runtime(max_memory_mb: u32) -> Runtime {
    Runtime::new(RuntimeConfig {
        max_in_flight: 4,
        max_memory_mb,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

fn memory(memory_mb: u32, request_timeout_ms: u64) -> Limits {
    Limits {
        memory_mb,
        request_timeout_ms,
        ..Limits::default()
    }
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
    let (loaded_tx, loaded_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let status = tokio.block_on(async {
            let rt = runtime(4);
            // Building and compiling the fixture is not what is being timed.
            let spin = load(&rt, "spin", 1).await;
            let _ = loaded_tx.send(());
            rt.handle(&spin, get("/"), ctx(timeout_ms(200)))
                .await
                .status()
        });
        let _ = tx.send(status);
    });
    loaded_rx.recv().expect("fixture failed to load");
    // The request has a 200 ms timeout; only a guest that is never interrupted overruns this.
    let status = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("spinning guest was not interrupted");
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
}

/// Spinning guests yield to the executor: with more spinning requests than worker
/// threads, other tasks on the runtime keep running long before the spins time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spinning_guests_do_not_starve_other_tasks() {
    const SPINS: usize = 4;
    let spin_timeout = Duration::from_secs(5);
    let rt = Arc::new(runtime(64));
    let spin = load(&rt, "spin", 1).await;
    let before = rt.instantiations();

    let started = Instant::now();
    let spins: Vec<_> = (0..SPINS)
        .map(|_| {
            let rt = rt.clone();
            let spin = spin.clone();
            tokio::spawn(async move {
                rt.handle(&spin, get("/"), ctx(timeout_ms(5_000)))
                    .await
                    .status()
            })
        })
        .collect();
    // Wait (on the test thread, which is not a worker) until both workers run a guest.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.instantiations() < before + 2 {
        assert!(Instant::now() < deadline, "spins were never instantiated");
        std::thread::sleep(Duration::from_millis(5));
    }

    let probe = Instant::now();
    tokio::spawn(tokio::time::sleep(Duration::from_millis(50)))
        .await
        .unwrap();
    let took = probe.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "a 50 ms sleep took {took:?} next to {SPINS} spinning guests"
    );

    for handle in spins {
        assert_eq!(handle.await.unwrap(), http::StatusCode::GATEWAY_TIMEOUT);
    }
    assert!(
        started.elapsed() < spin_timeout * 3,
        "spins took {:?}",
        started.elapsed()
    );
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
    async fn local_get(&self, _: &str) -> Result<Option<Vec<u8>>, KvError> {
        std::future::pending().await
    }
    async fn local_set(&self, _: &str, _: Vec<u8>, _: u64) -> Result<Result<(), String>, KvError> {
        std::future::pending().await
    }
    async fn local_delete(&self, _: &str) -> Result<(), KvError> {
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
    memory_cap_gives_500_with(runtime(4)).await;
}

/// The same cap and out-of-memory status with compact memories (small reservation and
/// guard, movable on growth).
#[tokio::test(flavor = "multi_thread")]
async fn memory_cap_gives_500_compact() {
    memory_cap_gives_500_with(compact_runtime()).await;
}

async fn memory_cap_gives_500_with(rt: Runtime) {
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

/// The epoch deadline counts the time spent waiting for a slot: a spinning guest that
/// got its slot late is stopped when its request times out, not a full timeout later.
#[tokio::test(flavor = "multi_thread")]
async fn queued_spin_frees_slot_within_one_timeout() {
    let rt = Arc::new(runtime(1));
    let counter = load(&rt, "counter", 1).await;
    let spin = load(&rt, "spin", 2).await;
    let echo = load(&rt, "echo", 3).await;
    let before = rt.instantiations();

    // Hold the only slot for 1.2 s with a guest parked in a host call.
    let holder = tokio::spawn({
        let rt = rt.clone();
        async move {
            serve_within(
                &rt,
                &counter,
                get("/"),
                ctx_with_kv(timeout_ms(1_200), Arc::new(StalledKv)),
                Duration::from_secs(20),
            )
            .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.instantiations() == before {
        assert!(Instant::now() < deadline, "holder was never instantiated");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Queue a spin with 1.5 s: it gets the slot after ~1.2 s, leaving ~0.3 s to run.
    let start = Instant::now();
    let (status, _) = serve_within(
        &rt,
        &spin,
        get("/"),
        ctx(timeout_ms(1_500)),
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(holder.await.unwrap().0, http::StatusCode::GATEWAY_TIMEOUT);

    // The slot is free once the spin's 1.5 s are up, not 1.2 s + a fresh 1.5 s later.
    let (status, _) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(timeout_ms(5_000)),
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    let took = start.elapsed();
    assert!(
        took < Duration::from_millis(2_250),
        "slot freed after {took:?}"
    );
}

/// The epoch ticker only runs while a request is in flight.
#[tokio::test(flavor = "multi_thread")]
async fn epoch_ticker_parks_when_idle() {
    let rt = runtime(4);
    let spin = load(&rt, "spin", 1).await;
    let idle = rt.epoch_ticks();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rt.epoch_ticks(), idle, "ticked with no request in flight");

    let (status, _) = serve_within(
        &rt,
        &spin,
        get("/"),
        ctx(timeout_ms(300)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    let busy = rt.epoch_ticks();
    assert!(busy > idle, "no ticks while the guest ran");

    // Once the aborted guest has stopped (its next yield), the ticker parks again.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = rt.epoch_ticks();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rt.epoch_ticks(), after, "kept ticking once idle");
}

/// The defaults: a budget of half the machine's RAM, capped at 4 GiB; compact memories
/// only on Android.
#[test]
fn memory_defaults() {
    let budget = mesh_runtime::default_memory_budget_mb();
    assert!((64..=4096).contains(&budget), "budget {budget} MiB");
    let cfg = RuntimeConfig::default();
    assert_eq!(cfg.max_memory_mb, budget);
    assert_eq!(cfg.compact_memory, cfg!(target_os = "android"));
}

/// Waits until `rt` has created `n` more instances than `before`.
async fn wait_instantiated(rt: &Runtime, before: u64, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.instantiations() < before + n {
        assert!(Instant::now() < deadline, "never instantiated");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Two 48 MiB requests under a 64 MiB budget: the second waits for the first's
/// reservation. It gets `503 Memory budget exhausted` when the first holds it past the
/// second's timeout, and is served when the first finishes in time.
#[tokio::test(flavor = "multi_thread")]
async fn memory_budget_waits_then_503() {
    let rt = Arc::new(budget_runtime(64));
    let counter = load(&rt, "counter", 1).await;
    let echo = load(&rt, "echo", 2).await;

    // The holder keeps its 48 MiB until its 1.5 s deadline.
    let before = rt.instantiations();
    let holder = tokio::spawn({
        let (rt, counter) = (rt.clone(), counter.clone());
        async move {
            serve_within(
                &rt,
                &counter,
                get("/"),
                ctx_with_kv(memory(48, 1_500), Arc::new(StalledKv)),
                Duration::from_secs(20),
            )
            .await
        }
    });
    wait_instantiated(&rt, before, 1).await;

    let start = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        rt.handle(&echo, get("/?size=3"), ctx(memory(48, 300))),
    )
    .await
    .expect("no response");
    let took = start.elapsed();
    assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.body().as_ref(), b"Memory budget exhausted");
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_millis(1_400),
        "503 after {took:?}"
    );
    assert_eq!(
        rt.instantiations(),
        before + 1,
        "the waiting request was instantiated"
    );
    assert_eq!(holder.await.unwrap().0, http::StatusCode::GATEWAY_TIMEOUT);

    // Now the holder finishes (at its 400 ms deadline) within the second's 5 s.
    let before = rt.instantiations();
    let holder = tokio::spawn({
        let rt = rt.clone();
        async move {
            serve_within(
                &rt,
                &counter,
                get("/"),
                ctx_with_kv(memory(48, 400), Arc::new(StalledKv)),
                Duration::from_secs(20),
            )
            .await
        }
    });
    wait_instantiated(&rt, before, 1).await;
    let start = Instant::now();
    let (status, took) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(memory(48, 5_000)),
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert!(took >= Duration::from_millis(200), "did not wait: {took:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(holder.await.unwrap().0, http::StatusCode::GATEWAY_TIMEOUT);
}

/// A request reserving more than the whole budget is refused at once.
#[tokio::test(flavor = "multi_thread")]
async fn request_larger_than_budget_503() {
    let rt = budget_runtime(64);
    let echo = load(&rt, "echo", 1).await;
    let before = rt.instantiations();

    let start = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        rt.handle(&echo, get("/?size=3"), ctx(memory(65, 5_000))),
    )
    .await
    .expect("no response");
    assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.body().as_ref(), b"Memory budget exhausted");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(rt.instantiations(), before);

    // Exactly the whole budget is served.
    let (status, _) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(memory(64, 5_000)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}

/// An engine with compact memories builds and serves.
#[tokio::test(flavor = "multi_thread")]
async fn compact_memory_engine_serves() {
    let rt = compact_runtime();
    let echo = load(&rt, "echo", 1).await;
    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        rt.handle(&echo, get("/?size=100000"), ctx(Limits::default())),
    )
    .await
    .expect("no response");
    assert_eq!(resp.status(), http::StatusCode::OK);
    assert_eq!(resp.body().len(), 100_000);
}

/// [`Runtime::acquire_slot`] takes a slot from the same in-flight semaphore as WASM
/// requests, waiting at most until its deadline.
#[tokio::test(flavor = "multi_thread")]
async fn acquire_slot_shares_in_flight_cap() {
    let rt = runtime(1);
    let slot = rt
        .acquire_slot(Instant::now() + Duration::from_millis(100))
        .await
        .expect("a free slot");
    let start = Instant::now();
    assert!(
        rt.acquire_slot(Instant::now() + Duration::from_millis(200))
            .await
            .is_none()
    );
    assert!(start.elapsed() >= Duration::from_millis(150));
    let echo = load(&rt, "echo", 1).await;
    let (status, _) = serve_within(
        &rt,
        &echo,
        get("/?size=3"),
        ctx(timeout_ms(300)),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    drop(slot);
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
