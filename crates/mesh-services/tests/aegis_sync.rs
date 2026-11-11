//! `services/aegis-sync` (the Aegis sync server as a Meshnode WASM service) driven in-process
//! through `mesh_runtime` over a `MemKv`: the shared contract vectors from the vendored
//! `aegis-sync-core`, plus the mesh-specific rules (tie `ord`, replay nonces in `kv/local`,
//! the membership gate) and paging at scale.
//!
//! Task 7 runs the `healthz_*`, `auth_*`, `records_*` and `bounds_*` vectors that need no
//! devices endpoint; the rest run once those endpoints exist (Task 8).

// The service's own key layout, so seeded state is exactly where the service reads it. Some
// of it (the store bounds, the account prefix) only the service uses.
#[allow(dead_code)]
#[path = "../../../services/aegis-sync/src/keys.rs"]
mod keys;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aegis_sync_core::auth::MAX_TTL_MS;
use aegis_sync_core::devices::{AccountFacts, DeviceFacts};
use aegis_sync_core::order::{order_bytes, tie_ord};
use aegis_sync_core::page::page_sorted;
use aegis_sync_core::vector::{self, Host, NOW_MS, Req, Tamper, Vector};
use aegis_sync_core::wire::{
    MAX_CT_LEN, MAX_RECORDS_PER_ACCOUNT, MAX_RESPONSE_BYTES, MAX_RESPONSE_RECORDS, WireRecord,
    hlc_key, wire_bytes,
};
use bytes::Bytes;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::test_fixtures::aegis_sync_wasm;
use mesh_runtime::{KvBackend, Loaded, MemKv, RequestCtx, Runtime, RuntimeConfig};
use serde_json::{Value, json};

/// The service's deploy defaults (spec §5.3): 64 MB, 30 s, 8 MiB.
const SERVICE_LIMITS: Limits = Limits {
    memory_mb: 64,
    request_timeout_ms: 30_000,
    max_body_bytes: 8 * 1024 * 1024,
};

/// Room for a full `MAX_RESPONSE_BYTES` page: the runtime caps a RESPONSE body at
/// `max_body_bytes` too, so the 8 MiB default cuts a 32 MiB page off with a 500.
const WIDE_LIMITS: Limits = Limits {
    max_body_bytes: 40 * 1024 * 1024,
    ..SERVICE_LIMITS
};

struct Svc {
    rt: Runtime,
    svc: Loaded,
}

async fn service() -> Svc {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    rt.set_wall_clock_ms(Some(NOW_MS));
    let wasm = std::fs::read(aegis_sync_wasm()).expect("read aegis_sync.wasm");
    let svc = rt.load([0xa5; 32], &wasm).await.expect("load aegis-sync");
    Svc { rt, svc }
}

fn fresh_kv() -> Arc<MemKv> {
    let kv = Arc::new(MemKv::default());
    kv.set_now_ms(NOW_MS as u64);
    kv
}

impl Svc {
    async fn send(&self, kv: &Arc<MemKv>, limits: Limits, req: &Req) -> (u16, Option<Value>) {
        let mut b = http::Request::builder()
            .method(req.method.as_str())
            .uri(format!("http://sync.mesh.localhost{}", req.path));
        for (k, v) in &req.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        let body = match &req.body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Bytes::from(serde_json::to_vec(v).unwrap())
            }
            None => Bytes::new(),
        };
        let backend: Arc<dyn KvBackend> = kv.clone();
        let ctx = RequestCtx {
            service: "sync".into(),
            node_id: NodeId([7; 32]),
            kv: backend,
            limits,
            outbound: Vec::new(),
        };
        let resp = self.rt.handle(&self.svc, b.body(body).unwrap(), ctx).await;
        let status = resp.status().as_u16();
        let body = serde_json::from_slice(resp.body()).ok();
        (status, body)
    }

    async fn call(&self, kv: &Arc<MemKv>, req: Req) -> (u16, Option<Value>) {
        self.send(kv, SERVICE_LIMITS, &req).await
    }
}

fn req(method: &str, path: &str, auth: Option<String>, body: Option<Value>) -> Req {
    Req {
        method: method.into(),
        path: path.into(),
        headers: auth
            .map(|a| vec![("authorization".into(), a)])
            .unwrap_or_default(),
        body,
    }
}

fn auth(acct: &str, dev: &str, nonce: &str) -> Option<String> {
    Some(vector::auth_header(acct, dev, nonce, Tamper::None))
}

/// A fresh hex nonce per call.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0x1000);
    format!("{:x}", N.fetch_add(1, Ordering::Relaxed))
}

// ------------------------------------------------------------------------- seeding

async fn seed_facts(kv: &MemKv, acct: &str, f: &AccountFacts) {
    for d in &f.devices {
        let m = serde_json::to_vec(d).unwrap();
        kv.set_add(&keys::devices_set(acct), m).await.unwrap();
    }
    for (set, ids) in [
        (keys::pending_set(acct), &f.pending),
        (keys::approved_set(acct), &f.approved),
        (keys::revoked_set(acct), &f.revoked),
    ] {
        for id in ids {
            kv.set_add(&set, id.as_bytes().to_vec()).await.unwrap();
        }
    }
    for l in &f.labels {
        let m = serde_json::to_vec(l).unwrap();
        kv.set_add(&keys::labels_set(acct), m).await.unwrap();
    }
}

async fn seed_record(kv: &MemKv, acct: &str, ns: &str, rec: &WireRecord) {
    let set = keys::hlcidx_set(acct, ns, &rec.hlc);
    kv.set_add(&set, rec.uuid.as_bytes().to_vec())
        .await
        .unwrap();
    let mut stored = rec.clone();
    stored.ord = None;
    kv.set_ordered(
        &keys::rec_key(acct, ns, &rec.uuid),
        order_bytes(&rec.hlc),
        Some(serde_json::to_vec(&stored).unwrap()),
    )
    .await
    .unwrap();
}

async fn seed_given(kv: &MemKv, v: &Vector) {
    for (acct, facts) in &v.given.accounts {
        seed_facts(kv, acct, facts).await;
    }
    for (acct, ns, rec) in &v.given.records {
        seed_record(kv, acct, ns, rec).await;
    }
}

/// Account `acct` with `dev` as its only member.
async fn seed_member(kv: &MemKv, acct: &str, dev: &str) {
    let facts = AccountFacts {
        devices: vec![DeviceFacts {
            device_id: dev.into(),
            label: "phone".into(),
            registered_ms: 1000,
        }],
        ..AccountFacts::default()
    };
    seed_facts(kv, acct, &facts).await;
}

fn rec(uuid: &str, wall: i64, counter: u64, node: &str, ct: &str) -> WireRecord {
    WireRecord {
        uuid: uuid.into(),
        hlc: json!({ "wall_ms": wall, "counter": counter, "node": node }),
        deleted: false,
        nonce: "aa".into(),
        ct: ct.into(),
        ord: None,
    }
}

/// What the mesh serves for one namespace: uuid order, with the tie `ord` on every record
/// that shares its full `hlc` with a lower uuid.
fn served(records: &[WireRecord]) -> Vec<WireRecord> {
    let mut sorted: Vec<WireRecord> = records.to_vec();
    sorted.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    let mut lowest: BTreeMap<(i64, u64, String), String> = BTreeMap::new();
    for r in &sorted {
        let (w, c, n) = hlc_key(&r.hlc);
        lowest
            .entry((w, c, n.to_string()))
            .or_insert_with(|| r.uuid.clone());
    }
    sorted
        .into_iter()
        .map(|mut r| {
            let (w, c, n) = hlc_key(&r.hlc);
            r.ord = (lowest[&(w, c, n.to_string())] != r.uuid).then(|| tie_ord(&r));
            r
        })
        .collect()
}

fn as_value(page: &[WireRecord]) -> Value {
    serde_json::to_value(page).unwrap()
}

// ------------------------------------------------------------------------- vectors

fn vectors() -> Vec<Vector> {
    vector::load_all(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../vendor/aegis-sync-core/tests/vectors"
    ))
}

/// Task 7's vectors: the four groups whose routes exist now, minus any vector that calls a
/// devices endpoint (Task 8).
fn in_task7(v: &Vector) -> bool {
    let group_ok = ["healthz_", "auth_", "records_", "bounds_"]
        .iter()
        .any(|g| v.name.starts_with(g));
    group_ok && v.steps().all(|(r, _)| !r.path.starts_with("/v1/devices"))
}

#[tokio::test(flavor = "multi_thread")]
async fn contract_vectors() {
    let all = vectors();
    let deferred: BTreeSet<&str> = all
        .iter()
        .filter(|v| v.runs_on(Host::Mesh) && !in_task7(v))
        .map(|v| v.name.as_str())
        .collect();
    // Everything deferred is a devices case; a new non-devices vector must run here.
    for name in &deferred {
        assert!(
            name.starts_with("devices_") || *name == "bounds_label_too_long_413",
            "{name} is deferred but needs no devices endpoint"
        );
    }

    let s = service().await;
    let mut ran = Vec::new();
    let mut failures = Vec::new();
    for v in all.iter().filter(|v| v.runs_on(Host::Mesh) && in_task7(v)) {
        let kv = fresh_kv();
        seed_given(&kv, v).await;
        for (i, (r, expect)) in v.steps().enumerate() {
            let (status, body) = s.call(&kv, r.clone()).await;
            if let Err(e) = vector::check(expect, status, body.as_ref()) {
                failures.push(format!(
                    "{} step {i} ({} {}): {e}",
                    v.name, r.method, r.path
                ));
                break;
            }
        }
        ran.push(v.name.as_str());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        ran.contains(&"records_cross_uuid_tie_tie_ord"),
        "the mesh tie vector runs"
    );
    assert!(
        !ran.contains(&"records_cross_uuid_tie_counter_bump"),
        "native_only is skipped"
    );
    for want in [
        "healthz_ok",
        "auth_replayed_nonce_401",
        "bounds_bad_hlc_400",
    ] {
        assert!(ran.contains(&want), "{want} did not run");
    }
    eprintln!(
        "ran {} vectors: {ran:?}\ndeferred to Task 8: {deferred:?}",
        ran.len()
    );
}

// ------------------------------------------------------------------------- mesh rules

#[tokio::test(flavor = "multi_thread")]
async fn tie_ord_rule_over_the_candidate_index() {
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    let push = |records: Vec<WireRecord>| {
        req(
            "POST",
            "/v1/records",
            auth("A", "D1", &nonce()),
            Some(json!({ "ns": "bm", "records": records })),
        )
    };
    let get = || req("GET", "/v1/records?ns=bm", auth("A", "D1", &nonce()), None);
    let ords = |body: Option<Value>| -> Vec<(String, Option<Value>)> {
        body.unwrap()["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["uuid"].as_str().unwrap().to_string(),
                    r.get("ord").cloned(),
                )
            })
            .collect()
    };
    let h = (1000, 0, "n1");
    let r = |u: &str| rec(u, h.0, h.1, h.2, u);
    let tie = |u: &str| Some(tie_ord(&r(u)));

    // Alone: no ord.
    assert_eq!(s.call(&kv, push(vec![r("u2")])).await.0, 200);
    assert_eq!(ords(s.call(&kv, get()).await.1), [("u2".into(), None)]);
    // A tie partner pushed in a later request: only the higher uuid gets the ord.
    assert_eq!(s.call(&kv, push(vec![r("u1")])).await.0, 200);
    assert_eq!(
        ords(s.call(&kv, get()).await.1),
        [("u1".into(), None), ("u2".into(), tie("u2"))]
    );
    // u1 moves to a newer hlc. The index still lists it under the old one, but its CURRENT
    // record no longer has that hlc, so u2 is untied again.
    let moved = rec("u1", 2000, 0, "n1", "u1v2");
    assert_eq!(s.call(&kv, push(vec![moved])).await.0, 200);
    assert_eq!(
        ords(s.call(&kv, get()).await.1),
        [("u1".into(), None), ("u2".into(), None)]
    );
    // A tombstone counts as a tie partner.
    let mut tomb = r("u0");
    tomb.deleted = true;
    tomb.ct = String::new();
    assert_eq!(s.call(&kv, push(vec![tomb])).await.0, 200);
    // A three-way tie: everyone but the lowest uuid.
    assert_eq!(s.call(&kv, push(vec![r("u3")])).await.0, 200);
    assert_eq!(
        ords(s.call(&kv, get()).await.1),
        [
            ("u0".into(), None),
            ("u1".into(), None),
            ("u2".into(), tie("u2")),
            ("u3".into(), tie("u3"))
        ]
    );
    // Nothing stored carries an ord: it is computed when served.
    let stored = kv
        .get(&keys::rec_key(&a, "bm", "u2"))
        .await
        .unwrap()
        .unwrap();
    let stored: Value = serde_json::from_slice(&stored).unwrap();
    assert!(stored.get("ord").is_none(), "{stored}");
}

#[tokio::test(flavor = "multi_thread")]
async fn nonce_is_spent_only_after_admit_and_outlives_the_token() {
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1, d2) = (
        vector::account_id("A"),
        vector::device_id("D1"),
        vector::device_id("D2"),
    );
    seed_member(&kv, &a, &d1).await;
    let get = |dev: &str, n: &str| req("GET", "/v1/records?ns=bm", auth("A", dev, n), None);

    // Refused by the membership gate: the nonce is not spent.
    assert_eq!(s.call(&kv, get("D2", "07")).await.0, 403);
    assert_eq!(
        kv.local_get(&keys::nonce_key(&d2, "07")).await.unwrap(),
        None
    );
    // Once D2 is a member the same token is accepted, and then spent.
    let facts = AccountFacts {
        devices: vec![DeviceFacts {
            device_id: d2.clone(),
            label: "laptop".into(),
            registered_ms: 2000,
        }],
        ..AccountFacts::default()
    };
    seed_facts(&kv, &a, &facts).await;
    assert_eq!(s.call(&kv, get("D2", "07")).await.0, 200);
    assert!(
        kv.local_get(&keys::nonce_key(&d2, "07"))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(s.call(&kv, get("D2", "07")).await.0, 401);

    // A token issued up to MAX_FUTURE_SKEW_MS ahead of the node's clock lives longer than
    // MAX_TTL_MS from now; its nonce must stay spent until the token itself expires.
    let early = NOW_MS - 60_000;
    s.rt.set_wall_clock_ms(Some(early));
    kv.set_now_ms(early as u64);
    assert_eq!(s.call(&kv, get("D1", "08")).await.0, 200);
    let last_valid = NOW_MS + MAX_TTL_MS; // the token's `expires_ms`
    s.rt.set_wall_clock_ms(Some(last_valid));
    kv.set_now_ms(last_valid as u64);
    assert_eq!(
        s.call(&kv, get("D1", "08")).await.0,
        401,
        "replay at expires_ms"
    );
    s.rt.set_wall_clock_ms(Some(NOW_MS));
}

#[tokio::test(flavor = "multi_thread")]
async fn record_routes_admit_members_only() {
    let s = service().await;
    let kv = fresh_kv();
    let a = vector::account_id("A");
    let id = |d: &str| vector::device_id(d);
    let dev = |d: &str, ms: i64| DeviceFacts {
        device_id: id(d),
        label: d.into(),
        registered_ms: ms,
    };
    let facts = AccountFacts {
        devices: vec![dev("D1", 1), dev("D2", 2), dev("D3", 3), dev("D4", 4)],
        pending: [id("D2"), id("D3")].into(),
        approved: [id("D3")].into(),
        revoked: [id("D4")].into(),
        ..AccountFacts::default()
    };
    seed_facts(&kv, &a, &facts).await;
    for (d, want) in [("D1", 200), ("D2", 403), ("D3", 200), ("D4", 403)] {
        let get = req("GET", "/v1/records?ns=bm", auth("A", d, &nonce()), None);
        assert_eq!(s.call(&kv, get).await.0, want, "GET as {d}");
        let body = json!({ "ns": "bm", "records": [rec(&format!("by-{d}"), 1, 0, "n", "c")] });
        let post = req("POST", "/v1/records", auth("A", d, &nonce()), Some(body));
        assert_eq!(s.call(&kv, post).await.0, want, "POST as {d}");
    }
    let keys = kv
        .list_keys(&keys::rec_prefix(&a, "bm"), None, 10)
        .await
        .unwrap();
    assert_eq!(
        keys,
        [
            keys::rec_key(&a, "bm", "by-D1"),
            keys::rec_key(&a, "bm", "by-D3")
        ]
    );
    // Another account's member is not a member of A.
    let b = vector::account_id("B");
    seed_member(&kv, &b, &id("D4")).await;
    let get = req("GET", "/v1/records?ns=bm", auth("A", "D4", &nonce()), None);
    assert_eq!(s.call(&kv, get).await.0, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn namespaces_with_slashes_stay_apart_and_mesh_key_bounds_refuse() {
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    let push = |ns: &str, records: Vec<WireRecord>| {
        let body = json!({ "ns": ns, "records": records });
        req("POST", "/v1/records", auth("A", "D1", &nonce()), Some(body))
    };
    let get = |ns: &str| {
        let path = format!(
            "/v1/records?ns={}",
            ns.replace('%', "%25").replace('/', "%2F")
        );
        req("GET", &path, auth("A", "D1", &nonce()), None)
    };
    assert_eq!(
        s.call(&kv, push("a/b", vec![rec("c", 1, 0, "n", "1")]))
            .await
            .0,
        200
    );
    assert_eq!(
        s.call(&kv, push("a", vec![rec("b/c", 1, 0, "n", "2")]))
            .await
            .0,
        200
    );
    let uuids = |b: Option<Value>| -> Vec<String> {
        b.unwrap()["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uuid"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(uuids(s.call(&kv, get("a/b")).await.1), ["c"]);
    assert_eq!(uuids(s.call(&kv, get("a")).await.1), ["b/c"]);

    // A key the replicated store cannot hold (over 512 bytes) is refused up front, and the
    // request writes nothing at all.
    let long_ns = "n".repeat(500);
    let batch = vec![rec("ok", 1, 0, "n", "x")];
    assert_eq!(s.call(&kv, push(&long_ns, batch)).await.0, 413);
    let batch = vec![
        rec("fine", 1, 0, "n", "x"),
        rec(&"u".repeat(480), 1, 0, "n", "x"),
    ];
    assert_eq!(s.call(&kv, push("bm", batch)).await.0, 413);
    // An hlc node too long for a 64-byte store order.
    let batch = vec![
        rec("fine", 1, 0, "n", "x"),
        rec("u", 1, 0, &"n".repeat(49), "x"),
    ];
    assert_eq!(s.call(&kv, push("bm", batch)).await.0, 413);
    let all = kv.list_keys("rec/", None, 100).await.unwrap();
    assert_eq!(all.len(), 2, "{all:?}");
}

// ------------------------------------------------------------------------- paging

/// How to walk a namespace in [`walk`].
struct Walk<'a> {
    ns: &'a str,
    /// The `limit` query parameter, or none for the default.
    limit: Option<usize>,
    /// The first cursor, or none to start at the beginning.
    start: Option<&'a str>,
    /// Stop after this many pages even if there is a `next`.
    max_pages: usize,
}

/// Walks a namespace page by page, checking every page against the core's `page_sorted` over
/// `model` (what the mesh should serve, see [`served`]); returns the uuids in walk order.
async fn walk(
    s: &Svc,
    kv: &Arc<MemKv>,
    limits: Limits,
    w: Walk<'_>,
    model: &[WireRecord],
) -> Vec<String> {
    let Walk {
        ns,
        limit,
        start,
        max_pages,
    } = w;
    let mut seen = Vec::new();
    let mut cursor: Option<String> = start.map(str::to_string);
    for _ in 0..max_pages {
        let mut path = format!("/v1/records?ns={ns}");
        if let Some(l) = limit {
            path.push_str(&format!("&limit={l}"));
        }
        if let Some(c) = &cursor {
            path.push_str(&format!("&cursor={c}"));
        }
        let started = Instant::now();
        let (status, body) = s
            .send(
                kv,
                limits,
                &req("GET", &path, auth("A", "D1", &nonce()), None),
            )
            .await;
        let took = started.elapsed();
        assert_eq!(status, 200, "GET {path}");
        assert!(took < Duration::from_secs(30), "GET {path} took {took:?}");
        let body = body.unwrap();
        let want = page_sorted(
            model.iter(),
            cursor.as_deref(),
            limit.unwrap_or(MAX_RESPONSE_RECORDS),
        );
        assert_eq!(
            body["records"],
            as_value(&want.records),
            "page after {cursor:?}"
        );
        assert_eq!(body["next"], json!(want.next), "next after {cursor:?}");
        let page: Vec<WireRecord> = serde_json::from_value(body["records"].clone()).unwrap();
        assert!(page.len() <= MAX_RESPONSE_RECORDS);
        let bytes: u64 = page.iter().map(|r| wire_bytes(r) as u64).sum();
        assert!(
            page.len() == 1 || bytes <= MAX_RESPONSE_BYTES,
            "{bytes} bytes"
        );
        seen.extend(page.into_iter().map(|r| r.uuid));
        match body["next"].as_str() {
            Some(n) => cursor = Some(n.to_string()),
            None => return seen,
        }
    }
    seen
}

fn whole(ns: &str) -> Walk<'_> {
    Walk {
        ns,
        limit: None,
        start: None,
        max_pages: usize::MAX,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn paging_large_namespace() {
    const N: usize = 50_000;
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    // Record 1000k+1 shares record 1000k's hlc, so the tie rule runs at scale too.
    let records: Vec<WireRecord> = (0..N)
        .map(|i| {
            let tick = if i % 1000 == 1 { i - 1 } else { i };
            let wall = 1_700_000_000_000 + tick as i64;
            rec(&format!("u{i:06}"), wall, 0, "node", &format!("ct{i}"))
        })
        .collect();
    for r in &records {
        seed_record(&kv, &a, "big", r).await;
    }
    let model = served(&records);
    assert!(model.iter().filter(|r| r.ord.is_some()).count() >= N / 1000);

    let started = Instant::now();
    let uuids = walk(&s, &kv, SERVICE_LIMITS, whole("big"), &model).await;
    eprintln!("walked {N} records in {:?}", started.elapsed());
    assert_eq!(uuids.len(), N);
    let unique: BTreeSet<&String> = uuids.iter().collect();
    assert_eq!(unique.len(), N, "every uuid exactly once");
    // An odd page size, resuming from a cursor between two stored uuids.
    let mid = Walk {
        ns: "big",
        limit: Some(4_999),
        start: Some("u012345x"),
        max_pages: 2,
    };
    let mid = walk(&s, &kv, SERVICE_LIMITS, mid, &model).await;
    assert_eq!(mid.first().map(String::as_str), Some("u012346"));
    assert_eq!(mid.len(), 2 * 4_999);

    // The account now holds MAX_RECORDS_PER_ACCOUNT records: a new key is refused (507),
    // an update to an existing one still passes.
    assert_eq!(N, MAX_RECORDS_PER_ACCOUNT);
    let push = |r: WireRecord| {
        let body = json!({ "ns": "other", "records": [r] });
        req("POST", "/v1/records", auth("A", "D1", &nonce()), Some(body))
    };
    assert_eq!(s.call(&kv, push(rec("new", 1, 0, "n", "x"))).await.0, 507);
    let update =
        json!({ "ns": "big", "records": [rec("u000005", 1_800_000_000_000, 0, "n", "v2")] });
    let update = req(
        "POST",
        "/v1/records",
        auth("A", "D1", &nonce()),
        Some(update),
    );
    assert_eq!(s.call(&kv, update).await.0, 200);
}

/// A page of full-size records reaches `MAX_RESPONSE_BYTES` before `MAX_RESPONSE_RECORDS`; the
/// service serves it within 64 MB of guest memory.
#[tokio::test(flavor = "multi_thread")]
async fn paging_byte_budget_within_memory_limit() {
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    let ct = "a".repeat(MAX_CT_LEN);
    let records: Vec<WireRecord> = (0..640)
        .map(|i| rec(&format!("f{i:04}"), 1_700_000_000_000 + i, 0, "node", &ct))
        .collect();
    for r in &records {
        seed_record(&kv, &a, "fat", r).await;
    }
    let model = served(&records);
    let first = page_sorted(model.iter(), None, MAX_RESPONSE_RECORDS);
    assert!(first.next.is_some() && first.records.len() < records.len());
    let uuids = walk(&s, &kv, WIDE_LIMITS, whole("fat"), &model).await;
    assert_eq!(uuids.len(), records.len());
}
