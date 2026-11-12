//! `services/aegis-sync` on two in-process nodes of one mesh (spec §8.4): the service runs
//! through `mesh_runtime` on each node over that node's replicated store (`ServiceKv`), and the
//! nodes sync through the mesh. Each test drives both nodes and checks that they converge:
//! record divergence (edit against tombstone, equal hlcs), stale pushes, split first
//! registration, owner handoff, revocation, per-node replay nonces and `kv/local`, and the time
//! a full page takes on the real store.
//!
//! Sync is awaited deterministically: [`Pair::synced`] asks both nodes for a round and waits
//! (bounded) until each node's version vector covers what the other had written. No test sleeps
//! for a fixed time.

mod common;

// The service's own key layout, so the tests look exactly where the service writes.
#[allow(dead_code)]
#[path = "../../../services/aegis-sync/src/keys.rs"]
mod keys;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aegis_sync_core::order::tie_ord;
use aegis_sync_core::vector::{self, NOW_MS, Req, Tamper};
use aegis_sync_core::wire::{MAX_RECORDS_PER_REQUEST, MAX_RESPONSE_RECORDS, WireRecord, hlc_key};
use bytes::Bytes;
use common::{T, test_cfg, wait_until};
use mesh_core::Node;
use mesh_proto::{Limits, NodeId, Role, VersionVector};
use mesh_runtime::test_fixtures::aegis_sync_wasm;
use mesh_runtime::{KvBackend, Loaded, RequestCtx, Runtime, RuntimeConfig};
use mesh_services::{ServiceKv, data_ns};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

/// The service's deploy defaults (spec §5.3): 64 MB, 30 s, 8 MiB.
const SERVICE_LIMITS: Limits = Limits {
    memory_mb: 64,
    request_timeout_ms: 30_000,
    max_body_bytes: 8 * 1024 * 1024,
};

/// One node of the pair with the service loaded on it.
struct Side {
    node: Node,
    kv: Arc<dyn KvBackend>,
    rt: Runtime,
    svc: Loaded,
    _dir: TempDir,
}

impl Side {
    async fn new(node: Node, dir: TempDir, wasm: &[u8]) -> Side {
        let kv: Arc<dyn KvBackend> =
            Arc::new(ServiceKv::new(node.store().clone(), data_ns("sync", 0)));
        let rt = Runtime::new(RuntimeConfig::default()).unwrap();
        rt.set_wall_clock_ms(Some(NOW_MS));
        let svc = rt.load([0xa5; 32], wasm).await.expect("load aegis-sync");
        Side {
            node,
            kv,
            rt,
            svc,
            _dir: dir,
        }
    }

    fn id(&self) -> NodeId {
        self.node.node_id()
    }

    /// The guest's wall clock on this node (the store keeps its own).
    fn set_clock(&self, now_ms: i64) {
        self.rt.set_wall_clock_ms(Some(now_ms));
    }

    async fn send(&self, req: &Req) -> (u16, Bytes) {
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
            node_id: self.id(),
            kv: self.kv.clone(),
            limits: SERVICE_LIMITS,
            outbound: Vec::new(),
        };
        let resp = self.rt.handle(&self.svc, b.body(body).unwrap(), ctx).await;
        (resp.status().as_u16(), resp.into_body())
    }

    async fn call(&self, req: Req) -> (u16, Option<Value>) {
        let (status, raw) = self.send(&req).await;
        (status, serde_json::from_slice(&raw).ok())
    }

    /// `GET /v1/devices` as `dev`; the body.
    async fn devices(&self, acct: &str, dev: &str) -> Value {
        let (status, body) = self
            .call(req("GET", "/v1/devices", auth(acct, dev, &nonce()), None))
            .await;
        assert_eq!(status, 200, "GET /v1/devices as {dev}");
        body.unwrap()
    }

    /// The whole of `ns` as served to `dev` (one page; the tests stay under a page).
    async fn records(&self, acct: &str, dev: &str, ns: &str) -> Value {
        let path = format!("/v1/records?ns={ns}");
        let (status, body) = self
            .call(req("GET", &path, auth(acct, dev, &nonce()), None))
            .await;
        assert_eq!(status, 200, "GET {path} as {dev}");
        let body = body.unwrap();
        assert_eq!(body["next"], Value::Null, "one page");
        body["records"].clone()
    }

    async fn push(&self, acct: &str, dev: &str, ns: &str, records: &[WireRecord]) -> u16 {
        let body = json!({ "ns": ns, "records": records });
        self.call(req(
            "POST",
            "/v1/records",
            auth(acct, dev, &nonce()),
            Some(body),
        ))
        .await
        .0
    }

    async fn vv(&self) -> VersionVector {
        self.node
            .store()
            .call(|r| r.version_vector())
            .await
            .unwrap()
    }
}

/// Two hosts of one mesh, each running the service.
struct Pair {
    a: Side,
    b: Side,
}

async fn pair() -> Pair {
    let wasm = std::fs::read(aegis_sync_wasm()).expect("read aegis_sync.wasm");
    let dir_a = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(dir_a.path(), "a"))
        .await
        .unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let ticket = a
        .invite(Role::Host, Duration::from_secs(900))
        .await
        .unwrap();
    let b = timeout(T, Node::join(test_cfg(dir_b.path(), "b"), ticket))
        .await
        .expect("join timed out")
        .unwrap();
    assert!(b.wait_first_sync(T).await);
    let (ia, ib) = (a.node_id(), b.node_id());
    wait_until("A and B online to each other", T, || {
        online(&a, ib) && online(&b, ia)
    })
    .await;
    Pair {
        a: Side::new(a, dir_a, &wasm).await,
        b: Side::new(b, dir_b, &wasm).await,
    }
}

fn online(node: &Node, id: NodeId) -> bool {
    node.members().iter().any(|m| m.id == id && m.online)
}

/// `have` holds every op `want` names.
fn covers(have: &VersionVector, want: &VersionVector) -> bool {
    want.0.iter().all(|(id, seq)| have.get(id) >= *seq)
}

impl Pair {
    /// Waits until each node holds every op the other had written when this was called.
    /// Asks both for a round now, and again while waiting, so the wait does not depend on the
    /// sync interval; bounded by [`T`].
    async fn synced(&self) {
        let (want_a, want_b) = (self.b.vv().await, self.a.vv().await);
        let deadline = tokio::time::Instant::now() + T;
        loop {
            if covers(&self.a.vv().await, &want_a) && covers(&self.b.vv().await, &want_b) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for A and B to sync");
            }
            self.a.node.sync_now();
            self.b.node.sync_now();
            sleep(Duration::from_millis(25)).await;
        }
    }

    /// Cuts A off from B both ways.
    async fn split(&self) {
        let (ia, ib) = (self.a.id(), self.b.id());
        self.a.node.partition(&[ib]);
        self.b.node.partition(&[ia]);
        wait_until("A and B apart", T, || {
            !online(&self.a.node, ib) && !online(&self.b.node, ia)
        })
        .await;
    }

    async fn heal(&self) {
        self.a.node.heal();
        self.b.node.heal();
        let (ia, ib) = (self.a.id(), self.b.id());
        wait_until("A and B back", T, || {
            online(&self.a.node, ib) && online(&self.b.node, ia)
        })
        .await;
        self.synced().await;
    }

    fn both(&self) -> [(&'static str, &Side); 2] {
        [("A", &self.a), ("B", &self.b)]
    }

    async fn shutdown(self) {
        self.a.node.shutdown().await;
        self.b.node.shutdown().await;
    }
}

// ------------------------------------------------------------------------- requests

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

/// A `POST /v1/devices` registering `dev` on `acct` with `label`.
fn register(acct: &str, dev: &str, label: &str) -> Req {
    let body = json!({
        "accountId": vector::account_id(acct),
        "deviceId": vector::device_id(dev),
        "label": label,
        "accountSig": vector::register_sig(acct, dev),
    });
    req("POST", "/v1/devices", auth(acct, dev, &nonce()), Some(body))
}

/// A `POST` to a device route (`/v1/devices/remove` or `/approve`) naming `target`.
fn act(path: &str, acct: &str, actor: &str, target: &str) -> Req {
    let body = json!({ "deviceId": vector::device_id(target) });
    req("POST", path, auth(acct, actor, &nonce()), Some(body))
}

/// A fresh hex nonce per call.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0x9000);
    format!("{:x}", N.fetch_add(1, Ordering::Relaxed))
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

fn tomb(uuid: &str, wall: i64, node: &str) -> WireRecord {
    WireRecord {
        deleted: true,
        ..rec(uuid, wall, 0, node, "")
    }
}

/// What the mesh serves for one namespace: uuid order, with the tie `ord` on every record
/// that shares its full `hlc` with a lower uuid.
fn served(records: &[WireRecord]) -> Value {
    let mut sorted: Vec<WireRecord> = records.to_vec();
    sorted.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    let mut lowest: BTreeMap<(i64, u64, String), String> = BTreeMap::new();
    for r in &sorted {
        let (w, c, n) = hlc_key(&r.hlc);
        lowest
            .entry((w, c, n.to_string()))
            .or_insert_with(|| r.uuid.clone());
    }
    let page: Vec<WireRecord> = sorted
        .into_iter()
        .map(|mut r| {
            let (w, c, n) = hlc_key(&r.hlc);
            r.ord = (lowest[&(w, c, n.to_string())] != r.uuid).then(|| tie_ord(&r));
            r
        })
        .collect();
    serde_json::to_value(page).unwrap()
}

/// A, with D1 the only member of account A, registered on node A and synced to B.
async fn pair_with_member() -> Pair {
    let p = pair().await;
    assert_eq!(p.a.call(register("A", "D1", "phone")).await.0, 200);
    p.synced().await;
    p
}

/// `(earlier, later)` of two device names such that `earlier` has the GREATER id, so a test
/// that orders them by time cannot pass by id order.
fn by_greater_id(x: &'static str, y: &'static str) -> (&'static str, &'static str) {
    if vector::device_id(x) > vector::device_id(y) {
        (x, y)
    } else {
        (y, x)
    }
}

// ------------------------------------------------------------------------- records

/// Concurrent edit and delete of one uuid during a split: the higher hlc wins on both nodes
/// after the heal, whichever side wrote it (a newer tombstone over an edit, and a newer edit
/// over a tombstone).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn divergence_edit_vs_tombstone() {
    let p = pair_with_member().await;
    let base = [
        rec("u1", NOW_MS - 10_000, 0, "n", "v0"),
        rec("u2", NOW_MS - 10_000, 0, "n", "v0"),
    ];
    assert_eq!(p.a.push("A", "D1", "bm", &base).await, 200);
    p.synced().await;

    p.split().await;
    // u1: A edits at -5 s, B deletes at -4 s (the tombstone is newer).
    // u2: A deletes at -6 s, B edits at -3 s (the edit is newer).
    // Every hlc is distinct, so no tie `ord` is involved.
    let a_side = [
        rec("u1", NOW_MS - 5_000, 0, "n", "edited on A"),
        tomb("u2", NOW_MS - 6_000, "n"),
    ];
    let b_side = [
        tomb("u1", NOW_MS - 4_000, "n"),
        rec("u2", NOW_MS - 3_000, 0, "n", "edited on B"),
    ];
    assert_eq!(p.a.push("A", "D1", "bm", &a_side).await, 200);
    assert_eq!(p.b.push("A", "D1", "bm", &b_side).await, 200);
    // Apart, each node serves its own writes.
    assert_eq!(p.a.records("A", "D1", "bm").await, served(&a_side));
    assert_eq!(p.b.records("A", "D1", "bm").await, served(&b_side));

    p.heal().await;
    let want = served(&b_side);
    for (name, side) in p.both() {
        assert_eq!(side.records("A", "D1", "bm").await, want, "node {name}");
    }
    p.shutdown().await;
}

/// Two uuids pushed with the same full hlc, one on each node during a split: both nodes serve
/// both records with the same `ord`s (none on the lower uuid, `tie_ord` on the other).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn divergence_equal_hlc_different_uuid() {
    let p = pair_with_member().await;
    p.split().await;
    let on_a = rec("u-a", NOW_MS - 1_000, 3, "n", "from A");
    let on_b = rec("u-b", NOW_MS - 1_000, 3, "n", "from B");
    assert_eq!(
        p.a.push("A", "D1", "bm", std::slice::from_ref(&on_a)).await,
        200
    );
    assert_eq!(
        p.b.push("A", "D1", "bm", std::slice::from_ref(&on_b)).await,
        200
    );
    // Apart, neither knows of the tie: no `ord` at all.
    assert_eq!(
        p.a.records("A", "D1", "bm").await,
        served(std::slice::from_ref(&on_a))
    );
    assert_eq!(
        p.b.records("A", "D1", "bm").await,
        served(std::slice::from_ref(&on_b))
    );

    p.heal().await;
    let want = served(&[on_a.clone(), on_b.clone()]);
    assert_eq!(want[0]["ord"], Value::Null, "{want}");
    assert_eq!(want[1]["ord"], json!(tie_ord(&on_b)), "{want}");
    for (name, side) in p.both() {
        assert_eq!(side.records("A", "D1", "bm").await, want, "node {name}");
    }
    p.shutdown().await;
}

/// One uuid pushed with the same full hlc but different content on each node during a split:
/// after the heal both nodes serve the same one of the two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn divergence_equal_hlc_same_uuid_different_content() {
    let p = pair_with_member().await;
    p.split().await;
    let on_a = rec("u1", NOW_MS - 1_000, 0, "n", "content A");
    let on_b = rec("u1", NOW_MS - 1_000, 0, "n", "content B");
    assert_eq!(
        p.a.push("A", "D1", "bm", std::slice::from_ref(&on_a)).await,
        200
    );
    assert_eq!(
        p.b.push("A", "D1", "bm", std::slice::from_ref(&on_b)).await,
        200
    );

    p.heal().await;
    let on_node_a = p.a.records("A", "D1", "bm").await;
    let on_node_b = p.b.records("A", "D1", "bm").await;
    assert_eq!(on_node_a, on_node_b, "the same winner on both nodes");
    assert!(
        on_node_a == served(&[on_a]) || on_node_a == served(&[on_b]),
        "{on_node_a}"
    );
    p.shutdown().await;
}

/// A push older than what sync delivered is answered `200` and changes nothing, on either
/// node, before and after the next sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_push_after_sync_loses() {
    let p = pair_with_member().await;
    let newer = rec("u1", NOW_MS - 1_000, 0, "n", "newer");
    assert_eq!(
        p.a.push("A", "D1", "bm", std::slice::from_ref(&newer))
            .await,
        200
    );
    p.synced().await;
    let want = served(&[newer]);
    assert_eq!(p.b.records("A", "D1", "bm").await, want);

    let stale = [
        rec("u1", NOW_MS - 2_000, 0, "n", "stale"),
        tomb("u1", NOW_MS - 1_500, "n"),
    ];
    for r in &stale {
        assert_eq!(
            p.b.push("A", "D1", "bm", std::slice::from_ref(r)).await,
            200
        );
    }
    assert_eq!(p.b.records("A", "D1", "bm").await, want, "node B");
    p.synced().await;
    for (name, side) in p.both() {
        assert_eq!(side.records("A", "D1", "bm").await, want, "node {name}");
    }
    p.shutdown().await;
}

// ------------------------------------------------------------------------- devices

/// The same new account registered on both sides of a split (spec §5.2's concurrency edge):
/// each node calls its own device the owner while apart; after the heal both nodes agree that
/// the earlier registration owns the account and the other is a member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_first_registration_one_owner_after_heal() {
    let p = pair().await;
    // The earlier registration has the greater id, so only `registered_ms` can pick it.
    let (early, late) = by_greater_id("D1", "D2");
    let id = |d: &str| vector::device_id(d);
    p.split().await;
    p.b.set_clock(NOW_MS + 10_000);
    let reg_a = p.a.call(register("A", early, early)).await;
    let reg_b = p.b.call(register("A", late, late)).await;
    for r in [reg_a, reg_b] {
        assert_eq!(r, (200, Some(json!({ "ok": true, "pending": false }))));
    }
    assert_eq!(
        p.a.devices("A", early).await["ownerDeviceId"],
        json!(id(early))
    );
    assert_eq!(
        p.b.devices("A", late).await["ownerDeviceId"],
        json!(id(late))
    );

    p.heal().await;
    let mut views = Vec::new();
    for (name, side) in p.both() {
        for dev in [early, late] {
            let body = side.devices("A", dev).await;
            assert_eq!(
                body["ownerDeviceId"],
                json!(id(early)),
                "node {name}: {body}"
            );
            assert_eq!(body["pending"], json!([]), "node {name}: {body}");
            assert_eq!(body["devices"].as_array().unwrap().len(), 2, "{body}");
            views.push(body);
        }
    }
    // The two views agree (`lastSeenMs` is per node and may differ; the rest may not).
    let strip = |mut v: Value| {
        for d in v["devices"].as_array_mut().unwrap() {
            d.as_object_mut().unwrap().remove("lastSeenMs");
        }
        v
    };
    let first = strip(views[0].clone());
    for v in &views[1..] {
        assert_eq!(strip(v.clone()), first);
    }
    // On either node, the later device cannot remove the owner; the owner may remove it.
    for (name, side) in p.both() {
        let r = side.call(act("/v1/devices/remove", "A", late, early)).await;
        assert_eq!(r.0, 403, "node {name}");
    }
    let r = p.b.call(act("/v1/devices/remove", "A", early, late)).await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    p.shutdown().await;
}

/// Ownership moves through the mesh: a joiner asks on B, the owner approves it on A, then
/// signs itself out on A; after sync, B names the joiner owner and it acts as owner there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_handoff_across_nodes() {
    let p = pair_with_member().await;
    let id = |d: &str| vector::device_id(d);
    p.b.set_clock(NOW_MS + 5_000);
    let joined = p.b.call(register("A", "D2", "laptop")).await;
    assert_eq!(joined, (200, Some(json!({ "ok": true, "pending": true }))));
    p.synced().await;
    // A sees the request and the owner approves it there.
    let body = p.a.devices("A", "D1").await;
    assert_eq!(body["pending"][0]["deviceId"], json!(id("D2")), "{body}");
    p.a.set_clock(NOW_MS + 10_000);
    let r = p.a.call(act("/v1/devices/approve", "A", "D1", "D2")).await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    p.synced().await;
    // B: D2 is a member now, not the owner.
    let body = p.b.devices("A", "D2").await;
    assert_eq!(body["ownerDeviceId"], json!(id("D1")), "{body}");
    assert_eq!(body["pending"], json!([]), "{body}");

    // The owner signs out on A.
    let r = p.a.call(act("/v1/devices/remove", "A", "D1", "D1")).await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    p.synced().await;
    for (name, side) in p.both() {
        let body = side.devices("A", "D2").await;
        assert_eq!(
            body["ownerDeviceId"],
            json!(id("D2")),
            "node {name}: {body}"
        );
        assert_eq!(body["devices"].as_array().unwrap().len(), 1, "{body}");
        let get = req("GET", "/v1/records?ns=bm", auth("A", "D1", &nonce()), None);
        assert_eq!(side.call(get).await.0, 403, "node {name}: D1 is out");
    }
    // D2 acts as owner on B: a third device asks on A, D2 approves it on B.
    p.a.set_clock(NOW_MS + 20_000);
    assert_eq!(p.a.call(register("A", "D3", "tablet")).await.0, 200);
    p.synced().await;
    p.b.set_clock(NOW_MS + 30_000);
    let r = p.b.call(act("/v1/devices/approve", "A", "D2", "D3")).await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    p.synced().await;
    for (name, side) in p.both() {
        let body = side.devices("A", "D3").await;
        assert_eq!(
            body["ownerDeviceId"],
            json!(id("D2")),
            "node {name}: {body}"
        );
        assert_eq!(body["devices"].as_array().unwrap().len(), 2, "{body}");
        let r = side.call(act("/v1/devices/remove", "A", "D3", "D2")).await;
        assert_eq!(r.0, 403, "node {name}: D3 cannot remove the owner");
    }
    p.shutdown().await;
}

/// A device revoked on A keeps working on B only until B has the revocation: with the nodes
/// apart B still serves it, and once they sync B refuses it (no wall-clock wait involved).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revocation_reaches_other_node() {
    let p = pair_with_member().await;
    assert_eq!(p.b.call(register("A", "D2", "laptop")).await.0, 200);
    p.synced().await;
    let r = p.a.call(act("/v1/devices/approve", "A", "D1", "D2")).await;
    assert_eq!(r.0, 200);
    p.synced().await;
    let get = || req("GET", "/v1/records?ns=bm", auth("A", "D2", &nonce()), None);
    assert_eq!(p.b.call(get()).await.0, 200, "D2 is a member on B");

    p.split().await;
    let r = p.a.call(act("/v1/devices/remove", "A", "D1", "D2")).await;
    assert_eq!(r, (200, Some(json!({ "ok": true }))));
    assert_eq!(p.a.call(get()).await.0, 403, "A refuses D2 at once");
    assert_eq!(p.b.call(get()).await.0, 200, "B has not heard yet");

    p.heal().await;
    for (name, side) in p.both() {
        assert_eq!(side.call(get()).await.0, 403, "node {name}: records");
        let back = side.call(register("A", "D2", "again")).await;
        assert_eq!(back.0, 403, "node {name}: re-register");
        let body = side.devices("A", "D1").await;
        assert_eq!(body["devices"].as_array().unwrap().len(), 1, "{body}");
    }
    p.shutdown().await;
}

// ------------------------------------------------------------------------- node-local state

/// Replay nonces live in `kv/local`, so they are per node (documented: the gateway is
/// loopback-only, so a token cannot be captured off one node and replayed on another). The
/// same token is refused twice on one node, but accepted once on the other node even after a
/// sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_is_per_node() {
    let p = pair_with_member().await;
    let token = auth("A", "D1", &nonce());
    let get = || req("GET", "/v1/devices", token.clone(), None);
    assert_eq!(p.a.call(get()).await.0, 200);
    assert_eq!(p.a.call(get()).await.0, 401, "replayed on A");
    // A replicated write after the spend, so the sync below carries everything A wrote.
    let body = json!({ "ns": "bm", "records": [rec("u1", NOW_MS, 0, "n", "x")] });
    let push = req("POST", "/v1/records", auth("A", "D1", &nonce()), Some(body));
    assert_eq!(p.a.call(push).await.0, 200);
    p.synced().await;
    assert_eq!(p.b.call(get()).await.0, 200, "the same token, once, on B");
    assert_eq!(p.b.call(get()).await.0, 401, "replayed on B");
    assert_eq!(p.a.call(get()).await.0, 401, "still spent on A");
    p.shutdown().await;
}

/// What the service writes to `kv/local` on one node (the spent nonce, the open-registration
/// warning) is not on the other node after a sync, while the replicated writes of the same
/// requests are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_store_not_replicated_across_nodes() {
    let p = pair().await;
    let dev = vector::device_id("D1");
    let n = nonce();
    let body = json!({
        "accountId": vector::account_id("A"),
        "deviceId": dev,
        "label": "phone",
        "accountSig": vector::register_sig("A", "D1"),
    });
    let reg = req("POST", "/v1/devices", auth("A", "D1", &n), Some(body));
    assert_eq!(p.a.call(reg).await.0, 200);
    let nonce_key = keys::nonce_key(&dev, &n);
    let local_keys = [nonce_key.as_str(), keys::WARNED_OPEN_REGISTRATION];
    for k in local_keys {
        assert!(p.a.kv.local_get(k).await.unwrap().is_some(), "{k} on A");
    }
    p.synced().await;
    // The replicated device fact arrived on B ...
    let devices = keys::devices_set(&vector::account_id("A"));
    let on_b = p.b.kv.set_members(&devices, None, 10).await.unwrap();
    assert_eq!(on_b.len(), 1, "the registration replicated");
    // ... the node-local entries did not.
    for k in local_keys {
        assert_eq!(p.b.kv.local_get(k).await.unwrap(), None, "{k} on B");
        assert!(
            p.a.kv.local_get(k).await.unwrap().is_some(),
            "{k} still on A"
        );
    }
    p.shutdown().await;
}

// ------------------------------------------------------------------------- timing

/// A full 5 000-record page served from a real node's replicated store (not `MemKv`), well
/// inside the service's 30 s request limit. One node: replicating 5 000 records to a second
/// node is not what is timed, and under a parallel test run it is slow enough to flake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_full_page_on_real_store_within_limits() {
    const BUDGET: Duration = Duration::from_secs(10);
    let wasm = std::fs::read(aegis_sync_wasm()).expect("read aegis_sync.wasm");
    let dir = tempfile::tempdir().unwrap();
    let node = Node::create_mesh(test_cfg(dir.path(), "a")).await.unwrap();
    let side = Side::new(node, dir, &wasm).await;
    assert_eq!(side.call(register("A", "D1", "phone")).await.0, 200);
    let ct = "c".repeat(200);
    let records: Vec<WireRecord> = (0..MAX_RESPONSE_RECORDS)
        .map(|i| {
            rec(
                &format!("u{i:05}"),
                NOW_MS - 100_000 + i as i64,
                0,
                "n",
                &ct,
            )
        })
        .collect();
    let started = Instant::now();
    for chunk in records.chunks(MAX_RECORDS_PER_REQUEST) {
        assert_eq!(side.push("A", "D1", "big", chunk).await, 200);
    }
    eprintln!(
        "pushed {} records in {:?}",
        records.len(),
        started.elapsed()
    );

    let get = req("GET", "/v1/records?ns=big", auth("A", "D1", &nonce()), None);
    let started = Instant::now();
    let (status, raw) = side.send(&get).await;
    let took = started.elapsed();
    eprintln!(
        "GET of a full {MAX_RESPONSE_RECORDS}-record page ({} bytes) on the real store took {took:?}",
        raw.len()
    );
    assert_eq!(status, 200);
    let body: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(body["records"], served(&records));
    assert_eq!(body["next"], Value::Null);
    assert!(took < BUDGET, "{took:?}, budget {BUDGET:?}");
    side.node.shutdown().await;
}
