# mesh-client SDK and Partial Replication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Client-role members replicate only `_mesh`, and an embeddable `mesh-client` SDK (Rust + Kotlin via UniFFI) joins a mesh as a client and sends HTTP to services over `httpfwd`.

**Architecture:** Sync sessions learn the peer's scope from a `scope` field in `Hello`; hosts serve client peers a scoped diff (out-of-scope runs become `Covered`), and every node ignores `Covered` from partial peers. Remote forwarding and placement move out of `mesh-gateway`/`mesh-services` into a new wasmtime-free crate `mesh-forward`, which both the gateway and `mesh-client` use. `mesh-client-ffi` wraps the SDK for Kotlin.

**Tech Stack:** Rust 2024, iroh, redb, postcard, UniFFI 0.29 (tokio), Kotlin/JNA, cargo-ndk.

**Spec:** `docs/superpowers/specs/2026-11-12-mesh-client-design.md` (master: `docs/superpowers/specs/2026-11-08-meshnode-design.md` §5.8, §6, §8).

## Global Constraints

- Branch `feat/mesh-client` in `<meshnode-checkout>`; git needs `-c safe.directory=<meshnode-checkout>`. Never push.
- Build env: `CARGO_TARGET_DIR=$LOCALAPPDATA/meshnode-target`, `CARGO_INCREMENTAL=0`, `-j 2`. C: fills up — deleting `$LOCALAPPDATA/meshnode-target/debug` is allowed; build Android/FFI in a separate target dir only if needed.
- Safety: never install/start/stop OS services, never elevate, never touch real data dirs, never run `#[ignore]` tests, never `adb`/touch a device. Never run `tauri android init --force`.
- Licence `AGPL-3.0-only` for new crates (`license.workspace = true`).
- Scope v1 is exactly `Partial(["_mesh"])` for client-role nodes and `Full` for host-role nodes.
- **No wire compatibility (owner: nothing from this project is in use, no data to keep):** `SyncMsg::Hello` gains a field: `Hello { mesh: MeshId, vv: VersionVector, scope: Scope }`. There is no `Hello2` and no legacy-`Hello` handling; old and new nodes need not interoperate.
- `mesh-client` must not depend (transitively) on `wasmtime`, `mesh-runtime`, `mesh-services`, `hyper`, or `mesh-gateway`. CI checks `cargo tree -p mesh-client -e normal` for `wasmtime`, `mesh-runtime`, `mesh-services`, `hyper`.
- Request limits (from the gateway): body ≤ 16 MiB; up to 3 host attempts; default request timeout 30 s; hop-by-hop headers dropped.
- UniFFI Kotlin package `net.meshnode.client`, cdylib name `mesh_client_ffi`; Android ABIs arm64-v8a and x86_64.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01VKLL6Rs4i1E9FwhqtXKgyB`.

## Review Focus

1. **A new host whose first and only sync partner for a while is a client.** It must never mark another origin's data ops as seen. Test: Task 3 `host_syncing_only_from_client_gets_data_later`.
2. **A client that is offline while a service is deployed and later removed.** On reconnect its `services()` must match the hosts' view, and requests for the removed service get `NoHost` or the host's 404, never a hang. Test: Task 6 `services_follow_deploy_and_remove_after_offline`.
3. **Two processes opening the same client data dir.** The second gets `AlreadyOpen`, and the first keeps working. Test: Task 6 `second_open_same_dir_refused`.
4. **A client whose ticket is for a host role.** The SDK must refuse it with `InvalidTicket` rather than join as a Partial store with a host role. Test: Task 6 `host_ticket_refused`.
5. **A host that restarts during a client request.** The request is retried on the next host, or fails with a clear error within `request_timeout`, never more. Test: Task 6 `request_fails_over_when_host_restarts`.

---

### Task 1: Proto — `Scope` in `SyncMsg::Hello`

**Files:** Modify `crates/mesh-proto/src/sync.rs`, `crates/mesh-proto/src/lib.rs` (re-export); Test: in-file `#[cfg(test)]`.

**Interfaces:**
- Produces: `pub enum Scope { Full, Partial(Vec<String>) }` (serde, Clone, Eq, Debug), `impl Scope { pub fn client() -> Scope /* Partial(vec!["_mesh"]) */; pub fn admits(&self, ns: &str) -> bool }`; `SyncMsg::Hello { mesh, vv, scope }` (field added; variant order unchanged).

- [ ] **Step 1: Failing tests:** `hello_with_scope_roundtrips` (both `Full` and `Scope::client()`); `scope_admits` (`Full` admits anything, `client()` admits only `"_mesh"`).
- [ ] **Step 2: Run** `cargo test -p mesh-proto -j 2` — FAIL.
- [ ] **Step 3: Implement.** Fix every exhaustive `match` on `SyncMsg` in the workspace so it compiles (sessions send their own scope as `Full` for now and ignore the peer's — Task 3 adds the scope logic).
- [ ] **Step 4: Run** `cargo test --workspace --all-features -j 2` — PASS; fmt, clippy.
- [ ] **Step 5: Commit** `feat(proto): sync scope in Hello`.

### Task 2: Store — scoped diffs and out-of-scope writes

**Files:** Modify `crates/mesh-store/src/{sync.rs, error.rs, lib.rs}` and the local write paths (set/set_ordered/delete/set_add/purge); Test: `crates/mesh-store/tests/scope.rs`.

**Interfaces:**
- Consumes: `Scope` (Task 1).
- Produces: `Store::diff_since_scoped(&self, theirs: &VersionVector, limit: usize, scope: &Scope) -> Result<Vec<SyncItem>, StoreError>` (`Full` ⇒ identical to `diff_since`); `Store::set_scope(&self, scope: Scope)` and `Store::scope(&self) -> Scope` (in memory, default `Full`; not persisted — the node sets it from its role at open); `StoreError::OutOfScope { ns: String }`.
- Rule: within one origin, a maximal run of seqs that are either missing rows (compacted) or ops whose `ns` the scope does not admit becomes ONE `Covered { origin, upto }`; admitted ops are emitted as `SyncItem::Op`. Items count toward `limit` exactly as in `diff_since`.

- [ ] **Step 1: Failing tests:** `scoped_full_equals_diff_since`; `out_of_scope_run_becomes_one_covered` (ops: `_mesh`, `svc`, `svc`, `_mesh` → `Op, Covered{upto=3}, Op`); `out_of_scope_run_merges_with_compacted_gap`; `trailing_out_of_scope_covered_upto_vv`; `scoped_respects_limit`; `partial_store_refuses_out_of_scope_write` (each local write kind → `OutOfScope`); `partial_store_accepts_mesh_write`; `remote_ops_unaffected_by_scope` (applying a remote op in another ns on a Partial store still works — the session, not the store, filters; documented).
- [ ] **Step 2: Run** `cargo test -p mesh-store -j 2` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p mesh-store -j 2` — PASS; fmt, clippy.
- [ ] **Step 5: Commit** `feat(store): scoped catch-up diffs; refuse out-of-scope local writes`.

### Task 3: Core — sync sessions honour scope

**Files:** Modify `crates/mesh-core/src/{sync.rs, node.rs, membership.rs}` (and wherever the store is opened); Test: `crates/mesh-core/tests/partial.rs`.

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces: `NodeHandle::scope(&self) -> Scope` / `Node::scope`; a node sets its store scope from its own effective role (`Role::Client` ⇒ `Scope::client()`, else `Full`) at open/join and whenever its role changes (Task 4 handles upgrade).
- Session rules:
  - send `Hello { mesh, vv, scope: own }`; record the peer's scope from its `Hello`;
  - refuse (end the session with a protocol error and the existing backoff) a peer whose scope does not match its effective role in `_mesh` (`Host` ⇔ `Full`, `Client` ⇔ `Partial(["_mesh"])`); a peer with no known role yet (provisional during join) is treated by role as today;
  - serve a `Partial` peer with `diff_since_scoped(.., peer_scope)`;
  - ignore (do not apply, do not raise `known`) every `SyncMsg::Covered` received from a `Partial` peer; log at debug;
  - ops received from a `Partial` peer are applied normally.

- [ ] **Step 1: Failing tests** (in-process nodes, follow `tests/join.rs` patterns):
  - `client_store_holds_only_mesh` — two hosts + a client (client invite); a host writes `svc/k`; after sync the client's `get("svc","k")` is `None`, its `list_keys` over `svc` empty, and it sees both hosts in `members()`.
  - `client_receives_mesh_writes` — a host's `_mesh` write appears on the client.
  - `host_syncing_only_from_client_gets_data_later` — hosts A,B + client C; A writes `svc` data; partition so new host D syncs only with C; D must not have `svc` data AND `D.vv[A]` must not cover A's `svc` ops; heal D↔A; D gets the data.
  - `covered_from_partial_peer_ignored` — unit-level: feed a session a `Covered` from a partial peer; `vv` unchanged.
  - `scope_role_mismatch_refused` — a client node forced (test hook) to announce `Full` is refused by a host.
  - `client_write_outside_mesh_refused` — `client.store().set("svc", ..)` → `NodeError::Store(StoreError::OutOfScope{..})`.
- [ ] **Step 2: Run** `cargo test -p mesh-core --test partial -j 2` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p mesh-core -j 2` (all existing tests stay green) — PASS; fmt, clippy.
- [ ] **Step 5: Commit** `feat(core): partial replication for client-role members`.

### Task 4: Core — role upgrade re-bootstraps as Full; `meshd` client nodes

**Files:** Modify `crates/mesh-core/src/{rebootstrap.rs, membership.rs, node.rs}`, `crates/mesh-host` only if its client-role path assumes full data (gateway local dispatch must already forward for client role — verify); Test: `crates/mesh-core/tests/partial.rs`, `crates/mesh-host/tests/*` (one client-role test).

**Interfaces:**
- Produces: when a Partial node's own effective role becomes `Host`, it emits `NodeEvent::RebootstrapStarting` and runs the existing re-bootstrap with scope `Full` (reset + full catch-up + re-apply own export); its scope flips to `Full` only after the re-bootstrap completes. A Partial node's own re-bootstrap (any other trigger) asks peers for a scoped catch-up.

- [ ] **Step 1: Failing tests:** `client_upgraded_to_host_rebootstraps_full` (client C re-invited as host by A ⇒ C ends with A's `svc` data and `scope() == Full`); `partial_rebootstrap_stays_partial`; `meshd_client_role_does_not_store_service_data` (a `mesh-host` daemon joined with a client ticket serves a deployed site through its gateway by forwarding, and its store has no `svc` namespace).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test -p mesh-core -p mesh-host -j 2` — PASS; fmt, clippy.

**Owner command "declare fresh" (owner decision, 2026-11-15):** a host that stayed stale (off > 80 days with no other host) skips tombstone GC for ever; the owner can clear that explicitly.
- Produces: `Node::declare_fresh(&self) -> Result<(), NodeError>` — records a full sync NOW for the staleness clock only (never advances `synced_self_seq`); `NodeError::Rebootstrapping` while re-bootstrapping; an admin request `DeclareFresh`; CLI `meshd declare-fresh --yes` (refuses without `--yes`, printing the risk); a UI action on the node's own page behind a confirm dialog stating the risk: "deletions that other hosts already cleaned up may come back".
- Tests: `declare_fresh_clears_staleness_and_gc_runs` (stale host → declare → `is_stale() == false`, tombstone GC runs on the next maintenance tick); `declare_fresh_refused_while_rebootstrapping`; admin/CLI round-trip test; UI unit test for the confirm dialog.

- [ ] **Step 5: Commit** `feat(core): re-bootstrap as full on role upgrade`.

### Task 5: `mesh-forward` — wasmtime-free placement and remote forwarding

**Files:** Create `crates/mesh-forward/{Cargo.toml, src/lib.rs, src/placement.rs, src/remote.rs}`; Modify `crates/mesh-services/src/{placement.rs, manifest.rs, lib.rs}` (re-export from mesh-forward, keep public paths), `crates/mesh-gateway/src/{forward.rs, lib.rs}`, root `Cargo.toml` members.

**Interfaces:**
- Produces (`mesh_forward`):
  - `placement::{placed_hosts(m: &ServiceManifest, node: &NodeHandle) -> Vec<NodeId>, hosts_among, placed_among}` (moved verbatim) and `manifest_key(name) -> String`, `read_manifest(node: &NodeHandle, name: &str) -> Option<ServiceManifest>`, `list_manifests(node: &NodeHandle) -> Vec<ServiceManifest>`;
  - `pub struct RemoteForwarder` (Clone) with `new(node: NodeHandle) -> Self`, `async fn forward(&self, service: &str, req: Request<Bytes>, timeout: Duration) -> Result<Response<Bytes>, ForwardError>` — the connection cache, candidate ranking by RTT, `MAX_ATTEMPTS = 3`, `RETRYABLE` 503 bodies, frame conversion, all moved from `mesh-gateway::forward`; `ForwardError::{NoHost, Timeout(u64), ...}` moved too; test hooks `attempts()`, `connects()`, `try_first()` behind `test-hooks`.
- `mesh-gateway::Forwarder` keeps its public API and behaviour: local dispatch through `Services`, else `RemoteForwarder::forward`.
- `mesh-forward` dependencies: mesh-core, mesh-proto, bytes, http, iroh, tokio, tracing, thiserror — nothing else.

- [ ] **Step 1:** Run the existing gateway and services tests as the baseline (`cargo test -p mesh-gateway -p mesh-services -j 2`); record counts.
- [ ] **Step 2: Failing test:** `crates/mesh-forward/tests/no_runtime_deps.rs` — runs `cargo tree -p mesh-forward -e normal --prefix none` (via `std::process::Command`, `CARGO` env var) and asserts no line starts with `wasmtime`, `mesh-runtime`, `mesh-services`, `hyper`. FAIL (crate missing).
- [ ] **Step 3: Implement** by moving code; no behaviour change.
- [ ] **Step 4: Run** the baseline tests again (same counts, all green) plus the new test; fmt, clippy.
- [ ] **Step 5: Commit** `refactor: move placement and remote forwarding into mesh-forward`.

### Task 6: `mesh-client` — the Rust SDK

**Files:** Create `crates/mesh-client/{Cargo.toml, README.md, src/lib.rs, src/error.rs, src/client.rs, src/events.rs, examples/fetch.rs}`; Test: `crates/mesh-client/tests/{client.rs, no_runtime_deps.rs}`.

**Interfaces:**
- Consumes: mesh-core `Node`/`NodeConfig` (client role), Task 5 `RemoteForwarder`, `list_manifests`, `placed_hosts`.
- Produces: exactly the API of spec §4 — `ClientOptions { name, platform, request_timeout }` (`Default` with 30 s), `MeshClient::{join, open, request, status, members, services, subscribe, invite, remove, shutdown}`, `ClientStatus { mesh: String, id: String, online_hosts: usize, first_sync_done: bool, removed: bool }`, `Member { id, name, role, platform, online, rtt_ms: Option<u64> }`, `ServiceInfo { name, kind, hosts: Vec<String> }`, `ClientEvent::{MembersChanged, ServicesChanged, Removed, Online, Offline}`, `ClientError::{InvalidTicket, AlreadyJoined, NotJoined, AlreadyOpen, Removed, NoHost, Timeout, Io(String), Internal(String)}`.
- Rules: `join` refuses a ticket whose role is not `Client` (`InvalidTicket`) and a dir that already holds `node.key` (`AlreadyJoined`); `open` needs an existing identity (`NotJoined`); a lock file `client.lock` in the dir (exclusive OS file lock) ⇒ `AlreadyOpen`; `request` requires a path starting with `/` (else a locally built `400` response) and a body ≤ 16 MiB (else a locally built `413` response) — no new error variants; `NoHost` only when no candidate host exists or every attempt failed to connect; after removal every call returns `Removed`.
- The client node never starts a gateway, never registers the `httpfwd` server handler, never runs services, never fetches blobs.

- [ ] **Step 1: Failing tests** (in-process hosts built with `mesh-host` test helpers or mesh-core + mesh-services + a fixture service, as `mesh-gateway` tests do; the client side uses only the public SDK):
  - `join_and_request_static_site` / `join_and_request_wasm_fixture`;
  - `failover_between_two_hosts` and `request_fails_over_when_host_restarts`;
  - `no_host_when_none_reachable` (returns within `request_timeout`);
  - `criterion5_client_via_remaining_host` (client + hosts A,B; A shut down; requests served via B);
  - `removed_client_gets_removed_event_and_errors`;
  - `open_reuses_identity`, `second_open_same_dir_refused`, `host_ticket_refused`, `join_into_used_dir_refused`;
  - `invite_from_client_works` (a new client joins with it);
  - `services_follow_deploy_and_remove_after_offline`;
  - `oversized_body_gets_local_413`;
  - `no_runtime_deps.rs` (same check as Task 5 for `mesh-client`).
- [ ] **Step 2: Run** `cargo test -p mesh-client -j 2` — FAIL.
- [ ] **Step 3: Implement** the SDK, `README.md` (Rust usage; Kotlin section added in Task 7) and `examples/fetch.rs` (`fetch join <dir> <ticket> <service> <path>` / `fetch open <dir> <service> <path>`).
- [ ] **Step 4: Run** `cargo test -p mesh-client -j 2`, `cargo build -p mesh-client --example fetch`, fmt, root clippy — PASS.
- [ ] **Step 5: Commit** `feat(client): mesh-client SDK`.

### Task 7: `mesh-client-ffi` — Kotlin bindings

**Files:** Create `crates/mesh-client-ffi/{Cargo.toml, uniffi.toml, src/lib.rs, src/bin/uniffi-bindgen.rs}`, `crates/mesh-client-ffi/kotlin/` (committed bindings under `net/meshnode/client/`), `scripts/gen-client-bindings.sh`, `scripts/build-client-aar.sh`, `crates/mesh-client-ffi/jvm-test/` (Gradle or plain `kotlinc` + JNA test, whichever builds offline here); Modify root `Cargo.toml` members, `crates/mesh-client/README.md`.

**Interfaces:**
- Consumes: Task 6 API.
- Produces (UniFFI, proc-macro style like `mesh-ffi`): object `MeshClient` with async constructors `join(dir, ticket, options)` / `open(dir, options)`, async `request(service, req: MeshRequest) -> MeshResponse`, `invite(role: MeshRole, ttl_secs: u64) -> String`, `remove(id: String)`, `shutdown()`; sync `status() -> MeshStatus`, `members() -> Vec<MeshMember>`, `services() -> Vec<MeshService>`; `set_listener(listener: Box<dyn MeshClientListener>)` delivering `MeshEvent`; records `MeshRequest { method, path, headers: Vec<MeshHeader>, body: Vec<u8> }`, `MeshResponse { status: u16, headers, body }`; error `MeshClientException` mirroring `ClientError`. Package `net.meshnode.client`, cdylib `mesh_client_ffi`.
- `scripts/gen-client-bindings.sh [--check]`: builds the host cdylib, generates Kotlin into `crates/mesh-client-ffi/kotlin`, `--check` diffs against the committed copy (UNIFFI_NO_FORMAT=1 as in `apps/android/gen-bindings.sh`).
- `scripts/build-client-aar.sh`: `cargo ndk -t arm64-v8a -t x86_64 build --release -p mesh-client-ffi`, then zips `jni/<abi>/libmesh_client_ffi.so`, `classes.jar` (compiled bindings) and a minimal `AndroidManifest.xml` into `dist/mesh-client.aar`.
- **JVM test decision (pins spec §6.4):** first try a JVM test that loads the HOST cdylib (Windows `mesh_client_ffi.dll` / Linux `.so`) via `jna.library.path`, starts nothing itself, and calls `MeshClient.join` with an invalid ticket expecting `MeshClientException.InvalidTicket`, plus `open` on an empty dir expecting `NotJoined` (proves loading, async bridging and error mapping without an in-process mesh). If the JVM toolchain is unavailable offline on the desktop, the fallback is a Rust test of every exported function plus `--check` on the bindings; record which in the report.

- [ ] **Step 1: Failing tests:** Rust `ffi_join_invalid_ticket_maps_error`, `ffi_request_roundtrip` (in-process host + SDK through the FFI types), `ffi_listener_receives_events`; the JVM test above.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement**, generate bindings, build the Android `.so` for both ABIs once (`cargo ndk`), build the AAR once.
- [ ] **Step 4: Run** `cargo test -p mesh-client-ffi -j 2`, `bash scripts/gen-client-bindings.sh --check`, the JVM test — PASS; fmt, clippy.
- [ ] **Step 5: Commit** `feat(client): Kotlin bindings for mesh-client`.

### Task 8: CI, manual check and back-ports

**Files:** Modify `.github/workflows/ci.yml`; Create `docs/manual-checks/sub-spec-8a.md`; Modify master spec §3, §5.8, §6, §8, §14 (row 8 → 8a implemented/manual check pending, 8b pending), sub-spec "As built" section.

- [ ] **Step 1:** CI jobs: `mesh-client` (tests of mesh-forward, mesh-client, mesh-client-ffi; the no-runtime-deps tests), `mesh-client-ffi-android` (bindings `--check`, `cargo ndk` build for both ABIs, the JVM test if Task 7 used it). Validate YAML with python yaml.
- [ ] **Step 2:** Manual check (spec §7): exact commands with `examples/fetch.rs`, pass criteria, results table; how to inspect the client's store size; removal step.
- [ ] **Step 3:** Back-ports and as-built notes (including the `Hello` scope field, `mesh-forward`, the local `413`).
- [ ] **Step 4:** Full verification: fmt, root clippy, `cargo test --workspace --all-features -j 2`, bindings `--check`.
- [ ] **Step 5: Commit** `docs: sub-spec 8a manual check and back-ports; ci: mesh-client jobs`.
