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

## Editing meshd.toml on the phone

Needed for the policy (C6) and `periodic_wake_s`. It works because the debug APK is debuggable (`run-as`). Edit only while the node is stopped: a running node persists the policy from memory (it would overwrite your edit) and reads `periodic_wake_s` only at start. The `[android]` section is `deny_unknown_fields`: a typo makes the next start fail with a config error (shown in the notification).

1. Press Stop in the notification.
2. `adb exec-out run-as net.meshnode.app cat files/meshnode/meshd.toml > meshd.toml`
3. Edit `meshd.toml` locally, e.g. `[android]` with `power = "always_on"` (or `"balanced"`, `"saver"`) and `periodic_wake_s = 60`.
4. `adb push meshd.toml /data/local/tmp/meshd.toml`
5. `adb shell run-as net.meshnode.app cp /data/local/tmp/meshd.toml files/meshnode/meshd.toml`
6. `adb shell rm /data/local/tmp/meshd.toml`
7. Open the app and press Start.

## Steps (spec §8.3)

1. Install the debug APK. The onboarding screen shows POST_NOTIFICATIONS, exact alarms and battery-optimisation exemption, each as granted or missing. Grant all three.
2. Join an existing desktop mesh via the invite box on the page (`meshd invite` on the desktop, paste the text).
3. On the desktop, `meshd deploy static site <dir>`. In the phone's browser open `http://site.mesh.localhost:7777`; the site loads.
4. Reboot the phone and unlock once. The service starts and the phone shows online in `meshd members`. Commands: `adb logcat -d | grep -iE "meshnode|MeshService"` on the PC, and `meshd members` on a desktop. Pass: the phone shows online in `meshd members` within 60 s of the unlock. Record the seconds.
5. Swipe the app away from recents. Commands: `adb shell pidof net.meshnode.app` (prints a pid when the process is up) and `adb shell dumpsys activity services net.meshnode.app` (lists `MeshService` as running, foreground). Pass: the service is running again within 10 s (with the exact-alarm grant).
6. Press Stop in the notification. The notification disappears; reboot; the service stays stopped. Open the app and press Start; it runs again.
7. Screen off for 10 minutes or more, unplugged: run `meshd probe <phone>` from a desktop with the phone on Wi-Fi, then again on mobile data. Record p50 / p95 / max. Pass: p95 at most 60 s, otherwise apply the `periodic_wake_s` rule below. Optional lab shortcut (not a substitute for the real 10 minutes): after the screen turns off, `adb shell dumpsys deviceidle force-idle`, confirm `adb shell dumpsys deviceidle get deep` prints `IDLE`, probe, then `adb shell dumpsys deviceidle unforce`.
8. Overnight, unplugged, on mobile data. Before: `adb shell dumpsys batterystats --reset`. After: `adb shell dumpsys batterystats net.meshnode.app`, and read the app UID's estimated mAh and the hours unplugged. Reconnects: count the reconnect lines in `adb logcat -d | grep -iE "meshnode"` (and the `<data>/logs` file), or the jumps in `connect=` during a long probe. Record both; no numeric pass criterion (the spike measured about 0.3 % per 24 h).
9. APK size: see Build and install; record both numbers below.

## Carried checks (from the task reviews)

None of these has automated coverage. Mark each in the results table.

- **C1 Boot.** Reboot and unlock without opening the app. Commands: `adb logcat -d | grep -iE "meshnode|MeshService"`, `adb shell pidof net.meshnode.app`, and `meshd members` on a desktop. Pass: the notification shows and the phone is online in `meshd members` within 60 s of the unlock.
- **C2 Swipe with the exact-alarm grant.** With "Alarms & reminders" granted, swipe the app away, then poll `adb shell pidof net.meshnode.app` and `adb shell dumpsys activity services net.meshnode.app`. Pass: the service is running again within 10 s.
- **C3 Swipe without the exact-alarm grant.** Revoke that grant, swipe away, poll as in C2. Record whether and when the service returns (inexact alarm; the 15-minute watchdog is the backstop). With the battery exemption missing as well, the restart may be refused: then the restart notification must appear (see C4). Pass: either the service is back, or the notification appears; never silence.
- **C4 Watchdog without the battery exemption.** Revoke the battery-optimisation exemption and the exact-alarm grant, swipe the app away (do not use Stop), and wait up to 15 minutes, polling `adb shell pidof net.meshnode.app` and `adb shell dumpsys notification --noredact | grep -i "stopped in the background"`. Android refuses a background foreground-service start from the inexact alarm and the watchdog without the exemption. Pass: the notification "Meshnode stopped in the background — tap to restart" appears, and tapping it starts the service.
- **C5 Stop then Start quickly.** Press Stop (notification), then Start (app) within a second or two, repeatedly. The node ends up running; no crash; no "lock held" error in the notification.
- **C6 Always-on lock released after Stop.** Set `power = "always_on"` (see Editing meshd.toml on the phone) and Start. `adb shell dumpsys power | grep -i "meshnode:"` shows a held `PARTIAL_WAKE_LOCK` tagged `meshnode:always` (the Kotlin side prefixes the Rust tag with `meshnode:`; the other tags are `meshnode:charging`, `meshnode:request`, `meshnode:catchup`, `meshnode:alarm` (the Rust alarm lock), and `meshnode:alarm-receiver` from the alarm receiver). Press Stop and run the command again. Pass: `meshnode:always` is listed before Stop and absent after.
- **C7 Page under the CSP.** The placeholder page renders (styles and script load from `app.css` and `app.js`) and `invoke` works: status, join, Start and Stop return data, and no CSP violation shows (check `chrome://inspect` console). `chrome://inspect` works because the debug build's WebView is debuggable.
- **C8 Onboarding grants.** Each grant button opens the right system screen; the status line updates on return; a denied grant never blocks Start and never crashes the app.
- **C9 Cold start to startForeground.** After a reboot or `adb shell am kill net.meshnode.app`, run `adb logcat -v time -d | grep -E "Start proc.*net.meshnode.app|net.meshnode.app.*(startForeground|foreground)"` and the first notification post from `adb logcat -v time -d | grep -i "enqueueNotification.*net.meshnode.app"` (the app logs nothing on success). Pass: under 2 s from the process start line to the foreground notification (hard limit 10 s).
- **C10 ndk-context.** Reboot and do not open the app; wait until the notification shows and the node runs (the service initialised the Android context first). Then open the app: it must not crash. Repeat the other way round: force-stop, open the app first, then let the service start. Check `adb logcat -d | grep -iE "AndroidRuntime|FATAL"` is empty. The WebView is debuggable, so `chrome://inspect` works here too.

## `periodic_wake_s` default rule

The default is off (`[android] periodic_wake_s = 0` in `meshd.toml`). If in step 7 the p95 on a sleeping phone exceeds 60 s (success criterion 7), find the smallest `periodic_wake_s` that brings p95 under 60 s (edit `meshd.toml` as in Editing meshd.toml on the phone, Start, re-run the probe), and make that value the default in `mesh-host`'s config. If p95 is 60 s or less, leave the default at 0.

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
| Release APK size (stripped, unsigned, `app-universal-release-unsigned.apk`, arm64) | 35,678,515 bytes (34.0 MiB; measured 2026-11-11 with `apps/android/build-apk.sh --release`, with the R8 keep rules for JNA and the UniFFI bindings; `libmeshnode_app.so` inside is 32.6 MB) |
| Debug APK size (unstripped, arm64) | 802 MiB (measured in Task 8) |

## Carried owner gates from sub-specs 2 to 4

Still pending and unchanged by this sub-spec. Run them from their own documents and record the outcome there.

- Sub-spec 2: `docs/manual-checks/sub-spec-2.md`, steps 1-5, two desktops on different networks.
- Sub-spec 3: `docs/manual-checks/sub-spec-3.md`, steps 1-5, two desktops (`counter` fixture, static site, placement, undeploy and purge).
- Sub-spec 4: `docs/manual-checks/sub-spec-4.md`, steps 1-6 on Windows (elevated; installs a real OS service); `cargo test -p meshd --test service -- --ignored` elevated on a disposable VM; Linux (systemd) in a VM; macOS untested.

Steps 2-3 above need a working desktop mesh, so do the sub-spec 4 steps first.

## Automated coverage (for reference)

Rust: `mesh-host` (moved daemon tests, wake and catch-up, saver 503, power decision table), `mesh-admin` in-process tests, `mesh-ffi` host-side tests, `mesh-gateway` `_ping`, `meshd` probe. Kotlin: JVM unit tests (restart decision table, notification text). CI: `cargo ndk` builds, clippy on the Android target, the bindings drift check, the release APK build, `testUniversalDebugUnitTest` and `lintUniversalDebug`. No emulator and no device in CI.
