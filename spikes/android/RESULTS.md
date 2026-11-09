# Android feasibility spike — results

> **THROWAWAY.** Code in `spikes/android/` answers spec §14 step 0 and won't be reused as-is.
> Device: Samsung Galaxy S23 (SM-S911B), Android 17 / API 37, One UI 9, arm64-v8a.
> Desktop peer: Windows 11, same home network unless noted.
> Stack: wasmtime 50.0.1 (+ wasmtime-wasi-http p2), iroh 1.4.1 (`presets::N0`), NDK 27.2, APK targetSdk 35.

## Q1 — Does wasmtime run a `wasi:http` component on the phone?  **YES, with native Cranelift compilation, in the app sandbox**

| Context | Mode | Compile | Deserialize (in-memory) | Request |
|---|---|---|---|---|
| Desktop (x86_64) | Cranelift | 68 ms | 0.5 ms | ~0.4 ms median |
| Desktop | Pulley | 70 ms | 0.4 ms | ~5 ms median |
| Phone, `adb shell` | Cranelift | 150 ms | 1.2 ms | 2.7 ms median |
| Phone, `adb shell` | Pulley | 163 ms | 0.6 ms | 20 ms median |
| **Phone, APK app sandbox** (`untrusted_app`) | **Cranelift** | 137–174 ms | **1.0–1.3 ms** | **1.7–8 ms** |
| Phone, APK app sandbox | Pulley | 145 ms | 7.8 ms | 10 ms median |

- The guest is a stock `wstd` 0.6 `#[http_server]` component built for `wasm32-wasip2`. The host is the `ProxyPre` + `call_handle` pattern from wasmtime-wasi-http.
- `Component::serialize` + `unsafe Component::deserialize(&bytes)` (anonymous memory, never mmap of an app-data file) works. **The spec's caching plan (§7) holds as written.**
- No SELinux denial for JIT memory. Pulley isn't needed, but it's a working fallback at about 4–5× the per-request cost.
- During screen-off / forced Doze, WASM requests on the background service kept answering in 3–11 ms.

## Q2 — Can iroh connect phone ↔ desktop, including over mobile data?  **YES, with a direct path in both cases**

| Phone network | Dial by EndpointId only | Initial path | Upgraded to | Steady RTT |
|---|---|---|---|---|
| Same Wi-Fi | 0.68 s | relay (~400 ms) | **direct IP** within 1 s | 3–12 ms |
| Mobile data (Wi-Fi off) | 1.0 s | relay (~460 ms) | **direct IP** (holepunched through carrier NAT) within 4 s | ~75 ms |

- Discovery by ID alone (n0 DNS/pkarr) worked; no addresses were exchanged by hand.
- The relay path stays open as a standby next to the direct path.

## Q3 — Does a `specialUse` foreground service survive with the screen off?  **YES in forced deep Doze; the overnight unplugged run is still pending**

Forced Doze over adb (`dumpsys battery unplug`, screen off, `deviceidle force-idle`), with 30 s pings plus one WASM request per tick:

| Test | Battery-opt exemption | Result |
|---|---|---|
| A (~2 min) | granted | 5/5 pings OK, direct path kept, no late ticks |
| B (~2 min) | **removed** | 4/4 pings OK, direct path kept, no late ticks |

- **A foreground service keeps network access in Doze even without the exemption.** The exemption is still worth requesting as insurance against OEM app sleeping (Samsung "sleeping apps").
- `START_STICKY` / repeated `onStartCommand` are handled (`start()` is idempotent).

### Real-world checks (owner, 2026-11-08/09, no wake lock, battery exemption granted)

| Check | Result |
|---|---|
| **Reboot** | ✅ `BOOT_COMPLETED` started the service; it reconnected to the desktop 4 s later. |
| **Swipe away from recents** | ❌ **The node stopped.** `onTaskRemoved` was logged, then nothing more. The desktop saw the connection time out 46 s later, and `START_STICKY` hadn't restarted it before the reboot about 2 min later. One UI kills (or freezes) the process on swipe even with a foreground service and the exemption. |
| **Overnight, unplugged, screen off, 10 h 12 m** | ✅ The service stayed alive through 5 deep-Doze cycles. **One** reconnect in 10 h (relay → direct again within seconds). Every WASM tick succeeded (4–50 ms). |
| **Battery** | ✅ **The spike app used 5.0 mAh in 10.2 h** (batterystats, 3900 mAh battery) ≈ **0.13 % → ~0.3 % per 24 h**, far under the ≤ 5 % target. The whole phone used 559 mAh (14 %), mostly the system. The CPU was awake only 15.5 % of the time. |

**Surprise: timers stretch while the CPU is suspended.** The 300 s ping timer (tokio, monotonic clock) fired every **18–43 min of wall time**, because the monotonic clock doesn't advance while the CPU is suspended. The app's own timers effectively pause in deep sleep. All periodic work (catch-up every 60 s, address refresh, keepalives) will run far less often on a sleeping phone unless it's scheduled on a wall-clock wakeup.

## Q4 — Keepalive / NAT binding  **answered for Wi-Fi**

- Direct paths stayed up with 30 s application pings on both Wi-Fi and mobile data (for 90 s on mobile data).
- **Overnight on Wi-Fi, the connection survived gaps of 18–43 min with no app traffic and the CPU suspended**, with just one QUIC idle timeout in 10 h. The desktop didn't time out the phone, so the phone must still have been answering the desktop's keepalives. That points to incoming packets waking the phone ("wakeupap=u0a392" entries in batterystats). It's an inference, not yet measured directly.
- **Not yet measured:** (a) the same on mobile data overnight, and (b) **the latency of an incoming request to a sleeping phone**: the desktop sends, and we time how long the phone takes to answer. That decides whether success criterion 7 ("applies a desktop write within 60 s") and serving client-role devices through a sleeping phone are realistic without a wake lock.

## Requirements found (feed into the sub-specs)

1. **`mesh-ffi` must call `ndk_context::initialize_android_context(vm, context)` at load time.** iroh's DNS resolver (`n0-dns-resolver`) and network monitor (`netwatch` → `netdev`) use it. Without it, a tokio worker panics with `android context was not initialized`. The spike does this in its JNI `start()` with a leaked global ref to the service `Context`.
2. The Rust library is ~51 MB unstripped (wasmtime + Cranelift + iroh). It needs `strip` + LTO in sub-project 5, and the APK size should be measured then.
3. Starting the service from adb needs the activity (the service isn't exported). Irrelevant in production.
4. Gradle/AGP 8.7.3 pulled build-tools 34 automatically. Pin the build-tools version in the real project.
5. **Recovering from swipe-away:** `START_STICKY` isn't enough on One UI. Proposed: in `onTaskRemoved`, schedule an exact alarm (`setExactAndAllowWhileIdle`, a few seconds out) whose receiver restarts the service. Exact alarms are an allowed way to start a foreground service from the background. Keep the 15-min WorkManager watchdog as a backstop. Tell the user in onboarding to lock the app in recents. Verify on the S23 in sub-project 5.
6. **Periodic work must not rely on in-process monotonic timers alone on Android.** Catch-up and address refresh need a wall-clock trigger: an `AlarmManager` inexact/while-idle alarm, or doing the work whenever the network or a peer wakes the process. Design for "the phone's timers run rarely, but it answers incoming traffic".

## Verdict

The spec's Android approach (§10.2: one process, a Rust core via JNI, a `specialUse` foreground service, an in-memory wasmtime cache) is **feasible on current Android, and cheap on battery** (~0.3 %/day with no wake lock). Changes it needs:
- the `ndk-context` setup (requirement 1)
- recovery from swipe-away (requirement 5)
- wall-clock scheduling for periodic work (requirement 6)

**Still open:** how quickly a sleeping phone answers incoming requests, and overnight behaviour on mobile data. Neither blocks sub-specs 1–4, which are platform-independent. Both can be measured in a second overnight run, or early in sub-project 5.
