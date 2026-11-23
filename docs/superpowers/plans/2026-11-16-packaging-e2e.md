# Sub-spec 8b: Packaging, Signing, Upgrades and §1 E2E — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship upgradeable rpm/deb/MSI/APK installers, release-sign the APK, refuse version mismatches cleanly, and give the owner a §1 checklist that their real run passes.

**Architecture:** Version mismatches become typed errors (`StoreError::IncompatibleSchema`, `NodeError::Incompatible`) that every shell maps to one non-restarting refusal (meshd exit status 4, Android `FfiError::Incompatible`). Peers learn each other's versions over a tiny JSON probe (`meshnode/version`) behind the membership gate. Desktop packages come from the Tauri bundler of `apps/desktop` with `meshd` as an external binary; the package owns the service and `meshd service install/uninstall` refuse when it does.

**Tech Stack:** Rust 2024, iroh, redb, Tauri 2 bundler (deb/rpm/WiX MSI), Gradle/Kotlin, podman (rootless), Svelte 5 + ts-rs.

**Spec:** `docs/superpowers/specs/2026-11-16-packaging-e2e-design.md` (read it first; section refs below are to it).

## Global Constraints

- Branch `feat/packaging-e2e`. Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. NEVER push (no remote).
- Linux build env: the repo is on FAT32 (`/mnt/storage`), so `export CARGO_TARGET_DIR=$HOME/.cache/meshnode-target CARGO_INCREMENTAL=0`; use `-j 4`.
- Windows build env (Tasks 8, 12 only, on the owner's Windows install): `CARGO_TARGET_DIR=%LOCALAPPDATA%\meshnode-target`, delete its `debug` before and after big runs; git needs `-c safe.directory=<meshnode-checkout>`.
- Never: install packages on the host PC, start/stop OS services, elevate/sudo, adb or touch a device, run `#[ignore]` tests, `tauri android init --force`, run Aegis src-tauri's full `cargo test`.
- Never create, read or print a real keystore, key password or key path. No secrets in repo, logs or artifacts.
- Product version source: `apps/meshd/Cargo.toml` `version` (today `0.1.0`); desktop and Android `Cargo.toml` + `tauri.conf.json` must equal it.
- Incompatible-data exit status of `meshd run`: **4** (existing: 1 failure, 2 usage, 3 daemon not running).
- macOS packaging, Authenticode, rpm/deb GPG signing, migrations: out of scope.
- Owner decisions in spec §1 are fixed; anything that would change one goes back to the owner.

## Review Focus

1. **Upgrade with the service running** (rpm `$1 == 2`, deb `upgrade`, MSI major upgrade): expected — service stopped, files replaced, service running again, still enabled, data and `meshd.toml` untouched. Pinned by Task 7's container lifecycle and Task 8's MSI table check + manual check.
2. **Plain remove vs purge** — `rpm -e`, `apt remove`, MSI uninstall must leave `/var/lib/meshnode` / `%ProgramData%\meshnode` and the `meshnode` user; only `dpkg --purge` deletes data. Pinned by Task 7 (`remove keeps data`, `purge deletes data`).
3. **Incompatible store found by the service at boot** — expected: one clear message in journal/Event Log/notification, no restart loop. Pinned by Task 3 (`unit_prevents_restart_on_incompatible`, `incompatible_start_writes_start_error`, Kotlin `noAutoRestartAfterIncompatible`).
4. **Manual `meshd service install` on a package-managed machine** (old habit, or the UI button) — expected: refused with "installed by the system package…", nothing changed. Pinned by Task 5 (`install_refused_when_package_managed`).
5. **Release APK built without the keystore properties** — expected: Gradle fails with a message naming the missing file, never a debug-signed or unsigned "release". Pinned by Task 9 (`release build without keystore fails`).

---

### Task 0: Linux toolchain (owner gate + verification)

**Files:** none (records results in the SDD ledger `.superpowers/sdd/2026-11-16-packaging-e2e/progress.md`, gitignored).

- [ ] **Step 1: Check the tools** — run: `for t in podman rpm rpmbuild keytool cargo; do command -v $t; done; cargo tauri --version; echo $ANDROID_HOME $NDK_HOME $JAVA_HOME; rustup target list --installed`. Record what is missing.
- [ ] **Step 2: Ask the owner** to install what is missing (it is outside the workspace): Tauri CLI (`cargo install tauri-cli --version "^2" --locked`, the CI-pinned version), Android SDK + NDK 27.2.12479018 + build-tools 36.0.0 + JDK 17, `cargo-ndk`, the `aarch64-linux-android` Rust target. Tasks 3 (Kotlin part), 9 and 11's Android jobs wait for this; Tasks 1–2, 4–7 do not.
- [ ] **Step 3: Baseline** — run `cargo test --workspace -j 4 2>&1 | grep -E "^test result|FAILED"`. Expected: all ok (772 on Windows; record the Linux count). Any Linux-only failure is recorded and reported to the owner before Task 1, not fixed silently.

### Task 1: One product version

**Files:**
- Create: `scripts/set-version.sh`, `scripts/check-version.sh`
- Modify: `apps/desktop/src-tauri/tauri.conf.json`, `apps/android/src-tauri/tauri.conf.json` (only if they diverge)

**Interfaces:**
- Produces: `scripts/check-version.sh` → exit 0 and prints `0.1.0` when `apps/meshd/Cargo.toml`, `apps/desktop/src-tauri/{Cargo.toml,tauri.conf.json}`, `apps/android/src-tauri/{Cargo.toml,tauri.conf.json}` agree; exit 1 naming each file that differs. `scripts/set-version.sh <x.y.z>` rewrites all five and refuses a non-`x.y.z` argument.

- [ ] **Step 1: Test** — in `$TMPDIR`, copy the five files into a fake tree (the scripts take an optional root dir argument), change one version, run `check-version.sh <root>`. Expected: exit 1, message names that file. Restore → exit 0.
- [ ] **Step 2: Implement both scripts** (bash, `set -euo pipefail`, LF). Only touch the package `version` line in each Cargo.toml (`[package]` section), the top-level `"version"` in each tauri.conf.json.
- [ ] **Step 3: APK versionCode** — read how Tauri 2 derives `tauri.android.versionCode` from `version` (Tauri docs: `bundle.android.versionCode`; verify, don't assume). Document the formula and its limits in a comment in `scripts/set-version.sh`. If Tauri does not derive it, `set-version.sh` sets `bundle.android.versionCode = major*10000 + minor*100 + patch` (minor, patch ≤ 99, enforced).
- [ ] **Step 4: Run** `scripts/check-version.sh` on the repo → `0.1.0`, exit 0.
- [ ] **Step 5: Commit** `build: single product version with set/check scripts`.

### Task 2: Typed incompatible-data errors (store + state files)

**Files:**
- Modify: `crates/mesh-store/src/error.rs`, `crates/mesh-store/src/lib.rs:141-147`
- Modify: `crates/mesh-core/src/error.rs`, `crates/mesh-core/src/local.rs` (`read_genesis`, `read_own_role`, `Invites::load`, `bad_file`)
- Modify: `crates/mesh-host/src/daemon.rs` (`DaemonError`), `crates/mesh-host/src/config.rs` (`ConfigError`)
- Test: `crates/mesh-store/tests/` (existing open tests file), `crates/mesh-core/tests/node.rs`, `crates/mesh-host/tests/daemon.rs`

**Interfaces:**
- Produces:
  - `StoreError::IncompatibleSchema { found: u64, supported: u64 }`, Display: `"data was written by an incompatible Meshnode version (store schema {found}, this build reads {supported}); install a matching version"`.
  - `pub const SCHEMA_VERSION: u64` re-exported from `mesh_store` (today `pub(crate)` in `tables.rs`).
  - `NodeError::Incompatible { file: String, detail: String }`, Display `"{file}: {detail}"`; a store `IncompatibleSchema` surfaces as `NodeError::Incompatible { file: "store", detail: <store message> }`; an unreadable/unknown `mesh.json`, `role.json`, `invites` file as `NodeError::Incompatible { file: "<name>", detail }` (replaces today's `bad_file` result for these files).
  - `DaemonError::is_incompatible(&self) -> bool` — true for `Node(NodeError::Incompatible{..})` and `Config(ConfigError::Parse{..})` (invalid `meshd.toml`; already `deny_unknown_fields`).
  - Test hook (feature `test-hooks` on mesh-store): `mesh_store::test_hooks::set_schema(path: &Path, schema: u64)` rewrites the meta `schema` of a closed store.

- [ ] **Step 1: Failing tests**
  - mesh-store `open_refuses_newer_schema`: create a store, close, `set_schema(path, SCHEMA_VERSION + 1)`, reopen → `matches!(e, StoreError::IncompatibleSchema { found, supported } if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION)`; same for `found = 0` (older).
  - mesh-core `unknown_role_file_is_incompatible`: write `role.json` = `{"role":"overlord"}` in a node dir → `read_own_role` → `NodeError::Incompatible { file, .. }` with `file == "role.json"`; same for garbage `mesh.json` (`file == "mesh.json"`).
  - mesh-host `start_on_incompatible_store_is_incompatible`: start a daemon on a dir, stop, bump schema, start again → `Err(e)` with `e.is_incompatible()` and `e.to_string().contains("store schema")`.
- [ ] **Step 2: Run** `cargo test -p mesh-store -p mesh-core -p mesh-host --features test-hooks <names>` → FAIL (variants missing).
- [ ] **Step 3: Implement** the variants and mappings above. Keep `StoreError::Corrupt` for real corruption; only the schema check changes.
- [ ] **Step 4: Run** the three crates' full tests → PASS; then `cargo test --workspace -j 4` → all ok.
- [ ] **Step 5: Commit** `feat(store,core,host): typed incompatible-data errors`.

### Task 3: Refuse cleanly on every surface, no restart loop

**Files:**
- Modify: `apps/meshd/src/main.rs`, `apps/meshd/src/commands.rs` (`CliError`, status), `apps/meshd/src/output.rs`
- Modify: `apps/meshd/src/service/linux.rs` (`unit_file`), `apps/meshd/src/service/windows.rs` (dispatcher stop path)
- Modify: `crates/mesh-host/src/paths.rs` (start-error file), `crates/mesh-host/src/daemon.rs`
- Modify: `crates/mesh-ffi/src/host.rs` (`FfiError`), Kotlin `RestartPolicy.kt`, `Prefs.kt`, `MeshService.kt`, the callers of `RestartPolicy.shouldStart` (`BootReceiver.kt`, `AlarmReceiver.kt`, `RestartReceiver.kt`, watchdog, `MainActivity.kt`), committed UniFFI bindings via `apps/android/gen-bindings.sh`
- Modify: `crates/mesh-ui/src/` no-daemon state DTO + `apps/ui` no-daemon screen
- Test: `apps/meshd/tests/` (new `incompatible.rs` if no suitable file), Kotlin JVM test for `RestartPolicy`, `apps/ui` vitest for the screen

**Interfaces:**
- Consumes: `DaemonError::is_incompatible`, `NodeError::Incompatible` (Task 2).
- Produces:
  - `CliError::incompatible(message) -> CliError` with `code: 4`; `meshd run` returns it when the daemon start error `is_incompatible()`.
  - `unit_file` adds the line `RestartPreventExitStatus=4` in `[Service]`.
  - `mesh_host::paths::START_ERROR_FILE = "start-error.txt"` in the data dir: written (atomically) with the error message when a start fails with `is_incompatible()`, deleted on a successful start. `meshd status` when the daemon is not running prints `not running: <first line of start-error.txt>` if present; the desktop UI no-daemon screen shows it (new optional field `startError: string | null` on the no-daemon DTO; name it in ts-rs output).
  - Windows: a pure `fn service_exit(err: &DaemonError) -> (ServiceExitCode, bool /*log_event*/)`; for incompatible errors the service reports `Stopped` with `ServiceExitCode::ServiceSpecific(4)` and writes the message to the Event Log. Verify (Microsoft docs) whether failure actions with `set_failure_actions_on_non_crash_failures(true)` fire on a service-specific exit; if they do, the incompatible path instead reports exit code 0 after logging, and the plan comment records why.
  - `FfiError::Incompatible { message: String }` (Kotlin `FfiException.Incompatible`).
  - Kotlin: `RestartPolicy.shouldStart(trigger, userStopped, refusedIncompatible, running)`; automatic triggers (`BOOT`, `PACKAGE_REPLACED`, `TASK_REMOVED_ALARM`, `WATCHDOG`) return false while `refusedIncompatible`; `APP_OPEN` and `USER_START` may start; `clearsRefusedIncompatible(trigger) = trigger in {APP_OPEN, USER_START}`. `Prefs.refusedIncompatible` persisted; `MeshService` sets it on `FfiException.Incompatible` and keeps the existing failed-start notification ("Meshnode couldn't start: <message> — tap to open").

- [ ] **Step 1: Failing tests**
  - `unit_prevents_restart_on_incompatible` (linux.rs unit tests): `unit_file(..)` contains `"RestartPreventExitStatus=4\n"` inside `[Service]`.
  - `meshd_run_exits_4_on_incompatible_store` (apps/meshd/tests): create a node dir via the CLI/test helper, bump schema (Task 2 hook), run `meshd run --data <dir>` as a child process with a timeout → exit status 4, stderr contains `"store schema"`.
  - `incompatible_start_writes_start_error`: same setup through `mesh-host` → `start-error.txt` exists with the message; fix the schema back, start → file gone.
  - Windows pure fn `service_exit_incompatible_stops` (compiled on all platforms if `ServiceExitCode` is, else `#[cfg(windows)]`).
  - Kotlin `noAutoRestartAfterIncompatible`: for each automatic trigger, `shouldStart(t, false, true, false) == false`; `shouldStart(APP_OPEN, false, true, false) == true`; `clearsRefusedIncompatible(USER_START) == true`, `(BOOT) == false`.
  - UI vitest: no-daemon screen with `startError: "data was written by an incompatible…"` renders that text.
- [ ] **Step 2: Run** each → FAIL.
- [ ] **Step 3: Implement** per Interfaces. Regenerate ts types (`scripts/gen-ts-types.sh`) and Kotlin bindings (`apps/android/gen-bindings.sh`).
- [ ] **Step 4: Run** `cargo test --workspace -j 4`, `scripts/gen-ts-types.sh --check`, `npm --prefix apps/ui test`, and (after Task 0's gate) the Kotlin JVM tests the `android` CI job runs → all PASS. If the Android toolchain is not yet installed, record "Kotlin tests not run" in the ledger and run them before Task 9.
- [ ] **Step 5: Commit** `feat: refuse incompatible data cleanly on every surface`.

### Task 4: Peer version probe and "incompatible version" status

**Files:**
- Create: `crates/mesh-proto/src/version.rs` (`PeerVersion`), `crates/mesh-core/src/version.rs` (probe handler + client)
- Modify: `crates/mesh-core/src/node.rs` (mount behind `Gate`, `MemberStatus`), `crates/mesh-core/src/peers/slot.rs` (on sync dial failure), `crates/mesh-core/src/transport.rs`
- Modify: `crates/mesh-admin/src/proto.rs` (`MemberInfoView`), `apps/meshd/src/output.rs`, `crates/mesh-ui/src/dto.rs` (`MemberDto`), `apps/ui/src/routes/DeviceCard.svelte`, `apps/ui/src/routes/fixtures.ts`
- Test: `crates/mesh-core/tests/version.rs` (new), mesh-proto unit tests, UI vitest

**Interfaces:**
- Produces:
  - `pub const ALPN_VERSION: &[u8] = b"meshnode/version";` — deliberately **unversioned** and frozen: its payload is JSON so future fields can be added without breaking old readers.
  - `PeerVersion { product: String, protocols: BTreeMap<String, u32>, store_schema: u64 }`, serde JSON, unknown fields ignored (no `deny_unknown_fields`). `PeerVersion::current(product: &str)` takes `product` from the new `NodeConfig.product_version: String` (set by mesh-host from the product version, so the probe reports the product, not the mesh-core crate version), `protocols = {"sync":1,"join":1,"httpfwd":1}` from the ALPN constants, `store_schema = mesh_store::SCHEMA_VERSION`.
  - `PeerVersion::compatible_with(&self, other: &PeerVersion) -> bool` — equal `protocols["sync"]` and equal `store_schema`.
  - Wire: the dialer opens a bi-stream, the acceptor writes its `PeerVersion` JSON (≤ 4 KiB) and closes; mounted behind the existing membership `Gate` (non-members refused exactly like `sync`).
  - `MemberStatus.version: Option<PeerVersion>` and `MemberStatus.incompatible: bool`; `MemberInfoView` and `MemberDto` gain `version: Option<String>` (product) and `incompatible: bool`.
  - Test hook (`test-hooks`): `NodeConfig.sync_alpn_override: Option<Vec<u8>>` and `protocols_override` so a test node can advertise `sync/2`.
- Behaviour: when dialing `ALPN_SYNC` to a member fails because the peer doesn't offer that ALPN (verify how iroh/quinn reports a failed ALPN negotiation; match that error, not a string), the slot dials `ALPN_VERSION`, records the result, and backs off at its normal retry interval. Successful sync sessions also fetch the version once per connection, so `version` is known for compatible peers too.

- [ ] **Step 1: Failing tests**
  - mesh-proto `peer_version_ignores_unknown_fields`: JSON with an extra `"future": 1` decodes; `compatible_with` true for equal sync+schema, false when `sync` differs, false when `store_schema` differs.
  - mesh-core `incompatible_peer_is_reported` (in-process, relay/discovery off, like existing tests): node A normal, node B member with `sync_alpn_override = b"meshnode/sync/2"` and protocols `sync:2` → within the test timeout A's `MemberStatus` for B has `online == false`, `incompatible == true`, `version.unwrap().protocols["sync"] == 2`; and B's view of A has `incompatible == true`.
  - mesh-core `version_probe_refuses_non_member`: a non-member endpoint dialing `ALPN_VERSION` is closed with `NOT_MEMBER`.
  - mesh-core `compatible_peer_has_version`: two normal nodes → each sees the other's `version.product == <configured product>` and `incompatible == false`.
  - UI vitest: `DeviceCard` with `incompatible: true, version: "0.2.0"` shows `Incompatible version (0.2.0)` instead of the offline label.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** per Interfaces; `meshd status` prints `incompatible version (<product>)` for such members.
- [ ] **Step 4: Run** `cargo test --workspace -j 4`, `scripts/gen-ts-types.sh --check`, `npm --prefix apps/ui test` → PASS.
- [ ] **Step 5: Commit** `feat(core): peer version probe; report incompatible peers`.

### Task 5: Package-managed service detection; MSI admin user

**Files:**
- Modify: `apps/meshd/src/service/mod.rs`, `linux.rs`, `windows.rs`, `apps/meshd/src/cli.rs`, `apps/meshd/src/commands.rs`
- Test: unit tests in those files

**Interfaces:**
- Produces:
  - `service::package_marker() -> Option<PathBuf>`: Linux `/usr/share/meshnode/packaged`; Windows `<install_dir>\packaged` (`install_dir()` exists in windows.rs). Pure `fn is_package_managed(marker_exists: bool, from_package: bool) -> PackageGate` with `PackageGate::{Allow, Refuse}`.
  - `meshd service install|uninstall` refuse with `CliError::failed("Meshnode is installed by the system package; use your package manager (or the Windows installer) instead")` when the marker exists, unless the hidden flag `--from-package` is given (used only by package scripts / MSI custom actions; `#[arg(hide = true)]`).
  - `meshd service install --from-package --admin-user <ACCOUNT>` (Windows only): resolves `ACCOUNT` (as given by MSI `[LogonUser]`, `DOMAIN\user` or `user`) to a SID with `LookupAccountNameW` and writes it as `admin.windows_user_sid` in `meshd.toml` — the same entry today's install writes from the current token (`windows.rs:257-264`). Unresolvable account → install fails with `"cannot resolve the installing user <ACCOUNT>"`. If `meshd.toml` already has `admin.windows_user_sid`, it is kept (upgrade), never overwritten.
  - Linux `installed()` also finds the package unit at `/usr/lib/systemd/system/meshnode.service` (data dir parsed by the existing `data_dir_from_unit`).
  - On Linux `install --from-package` is not used (scriptlets call `systemctl` directly); `uninstall --from-package` likewise unused. Keep the flag cross-platform for symmetry.
- [ ] **Step 1: Failing tests** — `install_refused_when_package_managed`: `is_package_managed(true, false) == Refuse`, `(true, true) == Allow`, `(false, false) == Allow`; command-level test with the marker path injected (make the marker path a parameter of an inner fn) → error text above, nothing written. `admin_user_kept_on_upgrade` (pure fn over a `HostConfig`): existing SID stays. `linux_installed_finds_package_unit` with a temp root.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** Windows code must at least `cargo check --target x86_64-pc-windows-msvc` if the target is installed; otherwise record "Windows compile checked in Task 8".
- [ ] **Step 4: Run** `cargo test -p meshd` → PASS. **Step 5: Commit** `feat(meshd): package-managed service detection; --admin-user for the MSI`.

### Task 6: Linux rpm and deb

**Files:**
- Create: `apps/desktop/src-tauri/packaging/linux/meshnode.service` (committed, generated), `packaging/linux/packaged` (empty marker), `packaging/linux/scripts/{preinst,postinst,prerm,postrm}.sh`, `scripts/build-desktop-packages.sh`, `scripts/inspect-linux-packages.sh`
- Modify: `apps/desktop/src-tauri/tauri.conf.json` (`bundle`), `apps/meshd/src/service/linux.rs` (test comparing the committed unit), `.gitignore` (`apps/desktop/src-tauri/binaries/`)

**Interfaces:**
- Consumes: `unit_file` incl. `RestartPreventExitStatus=4` (Task 3); marker path `/usr/share/meshnode/packaged` (Task 5).
- Produces: `scripts/build-desktop-packages.sh` → builds `meshd` release, stages it as the Tauri external binary (`binaries/meshd-<target-triple>`), runs `cargo tauri build --bundles deb,rpm` on Linux (`msi` on Windows), prints artifact paths. Package name `meshnode`, version from Task 1.
- Bundle config (verify each key in Tauri 2 docs before use; documented ones: `bundle.externalBin`, `bundle.linux.deb.files`, `bundle.linux.rpm.files`, `bundle.linux.{deb,rpm}.{preInstallScript,postInstallScript,preRemoveScript,postRemoveScript}`): `bundle.active = true`, targets per platform; files map the unit to `/usr/lib/systemd/system/meshnode.service` and the marker to `/usr/share/meshnode/packaged`; `meshd` must land at `/usr/bin/meshd` (check where Tauri puts external binaries on Linux; if not `/usr/bin`, map it via `files`).
- Scriptlets — one shared set; each normalises its argument first (rpm passes a count: `$1` = 1 install, 2 upgrade for pre/post-install; 0 remove, 1 upgrade for pre/post-remove; deb passes `install|upgrade|remove|purge|…`):
  - preinst: create system group/user `meshnode` if missing (`useradd --system --no-create-home --home-dir /var/lib/meshnode --shell /usr/sbin/nologin --user-group meshnode` or `adduser --system --group` on Debian; use whichever exists).
  - postinst: `systemctl daemon-reload`; first install → `systemctl enable --now meshnode.service`; upgrade → `systemctl try-restart meshnode.service`. Skip `systemctl` calls gracefully when systemd is not running (e.g. container without systemd, `[ -d /run/systemd/system ]`).
  - prerm: remove (not upgrade) → `systemctl disable --now meshnode.service`; upgrade → nothing (postinst restarts).
  - postrm: `systemctl daemon-reload`; deb `purge` only → `rm -rf /var/lib/meshnode` and remove the `meshnode` user/group. Never delete data otherwise.
- [ ] **Step 1: Failing test** — `committed_unit_matches_unit_file` in `linux.rs`: `include_str!` of the committed unit == `unit_file(Path::new("/usr/bin/meshd"), Path::new("/var/lib/meshnode"))`.
- [ ] **Step 2: Run** → FAIL (file missing). **Step 3: Write** the unit (generate from `unit_file`, e.g. a `meshd service print-unit` hidden subcommand or the test's failure output), scriptlets, config, build script.
- [ ] **Step 4: Inspect** — `scripts/inspect-linux-packages.sh <rpm> <deb>`: `rpm -qlpv`, `rpm -qp --scripts`, `rpm -qp --queryformat '%{VERSION}'`; `dpkg-deb -c`/`-I` and shellcheck run inside rootless podman (`docker.io/library/debian:12`, `docker.io/koalaman/shellcheck:stable`) since neither is on the host. Assert: `/usr/bin/meshd`, unit, marker present; version == `scripts/check-version.sh` output; shellcheck clean. Run it → PASS. If the sandbox blocks image pulls, report it (owner can allow the registry with `/sandbox`); do not bypass.
- [ ] **Step 5: Commit** `build(linux): rpm and deb packages own the meshnode service`.

### Task 7: Container lifecycle test for rpm/deb

**Files:**
- Create: `scripts/test-linux-packages.sh`, `packaging/test/` helper (if needed)

**Interfaces:**
- Consumes: two builds of each package — the current one, and an "older" one made by building with `scripts/set-version.sh 0.0.1` in a scratch git worktree under `$TMPDIR` (never committing the change).
- Produces: `scripts/test-linux-packages.sh <old.rpm> <new.rpm> <old.deb> <new.deb>` — exit 0 when all assertions pass; runs in `registry.fedoraproject.org/fedora:41` (rpm) and `docker.io/library/debian:12` (deb), rootless podman. Prefer a systemd-capable run (`podman run --systemd=always … /sbin/init`, after installing systemd in Debian's image); if systemd can't run rootless here, replace `systemctl` with a recording stub and assert on recorded calls, and write that limitation at the top of the script and in the 8b manual check.
- Assertions, in order: install old → user `meshnode` exists, `/usr/bin/meshd` reports 0.0.1, unit enabled (or stub saw `enable --now`); write a sentinel file into `/var/lib/meshnode`; upgrade to new → version new, unit still enabled, service restarted (or stub saw `try-restart`, no `disable`), sentinel present; `remove keeps data` (`rpm -e` / `apt-get remove`) → binary gone, sentinel and user present; reinstall → sentinel present; `purge deletes data` (deb only, `dpkg --purge`) → `/var/lib/meshnode` and user gone. rpm: assert the documented manual purge command from the checklist works.
- [ ] **Step 1:** Write the script with the assertions first; run with the packages → FAIL where scriptlets are wrong.
- [ ] **Step 2:** Fix scriptlets (Task 6 files) until PASS.
- [ ] **Step 3: Commit** `test(linux): container install/upgrade/remove/purge lifecycle`.

### Task 8: Windows MSI (executed on the owner's Windows install)

**Files:**
- Create: `apps/desktop/src-tauri/packaging/windows/service.wxs` (fragment), `packaging/windows/packaged` marker, `scripts/inspect-msi.ps1`
- Modify: `apps/desktop/src-tauri/tauri.conf.json` (`bundle.windows.wix`: fixed `upgradeCode` (generate one GUID now, commit it), `fragmentPaths`, `componentRefs`)

**Interfaces:**
- Consumes: `meshd service install --from-package --admin-user [LogonUser]` / `meshd service uninstall --from-package` (Task 5); marker in the install dir.
- Produces: MSI that installs UI + `meshd.exe` + marker under Program Files; deferred, no-impersonate custom actions: on install (not upgrade-with-existing-service) → `meshd service install --from-package --admin-user "[LogonUser]"`; on upgrade → stop service before file replacement, start after; on uninstall (not upgrade) → `meshd service uninstall --from-package` (keeps `%ProgramData%\meshnode`). Downgrade refused with a message (Tauri's template has a downgrade check; verify). Verify `LogonUser` is the launching user, not SYSTEM, in the deferred action's `CustomActionData` (pass it via a property set in the UI/immediate sequence).
- [ ] **Step 1:** `scripts/inspect-msi.ps1 <msi>` (Windows Installer COM `WindowsInstaller.Installer` → `OpenDatabase`, read `Property` and `CustomAction` tables): asserts `UpgradeCode` == committed GUID, `ProductVersion` == product version, the custom actions exist with `--from-package`. Write it first; run on a Tauri-built MSI → FAIL.
- [ ] **Step 2:** Implement fragment + config; `scripts/build-desktop-packages.sh` on Windows; inspect → PASS. `cargo test -p meshd` on Windows → PASS (covers Task 5's Windows code).
- [ ] **Step 3:** Nothing is installed on the owner's PC. Install/upgrade/uninstall on the laptop is in the manual check (Task 10).
- [ ] **Step 4: Commit** `build(windows): MSI with service, admin user and major upgrade`.

### Task 9: Android release signing and versionCode

**Files:**
- Modify: `apps/android/src-tauri/gen/android/app/build.gradle.kts`, `apps/android/build-apk.sh`, `.gitignore`
- Create: `apps/android/keystore.properties.example`

**Interfaces:**
- Produces:
  - Properties file lookup: env `MESHNODE_KEYSTORE_PROPERTIES` if set, else `apps/android/keystore.properties` (gitignored). Keys: `storeFile`, `storePassword`, `keyAlias`, `keyPassword` (`.example` has placeholders only).
  - `release` build type uses `signingConfigs.create("release")` from that file. Missing file and no opt-out → Gradle fails with `"Release signing: <path> not found. Copy apps/android/keystore.properties.example, or pass -PmeshnodeUnsignedRelease=true for an unsigned build (CI)."`.
  - Opt-out: Gradle property `meshnodeUnsignedRelease=true` → unsigned release APK (CI only).
  - Release must not use the `versionCode`/`versionName` defaults (`"1"`/`"1.0"`): fail if `tauri.properties` lacks them.
  - `build-apk.sh --release` → after the build, runs `apksigner verify --print-certs` on the APK and prints the SHA-256 certificate digest; `--release --unsigned` passes the opt-out and skips verification.
  - `.gitignore`: `*.jks`, `*.keystore`, `apps/android/keystore.properties`.
- [ ] **Step 1: Test `release build without keystore fails`** — with no properties file and no opt-out: `apps/android/build-apk.sh --release` → non-zero and the message above. With `--unsigned` → APK built, `apksigner verify` reports it unsigned. (Needs Task 0's toolchain.)
- [ ] **Step 2: Implement.** **Step 3:** Run both → expected results. The signed path is the owner's (manual check): implementer never creates a keystore.
- [ ] **Step 4: Commit** `build(android): release signing from a local keystore; CI opt-out`.

### Task 10: Docs — release rule, checklists, manual check, back-ports

**Files:**
- Create: `CONTRIBUTING.md` (or a section if one exists), `docs/e2e-checklist.md`, `docs/e2e-results.md`, `docs/manual-checks/sub-spec-8b.md`
- Modify: `README.md` (build/install section), master spec §2, §10, §13, §14, §15; 8b spec gets §11 "As built"

**Content requirements:**
- CONTRIBUTING release rule — spec §5.1 verbatim intent, naming `mesh_store::SCHEMA_VERSION`, the `/N` ALPNs, and that `meshnode/version` is frozen and JSON-extensible.
- `docs/e2e-checklist.md` — spec §6: one section per §1 criterion 1–7, devices (Linux desktop `.rpm`, phone release APK, Windows laptop MSI as client), exact commands/taps, expected result, where to record. Preamble: fresh start (uninstall the debug APK first; it deletes its data), keystore creation command (`keytool -genkeypair -v -keystore ~/.config/meshnode/release.jks -alias meshnode -keyalg RSA -keysize 4096 -validity 10000`) + **back it up**, `sudo usermod -aG meshnode $USER` + re-login, SmartScreen "More info → Run anyway", rpm manual purge command. Install/upgrade-in-place/uninstall per artifact.
- `docs/e2e-results.md` — template only (date, versions, devices/OS, per-criterion pass/fail, battery measurement, notes). Implementer fills in nothing.
- `docs/manual-checks/sub-spec-8b.md` — what needs root/device/Windows: MSI install/upgrade/uninstall on the laptop (admin access works without elevation after install), APK signed build + install-over-upgrade, an incompatible-store refusal on each surface (copy a store, bump schema with a documented dev command), the container-test limitation if any.
- [ ] **Step 1:** Write; every command in the docs must be one that exists (check `--help` / the scripts from Tasks 1–9).
- [ ] **Step 2: Commit** `docs: 8b release rule, E2E checklist/results, manual check, back-ports`.

### Task 11: CI

**Files:** Modify `.github/workflows/ci.yml`

- Add to existing jobs or new ones: `scripts/check-version.sh`; Linux job building rpm/deb + `inspect-linux-packages.sh` + `test-linux-packages.sh` (podman or docker on `ubuntu-latest`; old build via `set-version.sh 0.0.1` in a scratch copy); Windows job building the MSI + `inspect-msi.ps1`; `android-apk` passes `-PmeshnodeUnsignedRelease=true` (via `build-apk.sh --release --unsigned`).
- [ ] **Step 1:** Edit; validate YAML (`python3 -I -c "import yaml,sys;yaml.safe_load(open(sys.argv[1]))" .github/workflows/ci.yml`) and that every script/flag referenced exists. CI is still unrun (no remote): say so.
- [ ] **Step 2: Commit** `ci: packages, lifecycle tests, version check, unsigned release APK`.

### Task 12: Owner's §1 run and fix loop

- [ ] **Step 1:** Final branch review (whole diff), workspace tests/fmt/clippy green on Linux; Windows `cargo test -p meshd` + MSI build on the Windows install.
- [ ] **Step 2:** Hand the owner `docs/manual-checks/sub-spec-8b.md` then `docs/e2e-checklist.md`. The owner runs them and fills `docs/e2e-results.md`.
- [ ] **Step 3:** For each failure the owner reports: fix only what blocks a checklist step (own commit, test first where testable); anything else → named follow-up in the 8b spec §11. Repeat until the owner records §1 as passed.
- [ ] **Step 4:** Merge choice to the owner (finishing-a-development-branch); update memory.
