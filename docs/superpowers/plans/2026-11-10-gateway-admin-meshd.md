# Gateway, Admin API, meshd (sub-spec 4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `mesh-gateway` (localhost HTTP gateway, `httpfwd` forwarding, port mappings, status page), `mesh-admin` (admin protocol over a socket/pipe), and the `meshd` app (daemon, CLI, OS services, logging), plus the sub-spec 3 resource limits carried here.

**Architecture:** `mesh-core` registers extra ALPN handlers behind its membership gate; `mesh-gateway` provides a late-bound `httpfwd` handler (answers `503 Service not yet replicated` until `Services` is attached) and a forwarding client. `mesh-admin` defines the protocol, the transports and an `AdminBackend` trait; `apps/meshd` is a lib + bin crate whose `Daemon` implements the backend by composing `Node`, `Services` and the gateway.

**Tech Stack:** Rust 2024 (MSRV 1.90), tokio (incl. `tokio::net::windows::named_pipe`, `UnixListener`), hyper 1.12 + hyper-util 0.1, http-body-util, clap 4 (derive), toml, qrcode, tracing-appender, windows-service, windows-sys 0.61 (Win32 security, named pipes, event log).

**Spec:** `docs/superpowers/specs/2026-11-10-gateway-admin-meshd-design.md` (section numbers below refer to it). Also: `2026-11-09-node-core-design.md` §3.2 and `2026-11-09-runtime-services-design.md` §3, §5, §7.

## Global Constraints

- Workspace: `edition = "2024"`, `rust-version = "1.90"`; new deps in `[workspace.dependencies]` with `.workspace = true`; Windows-only deps under `[target.'cfg(windows)'.dependencies]`.
- Every task ends with `cargo test --workspace --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --all --check` passing; library crates also warning-free without features.
- Git needs `git -c safe.directory=D:/AI_Apps/Meshnode`; branch `feat/gateway-admin-meshd`. Commit trailers as given by your own system instructions.
- All store access from async code via `StoreHandle`; no blocking file or socket calls on the async runtime (use tokio APIs or `spawn_blocking`).
- Frames: `mesh_core::frame` codec (`u32` BE length + version byte + postcard). `httpfwd` frame cap 64 MiB + 64 KiB; admin frame cap 64 MiB.
- Values: gateway `127.0.0.1:7777`, default port map `7801 → "sync"`; 256 connections; 30 s header read; 60 s keep-alive; 64 MiB request-body cap; forward attempts 3, per-attempt timeout = `request_timeout_ms + 5 s`, forwarding semaphore 64; `httpfwd` stream read timeout 30 s, ≤ 256 streams per connection; ≤ 128 headers, ≤ 64 KiB header bytes, path ≤ 8 KiB; admin ≤ 16 connections, `FileData` ≤ 64 MiB, static upload ≤ 1 GiB, `LogsTail` ≤ 10 000 lines; shutdown bound 20 s; logs 7 daily files; memory budget default half of RAM capped at 4096 MiB.
- Retryable `503` bodies (exact): `Service not placed on this node`, `Service not yet replicated`, `Node re-bootstrapping`. Others (exact): `404 Service not deployed`, `404 Unknown host`, `421 Misdirected Request`, `502 No host could serve <name>`, `503 Memory budget exhausted`, `413` for bodies over the cap.
- Test API behind `cfg(any(test, feature = "test-hooks"))`; each new crate has a `test-hooks` feature and the self dev-dependency pattern.

## Review Focus

1. **A Host header naming an invalid service** (`Foo_Bar.mesh.localhost`, `a..mesh.localhost`, a 300-char label) must give `404 Unknown host` (after lower-casing), never a 500 or a panic. Test: Task 5 `invalid_service_host_404`.
2. **A peer answering a forward with an oversized or malformed response frame** must make the forwarder try the next host / return 502, never buffer unboundedly or panic. Test: Task 4 `forward_oversized_response_rejected`.
3. **Garbage or a huge length prefix on the admin endpoint** must close only that connection; the daemon keeps serving others. Test: Task 6 `garbage_frame_closes_connection_only`.
4. **`meshd join` with a mistyped ticket** must print a readable error and exit 1, not panic. Test: Task 8 `join_bad_ticket_readable_error`.
5. **`meshd deploy static` over a directory containing a symlink that points outside it** must not upload the target (skip symlinks, warn). Test: Task 8 `deploy_static_skips_symlinks`.

---

### Task 1: Proto — `httpfwd` messages

**Files:** Create `crates/mesh-proto/src/httpfwd.rs`; modify `crates/mesh-proto/src/lib.rs`.

**Interfaces — Produces:** `ForwardRequest { service: String, method: String, path_and_query: String, headers: Vec<(String, Vec<u8>)>, body: Vec<u8> }`, `ForwardResponse { status: u16, headers: Vec<(String, Vec<u8>)>, body: Vec<u8> }` with decode-time validation (spec §3.1: valid service name, HTTP-token method, ≤ 128 headers, ≤ 64 KiB header bytes, `path_and_query` starts with `/` and ≤ 8 KiB, status 100–999); `pub const ALPN_HTTPFWD: &[u8] = b"meshnode/httpfwd/1"`; `pub const HTTPFWD_MAX_FRAME: usize = 64 * 1024 * 1024 + 64 * 1024`; `pub fn is_hop_by_hop(name: &str) -> bool` (case-insensitive list from spec §3.1).

- [ ] **Step 1: Failing tests** in `httpfwd.rs`: round trip both types via `encode`/`decode`; rejects: invalid service, method `"GE T"`, 129 headers, 65 KiB of header bytes, path without leading `/`, 8 KiB + 1 path, status 99; `is_hop_by_hop` true for `connection`, `Keep-Alive`, `transfer-encoding`, `TE`, `upgrade`, `Proxy-Authorization`, false for `host`, `content-type`; version byte `2` → `ProtoError::Version(2)`.
- [ ] **Step 2: Run** `cargo test -p mesh-proto httpfwd` — FAIL.
- [ ] **Step 3: Implement** (follow `join.rs`/`service.rs` stash-validation pattern).
- [ ] **Step 4: Run** `cargo test -p mesh-proto` — PASS.
- [ ] **Step 5: Commit** `feat(proto): httpfwd forward messages`.

### Task 2: `mesh-core` — extra gated protocols, `NodeHandle::connect`

**Files:** Modify `crates/mesh-core/src/{config.rs, node.rs, transport.rs, lib.rs}`; tests in `crates/mesh-core/tests/node.rs`.

**Interfaces — Produces:** `NodeConfig.extra_protocols: Vec<(Vec<u8>, Arc<dyn iroh::protocol::DynProtocolHandler>)>` (default empty; check iroh 1.4.1's dyn-handler type name and use what `Router::accept` takes), registered behind `Gate` in node start; `NodeHandle::connect(&self, peer: NodeId, alpn: &[u8]) -> Result<iroh::endpoint::Connection, NodeError>` (refuses non-effective peers with `NodeError::NotEffective`; uses known addresses/hints like the peer manager); re-export what callers need (`iroh::endpoint::Connection`, the handler trait) from `mesh_core`.

- [ ] **Step 1: Failing tests:** `extra_protocols_are_gated` (A with an echo handler on ALPN `b"test/echo"`; member B connects via `NodeHandle::connect` and echoes; a bare non-member iroh endpoint is closed with `NOT_MEMBER`); `node_handle_connect_reaches_member`; `connect_to_non_member_refused`.
- [ ] **Step 2: Run** `cargo test -p mesh-core --all-features --test node` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p mesh-core --all-features` — PASS.
- [ ] **Step 5: Commit** `feat(core): extra gated protocols and NodeHandle::connect`.

### Task 3: Runtime/services — memory budget, static in flight, compact memory, `local_state`

**Files:** Modify `crates/mesh-runtime/src/{lib.rs, engine.rs, handle.rs}`, `crates/mesh-services/src/{ops.rs, dispatch.rs, lib.rs}`; tests `crates/mesh-runtime/tests/limits.rs`, `crates/mesh-services/tests/deployer.rs`.

**Interfaces — Produces:**
- `RuntimeConfig.max_memory_mb: u32` (default `default_memory_budget_mb()` = half of physical RAM capped at 4096; implement RAM detection with `windows-sys` `GlobalMemoryStatusEx` on Windows, `/proc/meminfo` on Linux/Android, `sysctl hw.memsize` via `libc` on macOS — fall back to 4096 on error) and `RuntimeConfig.compact_memory: bool` (default `cfg!(target_os = "android")`).
- `Runtime::handle` reserves `limits.memory_mb` from a weighted semaphore before instantiation (wait counts against the deadline; `503 Memory budget exhausted` on timeout or when `memory_mb > max_memory_mb`).
- `Runtime::acquire_slot(&self, deadline: Instant) -> Option<SlotGuard>` (the in-flight semaphore) used by the static path in `mesh-services`.
- `ServicesConfig.max_memory_mb: Option<u32>` (None = default) passed through.
- `pub enum LocalState { Active, NotPlaced, NotReady, Frozen, NotDeployed }` and `Services::local_state(&self, name: &str) -> LocalState`.

- [ ] **Step 1: Failing tests:** `memory_budget_waits_then_503` (budget 64, two 48 MiB requests: second waits, then 503 `Memory budget exhausted` when the first holds past the second's timeout; when the first finishes in time, the second succeeds); `request_larger_than_budget_503` (immediate); `compact_memory_engine_serves` (`compact_memory: true` → `echo` works); `static_counts_in_flight` (`max_in_flight = 1`, a slot held by a stalled WASM request → a static request waits and times out per its deadline / is served once released); `local_state_reports_each_state` (Active, NotPlaced, NotReady, NotDeployed; Frozen via the re-bootstrap hold hook).
- [ ] **Step 2: Run** the focused tests — FAIL.
- [ ] **Step 3: Implement** (compact memory: choose wasmtime 50 `Config` calls such as `memory_reservation(0)`/small, `memory_guard_size(64 KiB)`, `memory_may_move(true)` — read the API and document the choice).
- [ ] **Step 4: Run** `cargo test -p mesh-runtime -p mesh-services --all-features` — PASS.
- [ ] **Step 5: Commit** `feat(runtime,services): memory budget, static in flight, compact memory, local_state`.

### Task 4: `mesh-gateway` — `httpfwd` handler and forwarder

**Files:** Create `crates/mesh-gateway/{Cargo.toml, src/lib.rs, src/forward.rs, src/serve_httpfwd.rs, src/error.rs}`, `crates/mesh-gateway/tests/{common/mod.rs, forward.rs}`.

**Interfaces — Consumes:** Task 1 types, Task 2 `extra_protocols`/`connect`, Task 3 `local_state`, `Services::handle`, `placed_hosts`/`services()` status, `NodeHandle::members()` (`online`, `rtt_ms`, role).
**Produces:**
- `pub struct HttpFwdHandler` (implements iroh's protocol handler) with `HttpFwdHandler::new() -> (HttpFwdHandler, HttpFwdSlot)`; `HttpFwdSlot::attach(&self, services: Services)` (late binding; before attach every request answers `503 Service not yet replicated`).
- `pub struct Forwarder` with `Forwarder::new(node: NodeHandle, services: Services) -> Forwarder` and `Forwarder::dispatch(&self, name: &str, req: http::Request<Bytes>) -> http::Response<Bytes>` (spec §3.3: local when `Active`, `404 Service not deployed`, else forward with up to 3 attempts, retryable bodies, `502 No host could serve <name>`, per-peer connection cache, forwarding semaphore 64, hop-by-hop stripping).
- Test harness: multi-node meshes with `Services` + `HttpFwdHandler` wired in (copy the minimal pieces of `mesh-services/tests/common`).

- [ ] **Step 1: Failing tests:** `forward_to_placed_host` (counter placed only on A; B dispatches → A's count); `forward_skips_unreachable_then_succeeds` (placed on A and C; A partitioned from B; B reaches C); `forward_all_fail_502` (exact body); `forwarded_request_never_reforwarded` (A receives an `httpfwd` request for a service it doesn't run → retryable 503, no outgoing forward — count connections/forwards via a test hook); `client_role_always_forwards`; `hop_by_hop_headers_stripped`; `unattached_handler_answers_retryable_503`; `forward_oversized_response_rejected` (Review Focus 2: a raw test handler answers with a length prefix > `HTTPFWD_MAX_FRAME` and with garbage bytes → next host / 502, no panic).
- [ ] **Step 2: Run** `cargo test -p mesh-gateway --all-features --test forward` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** 3× — PASS.
- [ ] **Step 5: Commit** `feat(gateway): httpfwd forwarding`.

### Task 5: `mesh-gateway` — localhost server, routing, ports, status page

**Files:** Create `crates/mesh-gateway/src/{server.rs, routing.rs, status_page.rs, config.rs}`; tests `crates/mesh-gateway/tests/server.rs`.

**Interfaces — Produces:** `pub struct GatewayConfig { listen: SocketAddr, ports: BTreeMap<u16, String> }` (`Default` = spec values; `validate()` rejects non-loopback and invalid service names); `Gateway::start(cfg: GatewayConfig, forwarder: Forwarder, node: NodeHandle, services: Services) -> Result<Gateway, GatewayError>` (main bind failure → `GatewayError::Bind { addr, source }`; mapped-port bind failure logged and skipped); `Gateway::local_addrs(&self) -> Vec<(SocketAddr, Option<String>)>` (test aid); `Gateway::shutdown(self)`. Routing per spec §4.2; status page per §4.3 (`/` HTML, `/status.json`).

- [ ] **Step 1: Failing tests** (real HTTP client over loopback — use `hyper-util`'s client or a minimal tokio TCP client; bind port 0): `host_routing_to_local_service`; `unknown_host_404`; `invalid_service_host_404` (Review Focus 1); `status_page_and_json` (both routes; a member name containing `<script>` appears escaped; POST to `/` → 405 or 404, no state change); `mapped_port_serves_service`; `mapped_port_rejects_foreign_host_421`; `oversized_body_413`; `non_loopback_listen_rejected`; `mapped_port_bind_failure_skipped` (occupy a port first; the main listener still serves); `main_port_in_use_fails` (`GatewayError::Bind` naming the address).
- [ ] **Step 2: Run** `cargo test -p mesh-gateway --all-features --test server` — FAIL.
- [ ] **Step 3: Implement** (limits from Global Constraints).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gateway): localhost server, routing, port mappings, status page`.

### Task 6: `mesh-admin` — protocol, transports, server, client

**Files:** Create `crates/mesh-admin/{Cargo.toml, src/lib.rs, src/proto.rs, src/transport.rs, src/transport_unix.rs, src/transport_windows.rs, src/server.rs, src/client.rs}`; tests `crates/mesh-admin/tests/admin.rs`.

**Interfaces — Produces:**
- Types exactly as spec §5.1 (`AdminRequest`, `AdminResponse`, `FileMeta`, `FileData`, `ErrorCode`, `DaemonState`, `Hello`, plus view structs `StatusInfo`, `MemberInfoView`, `ServiceInfoView` with the fields the CLI prints).
- `pub fn endpoint_for(data_dir: &Path) -> Endpoint` (Unix: `<data>/admin.sock`; Windows: `\\.\pipe\meshnode-admin` when `data_dir` is the OS default, else `meshnode-admin-<8 hex of blake3(canonical path)>`).
- `#[async_trait] pub trait AdminBackend: Send + Sync { fn state(&self) -> DaemonState; async fn handle(&self, req: AdminRequest, io: &mut RequestIo) -> AdminResponse; }` where `RequestIo` lets the handler read `FileData` frames and write streamed `LogLines` frames.
- `AdminServer::bind(endpoint: &Endpoint, access: Access, backend: Arc<dyn AdminBackend>) -> Result<AdminServer, AdminError>`; `Access::{CurrentUserOnly, ServiceGroup { group: String } /* unix */, WindowsSids(Vec<String>) /* windows: daemon account, Administrators, recorded user */}`; refuses with `AdminError::InUse` if another server is live on the endpoint (Unix: a connectable socket; stale socket file is removed); ≤ 16 connections; `shutdown()`.
- `AdminClient::connect(endpoint) -> Result<AdminClient, AdminError>` (`AdminError::NotRunning` when nothing listens; `AdminError::VersionMismatch { cli, daemon }`), `request(&mut self, req) -> Result<AdminResponse, AdminError>`, `deploy_static(&mut self, name, opts, files: Vec<(FileMeta, PathBuf)>)`, `logs_tail(&mut self, lines, follow) -> impl Stream<Item = Result<Vec<String>, AdminError>>`.
- Windows: pipe created with a security descriptor built from SDDL (`ConvertStringSecurityDescriptorToSecurityDescriptorW`) — e.g. `D:P(A;;GA;;;<daemon SID>)(A;;GA;;;BA)(A;;GA;;;<user SID>)`; Unix: socket mode/group set after bind.

- [ ] **Step 1: Failing tests** (a fake `AdminBackend`): `hello_version_mismatch_closes`; `request_round_trip_every_variant` (fake echoes a typed response per variant); `deploy_static_upload` (frames received match; traversal path `../x`, absolute path, size mismatch, a 1 GiB + 1 total → `InvalidArgument`); `logs_tail_follow_streams`; `garbage_frame_closes_connection_only` (Review Focus 3); `endpoint_in_use_refused`; `not_running_reported`; `error_codes_render` (each `ErrorCode` → a readable `Display`); `seventeenth_connection_waits_or_refused`.
- [ ] **Step 2: Run** `cargo test -p mesh-admin --all-features` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** on Windows (and the Unix path compiles via `cargo check --target x86_64-unknown-linux-gnu` if the target is installed; otherwise CI covers it) — PASS.
- [ ] **Step 5: Commit** `feat(admin): admin protocol, socket/pipe transports, server and client`.

### Task 7: `meshd` lib — config, logging, `Daemon`

**Files:** Create `apps/meshd/{Cargo.toml, src/lib.rs, src/config.rs, src/logging.rs, src/daemon.rs, src/paths.rs}`; modify root `Cargo.toml` (`members` += `"apps/*"`); tests `apps/meshd/tests/daemon.rs`.

**Interfaces — Consumes:** Tasks 2–6. **Produces:**
- `paths::default_data_dir() -> PathBuf` (spec §6.1 per OS), honouring `MESHD_DATA`.
- `MeshdConfig` (serde, spec §6.1 TOML), `MeshdConfig::load_or_default(dir)`, `save(dir)` (atomic), `set(key, value) -> Result<bool /*restart_required*/, ConfigError>` with spec validation.
- `logging::init(data_dir, level) -> LoggingGuard` (stdout + daily rolling file, 7 kept).
- `Daemon::start(data_dir: PathBuf, cfg: MeshdConfig) -> Result<Daemon, DaemonError>`: Uninitialised (admin only) or Running (Node with `extra_protocols = [(ALPN_HTTPFWD, httpfwd_handler)]` → `Services::start` → `slot.attach(services)` → `Gateway::start`) — and `impl AdminBackend for Daemon` covering every request (Init/Join transitions in place; ResetIdentity stops and wipes all but `meshd.toml`; DeployStatic stages under `<data>/tmp/<random>` and cleans up; LogsTail reads the newest log file); `Daemon::run_until(self, shutdown: impl Future)` (admin server + orderly 20 s shutdown).

- [ ] **Step 1: Failing tests:** `uninitialised_accepts_only_init_join`; `init_then_status_running`; `join_through_daemon` (two daemons on temp dirs; B joins A's ticket via its admin endpoint); `every_request_against_real_daemon` (status, members, invite, set_info, deploy wasm counter + call through the gateway, deploy static, set_placement/outbound/limits, undeploy, purge, config get/set, logs tail); `reset_identity_only_when_removed`; `config_set_validation` (bad listen, port 80, unknown key → errors; `name` applies at once; `gateway.listen` → restart_required); `shutdown_within_20s`.
- [ ] **Step 2: Run** `cargo test -p meshd --all-features --test daemon` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(meshd): config, logging and the daemon`.

### Task 8: `meshd` CLI

**Files:** Create `apps/meshd/src/{main.rs, cli.rs, output.rs, local.rs}`; tests `apps/meshd/tests/cli.rs`.

**Interfaces — Produces:** the commands, options, outputs and exit codes of spec §6.2 (clap derive); `local.rs` = `init`/`join`/`reset-identity` on the data dir when no daemon is reachable (local join waits ≤ 60 s for first sync); `invite` prints the ticket and a terminal QR (`qrcode`, unicode dense render); `--json` on read commands; `remove <id-prefix>` resolves a unique prefix (ambiguous/unknown → exit 1 with a message); deploy static walks the directory (relative `/` paths, skips symlinks with a warning, Review Focus 5).

- [ ] **Step 1: Failing tests** (run the built binary via `env!("CARGO_BIN_EXE_meshd")` with temp `--data`): `cli_reports_daemon_not_running` (exit 3, exact message); `init_local_then_status_through_daemon`; `join_bad_ticket_readable_error` (Review Focus 4, exit 1, no panic text); `deploy_static_skips_symlinks` (symlink creation may need privileges on Windows — skip with a note if `std::os::windows::fs::symlink_file` fails); `tags_and_nodes_mutually_exclusive` (exit 2); `json_output_parses`; `remove_ambiguous_prefix_error`.
- [ ] **Step 2: Run** `cargo test -p meshd --all-features --test cli` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(meshd): CLI`.

### Task 9: OS services

**Files:** Create `apps/meshd/src/service/{mod.rs, linux.rs, macos.rs, windows.rs, eventlog.rs}`; golden files `apps/meshd/tests/golden/{meshnode.service, net.meshnode.meshd.plist}`; tests `apps/meshd/tests/service.rs`.

**Interfaces — Produces:** `service::install(data: &Path) / uninstall(purge_data: bool) / start() / stop() / status() -> Result<ServiceStatus, ServiceError>` per spec §6.4 (privilege check first: Unix `geteuid() == 0`, Windows token elevation); `linux::unit_file(binary, data) -> String`, `macos::plist(binary, data) -> String` (pure, golden-tested); Windows: `windows-service` registration (LocalService, delayed auto start, failure actions 3 × 5 s), data-dir DACL, SID recorded in `meshd.toml`, Event Log source `Meshnode`; `meshd run --service` enters the Windows service dispatcher and maps service stop to the daemon's shutdown; Event Log entries for start/stop/fatal.

- [ ] **Step 1: Failing tests:** `systemd_unit_golden`; `launchd_plist_golden`; `install_without_privileges_refused` (non-elevated test process → `ServiceError::NeedsPrivileges` with the spec message); `windows_install_roundtrip` `#[ignore]` (needs Administrator: install → status running → stop → uninstall).
- [ ] **Step 2: Run** `cargo test -p meshd --all-features --test service` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS; run the ignored Windows test once elevated if possible and record the result (or say it could not be run).
- [ ] **Step 5: Commit** `feat(meshd): OS service install for systemd, Windows and launchd`.

### Task 10: End-to-end, CI, manual check, back-ports

**Files:** Create `apps/meshd/tests/e2e.rs`, `docs/manual-checks/sub-spec-4.md`; modify `.github/workflows/ci.yml`, `docs/superpowers/specs/2026-11-08-meshnode-design.md`.

- [ ] **Step 1: E2E test** `two_daemons_serve_and_remove`: start two `meshd run` child processes on temp data dirs with ports 0/free ports (add `gateway.listen` via config), `init` A, `invite`, `join` B, deploy a static site on A, fetch `http://site.mesh.localhost:<B port>/` through B's gateway (send the Host header explicitly) and get the file; `remove` B from A; B's status reaches removed/refused; both daemons stop cleanly. Run 3×.
- [ ] **Step 2: CI:** macOS job `cargo build -p meshd`; Android job adds `-p mesh-gateway -p mesh-admin`; Linux/Windows test jobs unchanged otherwise.
- [ ] **Step 3: Manual check doc** with spec §8.2's six steps, a results table, Linux pending, macOS untested.
- [ ] **Step 4: Back-ports** spec §2 items 1–7 into the master spec (§3 crate table adds `mesh-gateway`, `mesh-admin`, `apps/meshd` deps; §8 forwarding details; §9 CLI-through-daemon and uninitialised daemon; §10.1 logging; §14 row 4 "implemented, manual check pending"), each marked "(back-ported from sub-spec 4)".
- [ ] **Step 5: Full verification** (workspace tests, clippy with and without features, fmt, `cargo ndk -t arm64-v8a build -p mesh-gateway -p mesh-admin`).
- [ ] **Step 6: Commit** `test(meshd): end-to-end; ci; docs: back-port sub-spec 4`.
