# Manual check: sub-spec 6 (Tauri management UI)

Run by the owner on Windows 11 and the Galaxy S23 (spec `2026-11-11-tauri-ui-design.md` §9.6). Until this is done, master spec §14 row 6 stays "implemented, manual check pending". Nothing here has been run on a device or with a real webview by the build agent: it never installs services, never answers a UAC prompt and never touches a phone.

Step 2 is success criterion 2 of the master spec: pairing within 30 s, once on Wi-Fi and once on mobile data.

## Build and run

Desktop (Windows, from the repo root; `D:` is FAT32, so keep the target directory off it):

```
set CARGO_TARGET_DIR=%LOCALAPPDATA%\meshnode-target
cargo build -p meshd
cd apps\ui && npm ci && cd ..\..
cd apps\desktop
cargo tauri build --no-bundle
```

`beforeBuildCommand` builds `apps/ui`. The app is `meshnode-desktop.exe` in the release folder of the target directory. Put `meshd.exe` next to it, or on `PATH`; otherwise use **Locate meshd…** in the UI. For live development: `cargo tauri dev` in `apps/desktop`.

Android: `apps/android/build-apk.sh` (debug, installable) and `apps/android/build-apk.sh --release` (stripped, unsigned, size only). The owner installs the debug APK with `adb install -r apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk` (not run by the agent).

## Steps (spec §9.6)

Each step has a row in the results table. Record the date, the device, the network and the result.

1. **Fresh PC: Run without installing.** On a PC with no Meshnode daemon (no service, no `%LOCALAPPDATA%\Meshnode`), start the desktop app. Actions: the **No daemon** screen shows; press **Run without installing**; wait. Pass: within 10 s the first-run screen (**Create a new mesh** / **Join a mesh**) appears, with the device name prefilled from the hostname; `%LOCALAPPDATA%\Meshnode\meshd.log` exists; closing the app leaves `meshd` running (Task Manager) and reopening the app connects without the No daemon screen. Create a new mesh; Home shows "1 of 1 devices online".
2. **Pair by QR within 30 s, on Wi-Fi and on mobile data.** Desktop: **Add device**, role host, 15 min; a QR, the ticket text and the ring countdown show. Phone: install the debug APK, Start the node, **Join a mesh**, grant the camera, scan the QR. Pass: from the moment of the scan, both devices list each other online within 30 s (time it); the desktop shows the success state ("<phone> joined") with the card animating in. Run it twice with a fresh phone identity each time (Settings, Reset identity): once with the phone on Wi-Fi, once on mobile data (Wi-Fi off). Record the seconds for each.
3. **Deploy a static site from the phone.** Phone: Services, Deploy, choose a site folder (system folder picker), name `site`, default placement, deploy. Pass: the progress bar completes and the service is listed; `http://site.mesh.localhost:7777` (or the port shown on Home) loads the site in the PC browser and in the phone browser; on the desktop the gateway URL on Home opens in the default browser.
4. **Placement, outbound, undeploy, purge, with confirmations.** On a service: change placement (add and remove a tag chip), edit the outbound allowlist and save, then Undeploy, then Purge. Pass: each change shows on both devices within 10 s; Undeploy and Purge each open a confirmation whose button stays disabled until the service name is typed exactly, and the text states the honest limit (purge hides the data on every device); a wrong name never enables the button.
5. **Logs.** Open Logs on the desktop and on the phone. Pass: lines stream live; Pause stops the list moving and Resume continues; the level chips filter; the search box filters; the view stays responsive.
6. **Latency probe from the phone to the PC.** Phone: Settings, Latency probe, pick the PC. Pass: samples draw live on the chart, the run ends with p50 / p95 / max, and a healthy link shows no sample errors. Record the three numbers.
7. **Power policy persists.** Phone: Settings, change the power policy (for example Balanced to Saver); force-stop the app, reopen it and press Start if needed. Pass: the chosen card is still selected after the restart.
8. **Remove the phone from the desktop.** Desktop: Home, the phone's card menu, Remove, type its name. Pass: the confirmation states that a removed device can never rejoin with the same identity; the desktop drops the card, and the phone's UI reflects the removal within 60 s; both UIs agree. Record the seconds.
9. **Look and feel.** Light and dark (the Settings override, and the OS setting), reduced motion on (OS setting: no pulse, no slide), a narrow phone width (bottom tabs, floating **Add device**, touch targets, safe-area insets), and the desktop window dragged down to 800 px wide (sidebar, no horizontal scroll, readable). Pass: nothing clipped, text readable in both themes, focus rings visible when tabbing on the desktop.
10. **Install as system service via UAC. VM or spare machine recommended.** This installs a real Windows service and needs elevation. Use a disposable VM or a spare PC if you can. On a PC with no daemon, press **Install as system service** and accept the UAC prompt. Pass: within 30 s the UI connects, shows "System service" as the connection, and `meshd service status` agrees. Remove it afterwards with an elevated `meshd service uninstall --purge-data`. Do not run this on the machine that holds your only copy of a mesh identity.

## Carried checks (built but never run here)

Mark each in the results table. None has automated coverage with a real webview or device.

- **C1 Live streams, desktop.** In the desktop app (a webview with zero core permissions), Logs follow, the latency probe and the deploy progress bar (a static folder deploy) each update live. Pass: lines, samples and progress reach the page as they happen; no CSP or permission error in the webview console (F12 in `cargo tauri dev`).
- **C2 Live streams, Android.** The same three on the S23 (`chrome://inspect` for the console). Pass: as C1.
- **C3 Desktop dialogs and `open_url`.** **Locate meshd…** opens a file dialog and the choice is remembered after a restart; the deploy sheet's file and folder dialogs work; clicking the gateway URL opens the default browser. Pass: also, a non-local URL such as `http://example.com` is refused; only `http://*.localhost:*` and `http://127.0.0.1:*` open.
- **C4 `daemon_install` through UAC, and declining.** Accepting is step 10. Also press **Install as system service** and click No on the UAC prompt. Pass: the UI says "Installation cancelled", nothing changes (no service, the No daemon screen stays) and Run without installing still works. Same VM or spare-machine advice as step 10.
- **C5 Android SAF folder picker, `pick_wasm` and in-process admin.** Deploy a nested folder from the picker (subfolders, 100 or more files, a space and a non-ASCII letter in a file name): all files arrive and the site loads. Deploy a `.wasm` file through the picker (copied to the cache, then deployed). With the node running, every admin call works in-process (Home, Services, Settings). With the node stopped (Stop in the notification), the UI shows **Node stopped** with Start, admin calls say "Start the node", and Start brings the UI back. Cancelling either picker shows no error and no stuck progress.
- **C6 QR camera permission flow.** Fresh install: tap Scan and the system camera prompt appears. Allow: the scanner opens. Revoke the permission (App info, Permissions), tap Scan and deny (twice, so that "don't ask again" applies). Pass: "Camera access is off — paste the invite instead" shows with the paste field, and pasting the ticket joins.
- **C7 Quick service restart during a log follow.** Open Logs on the phone, then Stop and Start the service quickly (notification, then the app), several times; also stop and start the daemon on the desktop while its Logs view is open. Pass: the view shows "Disconnected — Resume" (no frozen list, no crash), Resume works once the node is back, and repeated cycles never leave the view stuck or stop other screens updating.
- **C8 Concurrent streams.** Open Logs and a probe together; leave Logs and return to it a few times. Pass: no "too many streams" error in normal use (the cap is 4 per window).

## Results

| Date | Device | Network | Step or check | Result | Notes |
|---|---|---|---|---|---|
| | PC (Windows 11) | | 1 Run without installing | | |
| | PC + S23 | Wi-Fi | 2 pairing | | seconds: |
| | PC + S23 | Mobile data | 2 pairing | | seconds: |
| | S23 | | 3 static site from the phone | | |
| | | | 4 placement, outbound, undeploy, purge | | |
| | | | 5 logs | | |
| | S23 to PC | | 6 latency probe | | p50 / p95 / max: |
| | S23 | | 7 power policy persists | | |
| | | | 8 remove the phone | | seconds: |
| | | | 9 look and feel | | |
| | VM or spare PC | | 10 install as service via UAC | | |
| | | | C1 streams, desktop | | |
| | | | C2 streams, Android | | |
| | | | C3 dialogs and open_url | | |
| | | | C4 UAC declined | | |
| | | | C5 SAF, pick_wasm, in-process admin | | |
| | | | C6 camera permission | | |
| | | | C7 restart during log follow | | |
| | | | C8 stream cap | | |

### Size

| Item | Value |
|---|---|
| Release APK size (stripped, unsigned, `app-universal-release-unsigned.apk`, arm64) | 37,695,125 bytes (35.9 MiB; the sub-spec 5 shell alone was 35,678,515 bytes) |

## Carried owner gates from sub-specs 2 to 5

Still pending and unchanged by this sub-spec; run them from their own documents (`docs/manual-checks/sub-spec-2.md` to `sub-spec-5.md`) and record the outcome there. The sub-spec 4 steps (a working desktop mesh) come first.

## Automated coverage (for reference)

Rust: `mesh-ui` (every command, error and hint, QR round trip, streams and their cap, `DirSource`), `mesh-ui-tauri` and `apps/desktop/src-tauri` (daemon locator, `install_command`, spawn of `meshd run`). Frontend: Vitest (poller, invite countdown, typed-name confirmation, error rendering, log view), `svelte-check`, ESLint and Prettier. Kotlin: the `FolderPicker` path mapping. CI: jobs `ui`, `ts-types` (ts-rs drift), `desktop-ui` (Windows and Ubuntu), `android-apk` (builds `apps/ui` first). No real webview, no UAC and no device in CI.
