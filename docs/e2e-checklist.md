# End-to-end checklist (master spec §1)

The owner runs this on real devices before each release (master spec §13; sub-spec 8b §6). Record every step in `docs/e2e-results.md`; each step below names its row. Only the owner marks a step passed. The build agent ran none of this: it never installed a package, ran adb, started or stopped a service, or elevated.

Steps that need root, adb or Administrator are the owner's. `meshd` commands on the desktop run as your user once you are in the `meshnode` group (step 0.4).

## Devices

| Device | Role | Artifact | Notes |
|---|---|---|---|
| Desktop: Linux | host, founder | `.rpm` | Linux needs glibc 2.39 or later (Debian 13+, Ubuntu 24.04+, Fedora 40+) |
| Phone: Android | host | release-signed APK, sideloaded | package `net.meshnode.app` |
| Laptop: Windows laptop | client (criterion 5) | MSI, unsigned | the user who runs the installer gets admin access |

Order: 0, A, criteria 2, 1, 3, 4, 5, 7, 6, then Z. Criterion 1 needs the paired phone; criterion 6 removes the phone, so it runs last.

## 0. Preamble (once)

### 0.1 Fresh start

The first release starts from fresh data on every device (8b spec §1). Later releases must upgrade in place.

- **Phone.** The release APK has a different signature from the debug APK, so Android refuses to install it over the debug one. Uninstall the debug APK first; **this deletes its data** (node identity and replica). If you want to run the Android part of the incompatible-data check (`docs/manual-checks/sub-spec-8b.md` §4), do it before this step: it needs the debuggable app.
  ```
  adb uninstall net.meshnode.app
  ```
- **Desktop.** No service from `meshd service install`, and no other `meshd` running:
  ```
  systemctl status meshnode     # expect: "Unit meshnode.service could not be found."
  pgrep -a meshd                # expect: no output
  ```
  If a unit exists from a dev install, remove it with that build: `sudo meshd service uninstall --purge-data`.

  No leftover data or user from a dev install:
  ```
  sudo ls /var/lib/meshnode     # expect: "No such file or directory"
  getent passwd meshnode        # expect: no output
  ```
  If either is found while no package is installed (`rpm -q meshnode` says "not installed"), it is old dev data. Leftover data would make A1's **Create a new mesh** fail (the node is already initialised). Delete it (fresh start; it holds an old node identity): `sudo rm -rf /var/lib/meshnode`, then `sudo userdel meshnode`, and `sudo groupdel meshnode` if `getent group meshnode` still prints a line.
- **Laptop.** No service from a dev install: `sc query meshnode` must say the service does not exist. Otherwise, from an elevated prompt, `meshd service uninstall --purge-data` with that build.

  No leftover data: `dir "%ProgramData%\meshnode"` must say "File Not Found". If the folder exists, delete it from an elevated prompt: `rmdir /s /q "%ProgramData%\meshnode"`. Otherwise the install keeps its old `admin.windows_user_sid` and its old node, and A2's checks fail.

### 0.2 Android release keystore (once, keep forever)

```
mkdir -p ~/.config/meshnode
keytool -genkeypair -v -keystore ~/.config/meshnode/release.jks -alias meshnode -keyalg RSA -keysize 4096 -validity 10000
cp apps/android/keystore.properties.example apps/android/keystore.properties
```

Then edit `apps/android/keystore.properties` and set `storePassword` and `keyPassword`. The keystore is PKCS12, so both are the password you gave keytool. `apps/android/keystore.properties` and `*.jks` are gitignored; never commit them. To keep the properties file outside the repo, point `MESHNODE_KEYSTORE_PROPERTIES` at it.

**Back up `~/.config/meshnode/release.jks` and its password now**, somewhere off this machine. If you lose it, there are no more updates: only uninstall, which deletes the phone's data.

### 0.3 Build the artifacts

The repo is on FAT32, so keep the cargo target directory off it: `export CARGO_TARGET_DIR=~/.cache/meshnode-target`. `apps/android/build-apk.sh` needs `JAVA_HOME` (JDK 17), `ANDROID_HOME` and `NDK_HOME` (NDK 27.2); see its header.

The upgrade check (part A) needs an older build. Build 0.0.1 from the same commit in a scratch worktree, so the main checkout is never edited, then build the release in the main checkout:

```
REPO="$(git rev-parse --show-toplevel)"   # run from the Meshnode checkout
OLD="${TMPDIR:-/tmp}/meshnode-old"
(
  set -e                                # any failure stops here, before anything runs in the main checkout
  cd "$REPO"
  git worktree add "$OLD" HEAD
  cd "$OLD"
  scripts/set-version.sh 0.0.1
  cargo update -w --offline
  cargo update -w --offline --manifest-path apps/desktop/src-tauri/Cargo.toml
  ln -s "$REPO/apps/ui/node_modules" apps/ui/node_modules
  scripts/build-desktop-packages.sh     # prints the 0.0.1 .deb and .rpm paths
  MESHNODE_KEYSTORE_PROPERTIES="$REPO/apps/android/keystore.properties" apps/android/build-apk.sh --release
  cp apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk ~/meshnode-0.0.1.apk
)
cd "$REPO" && git worktree remove --force "$OLD"

scripts/check-version.sh                # prints the release version (0.1.0)
scripts/build-desktop-packages.sh
apps/android/build-apk.sh --release
```

Notes: `cargo update` refreshes the worktree's lockfiles for 0.0.1 (`scripts/build-desktop-packages.sh` builds `meshd` with `--locked`); the Android build updates its own. The worktree has no `node_modules` and no `keystore.properties` (both untracked), hence the symlink and `MESHNODE_KEYSTORE_PROPERTIES`. The subshell runs with `set -e`, so a failed `git worktree add` or `cd` stops it before anything touches the main checkout; if it fails part-way, fix the cause and run the `git worktree remove` line before retrying. `--force` drops the uncommitted 0.0.1 change with the worktree; nothing of it reaches the main checkout. `build-apk.sh --release` prints a `certificate SHA-256 digest` line each time: record both in `e2e-results.md` (Build); they must be equal. The rpm files are `$CARGO_TARGET_DIR/release/bundle/rpm/Meshnode-0.0.1-1.x86_64.rpm` and `Meshnode-0.1.0-1.x86_64.rpm`. The release APK is `apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk`.

Optional, no root: `scripts/inspect-linux-packages.sh <rpm> <deb>` checks the packages' contents.

**MSI (on the owner's Windows install, from Git Bash):** the same worktree sequence (without the APK lines), with `REPO` set to the Windows checkout (as `/c/...`), two changes: set the target dir in `C:/...` form, which both Git Bash and cargo read, e.g. `export CARGO_TARGET_DIR="$(cygpath -m "$HOME")/meshnode-target"`; and replace the `ln -s` line with `(cd apps/ui && npm ci)` (Git Bash's `ln -s` copies). `scripts/build-desktop-packages.sh` then builds the MSI and prints its path: `$CARGO_TARGET_DIR/release/bundle/msi/Meshnode_<version>_x64_en-US.msi` (without `CARGO_TARGET_DIR`: `apps\desktop\src-tauri\target\release\bundle\msi\`). Copy both MSIs to the laptop.

### 0.4 Admin access on the desktop

The package does not add you to the `meshnode` group, which opens the admin socket. After the first rpm install (A1):

```
sudo usermod -aG meshnode $USER
```

Then log out and back in. Check: `id -nG` lists `meshnode`, and `meshd status` works without sudo.

### 0.5 SmartScreen on the laptop

The MSI is unsigned. When Windows shows "Windows protected your PC", click **More info**, then **Run anyway**. Then accept the UAC prompt.

## A. Install and upgrade in place

Install 0.0.1 everywhere, build a small mesh, upgrade to 0.1.0, and check that data and membership survived. Downgrades are not supported on any platform.

**A1. Desktop rpm, install 0.0.1.**
```
sudo rpm -i --nosignature $CARGO_TARGET_DIR/release/bundle/rpm/Meshnode-0.0.1-1.x86_64.rpm
```
Fedora's rpm refuses unsigned packages without `--nosignature`. (`sudo dnf install --nogpgcheck <rpm>` also skips the check and resolves dependencies; it is untested here.) Then do 0.4.
Pass:
- `systemctl is-enabled meshnode` prints `enabled`, and `systemctl is-active meshnode` prints `active`.
- `meshd --version` prints `meshd 0.0.1`.
- `id meshnode` shows the system user.
- `sudo meshd service install` is refused: "Meshnode is installed by the system package; use your package manager (or the Windows installer) instead".

Then found the mesh: start **Meshnode** from the app menu and choose **Create a new mesh** (or `meshd init --name desktop`). Deploy a test site: make a folder with an `index.html`, then `meshd deploy static site <folder>`.

**A2. Laptop MSI, install 0.0.1.** Run the 0.0.1 MSI (0.5). Pass:
- The service runs: `meshd service status` prints `meshnode service: installed, running` (no elevation needed; not yet verified for a non-administrator: the service grants status queries to interactive users). `meshd.exe` is in `C:\Program Files\Meshnode\`, which the MSI does not put on `PATH`: use `"C:\Program Files\Meshnode\meshd.exe"` (or add the folder to your `PATH`).
- The Meshnode app opens and connects **without elevation**.
- `meshd config get admin.windows_user_sid` equals the SID that `whoami /user` prints.
- From an elevated prompt, `meshd service install` is refused with the package message.

Join as a client: on the desktop, `meshd invite --client` (valid 15 min; `--ttl 1h` for longer). On the laptop, in the app's first-run screen, choose **Join a mesh** and paste the ticket, or run `meshd join --name laptop <ticket>`. Pass: `meshd members` on the desktop lists `laptop` with role `client`, online.

**A3. Phone APK, install 0.0.1.** `adb install ~/meshnode-0.0.1.apk`. Open the app, grant the onboarding permissions, press Start, then **Join a mesh** and scan the desktop's QR (desktop app: **Add device**, role host). Pass: `meshd members` on the desktop lists the phone online.

**A4. Upgrade all three to 0.1.0.**
- Desktop: `sudo rpm -U --nosignature $CARGO_TARGET_DIR/release/bundle/rpm/Meshnode-0.1.0-1.x86_64.rpm`
- Phone: `adb install -r apps/android/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk` (do not open the app afterwards)
- Laptop: run the 0.1.0 MSI.

Pass:
- Desktop: `meshd --version` prints `meshd 0.1.0`; the unit is still `enabled` and `active`; `systemctl show -p NRestarts meshnode` prints `NRestarts=0`.
- Phone: `adb shell dumpsys package net.meshnode.app | grep versionName` prints `versionName=0.1.0`. The service restarted by itself (the notification is back without opening the app), and within 60 s `meshd members` on the desktop shows it online.
- Laptop: the service runs, the app still connects without elevation, and `meshd config get admin.windows_user_sid` is unchanged.
- Everywhere: the same node ids in `meshd members` (no new members) and `site` still in `meshd services`.
- Laptop, MSI downgrade: running the 0.0.1 MSI again is refused with "A newer version of Meshnode is already installed."; 0.1.0 stays installed and running.

**A5. Phone fresh start for criterion 2.** The phone joined on 0.0.1; criterion 2 needs a fresh pairing on the release. Remove it (desktop app Home, the phone's card, **Remove device…**, or `meshd remove <phone id prefix>`), then `adb uninstall net.meshnode.app` (deletes its data) and `adb install <release APK>`. The removed identity stays listed in `meshd members` as `(not effective)`; that is expected.

## Criterion 1: install and always on

**Desktop.** `sudo reboot` and **do not log in**. Pass: within 2 minutes of the boot, the phone's Home (or `meshd members` on the laptop) shows the desktop online. Then log in: `systemctl status meshnode` shows active since boot.

**Phone, reboot.** Reboot, unlock once, do not open the app. Pass: the notification shows, and `meshd members` on the desktop shows the phone online within 60 s of the unlock. Record the seconds.

**Phone, swipe away.** Swipe the app out of recents. Pass: `adb shell pidof net.meshnode.app` prints a pid again within 10 s, and the phone stays online in `meshd members`.

Record: row C1 (three entries).

## Criterion 2: pairing

Phone on **mobile data** (Wi-Fi off), fresh release install from A5. Desktop app: **Add device**, role host, 15 min. Phone: open the app, grant the onboarding permissions, Start, **Join a mesh**, allow the camera, scan the QR. Start a timer at the scan.

Pass: both devices list each other online within 30 s. Then turn the phone's Wi-Fi on: both still show each other online within 30 s.

Record: row C2 (seconds, network).

## Criterion 3: hosting on both

Build the Aegis sync service from the Aegis repo (`sync-server/wasm`, see its `AGENTS.md`; needs `rustup target add wasm32-wasip2` once) and deploy it on the desktop:
```
cargo build --locked --release --target wasm32-wasip2 --manifest-path <aegis>/sync-server/wasm/Cargo.toml
meshd deploy wasm sync <aegis>/sync-server/wasm/target/wasm32-wasip2/release/aegis_sync.wasm --memory-mb 64 --timeout-ms 30000 --max-body 8388608
```
Nodes have no port mappings by default. On **each** node map port 7801 to it, then restart that node: `meshd config set gateway.ports 7801=sync` on the desktop (and the laptop), and on the phone Settings, Port mappings, `7801` → `sync`.
`site` is already deployed (A1). Also deploy a site **from the phone**: Services, Deploy, pick a folder, name `site2`.

Pass:
- `meshd services` lists `site`, `site2` and `sync` (`sync` with `64 MB / 30000 ms / 8388608 B`), and the phone's Services lists the same.
- In the desktop browser: `http://site.mesh.localhost:7777`, `http://site2.mesh.localhost:7777` and `http://127.0.0.1:7801/healthz` (200).
- In the **phone's** browser: the same three URLs.

Record: row C3.

## Criterion 4: real use (Aegis)

Aegis desktop on the desktop and Aegis Android on the phone, each using **its own device's** gateway, `http://127.0.0.1:7801`. Build Aegis from its repo as its `AGENTS.md` says. The taps are in `docs/manual-checks/sub-spec-7.md` steps 2–5 (register on the desktop, restore on the phone, approve on the desktop, then sync). Type the URL into Aegis's sync settings.

Pass:
- Bookmarks added on each device appear on the other.
- With the phone in airplane mode, edit on both (add, edit the same bookmark on both, delete). Turn airplane mode off and press Sync now on both. Both show the **same** bookmark set.

Record: row C4 (time from reconnect to convergence).

## Criterion 5: survives a node going down

Put the laptop on a different network from the phone if you can (a phone hotspot for the laptop is fine). Power the desktop off: `sudo poweroff`.

Pass:
- **Phone, locally:** the phone's browser loads `http://site.mesh.localhost:7777`, `http://site2.mesh.localhost:7777` and `http://127.0.0.1:7801/healthz`. Aegis Android still syncs.
- **Laptop (client, forwarding through the phone):** the laptop's browser loads the same three URLs. `meshd members` on the laptop shows the desktop offline and the phone online.

Record: row C5. Power the desktop on again afterwards.

## Criterion 7: battery (before criterion 6)

Phone on Wi-Fi, default power policy (Balanced), charged, unplugged, screen off for 24 h. At the start, with USB connected: `adb shell dumpsys batterystats --reset`. Then unplug, note the battery % (Settings, Battery) and the time, and turn the screen off.

After 24 h, **before turning the screen on**:
1. On the desktop, `meshd members`. Pass: the phone is online.
2. Make a desktop write: deploy a small folder, `meshd deploy static battery <folder>`. Pass: within 60 s the phone's `LAG` in `meshd members` drops to `0` (the phone holds every op the desktop has).

Then note the battery %. Optional: `adb shell dumpsys batterystats net.meshnode.app` for the app's estimated mAh.

Pass: drain at most 5 % per 24 h. Record: row C7 (start %, end %, hours, drain per 24 h, the write delay).

## Criterion 6: removal (last)

Remove the phone from the desktop: desktop app Home, the phone's card, **Remove device…**, type its name. (Or `meshd remove <phone id prefix>`.)

Pass:
- Within 60 s the phone's app shows "This device was removed from your mesh".
- `meshd members` on the desktop and the laptop no longer show the phone as effective.
- A later desktop write never reaches the phone: `meshd deploy static after-removal <folder>`; the phone's Services does not list it.

Record: row C6 (seconds). To use the phone again, reset its identity in the app and pair it again.

## Z. Uninstall and purge

Run when you are done with the mesh on that device. Plain uninstall keeps the data everywhere except Android, where uninstall always deletes it.

**Z1. Desktop rpm.**
- Remove: `sudo rpm -e meshnode`. Pass: `systemctl status meshnode` says the unit could not be found; `/var/lib/meshnode` and the `meshnode` user are still there.
- Reinstall: `sudo rpm -i --nosignature <0.1.0 rpm>`. Pass: enabled and active again, and `meshd status` shows the same `Node:` id as before.
- Purge (deletes the node identity and data; rpm has no purge):
  ```
  sudo sh -c 'rpm -e meshnode && userdel meshnode && rm -rf /var/lib/meshnode'
  ```
  `userdel` keeps the `meshnode` group while it still has members (you, after 0.4). If `getent group meshnode` still prints a line, run `sudo groupdel meshnode`.

**Z2. Phone.** `adb uninstall net.meshnode.app`. Pass: the app is gone; this deletes its data.

**Z3. Laptop MSI.** Settings, Apps, Installed apps, Meshnode, Uninstall. Pass: `sc query meshnode` says the service does not exist; `%ProgramData%\meshnode` is still there. Purge, from an elevated prompt: `rmdir /s /q "%ProgramData%\meshnode"`.

Record: rows Z1–Z3.

## Debian/Ubuntu (deb), for reference

The owner's run uses the rpm; the deb's install, upgrade, remove and purge are tested in containers (`scripts/test-linux-packages.sh`). Commands: `sudo apt-get install ./Meshnode_<version>_amd64.deb` (install and upgrade), `sudo apt-get remove meshnode` (keeps data and user), `sudo dpkg --purge meshnode` (deletes `/var/lib/meshnode`, the user and the group).
