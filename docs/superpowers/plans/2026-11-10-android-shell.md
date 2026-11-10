# Android Shell (sub-spec 5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a Meshnode host on Android: `meshd`'s daemon moved into a shared `mesh-host` crate, wrapped by `mesh-ffi` (UniFFI), inside a Tauri 2 Android app whose Kotlin foreground service keeps the node alive, restarts it, schedules wall-clock work and applies the power policy.

**Architecture:** `mesh-host` holds the platform-neutral daemon (`Daemon`, config, lock, logging, admin handling) plus Android hooks (`wake`, power decisions); `meshd` becomes a thin desktop binary over it. `mesh-admin` gains an in-process transport. `mesh-ffi` exposes one global host to Kotlin; the Tauri app's single `cdylib` links it, so the service and the webview share one node.

**Tech Stack:** Rust 2024 (iroh 1.4, wasmtime 50, redb), UniFFI 0.29 (proc-macro), `jni` 0.21, `ndk-context` 0.1, `tracing-android`, Tauri 2 (Android), Kotlin + Gradle (AGP from `tauri android init`), WorkManager, JDK 17, Android SDK 35 / build-tools 35.0.0 / NDK 27.2.12479018.

**Spec:** `docs/superpowers/specs/2026-11-10-android-shell-design.md`

## Global Constraints

- minSdk 29, targetSdk 35, compileSdk 35, build-tools `35.0.0`, NDK `27.2.12479018`; ABIs arm64-v8a (+ x86_64 for the emulator).
- Android package / application id: `net.meshnode.app`. Rust cdylib name: `meshnode_app`.
- Foreground service type `specialUse`; permissions: `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_SPECIAL_USE`, `POST_NOTIFICATIONS`, `RECEIVE_BOOT_COMPLETED`, `WAKE_LOCK`, `SCHEDULE_EXACT_ALARM`, `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`, `INTERNET`, `ACCESS_NETWORK_STATE`.
- Notification text: `Meshnode: hosting N services · M peers online`; start failure: `Meshnode couldn't start: <reason> — tap to open`.
- Config: `meshd.toml` `[android] power = "balanced" | "always_on" | "saver"` (default `balanced`), `[android] periodic_wake_s = 0`.
- Timings: restart alarm ≈ 5 s; watchdog 15 min; wall-clock alarm 15 min inexact; alarm receiver wake lock 10 s; `wake()` returns after ≤ 10 s; incoming-after-silence threshold 60 s; Balanced request lock ≤ request timeout + 30 s; catch-up lock ≤ 60 s; shutdown limit 20 s (unchanged).
- Reserved `httpfwd` service `_ping` → `204`. Saver + screen off: inbound `httpfwd` → `503 Service not yet replicated`.
- Data dir on Android: `context.filesDir/meshnode`.
- Desktop behaviour of `meshd` must not change. `cargo test --workspace --all-features -j 4`, clippy `-D warnings` (with and without features), `cargo fmt --check` stay green after every task.
- Build env: D: is FAT32 — use `CARGO_TARGET_DIR=%LOCALAPPDATA%\meshnode-target`. Git: `git -c safe.directory=D:/AI_Apps/Meshnode`.
- **Safety:** this is the owner's real Windows PC. Never install/start/stop/uninstall OS services, never write to system locations, never run `--ignored` tests. No device or emulator is required for any task; never `adb install` onto a device.
- Commit trailers: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01VKLL6Rs4i1E9FwhqtXKgyB`.

## Review Focus

1. **Monotonic vs wall clock:** the "60 s of silence" check must use the node's `WallClock` (ms), not `Instant` — monotonic time stops during suspend, which is exactly when it matters. Test with a fake wall clock (Task 3).
2. **`start()` called repeatedly / concurrently** (boot + alarm + `START_STICKY` racing): exactly one `Daemon`, no second `meshd.lock` attempt failing as an error (Task 7 test `concurrent_start_is_single`).
3. **`stop()` while `start()` is still running** (user taps Stop during a slow start): ends stopped, lock released (Task 7 test `stop_during_start_ends_stopped`).
4. **Wake locks always released:** a request that errors or times out, or a policy change mid-request, must release its lock; every acquire carries `max_ms` so Android's own timeout is the backstop (Task 5 tests).
5. **The move to `mesh-host` changing behaviour silently** (shutdown order, admin socket path, `KEPT_ON_RESET`): the pure-move commit runs the unchanged daemon tests (Task 1).

---

### Task 1: Extract `mesh-host` from `apps/meshd` (pure move, then `HostOptions`)

**Files:**
- Create: `crates/mesh-host/{Cargo.toml, src/lib.rs}`; move `apps/meshd/src/{config.rs, daemon.rs, daemon/requests.rs, lock.rs, logging.rs, paths.rs}` → `crates/mesh-host/src/`; move `apps/meshd/tests/daemon.rs` → `crates/mesh-host/tests/daemon.rs`
- Modify: `apps/meshd/{Cargo.toml, src/lib.rs, src/*.rs imports}`, root `Cargo.toml` (`members = ["crates/*", "apps/meshd"]`)

**Interfaces:**
- Produces: crate `mesh_host` with `Daemon`, `DaemonError`, `HostConfig` (renamed from `MeshdConfig`; `meshd` keeps `pub use mesh_host::HostConfig as MeshdConfig`), `ConfigError`, `wipe_identity`, `lock`, `logging`, `paths`, `TestOptions` (feature `test-hooks`), and new:
  - `pub struct HostOptions { pub admin_socket: bool, pub periodic: Periodic, pub power: Option<Arc<dyn WakeLockSink>> }` with `Default` = `{ admin_socket: true, periodic: Periodic::Timers, power: None }` (`Periodic`, `WakeLockSink` defined here as stubs, filled in Task 5).
  - `Daemon::start_opts(data_dir, cfg, HostOptions) -> Result<Daemon, DaemonError>`; `Daemon::start` = `start_opts(.., HostOptions::default())`. `TestOptions` gains `host: HostOptions` (default).
  - When `admin_socket == false`, no `AdminServer` is bound; everything else is identical.
- Ruling carried from the spec: `wasm_reservation` is **not** a `HostOptions` field — `RuntimeConfig::compact_memory` already defaults to on for `target_os = "android"`.

- [ ] **Step 1:** Commit A — pure move: `git mv` the files, fix `use` paths and `Cargo.toml` deps (`mesh-host` gets `test-hooks` feature forwarding to the same inner features `meshd` used; `meshd`'s `test-hooks` forwards to `mesh-host/test-hooks`). No logic change, no renames other than paths.
- [ ] **Step 2:** Run `cargo test --workspace --all-features -j 4` — expect the same test count as before the move (485 passed / 2 ignored at master `0249d0d`). Commit `refactor: move meshd's daemon into mesh-host (pure move)`.
- [ ] **Step 3:** Commit B — rename `MeshdConfig` → `HostConfig` (alias in `meshd`), add `HostOptions` + `start_opts`, add test `crates/mesh-host/tests/daemon.rs::no_admin_socket_when_disabled` (start with `admin_socket: false`; assert `<data>/admin.sock` absent on Unix / `AdminClient::connect` to the data dir's endpoint returns `NotRunning`; node still initialises via the in-crate request path used by existing tests — if that path is the socket, initialise with `admin_socket: true` first, shut down, restart with `false`, and assert `status` through `Daemon::state()` equivalents).
- [ ] **Step 4:** `cargo ndk -t arm64-v8a build -p mesh-host` succeeds. Full workspace tests + clippy (with/without `--all-features`) + fmt green. Commit `feat(mesh-host): HostOptions`.

### Task 2: `mesh-admin` in-process transport

**Files:**
- Create: `crates/mesh-admin/src/in_process.rs`
- Modify: `crates/mesh-admin/src/{server.rs, lib.rs}`; Test: `crates/mesh-admin/tests/admin.rs`

**Interfaces:**
- Produces: `pub struct InProcessAdmin` (Clone) with
  - `pub fn new(backend: Arc<dyn AdminBackend>) -> InProcessAdmin`
  - `pub fn daemon_state(&self) -> DaemonState`
  - `pub async fn request(&self, req: AdminRequest) -> AdminResponse`
  - `pub async fn deploy_static(&self, name: String, opts: DeployOpts, files: Vec<(FileMeta, Vec<u8>)>) -> AdminResponse`
  - `pub fn logs_tail(&self, req: AdminRequest) -> impl Stream<Item = Result<String, AdminResponse>>` (lines until the ending response; an `Ok` end yields nothing more, an error end yields `Err(resp)`).
- `server.rs`: the gating `check()` and `handle_one()` dispatch become one `pub(crate) async fn dispatch(backend, req, io) -> AdminResponse` used by both transports; `RequestIo` gets an in-memory variant (files from a `Vec`, lines to an mpsc). Upload validation (count, sizes, paths via `storable_path`) is identical.

- [ ] **Step 1:** Write failing tests in `admin.rs`, each mirroring an existing socket test with the same fake backend: `in_process_uninitialised_gating`, `in_process_removed_allows_reset_only`, `in_process_deploy_static_round_trip`, `in_process_upload_rejects_bad_path`, `in_process_upload_rejects_count_over_limit`, `in_process_logs_tail_streams_then_ends`.
- [ ] **Step 2:** Run `cargo test -p mesh-admin --all-features in_process` — FAIL (no `InProcessAdmin`).
- [ ] **Step 3:** Implement; the socket tests must still pass unchanged.
- [ ] **Step 4:** `cargo test -p mesh-admin --all-features` green; clippy; commit `feat(mesh-admin): in-process transport`.

### Task 3: `mesh-core` — incoming-traffic catch-up and `wake`

**Files:**
- Modify: `crates/mesh-core/src/{config.rs, node.rs, peers/mod.rs}` (where inbound `sync/1` connections/messages are handled); Test: `crates/mesh-core/tests/node.rs`

**Interfaces:**
- Produces:
  - `NodeConfig.catch_up_on_incoming_after: Option<Duration>` (default `None`).
  - `Node::set_catch_up_on_incoming(&self, after: Option<Duration>)` (runtime toggle; Saver uses it).
  - `Node::wake(&self, limit: Duration) -> WakeOutcome` where `pub struct WakeOutcome { pub peers: usize, pub rounds_completed: usize, pub timed_out: bool }`: calls `network_change()` then `sync_now()`, and waits for `NodeEvent::RoundComplete` from every peer connected at call time, or `limit`.
- Silence is measured with the node's `WallClock` (`cfg.wall`, ms): record `last_peer_traffic_ms` on every inbound connection accept and inbound `sync/1` message; on such an event, if `after` is `Some(d)` and `now_ms - last ≥ d`, call `start_round_all()` (once per event, not per message burst).

- [ ] **Step 1:** Failing tests (fake `WallClock` that the test advances; two nodes with `Timings::fast_for_tests()`): `incoming_after_silence_triggers_catch_up` (advance wall clock 61 s, peer sends → a `RoundComplete` arrives without waiting for the periodic timer — make periodic timers long in this test), `incoming_within_silence_does_not` (advance 30 s → no extra round within 2 s), `catch_up_toggle_off` (`set_catch_up_on_incoming(None)` → no extra round), `wake_completes_rounds_with_connected_peers` (`wake(10s)` → `rounds_completed == peers == 1`, `!timed_out`), `wake_with_no_peers_returns_immediately`.
- [ ] **Step 2:** Run — FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p mesh-core --all-features` green (run the new tests 3×); commit `feat(mesh-core): wall-clock incoming catch-up and wake`.

### Task 4: `mesh-gateway` — `_ping`, pause, request activity

**Files:**
- Modify: `crates/mesh-gateway/src/{serve_httpfwd.rs, lib.rs}`, `crates/mesh-proto/src/httpfwd.rs` (`pub const PING_SERVICE: &str = "_ping";`); Test: `crates/mesh-gateway/tests/forward.rs`

**Interfaces:**
- Produces on `HttpFwdSlot`:
  - `pub fn set_paused(&self, paused: bool)` — while paused, new inbound streams answer `503 Service not yet replicated` (same response as `not_replicated()`); `_ping` still answers 204.
  - `pub fn set_activity(&self, f: Arc<dyn Fn(Activity) + Send + Sync>)` with `pub enum Activity { Start, End }` — `Start` when an inbound stream begins serving, `End` when its response is written, failed, reset or timed out (every `Start` has exactly one `End`, including on error paths).
- `_ping`: `ForwardRequest { service: "_ping", .. }` → `204`, empty body, before the services lookup and regardless of attach/detach/pause. `valid_service_name("_ping")` is false (assert in a test), so no user service collides.

- [ ] **Step 1:** Failing tests: `ping_service_answers_204` (even with no services attached), `ping_gated_for_non_members` (non-member connect refused as for any `httpfwd`), `paused_answers_503_then_resumes`, `activity_start_end_balanced_on_success_error_and_timeout`, `ping_name_is_not_a_valid_service_name`.
- [ ] **Step 2:** Run — FAIL. **Step 3:** Implement. **Step 4:** `cargo test -p mesh-gateway --all-features` green; commit `feat(mesh-gateway): _ping, pause, request activity`.

### Task 5: `mesh-host` — Android hooks: periodic, power policy, wake

**Files:**
- Create: `crates/mesh-host/src/power.rs`
- Modify: `crates/mesh-host/src/{lib.rs, config.rs, daemon.rs, daemon/requests.rs}`; Test: `crates/mesh-host/tests/android_hooks.rs`, unit tests in `power.rs`

**Interfaces:**
- Consumes: Task 3 (`catch_up_on_incoming_after`, `set_catch_up_on_incoming`, `Node::wake`), Task 4 (`set_paused`, `set_activity`), Task 2 (`InProcessAdmin`).
- Produces:
  - `pub enum Periodic { Timers, Driven }` — `Driven` sets `catch_up_on_incoming_after = Some(60 s)`.
  - `pub enum PowerPolicy { Balanced, AlwaysOn, Saver }` (serde `balanced` / `always_on` / `saver`); `HostConfig.android: AndroidConfig { power: PowerPolicy, periodic_wake_s: u32 }` under `[android]`, defaults `Balanced`, `0`; omitted from the file when default.
  - `pub trait WakeLockSink: Send + Sync { fn acquire(&self, tag: &str, max_ms: u64); fn release(&self, tag: &str); }`
  - `pub struct PowerState { pub policy: PowerPolicy, pub charging: bool, pub screen_on: bool }`
  - Pure: `pub fn wake_lock_for(state: &PowerState, activity: LockActivity, request_timeout_ms: u64) -> Option<(&'static str, u64)>` with `pub enum LockActivity { Idle, Request, CatchUp, Alarm { periodic_wake_s: u32 } }`. Table (tags `"always"`, `"charging"`, `"request"`, `"catchup"`, `"alarm"`):
    - AlwaysOn → `("always", u64::MAX)` for every activity.
    - Balanced: charging → `("charging", u64::MAX)`; Request → `("request", request_timeout_ms + 30_000)`; CatchUp → `("catchup", 60_000)`; Alarm with `s > 0` → `("alarm", s*1000)`; Idle / Alarm 0 → `None`.
    - Saver: Alarm → `("alarm", 10_000)` only; everything else `None`.
  - `pub fn saver_paused(state: &PowerState) -> bool` = `policy == Saver && !screen_on`.
  - `Daemon` methods: `pub async fn wake(&self, reason: WakeReason) -> WakeOutcome` (`WakeReason { Alarm, Network, Boot, Incoming }`; limit 10 s; no-op outcome when not running; takes the `"catchup"` lock per `wake_lock_for` for its duration); `pub fn network_changed(&self)`; `pub fn set_power(&self, state: PowerState)` (applies `saver_paused` → `slot.set_paused` and `set_catch_up_on_incoming(None | Some(60 s))`, and acquires/releases `"always"`/`"charging"` per the table); `pub fn in_process_admin(&self) -> InProcessAdmin`; `pub fn status(&self) -> HostStatus` (`{ state, node_id: Option<String>, mesh_name: Option<String>, services_hosted: u32, peers_online: u32, gateway_ports: Vec<u16> }`).
  - Request activity: when `power` is set, the daemon registers `set_activity` on each node start; a ref-count of in-flight requests holds `"request"` (acquire at 0→1 with the table's max, release 30 s after 1→0 unless a new request arrives).

- [ ] **Step 1:** Failing tests: `power.rs` unit `power_decision_table` (every policy × charging × screen × activity row above), `saver_paused_only_screen_off`; `android_hooks.rs` (two daemons, `TestOptions::fast()`, a recording `WakeLockSink`): `wake_triggers_catch_up`, `wake_when_uninitialised_is_noop`, `saver_screen_off_refuses_httpfwd_503` (desktop daemon forwards to the paused phone-daemon only → 503; set screen on → 200), `request_lock_acquired_and_released` (one forwarded request → `acquire("request", ..)` then `release("request")` within 35 s of paused tokio time), `always_on_holds_until_policy_change`, `android_config_round_trip` (default omitted; non-default persisted).
- [ ] **Step 2:** Run — FAIL. **Step 3:** Implement. **Step 4:** `cargo test -p mesh-host --all-features` green; `cargo ndk -t arm64-v8a build -p mesh-host`; commit `feat(mesh-host): Android hooks — periodic, power policy, wake`.

### Task 6: `meshd probe`

**Files:**
- Modify: `crates/mesh-admin/src/proto.rs` (`AdminRequest::Probe { node: String, count: u32, interval_ms: u32 }`, allowed only when `Running`), `crates/mesh-host/src/daemon/requests.rs`, `apps/meshd/src/{cli.rs, commands.rs, output.rs}`; Test: `apps/meshd/tests/cli.rs` or `crates/mesh-host/tests/daemon.rs`

**Interfaces:**
- `Probe` resolves `node` as a unique hex prefix of a member id (same rule as `remove`), then sends `count` `_ping` `ForwardRequest`s over `NodeHandle::connect(peer, ALPN_HTTPFWD)` (reusing the forwarder's frame write/read helpers), one every `interval_ms`, each with a 30 s timeout. Streams one line per sample through `RequestIo::send_lines`: `seq=<n> ms=<rtt>` or `seq=<n> error=<reason>`; ends with `Ok`.
- CLI: `meshd probe <node> [--count 20] [--interval 5s]`; prints each line, then `p50=<ms> p95=<ms> max=<ms> failed=<k>/<count>` (nearest-rank percentiles over successes). Exit code 0 even with failures; exit 2 if every sample failed.

- [ ] **Step 1:** Failing tests: `probe_reports_samples` (two daemons; `count 3`, `interval 100ms` → 3 `ms=` lines), `probe_unknown_node_errors`, unit `percentiles_nearest_rank` (in `output.rs`).
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** green; commit `feat(meshd): probe`.

### Task 7: `crates/mesh-ffi`

**Files:**
- Create: `crates/mesh-ffi/{Cargo.toml, src/lib.rs, src/host.rs, src/android.rs, uniffi.toml, tests/host.rs}`

**Interfaces:**
- Consumes: `mesh-host` (Task 5), `mesh-admin` `InProcessAdmin` (Task 2).
- `Cargo.toml`: `crate-type = ["lib"]` (the cdylib is the Tauri crate); deps `uniffi = "0.29"` (features `tokio`), `mesh-host`, `mesh-admin`, `postcard`, `tokio`; `[target.'cfg(target_os = "android")'.dependencies] jni = "0.21"`, `ndk-context = "0.1"`, `tracing-android = "0.2"`. `uniffi::setup_scaffolding!()`; `uniffi.toml`: `[bindings.kotlin] package_name = "net.meshnode.ffi"`, `cdylib_name = "meshnode_app"`.
- Exports (`#[uniffi::export]`, async where noted), all on one process-global `Host` (`OnceLock<Host { rt: tokio::runtime::Runtime (multi-thread, 2 workers), daemon: tokio::sync::Mutex<Option<Daemon>>, last_error: Mutex<Option<String>> }>`):
  - `async fn start(data_dir: String) -> Result<FfiStatus, FfiError>` — reads `meshd.toml` (`HostConfig::load_or_default`), starts with `HostOptions { admin_socket: false, periodic: Periodic::Driven, power: <sink if set> }`; idempotent and serialised by the mutex (a concurrent second call waits and returns the same status).
  - `async fn stop()`; `fn status() -> FfiStatus` (mirrors `HostStatus` + `last_error`); `async fn wake(reason: FfiWakeReason)`; `fn network_changed()`; `fn set_power(policy: FfiPowerPolicy, charging: bool, screen_on: bool)`; `fn set_wake_lock_sink(sink: Arc<dyn FfiWakeLockSink>)` (callback interface `acquire(tag: String, max_ms: u64)`, `release(tag: String)`); `fn set_service_control(ctl: Arc<dyn FfiServiceControl>)` (callback interface `start_service()`, `stop_service()` — used by Tauri commands, Task 8); `async fn admin(request: Vec<u8>) -> Result<Vec<u8>, FfiError>` (postcard `AdminRequest` → postcard `AdminResponse`; `NotRunning` when stopped); `async fn admin_deploy_static(...)`, `fn admin_logs(request: Vec<u8>, sink: Arc<dyn FfiLineSink>)`.
  - `FfiError` (`uniffi::Error`): `NotInitialised`, `AlreadyRunning { message }`, `Start { message }`, `NotRunning`, `Decode { message }`, `Panic { message }`.
  - Every export wraps its body in `catch_unwind` (async: `FutureExt::catch_unwind` on an `AssertUnwindSafe` future); a panic during `start` leaves the host stopped and sets `last_error`.
- `android.rs` (`#[cfg(target_os = "android")]`): raw JNI export `Java_net_meshnode_app_MeshNative_initAndroid(env: JNIEnv, _class: JClass, context: JObject)` → `ndk_context::initialize_android_context(vm_ptr, global_ref_ptr)` once (global ref leaked deliberately); installs the `tracing-android` layer (tag `meshnode`) alongside the file logger. `start` on Android returns `NotInitialised` if this has not run. (Ruling: UniFFI cannot pass an Android `Context`, so this one entry point is plain JNI.)

- [ ] **Step 1:** Failing host-side tests (no JNI) in `tests/host.rs` (each in a temp data dir; the global `Host` is shared, so tests run serially via a test mutex and `stop()` at the end): `start_is_idempotent`, `concurrent_start_is_single` (two `start` futures joined → one daemon, both `Ok`), `stop_then_start`, `stop_during_start_ends_stopped`, `admin_bytes_round_trip` (`Status` encoded → decoded `AdminResponse::Status`), `admin_when_stopped_is_not_running`, `panic_in_start_leaves_stopped` (test-only hook under `test-hooks` that panics inside start), `wake_lock_sink_receives_always_on` (set policy AlwaysOn → sink sees `acquire("always", ..)`).
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** `cargo test -p mesh-ffi --all-features` green; `cargo ndk -t arm64-v8a build -p mesh-ffi` green; generate Kotlin bindings once to prove they build: `cargo run -p mesh-ffi --features bindgen --bin uniffi-bindgen -- generate --library <target>/aarch64-linux-android/debug/libmesh_ffi.so --language kotlin --out-dir <tmp>` (add the tiny `uniffi-bindgen` bin behind feature `bindgen`). Commit `feat(mesh-ffi): UniFFI host surface`.

### Task 8: `apps/android` — Tauri project, Rust side, placeholder page, debug APK

**Files:**
- Create via `cargo install tauri-cli --version "^2" --locked` then `cargo tauri android init` inside `apps/android` (non-interactive config): `apps/android/{package.json?, src/index.html, src-tauri/{Cargo.toml, tauri.conf.json, build.rs, src/lib.rs, src/main.rs, capabilities/default.json}, src-tauri/gen/android/**}`
- Modify: root `Cargo.toml` (`exclude` += `"apps/android"`; members stay explicit from Task 1), `.gitignore` (`apps/android/src-tauri/gen/android/**/build/`, `.gradle/`, `local.properties`)

**Interfaces:**
- `src-tauri` is its **own workspace** (`[workspace]` table in its `Cargo.toml`), path-depends on `../../../crates/mesh-ffi`; lib name `meshnode_app`, `crate-type = ["staticlib", "cdylib", "rlib"]`; `lib.rs`: `mesh_ffi::uniffi_reexport_scaffolding!();` and the Tauri builder with commands `status() -> FfiStatus`, `join(invite: String, name: String) -> Result<(), String>` (postcard `AdminRequest::Join` through `mesh_ffi::admin`), `service_start()`, `service_stop()` (through the registered `FfiServiceControl`). No other Tauri plugins.
- `tauri.conf.json`: identifier `net.meshnode.app`, productName `Meshnode`, `frontendDist: "../src"`, no dev server.
- `src/index.html`: plain HTML + inline JS using `window.__TAURI__.core.invoke`; shows state, node id, mesh, peers online, services hosted (refresh every 5 s); an invite textarea + name field + Join button; Start / Stop buttons. `"withGlobalTauri": true`.
- Gradle (`gen/android`): `compileSdk 35`, `targetSdk 35`, `minSdk 29`, `buildToolsVersion "35.0.0"`, `ndkVersion "27.2.12479018"`; `abiFilters` arm64-v8a, x86_64; dependency `net.java.dev.jna:jna:5.14.0@aar`, `androidx.work:work-runtime-ktx`; UniFFI Kotlin bindings generated into `gen/android/app/src/main/java/net/meshnode/ffi/` by a Gradle task (or a committed generation script `apps/android/gen-bindings.sh`) from the built `.so`.
- Release profile for the `.so` in `src-tauri/Cargo.toml`: `[profile.release] strip = true, lto = "thin", codegen-units = 1, opt-level = "s"`.

- [ ] **Step 1:** Scaffold with the Tauri CLI; set the pins above; wire `mesh-ffi`; commit the generated project (minus build outputs).
- [ ] **Step 2:** Build: `cargo tauri android build --debug --target aarch64 --apk` (env: `JAVA_HOME=C:\Program Files\Microsoft\jdk-17.0.21.21-hotspot`, `ANDROID_HOME=%LOCALAPPDATA%\Android\Sdk`, `NDK_HOME=%ANDROID_HOME%\ndk\27.2.12479018`). Expected: an APK under `gen/android/app/build/outputs/apk/`. Record its size in the report. Do **not** install it on any device.
- [ ] **Step 3:** Commit `feat(android): Tauri 2 project with mesh-ffi and placeholder page`.

### Task 9: `apps/android` — Kotlin service, receivers, scheduling, onboarding

**Files (all under `apps/android/src-tauri/gen/android/app/src/`):**
- Create: `main/java/net/meshnode/app/{MeshNative.kt, MeshService.kt, RestartPolicy.kt, NotificationText.kt, BootReceiver.kt, RestartReceiver.kt, AlarmReceiver.kt, WatchdogWorker.kt, Alarms.kt, WakeLocks.kt, Prefs.kt, OnboardingActivity.kt}`; `test/java/net/meshnode/app/{RestartPolicyTest.kt, NotificationTextTest.kt}`
- Modify: `main/AndroidManifest.xml`, `main/java/net/meshnode/app/MainActivity.kt` (start `MeshService` on create unless `user_stopped`; route first launch to onboarding)

**Interfaces:**
- `MeshNative`: `external fun initAndroid(context: Context)`; `init { System.loadLibrary("meshnode_app") }`.
- `RestartPolicy` (pure): `enum class Trigger { BOOT, PACKAGE_REPLACED, TASK_REMOVED_ALARM, WATCHDOG, APP_OPEN, USER_START }`; `fun shouldStart(trigger: Trigger, userStopped: Boolean, running: Boolean): Boolean` = `!running && (trigger == USER_START || !userStopped)`; `USER_START` also clears `userStopped` (caller's job, via `Prefs`).
- `NotificationText` (pure): `fun running(services: Int, peers: Int): String` → `"Meshnode: hosting $services services · $peers peers online"`; `fun failed(reason: String): String` → `"Meshnode couldn't start: $reason — tap to open"`; `fun uninitialised(): String` → `"Meshnode: not set up — tap to open"`.
- `MeshService`: per spec §5.1 (foreground `specialUse`, `startForeground` within 5 s with the "starting" text, `START_STICKY`, Stop action → `stop()` + `Prefs.userStopped = true` + `stopSelf`, Open action → `MainActivity`; registers `FfiWakeLockSink` (`WakeLocks`: `PowerManager.PARTIAL_WAKE_LOCK` per tag, `acquire(max_ms)` uses `acquire(timeout)` when `max_ms != Long.MAX`), `FfiServiceControl`, `NetworkCallback` → `networkChanged()` + `wake(NETWORK)`, power/screen receivers → `setPower(...)`; refreshes the notification every 30 s while the screen is on and after each `wake`; `onTaskRemoved` → `Alarms.scheduleRestart(5 s)`).
- `Alarms`: `scheduleRestart(delayMs)` (`setExactAndAllowWhileIdle` if `canScheduleExactAlarms()`, else `setAndAllowWhileIdle`); `scheduleTick()` (`setAndAllowWhileIdle`, 15 min, re-armed by `AlarmReceiver`).
- `AlarmReceiver`: `goAsync()`, 10 s wake lock, `wake(ALARM)`, re-arm `scheduleTick()`. `WatchdogWorker`: `PeriodicWorkRequest` 15 min, unique name `meshnode-watchdog`, `KEEP`.
- `OnboardingActivity` (first launch; plain Android views): status lines + buttons for `POST_NOTIFICATIONS`, exact alarms (`ACTION_REQUEST_SCHEDULE_EXACT_ALARM`), battery exemption (`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`); text "Lock Meshnode in recents so swiping it away doesn't stop it" and a link to `https://dontkillmyapp.com`; Continue → `MainActivity`.
- Manifest: permissions from Global Constraints; `<service android:name=".MeshService" android:foregroundServiceType="specialUse" android:exported="false">` with `<property android:name="android.app.PROPERTY_SPECIAL_USE_FGS_SUBTYPE" android:value="Self-hosted peer-to-peer mesh node serving the owner's devices"/>`; receivers for `BOOT_COMPLETED`, `MY_PACKAGE_REPLACED` (exported, system broadcasts only), `RestartReceiver`, `AlarmReceiver` (not exported).

- [ ] **Step 1:** Write `RestartPolicyTest` (every trigger × userStopped × running) and `NotificationTextTest` (exact strings above).
- [ ] **Step 2:** Run `gradlew testDebugUnitTest` in `gen/android` — FAIL.
- [ ] **Step 3:** Implement all files.
- [ ] **Step 4:** `gradlew testDebugUnitTest` green; rebuild the APK (Task 8 Step 2 command) green; `gradlew lintDebug` has no errors. Commit `feat(android): foreground service, restart, alarms, onboarding`.

### Task 10: CI, manual check, back-ports, full verification

**Files:**
- Modify: `.github/workflows/ci.yml`, `docs/superpowers/specs/2026-11-08-meshnode-design.md`
- Create: `docs/manual-checks/sub-spec-5.md`

- [ ] **Step 1: CI** — android job: add `cargo ndk -t arm64-v8a build -p mesh-host -p mesh-ffi`; new job `android-apk` (ubuntu): JDK 17 (`actions/setup-java`), `android-actions/setup-android`, `sdkmanager "ndk;27.2.12479018" "build-tools;35.0.0" "platforms;android-35"`, `cargo install tauri-cli --version "^2" --locked`, `rustup target add aarch64-linux-android`, `cargo tauri android build --debug --target aarch64 --apk` in `apps/android`, then `./gradlew testDebugUnitTest` in `gen/android`. Linux/Windows test jobs unchanged.
- [ ] **Step 2: Manual check doc** — spec §8.3's nine steps with a results table (Wi-Fi / mobile-data p50 / p95 / max; reconnects; battery mAh; APK size), the rule "if sleeping p95 > 60 s, set the `periodic_wake_s` default from the data", install commands (`adb install -r <apk>`, run by the owner), and the carried owner gates from sub-specs 2–4.
- [ ] **Step 3: Back-ports** per spec §9, each marked "(back-ported from sub-spec 5)"; §14 row 5 → "implemented, manual check pending; see `docs/manual-checks/sub-spec-5.md`". Also correct §3's `apps/android` row (Tauri project's Rust crate is its own workspace and links `mesh-ffi`) and `mesh-ffi`'s deps (host, admin).
- [ ] **Step 4: Full verification** — `cargo test --workspace --all-features -j 4`; clippy with and without `--all-features` `-D warnings`; `cargo fmt --all --check`; `cargo ndk -t arm64-v8a build -p mesh-host -p mesh-ffi -p mesh-gateway -p mesh-admin`; APK build; `gradlew testDebugUnitTest`.
- [ ] **Step 5: Commit** `ci: Android APK job; docs: sub-spec 5 manual check and back-ports`.
