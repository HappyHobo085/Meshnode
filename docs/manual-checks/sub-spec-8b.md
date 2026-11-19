# Manual check: sub-spec 8b (packaging, release signing, upgrades)

Run by the owner (spec `2026-11-16-packaging-e2e-design.md` §1.4, §7). This covers the steps that need root, a device or Windows. The §1 run itself is `docs/e2e-checklist.md`. Until both are done, master spec §14 row 8b stays "manual check and §1 run pending".

The build agent ran none of this. It never installed a package on this PC, ran adb, started or stopped a service, elevated, or held a keystore. What it did run without root:
- `scripts/inspect-linux-packages.sh` on the 0.1.0 rpm and deb (contents, modes, unit, scriptlets, glibc dependency).
- `scripts/test-linux-packages.sh` with a 0.0.1 and a 0.1.0 build: install, upgrade, remove, reinstall and purge of the rpm (Fedora 41) and the deb (Debian 13) in rootless podman.
- `apps/android/build-apk.sh --release` without a keystore (fails with "Release signing: … not found") and `--release --unsigned` (an unsigned APK).
- The automated incompatible-data and peer-version tests (§4, §5).

**The MSI is not built yet** (Task 8, on the owner's Windows install). Everything MSI-specific below is written from the spec and is marked "(confirmed in Task 8)" where Task 8 must check it.

## 1. Container test limitation

None. The lifecycle test runs real systemd as PID 1 in each container (`podman run --systemd=always`), so unit enablement, the restart on upgrade and the stop on remove are real. What it does not cover: Nobara itself (the test uses Fedora 41), the desktop app's admin access through the `meshnode` group, and a reboot. The owner's rpm run (checklist A1, A4, Z1, criterion 1) covers those.

## 2. Windows MSI on the laptop

Build on the owner's Windows install (checklist 0.3). Inspect the MSI's tables first: `scripts/inspect-msi.ps1 <msi>` (written in Task 8; confirmed in Task 8). Pass: the fixed UpgradeCode, ProductVersion equal to `scripts/check-version.sh`, and the custom actions with `--from-package`.

Then on the laptop:

1. **Install** (checklist A2). Pass: SmartScreen shows **More info → Run anyway**; after the UAC prompt the service runs; the Meshnode app opens and every screen works **without elevation**; `meshd config get admin.windows_user_sid` equals the SID from `whoami /user`, not SYSTEM (`S-1-5-18`).
2. **Refusal.** From an elevated prompt: `meshd service install` and `meshd service uninstall` both print "Meshnode is installed by the system package; use your package manager (or the Windows installer) instead". The app's **Install as system service** shows the same refusal.
3. **Upgrade in place** (checklist A4): run the newer MSI. Pass: the service is stopped before the files are replaced and running again after (Event Log, source `Meshnode`: "Meshnode stopped." then "Meshnode started …"); `%ProgramData%\meshnode` is untouched (same node id in `meshd status`); the admin SID is unchanged.
4. **Downgrade**: run the older MSI after the newer one. Pass: refused with a message (confirmed in Task 8).
5. **Uninstall** (checklist Z3). Pass: the service is gone (`sc query meshnode`), `C:\Program Files\Meshnode` is gone, `%ProgramData%\meshnode` stays. Purge from an elevated prompt: `rmdir /s /q "%ProgramData%\meshnode"`.

**Changing the admin user later.** As the current admin user: `meshd config set admin.windows_user_sid <SID>` (a user SID, `S-1-5-21-…` or `S-1-12-1-…`; `whoami /user` prints yours). It prints "Saved. Restart meshd for this to take effect."; then, elevated, `meshd service stop` and `meshd service start`.

## 3. Android release signing and upgrade

1. Create the keystore and properties file (checklist 0.2). Never commit or share either.
2. `apps/android/build-apk.sh --release`. Pass: it ends with `build-apk: signed …/app-universal-release.apk` and a `certificate SHA-256 digest` line. Record the digest; every later release must print the same one.
3. Without the properties file (rename it for a moment, or set `MESHNODE_KEYSTORE_PROPERTIES` to a missing path), `build-apk.sh --release` fails with "Release signing: … not found" and produces no APK. (Already seen by the build agent; optional.)
4. **Install over** (checklist A3, A4): the 0.0.1 build, then the release with `adb install -r`. Pass: Android accepts it (same key, higher versionCode), `versionName` changes, the node keeps its identity and data, and the service restarts without opening the app (`MY_PACKAGE_REPLACED`).
5. **Wrong key is refused.** Installing a debug APK over the release one with `adb install -r` fails with `INSTALL_FAILED_UPDATE_INCOMPATIBLE`, and the installed app keeps its data. (Optional; shows why the keystore must be kept.)

## 4. Incompatible data: refused on every surface

**Gap: there is no dev command that bumps an existing store's schema.** `mesh_store::test_hooks::set_schema` exists only behind the `test-hooks` feature and is called only from tests; no binary exposes it. The store-schema path is covered by automated tests:
```
cargo test -p mesh-store --test rules open_refuses_newer_schema
cargo test -p mesh-host --test daemon incompatible
cargo test -p meshd --test incompatible     # meshd run exits 4; meshd status says why
```
For the owner's surfaces, use the same refusal path through `meshd.toml`: it refuses unknown keys, so a key from a "future" version makes the node refuse to start exactly as an incompatible store does (exit 4, `start-error.txt`, every surface). Never edit `node.key`, `store.redb`, `mesh.json` or `role.json` for this.

### 4.1 Linux (installed rpm): systemd, `meshd status`, desktop app

```
sudo systemctl stop meshnode
sudo test -e /var/lib/meshnode/meshd.toml && echo existed     # note whether it existed
echo 'future_key = 1' | sudo tee -a /var/lib/meshnode/meshd.toml
sudo systemctl start meshnode
```
Pass:
- After a few seconds, `systemctl is-active meshnode` prints `failed`; `systemctl show -p ExecMainStatus -p NRestarts meshnode` prints `ExecMainStatus=4` and `NRestarts=0`, and still `0` a minute later (no restart loop).
- `journalctl -u meshnode -n 20` shows the reason, naming `meshd.toml`.
- `meshd status` prints `not running: …` naming `meshd.toml`, and `echo $?` prints `3`.
- The desktop app shows "Meshnode refused to start" with the same reason.

Restore:
```
sudo sed -i '/^future_key = 1$/d' /var/lib/meshnode/meshd.toml && sudo chown meshnode:meshnode /var/lib/meshnode/meshd.toml   # if it existed
sudo rm /var/lib/meshnode/meshd.toml                                                                                        # if it did not
sudo systemctl start meshnode
```
Pass: active again; `meshd status` is normal; `/var/lib/meshnode/start-error.txt` is gone.

### 4.2 Windows (installed MSI): service, Event Log, `meshd status`, desktop app

Elevated PowerShell:
```
$p = "$env:ProgramData\meshnode\meshd.toml"
Test-Path $p                       # note whether it existed
meshd service stop
Add-Content -Path $p -Value 'future_key = 1'
meshd service start
```
Pass:
- The service ends **stopped** and stays stopped (`meshd service status` a minute later: `installed, stopped`). It stops with exit 0 on purpose: the failure actions would restart any non-zero exit.
- Event Viewer, Windows Logs, Application, source `Meshnode`: "Meshnode failed to start: …" naming `meshd.toml`.
- As the installing user, **not elevated**: `meshd status` prints `not running: …` (it reads `%ProgramData%\meshnode\start-error.txt`, which standard users may read), and the desktop app shows "Meshnode refused to start".

Restore (elevated): if the file existed, `(Get-Content $p) | Where-Object { $_ -ne 'future_key = 1' } | Set-Content $p`; if not, `Remove-Item $p`. Then `meshd service start`. Pass: running again.

### 4.3 Android: notification, no restart

The release app is not debuggable, so `run-as` cannot reach its files. Do this with the debug APK **before** the checklist's fresh start (0.1), which uninstalls it anyway:
```
apps/android/build-apk.sh
adb install -r apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
adb shell am force-stop net.meshnode.app
adb shell "run-as net.meshnode.app sh -c 'mkdir -p files/meshnode && echo future_key = 1 >> files/meshnode/meshd.toml'"
```
Open the app and press Start. Pass:
- The notification reads "Meshnode couldn't start: … — tap to open", naming `meshd.toml`.
- Reboot and unlock without opening the app, and swipe it away once: the service does not start by itself (`adb shell pidof net.meshnode.app` stays empty, or the process runs without `MeshService` in `adb shell dumpsys activity services net.meshnode.app`). Opening the app tries again and shows the same notification.

No restore needed: checklist 0.1 uninstalls this app.

## 5. Peer on an incompatible version

The owner cannot trigger this without a special build: 0.0.1 and 0.1.0 from the same commit speak the same protocols and store schema, so they are compatible. Covered by the automated test `cargo test -p mesh-core --test version` (`incompatible_peer_is_reported`). `meshd members` shows such a peer as `incompatible version (x)` and the desktop app's device card as "Incompatible version (x)".

## Results

| Date | Device | Step | Pass/fail | Notes |
|---|---|---|---|---|
| | Windows | 2 MSI tables (`inspect-msi.ps1`) | | |
| | Laptop | 2.1 install, admin without elevation | | |
| | Laptop | 2.2 refusal | | |
| | Laptop | 2.3 upgrade in place | | |
| | Laptop | 2.4 downgrade refused | | |
| | Laptop | 2.5 uninstall, data kept | | |
| | PC | 3.2 signed release build | | digest: |
| | S23 | 3.4 install over | | |
| | S23 | 3.5 wrong key refused (optional) | | |
| | PC | 4.1 incompatible, Linux | | |
| | Laptop | 4.2 incompatible, Windows | | |
| | S23 | 4.3 incompatible, Android | | |
