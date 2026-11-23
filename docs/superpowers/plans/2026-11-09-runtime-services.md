# Runtime and Services (sub-spec 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `mesh-runtime` (a wasmtime 50 component host with a `KvBackend` trait, limits, outbound allowlist and code cache) and `mesh-services` (manifests, deploy/undeploy/purge, placement, deployer loop, static sites, blob GC) on `mesh-core`'s public API, plus `meshdev` service commands.

**Architecture:** `mesh-runtime` knows nothing about the mesh: it loads components and handles one buffered HTTP request per fresh instance, calling an injected `KvBackend`. `mesh-services` holds a non-owning `NodeHandle`, keeps manifests in `_mesh`, runs a deployer task that fetches blobs and swaps `Arc<Active>` entries, and dispatches requests to the runtime or a native static handler. KV writes go through `mesh-core`'s gated `StoreHandle`.

**Tech Stack:** Rust 2024 (MSRV 1.90), wasmtime / wasmtime-wasi / wasmtime-wasi-http 50.0.x, `http` 1, `bytes`, `async-trait`, tokio, tracing, thiserror; fixtures: `wstd` 0.6 + `wit-bindgen` for `wasm32-wasip2`.

**Spec:** `docs/superpowers/specs/2026-11-09-runtime-services-design.md` (section numbers below refer to it). Also read `docs/superpowers/specs/2026-11-09-node-core-design.md` §3.2 (the `Node` API) and §8 (blobs).

## Global Constraints

- Workspace: `edition = "2024"`, `rust-version = "1.90"`; new deps in `[workspace.dependencies]`, referenced with `.workspace = true`. wasmtime, wasmtime-wasi, wasmtime-wasi-http pinned to `50.0` (the spike's versions).
- Every task ends with `cargo test --workspace --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --all --check` passing.
- Git needs `git -c safe.directory=<meshnode-checkout>`; branch `feat/runtime-services`. Commit trailers as given by your own system instructions.
- `mesh-runtime` must not depend on `mesh-core` or `mesh-store`. `mesh-services` uses only `mesh-core`'s public API (plus Task 7's additions).
- All store access from async code goes through `StoreHandle`; service KV writes only through its gated write methods.
- Defaults: `Limits { memory_mb: 64, request_timeout_ms: 10_000, max_body_bytes: 8 * 1024 * 1024 }`, `Placement::AllHosts`, outbound empty, `max_in_flight = 64`, epoch tick 10 ms, deployer rescan 60 s, retry backoff 5 s → 300 s, blob GC hourly with a 24 h grace, static file cap 64 MiB, stdio line cap 4 KiB, `list-keys`/`set-members` host-side limit cap 1000.
- `_mesh` keys: `service/<name>` (LWW, raw postcard `ServiceManifest`), `service-gen/<name>` (grow-only set of 8-byte big-endian `u64`). Data namespace `<name>` (gen 0) or `<name>.<gen>`.
- Status mapping (spec §3.3, §5.3): 413 body too large; 500 trap / OOM / response too large / KV other; 504 timeout; 507 KV full; 503 KV removed or rebootstrapping; `503 Service not yet replicated`; `503 Service not placed on this node`; `404 Service not deployed`.
- Test API behind `cfg(any(test, feature = "test-hooks"))`; each crate has a `test-hooks` feature and the self dev-dependency pattern used by `mesh-core`.

## Review Focus

1. **Garbage deploy input** (0 bytes, a PNG, a core wasm module that is not a component) must fail `deploy_wasm` with `InvalidComponent` and write no manifest and pin nothing new. Test: Task 8 `garbage_bytes_rejected`.
2. **Percent-encoded traversal** (`/%2e%2e/secret`, `/a/%2F../b`, `/..%5c`) must give `400`, never a file outside the collection. Test: Task 10 `encoded_traversal_rejected`.
3. **A huge `limit` in `list-keys` / `set-members`** (`u32::MAX`) must be capped at 1000 host-side, not allocate unboundedly. Test: Task 3 `list_limit_capped`.
4. **A KV value over the store's 1 MiB limit** must give `500` for that request and leave the node and other requests fine. Test: Task 3 `oversized_kv_value_gives_500`.
5. **Node restart with services deployed** must bring every placed service back to `Ready` (cache hit for WASM) without a redeploy. Test: Task 9 `services_resume_after_restart`.

---

### Task 1: Proto — `ServiceManifest` and friends

**Files:** Create `crates/mesh-proto/src/service.rs`; modify `crates/mesh-proto/src/lib.rs`.

**Interfaces — Produces:** `ServiceManifest { name: String, kind: ServiceKind, version: u64, limits: Limits, placement: Placement, outbound: Vec<String> }`, `ServiceKind::{Wasm { module: [u8; 32] }, Static { root: [u8; 32] }}`, `Placement::{AllHosts, Tags(Vec<String>), Nodes(Vec<NodeId>)}`, `Limits { memory_mb: u32, request_timeout_ms: u64, max_body_bytes: u64 }` with `Default` = spec values; `pub fn valid_service_name(&str) -> bool`; `ServiceManifest::validate(&self) -> Result<(), ProtoError>` (spec §4.1 bounds); decode-time validation via the crate's existing stash pattern; `pub fn encode_raw / decode_raw` helpers for `_mesh` raw postcard (no version byte) if not already present.

- [ ] **Step 1: Failing tests** in `service.rs`: round-trip via raw postcard; `valid_service_name` accepts `a`, `my-svc-1`, 32 chars; rejects ``, `A`, `_mesh`, `a.b`, 33 chars; `validate` rejects memory 0 and 1025, timeout 99 and 300_001, body > 64 MiB, 65 outbound, outbound `"host"` (no port), `":80"`, `"h:99999"`, 65 tags, a 33-byte tag; decode of an invalid manifest → `ProtoError::Limit`.
- [ ] **Step 2: Run** `cargo test -p mesh-proto service` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p mesh-proto` — PASS.
- [ ] **Step 5: Commit** `feat(proto): ServiceManifest, Placement, Limits`.

### Task 2: `mesh-runtime` scaffold, WIT, fixture builder, load + basic handle

**Files:** Create `crates/mesh-runtime/{Cargo.toml, src/lib.rs, src/error.rs, src/engine.rs, src/host.rs}`, `crates/mesh-runtime/wit/{world.wit, deps/...}`, `crates/mesh-runtime/tests/fixtures/{Cargo.toml (workspace), echo/, bad/}`, `crates/mesh-runtime/tests/common/mod.rs` (fixture builder), `crates/mesh-runtime/tests/runtime.rs`. Modify root `Cargo.toml` (workspace deps; `exclude` the fixtures workspace).

**Interfaces — Produces:** `Runtime::new(RuntimeConfig) -> Result<Runtime, RuntimeError>`, `Runtime::load(&self, hash: [u8;32], wasm: &[u8]) -> Result<Loaded, RuntimeError>`, `Runtime::handle(&self, &Loaded, http::Request<Bytes>, RequestCtx) -> http::Response<Bytes>`, `RuntimeConfig { cache: CodeCache, max_in_flight: usize }` (`Default`: `CodeCache::None`, 64), `CodeCache::{None, Mmap(PathBuf), Read(PathBuf)}`, `RequestCtx { service, node_id: NodeId, kv: Arc<dyn KvBackend>, limits: mesh_proto::Limits, outbound: Vec<String> }`, `KvBackend` trait and `KvError` exactly as spec §3.1 (`async-trait`), `RuntimeError::{Invalid, Compile, Cache}`, `pub struct MemKv` (in-memory `KvBackend`) behind `test-hooks` for tests in this and later crates. Test helper `fixture(name) -> Vec<u8>`: builds `tests/fixtures` once per process with `cargo build --release --target wasm32-wasip2 --manifest-path …` into `target/fixtures`, guarded by a file lock (`fs4` or a lock file + retry) so parallel test binaries don't race; returns the bytes of `<name>.wasm`.

Notes: follow the spike's host pattern (`spikes/android/spike/src/lib.rs`: `WasiView`, `WasiHttpView`, `ProxyPre`, `add_to_linker_async`, `add_only_http_to_linker_async`). Engine config: `async_support`, `epoch_interruption`, Cranelift. WIT: package `meshnode:kv` (spec/master §7 interface) and world `meshnode:service/service` (exports `wasi:http/incoming-handler`, imports `meshnode:kv/store` + the allowed wasi interfaces). The host gets KV bindings via `wasmtime::component::bindgen!` (KV linking is wired in Task 3; here link a stub that traps). WASI context: no preopens, no env, no args, stdout/stderr captured (piped into `tracing` in Task 5; here an inherit-nothing sink).

- [ ] **Step 1: Fixtures** `echo` (returns the request body; `?size=N` returns N bytes of `a`; prints one line to stdout and stderr) and `bad` (imports a non-world interface, e.g. `wasi:filesystem/preopens`). Guests use `wstd` 0.6 `#[http_server]` (+ `wit-bindgen` for `meshnode:kv` in later fixtures).
- [ ] **Step 2: Failing tests:** `echo_round_trip` (POST body comes back, status 200); `invalid_component_rejected_at_load` (`bad` → `RuntimeError::Invalid`); `garbage_rejected_at_load` (empty bytes and `b"\0asm\x01\0\0\0"` core module → `Invalid` or `Compile`).
- [ ] **Step 3: Run** `cargo test -p mesh-runtime --all-features --test runtime` — FAIL.
- [ ] **Step 4: Implement.**
- [ ] **Step 5: Run** the tests 2× (second run hits the built fixtures) and the full workspace suite — PASS.
- [ ] **Step 6: Commit** `feat(runtime): wasmtime host scaffold, world, fixtures, load and handle`.

### Task 3: `meshnode:kv` binding

**Files:** Modify `crates/mesh-runtime/src/host.rs` (+ `src/kv.rs`); add fixture `counter`; tests in `tests/kv.rs`.

**Interfaces — Consumes:** Task 2. **Produces:** host implementation of every `meshnode:kv/store` function calling `RequestCtx.kv`; `node-id` returns `ctx.node_id.0`; a backend error stores the mapped status in host state and traps (spec §3.3 table); `list-keys` / `set-members` clamp `limit` to 1000.

- [ ] **Step 1: Fixture `counter`** with the routes in spec §8.1.
- [ ] **Step 2: Failing tests:** `kv_semantics_all_calls` (every route against `MemKv`, values checked); `isolation_two_services_two_namespaces` (two `MemKv`s, writes via one invisible to the other); `kv_errors_map_to_507_503_500` (a `MemKv` test mode returning each `KvError` → 507/503/503/500); `list_limit_capped` (Review Focus 3: insert 1500 keys, `/list?limit=4294967295` returns 1000); `oversized_kv_value_gives_500` (Review Focus 4: `MemKv` configured with a 1 MiB value limit returning `Other`; a 2 MiB `/set` → 500; next request OK).
- [ ] **Step 3: Run** `cargo test -p mesh-runtime --all-features --test kv` — FAIL.
- [ ] **Step 4: Implement.**
- [ ] **Step 5: Run** — PASS.
- [ ] **Step 6: Commit** `feat(runtime): meshnode:kv host binding`.

### Task 4: Limits — time, memory, bodies, in-flight

**Files:** Modify `crates/mesh-runtime/src/{engine.rs, host.rs}`; fixtures `spin`, `hog`; tests in `tests/limits.rs`.

**Interfaces — Produces:** epoch ticker task (10 ms) owned by `Runtime` (stops when `Runtime` drops); per-request epoch deadline; `StoreLimits` memory cap; outer `tokio::time::timeout`; request-body check before instantiation; response-body cap; `Semaphore(max_in_flight)` with waiting counted against the timeout.

- [ ] **Step 1: Failing tests:** `timeout_gives_504_and_node_survives` (`spin` with 200 ms timeout → 504 within 1 s; a following `echo` → 200); `memory_cap_gives_500` (`hog` with 16 MB → 500; next request fine); `body_cap_gives_413_before_instantiation` (body = cap + 1 → 413, and a counting `KvBackend`/hook proves no instantiation); `oversized_response_gives_500` (`echo?size=` cap+1 → 500); `in_flight_cap_queues_then_times_out` (`max_in_flight = 1`, one `spin` with 2 s timeout in flight, a second request with 300 ms timeout → 504).
- [ ] **Step 2: Run** `cargo test -p mesh-runtime --all-features --test limits` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** 3× — PASS.
- [ ] **Step 5: Commit** `feat(runtime): time, memory, body and concurrency limits`.

### Task 5: Outbound allowlist and stdio logging

**Files:** Modify `crates/mesh-runtime/src/host.rs` (+ `src/outbound.rs`, `src/stdio.rs`); fixture `outbound`; tests `tests/outbound.rs`.

**Interfaces — Produces:** `WasiHttpHooks::send_request` implementation (check wasmtime-wasi-http 50.0.1's `WasiHttpHooks` trait in the registry source): exact case-insensitive `host:port` match with default ports 443/80, denied → `ErrorCode::HttpRequestDenied` + `warn!(service, dest)`; allowed → the default send. Stdio: a `StdoutStream` that line-buffers into `tracing` (`info` stdout / `warn` stderr, field `service`, lines truncated at 4 KiB).

- [ ] **Step 1: Failing tests:** `outbound_allowed_vs_denied` (a local hyper/axum-less `tokio::net` HTTP responder on `127.0.0.1:<port>`; allowlist `127.0.0.1:<port>` → 200 relayed; empty allowlist → denied text; `localhost:<port>` vs `127.0.0.1:<port>` mismatch denied); `stdout_routed_to_log` (`tracing` test subscriber captures `echo`'s stdout at info and stderr at warn with the service field; a 10 KiB line is truncated to 4 KiB).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(runtime): outbound allowlist and stdio to log`.

### Task 6: Code cache

**Files:** Create `crates/mesh-runtime/src/cache.rs`; tests `tests/cache.rs`.

**Interfaces — Produces:** cache behaviour per spec §3.4 for `CodeCache::{Mmap, Read}`; atomic writes; corrupt entries deleted and recompiled; a `#[doc(hidden)] pub fn cache_stats(&self) -> (hits, misses)` behind `test-hooks`.

- [ ] **Step 1: Failing tests:** `code_cache_hit_after_restart_mmap_and_read_modes` (for each mode: load → miss; drop the runtime; new runtime on the same dir → hit; the loaded component serves `echo`); `corrupt_cache_entry_recompiled` (overwrite the `.cwasm` with junk → load succeeds as a miss and rewrites it); `read_mode_never_mmaps` (asserts the `Read` path calls `Component::deserialize` — structure the code so this is a unit test on the cache module's chosen strategy enum).
- [ ] **Step 2–4:** Run (FAIL) → implement → run (PASS).
- [ ] **Step 5: Commit** `feat(runtime): compiled-code cache (mmap and read modes)`.

### Task 7: `mesh-core` additions — `NodeHandle`, blob pins/remove

**Files:** Modify `crates/mesh-core/src/{node.rs, blobs.rs, lib.rs}`; tests in `crates/mesh-core/tests/{node.rs, blobs.rs}`.

**Interfaces — Produces:**
- `#[derive(Clone)] pub struct NodeHandle` (holds the node's `Arc<Inner>` without the `Drop`-cancels behaviour of `Node`) with `node_id()`, `mesh()`, `store() -> &StoreHandle`, `blobs() -> &Blobs`, `members() -> Vec<MemberStatus>`, `status() -> NodeStatus`, `cancelled() -> impl Future<Output = ()>` (resolves when the node is cancelled/shut down); `Node::handle(&self) -> NodeHandle`. Dropping a `NodeHandle` must not cancel anything.
- `pub enum BlobKind { Raw, Collection }`; `Blobs::pins() -> Result<Vec<(Hash, BlobKind)>, NodeError>`; `Blobs::remove(hash) -> Result<(), NodeError>` (removes the pin tag and the data; for a collection also its children that no other pin references).

- [ ] **Step 1: Failing tests:** `node_handle_drop_does_not_cancel` (drop a handle; node still replicates); `node_handle_cancelled_resolves_on_shutdown`; `pins_lists_raw_and_collection`; `remove_deletes_unpinned_data` (after `remove`, `has` is false and `pins` omits it; a second remove is Ok); `remove_collection_keeps_shared_child` (two collections sharing a file; remove one; the other still `has_collection`).
- [ ] **Step 2–4:** Run (FAIL) → implement → run (PASS) including all existing `mesh-core` suites.
- [ ] **Step 5: Commit** `feat(core): NodeHandle; blob pins and remove`.

### Task 8: `mesh-services` scaffold — manifests, operations, KV adapter

**Files:** Create `crates/mesh-services/{Cargo.toml, src/lib.rs, src/error.rs, src/manifest.rs, src/kv.rs, src/ops.rs}`; tests `crates/mesh-services/tests/{common/mod.rs, ops.rs}`.

**Interfaces — Consumes:** Tasks 1, 2, 7. **Produces:** `Services::start(&Node, ServicesConfig)` (starts nothing but holding state until Task 9 adds the deployer), `deploy_wasm`, `deploy_static`, `undeploy`, `purge`, `set_placement`, `set_outbound`, `set_limits`, `ServicesConfig`, `DeployOpts` (`Default`), `ServicesError` (spec §7); `ServiceKv { store: StoreHandle, ns: String }: KvBackend` with the spec §7 error mapping; helpers `manifest_key(name)`, `gen_key(name)`, `current_generation(&StoreHandle, name)`, `data_ns(name, gen)`. Test harness re-uses `mesh-core`'s pattern (loopback, relay off) — copy the minimal helpers into `tests/common`.

- [ ] **Step 1: Failing tests:** `deploy_wasm_writes_manifest_v1_then_v2`; `garbage_bytes_rejected` (Review Focus 1: empty, PNG header, core module → `InvalidComponent`, no manifest key, `pins()` unchanged); `invalid_name_rejected` (`A`, `_mesh`, `a.b`, 33 chars → `InvalidName`); `manifest_validation_rejects_bad_values` (via `set_limits`/`set_outbound`); `undeploy_deletes_manifest_keeps_data`; `purge_bumps_generation` (gen 0 → 1; `data_ns` → `name.1`; old keys tombstoned on this node); `set_placement_bumps_version`; `service_kv_error_mapping` (a removed node's `ServiceKv::set` → `KvError::Removed`).
- [ ] **Step 2–4:** Run (FAIL) → implement → run (PASS).
- [ ] **Step 5: Commit** `feat(services): manifests, deploy/undeploy/purge, KV adapter`.

### Task 9: Placement, deployer loop, dispatch, status

**Files:** Create `crates/mesh-services/src/{placement.rs, deployer.rs, dispatch.rs, status.rs}`; tests `tests/deployer.rs`.

**Interfaces — Consumes:** Task 8, `NodeHandle`. **Produces:** `placed(&ServiceManifest, &NodeHandle) -> bool`, `placed_hosts(..) -> Vec<NodeId>` (spec §5.1); the deployer task (spec §5.2) tied to `NodeHandle::cancelled()`; `Services::handle(name, req)` (spec §5.3 table, with exact bodies); `Services::services() -> Vec<ServiceStatus>` (spec §5.4); `Services::shutdown`. Fetch: `blobs.fetch` (Wasm) / `blobs.fetch_collection` (Static); WASM load via `Runtime::load`; requests via `Runtime::handle` with `RequestCtx { kv: ServiceKv(data_ns), limits, outbound, node_id }`. (Static payloads are loaded in Task 10; until then a Static active entry answers `501`.)

- [ ] **Step 1: Failing tests (2–3 node meshes):** `deploy_wasm_on_a_served_on_b` (counter: call on A and B share one count); `new_version_swaps_old_serves_until_ready` (deploy v1; deploy v2 whose blob is withheld from B via a hook or partition; B still serves v1; heal → v2); `placement_tags_and_nodes` (tags and node lists; only placed nodes become `Ready`); `not_placed_gives_503_and_no_fetch` (B not placed → exact 503 body; `blobs.has` false on B); `placement_matches_no_host_status_warns` (`placed_hosts` empty); `undeploy_gives_404_data_kept`; `purge_bumps_generation_hides_keys_and_sets` (after purge the counter restarts at 1 on both nodes; a set added before purge is not visible via `/members`); `concurrent_deploys_lww_winner_everywhere` (partition, deploy different modules on A and B, heal → same manifest and same behaviour on both); `never_fetched_gives_503` (placed node with the blob unavailable → `503 Service not yet replicated`); `kv_write_refused_when_removed_gives_503`; `client_role_never_runs`; `services_resume_after_restart` (Review Focus 5: restart B → its services return to `Ready` without redeploy; the code cache records a hit).
- [ ] **Step 2–4:** Run (FAIL) → implement → run 3× (PASS).
- [ ] **Step 5: Commit** `feat(services): placement, deployer loop, dispatch and status`.

### Task 10: Static sites

**Files:** Create `crates/mesh-services/src/static_site.rs`; tests `tests/static_site.rs`.

**Interfaces — Produces:** `StaticSite::load(&Blobs, root) -> Result<StaticSite, ServicesError>` (path → hash map from the collection); `StaticSite::serve(&self, &Blobs, &http::Request<Bytes>) -> http::Response<Bytes>` per spec §6 (methods, normalisation, index, 301, MIME table + charset, ETag/304, `Cache-Control: no-cache`, 404/503/500). The deployer uses it for `ServiceKind::Static`.

- [ ] **Step 1: Failing tests:** `static_site_index_etag_304_404_traversal_405_redirect` (one site with `index.html`, `docs/index.html`, `app.js`, `img/x.png`: `/` and `/docs/` serve the index files; `/docs` → 301 `Location: /docs/`; MIME and charset per extension; ETag present; matching `If-None-Match` → 304; `/nope` → 404; `POST /` → 405; `HEAD /` → 200 empty body with headers); `encoded_traversal_rejected` (Review Focus 2: `/%2e%2e/x`, `/a/..%2Fb`, `/..%5cx`, `/a%00b` → 400); `deploy_static_on_a_served_on_b` (end-to-end via `Services::handle`).
- [ ] **Step 2–4:** Run (FAIL) → implement → run (PASS).
- [ ] **Step 5: Commit** `feat(services): static site handler`.

### Task 11: Blob GC

**Files:** Create `crates/mesh-services/src/gc.rs`; tests `tests/gc.rs`.

**Interfaces — Consumes:** `Blobs::pins/remove` (Task 7). **Produces:** an hourly GC task (spec §5.5) with `<data>/services/gc.json` (atomic write), a `test-hooks` `Services::gc_now()` and clock injection via the node's `WallClock`.

- [ ] **Step 1: Failing test:** `blob_gc_after_24h_unreferenced_keeps_referenced` (deploy v1, then v2: v1's blob unreferenced; `gc_now` at +1 h keeps it; at +25 h removes it; v2's blob, a static root and its children, and a blob of a deployed-but-not-placed service are kept; re-referencing before 24 h clears the entry).
- [ ] **Step 2–4:** Run (FAIL) → implement → run (PASS).
- [ ] **Step 5: Commit** `feat(services): blob GC for unreferenced service blobs`.

### Task 12: `meshdev` service commands, manual check, back-ports, Android build

**Files:** Modify `crates/mesh-core/examples/meshdev.rs` → move to `crates/mesh-services/examples/meshdev.rs` (it now needs `mesh-services`; keep all existing commands) and delete the old one; create `docs/manual-checks/sub-spec-3.md`; modify `docs/superpowers/specs/2026-11-08-meshnode-design.md` (back-port spec §2 items 1–7, mark "(back-ported from sub-spec 3)"; §14 row 3 "implemented, manual check pending"); the sub-spec 3 spec's §7 "add `Clone` to `Node`" wording → `NodeHandle` "(amended during implementation)".

**Interfaces — Produces:** commands per spec §8.4: `deploy wasm <name> <file> [--tags a,b | --nodes id,..] [--outbound host:port,..]`, `deploy static <name> <dir>`, `services`, `undeploy <name>`, `purge <name>`, `call <name> <METHOD> <path> [body]`.

- [ ] **Step 1:** Implement the commands; smoke-test two local processes: deploy the `counter` fixture (from `target/fixtures`) on one, `call` on the other.
- [ ] **Step 2:** `cargo ndk -t arm64-v8a build -p mesh-services` (set `ANDROID_NDK_HOME` to `%LOCALAPPDATA%\Android\Sdk\ndk\27.2.12479018` if unset); record the result in the manual-check doc.
- [ ] **Step 3:** Write `docs/manual-checks/sub-spec-3.md` with spec §8.4's five steps and an empty results table.
- [ ] **Step 4:** Back-ports as listed above.
- [ ] **Step 5:** Full verification (workspace tests, clippy, fmt).
- [ ] **Step 6: Commit** `feat(services): meshdev service commands; docs: back-port sub-spec 3`.
