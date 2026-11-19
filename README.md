# Meshnode

A self-hosted server mesh across the owner's own devices (Linux, Windows, Android): every host replicates and serves the hosted WASM services and static sites. Design: `docs/superpowers/specs/2026-11-08-meshnode-design.md`.

## Build and install

One product version for everything: `scripts/set-version.sh <x.y.z>` sets it, `scripts/check-version.sh` checks it. Release rule: `CONTRIBUTING.md`. Downgrades are not supported on any platform.

The repo sits on a FAT32 disk, so keep build output off it: `export CARGO_TARGET_DIR=~/.cache/meshnode-target`.

### Linux (rpm, deb)

```
scripts/build-desktop-packages.sh       # builds the deb and rpm, prints their paths
scripts/inspect-linux-packages.sh <rpm> <deb>
scripts/test-linux-packages.sh <old.rpm> <new.rpm> <old.deb> <new.deb>   # rootless podman lifecycle test
```

Package `meshnode`: the desktop app, `/usr/bin/meshd` and the systemd unit `meshnode.service` (user `meshnode`, data in `/var/lib/meshnode`). Needs glibc 2.39 or later (Debian 13+, Ubuntu 24.04+, Fedora 40+). The packages are unsigned.

| | rpm (Fedora, Nobara) | deb (Debian, Ubuntu) |
|---|---|---|
| Install or upgrade | `sudo rpm -i --nosignature <rpm>` / `sudo rpm -U --nosignature <rpm>` | `sudo apt-get install ./<deb>` |
| Remove (keeps data and user) | `sudo rpm -e meshnode` | `sudo apt-get remove meshnode` |
| Purge (deletes data and user) | `sudo sh -c 'rpm -e meshnode && userdel meshnode && rm -rf /var/lib/meshnode'` | `sudo dpkg --purge meshnode` |

Install enables and starts the service; upgrade restarts it if it is enabled (also after a refused start); data, user and enablement are kept. To use the app and `meshd` without sudo, join the group once and log in again: `sudo usermod -aG meshnode $USER`. While the package is installed, `meshd service install` and `uninstall` refuse; use the package manager.

### Windows (MSI)

Built on Windows from Git Bash with `scripts/build-desktop-packages.sh` (MSI details confirmed in Task 8). Unsigned: at the SmartScreen prompt choose **More info → Run anyway**. The user who runs the installer gets admin access without elevation. Uninstall keeps `%ProgramData%\meshnode`.

### Android (APK)

```
apps/android/build-apk.sh               # debug APK
apps/android/build-apk.sh --release     # release APK, signed with your keystore
```

The release build signs with the keystore named in `apps/android/keystore.properties` (copy `keystore.properties.example`; gitignored) or the file `MESHNODE_KEYSTORE_PROPERTIES` points at, and fails without one. It prints the certificate's SHA-256 digest. `--release --unsigned` is for CI only. Create the keystore once with the `keytool` command in `docs/e2e-checklist.md` (0.2). **Back up the keystore: losing it means no more updates, only uninstall and data loss.**

Install with `adb install <apk>`, upgrade with `adb install -r <apk>` (data kept).

## Testing

`cargo test --workspace`. Release checks on real devices: `docs/e2e-checklist.md`, results in `docs/e2e-results.md`. Per-sub-spec manual checks: `docs/manual-checks/`.
