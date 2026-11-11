//! `services/aegis-sync` (the Aegis sync server as a Meshnode WASM service) driven in-process
//! through `mesh_runtime` over a `MemKv`: the shared contract vectors from the vendored
//! `aegis-sync-core`, plus the mesh-specific rules (tie `ord`, replay nonces in `kv/local`,
//! the membership gate, the device endpoints, caps, tombstone retention) and paging at scale.
//!
//! Every vector that runs on the mesh runs here. The server-wide caps are reached through a
//! second build of the service with its test-only `test-caps` feature (lowered caps, see
//! `services/aegis-sync/src/caps.rs`); production builds never enable it.

// In-process nodes, for the test over the real replicated store.
mod common;

// The service's own key layout, so seeded state is exactly where the service reads it. Some
// of it (the store bounds, the account prefix) only the service uses.
#[allow(dead_code)]
#[path = "../../../services/aegis-sync/src/keys.rs"]
mod keys;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aegis_sync_core::auth::{AuthToken, MAX_TTL_MS, canonical};
use aegis_sync_core::devices::{AccountFacts, ApprovedFact, DeviceFacts};
use aegis_sync_core::order::{order_bytes, tie_ord};
use aegis_sync_core::page::{Page, page_sorted};
use aegis_sync_core::vector::{self, Host, NOW_MS, Req, Tamper, Vector};
use aegis_sync_core::wire::{
    MAX_CT_LEN, MAX_RECORDS_PER_ACCOUNT, MAX_RESPONSE_BYTES, MAX_RESPONSE_RECORDS, WireRecord,
    hlc_key, wire_bytes,
};
use bytes::Bytes;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::test_fixtures::{aegis_sync_wasm, aegis_sync_wasm_test_caps};
use mesh_runtime::{KvBackend, Loaded, MemKv, RequestCtx, Runtime, RuntimeConfig};
use serde_json::{Value, json};

/// The service's deploy defaults (spec §5.3): 64 MB, 30 s, 8 MiB.
const SERVICE_LIMITS: Limits = Limits {
    memory_mb: 64,
    request_timeout_ms: 30_000,
    max_body_bytes: 8 * 1024 * 1024,
};

/// The service's page byte budget (`records::MESH_MAX_RESPONSE_BYTES`): 6 MiB instead of the
/// core's 32 MiB `MAX_RESPONSE_BYTES`, because the runtime caps response bodies at
/// `max_body_bytes` (8 MiB) too. Every walked page is checked against [`page_mesh`] with this
/// budget, so a different value in the service fails the paging tests.
const MESH_MAX_RESPONSE_BYTES: u64 = 6 * 1024 * 1024;

struct Svc {
    rt: Runtime,
    svc: Loaded,
}

async fn service() -> Svc {
    service_from(aegis_sync_wasm()).await
}

/// The service built with `test-caps`: the caps in [`test_caps`].
async fn service_test_caps() -> Svc {
    service_from(aegis_sync_wasm_test_caps()).await
}

/// The values `services/aegis-sync/src/caps.rs` uses under its `test-caps` feature.
mod test_caps {
    pub const MAX_ACCOUNTS: usize = 1;
    pub const MAX_TOTAL_RECORDS: usize = 4;
    pub const MAX_TOTAL_CT_BYTES: u64 = 40;
    pub const MAX_SEEN_NONCES_PER_DEVICE: usize = 10;
}

async fn service_from(path: std::path::PathBuf) -> Svc {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    rt.set_wall_clock_ms(Some(NOW_MS));
    let wasm = std::fs::read(path).expect("read aegis_sync.wasm");
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
        let (status, raw) = self.send_raw(kv, limits, req).await;
        (status, serde_json::from_slice(&raw).ok())
    }

    /// The status and the raw response body.
    async fn send_raw(&self, kv: &Arc<MemKv>, limits: Limits, req: &Req) -> (u16, Bytes) {
        let backend: Arc<dyn KvBackend> = kv.clone();
        self.send_to(backend, limits, req).await
    }

    /// [`Svc::call`] over any backend (the real store's `ServiceKv`, for one).
    async fn call_on(&self, kv: &Arc<dyn KvBackend>, req: Req) -> (u16, Option<Value>) {
        let (status, raw) = self.send_to(kv.clone(), SERVICE_LIMITS, &req).await;
        (status, serde_json::from_slice(&raw).ok())
    }

    async fn send_to(
        &self,
        backend: Arc<dyn KvBackend>,
        limits: Limits,
        req: &Req,
    ) -> (u16, Bytes) {
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
        let ctx = RequestCtx {
            service: "sync".into(),
            node_id: NodeId([7; 32]),
            kv: backend,
            limits,
            outbound: Vec::new(),
        };
        let resp = self.rt.handle(&self.svc, b.body(body).unwrap(), ctx).await;
        (resp.status().as_u16(), resp.into_body())
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

/// Like [`auth`], but issued at `issued_ms` instead of the vectors' fixed `NOW_MS`.
fn auth_at(acct: &str, dev: &str, nonce: &str, issued_ms: i64) -> Option<String> {
    use ed25519_dalek_v2::Signer;
    let token = AuthToken {
        account_id: vector::account_id(acct),
        device_id: vector::device_id(dev),
        issued_ms,
        expires_ms: issued_ms + MAX_TTL_MS,
        nonce: nonce.into(),
    };
    let sig = vector::device_key(dev).sign(&canonical(&token)).to_bytes();
    let token_hex = hex::encode(serde_json::to_vec(&token).unwrap());
    Some(format!(
        "AegisSig {}.{token_hex}.{}",
        token.account_id,
        hex::encode(sig)
    ))
}

/// The node id every request runs on (`RequestCtx::node_id`), as the service spells it.
fn node_hex() -> String {
    hex::encode([7u8; 32])
}

/// A `POST /v1/devices` registering `dev` on `acct` with `label`.
fn register(acct: &str, dev: &str, label: &str, auth: Option<String>) -> Req {
    let body = json!({
        "accountId": vector::account_id(acct),
        "deviceId": vector::device_id(dev),
        "label": label,
        "accountSig": vector::register_sig(acct, dev),
    });
    req("POST", "/v1/devices", auth, Some(body))
}

/// A `POST` to a device route (`/v1/devices/remove` or `/approve`) naming `target`.
fn act(path: &str, acct: &str, actor: &str, target: &str) -> Req {
    let body = json!({ "deviceId": vector::device_id(target) });
    req("POST", path, auth(acct, actor, &nonce()), Some(body))
}

/// Sets both the guest's wall clock and the `kv/local` TTL clock.
fn set_clock(s: &Svc, kv: &MemKv, now_ms: i64) {
    s.rt.set_wall_clock_ms(Some(now_ms));
    kv.set_now_ms(now_ms as u64);
}

/// This node's share of the per-node counter `metric`.
async fn own_count(kv: &MemKv, metric: &str) -> Option<i64> {
    kv.get(&keys::count_key(&node_hex(), metric))
        .await
        .unwrap()
        .map(|v| String::from_utf8(v).unwrap().parse().unwrap())
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
        (keys::revoked_set(acct), &f.revoked),
    ] {
        for id in ids {
            kv.set_add(&set, id.as_bytes().to_vec()).await.unwrap();
        }
    }
    // Approvals are stored as `ApprovedFact`s, as the approve endpoint writes them.
    for (id, ms) in &f.approved {
        let fact = ApprovedFact {
            device_id: id.clone(),
            approved_ms: *ms,
        };
        let m = serde_json::to_vec(&fact).unwrap();
        kv.set_add(&keys::approved_set(acct), m).await.unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn contract_vectors() {
    let all = vectors();
    let skipped: Vec<&str> = all
        .iter()
        .filter(|v| !v.runs_on(Host::Mesh))
        .map(|v| v.name.as_str())
        .collect();

    let s = service().await;
    let mut ran = Vec::new();
    let mut failures = Vec::new();
    for v in all.iter().filter(|v| v.runs_on(Host::Mesh)) {
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
    assert_eq!(
        skipped,
        ["records_cross_uuid_tie_counter_bump"],
        "only native_only vectors are skipped"
    );
    assert_eq!(ran.len() + skipped.len(), all.len());
    for want in [
        "healthz_ok",
        "auth_replayed_nonce_401",
        "bounds_bad_hlc_400",
        "bounds_label_too_long_413",
        "devices_owner_never_pending_ranks_first",
        "devices_owner_stays_after_later_approval",
        "devices_pending_cap_429",
        "devices_owner_self_remove_handoff",
    ] {
        assert!(ran.contains(&want), "{want} did not run");
    }
    let devices = ran.iter().filter(|n| n.starts_with("devices_")).count();
    assert_eq!(devices, 14, "every devices_* vector runs");
    eprintln!("ran {} vectors: {ran:?}\nskipped: {skipped:?}", ran.len());
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
        approved: [(id("D3"), 3)].into(),
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

/// `page::page_sorted` with the byte budget as a parameter: the same loop, line for line.
/// [`page_mesh_is_page_sorted`] pins it to the core's function at the core's budget; the walks
/// use it with [`MESH_MAX_RESPONSE_BYTES`], the service's budget.
fn page_mesh(sorted: &[WireRecord], cursor: Option<&str>, limit: usize, budget: u64) -> Page {
    let limit = limit.clamp(1, MAX_RESPONSE_RECORDS);
    let mut records: Vec<WireRecord> = Vec::new();
    let mut bytes = 0u64;
    let mut next: Option<String> = None;
    let after_cursor = sorted
        .iter()
        .skip_while(|r| cursor.is_some_and(|c| r.uuid.as_str() <= c));
    for r in after_cursor {
        if records.len() >= limit {
            next = Some(records.last().map(|w| w.uuid.clone()).unwrap_or_default());
            break;
        }
        bytes += wire_bytes(r) as u64;
        if records.is_empty() && bytes > budget {
            records.push(r.clone());
            next = Some(r.uuid.clone());
            break;
        }
        if bytes > budget {
            next = Some(records.last().map(|w| w.uuid.clone()).unwrap_or_default());
            break;
        }
        records.push(r.clone());
    }
    Page { records, next }
}

#[test]
fn page_mesh_is_page_sorted() {
    let small: Vec<WireRecord> = (0..12_000)
        .map(|i| rec(&format!("s{i:05}"), i, 0, "n", "c"))
        .collect();
    let ct = "a".repeat(MAX_CT_LEN);
    let fat: Vec<WireRecord> = (0..600)
        .map(|i| rec(&format!("f{i:04}"), i, 0, "n", &ct))
        .collect();
    let mut huge = vec![rec("h0", 1, 0, "n", "c"), rec("h1", 1, 0, "n", "c")];
    huge[0].ct = "a".repeat(MAX_RESPONSE_BYTES as usize + 1);
    for set in [&small, &fat, &huge] {
        for cursor in [None, Some("f0100"), Some("s05000x"), Some("zz")] {
            for limit in [0, 1, 7, 4_999, MAX_RESPONSE_RECORDS, usize::MAX] {
                let a = page_sorted(set.iter(), cursor, limit);
                let b = page_mesh(set, cursor, limit, MAX_RESPONSE_BYTES);
                assert_eq!(as_value(&a.records), as_value(&b.records));
                assert_eq!(a.next, b.next);
            }
        }
    }
}

/// Walks a namespace page by page, checking every page against [`page_mesh`] at the service's
/// budget over `model` (what the mesh should serve, see [`served`]) and every raw response body
/// against that budget; returns the uuids in walk order.
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
        let (status, raw) = s
            .send_raw(
                kv,
                limits,
                &req("GET", &path, auth("A", "D1", &nonce()), None),
            )
            .await;
        let took = started.elapsed();
        assert_eq!(status, 200, "GET {path}");
        assert!(took < Duration::from_secs(30), "GET {path} took {took:?}");
        assert!(
            raw.len() as u64 <= MESH_MAX_RESPONSE_BYTES,
            "GET {path}: a {} byte body",
            raw.len()
        );
        let body: Value = serde_json::from_slice(&raw).unwrap();
        let want = page_mesh(
            model,
            cursor.as_deref(),
            limit.unwrap_or(MAX_RESPONSE_RECORDS),
            MESH_MAX_RESPONSE_BYTES,
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
            page.len() == 1 || bytes <= MESH_MAX_RESPONSE_BYTES,
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

/// A namespace whose records add up to far more than the 6 MiB page budget (640 records of
/// 64 KiB ciphertext, about 40 MiB) is served across several pages under the service's
/// default limits (64 MB, 30 s, 8 MiB body): every record exactly once, every response body at
/// most 6 MiB.
#[tokio::test(flavor = "multi_thread")]
async fn paging_byte_budget_spans_pages() {
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
    let total: u64 = records.iter().map(|r| wire_bytes(r) as u64).sum();
    assert!(total > 6 * MESH_MAX_RESPONSE_BYTES, "{total} bytes");
    let model = served(&records);
    let first = page_mesh(&model, None, MAX_RESPONSE_RECORDS, MESH_MAX_RESPONSE_BYTES);
    assert!(first.next.is_some() && first.records.len() < 100);
    let uuids = walk(&s, &kv, SERVICE_LIMITS, whole("fat"), &model).await;
    assert_eq!(uuids.len(), records.len());
    let unique: BTreeSet<&String> = uuids.iter().collect();
    assert_eq!(unique.len(), records.len(), "every record exactly once");
}

/// A NUL anywhere in `ns`, a uuid or a record nonce is a 400 before canonicalisation. A uuid
/// spelled like `canonicalize_body`'s internal empty-id slot ("\0empty:N") would otherwise be
/// treated as missing and given a synthesized id, which may be an existing record's.
#[tokio::test(flavor = "multi_thread")]
async fn nul_in_ns_uuid_or_nonce_is_refused() {
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    let push = |ns: &str, records: Vec<WireRecord>| {
        let body = json!({ "ns": ns, "records": records });
        req("POST", "/v1/records", auth("A", "D1", &nonce()), Some(body))
    };
    let ok = rec("fine", 1, 0, "n", "x");
    let sentinel = rec("\u{0}empty:1", 1, 0, "n", "x");
    assert_eq!(
        s.call(&kv, push("bm", vec![ok.clone(), sentinel])).await.0,
        400
    );
    let nul_uuid = rec("a\u{0}b", 1, 0, "n", "x");
    assert_eq!(
        s.call(&kv, push("bm", vec![ok.clone(), nul_uuid])).await.0,
        400
    );
    let mut nul_nonce = rec("u", 1, 0, "n", "x");
    nul_nonce.nonce = "a\u{0}".into();
    assert_eq!(
        s.call(&kv, push("bm", vec![ok.clone(), nul_nonce])).await.0,
        400
    );
    assert_eq!(s.call(&kv, push("b\u{0}m", vec![ok.clone()])).await.0, 400);
    let stored = kv.list_keys("rec/", None, 10).await.unwrap();
    assert!(stored.is_empty(), "nothing written: {stored:?}");
    // The same record without the NUL is stored.
    assert_eq!(s.call(&kv, push("bm", vec![ok])).await.0, 200);
    assert_eq!(kv.list_keys("rec/", None, 10).await.unwrap().len(), 1);
}

// ------------------------------------------------------------------------- devices

/// The same token twice on one node: the second request is a replay (`401`), on the device
/// routes as on the record routes.
#[tokio::test(flavor = "multi_thread")]
async fn replay_same_node_401() {
    let s = service().await;
    let kv = fresh_kv();
    let first = auth("A", "D1", "aa01");
    let reg = register("A", "D1", "phone", first.clone());
    assert_eq!(s.call(&kv, reg.clone()).await.0, 200);
    assert_eq!(s.call(&kv, reg).await.0, 401, "a replayed registration");
    let list = req("GET", "/v1/devices", auth("A", "D1", "aa02"), None);
    assert_eq!(s.call(&kv, list.clone()).await.0, 200);
    assert_eq!(s.call(&kv, list).await.0, 401, "a replayed GET /v1/devices");
    let mut approve = act("/v1/devices/approve", "A", "D1", "D2");
    approve.headers = vec![("authorization".into(), auth("A", "D1", "aa03").unwrap())];
    assert_eq!(s.call(&kv, approve.clone()).await.0, 404, "nothing pending");
    assert_eq!(s.call(&kv, approve).await.0, 401, "a replayed approve");
}

/// D5 through the endpoints on one node, with the clock moving. The first device owns the
/// account. When it signs out, the role passes to the EARLIEST-APPROVED joiner: here the one
/// that asked second and has the greater id, so neither request time nor id can pick it. That
/// owner then approves the joiner that asked FIRST, and keeps the role (the review
/// reproduction: ranking joiners by request time handed the account to the device just
/// approved). The owner can act, the other joiner cannot, the last member cannot be removed,
/// and the removed devices are out for good.
#[tokio::test(flavor = "multi_thread")]
async fn owner_handoff_single_node() {
    let s = service().await;
    let kv = fresh_kv();
    let id = |d: &str| vector::device_id(d);
    // `second` asks after `first` and has the greater id; it is approved first.
    let (first, second) = if id("D2") > id("D3") {
        ("D3", "D2")
    } else {
        ("D2", "D3")
    };
    let created = s
        .call(&kv, register("A", "D1", "phone", auth("A", "D1", &nonce())))
        .await;
    assert_eq!(
        created,
        (200, Some(json!({ "ok": true, "pending": false })))
    );
    for (dev, at) in [(first, NOW_MS + 10_000), (second, NOW_MS + 20_000)] {
        set_clock(&s, &kv, at);
        let joined = s
            .call(&kv, register("A", dev, dev, auth("A", dev, &nonce())))
            .await;
        assert_eq!(joined, (200, Some(json!({ "ok": true, "pending": true }))));
    }
    set_clock(&s, &kv, NOW_MS + 30_000);
    // A pending device cannot approve itself or another.
    let r = s
        .call(&kv, act("/v1/devices/approve", "A", first, second))
        .await;
    assert_eq!(r.0, 403);
    let r = s
        .call(&kv, act("/v1/devices/approve", "A", "D1", second))
        .await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    let list = |dev: &str| req("GET", "/v1/devices", auth("A", dev, &nonce()), None);
    let (status, body) = s.call(&kv, list("D1")).await;
    assert_eq!(status, 200);
    let want = json!({
        "devices": [
            { "deviceId": id("D1"), "label": "phone", "registeredMs": NOW_MS },
            { "deviceId": id(second), "label": second, "registeredMs": NOW_MS + 20_000,
              "lastSeenMs": NOW_MS + 30_000 },
        ],
        "pending": [{ "deviceId": id(first), "registeredMs": NOW_MS + 10_000 }],
        "ownerDeviceId": id("D1"),
    });
    assert!(vector::subset(&want, body.as_ref().unwrap()), "{body:?}");

    // The owner signs itself out: the only (so the earliest-) approved joiner inherits.
    let r = s
        .call(&kv, act("/v1/devices/remove", "A", "D1", "D1"))
        .await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    // The new owner approves the device that asked earlier, and stays owner.
    set_clock(&s, &kv, NOW_MS + 40_000);
    let r = s
        .call(&kv, act("/v1/devices/approve", "A", second, first))
        .await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    let body = s.call(&kv, list(first)).await.1.unwrap();
    assert_eq!(body["ownerDeviceId"], json!(id(second)), "{body}");
    assert_eq!(body["devices"].as_array().unwrap().len(), 2);
    // The later-approved joiner is not the owner; the owner is.
    let r = s
        .call(&kv, act("/v1/devices/remove", "A", first, second))
        .await;
    assert_eq!(r.0, 403);
    let r = s
        .call(&kv, act("/v1/devices/remove", "A", second, first))
        .await;
    assert_eq!(r.0, 200);
    // The last member cannot be removed.
    let r = s
        .call(&kv, act("/v1/devices/remove", "A", second, second))
        .await;
    assert_eq!(r.0, 409);
    // The old owner and the removed joiner are out, and cannot register back in.
    for gone in ["D1", first] {
        let get = req("GET", "/v1/records?ns=bm", auth("A", gone, &nonce()), None);
        assert_eq!(s.call(&kv, get).await.0, 403, "{gone} reads records");
        let back = register("A", gone, "again", auth("A", gone, &nonce()));
        assert_eq!(s.call(&kv, back).await.0, 403, "{gone} re-registers");
    }
    set_clock(&s, &kv, NOW_MS);
}

/// `lastSeenMs` is a register per node; the newest across nodes is served, and a
/// re-registration refreshes this node's.
#[tokio::test(flavor = "multi_thread")]
async fn last_seen_is_newest_across_nodes() {
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    let list = || req("GET", "/v1/devices", auth("A", "D1", &nonce()), None);
    let seen = |body: Option<Value>| body.unwrap()["devices"][0]["lastSeenMs"].clone();
    // Never seen: the registration time.
    assert_eq!(seen(s.call(&kv, list()).await.1), json!(1000));
    let other = hex::encode([9u8; 32]);
    kv.set(&keys::seen_key(&a, &d1, &other), b"5000".to_vec())
        .await
        .unwrap();
    kv.set(&keys::seen_key(&a, &d1, &node_hex()), b"3000".to_vec())
        .await
        .unwrap();
    assert_eq!(seen(s.call(&kv, list()).await.1), json!(5000));
    let again = register("A", "D1", "phone", auth("A", "D1", &nonce()));
    assert_eq!(s.call(&kv, again).await.0, 200);
    assert_eq!(seen(s.call(&kv, list()).await.1), json!(NOW_MS));
    let own = kv.get(&keys::seen_key(&a, &d1, &node_hex())).await.unwrap();
    assert_eq!(own, Some(NOW_MS.to_string().into_bytes()));
}

/// The open-registration warning is throttled through `kv/local`: the first request on a
/// node logs it and records that it did.
#[tokio::test(flavor = "multi_thread")]
async fn open_registration_warning_is_recorded_per_node() {
    let s = service().await;
    let kv = fresh_kv();
    assert_eq!(
        kv.local_get(keys::WARNED_OPEN_REGISTRATION).await.unwrap(),
        None
    );
    assert_eq!(s.call(&kv, req("GET", "/healthz", None, None)).await.0, 200);
    assert!(
        kv.local_get(keys::WARNED_OPEN_REGISTRATION)
            .await
            .unwrap()
            .is_some()
    );
}

// ------------------------------------------------------------------------- caps

/// `MAX_ACCOUNTS` (lowered to 1): creating a second account is `507`, and the refusal spends
/// no nonce; joining or re-registering on an existing account is not gated by it.
#[tokio::test(flavor = "multi_thread")]
async fn caps_account_limit_507() {
    assert_eq!(test_caps::MAX_ACCOUNTS, 1);
    let s = service_test_caps().await;
    let kv = fresh_kv();
    let first = register("A", "D1", "phone", auth("A", "D1", &nonce()));
    assert_eq!(s.call(&kv, first).await.0, 200);
    assert_eq!(own_count(&kv, "accounts").await, Some(1));
    let n = nonce();
    let second = register("B", "D2", "laptop", auth("B", "D2", &n));
    assert_eq!(s.call(&kv, second).await.0, 507);
    let d2 = vector::device_id("D2");
    assert_eq!(kv.local_get(&keys::nonce_key(&d2, &n)).await.unwrap(), None);
    assert!(
        kv.list_keys("devices/", None, 10)
            .await
            .unwrap()
            .iter()
            .all(|k| !k.contains(&vector::account_id("B"))),
        "nothing written for B"
    );
    let join = register("A", "D3", "tablet", auth("A", "D3", &nonce()));
    let joined = s.call(&kv, join).await;
    assert_eq!(joined, (200, Some(json!({ "ok": true, "pending": true }))));
    let again = register("A", "D1", "phone 2", auth("A", "D1", &nonce()));
    assert_eq!(s.call(&kv, again).await.0, 200);
    assert_eq!(own_count(&kv, "accounts").await, Some(1));
}

/// `MAX_TOTAL_RECORDS` (4) and `MAX_TOTAL_CT_BYTES` (40), summed over every node's counter:
/// only new keys and ciphertext growth are refused, so a full server still takes updates
/// that keep or shrink a record.
#[tokio::test(flavor = "multi_thread")]
async fn caps_total_records_and_bytes_507() {
    assert_eq!(test_caps::MAX_TOTAL_RECORDS, 4);
    assert_eq!(test_caps::MAX_TOTAL_CT_BYTES, 40);
    let s = service_test_caps().await;
    let kv = fresh_kv();
    let reg = register("A", "D1", "phone", auth("A", "D1", &nonce()));
    assert_eq!(s.call(&kv, reg).await.0, 200);
    let push = |records: Vec<WireRecord>| {
        let body = json!({ "ns": "bm", "records": records });
        req("POST", "/v1/records", auth("A", "D1", &nonce()), Some(body))
    };
    let ten = "0123456789";
    let a = vector::account_id("A");
    let stored = |kv: &Arc<MemKv>, uuid: &str| {
        let key = keys::rec_key(&a, "bm", uuid);
        let kv = kv.clone();
        async move { kv.get(&key).await.unwrap().is_some() }
    };

    let three = vec![
        rec("r1", 1, 0, "n", ten),
        rec("r2", 1, 0, "n", ten),
        rec("r3", 1, 0, "n", ten),
    ];
    assert_eq!(s.call(&kv, push(three)).await.0, 200);
    assert_eq!(own_count(&kv, "records").await, Some(3));
    assert_eq!(own_count(&kv, "ct_bytes").await, Some(30));
    // 3 + 2 new keys > 4: refused whole.
    let two = vec![rec("r4", 1, 0, "n", ""), rec("r5", 1, 0, "n", "")];
    assert_eq!(s.call(&kv, push(two)).await.0, 507);
    assert!(!stored(&kv, "r4").await);
    // Exactly at both caps passes.
    assert_eq!(
        s.call(&kv, push(vec![rec("r4", 1, 0, "n", ten)])).await.0,
        200
    );
    assert_eq!(own_count(&kv, "records").await, Some(4));
    assert_eq!(own_count(&kv, "ct_bytes").await, Some(40));
    // One byte of growth is refused; a shrinking update passes.
    let grow = rec("r1", 2, 0, "n", "0123456789a");
    assert_eq!(s.call(&kv, push(vec![grow])).await.0, 507);
    let shrink = rec("r1", 3, 0, "n", "01234");
    assert_eq!(s.call(&kv, push(vec![shrink])).await.0, 200);
    assert_eq!(own_count(&kv, "ct_bytes").await, Some(35));
    // At the record cap a new key is refused, even an empty one.
    assert_eq!(
        s.call(&kv, push(vec![rec("r5", 1, 0, "n", "")])).await.0,
        507
    );
    // The cap is over the sum of every node's counter: another node that pruned one record
    // (a negative share) makes room for one more.
    let other = hex::encode([9u8; 32]);
    kv.set(&keys::count_key(&other, "records"), b"-1".to_vec())
        .await
        .unwrap();
    assert_eq!(
        s.call(&kv, push(vec![rec("r5", 1, 0, "n", "")])).await.0,
        200
    );
    assert!(stored(&kv, "r5").await);
    assert_eq!(
        s.call(&kv, push(vec![rec("r6", 1, 0, "n", "")])).await.0,
        507
    );
    assert_eq!(own_count(&kv, "records").await, Some(5));
}

/// `MAX_SEEN_NONCES_PER_DEVICE` (lowered to 10): a device holding that many live nonces is
/// refused (`429`) without spending the new one, other devices are not, and the count frees
/// once the nonces expire.
#[tokio::test(flavor = "multi_thread")]
async fn nonce_cap_per_device_429() {
    let cap = test_caps::MAX_SEEN_NONCES_PER_DEVICE;
    assert_eq!(cap, 10);
    let s = service_test_caps().await;
    let kv = fresh_kv();
    let a = vector::account_id("A");
    let (d1, d2) = (vector::device_id("D1"), vector::device_id("D2"));
    seed_member(&kv, &a, &d1).await;
    seed_member(&kv, &a, &d2).await;
    let get = |auth: Option<String>| req("GET", "/v1/records?ns=bm", auth, None);
    for _ in 0..cap {
        let r = s.call(&kv, get(auth("A", "D1", &nonce()))).await;
        assert_eq!(r.0, 200);
    }
    let n = nonce();
    let refused = s.call(&kv, get(auth("A", "D1", &n))).await;
    assert_eq!(refused.0, 429);
    assert_eq!(kv.local_get(&keys::nonce_key(&d1, &n)).await.unwrap(), None);
    // Per device: D2 is not affected.
    assert_eq!(s.call(&kv, get(auth("A", "D2", &nonce()))).await.0, 200);
    // Every counted nonce has expired (ttl 300 001 ms, plus at most one bucket).
    let later = NOW_MS + MAX_TTL_MS + 1 + keys::NONCE_BUCKET_MS;
    set_clock(&s, &kv, later);
    let ok = s.call(&kv, get(auth_at("A", "D1", &n, later))).await;
    assert_eq!(ok.0, 200, "the cap frees as the nonces expire");
    set_clock(&s, &kv, NOW_MS);
}

/// The production cap is `MAX_SEEN_NONCES_PER_DEVICE` (2 000), read from the per-device
/// bucket counts in `kv/local` (seeded here, so no 2 000 requests are needed).
#[tokio::test(flavor = "multi_thread")]
async fn nonce_cap_default_is_2000() {
    use aegis_sync_core::auth::MAX_SEEN_NONCES_PER_DEVICE;
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    // A token issued at NOW is spent for MAX_TTL_MS + 1 (see `auth.rs`).
    let bucket = keys::nonce_bucket(NOW_MS + MAX_TTL_MS + 1);
    let held = (MAX_SEEN_NONCES_PER_DEVICE - 1).to_string();
    kv.local_set(
        &keys::nonce_count_key(&d1, bucket),
        held.into_bytes(),
        MAX_TTL_MS as u64 * 2,
    )
    .await
    .unwrap()
    .unwrap();
    let get = || req("GET", "/v1/records?ns=bm", auth("A", "D1", &nonce()), None);
    assert_eq!(s.call(&kv, get()).await.0, 200, "the 2 000th live nonce");
    assert_eq!(s.call(&kv, get()).await.0, 429, "the 2 001st");
    let count = kv
        .local_get(&keys::nonce_count_key(&d1, bucket))
        .await
        .unwrap();
    assert_eq!(count, Some(b"2000".to_vec()));
}

// ------------------------------------------------------------------------- retention

/// Native retention after a push: per `(account, ns)`, tombstones past the newest 500 AND
/// older than 90 days are pruned; live records never; fresh tombstones never, however many.
#[tokio::test(flavor = "multi_thread")]
async fn retention_prunes_old_tombstones() {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    let s = service().await;
    let kv = fresh_kv();
    let (a, d1) = (vector::account_id("A"), vector::device_id("D1"));
    seed_member(&kv, &a, &d1).await;
    let tomb = |uuid: String, wall: i64| WireRecord {
        deleted: true,
        ct: String::new(),
        ..rec(&uuid, wall, 0, "node", "")
    };
    let push = |ns: &str, records: Vec<WireRecord>| {
        let body = json!({ "ns": ns, "records": records });
        req("POST", "/v1/records", auth("A", "D1", &nonce()), Some(body))
    };
    let served = |ns: &str| {
        let get = req(
            "GET",
            &format!("/v1/records?ns={ns}"),
            auth("A", "D1", &nonce()),
            None,
        );
        let s = &s;
        let kv = kv.clone();
        async move {
            let (status, body) = s.call(&kv, get).await;
            assert_eq!(status, 200);
            let body = body.unwrap();
            assert_eq!(body["next"], Value::Null);
            body["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["uuid"].as_str().unwrap().to_string())
                .collect::<Vec<String>>()
        }
    };
    let old = NOW_MS - 100 * DAY;

    // 600 old tombstones and one old live record: the 100 oldest tombstones go.
    let mut bm: Vec<WireRecord> = (0..600)
        .map(|i| tomb(format!("t{i:04}"), old + i * 1000))
        .collect();
    bm.push(rec("live", old - DAY, 0, "node", "x"));
    assert_eq!(s.call(&kv, push("bm", bm)).await.0, 200);
    let left = served("bm").await;
    assert_eq!(left.len(), 501);
    assert_eq!(left[0], "live", "a live record is never pruned");
    let want: Vec<String> = (100..600).map(|i| format!("t{i:04}")).collect();
    assert_eq!(left[1..], want[..], "the 100 oldest tombstones are gone");
    assert_eq!(
        kv.get(&keys::rec_key(&a, "bm", "t0099")).await.unwrap(),
        None
    );
    let index = kv
        .list_keys(&keys::tomb_prefix(&a, "bm"), None, 1000)
        .await
        .unwrap();
    assert_eq!(index.len(), 500, "pruned tombstones leave the index");

    // 600 tombstones younger than 90 days: all kept.
    let fresh: Vec<WireRecord> = (0..600)
        .map(|i| tomb(format!("f{i:04}"), NOW_MS - DAY + i))
        .collect();
    assert_eq!(s.call(&kv, push("fresh", fresh)).await.0, 200);
    assert_eq!(served("fresh").await.len(), 600);

    // 450 old + 100 fresh: 50 over the window, and the oldest 50 are old enough.
    let mut mix: Vec<WireRecord> = (0..450)
        .map(|i| tomb(format!("m{i:04}"), old + i))
        .collect();
    mix.extend((450..550).map(|i| tomb(format!("m{i:04}"), NOW_MS - DAY + i)));
    assert_eq!(s.call(&kv, push("mix", mix)).await.0, 200);
    let left = served("mix").await;
    assert_eq!(left.len(), 500);
    assert_eq!(left.first().map(String::as_str), Some("m0050"));

    // A record moved off a tombstone leaves the index; a later push prunes nothing more.
    let revived = rec("t0100", NOW_MS - DAY, 0, "node", "back");
    assert_eq!(s.call(&kv, push("bm", vec![revived])).await.0, 200);
    let index = kv
        .list_keys(&keys::tomb_prefix(&a, "bm"), None, 1000)
        .await
        .unwrap();
    assert_eq!(index.len(), 499);
    assert_eq!(served("bm").await.len(), 501);

    // This node's record counter: every key written, less every key pruned.
    let records = 601 - 100 + 600 + 550 - 50;
    assert_eq!(own_count(&kv, "records").await, Some(records));

    // A stale copy of a pruned uuid (an hlc older than its tombstone's) answers 200 but is not
    // stored (the store keeps the pruned key's ordered tombstone), and re-pushing it, however
    // often, grows neither counter nor the tombstone index.
    let bytes = own_count(&kv, "ct_bytes").await;
    let stale = rec("t0000", old - DAY, 0, "node", "stale ciphertext");
    let stale_tomb = tomb("t0001".into(), old - DAY);
    for _ in 0..3 {
        let r = s
            .call(&kv, push("bm", vec![stale.clone(), stale_tomb.clone()]))
            .await;
        assert_eq!(r.0, 200);
    }
    assert_eq!(own_count(&kv, "records").await, Some(records));
    assert_eq!(own_count(&kv, "ct_bytes").await, bytes);
    assert_eq!(
        kv.get(&keys::rec_key(&a, "bm", "t0000")).await.unwrap(),
        None
    );
    let left = served("bm").await;
    assert!(
        !left.iter().any(|u| u == "t0000" || u == "t0001"),
        "{left:?}"
    );
    let index = kv
        .list_keys(&keys::tomb_prefix(&a, "bm"), None, 1000)
        .await
        .unwrap();
    assert_eq!(
        index.len(),
        499,
        "no index key for a write the store refused"
    );
}

// ------------------------------------------------------------------------- real store

/// Retention's ordered delete on the REAL replicated store (`mesh-store` through
/// `ServiceKv`), not MemKv. After a prune, a stale older copy of the pruned uuid answers 200
/// but is not resurrected: the key keeps the ordered mesh tombstone, the record stays out of
/// GET, and re-pushing it grows no counter. A copy NEWER than the pruned tombstone is stored,
/// as natively.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_store_prune_is_not_resurrected_by_a_stale_push() {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    let dir = tempfile::tempdir().unwrap();
    let node = mesh_core::Node::create_mesh(common::test_cfg(dir.path(), "a"))
        .await
        .unwrap();
    let kv: Arc<dyn KvBackend> = Arc::new(mesh_services::ServiceKv::new(
        node.store().clone(),
        "sync".into(),
    ));
    let s = service().await;
    let a = vector::account_id("A");
    let reg = register("A", "D1", "phone", auth("A", "D1", &nonce()));
    assert_eq!(s.call_on(&kv, reg).await.0, 200);
    let push = |records: Vec<WireRecord>| {
        let body = json!({ "ns": "bm", "records": records });
        req("POST", "/v1/records", auth("A", "D1", &nonce()), Some(body))
    };
    let served = || async {
        let get = req("GET", "/v1/records?ns=bm", auth("A", "D1", &nonce()), None);
        let (status, body) = s.call_on(&kv, get).await;
        assert_eq!(status, 200);
        body.unwrap()["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uuid"].as_str().unwrap().to_string())
            .collect::<Vec<String>>()
    };
    let records_count = || async {
        kv.get(&keys::count_key(&node_hex(), "records"))
            .await
            .unwrap()
            .map(|v| String::from_utf8(v).unwrap())
    };
    let old = NOW_MS - 100 * DAY;

    // 501 tombstones past the age floor: the oldest one is pruned.
    let tombs: Vec<WireRecord> = (0..501)
        .map(|i| WireRecord {
            deleted: true,
            ..rec(&format!("t{i:04}"), old + i * 1000, 0, "node", "")
        })
        .collect();
    assert_eq!(s.call_on(&kv, push(tombs)).await.0, 200);
    let left = served().await;
    assert_eq!(left.len(), 500);
    assert_eq!(left.first().map(String::as_str), Some("t0001"));
    let key = keys::rec_key(&a, "bm", "t0000");
    assert_eq!(
        kv.get(&key).await.unwrap(),
        None,
        "pruned on the real store"
    );
    assert_eq!(records_count().await.as_deref(), Some("500"));

    // A stale (older) live copy of the pruned uuid, pushed three times.
    let stale = rec("t0000", old - DAY, 0, "node", "stale ciphertext");
    for _ in 0..3 {
        assert_eq!(s.call_on(&kv, push(vec![stale.clone()])).await.0, 200);
    }
    assert_eq!(kv.get(&key).await.unwrap(), None, "not resurrected");
    assert!(!served().await.contains(&"t0000".to_string()));
    assert_eq!(records_count().await.as_deref(), Some("500"), "no growth");

    // A copy newer than the pruned tombstone is stored.
    let newer = rec("t0000", NOW_MS - DAY, 0, "node", "back");
    assert_eq!(s.call_on(&kv, push(vec![newer])).await.0, 200);
    assert!(kv.get(&key).await.unwrap().is_some());
    assert_eq!(served().await.first().map(String::as_str), Some("t0000"));
    assert_eq!(records_count().await.as_deref(), Some("501"));

    node.shutdown().await;
}
