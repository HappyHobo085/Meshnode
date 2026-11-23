# Meshnode

A self-hosted server mesh across the owner's own devices (Linux, Windows, Android): every host replicates and serves the hosted WASM services and static sites. Design: `docs/superpowers/specs/2026-11-08-meshnode-design.md`.

**Status:** early and pre-1.0 (0.1.0). Formats and protocols may change between releases without migrations (see `CONTRIBUTING.md`), and the installers are unsigned.

## Build and install

One product version for everything: `scripts/set-version.sh <x.y.z>` sets it, `scripts/check-version.sh` checks it. Release rule: `CONTRIBUTING.md`. Downgrades are not supported on any platform.

Build output can live outside the checkout (useful when it sits on a slow or FAT32 disk): `export CARGO_TARGET_DIR=~/.cache/meshnode-target`.

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

Built on Windows from Git Bash with `scripts/build-desktop-packages.sh`, giving `<target dir>\release\bundle\msi\Meshnode_<version>_x64_en-US.msi` (the target dir is `CARGO_TARGET_DIR`, else `apps\desktop\src-tauri\target`); `scripts/inspect-msi.ps1 <msi>` checks its tables and `scripts/test-msi.ps1 <old.msi> <new.msi>` runs the install/upgrade/uninstall lifecycle on a disposable machine. Unsigned: at the SmartScreen prompt choose **More info → Run anyway**. It installs into `C:\Program Files\Meshnode` (fixed; `meshd.exe` is not put on `PATH`) and installs and starts the `meshnode` service. The user who runs the installer gets admin access without elevation. An upgrade stops the service, replaces the files and starts it again, keeping the data; an older MSI over a newer one is refused ("A newer version of Meshnode is already installed."). Uninstall removes the service and keeps `%ProgramData%\meshnode`. While it is installed, `meshd service install` and `uninstall` refuse.

### Android (APK)

```
apps/android/build-apk.sh               # debug APK
apps/android/build-apk.sh --release     # release APK, signed with your keystore
```

The release build signs with the keystore named in `apps/android/keystore.properties` (copy `keystore.properties.example`; gitignored) or the file `MESHNODE_KEYSTORE_PROPERTIES` points at, and fails without one. It prints the certificate's SHA-256 digest. `--release --unsigned` is for CI only. Create the keystore once with the `keytool` command in `docs/e2e-checklist.md` (0.2). **Back up the keystore: losing it means no more updates, only uninstall and data loss.**

Install with `adb install <apk>`, upgrade with `adb install -r <apk>` (data kept).

## Testing

`cargo test --workspace`. Release checks on real devices: `docs/e2e-checklist.md`, results in `docs/e2e-results.md`. Per-sub-spec manual checks: `docs/manual-checks/`.

## Security

Report vulnerabilities privately; see `SECURITY.md`.

## License

Meshnode is licensed under the GNU Affero General Public License v3.0 only (`AGPL-3.0-only`); see `LICENSE`. `patches/ndk-context` is a patched third-party crate under its own MIT/Apache-2.0 licence.
