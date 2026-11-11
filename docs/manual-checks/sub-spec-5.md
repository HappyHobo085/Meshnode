# Manual check: sub-spec 5 (Android shell)

Run by the owner on the Galaxy S23 (Android 17) with a desktop mesh (sub-spec 5 §8.3). Until this is done, master spec §14 row 5 stays "implemented, manual check pending". Nothing in this document has been run on a device yet: the build agent never installs on any device.

## Build and install

Build the debug APK on the PC (JDK 17, `ANDROID_HOME`, `NDK_HOME` = NDK 27.2.12479018, compileSdk 36 / build-tools 36.0.0, and the Tauri CLI: `cargo install tauri-cli --version "=2.12.4" --locked`):

```
apps/android/build-apk.sh
```

Output: `apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk` (arm64, debug-signed, unstripped, large). `build-apk.sh` wraps `cargo tauri android build --debug --target aarch64 --apk`; it exists only because the CLI cannot make symlinks on a FAT32 checkout. In Linux CI the CLI is called directly.

The owner installs it, with the phone connected and USB debugging on (not run by the agent):

```
adb install -r apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
```

For the size measurement, `apps/android/build-apk.sh --release` builds a stripped, **unsigned** release APK (`.../apk/universal/release/app-universal-release-unsigned.apk`). It cannot be installed until signed (release signing is sub-spec 8). The CI job `android-apk` builds the same and uploads it as an artifact.

Probe, from a desktop mesh member: `meshd probe <phone-node-id-prefix> --count 20 --interval 5s --timeout 90s`. Each sample line shows `connect=` (the fresh dial) and the exchange time; the summary has p50 / p95 / max and failures. `--timeout` accepts 1 s to 2 min (default 30 s), so the 60 s criterion can be observed. In Saver mode with the screen off the phone answers 503, which the desktop reports as 502 (the forwarder finds no other host).

Logs: `adb logcat` (tracing goes to logcat as well as `<data>/logs`).

## Steps (spec §8.3)

1. Install the debug APK. The onboarding screen shows POST_NOTIFICATIONS, exact alarms and battery-optimisation exemption, each as granted or missing. Grant all three.
2. Join an existing desktop mesh via the invite box on the page (`meshd invite` on the desktop, paste the text).
3. On the desktop, `meshd deploy static site <dir>`. In the phone's browser open `http://site.mesh.localhost:7777`; the site loads.
4. Reboot the phone and unlock once. The service starts and the phone shows online in `meshd members`. Record the time from boot to online.
5. Swipe the app away from recents. The service is back within about 10 s.
6. Press Stop in the notification. The notification disappears; reboot; the service stays stopped. Open the app and press Start; it runs again.
7. Screen off for 10 minutes or more, unplugged: run `meshd probe <phone>` from a desktop with the phone on Wi-Fi, then again on mobile data. Record p50 / p95 / max.
8. Overnight, unplugged, on mobile data: record the reconnect count and the battery use (`adb shell dumpsys batterystats`, mAh).
9. APK size: see Build and install; record both numbers below.

## Carried checks (from the task reviews)

None of these has automated coverage. Mark each in the results table.

- **C1 Boot.** Reboot and unlock: the service starts by itself (`BOOT_COMPLETED`) and the notification shows, without opening the app. A foreground-service start from boot must not be refused.
- **C2 Swipe with the exact-alarm grant.** With "Alarms & reminders" granted, swipe the app away: the service returns within about 10 s.
- **C3 Swipe without the exact-alarm grant.** Revoke that grant, then swipe away. Record whether the service returns and when. (Without exact alarms the restart uses an inexact alarm; the 15-minute watchdog is the backstop.)
- **C4 Watchdog without the battery exemption.** Revoke the battery-optimisation exemption and the exact-alarm grant, stop the service process without using Stop (swipe it away), and wait up to 15 minutes. Android refuses a background foreground-service start for the inexact alarm and the WorkManager watchdog without the exemption; expect the notification "Meshnode stopped in the background — tap to restart" to appear. Tapping it must start the service.
- **C5 Stop then Start quickly.** Press Stop (notification), then Start (app) within a second or two, repeatedly. The node ends up running; no crash; no "lock held" error in the notification.
- **C6 Always-on lock released after Stop.** Set the policy to Always on (edit `[android] power = "always_on"` in `meshd.toml` until sub-spec 6 adds the setting, then restart). `adb shell dumpsys power | grep -i meshnode` shows a held partial wake lock. Press Stop; the lock is gone.
- **C7 Page under the CSP.** The placeholder page renders (styles and script load from `app.css` and `app.js`) and `invoke` works: status, join, Start and Stop return data, and no CSP violation shows (check `chrome://inspect` console).
- **C8 Onboarding grants.** Each grant button opens the right system screen; the status line updates on return; a denied grant never blocks Start and never crashes the app.
- **C9 Cold start to startForeground.** From a cold start (after reboot or process kill), record the time from `MeshService.onCreate` to the `startForeground` call in logcat. It must be well under Android's 10 s limit.
- **C10 ndk-context.** Reboot and do not open the app; wait until the notification shows and the node runs (the service initialised the Android context first). Then open the app: it must not crash. Repeat the other way round: force-stop, open the app first, then let the service start.

## `periodic_wake_s` default rule

The default is off (`[android] periodic_wake_s = 0` in `meshd.toml`). If in step 7 the p95 on a sleeping phone exceeds 60 s (success criterion 7), find the smallest `periodic_wake_s` that brings p95 under 60 s (edit `meshd.toml` in the app's data directory, restart, re-run the probe), and make that value the default in `mesh-host`'s config. If p95 is 60 s or less, leave the default at 0.

## Results

| Date | Device / Android | Step or check | Pass/fail | Notes |
|---|---|---|---|---|
| | S23 / 17 | 1 install, onboarding | | |
| | | 2 join | | |
| | | 3 static site | | |
| | | 4 reboot reconnect | | seconds |
| | | 5 swipe restart | | seconds |
| | | 6 stop / start / reboot | | |
| | | C1 to C10 | | one row each |

### Latency probe (step 7, screen off 10 min or more)

| Network | Samples | p50 | p95 | max | Failures | Typical connect= |
|---|---|---|---|---|---|---|
| Wi-Fi | | | | | | |
| Mobile data | | | | | | |

### Overnight (step 8) and size (step 9)

| Item | Value |
|---|---|
| Reconnects overnight | |
| Battery (mAh, hours unplugged) | |
| `periodic_wake_s` default decided | |
| Release APK size (stripped, unsigned, `app-universal-release-unsigned.apk`, arm64) | 35,482,467 bytes (33.8 MiB; measured 2026-11-11 with `apps/android/build-apk.sh --release`; `libmeshnode_app.so` inside is 32.5 MB) |
| Debug APK size (unstripped, arm64) | 802 MiB (measured in Task 8) |

## Carried owner gates from sub-specs 2 to 4

Still pending and unchanged by this sub-spec. Run them from their own documents and record the outcome there.

- Sub-spec 2: `docs/manual-checks/sub-spec-2.md`, steps 1-5, two desktops on different networks.
- Sub-spec 3: `docs/manual-checks/sub-spec-3.md`, steps 1-5, two desktops (`counter` fixture, static site, placement, undeploy and purge).
- Sub-spec 4: `docs/manual-checks/sub-spec-4.md`, steps 1-6 on Windows (elevated; installs a real OS service); `cargo test -p meshd --test service -- --ignored` elevated on a disposable VM; Linux (systemd) in a VM; macOS untested.

Steps 2-3 above need a working desktop mesh, so do the sub-spec 4 steps first.

## Automated coverage (for reference)

Rust: `mesh-host` (moved daemon tests, wake and catch-up, saver 503, power decision table), `mesh-admin` in-process tests, `mesh-ffi` host-side tests, `mesh-gateway` `_ping`, `meshd` probe. Kotlin: JVM unit tests (restart decision table, notification text). CI: `cargo ndk` builds, clippy on the Android target, the bindings drift check, the release APK build, `testUniversalDebugUnitTest` and `lintUniversalDebug`. No emulator and no device in CI.
