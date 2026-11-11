# Aegis Sync Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve the Aegis sync HTTP contract from a Meshnode WASM service backed by replicated `meshnode:kv`, sharing pure protocol code (`aegis-sync-core`) with Aegis's native server.

**Architecture:** In the Aegis repo, extract pure functions + contract test vectors into `sync-server/core`; the native axum server keeps its storage and calls the core. Meshnode vendors the core byte-identically, adds a node-local `meshnode:kv/local` WIT interface (for replay nonces), and builds `services/aegis-sync` (wasm32-wasip2) with thin handlers over kv. Aegis clients gain a "use Meshnode" suggestion and Android cleartext to 127.0.0.1.

**Tech Stack:** Rust (Meshnode edition 2024; Aegis sync-server edition 2021), wasmtime component model, `wit-bindgen`/`wasi` guest crates, redb, axum (Aegis native only), React/TS + Kotlin (Aegis client).

**Spec:** `docs/superpowers/specs/2026-11-11-aegis-sync-design.md` (master: `docs/superpowers/specs/2026-11-08-meshnode-design.md` §11).

## Global Constraints

- **Two repos.** Meshnode `D:/AI_Apps/Meshnode` branch `feat/aegis-sync`; Aegis `D:/AI_Apps/Aegis` branch `feat/sync-core` (create from `main`). Git in each needs `-c safe.directory=<repo>`. **Never push either repo.** Aegis `AGENTS.md` files are living docs: update the relevant one in the same commit as the code.
- Meshnode build env: `CARGO_TARGET_DIR=$LOCALAPPDATA/meshnode-target`, `CARGO_INCREMENTAL=0`, `-j 2`; C: is nearly full — deleting `$LOCALAPPDATA/meshnode-target/debug` is allowed. Aegis sync-server builds in its own `sync-server/target` (D: is FAT32 — if that fails, use `CARGO_TARGET_DIR=$LOCALAPPDATA/aegis-sync-target`).
- Aegis sync-server CI uses `--locked`; regenerate `sync-server/Cargo.lock` once when the workspace is created and commit it. No new crates in the core beyond what the server's lockfile already holds; the core has **no axum, no tokio**.
- `rust-version` of the core = the server's (`1.90.0`); Aegis's `msrv` job must read the core's too.
- Licence: core `AGPL-3.0-only`; Meshnode becomes `AGPL-3.0-only` everywhere except vendored third-party code (`patches/ndk-context` keeps MIT/Apache).
- Safety (owner's real PC): never install/start/stop OS services, never elevate, never touch real data dirs (`%ProgramData%\meshnode`, `%LOCALAPPDATA%\Meshnode`), never run ignored tests, never `adb`/touch a device.
- Values from the spec: token TTL / skew / nonce constants and every bound constant are moved, not changed; `kv/local` caps value ≤ **64 KiB**, ≤ **100 000** live keys per service; service default limits **64 MB / 30 s / 8 MiB**; port **7801 → sync**; owner = earliest-registered non-revoked device, device id tie-break; nonces in `kv/local` with ttl = token TTL (5 min).
- `order_bytes(hlc)`: `wall_ms` as big-endian i64 with the sign bit flipped (`(wall as u64) ^ (1<<63)`), then `counter` big-endian u64, then the `node` UTF-8 bytes. Byte order must equal `hlc_key` order.
- **`ord` rule for the mesh service (pinned here; Task 2 adds it to the core):** emitted only when another live record in the same `(acct, ns)` has an identical `hlc` (wall, counter, node) and a different uuid; then `ord = { wall_ms, counter, node: node + "~" + first 8 hex of content_digest([uuid]) }`, with `node` truncated so the result is ≤ `MAX_HLC_NODE_LEN`. It keeps `(wall, counter)` so the record's order against non-tied records is unchanged, is identical on every replica, totally orders tied records, and is a well-formed HLC the client's `ord_is_adoptable` accepts. The native server keeps its own counter-bump rule.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01VKLL6Rs4i1E9FwhqtXKgyB` (both repos).

## Review Focus

1. **A push and a delete race on two nodes.** A tombstone and an edit of the same uuid with different HLCs pushed to different nodes must converge to the higher HLC on both, and a stale push arriving later must lose. Test: Task 9 `divergence_edit_vs_tombstone`.
2. **Two records of different uuids with an identical HLC.** Both must be kept, served with deterministic distinct `ord`s that are identical on both nodes. Test: Task 2 `ord_tie_rule_*`, Task 9 `divergence_equal_hlc_different_uuid`.
3. **A replayed token.** The same `Authorization` header used twice on one node: the second is `401`; a token whose nonce expired from `kv/local` but is past TTL is refused by the TTL check. Test: Task 8 `replay_same_node_401`, Task 5 `local_ttl_expires`.
4. **The owner signs out while another node hasn't seen a later registration.** Owner computation must hand off to the earliest remaining non-revoked device on every node once synced. Test: Task 2 `owner_*`, Task 9 `owner_handoff_across_nodes`.
5. **A huge namespace.** 50 000 records: GET pages must respect 5 000 records / 32 MiB and the cursor must resume exactly; the service must finish inside 30 s / 64 MB. Test: Task 7 `paging_large_namespace`.

---

### Task 1: Aegis — create `aegis-sync-core`; move auth, wire and env

**Files (Aegis repo):**
- Create: `sync-server/core/{Cargo.toml, LICENSE (copy of the server's AGPL text), src/lib.rs, src/auth.rs, src/wire.rs, src/env.rs}`
- Modify: `sync-server/Cargo.toml` (add `[workspace] members = [".", "core"]`, depend on `aegis-sync-core = { path = "core" }`), `sync-server/Cargo.lock`, `sync-server/src/main.rs`, `.github/workflows/ci.yml` (`msrv` reads `sync-server/core/Cargo.toml` too), `sync-server/AGENTS.md`

**Interfaces:**
- Produces: `aegis_sync_core::auth::{AuthToken, canonical, unhex, is_canonical_hex, check_token(token: &AuthToken, now_ms: i64) -> Result<Verified, AuthError>, verify_account_root, constant_time_eq, MAX_TTL_MS, MAX_FUTURE_SKEW_MS, MAX_PAST_SKEW_MS, MAX_SEEN_NONCES, MAX_SEEN_NONCES_PER_DEVICE, MAX_NONCE_HEX_LEN}`; `wire::{WireRecord (pub fields), hlc_key, ord_key, hlc_well_formed, wire_bytes, MAX_* bound constants, MAX_HLC_NODE_LEN}`; `env::{Clock, Entropy, SystemClock?}` — the core defines the traits only; the server defines `SystemClock`/`OsEntropy`.
- `check_token` does everything `verify_auth` does except nonce bookkeeping and the `admit` closure; `verify_auth` in `main.rs` becomes: parse → `check_token` → admit → record nonce, behaviour identical.

- [ ] **Step 1:** Create branch `feat/sync-core` in Aegis. Run the baseline: `cargo test --locked --manifest-path sync-server/Cargo.toml` — record the pass count (expected 82).
- [ ] **Step 2: Write failing core tests** in `core/src/auth.rs` / `wire.rs`: `canonical_is_stable` (fixed token → fixed bytes), `check_token_rejects_expired`, `check_token_rejects_future_skew`, `check_token_rejects_uppercase_hex`, `hlc_well_formed_cases`, `wire_bytes_counts_hlc_and_ord` — each asserting the exact behaviour of the current `main.rs` code (copy the expectations from its tests).
- [ ] **Step 3: Run** `cargo test --manifest-path sync-server/core/Cargo.toml` — FAIL (crate missing).
- [ ] **Step 4: Implement** by moving code (same bodies), making items `pub`, and switching `main.rs` to import them. Clock reads become `clock.now_ms()`.
- [ ] **Step 5: Run** `cargo fmt --check`, `cargo clippy --locked --manifest-path sync-server/Cargo.toml --all-targets -- -D warnings`, `cargo test --locked --manifest-path sync-server/Cargo.toml` (workspace) — the original 82 pass unchanged plus the new core tests.
- [ ] **Step 6: Commit** (Aegis) `refactor(sync-server): extract aegis-sync-core (auth, wire, env)`.

### Task 2: Aegis — move canon and paging; add order, ord-tie and devices

**Files (Aegis):** Create `sync-server/core/src/{canon.rs, page.rs, order.rs, devices.rs}`; Modify `sync-server/src/main.rs`, `sync-server/core/src/lib.rs`, `sync-server/AGENTS.md`

**Interfaces:**
- Consumes: Task 1 modules.
- Produces:
  - `canon::{content_digest, content_pass, unique_uuid(rec, ns, taken, entropy: &mut dyn Entropy), canonicalize_body, reject_client_poisoning_stamps}` (bodies moved; `entropy_hex` takes `&mut dyn Entropy`).
  - `page::page_sorted<'a, I: Iterator<Item = &'a WireRecord>>(sorted: I, cursor: Option<&str>, limit: usize) -> Page` with `Page { records, next }` — the native `page_records` calls it.
  - `order::order_bytes(hlc: &Value) -> Vec<u8>` (Global Constraints encoding).
  - `order::tie_ord(rec: &WireRecord) -> Value` implementing the pinned mesh `ord` rule (the caller decides whether a tie exists).
  - `devices::{DeviceFacts { device_id, label, registered_ms }, AccountFacts { devices: Vec<DeviceFacts>, pending: BTreeSet<String>, approved: BTreeSet<String>, revoked: BTreeSet<String> }, owner(&AccountFacts) -> Option<&str>, is_pending(&AccountFacts, id) -> bool, may_remove(&AccountFacts, actor, target) -> Result<(), DeviceRefusal>}` where `DeviceRefusal` maps to the native statuses (`403` not owner, `409` last device).

- [ ] **Step 1: Write failing tests:**
  - `order_bytes_matches_hlc_key`: a property test over random `(wall i64 incl. negatives, counter, node)` pairs — `order_bytes(a).cmp(&order_bytes(b)) == hlc_key(a).cmp(&hlc_key(b))` (use a small deterministic generator; no new crates).
  - `ord_tie_rule_keeps_wall_and_counter`, `ord_tie_rule_is_deterministic`, `ord_tie_rule_distinct_for_distinct_uuids`, `ord_tie_rule_node_within_max_len` (64-char node), `ord_tie_rule_is_well_formed` (`hlc_well_formed`).
  - `owner_is_earliest_registered`, `owner_skips_revoked`, `owner_tie_breaks_by_id`, `owner_handoff_after_self_removal`, `pending_until_approved`, `may_remove_last_device_is_409`, `may_remove_other_requires_owner`.
  - `page_sorted_resumes_after_cursor`, `page_sorted_reports_next`.
- [ ] **Step 2: Run** core tests — FAIL.
- [ ] **Step 3: Implement**; native server switched to the moved functions; its stored-owner logic unchanged.
- [ ] **Step 4: Run** fmt / clippy / workspace tests — the original 82 still pass, all new pass.
- [ ] **Step 5: Commit** (Aegis) `refactor(sync-server): move canonicalisation and paging to the core; add order bytes, tie ord and device facts`.

### Task 3: Aegis — contract vectors, runners, CI and docs

**Files (Aegis):** Create `sync-server/core/tests/vectors/*.json`, `sync-server/core/tests/vectors.rs`, `sync-server/core/src/vector.rs` (vector types + a `pub fn load_all(dir) -> Vec<Vector>` reused by Meshnode), `sync-server/tests/vectors_native.rs` (or a `#[cfg(test)]` module in `main.rs` if handlers are not reachable from an integration test); Modify `.github/workflows/ci.yml`, `sync-server/README.md`, `sync-server/AGENTS.md`

**Interfaces:**
- Produces: `vector::{Vector { name, given: Given, request: Req, expect: Expect }, Given { accounts: Vec<AccountFacts>, records: Vec<(acct, ns, WireRecord)> }, Req { method, path, headers: Vec<(String,String)>, body: Option<Value> }, Expect { status: u16, body: Option<Value>, body_subset: Option<Value> }}`; test keys: fixed Ed25519 seeds documented in `vectors/README.md`; fixed clock `now_ms = 1_800_000_000_000`.
- Vector set (at least): `healthz_ok`; `auth_missing_401`, `auth_expired_401`, `auth_bad_sig_401`, `auth_uppercase_hex_401`; `devices_first_registration_owner`, `devices_second_pending`, `devices_approve_by_owner`, `devices_approve_by_non_owner_403`, `devices_remove_last_409`, `devices_revoked_reregister_403`, `devices_owner_self_remove_handoff`; `records_push_then_get`, `records_stale_push_loses`, `records_tombstone_wins_by_hlc`, `records_poisoned_stamp_dropped`, `records_client_ord_ignored`, `records_paging_cursor`; `bounds_too_many_records_413`, `bounds_ct_too_large_413`, `bounds_bad_hlc_400`. A vector may mark itself `native_only` or `mesh_only` with a reason (e.g. the `ord` tie rule differs; owner is stored vs computed) — such marks must be few and justified.

- [ ] **Step 1:** Write the vectors and the two runners; run — FAIL until the native harness drives handlers.
- [ ] **Step 2:** Implement the native harness (call the axum `app(state)` router via `tower::ServiceExt` only if `tower` is already in the lockfile; otherwise call the handler functions directly with constructed extractors, as the existing tests do).
- [ ] **Step 3: Run** workspace tests — all vectors pass natively; the original 82 pass.
- [ ] **Step 4:** CI: the `sync-server` job runs `--workspace` fmt/clippy/test (both members). Docs updated.
- [ ] **Step 5: Commit** (Aegis) `test(sync-server): contract test vectors shared with the Meshnode service`.

### Task 4: Aegis — client: Meshnode suggestion + Android cleartext to 127.0.0.1

**Files (Aegis):** Modify the sync-settings UI in `src/` (find the sync URL field), `src-tauri/src/` (a `meshnode_detected() -> bool` command; add to the IPC contract in `shared/types.ts`), Android manifest + `res/xml/network_security_config.xml` under `src-tauri/gen/android/...` (follow how Aegis already edits its Android project), `src/AGENTS.md`, `src-tauri/AGENTS.md`, `shared/AGENTS.md`

**Interfaces:**
- Produces: IPC `meshnode.detected` → `{ detected: boolean, url: "http://127.0.0.1:7801" }`. Desktop: Windows `\\.\pipe\meshnode-admin` exists (`std::fs::metadata` on the pipe path or `WaitNamedPipeW` with 0 timeout — no connection is made); Unix `<default data dir>/admin.sock` exists (`/var/lib/meshnode`, macOS `/Library/Application Support/Meshnode`). Android: `PackageManager.getPackageInfo("net.meshnode.app")` succeeds (manifest `<queries><package android:name="net.meshnode.app"/></queries>`).
- Network security config: `cleartextTrafficPermitted="true"` only for domain `127.0.0.1` (`includeSubdomains="false"`); base config keeps cleartext off.

- [ ] **Step 1: Write failing tests:** Rust `detect_meshnode_*` with an injected probe (pure function over "endpoint exists"); vitest `sync_settings_shows_meshnode_suggestion_when_detected`, `suggestion_not_applied_automatically`, `hidden_when_not_detected`; a JVM/unit or XML-parse test (or a Rust test reading the XML) asserting the config permits cleartext only for `127.0.0.1`.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.**
- [ ] **Step 4: Run** Aegis `npm test`, `cargo test --manifest-path src-tauri/Cargo.toml` (use `-j 2`; set `CARGO_TARGET_DIR` to a C: path if D: fails), lint as Aegis CI does — PASS. Do not run Android builds that install anything.
- [ ] **Step 5: Commit** (Aegis) `feat(sync): suggest the local Meshnode sync URL; allow cleartext to 127.0.0.1 on Android`.

### Task 5: Meshnode — `meshnode:kv/local` (node-local, non-replicated)

**Files:** Modify `crates/mesh-runtime/wit/deps/kv.wit`, `crates/mesh-runtime/wit/world.wit`, `crates/mesh-runtime/src/{kv.rs, host.rs, lib.rs}`, `crates/mesh-store/src/{tables.rs, lib.rs}` (+ new `local.rs`), `crates/mesh-core/src/db.rs` (`StoreHandle` local methods), `crates/mesh-services/src/{kv.rs, gc.rs}`; Test: `crates/mesh-store/tests/local.rs`, `crates/mesh-runtime/tests/kv.rs`, `crates/mesh-services/tests/gc.rs`; new fixture `crates/mesh-runtime/tests/fixtures/local` (a guest that exercises `kv/local`).

**Interfaces:**
- WIT (spec §4): `interface local { get: func(key: string) -> option<list<u8>>; set: func(key: string, value: list<u8>, ttl-ms: u64) -> result<_, string>; delete: func(key: string); }`; `world service` imports `meshnode:kv/local`.
- `KvBackend` gains `async fn local_get(&self, key) -> Result<Option<Vec<u8>>, KvError>`, `local_set(&self, key, value, ttl_ms) -> Result<Result<(), String>, KvError>`, `local_delete(&self, key) -> Result<(), KvError>`; `MemKv` implements them (with TTL using an injectable now).
- `mesh-store`: table `svc_local` keyed `ns \0 key` → `expiry_ms (u64 BE, 0 = none) ++ value`; `CoreStore::local_get/local_set/local_delete/local_sweep(now_ms) -> usize/local_count(ns)`; writes never create ops. Caps: value ≤ 64 KiB, ≤ 100 000 live keys per ns → `Err("...")` string.
- `ServiceKv` maps to the store with its namespace (generation-qualified, so purge makes old entries unreachable); the GC tick calls `local_sweep`.

- [ ] **Step 1: Write failing tests:** `local_not_replicated` (two in-process nodes: set on A → B's `local_get` is `None` after sync, and A has no new op), `local_ttl_expires` (fake clock), `local_value_cap_err`, `local_key_cap_err`, `local_purge_unreachable`, `local_sweep_deletes_expired`; runtime `guest_local_roundtrip` via the new fixture.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p mesh-store -p mesh-runtime -p mesh-services -j 2`, fmt, root clippy — PASS; existing fixtures (that don't import `local`) still load.
- [ ] **Step 5: Commit** `feat(runtime): meshnode:kv/local, a node-local non-replicated store for services`.

### Task 6: Meshnode — AGPL licence; vendor the core

**Files:** Replace `LICENSE*` with the AGPL-3.0 text (`LICENSE`), Modify root `Cargo.toml` (`license = "AGPL-3.0-only"`), every manifest that sets its own licence (`apps/*/src-tauri/Cargo.toml`, `crates/mesh-ui-tauri/Cargo.toml`, `apps/ui/package.json`, fixture workspaces), `README.md`; Create `vendor/aegis-sync-core/` (copied from Aegis `feat/sync-core` HEAD after Task 3), `vendor/VENDORED.md`, `scripts/sync-aegis-core.sh`; root `Cargo.toml` `exclude` adds `vendor`.

**Interfaces:**
- `scripts/sync-aegis-core.sh <aegis-checkout> [--check]`: copy mode replaces `vendor/aegis-sync-core` with `<aegis>/sync-server/core` at its HEAD and writes the commit hash into `VENDORED.md`; `--check` with a checkout diffs the vendored tree against `git -C <aegis> archive <hash> sync-server/core`; without a checkout it only validates `VENDORED.md` (40-hex hash present).
- `patches/ndk-context` keeps its MIT/Apache files.

- [ ] **Step 1:** Write the script with a self-test (`bash scripts/sync-aegis-core.sh --self-test` creates a temp git repo, copies, then `--check` passes and a tampered file fails).
- [ ] **Step 2:** Run the self-test — FAIL, then implement — PASS.
- [ ] **Step 3:** Vendor from `D:/AI_Apps/Aegis`; `--check` passes. Licence edits; `cargo metadata` shows no crate with a different licence except the vendored third-party ones.
- [ ] **Step 4: Run** fmt, root clippy, `cargo test --workspace --all-features -j 2` — PASS.
- [ ] **Step 5: Commit** `chore: relicense Meshnode AGPL-3.0-only; vendor aegis-sync-core`.

### Task 7: Meshnode — `services/aegis-sync`: records, healthz, auth, build

**Files:** Create `services/aegis-sync/{Cargo.toml, Cargo.lock, src/lib.rs, src/http.rs, src/records.rs, src/auth.rs, src/store.rs}`, `scripts/build-services.sh`; Modify root `Cargo.toml` `exclude` (`services`), `mesh_runtime::test_fixtures` (or a new `mesh-services` test helper) to build `services/aegis-sync` like the fixtures; Test: `crates/mesh-services/tests/aegis_sync.rs`.

**Interfaces:**
- Consumes: vendored core (path dep), Task 5's `kv/local` (via `wit-bindgen` generated bindings from `crates/mesh-runtime/wit`).
- Produces: routes `GET /healthz`, `GET /v1/records?ns&cursor&limit`, `POST /v1/records`; auth via `check_token` with the WASI wall clock, nonce check-and-record in `kv/local` key `nonce/<device>/<nonce>` (ttl = `MAX_TTL_MS`) only after admit; storage per spec §5.1 with `order_bytes` and the tie `ord` rule (a tie exists when another key in `rec/<acct>/<ns>/` holds a record with the identical `hlc` — check via a secondary index set `hlcidx/<acct>/<ns>/<order_bytes hex>` of uuids, grow-only, so the check is O(1)).
- Test helper: `aegis_sync_wasm() -> PathBuf` (built once per test run, cached like fixtures); a vector runner that loads `vendor/aegis-sync-core/tests/vectors` through `aegis_sync_core::vector::load_all`, seeds `given` through the service's own endpoints or direct kv writes, and drives requests through `mesh_runtime` with a `MemKv` or a real node.

- [ ] **Step 1: Write failing tests:** the vector runner over `healthz_*`, `auth_*`, `records_*`, `bounds_*` (skipping `native_only`); `paging_large_namespace` (50 000 records seeded directly into kv; pages of ≤ 5 000 and ≤ 32 MiB; full walk returns every uuid exactly once; each request finishes under the 30 s limit with 64 MB memory).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** the service and `scripts/build-services.sh` (`cargo build --release --target wasm32-wasip2 --manifest-path services/aegis-sync/Cargo.toml`).
- [ ] **Step 4: Run** `cargo test -p mesh-services --test aegis_sync -j 2`, fmt, clippy for the service workspace — PASS.
- [ ] **Step 5: Commit** `feat(services): aegis-sync records, healthz and auth over meshnode:kv`.

### Task 8: Meshnode — `services/aegis-sync`: devices, nonces, caps, tombstone retention

**Files:** Modify `services/aegis-sync/src/{lib.rs, http.rs}`, create `services/aegis-sync/src/{devices.rs, caps.rs, retention.rs}`; Test: `crates/mesh-services/tests/aegis_sync.rs`.

**Interfaces:**
- Produces: `POST /v1/devices`, `GET /v1/devices`, `POST /v1/devices/remove`, `POST /v1/devices/approve` per the native contract, with state per spec §5.1 (grow-only `devices/`, `pending/`, `approved/`, `revoked/`; owner via `devices::owner`; `last_seen` in `seen/<acct>/<device>/<node-id-hex>`, newest served); per-node soft caps (`MAX_ACCOUNTS`, `MAX_TOTAL_RECORDS`, `MAX_TOTAL_CT_BYTES`, `MAX_PENDING_PER_ACCOUNT`, `MAX_RECORDS_PER_ACCOUNT`) counted over the local view with per-node counters `count/<node-id-hex>/...`; tombstone retention (500 per ns, 90-day age floor) run after a POST, as native; open registration logs one warning per instance start (stderr).

- [ ] **Step 1: Write failing tests:** the full vector set (all non-`native_only`); `replay_same_node_401`; `owner_handoff_single_node`; `caps_account_limit_507` (with the cap lowered through a test-only constant path — a cfg feature on the service, not production code paths); `retention_prunes_old_tombstones`.
- [ ] **Step 2–4:** FAIL → implement → PASS (`cargo test -p mesh-services --test aegis_sync -j 2`, clippy, fmt).
- [ ] **Step 5: Commit** `feat(services): aegis-sync devices, replay defence, caps and retention`.

### Task 9: Meshnode — mesh behaviour tests and CI

**Files:** Create `crates/mesh-services/tests/aegis_sync_mesh.rs`; Modify `.github/workflows/ci.yml`.

**Interfaces:**
- Consumes: two in-process nodes with the service deployed on both (follow the existing two-node patterns in `crates/mesh-services/tests` / `crates/mesh-host/tests`).
- CI job `aegis-sync`: `rustup target add wasm32-wasip2`, `bash scripts/build-services.sh`, `cargo test -p mesh-services --test aegis_sync --test aegis_sync_mesh -j 2`, `bash scripts/sync-aegis-core.sh --check` (no checkout → hash validation only).

- [ ] **Step 1: Write failing tests:** `divergence_edit_vs_tombstone`, `divergence_equal_hlc_different_uuid` (both nodes serve both records with identical `ord`s), `stale_push_after_sync_loses`, `split_first_registration_one_owner_after_heal` (the earlier `registered_ms` wins), `owner_handoff_across_nodes`, `revocation_reaches_other_node` (revoke on A → B refuses within the sync, not by wall-clock wait), `replay_is_per_node` (documented: the same token on node B within TTL is accepted — loopback-only, per the spec).
- [ ] **Step 2–4:** FAIL → fix any service bug found → PASS.
- [ ] **Step 5: Commit** `test(services): aegis-sync convergence across two nodes; ci: aegis-sync job`.

### Task 10: Docs — manual check and master spec back-ports

**Files:** Create `docs/manual-checks/sub-spec-7.md`; Modify `docs/superpowers/specs/2026-11-08-meshnode-design.md` (spec §9 back-ports; §14 row 7 → "implemented, manual check pending; see `docs/manual-checks/sub-spec-7.md`"), `docs/superpowers/specs/2026-11-11-aegis-sync-design.md` ("(as built)" notes for anything that changed).

- [ ] **Step 1:** Write the manual check (spec §8.6 six steps; exact actions; pass criteria; results table; build/deploy commands; where Aegis's sync settings live; how to approve and revoke; note that the Aegis branch is unpushed and the APK must be built from it).
- [ ] **Step 2:** Back-ports and reconciliations.
- [ ] **Step 3:** Full verification: Meshnode fmt, root clippy, `cargo test --workspace --all-features -j 2`, the aegis-sync tests, `scripts/sync-aegis-core.sh --check D:/AI_Apps/Aegis`; Aegis `cargo test --locked --manifest-path sync-server/Cargo.toml` and its client tests.
- [ ] **Step 4: Commit** `docs: sub-spec 7 manual check and back-ports`.
