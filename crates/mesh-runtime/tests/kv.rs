mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::fixture;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::{KvBackend, KvError, Loaded, MemKv, RequestCtx, Runtime, RuntimeConfig};

fn ctx(kv: &Arc<MemKv>) -> RequestCtx {
    let kv: Arc<dyn KvBackend> = kv.clone();
    RequestCtx {
        service: "counter".to_string(),
        node_id: NodeId([7; 32]),
        kv,
        limits: Limits::default(),
        outbound: Vec::new(),
    }
}

async fn call(
    rt: &Runtime,
    svc: &Loaded,
    kv: &Arc<MemKv>,
    method: &str,
    path: &str,
    body: Bytes,
) -> (u16, String) {
    let req = http::Request::builder()
        .method(method)
        .uri(format!("http://counter.mesh.localhost{path}"))
        .body(body)
        .unwrap();
    let resp = rt.handle(svc, req, ctx(kv)).await;
    (
        resp.status().as_u16(),
        String::from_utf8_lossy(resp.body()).into_owned(),
    )
}

async fn get(rt: &Runtime, svc: &Loaded, kv: &Arc<MemKv>, path: &str) -> (u16, String) {
    call(rt, svc, kv, "GET", path, Bytes::new()).await
}

async fn setup() -> (Runtime, Loaded) {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let svc = rt.load([9; 32], &fixture("counter")).await.unwrap();
    (rt, svc)
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_semantics_all_calls() {
    let (rt, svc) = setup().await;
    let kv = Arc::new(MemKv::default());
    let ok = |s: &str| (200, s.to_string());

    // Counter: each request is a fresh instance, the value lives in the backend.
    assert_eq!(get(&rt, &svc, &kv, "/").await, ok("1"));
    assert_eq!(get(&rt, &svc, &kv, "/").await, ok("2"));
    assert_eq!(kv.get("n").await.unwrap(), Some(b"2".to_vec()));

    // set / get / del.
    assert_eq!(get(&rt, &svc, &kv, "/get?k=a/1").await.0, 404);
    assert_eq!(get(&rt, &svc, &kv, "/set?k=a/1&v=one").await, ok("ok"));
    assert_eq!(get(&rt, &svc, &kv, "/set?k=a/2&v=two").await, ok("ok"));
    assert_eq!(get(&rt, &svc, &kv, "/set?k=b/1&v=three").await, ok("ok"));
    assert_eq!(get(&rt, &svc, &kv, "/get?k=a/1").await, ok("one"));
    assert_eq!(get(&rt, &svc, &kv, "/del?k=a/2").await, ok("ok"));
    assert_eq!(get(&rt, &svc, &kv, "/get?k=a/2").await.0, 404);

    // list-keys with prefix and start-after.
    assert_eq!(get(&rt, &svc, &kv, "/list?prefix=a/").await, ok("a/1"));
    assert_eq!(
        get(&rt, &svc, &kv, "/list?prefix=&after=a/1").await,
        ok("b/1
n")
    );
    assert_eq!(
        get(&rt, &svc, &kv, "/list?prefix=&limit=1").await,
        ok("a/1")
    );

    // set-ordered: the higher order wins regardless of arrival; no value deletes.
    assert_eq!(
        get(&rt, &svc, &kv, "/ordered?k=o&o=2&v=two").await,
        ok("ok")
    );
    assert_eq!(
        get(&rt, &svc, &kv, "/ordered?k=o&o=1&v=one").await,
        ok("ok")
    );
    assert_eq!(get(&rt, &svc, &kv, "/get?k=o").await, ok("two"));
    assert_eq!(get(&rt, &svc, &kv, "/ordered?k=o&o=3").await, ok("ok"));
    assert_eq!(get(&rt, &svc, &kv, "/get?k=o").await.0, 404);

    // Grow-only sets.
    for m in ["m2", "m1", "m3"] {
        assert_eq!(
            get(&rt, &svc, &kv, &format!("/setadd?s=s&m={m}")).await,
            ok("ok")
        );
    }
    assert_eq!(get(&rt, &svc, &kv, "/contains?s=s&m=m1").await, ok("true"));
    assert_eq!(get(&rt, &svc, &kv, "/contains?s=s&m=m9").await, ok("false"));
    assert_eq!(get(&rt, &svc, &kv, "/members?s=s").await, ok("m1\nm2\nm3"));
    assert_eq!(
        get(&rt, &svc, &kv, "/members?s=s&after=m1&limit=1").await,
        ok("m2")
    );

    // node-id is the context's node id.
    assert_eq!(get(&rt, &svc, &kv, "/nodeid").await, ok(&"07".repeat(32)));
}

#[tokio::test(flavor = "multi_thread")]
async fn isolation_two_services_two_namespaces() {
    let (rt, svc) = setup().await;
    let (a, b) = (Arc::new(MemKv::default()), Arc::new(MemKv::default()));
    assert_eq!(get(&rt, &svc, &a, "/set?k=x&v=from-a").await.0, 200);
    assert_eq!(get(&rt, &svc, &a, "/setadd?s=s&m=m").await.0, 200);
    assert_eq!(get(&rt, &svc, &a, "/").await.1, "1");

    assert_eq!(get(&rt, &svc, &b, "/get?k=x").await.0, 404);
    assert_eq!(
        get(&rt, &svc, &b, "/list?prefix=").await,
        (200, String::new())
    );
    assert_eq!(get(&rt, &svc, &b, "/contains?s=s&m=m").await.1, "false");
    assert_eq!(get(&rt, &svc, &b, "/").await.1, "1");
    assert_eq!(get(&rt, &svc, &a, "/get?k=x").await.1, "from-a");
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_errors_map_to_507_503_500() {
    let (rt, svc) = setup().await;
    let cases = [
        (KvError::Full, 507),
        (KvError::Removed, 503),
        (KvError::Rebootstrapping, 503),
        (KvError::Other("boom".into()), 500),
    ];
    for (err, status) in cases {
        let kv = Arc::new(MemKv::default());
        kv.fail_with(Some(err.clone()));
        assert_eq!(get(&rt, &svc, &kv, "/").await.0, status, "{err:?}");
        assert_eq!(
            get(&rt, &svc, &kv, "/set?k=a&v=b").await.0,
            status,
            "{err:?}"
        );
        // The node keeps serving once the backend recovers.
        kv.fail_with(None);
        assert_eq!(get(&rt, &svc, &kv, "/").await, (200, "1".to_string()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_limit_capped() {
    let (rt, svc) = setup().await;
    let kv = Arc::new(MemKv::default());
    for i in 0..1500 {
        kv.set(&format!("k{i:05}"), vec![1]).await.unwrap();
        kv.set_add("s", format!("m{i:05}").into_bytes())
            .await
            .unwrap();
    }
    let (status, body) = get(&rt, &svc, &kv, "/list?prefix=k&limit=4294967295").await;
    assert_eq!(status, 200);
    assert_eq!(body.lines().count(), 1000);
    let (status, body) = get(&rt, &svc, &kv, "/members?s=s&limit=4294967295").await;
    assert_eq!(status, 200);
    assert_eq!(body.lines().count(), 1000);
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_kv_value_gives_500() {
    let (rt, svc) = setup().await;
    let kv = Arc::new(MemKv::default());
    kv.set_max_value_len(Some(1 << 20));
    let big = Bytes::from(vec![b'x'; 2 << 20]);
    assert_eq!(call(&rt, &svc, &kv, "POST", "/set?k=big", big).await.0, 500);
    assert_eq!(kv.get("big").await.unwrap(), None);
    let small = Bytes::from(vec![b'x'; 1 << 19]);
    assert_eq!(
        call(&rt, &svc, &kv, "POST", "/set?k=ok", small).await.0,
        200
    );
    assert_eq!(get(&rt, &svc, &kv, "/").await, (200, "1".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_local_roundtrip() {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let svc = rt.load([10; 32], &fixture("local")).await.unwrap();
    let kv = Arc::new(MemKv::default());
    kv.set_now_ms(1_000);
    let ok = |s: &str| (200, s.to_string());

    assert_eq!(get(&rt, &svc, &kv, "/lget?k=n").await.0, 404);
    assert_eq!(get(&rt, &svc, &kv, "/lset?k=n&v=one").await, ok("ok"));
    assert_eq!(get(&rt, &svc, &kv, "/lget?k=n").await, ok("one"));
    // Local entries are not in the replicated store.
    assert_eq!(get(&rt, &svc, &kv, "/get?k=n").await.0, 404);
    assert_eq!(kv.get("n").await.unwrap(), None);
    assert_eq!(kv.local_get("n").await.unwrap(), Some(b"one".to_vec()));

    // TTL: visible before the deadline, gone at it.
    assert_eq!(get(&rt, &svc, &kv, "/lset?k=t&v=x&ttl=500").await, ok("ok"));
    kv.set_now_ms(1_499);
    assert_eq!(get(&rt, &svc, &kv, "/lget?k=t").await, ok("x"));
    kv.set_now_ms(1_500);
    assert_eq!(get(&rt, &svc, &kv, "/lget?k=t").await.0, 404);

    assert_eq!(get(&rt, &svc, &kv, "/ldel?k=n").await, ok("ok"));
    assert_eq!(get(&rt, &svc, &kv, "/lget?k=n").await.0, 404);

    // A cap breach is an `err` the guest sees, not a trap.
    let big = Bytes::from(vec![b'x'; 64 * 1024 + 1]);
    let (status, body) = call(&rt, &svc, &kv, "POST", "/lset?k=big", big).await;
    assert_eq!(status, 200);
    assert!(body.starts_with("err: "), "{body}");
    assert_eq!(kv.local_get("big").await.unwrap(), None);

    // Backend failures trap with the mapped status, as for `store`.
    kv.fail_with(Some(KvError::Rebootstrapping));
    assert_eq!(get(&rt, &svc, &kv, "/lget?k=n").await.0, 503);
    kv.fail_with(None);
}

#[tokio::test(flavor = "multi_thread")]
async fn fixtures_without_local_still_load() {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    for (i, name) in ["counter", "echo", "hog", "spin", "outbound"]
        .iter()
        .enumerate()
    {
        rt.load([i as u8 + 20; 32], &fixture(name))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
