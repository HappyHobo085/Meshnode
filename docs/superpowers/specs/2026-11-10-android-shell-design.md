# Sub-spec 5: Android shell (`mesh-host`, `mesh-ffi`, `apps/android`)

> Master spec: `docs/superpowers/specs/2026-11-08-meshnode-design.md` (§10.2, §14 row 5).
> Spike: `spikes/android/RESULTS.md`. Previous sub-spec: `2026-11-10-gateway-admin-meshd-design.md`.

## 1. Intent

**What the owner asked for:** continue the master plan with sub-project 5, the Android shell: `mesh-ffi`, the
foreground service, boot/watchdog, and the power policy (master §14 row 5). Decisions taken in brainstorming:

- `apps/android` is a **Tauri 2 Android project now**, with a placeholder page; sub-spec 6 fills in the real UI.
- The still-open spike measurement (how fast a sleeping phone answers) is **built in and measured in the manual
  check**: a configurable periodic wake lock (default off) plus a latency probe; the result sets the default.
- **Approach A:** move `meshd`'s `Daemon` into a shared crate `mesh-host`; `mesh-ffi` wraps it; the Tauri app's
  Rust `cdylib` links `mesh-ffi`, so the Kotlin service and the webview share one `.so` and one node.

**Assumptions (designer, not owner):** an Android 17 phone is the manual-check device; the
phone joins a mesh founded on desktop (init on the phone is possible through the admin API but not exposed in
the placeholder page); debug signing only.

### 1.1 Done criteria
1. All tests in §8 pass on Windows; Linux CI is green, including the APK build job.
2. After the move to `mesh-host`, every `meshd` test that existed at the start passes with only import-path
   changes; `meshd` behaviour is unchanged.
3. The owner's manual check (§8.3) on the phone is run and its results table filled in. The probe numbers decide
   the `periodic_wake_s` default (§5.3).
4. The master spec is back-ported (§9).

## 2. Crates and the code split

| Crate | Role | Depends on |
|---|---|---|
| `crates/mesh-host` *(new)* | Everything `apps/meshd`'s `Daemon` does today except transports and OS services: `Daemon` start / run / shutdown, init / join / reset-identity, config (`MeshdConfig` → `HostConfig`; the file stays `meshd.toml`), the `meshd.lock` data-dir lock, logging setup, admin request handling (`AdminBackend` impl), `wipe_identity`, `TestOptions`. Platform-neutral; builds for `aarch64-linux-android`. | core, services, gateway, admin, proto |
| `crates/mesh-admin` *(changed)* | Adds the **in-process transport** (§4). The socket / named-pipe server and client are unchanged. | core, proto |
| `apps/meshd` *(slimmed)* | Binary + thin lib: CLI, socket/pipe `AdminServer` wiring, OS services, Event Log, `probe` command. | host, admin |
| `crates/mesh-ffi` *(new)* | UniFFI surface for Kotlin (§3). Global `Host` = `OnceLock<{ tokio Runtime, Mutex<Option<Daemon>> }>`. | host, admin |
| `apps/android` *(new)* | Tauri 2 Android project. `apps/android/src-tauri` is the single `cdylib`; it depends on `mesh-ffi` and re-exports its UniFFI scaffolding, and registers the Tauri commands of §6.2. Kotlin: `MeshService`, receivers, workers, onboarding (§5). | ffi, `tauri` 2 |

**Refactor rule:** a move, not a rewrite. The daemon tests (`apps/meshd/tests/daemon.rs`) move to
`crates/mesh-host/tests/`; `cli.rs`, `service.rs`, `e2e.rs` stay in `meshd`. Public names keep their meaning;
`meshd` re-exports what its CLI needs. One commit does the pure move, so its diff reviews as a rename.

### 2.1 `HostOptions` (new, passed to `Daemon::start_with`)
Defaults reproduce desktop behaviour exactly.

| Field | Desktop default | Android | Meaning |
|---|---|---|---|
| `periodic` | `Timers` | `Driven` | `Driven`: periodic catch-up and address refresh also run on `Daemon::wake()` and on incoming traffic after ≥ 60 s of silence (§5.2). Tokio timers keep running in both modes. |
| ~~`wasm_reservation`~~ | — | — | Not a field: replaced by `RuntimeConfig::compact_memory` (on for Android, `cfg!(target_os = "android")`), which gives the smaller wasmtime static memory reservation / guard (sub-spec 3 carry-over I5). *(as implemented)* |
| `power` | `None` | `Some(PowerHooks)` | Wake-lock callbacks and screen/charging state (§5.3). |

## 3. `mesh-ffi` surface (UniFFI; async → Kotlin `suspend`)

| Function | Behaviour |
|---|---|
| `init_android(context)` | A raw JNI export (UniFFI cannot pass a `Context`). Stores a global ref; calls `ndk_context::initialize_android_context(vm, context)`. Idempotent; the workspace's patched `ndk-context` (`patches/ndk-context`) keeps the first context, so it is safe next to Tauri's `tao` in either start order. There is no `releaseAndroidContext`. Must run before `start` (spike requirement 1); `start` returns `NotInitialised` otherwise on Android. |
| `start(data_dir, opts) -> Status` | Idempotent: if running, returns the current status. Starts `Daemon` with `HostOptions` (Android column). |
| `stop()` | Graceful shutdown within the 20 s limit; releases `meshd.lock`. No-op if stopped. |
| `status() -> Status` | `{ state: Stopped/Uninitialised/Running/Removed, node_id, mesh_name, services_hosted, peers_online, gateway_ports, last_error }`. |
| `wake(reason) -> ()` | `reason ∈ {Alarm, Network, Boot, Incoming}`. Runs catch-up + address refresh now; returns when done or after 10 s. |
| `network_changed()` | Calls iroh `Endpoint::network_change()`. |
| `set_power_state(charging, screen_on)` | Reports charging and screen state (§5.3); never changes the policy. |
| `set_power_policy(policy)` | Changes the policy of the running node and saves it to `meshd.toml`; `NotRunning` while stopped. |
| `set_wake_lock_sink(sink)` | Kotlin callback interface `{ acquire(tag, max_ms), release(tag) }`. |
| `admin(request: bytes) -> bytes` | postcard `AdminRequest` → `AdminResponse` over the in-process transport. |
| `admin_deploy_static(...)`, `admin_logs(...)` | Streaming variants for `DeployStatic` and `LogsTail` (callback interface for log lines). |

Panics inside any export are caught (UniFFI → Kotlin exception), logged, and leave the node in a defined state
(stopped if it panicked during `start`). Logging: `tracing` to `<data>/logs` (as on desktop) and to logcat via a
`tracing-android` layer.

## 4. In-process admin transport (`mesh-admin`)

`InProcessAdmin::new(backend: Arc<dyn AdminBackend>)` gives `request(AdminRequest) -> AdminResponse`,
`deploy_static(...)` and `logs_tail(...)` with **the same semantics as `AdminClient`** over a socket: the same
state gating (`DaemonState`), the same upload validation (file count, sizes, paths), the same errors. The shared
request-dispatch logic in `server.rs` is factored so both transports call one function; `RequestIo` gains an
in-memory implementation. Tests mirror the socket tests (§8.1).

## 5. Android app behaviour

### 5.1 `MeshService` (foreground service, type `specialUse`)
- `onCreate`: `init_android`, post the notification "Meshnode: hosting N services · M peers online" with **Stop**
  and **Open**, then `start()` on a coroutine. On a start error the notification reads "Meshnode couldn't start:
  <reason> — tap to open" and the service stays in the foreground (no crash loop).
- `onStartCommand`: `START_STICKY`; calls `start()` (idempotent).
- Notification refresh: every 30 s while the screen is on, and after each `wake()`.
- **Stop:** `stop()`, `stopForeground`, `stopSelf`, persist `user_stopped = true`. While set, boot / alarm /
  watchdog / package-replaced do **not** start the service. Opening the app and pressing Start clears it.
- `onTaskRemoved`: schedule the restart alarm (§5.2).
- `ConnectivityManager.NetworkCallback` → `network_changed()` and `wake(Network)`.
- `ACTION_POWER_CONNECTED/DISCONNECTED`, `ACTION_SCREEN_ON/OFF` → `set_power_state(...)`.
- Data dir: `context.filesDir/meshnode` (app-private).

### 5.2 Starting, restarting and wall-clock work

| Trigger | Mechanism | Action (unless `user_stopped`) |
|---|---|---|
| Boot | `BOOT_COMPLETED` receiver | start service |
| App updated | `MY_PACKAGE_REPLACED` receiver | start service |
| Swiped from recents | `onTaskRemoved` → `setExactAndAllowWhileIdle` ≈ 5 s → `RestartReceiver` | start service; without exact-alarm permission, an inexact alarm (the watchdog backstops) |
| Killed by the system | WorkManager `WatchdogWorker`, periodic 15 min | start service if not running |
| Wall-clock tick | `AlarmManager.setAndAllowWhileIdle`, inexact, 15 min, re-armed each time | `wake(Alarm)` under a 10 s wake lock (plus `periodic_wake_s`, §5.3) |

In Rust (`periodic: Driven`): an incoming peer connection or `sync/1` message after ≥ 60 s with no peer
traffic triggers a background catch-up (spike requirement 6).

The restart decision (`trigger × user_stopped × running → start / ignore`) is a pure Kotlin function.

Android refuses a foreground-service start from the background except for boot, package-replaced and an exact alarm; the inexact alarm and the WorkManager watchdog are refused without the battery exemption. A refused start posts the notification "Meshnode stopped in the background — tap to restart" instead of failing silently.

### 5.3 Power policy
Stored in `meshd.toml` (the single source of truth; Rust saves a change) `[android] power = "balanced" | "always_on" | "saver"` (default `balanced`) and
`[android] periodic_wake_s = 0` (default off). Rust decides; Kotlin only holds/releases locks it is told to.

| Policy | Wake lock | Serving |
|---|---|---|
| Balanced | While charging; while serving an incoming request (≤ request + 30 s); during a catch-up (≤ 60 s). If `periodic_wake_s > 0`, also for that long on each 15-min alarm. | normal |
| Always on | Held for the service's lifetime. | normal |
| Saver | Never (only the alarm receiver's 10 s). | Screen off: the gateway serves only the phone's own apps; inbound `httpfwd` from peers answers `503 Service not yet replicated` (the forwarder tries other hosts); no piggybacked catch-up, only the 15-min alarm. Screen on: normal. |

The policy decision is a pure function `(policy, charging, screen_on, activity) -> Option<WakeLock{tag, max}>`,
table-tested.

### 5.4 Onboarding (Kotlin, first launch, before the Tauri page)
Requests, each shown as granted / missing on a status line, none blocking start: `POST_NOTIFICATIONS`
(Android 13+), exact alarms, battery-optimisation exemption; then text: "lock Meshnode in recents" and a link to
per-manufacturer guidance (dontkillmyapp.com).

### 5.5 Latency probe
- **Reserved `httpfwd` service `_ping`:** the `httpfwd` handler answers `ForwardRequest { service: "_ping" }`
  with `204` before service lookup (`_` is invalid in service names, so no collision). Gated like all `httpfwd`.
- **`meshd probe <node-id-prefix> [--count 20] [--interval 5s]`:** a new admin request `Probe { node, count,
  interval_ms }`; the daemon sends `_ping` requests directly to that member and streams one line per sample;
  the CLI prints p50 / p95 / max and failures.

## 6. Build and the placeholder UI

### 6.1 Toolchain and build
- `apps/android` created with `tauri android init`; NDK 27.2, compileSdk 36, build-tools 36.0.0 (required by tauri-android 2.11+ and AGP 9.3.1), minSdk 29, targetSdk 35,
  ABIs arm64-v8a (+ x86_64 for the emulator).
- Android release profile for the `.so`: `strip = true`, `lto = "thin"`, `codegen-units = 1`,
  `opt-level = "s"`. The APK size is measured and recorded in the manual check (no target).
- `cargo tauri android build` cannot create the `jniLibs` symlink on a FAT32 checkout, so `apps/android/build-apk.sh` wraps it and falls back to copying; Linux CI calls the CLI directly. The Tauri CLI is pinned (tauri-cli 2.12.4). `apps/android/gen-bindings.sh` regenerates the committed Kotlin bindings; CI regenerates them and fails on a diff. Gradle variants: `testUniversalDebugUnitTest`, `lintUniversalDebug`.
- Debug signing for installs; the release APK is built unsigned for the size measurement. Release signing is sub-spec 8.

### 6.2 Placeholder page
Static HTML + two Tauri commands backed by `mesh-ffi`: `status` (state, node id, mesh, peers, services) and
`join(invite)` (`admin(Join)`). Plus Start/Stop buttons (clear / set `user_stopped` through the service).

## 7. Errors

| Situation | Behaviour |
|---|---|
| `start` fails (lock held, store error, bad config) | Notification shows the reason; service stays foreground; watchdog retries every 15 min. |
| Panic in an FFI call | Caught → Kotlin exception; logged; node stopped if mid-start. |
| Permission missing | Status line in onboarding / placeholder page; never a crash. |
| `wake` exceeds 10 s | Returns; work continues in the background. |
| `start` before `init_android` on Android | `NotInitialised` error. |

## 8. Testing

### 8.1 Rust (desktop CI)
- `mesh-host`: all moved daemon tests; new: `wake_triggers_catch_up`, `incoming_after_silence_triggers_catch_up`
  (paused tokio clock), `saver_screen_off_refuses_httpfwd_503`, `power_decision_table`.
- `mesh-admin`: `in_process_*` tests mirroring the socket tests (state gating, upload limits, logs tail).
- `mesh-ffi`: host-side tests (no JNI): `start_is_idempotent`, `stop_then_start`, `admin_bytes_round_trip`,
  `panic_in_start_leaves_stopped`.
- `mesh-gateway`: `ping_service_answers_204`, gated for non-members.
- `meshd`: `probe_reports_samples` (two daemons).

Probe output includes `connect=` (the fresh dial, reported separately from the exchange time) and the CLI takes `--timeout` (1 s to 2 min, default 30 s). Saver mode answers 503 on the phone, which the desktop sees as 502.

### 8.2 Kotlin (JVM unit tests, no device)
Restart decision table; notification text formatting.

### 8.3 Manual check (`docs/manual-checks/sub-spec-5.md`, owner, phone)
1. Install the debug APK; onboarding grants.
2. Join an existing desktop mesh via the invite box.
3. Desktop deploys a static site; the phone serves it at `http://<site>.mesh.localhost:7777` (phone browser).
4. Reboot → reconnects (time recorded).
5. Swipe away → restarts within ~10 s.
6. Stop → stays stopped through a reboot; Start clears it.
7. Screen off ≥ 10 min: `meshd probe <phone>` on Wi-Fi and on mobile data; record p50 / p95 / max.
8. Overnight unplugged on mobile data: reconnect count, battery (batterystats).
9. APK size.

If p95 on a sleeping phone exceeds 60 s (success criterion 7), set the `periodic_wake_s` default from the data.

The manual-check document also carries the checks that the reviews found no way to automate: boot, swipe with and without the exact-alarm grant, the watchdog without the battery exemption (a refused background restart posts a notification), Stop then Start in quick succession, the always-on lock released after Stop, the page under the CSP, onboarding grants, cold start to `startForeground`, and the service-first / app-first `ndk-context` order.

### 8.4 CI
`android` job: `cargo ndk` builds of the Android-capable crates including `mesh-host` and `mesh-ffi`, and clippy (`-D warnings`) of `apps/android/src-tauri` for the Android target. `android-apk` job (ubuntu): JDK 17, Android SDK (NDK 27.2.12479018, build-tools 36.0.0, platforms 35 and 36), pinned tauri-cli, the bindings drift check, the **release** APK (stripped, unsigned) through `build-apk.sh --release`, then `testUniversalDebugUnitTest` and `lintUniversalDebug`. No emulator in CI.

## 9. Back-ports to the master spec
§3 crate table (`mesh-host`; `mesh-ffi` depends on host + admin; `apps/meshd` depends on host), §3 diagram note
(in-process admin transport exists), §9 (`meshd probe`, `_ping`), §10.2 (wall-clock alarm, `periodic_wake_s`,
`user_stopped`, Saver's 503, onboarding), §14 row 5 status. Each marked "(back-ported from sub-spec 5)".

## 10. Out of scope
The real management UI (sub-spec 6); init from the phone's UI; release signing / Play Store (sub-spec 8);
remote admin of the phone from a desktop CLI; iOS; emulator tests in CI.
