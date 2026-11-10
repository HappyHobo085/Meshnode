# Sub-spec 3 — Runtime and services

**Date:** 2026-11-09
**Status:** Draft, awaiting owner review
**Parent:** `2026-11-08-meshnode-design.md` (§7 services, §12 errors, §14 row 3)
**Builds on:** `2026-11-09-node-core-design.md` (sub-spec 2: `Node`, the gated `StoreHandle`, `Blobs`, membership)
**Delivers:** crates `mesh-runtime` (wasmtime host) and `mesh-services` (manifests, deploy, placement, deployer, static sites, blob GC); a `ServiceManifest` type in `mesh-proto`; small additions to `mesh-core`; `meshdev` service commands.

---

## 1. Goal and success criteria

Run services on the mesh: deploy a WASM component or a static site from any host, have every placed host fetch and serve it from its local replica, keep serving across new versions, and contain a misbehaving service so the node never fails because of it.

Done when:
1. Every test in §8 passes.
2. `cargo test --workspace --all-features`, `clippy -D warnings`, `fmt --check` pass on Windows and Linux; `cargo ndk -t arm64-v8a build -p mesh-services` compiles.
3. The manual check in §8.4 has been run and recorded in `docs/manual-checks/sub-spec-3.md`.
4. The back-ports in §2 are made to the master spec.

Dependencies: `wasmtime`, `wasmtime-wasi`, `wasmtime-wasi-http` **50.0.x** (the versions the Android spike validated), `http`, `bytes`, `async-trait`, `tokio`, `tracing`, `thiserror`. Fixtures use `wstd` 0.6 for `wasm32-wasip2`.

## 2. Changes to the parent spec (made here, to be back-ported)

1. **Crate split.** `mesh-runtime` is a pure wasmtime host with no `mesh-core` dependency; `mesh-services` holds manifests, deploy, placement, the deployer, static sites and blob GC, and depends on `mesh-core`'s public API and `mesh-runtime`. Master §3's table changes: the deployer and placement move out of `mesh-core`, and `mesh-runtime` no longer depends on `mesh-store`. Service KV writes go through `mesh-core`'s gated `StoreHandle`, so a removed or re-bootstrapping node cannot write. The gateway (sub-spec 4) depends on `mesh-services`.
2. **`wasi:logging` → stdout/stderr.** wasmtime-wasi 50 does not implement `wasi:logging`, and most toolchains print to stdio. The guest's stdout and stderr go to the node log line by line (stdout at `info`, stderr at `warn`, tagged with the service name, each line capped at 4 KiB). The world imports `wasi:cli` stdio instead of `wasi:logging`.
3. **Only hosts run services.** Placement applies to effective host-role members; client-role nodes never fetch or run services (they forward requests, sub-spec 4).
4. **Data namespace and generations.** A service's data lives in namespace `<name>` for generation 0 and `<name>.<gen>` after a purge (§4). `[a-z0-9-]` names cannot contain `.` or collide with `_mesh`.
5. **Purge by generation.** `purge` adds a new generation instead of relying on key-by-key tombstones, so grow-only sets and keys written concurrently on other nodes are hidden too (§4).
6. **Pulley is left out.** Cranelift works in the Android app sandbox (spike); there is no interpreter fallback.
7. **Bodies are buffered**, not streamed, for both WASM and static responses (bounded by the limits).

## 3. `mesh-runtime`

### 3.1 API

```rust
pub struct Runtime;   // one Engine + Linker per node; owns the epoch ticker
impl Runtime {
    pub fn new(cfg: RuntimeConfig) -> Result<Runtime, RuntimeError>;
    /// Compiles or loads from the code cache; validates the component against the world.
    pub async fn load(&self, hash: [u8; 32], wasm: &[u8]) -> Result<Loaded, RuntimeError>;
    /// Never fails: every error becomes an HTTP status (§3.3).
    pub async fn handle(&self, svc: &Loaded, req: http::Request<Bytes>, ctx: RequestCtx) -> http::Response<Bytes>;
}
pub struct Loaded;    // cheap to clone; holds the component's ProxyPre

pub struct RuntimeConfig { pub cache: CodeCache, pub max_in_flight: usize /* default 64 */ }
pub enum CodeCache { None, Mmap(PathBuf) /* desktop */, Read(PathBuf) /* Android: read bytes, deserialize, never mmap */ }

pub struct RequestCtx {
    pub service: String,
    pub node_id: NodeId,
    pub kv: Arc<dyn KvBackend>,
    pub limits: Limits,
    pub outbound: Vec<String>,   // "host:port" allowlist
}
pub struct Limits { pub memory_mb: u32 /* 64 */, pub request_timeout_ms: u64 /* 10_000 */, pub max_body_bytes: usize /* 8 MiB */ }

#[async_trait]
pub trait KvBackend: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError>;
    async fn set(&self, key: &str, value: Vec<u8>) -> Result<(), KvError>;
    async fn set_ordered(&self, key: &str, order: Vec<u8>, value: Option<Vec<u8>>) -> Result<(), KvError>;
    async fn delete(&self, key: &str) -> Result<(), KvError>;
    async fn list_keys(&self, prefix: &str, start_after: Option<&str>, limit: u32) -> Result<Vec<String>, KvError>;
    async fn set_add(&self, set: &str, member: Vec<u8>) -> Result<(), KvError>;
    async fn set_contains(&self, set: &str, member: &[u8]) -> Result<bool, KvError>;
    async fn set_members(&self, set: &str, start_after: Option<&[u8]>, limit: u32) -> Result<Vec<Vec<u8>>, KvError>;
}
pub enum KvError { Full, Removed, Rebootstrapping, Other(String) }
pub enum RuntimeError { Invalid(String), Compile(String), Cache(std::io::Error) }
```

`Limits` and the allowlist type are shared with `mesh-proto`'s `ServiceManifest` (§4.1): `mesh-runtime` depends on `mesh-proto` for `NodeId` and `Limits`.

### 3.2 The world

The component targets world `meshnode:service/service`: it exports `wasi:http/incoming-handler` and may import `wasi:clocks`, `wasi:random`, `wasi:cli` stdio, `wasi:http/outgoing-handler` and `meshnode:kv/store`. The `meshnode:kv` WIT is master §7's, unchanged (`get`, `set`, `set-ordered`, `delete`, `list-keys`, `set-add`, `set-contains`, `set-members`, `node-id`). No filesystem, no preopened directories, no sockets. The linker also provides `wasi:cli/environment` (always empty), `wasi:cli/exit` and `wasi:cli/terminal-*` (always "not a terminal"), because every Rust `wasm32-wasip2` guest imports them *(amended during implementation)*.

`load` rejects (with `RuntimeError::Invalid`) a component that does not export the incoming handler or imports anything outside the world; such a component is never served. `load` accepts only binary components (`Component::from_binary`, no WAT text) *(amended during implementation)*.

### 3.3 Handling a request

0. A request without a `Host` header gets `Host: <service>` before it reaches the guest *(amended during implementation)*.
1. If the request body is larger than `max_body_bytes`: `413`, without instantiating.
2. Acquire a slot from the in-flight semaphore (`max_in_flight` per node); waiting counts against the timeout.
3. Create a fresh `Store` with a custom `ResourceLimiter`: `memory_mb` caps the **total** of all linear memories in the store (up to 16 memories; also 32 instances, 16 tables, 100 000 table elements), and the epoch deadline is one 10 ms tick with an epoch callback: on every tick the guest yields to the executor (`UpdateDeadline::Yield`), so a spinning guest never holds a worker thread, and once the request deadline (the slot wait included) has passed by wall clock the callback interrupts it (`Trap::Interrupt`, `504`), so the CPU deadline and the wall timeout end together *(amended during implementation; yield-per-tick amended in final review)*; instantiate from the cached `ProxyPre`; call the incoming handler.
4. The whole call (including time awaiting host calls: KV, outbound HTTP) runs inside `tokio::time::timeout(request_timeout_ms)`.
5. Collect the response body; if it exceeds `max_body_bytes`: `500`.

Status mapping, each logged with the service name:

| Situation | Status |
|---|---|
| Request body too large | `413` |
| Guest trap (including out-of-memory) | `500` |
| Epoch deadline or `timeout` elapsed | `504` |
| Response body too large | `500` |
| `KvError::Full` (trap raised by the host) | `507` |
| `KvError::Removed` / `KvError::Rebootstrapping` | `503` |
| `KvError::Other` | `500` |

The KV WIT has no error results, so a backend error traps the guest with the mapped status recorded in the store's host state.

**Outbound HTTP.** The `send_request` hook compares the URI's `host:port` (port defaulting to 443 for `https`, 80 for `http`) with the allowlist by exact, case-insensitive match. Denied → `ErrorCode::HttpRequestDenied` to the guest and a `warn` log with the destination. Allowed → wasmtime-wasi-http's default client. An authority containing userinfo (`@`) is always denied; a trailing-dot host is a different host from the same name without the dot; IPv6 entries are written bracketed (`[::1]:443`) *(amended during implementation)*.

**Stdio.** Guest stdout and stderr are line-buffered into `tracing` (§2.2). At most 1000 lines per stream per request are logged; then one `warn` "output truncated" and the rest is dropped *(amended during implementation)*.

**`node-id`** returns `ctx.node_id`'s 32 bytes.

### 3.4 Code cache

The cache key is `hex(module hash) ‖ hex(Engine::precompile_compatibility_hash())`. On a miss: compile, then write the serialized component atomically (temp file, fsync, rename) to `<dir>/<key>.cwasm`. On a hit: `Mmap` uses `Component::deserialize_file`; `Read` reads the bytes into memory and uses `Component::deserialize` (never memory-mapped, as Android requires). A cache entry that fails to deserialize is deleted and recompiled. `deserialize` is `unsafe` in wasmtime; it is sound here because the cache directory is node-private and only this code writes it (documented at the call site).

## 4. Manifests and operations (`mesh-services`)

### 4.1 Data in `_mesh`

Raw postcard, like other `_mesh` values.

| Item | Kind | Content |
|---|---|---|
| `service/<name>` | LWW register | `ServiceManifest` (below). A tombstone means undeployed. |
| `service-gen/<name>` | grow-only set of `u64` (8 bytes big-endian) | data generations; the current one is the largest member, 0 if empty |

```rust
// mesh-proto/src/service.rs
pub struct ServiceManifest {
    pub name: String,                 // [a-z0-9-]{1,32}
    pub kind: ServiceKind,            // Wasm { module: [u8; 32] } | Static { root: [u8; 32] }  (iroh-blobs hashes)
    pub version: u64,                 // increases on every manifest write
    pub limits: Limits,
    pub placement: Placement,         // AllHosts | Tags(Vec<String>) | Nodes(Vec<NodeId>)
    pub outbound: Vec<String>,        // "host:port"
}
```

Validation on decode and before every write: name pattern; `memory_mb` 1..=1024; `request_timeout_ms` 100..=300_000; `max_body_bytes` ≤ 64 MiB; ≤ 64 `outbound` entries, each `host:port` with a non-empty host and a `u16` port; ≤ 64 tags (each ≤ 32 bytes) or nodes. Defaults: `Limits { 64, 10_000, 8 MiB }`, `AllHosts`, no outbound.

### 4.2 Operations

All writes go through the gated `StoreHandle`.

- **`deploy_wasm(name, bytes, opts)`**: `runtime.load(hash, bytes)` (an invalid component fails here with `InvalidComponent`, so a rejected deploy pins nothing) → `blobs.add_bytes(bytes)` → write the manifest *(amended during implementation)*. A deploy holds the GC lock's shared side from adding the blob until the manifest is written (§5.5) with `version = previous + 1` (1 if none).
- **`deploy_static(name, dir, opts)`**: `blobs.add_path(dir)` (a collection) → write the manifest.
- **`set_placement`, `set_outbound`, `set_limits`**: rewrite the manifest with `version + 1`. `NotFound` if undeployed.
- **`undeploy(name)`**: delete the manifest. The data stays.
- **`purge(name)`**: add `current generation + 1` to `service-gen/<name>`, then `purge_namespace(old namespace)` on this node. Allowed whether or not the service is deployed.

Concurrent writes to one manifest resolve by LWW (the later HLC wins); every node converges to the same manifest.

**Documented limits of purge:** old set members, and keys only other nodes held, remain on disk but unreachable; a write against the old generation made during the purge race is lost to the service.

## 5. Placement, deployer, blob GC

### 5.1 Placement

`placed(manifest, me)` is false unless this node is an effective **host**. Then: `AllHosts` → true; `Tags(t)` → this node's own `info/<me>` tags intersect `t`; `Nodes(ids)` → `ids` contains this node. `placed_hosts(manifest)` applies the same rule to every effective host from `members()`.

### 5.2 Deployer loop

One task, supervised and stopped by the node's cancel token.

- **Triggers**: store `Change` events in `_mesh` for `service/*`, `service-gen/*`, `info/<me>`, `members`, `removed`; the node's `MembersChanged` event (via `NodeHandle::subscribe`) for membership-driven placement changes *(amended during implementation)*; a full rescan every 60 s and on `Lagged`.
- **Target** per service: the manifest if placed here, else none.
- **Active table**: `RwLock<HashMap<String, Arc<Active>>>`, `Active { manifest, generation, payload: Wasm(Loaded) | Static(StaticSite) }`. Requests clone the `Arc`, so in-flight requests finish on the version they started with.
- **Transitions**:
  - Module/root changed: fetch (`blobs.fetch` for WASM, `blobs.fetch_collection` for static; skipped when present locally) → `runtime.load` (WASM) or load the collection index (static) → swap. The previous version serves until the swap.
  - Only limits, outbound, placement or generation changed: swap immediately.
  - A newer manifest arrives during a fetch: the stale fetch is abandoned.
  - Failure: keep the previous version; retry with backoff 5 s → 5 min; status `Failed { error, retry_at }`.
  - Undeployed or no longer placed here: remove from the active table.

### 5.3 Dispatch: `Services::handle(name, req)`

| State | Response |
|---|---|
| Active | served by the runtime (§3.3) or the static handler (§6) |
| Placed here, never ready | `503 Service not yet replicated` |
| Not placed here | `503 Service not placed on this node` (sub-spec 4's gateway forwards instead) |
| No manifest | `404 Service not deployed` |

### 5.4 Status

`services() -> Vec<ServiceStatus>`, `ServiceStatus { manifest, generation, local: NotPlaced | Fetching | Ready { version } | Failed { error, retry_at }, placed_hosts: Vec<NodeId>, data_bytes: u64 }`. `data_bytes` comes from `namespace_sizes()` for the current namespace. An empty `placed_hosts` is a warning the UI shows.

### 5.5 Blob GC (hourly)

- GC considers every pinned blob on a node running `Services`. It holds the GC lock's exclusive side while deciding and removing, recomputes the referenced set right before removing, and skips while the node is not `Running` *(amended during implementation)*.
- **Referenced** = the module or root of every live manifest, a collection root's children (known from `kind`), and everything in the active table. Placement does not matter: a node that deployed a service it does not run keeps the blob so others can fetch it.
- A pinned hash that is not referenced gets its first-unreferenced time recorded in `<data>/services/gc.json` (atomic write); after 24 h unreferenced it is removed (unpinned and deleted). Becoming referenced again clears the entry.
- `mesh-core` additions: `Blobs::pins() -> Vec<(Hash, BlobKind)>` and `Blobs::remove(hash)` (unpin and delete the data; the implementation picks the iroh-blobs mechanism).

## 6. Static sites

- `GET` and `HEAD` only; others `405`.
- The path is percent-decoded and normalised; `..` segments, backslashes and NUL → `400`.
- `/` or a path ending in `/` → `<dir>/index.html`; a path naming a directory without a trailing `/` → `301` to the slashed path.
- `Content-Type` from a built-in extension table (`html`, `htm`, `css`, `js`, `mjs`, `json`, `svg`, `png`, `jpg`, `jpeg`, `gif`, `webp`, `ico`, `txt`, `wasm`, `woff`, `woff2`, `pdf`, `xml`, `map`), else `application/octet-stream`; `text/*`, `json`, `js` and `svg` get `; charset=utf-8`.
- `ETag: "<blob hash hex>"`; a matching `If-None-Match` → `304`. `Cache-Control: no-cache`.
- Missing path → `404`; a file the collection lists but the node lacks → `503`; a file over 64 MiB → `500`.
- `Active::Static` holds the collection's path → hash map, loaded at swap; files are read with `blobs.read`.

## 7. Wiring and errors

```rust
pub struct Services;
impl Services {
    pub async fn start(node: &Node, cfg: ServicesConfig) -> Result<Services, ServicesError>;
    pub async fn deploy_wasm(&self, name: &str, wasm: Bytes, opts: DeployOpts) -> Result<ServiceManifest, ServicesError>;
    pub async fn deploy_static(&self, name: &str, dir: &Path, opts: DeployOpts) -> Result<ServiceManifest, ServicesError>;
    pub async fn undeploy(&self, name: &str) -> Result<(), ServicesError>;
    pub async fn purge(&self, name: &str) -> Result<(), ServicesError>;
    pub async fn set_placement(&self, name: &str, p: Placement) -> Result<ServiceManifest, ServicesError>;
    pub async fn set_outbound(&self, name: &str, o: Vec<String>) -> Result<ServiceManifest, ServicesError>;
    pub async fn set_limits(&self, name: &str, l: Limits) -> Result<ServiceManifest, ServicesError>;
    pub fn services(&self) -> Vec<ServiceStatus>;
    pub async fn handle(&self, name: &str, req: http::Request<Bytes>) -> http::Response<Bytes>;
    pub async fn shutdown(self);
}
pub struct ServicesConfig { pub cache: CodeCache, pub max_in_flight: usize }
pub struct DeployOpts { pub limits: Limits, pub placement: Placement, pub outbound: Vec<String> }  // Default = spec defaults
pub enum ServicesError { InvalidName, InvalidManifest(String), InvalidComponent(String), NotFound, Node(NodeError), Runtime(RuntimeError), Io(std::io::Error) }
```

- `Services` holds a non-owning `NodeHandle` (not `Node: Clone`) *(amended during implementation)*: `NodeHandle { node_id, mesh, store, blobs, members, status, subscribe, cancelled, data_dir, now_ms }`, obtained from `Node::handle()`.
- **KV adapter**: `ServiceKv { store: StoreHandle, ns: String }` implements `KvBackend` over the gated writes and the read view; `NodeError::Store(Full)` → `KvError::Full`, `NodeError::Removed` → `Removed`, `NodeError::Rebootstrapping` → `Rebootstrapping`, others → `Other`. The namespace is fixed at construction, so a service cannot name another namespace.
- `mesh-core` additions (small) *(amended during implementation)*: `Node::handle()` and `NodeHandle` (above, including `cancelled` for tying tasks to the node's cancel token), `Blobs::{pins, remove, collection_entries, size}`, `BlobKind`, `Timings::blob_gc_tick`.

## 8. Testing

### 8.1 Fixtures

`crates/mesh-runtime/tests/fixtures/` is a separate cargo workspace (excluded from the main one), Rust + `wstd`, built for `wasm32-wasip2` once per test run by a helper into `target/fixtures/`, guarded by a file lock so concurrent test binaries do not race. CI adds `rustup target add wasm32-wasip2`.

| Fixture | Behaviour |
|---|---|
| `counter` | `GET /` increments key `n` and returns it; `/set?k=&v=`, `/get?k=`, `/del?k=`, `/list?prefix=`, `/ordered?k=&o=&v=`, `/setadd?s=&m=`, `/contains?s=&m=`, `/members?s=`, `/nodeid` exercise every KV call |
| `spin` | loops forever |
| `hog` | allocates until it fails |
| `outbound` | `GET /fetch?url=` makes an outgoing request; returns its status or the error text |
| `echo` | returns the request body (and a sized body via `?size=`); prints to stdout and stderr |
| `bad` | imports an interface outside the world |

### 8.2 `mesh-runtime` tests

In-memory `KvBackend`; a local `127.0.0.1` HTTP server for outbound.

`kv_semantics_all_calls`, `isolation_two_services_two_namespaces`, `timeout_gives_504_and_node_survives`, `memory_cap_gives_500`, `body_cap_gives_413_before_instantiation`, `oversized_response_gives_500`, `outbound_allowed_vs_denied`, `invalid_component_rejected_at_load`, `kv_errors_map_to_507_503_500`, `stdout_routed_to_log`, `code_cache_hit_after_restart_mmap_and_read_modes`, `corrupt_cache_entry_recompiled`, `in_flight_cap_queues_then_times_out`.

### 8.3 `mesh-services` tests

Multi-node, using `mesh-core`'s test harness (`test-hooks`).

`deploy_wasm_on_a_served_on_b`, `invalid_component_never_writes_manifest`, `new_version_swaps_old_serves_until_ready`, `placement_tags_and_nodes`, `not_placed_gives_503_and_no_fetch`, `placement_matches_no_host_status_warns`, `undeploy_gives_404_data_kept`, `purge_bumps_generation_hides_keys_and_sets`, `concurrent_deploys_lww_winner_everywhere`, `never_fetched_gives_503`, `static_site_index_etag_304_404_traversal_405_redirect`, `blob_gc_after_24h_unreferenced_keeps_referenced`, `kv_write_refused_when_removed_gives_503`, `client_role_never_runs`, `manifest_validation_rejects_bad_values`.

### 8.4 Demo and manual check

`meshdev` gains: `deploy wasm <name> <file> [--tags a,b | --nodes id,..] [--outbound host:port,..]`, `deploy static <name> <dir>`, `services`, `undeploy <name>`, `purge <name>`, `call <name> <METHOD> <path> [body]` (calls `Services::handle`; prints status, headers, body).

Manual check (`docs/manual-checks/sub-spec-3.md`), two desktops:
1. Deploy `counter` on A; `call counter GET /` on B.
2. Call it on both; both see one shared, increasing count.
3. Deploy a static site on A; `call site GET /` on B returns `index.html`.
4. Set placement to A only; B answers `503 not placed`.
5. Undeploy (`404`), then purge; redeploy and the counter starts from 1.

## 9. Out of scope here

The gateway, forwarding to other hosts and port mappings (sub-spec 4); the admin API and CLI (sub-spec 4); streaming bodies; the Pulley interpreter; CPU accounting beyond the timeout; the Aegis sync service (sub-spec 7).
