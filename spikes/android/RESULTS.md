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
- **Pending, needs the owner (physical device handling):**
  1. Overnight run unplugged, screen off (started 2026-11-08 ~22:46, 300 s pings, batterystats reset at start).
  2. Swiping the app away from recents → the service should keep running (`onTaskRemoved` is logged).
  3. Reboot → `BOOT_COMPLETED` should start the service after the first unlock.

## Q4 — Keepalive / NAT binding  **partly answered**

- Direct paths stayed up with 30 s application pings on both Wi-Fi and mobile data (for 90 s on mobile data).
- The overnight run at a 300 s interval answers whether connections survive long idle gaps, and how often a reconnect happens. See the log once it's done.

## Requirements found (feed into the sub-specs)

1. **`mesh-ffi` must call `ndk_context::initialize_android_context(vm, context)` at load time.** iroh's DNS resolver (`n0-dns-resolver`) and network monitor (`netwatch` → `netdev`) use it. Without it, a tokio worker panics with `android context was not initialized`. The spike does this in its JNI `start()` with a leaked global ref to the service `Context`.
2. The Rust library is ~51 MB unstripped (wasmtime + Cranelift + iroh). It needs `strip` + LTO in sub-project 5, and the APK size should be measured then.
3. Starting the service from adb needs the activity (the service isn't exported). Irrelevant in production.
4. Gradle/AGP 8.7.3 pulled build-tools 34 automatically. Pin the build-tools version in the real project.

## Verdict so far

The spec's Android approach (§10.2: one process, a Rust core via JNI, a `specialUse` foreground service, an in-memory wasmtime cache) is **feasible on current Android**. No design changes are needed. The only new requirement is the `ndk-context` setup. The remaining risk is long-duration behaviour on battery (Q3 pending items, Q4, battery drain). It doesn't block starting sub-specs 1–4, which are platform-independent.
