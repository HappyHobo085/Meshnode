# Manual check: sub-spec 8b (packaging, release signing, upgrades)

Run by the owner (spec `2026-11-16-packaging-e2e-design.md` §1.4, §7). This covers the steps that need root, a device or Windows. The §1 run itself is `docs/e2e-checklist.md`. Until both are done, master spec §14 row 8b stays "manual check and §1 run pending".

The build agent ran none of this. It never installed a package on this PC, ran adb, started or stopped a service, elevated, or held a keystore. What it did run without root:
- `scripts/inspect-linux-packages.sh` on the 0.1.0 rpm and deb (contents, modes, unit, scriptlets, glibc dependency).
- `scripts/test-linux-packages.sh` with a 0.0.1 and a 0.1.0 build: install, upgrade, remove, reinstall and purge of the rpm (Fedora 41) and the deb (Debian 13) in rootless podman.
- `apps/android/build-apk.sh --release` without a keystore (fails with "Release signing: … not found") and `--release --unsigned` (an unsigned APK).
- The automated incompatible-data and peer-version tests (§4, §5), and the `set_schema` example against a throwaway node (§4).

The MSI was built and exercised in a disposable Windows 11 Pro VM (Task 8), never on the owner's laptop. There, elevated over SSH: `scripts/inspect-msi.ps1` on the 0.1.0 MSI; `scripts/test-msi.ps1` with a 0.0.1 and a 0.1.0 MSI (install, refusal, upgrade in place, downgrade refused, uninstall, reinstall, purge; all silent, `msiexec /qn`); §4.2 with the installed MSI, including the standard-user reads. Not covered there: the installer's dialogs (no interactive desktop over SSH), SmartScreen, a UAC prompt, and the desktop app.

## 1. Container test limitation

None. The lifecycle test runs real systemd as PID 1 in each container (`podman run --systemd=always`), so unit enablement, the restart on upgrade and the stop on remove are real. What it does not cover: Nobara itself (the test uses Fedora 41), the desktop app's admin access through the `meshnode` group, and a reboot. The owner's rpm run (checklist A1, A4, Z1, criterion 1) covers those.

## 2. Windows MSI on the laptop

Build on the owner's Windows install (checklist 0.3). Inspect the MSI's tables first, from PowerShell: `powershell -ExecutionPolicy Bypass -File scripts\inspect-msi.ps1 <msi>`. Pass: it ends with `all checks passed` (the fixed UpgradeCode, ProductVersion equal to `scripts/check-version.sh`, `meshd.exe` and the marker in the pinned `C:\Program Files\Meshnode`, the service custom actions with `--from-package`, the downgrade block).

On a disposable Windows machine (not the laptop: it installs, uninstalls and finally deletes `%ProgramData%\meshnode`), elevated: `powershell -ExecutionPolicy Bypass -File scripts\test-msi.ps1 <old.msi> <new.msi> -Purge` runs the whole lifecycle silently and ends with `all checks passed`. Optional; the laptop steps below are the check.

Then on the laptop:

1. **Install** (checklist A2). Pass: SmartScreen shows **More info → Run anyway**; after the UAC prompt the service runs; the Meshnode app opens and every screen works **without elevation**; `meshd config get admin.windows_user_sid` equals the SID from `whoami /user`, not SYSTEM (`S-1-5-18`).
2. **Refusal.** From an elevated prompt: `meshd service install` and `meshd service uninstall` both print "Meshnode is installed by the system package; use your package manager (or the Windows installer) instead". The app's **Install as system service** shows the same refusal.
3. **Upgrade in place** (checklist A4): run the newer MSI. Pass: the service is stopped before the files are replaced and running again after (Event Log, source `Meshnode`: "Meshnode stopped." then "Meshnode started …"); `%ProgramData%\meshnode` is untouched (same node id in `meshd status`); the admin SID is unchanged.
4. **Downgrade**: run the older MSI after the newer one. Pass: refused with "A newer version of Meshnode is already installed." (silently, `msiexec /i <old.msi> /qn` exits 1603 with that line in its `/l*v` log); the newer version stays installed and its service keeps running.
5. **Uninstall** (checklist Z3). Pass: the service is gone (`sc query meshnode`), `C:\Program Files\Meshnode` is gone, `%ProgramData%\meshnode` stays. Purge from an elevated prompt: `rmdir /s /q "%ProgramData%\meshnode"`.

**Changing the admin user later.** As the current admin user: `meshd config set admin.windows_user_sid <SID>` (a user SID, `S-1-5-21-…` or `S-1-12-1-…`; `whoami /user` prints yours). It prints "Saved. Restart meshd for this to take effect."; then, elevated, `meshd service stop` and `meshd service start`.

## 3. Android release signing and upgrade

1. Create the keystore and properties file (checklist 0.2). Never commit or share either.
2. `apps/android/build-apk.sh --release`. Pass: it ends with `build-apk: signed …/app-universal-release.apk` and a `certificate SHA-256 digest` line. Record the digest; every later release must print the same one.
3. Without the properties file (rename it for a moment, or set `MESHNODE_KEYSTORE_PROPERTIES` to a missing path), `build-apk.sh --release` fails with "Release signing: … not found" and produces no APK. (Already seen by the build agent; optional.)
4. **Install over** (checklist A3, A4): the 0.0.1 build, then the release with `adb install -r`. Pass: Android accepts it (same key, higher versionCode), `versionName` changes, the node keeps its identity and data, and the service restarts without opening the app (`MY_PACKAGE_REPLACED`).
5. **Wrong key is refused.** Installing a debug APK over the release one with `adb install -r` fails with `INSTALL_FAILED_UPDATE_INCOMPATIBLE`, and the installed app keeps its data. (Optional; shows why the keystore must be kept.)

## 4. Incompatible data: refused on every surface

A dev-only example rewrites the store schema of a **stopped** node (it wraps `mesh_store::test_hooks::set_schema`):

```
cargo build -p mesh-store --features test-hooks --example set_schema
$CARGO_TARGET_DIR/debug/examples/set_schema <store.redb | data dir> <schema>
```

A data directory means its `store.redb`. It refuses a missing file, and it refuses while another process has the store open ("… is open in another process (a running meshd?); stop it first"), so it cannot change a running node. Setting the schema back to `1` undoes it. The build agent ran this on a throwaway node: bumped to 2, `meshd run` exited 4 and wrote `start-error.txt`, `meshd status` printed `not running: cannot start the node: store: data was written by an incompatible Meshnode version (store schema 2, this build reads 1); install a matching version` (exit 3); set back to 1, the node ran again and `start-error.txt` was gone.

The real install is bumped **in place, with a backup**, rather than through a copy: a copy of the data directory carries `node.key`, so a dev `meshd` on it would be a second node with the same identity if the refusal ever failed. In place, only the service's own surfaces see the bumped store.

Never edit `node.key`, `mesh.json` or `role.json` for this.

### 4.1 Linux (installed rpm): systemd, `meshd status`, desktop app

```
X=$CARGO_TARGET_DIR/debug/examples/set_schema
sudo systemctl stop meshnode
sudo cp -a /var/lib/meshnode/store.redb /var/lib/meshnode/store.redb.bak
sudo "$X" /var/lib/meshnode 2
sudo systemctl start meshnode
```
Pass:
- After a few seconds, `systemctl is-active meshnode` prints `failed`; `systemctl show -p ExecMainStatus -p NRestarts meshnode` prints `ExecMainStatus=4` and `NRestarts=0`, and still `0` a minute later (no restart loop).
- `journalctl -u meshnode -n 20` shows "store schema 2, this build reads 1".
- `meshd status` prints `not running: …store schema 2, this build reads 1…`, and `echo $?` prints `3`.
- The desktop app shows "Meshnode refused to start" with the same reason.

Restore:
```
sudo "$X" /var/lib/meshnode 1
sudo systemctl start meshnode
sudo rm /var/lib/meshnode/store.redb.bak
```
Pass: active again; `meshd status` shows the same `Node:` id; `/var/lib/meshnode/start-error.txt` is gone. If setting it back fails, `sudo mv /var/lib/meshnode/store.redb.bak /var/lib/meshnode/store.redb` instead, then start.

### 4.2 Windows (installed MSI): service, Event Log, `meshd status`, desktop app

Build the example on the owner's Windows install (`cargo build -p mesh-store --features test-hooks --example set_schema`, giving `<target dir>\debug\examples\set_schema.exe`; `target\debug\examples\set_schema.exe` without `CARGO_TARGET_DIR`) and copy it to the laptop. The laptop is a client, but it has a store too. Elevated prompt:
```
meshd service stop
copy "%ProgramData%\meshnode\store.redb" "%ProgramData%\meshnode\store.redb.bak"
set_schema.exe "%ProgramData%\meshnode" 2
meshd service start
```
Pass:
- The service ends **stopped** and stays stopped (`meshd service status` a minute later: `installed, stopped`). It stops with exit 0 on purpose: the failure actions would restart any non-zero exit.
- Event Viewer, Windows Logs, Application, source `Meshnode`: "Meshnode failed to start: …store schema 2, this build reads 1…".
- As the installing user, **not elevated**: `meshd status` prints `not running: …` (it reads `%ProgramData%\meshnode\start-error.txt`, which standard users may read), and the desktop app shows "Meshnode refused to start".
- **Stopped, no refusal** (M2): restore first (below), then `meshd service stop` and check that `%ProgramData%\meshnode\start-error.txt` does not exist. As a **standard user** (not an administrator; another account is fine), `meshd status` prints `meshd is not running (start it with "meshd run" or "meshd service install")` and exits 3, and the desktop app shows its not-running screen, **not** "Meshnode refused to start" or "start error unreadable". A standard user cannot list `%ProgramData%\meshnode`, but opening the missing file still reports "not found". Then `meshd service start` (elevated).

Restore (elevated): `set_schema.exe "%ProgramData%\meshnode" 1`, `meshd service start`, then `del "%ProgramData%\meshnode\store.redb.bak"`. Pass: running again.

### 4.3 Android: notification, no restart

The release app is not debuggable, so `run-as` cannot reach its files. Do this with the debug APK **before** the checklist's fresh start (0.1), which uninstalls it anyway. The debug app must have created or joined a mesh (a throwaway one is fine), so it has a store. The store is bumped on the PC and pushed back:
```
apps/android/build-apk.sh
adb install -r apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
adb shell am force-stop net.meshnode.app
adb exec-out run-as net.meshnode.app cat files/meshnode/store.redb > store.redb
$CARGO_TARGET_DIR/debug/examples/set_schema store.redb 2
adb push store.redb /data/local/tmp/store.redb
adb shell run-as net.meshnode.app cp /data/local/tmp/store.redb files/meshnode/store.redb
adb shell rm /data/local/tmp/store.redb
rm store.redb
```
`am force-stop` stops the service and keeps it from being restarted until the app is opened. Open the app and press Start. Pass:
- The notification reads "Meshnode couldn't start: … — tap to open", with "store schema 2, this build reads 1".
- Reboot and unlock without opening the app, and swipe it away once: the service does not start by itself (`adb shell pidof net.meshnode.app` stays empty, or the process runs without `MeshService` in `adb shell dumpsys activity services net.meshnode.app`). Opening the app tries again and shows the same notification.

No restore needed: checklist 0.1 uninstalls this app.

### 4.4 Secondary: the refusal path through `meshd.toml`

This tests the refusal **UI path**, not the schema check. `meshd.toml` refuses unknown keys, and an unparsable `meshd.toml` takes the same path (exit 4, `start-error.txt`, every surface). With the node stopped, add a line `future_key = 1` to `meshd.toml` in the data directory (create the file if it does not exist), start the node, and expect the steps above with a reason naming `meshd.toml`. Remove the line (or the file you created) to restore. The store-schema check itself is also covered by automated tests:
```
cargo test -p mesh-store --test rules open_refuses_newer_schema
cargo test -p mesh-host --test daemon incompatible
cargo test -p meshd --test incompatible     # meshd run exits 4; meshd status says why
```

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
| | Laptop | 4.2 stopped, no refusal (standard user) | | |
| | S23 | 4.3 incompatible, Android | | |
