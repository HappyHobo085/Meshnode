# Sub-spec 8b: packaging, release signing, upgrades and the §1 end-to-end run

> Master spec: `docs/superpowers/specs/2026-11-08-meshnode-design.md` (§1 success criteria, §2 platforms,
> §10 platform shells, §13 testing, §14 row 8b). Previous sub-spec: `2026-11-12-mesh-client-design.md` (8a).

## 1. Intent

**What the owner asked for:** start sub-spec 8b, the last row of §14: installers, release signing, and the
full §1 success-criteria run. The point is to move Meshnode from "works in tests and dev builds" to
"installed on the owner's real devices and used for real".

Decisions the owner took in brainstorming (2026-11-16):

- **Installers:** Linux `.rpm` and `.deb`, a release-signed Android APK, and a Windows MSI. **macOS `.dmg` is
  dropped from 8b** (deferred; §2's macOS row stays "best-effort", the existing macOS CI job stays as is).
- **Keys and builds are local.** Signing keys live on the owner's machines, outside the repo. No CI secrets,
  no GitHub Releases; the repo has no remote.
- **The MSI is built on the owner's Windows install** (dual-boot on the same PC).
- **E2E hardware:** this Nobara desktop (the Linux host node), the Galaxy S23 (Android host), and a Windows
  laptop (the MSI install and the client-role device for criterion 5).
- **Upgrade in place, data kept**, on every platform.
- **Compatibility across versions: detect and refuse cleanly**, no migrations and no mixed-version wire
  compatibility for now.
- **8b is done when the owner's real §1 run passes** on the three devices, not when the docs exist. The plan
  ends with the owner's run and a "fix only what blocks a step" loop.
- **Windows admin access after an MSI install goes to the installing user** (the account that ran the
  installer), so the UI works right after install.
- **Rootless containers are allowed** (podman/distrobox, Fedora and Debian images) to really exercise rpm/deb
  install, upgrade, remove and purge, locally and in CI. Nothing is installed on the host PC itself.
- **Fresh start for the first release.** No existing Meshnode data is in use (the owner said so in 8a), so the
  first release-signed APK replaces the debug APK by uninstall + install (Android refuses a different
  signature), and every device starts from fresh data. The first 8b release is the baseline that later
  upgrades must keep. *(settled from the 8a decision; the checklist says so.)*

Context: development moved from Windows (`D:\AI_Apps\Meshnode`) to Linux (`/mnt/storage/AI_Apps/Meshnode`,
same FAT32 disk) on 2026-11-16. Windows-side builds keep the old rules: `CARGO_TARGET_DIR=%LOCALAPPDATA%\
meshnode-target`, delete its `debug` folder before and after big runs, never run elevated tests on the
owner's PC.

### 1.1 Goals
1. Each of the three desktop/phone artifacts installs, upgrades in place and uninstalls cleanly, with the
   service enabled and the data kept across upgrades.
2. The APK is release-signed with a long-lived local keystore.
3. Every version mismatch (on-disk or wire) is detected and reported in plain words, never a crash loop,
   never silent corruption or silent defaults.
4. `docs/e2e-checklist.md` lets the owner run §1 criteria 1–7 on the three devices without asking anyone,
   and `docs/e2e-results.md` is the template they fill in.

### 1.2 Non-goals
- macOS packaging; Linux tarball (§2 lists it; deferred with macOS unless trivial, builder's call).
- Code signing for Windows (no Authenticode certificate: the MSI ships **unsigned**; a self-signed cert does
  not stop SmartScreen, so it buys nothing). GPG signing of the rpm/deb (local installs; deferred).
- Data migrations, rolling upgrades with mixed versions, auto-update, any download/distribution channel.
- Play Store; iOS.
- Fixing the open follow-ups of earlier sub-specs, unless an E2E step cannot pass without it (then it is
  named in the plan, not slipped in).

### 1.3 Anti-goals (a failure even if it "works")
- A signing key, keystore password or key path committed to the repo, printed in a log, or baked into an
  artifact.
- An upgrade or a plain uninstall that deletes or resets `node.key` or the store.
- A node that loops restarting forever on an incompatible store.
- A step in the E2E checklist that the implementer marks as passed without the owner having run it.

### 1.4 Done criteria
1. `rpm`, `deb`, APK and MSI build from documented one-line commands; their contents are checked by scripts
   (§7) that run without root, devices or elevation.
2. Upgrade and uninstall behaviour of each artifact is written down and covered by the checks in §7 as far as
   is possible without installing (script lint, scriptlet logic tests, MSI table inspection on Windows).
3. Incompatible-version handling (§5) has automated tests.
4. `docs/e2e-checklist.md` and `docs/e2e-results.md` exist; the manual check for 8b
   (`docs/manual-checks/sub-spec-8b.md`) covers the install/upgrade/uninstall steps that need root,
   a device, or Windows.
5. Workspace tests, fmt and clippy green; CI updated (§8); master spec back-ported (§9).
6. **The owner's §1 run passes** and is recorded in `docs/e2e-results.md` by the owner. Failures found in the
   run are fixed only as far as they block a checklist step; anything else becomes a named follow-up.

## 2. Versioning

- **One version for the product:** the `version` of `apps/meshd` (today `0.1.0`) is the source. The desktop
  app (`apps/desktop/src-tauri`) and the Android app (`apps/android/src-tauri`) carry the same version; a
  check (script + CI) fails when they differ. *(builder's call: how the single source is wired; a
  `scripts/set-version.sh <x.y.z>` that edits all three is fine.)*
- **APK:** `versionName` = the version; `versionCode` is derived from it and strictly increasing
  (e.g. `major*10000 + minor*100 + patch`; builder picks and documents the formula and its limits). Today
  Gradle reads them from `tauri.properties` with defaults `1` / `1.0`; the release build must not fall back to
  the defaults.
- **MSI:** ProductVersion = the version; a fixed, committed **UpgradeCode**; every install is a major upgrade
  (old version removed, new installed, data untouched). A **downgrade is refused** by the MSI with a clear
  message.
- **rpm/deb:** package version = the version. Downgrades are not supported (the store would refuse anyway,
  §5); the checklist says so.

## 3. Desktop packages

### 3.1 What a package contains
The desktop UI (`apps/desktop`) and `meshd`, together, plus the platform service definition. One package per
platform, named `meshnode`.

*Designer's guess, builder may change with reason:* the Tauri bundler produces the deb/rpm/MSI for the
desktop app, with `meshd` shipped alongside (e.g. as an external binary) and the service pieces added through
the bundler's packaging hooks. `bundle.active` is `false` today in `apps/desktop/src-tauri/tauri.conf.json`.
Verify every bundler key against the Tauri 2 docs before using it; if the bundler can't express something
(service install, scriptlets, MSI custom actions), a small dedicated packaging step is acceptable.

### 3.2 Linux (rpm and deb)
- `meshd` goes to `/usr/bin/meshd` (package-managed, not `/usr/local/bin`).
- The systemd unit ships as a file in `/usr/lib/systemd/system/meshnode.service`, with the same content
  `apps/meshd/src/service/linux.rs::unit_file` produces today (user/group `meshnode`, `StateDirectory`,
  `Restart=always`, data in `/var/lib/meshnode`), generated from that function at build time or checked
  against it by a test, so the two cannot drift.
- Install scriptlets: create the `meshnode` system user/group if missing; `systemctl daemon-reload`; enable
  and start the unit on first install. The installing user joining the `meshnode` group (which opens the
  admin socket to them, as `meshd service install` does with `$SUDO_USER`) is a **checklist step** for the
  owner, not a scriptlet (a package can't know the right user reliably). *(builder's call if a safe scriptlet
  form exists.)*
- **Upgrade:** stop the unit before files are replaced, restart after; the unit stays enabled. The user,
  group and `/var/lib/meshnode` are untouched.
- **Remove:** stop and disable the unit, remove files. `/var/lib/meshnode` and the user stay. **Purge**
  (`dpkg --purge`; rpm has no purge, so the checklist gives the manual command) is the only path that deletes
  data.
- **Relation to `meshd service install`/`uninstall` and the UI's "Install as service":** the package owns the
  service when installed. Those commands must detect a package-managed install and refuse with a message
  ("installed by the system package; use your package manager"), so the two never fight over the unit or the
  binary. They keep working for dev/unpackaged use.

### 3.3 Windows (MSI)
- Installs the UI and `meshd` under Program Files and registers the Windows Service exactly as
  `apps/meshd/src/service/windows.rs` does today (`LocalService`, auto start, restart on failure, the data
  directory and its DACL in `%ProgramData%\meshnode`). Reuse that code (e.g. `meshd service install` run by a
  custom action) rather than re-describing the service in WiX, unless the builder shows WiX's own service
  tables reproduce it exactly; either way one definition, not two.
- **Upgrade:** major upgrade; the service is stopped before files are replaced and started after; the data
  directory is untouched.
- **Uninstall:** stop and remove the service, remove files; `%ProgramData%\meshnode` stays. The checklist
  gives the purge step.
- Same refusal rule as Linux: `meshd service install/uninstall` and the UI button detect an MSI-managed
  install and refuse.
- **Admin access (owner decision):** today `meshd service install` records the installing user's SID in
  `meshd.toml` (`admin.windows_user_sid`, `apps/meshd/src/service/windows.rs`). A deferred custom action runs
  as SYSTEM, so the MSI must capture the SID of **the user who launched the installer** (in the client/UI
  phase, before elevation) and pass it to the install step; the result must be the same `meshd.toml` entry a
  manual install writes. On upgrade the existing entry is kept, never overwritten with SYSTEM. If the SID
  can't be determined, the install fails with a clear message rather than granting nothing or granting
  SYSTEM. The mechanism (e.g. a new `meshd service install` flag carrying the SID) is the builder's.
- Built on the owner's Windows install only. Unsigned: the checklist covers SmartScreen ("More info → Run
  anyway").

## 4. Android release signing

- **Keystore:** created once by the owner with a documented `keytool` command (the plan writes it out; the
  implementer never creates or reads a real keystore). It lives outside the repo, e.g.
  `~/.config/meshnode/release.jks`. Validity long enough for the life of the app (`keytool -validity` ≥ 25
  years). The checklist and README say: **back it up; losing it means no more updates, only uninstall and
  data loss.**
- **Wiring:** a local, gitignored properties file (e.g. `apps/android/keystore.properties`, path and
  passwords) read by `app/build.gradle.kts` into a `release` signing config. When the file is absent, the
  release build **fails with a clear message** instead of producing an unsigned or debug-signed "release" APK.
  CI keeps building the release APK **unsigned** by an explicit opt-out (an env var or Gradle property), so CI
  never needs the key.
- `apps/android/build-apk.sh --release` produces the signed APK; the script verifies the signature
  (`apksigner verify --print-certs`) and prints the certificate fingerprint (never a password).
- **Upgrade:** same package name, same key, higher `versionCode` → Android installs over the old app, keeps
  its data; the existing `MY_PACKAGE_REPLACED` receiver restarts the service.
- Hand edits in `gen/android` are allowed as before; **never** `tauri android init --force`.
- The `.gitignore` gets entries for `*.jks`, `*.keystore` and the properties file regardless of where the
  owner keeps the key.

## 5. Version mismatch: detect and refuse cleanly

The foundation exists: the store writes `schema = 1` and refuses any other value
(`crates/mesh-store/src/lib.rs`), and every wire protocol has a versioned name (`meshnode/httpfwd/1`, the
`sync`/`join` ALPNs, the `meshnode/op/1` and `meshnode/mesh/1` signing domains).

### 5.1 Release rule
Written into the master spec §13 and a short `CONTRIBUTING.md` section (new file if none):
- Any change to an on-disk format bumps that format's version (store `SCHEMA_VERSION`; and the version of
  `role.json`, `mesh.json`, `meshd.toml` if they gain one, §5.3).
- Any breaking wire change bumps the protocol's `/N`.
- No migrations for now; an older/newer format is refused.

### 5.2 Local refusal
- A node whose store, or another state file, is from an incompatible version **does not start serving** and
  reports, in plain words, what it found and what this build reads, e.g. "data was written by an
  incompatible Meshnode version (store schema 2, this build reads 1); install a matching version".
- The message reaches the owner through every surface: `meshd status` and `meshd run` output, the desktop UI's
  no-daemon/error screen, the Android notification ("Meshnode couldn't start: …"), the systemd journal and the
  Windows Event Log (start failures already go there).
- **No crash loop:** the daemon exits with a distinct exit status (or reports a distinct start error on
  Android). systemd's unit stops restarting on it (`RestartPreventExitStatus=` or equivalent); the Windows
  failure actions don't apply to it (the builder finds the mechanism that keeps the service stopped with the
  error visible); Android's restart policy (boot, alarm, watchdog, package-replaced) treats it like a
  user stop until the app is opened.
- Tests: open a store with a bumped schema → the specific error; daemon start on it → the exit status;
  the Android restart-decision function → no restart on this error.

### 5.3 Other state files
`meshd.toml`, `role.json`, `mesh.json` (and any other file the node reads at start): an unreadable,
unknown-format or invalid file gives the same clean refusal, naming the file, never silent defaults.
*Builder's call* whether these get an explicit version field now; at least, unknown or invalid content must
refuse, and the release rule (§5.1) must say how a future format change is detected.

### 5.4 Peer mismatch
- A peer running an incompatible version shows as **"incompatible version"** in `meshd status`/peer list and
  the UI member list, not just "offline", and its version is shown when known.
- Adding this mechanism is itself a format/wire change; since the first 8b release starts from fresh data
  (§1), it ships in that release and becomes part of the baseline.
- *Designer's guess:* this needs each node's product version and protocol versions to be readable without a
  successful `sync` session, for example in the replicated member record (written by each node about itself)
  or in a tiny version-probe exchange. The builder chooses the mechanism; constraints: it must work when the
  peers' `sync` protocol versions differ, it must not weaken the membership gate (only members learn
  versions), and it must itself be designed so its future versions can still be read.
- Tests: two in-process nodes with different advertised protocol versions → each reports the other as
  incompatible.

## 6. The §1 end-to-end run

- `docs/e2e-checklist.md`: one section per §1 criterion, mapped to the three devices:
  - **Desktop host:** this Nobara PC, installed from the `.rpm`.
  - **Phone host:** the S23, release APK (sideloaded).
  - **Client:** the Windows laptop, installed from the MSI, joined with a client-role invite (criterion 5:
    desktop powered off, the laptop reaches every service by forwarding through the phone).
  - Criterion 2 includes the phone on mobile data at least once; criterion 7 is the 24 h screen-off battery
    run (target ≤ 5 %/24 h, default power policy).
  - Each step: exact command or tap sequence, the expected result, and where to record it. Steps needing root,
    adb or elevation are the owner's.
- It also covers **install, upgrade-in-place (install an older build, then the newer one; data and
  membership still there) and uninstall/purge** for each artifact, and one deliberate version-mismatch check
  (§5) per surface the owner can trigger without special builds, if any (builder's call; may live in the 8b
  manual check instead).
- `docs/e2e-results.md`: the template (date, versions, device/OS, per-criterion pass/fail, measurements,
  notes). The implementer fills in nothing in it.
- The implementer **never** runs adb, installs packages on this PC, starts/stops services, elevates, or runs
  `#[ignore]` tests.

## 7. Verification without root or devices

- **rpm/deb, static:** scripts that build the packages and inspect them: file list and modes (`rpm -qlpv`,
  `dpkg -c`), scriptlets (`rpm -qp --scripts`, the deb control scripts), the unit content against
  `unit_file`, and the version. Scriptlets pass `shellcheck` where they're shell.
- **rpm/deb, in rootless containers (owner decision):** a script runs install → upgrade (older build, then
  newer) → remove → reinstall (data still there) → purge (deb) / documented manual purge (rpm) in a Fedora
  image (rpm) and a Debian image (deb), and asserts files, the `meshnode` user, the unit's enabled state and
  that `/var/lib/meshnode` survives upgrade and remove. systemd inside the container is the builder's call
  (a systemd-capable image, or asserting on `systemctl` calls via a stub where systemd can't run rootless);
  the limitation, if any, is written in the script and the manual check. The same script runs in CI. This
  is the deb's only real install test; the owner's E2E uses the rpm.
- **MSI:** on Windows, the built MSI's tables are inspected for the fixed UpgradeCode, the version, the
  service/custom-action entries (builder's tool choice). This runs on the owner's Windows install as part of
  the 8b manual check; nothing is installed by the implementer.
- **APK:** the signed-build path is tested only by the owner (the implementer has no keystore); the
  "no properties file → clear failure" path and the CI unsigned opt-out are tested by the implementer.
- **Version-mismatch:** automated tests (§5).

## 8. CI

- Build the deb and rpm on Linux and run the §7 inspection checks and the container lifecycle script.
- Build the MSI on the Windows runner (unsigned) and run the table checks if they can run there.
- `android-apk` keeps building the release APK unsigned through the explicit opt-out.
- The version-consistency check (§2).
- All CI jobs are still unrun (no remote); that remains an owner gate.

## 9. Back-ports to the master spec (at the end of 8b)
§2 (macOS dmg deferred; Linux tarball status), §10 (packages own the service; release signing), §13 (release
rule; e2e checklist exists), §14 (row 8b status), §15 (code signing for Windows, package GPG signing,
migrations, mixed-version compatibility, macOS packaging).

## 10. Left to the builder
Everything marked *builder's call* or *designer's guess* above; the exact Tauri bundler configuration; the
versionCode formula; the peer-version mechanism within §5.4's constraints; the packaging-check tooling; plan
granularity. Anything that would change an owner decision in §1 goes back to the owner.

## 11. As built (differences from this design)

Where this section and the text above disagree, this section is what was built. Master spec §2, §10, §13,
§14 and §15 carry the same changes. The MSI (plan Task 8) and CI (Task 11) are not built yet; the MSI text
in the docs is written from §3.3 and marked "(confirmed in Task 8)".

### 11.1 Versioning (§2)
- Six files carry the version: `apps/meshd/Cargo.toml` (the source), `crates/mesh-ffi/Cargo.toml` (so the
  Android node reports the product version), and `apps/{desktop,android}/src-tauri/{Cargo.toml,
  tauri.conf.json}`. `scripts/set-version.sh <x.y.z> [root]` writes them; `scripts/check-version.sh [root]`
  fails naming each file that differs.
- **versionCode** is derived by Tauri, not by us: `major*1000000 + minor*1000 + patch`. `set-version.sh`
  enforces minor and patch ≤ 999, major ≤ 2100 and not `0.0.0`. A release build fails when
  `tauri.properties` lacks the versionCode or versionName (debug keeps Gradle's defaults).

### 11.2 Linux packages (§3.2)
- Built by the Tauri bundler through `scripts/build-desktop-packages.sh`. `meshd` is an external binary
  passed with `--config` (not in `tauri.conf.json`: tauri-build would then need a staged `meshd` for every
  plain `cargo build` of the desktop app). The packaging files are staged with explicit modes because the
  FAT32 checkout reports every file as 0755. Artifacts: `Meshnode-<v>-1.x86_64.rpm`, `Meshnode_<v>_amd64.deb`;
  package name `meshnode`; marker `/usr/share/meshnode/packaged`.
- **Upgrade order:** the scriptlets do not stop the unit before the files are replaced; postinst runs
  `systemctl try-restart` (Debian's `dh_installsystemd` default). dpkg and rpm replace files by rename, so
  the old daemon keeps its inode until the restart. End state as designed: new binary running, unit
  enabled, data and user untouched.
- deb: postinst gets `configure <old>` both on upgrade and on a reinstall after `remove`, so preinst marks a
  real install in `/run/meshnode-package-install`, which postinst consumes (`enable --now`).
- **Minimum glibc 2.39** (Debian 13+, Ubuntu 24.04+, Fedora 40+): Rust std in the packaged binaries uses
  `pidfd_spawnp`. Declared as `libc6 (>= 2.39)` (deb) and `libc.so.6(GLIBC_2.39)(64bit)` (rpm; the bundler's
  rpm `depends` take no version, the soname capability is equivalent).
- The packages are unsigned; Fedora's rpm needs `--nosignature` (`rpm -i/-U`). rpm purge is manual:
  `rpm -e meshnode && userdel meshnode && rm -rf /var/lib/meshnode`. `userdel` keeps the `meshnode` group
  while it has members (the owner, after joining it), so the checklist adds `groupdel meshnode` for that case.
- The container lifecycle test runs **real systemd** (no stub, no limitation): Fedora 41 for the rpm,
  Debian 13 for the deb (Debian 12's glibc 2.36 is now refused at install).

### 11.3 Service install and the MSI (§3.3)
- `meshd service install --from-package [--admin-user <ACCOUNT>]` and `meshd service uninstall
  --from-package` are hidden flags for package scripts; `--admin-user` requires `--from-package` and is
  Windows-only. Without `--from-package`, install and uninstall refuse while the package marker exists:
  "Meshnode is installed by the system package; use your package manager (or the Windows installer) instead".
- An existing `admin.windows_user_sid` is kept on every install, the manual reinstall included; the account
  lookup accepts only a user SID. Changing it later: `meshd config set admin.windows_user_sid <SID>`, then a
  service restart.

### 11.4 Android signing (§4)
- `apps/android/build-apk.sh --release` reads `MESHNODE_KEYSTORE_PROPERTIES`, else
  `apps/android/keystore.properties` (template `keystore.properties.example`; `storeFile` may be absolute,
  `~/…` or relative to the properties file). The "no key" failure is raised when the release build runs
  (task `:app:checkMeshnodeRelease`), not at Gradle configuration, so debug builds and tests need no key.
- CI's opt-out is `--release --unsigned`, which sets the Gradle property `meshnodeUnsignedRelease=true`.
- Gradle's configuration cache (on in `gradle.properties`) stores the signing passwords in its encrypted
  cache under `gen/android/.gradle/`.

### 11.5 Version mismatch (§5)
- `meshd run` exits **4** on incompatible data; the unit has `RestartPreventExitStatus=4`. The reason is
  written to `<data>/start-error.txt` (removed by the next successful start, or by any other start
  failure). `meshd status` keeps exit 3 and prints `not running: <reason>`; the desktop app's no-daemon
  screen shows "Meshnode refused to start".
- **Windows:** the service stops with exit 0 after the Event Log entry, because the failure actions apply to
  any non-zero exit. `start-error.txt` is made readable by Authenticated Users so a non-elevated `meshd
  status` and the app can read it (uncompiled on Windows until Task 8).
- **Android:** `FfiError::Incompatible` sets a persisted flag; boot, alarm, watchdog, sticky restart,
  swipe-away and package-replaced do not start the service while it is set; opening the app or Start clears it.
- **§5.3:** no version field was added to the state files. An unparsable `meshd.toml` (it refuses unknown
  keys) counts as incompatible; so do unreadable or unknown `role.json`, `mesh.json` and `invites.json`.
- **§5.4 mechanism:** a gated probe on the unversioned, frozen ALPN `meshnode/version`; the reply is JSON
  `PeerVersion { product, protocols, store_schema }` that later versions extend with fields. Compatible
  means the same `sync` `/N` and store schema; a peer that refuses our ALPNs is detected from the TLS
  `no_application_protocol` alert. It shows as "incompatible version (x)" in `meshd members` (`meshd
  status` has no member list) and on the UI's device card.
- **Gap:** no dev command bumps an existing store's schema (`mesh_store::test_hooks::set_schema` is
  test-only). The manual check triggers the same refusal through an unknown `meshd.toml` key.
- macOS (dropped): launchd's `KeepAlive` would still restart on exit 4.

### 11.6 Fixes outside the plan
Before the packaging work, flaky or failing workspace tests were fixed so the workspace is reliably green
(plan Tasks 0b–0g). Three were product fixes: `set_info`/`set_tags_of` wait until the member list shows
the change (0b); a peer with a live session is dialled by its bare id (0c); the data-dir lock is released
on drop, so a child process's copy of the descriptor no longer keeps it after shutdown, which restarts on
upgrade depend on (0d). The rest were test-only.
