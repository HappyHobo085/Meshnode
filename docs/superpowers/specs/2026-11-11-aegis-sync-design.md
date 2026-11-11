# Sub-spec 7: Aegis sync port (`aegis-sync-core`, `services/aegis-sync`, Aegis client changes)

> Master spec: `docs/superpowers/specs/2026-11-08-meshnode-design.md` (§11, §14 row 7, success criteria 3–4).
> Previous sub-spec: `2026-11-11-tauri-ui-design.md`. Second repo: `D:/AI_Apps/Aegis` (GitHub
> `HappyHobo085/Aegis`), whose `AGENTS.md` files are living docs updated in the same commit as the code.

## 1. Intent

**What the owner asked for:** continue with sub-project 7 — run the Aegis sync-server as a Meshnode WASM
service so each Aegis client syncs through its own device's gateway (`http://127.0.0.1:7801`), with the same
HTTP contract. Decisions taken in brainstorming:

- **Approach A:** a pure crate `aegis-sync-core` (protocol functions + contract test vectors) extracted in the
  Aegis repo; the native axum server and the WASM service each keep thin handler glue and their own storage.
- **Vendored:** Meshnode carries a byte-identical copy of the core under `vendor/aegis-sync-core`.
- **Licence:** all of Meshnode becomes **AGPL-3.0-only** (third-party vendored code keeps its own licence).
- **No migration:** the mesh service starts empty; clients re-push; devices re-register.
- **Replay nonces** live in a new node-local, non-replicated `meshnode:kv/local` interface.
- **Open registration in v1** (no registration secret): the gateway is reachable only from the owner's own
  devices; per-node soft caps still bound growth; a warning is logged as the native server does.

**Assumptions (designer, not owner):** the Aegis work happens on branch `feat/sync-core` with local commits
only; pushing and merging Aegis is the owner's call. Aegis's native behaviour (stored owner map, stored `ord`
counter, in-memory nonces, snapshot file) is unchanged.

### 1.1 Done criteria
1. Aegis: all existing `sync-server` tests (82) pass unchanged apart from import paths; the core's unit tests
   and the contract vectors pass against the native server; Aegis CI jobs `sync-server` and `msrv` cover the
   core; client tests pass.
2. Meshnode: the identical vector set passes against the WASM service; the divergence, replay, revocation and
   `kv/local` tests (§8) pass; workspace tests, fmt and clippy are green; the vendored copy matches its recorded
   Aegis commit.
3. The licence change is complete (§7).
4. The owner's manual check (§8.6) is run; it covers success criterion 4.
5. The master spec is back-ported (§9).

## 2. Aegis repo: `aegis-sync-core`

`sync-server/Cargo.toml` becomes a two-member workspace (`.` server + `core`), sharing `Cargo.lock`
(`--locked` CI keeps working after the lock is regenerated once). Core: `name = "aegis-sync-core"`,
`edition = "2021"`, `rust-version` equal to the server's (the `msrv` job reads both and must also read the
core's), `license = "AGPL-3.0-only"`, no axum, no tokio, no new packages beyond the server's lockfile.

### 2.1 What moves (names and bodies unchanged where possible)
| Module | Contents |
|---|---|
| `auth.rs` | `AuthToken`, `canonical`, `unhex`, `is_canonical_hex`, the signature/TTL/skew checks of `verify_auth` (as `check_token(token, now, …) -> Result<Verified, AuthError>`; nonce bookkeeping stays with the caller), `verify_account_root`, `constant_time_eq`, the TTL/skew/nonce constants |
| `wire.rs` | `WireRecord`, `hlc_key`, `ord_key`, `hlc_well_formed`, `wire_bytes`, every bound constant (`MAX_RECORDS_PER_REQUEST`, `MAX_CT_LEN`, response limits, …) |
| `canon.rs` | `content_digest`, `content_pass`, `unique_uuid`, `canonicalize_body`, `reject_client_poisoning_stamps`, tie-break rules |
| `page.rs` | the paging core of `page_records` over an already uuid-sorted iterator + cursor |
| `devices.rs` (new) | owner / pending / may-remove computed from facts `{device_id, registered_ms, revoked, approved, pending}` — the earliest-registered non-revoked device is owner (id tie-break) |
| `order.rs` (new) | `order_bytes(hlc) -> Vec<u8>`: `wall_ms` big-endian i64 with the sign bit flipped, `counter` big-endian u64, then `node` bytes — byte order equals `hlc_key` order |
| `env.rs` | `trait Clock { fn now_ms(&self) -> i64 }`, `trait Entropy { fn fill(&mut self, buf: &mut [u8]) }`; anything that read the clock/OS randomness takes these |

The native server supplies `SystemClock` / `OsEntropy` and keeps its in-memory `Store`, snapshot, stored owner
map, nonce set and axum glue. Its test module stays in `main.rs`.

### 2.2 Contract vectors
`core/tests/vectors/*.json`: each case = `{ name, given: <state as facts>, request: {method, path, headers,
body}, expect: {status, body | body_subset} }`, grouped `auth_*`, `records_*`, `devices_*`, `bounds_*`. Signed
tokens in vectors use fixed test keys and a fixed clock (`Clock` is injected). Runners: the core's
`tests/vectors.rs` (pure-function level), a native-server harness in `sync-server` that drives each vector
through the axum handlers, and the Meshnode harness (§6.3).

### 2.3 Aegis CI and docs
`ci.yml`'s `sync-server` job runs fmt/clippy/test for the workspace (both members); the `msrv` job checks the
core's `rust-version` too. `sync-server/AGENTS.md` and `README.md` describe the core and the vectors.

## 3. Aegis client changes (same branch)
- **Desktop:** if a Meshnode daemon is detected — the admin endpoint exists: Windows `\\.\pipe\meshnode-admin`
  (default data dir), Unix `<default data dir>/admin.sock` — the sync settings show a one-click suggestion
  "Use Meshnode on this device (`http://127.0.0.1:7801`)". Never applied automatically.
- **Android:** the same suggestion when package `net.meshnode.app` is installed (manifest `<queries>` entry for
  package visibility). A `network_security_config.xml` allows cleartext **only** to `127.0.0.1`; everything else
  stays HTTPS-only.
- Tests in Aegis's vitest/cargo setups; `src-tauri/AGENTS.md` and `src/AGENTS.md` updated.

## 4. Meshnode: `meshnode:kv/local`
New WIT interface in `meshnode:kv`:
```
interface local {
  get: func(key: string) -> option<list<u8>>;
  set: func(key: string, value: list<u8>, ttl-ms: u64) -> result<_, string>;  // 0 = no expiry
  delete: func(key: string);
}
```
- Stored in a node-local redb table `svc_local`, keyed `(service, generation, key)`; **never replicated**, not
  in the op log, not in catch-up.
- Expired entries are invisible on read and swept in the existing GC tick. `purge` (new generation) makes the
  old generation's local entries unreachable and the sweep deletes them.
- Caps per service: value ≤ 64 KiB, ≤ 100 000 live keys; a breach returns `err` (no trap).
- Added to the `service` world; existing services that do not import it are unaffected.

## 5. Meshnode: `services/aegis-sync`
A `wasm32-wasip2` component in its own workspace (like `crates/mesh-runtime/tests/fixtures`), depending on
`vendor/aegis-sync-core` by path. Routes: `GET/POST /v1/records`, `POST /v1/devices`, `GET /v1/devices`,
`POST /v1/devices/remove`, `POST /v1/devices/approve`, `GET /healthz` — the native contract.

### 5.1 State mapping
| Native | Mesh |
|---|---|
| record `(acct, ns, uuid)` | `set-ordered("rec/<acct>/<ns>/<uuid>", order_bytes(client hlc), Some(canonical WireRecord))`; deletes are the client's tombstone record, written the same way |
| `ord` | derived deterministically from `(hlc, uuid)` by the core's tie-break rule, served exactly where the native server would serve one (rule pinned in the plan against `canonicalize_body` and the client's `ord_is_adoptable`) |
| devices | grow-only `devices/<acct>` of `{device_id, label, registered_ms}`; grow-only `pending/<acct>`, `approved/<acct>`, `revoked/<acct>` |
| owner | computed by `devices.rs` (not stored) |
| `last_seen` | per-node register `seen/<acct>/<device>/<node-id-hex>`; served as the newest across nodes |
| nonces | `kv/local` key `nonce/<device>/<nonce>` with ttl = token TTL |
| caps | per-node soft caps over the local view (mesh total may exceed by ≤ (nodes − 1) × one request's allowance) |
| tombstone retention | service logic over `list-keys` (count per ns + 90-day age), as native |
| registration secret | none in v1 (open registration, logged warning) |

### 5.2 Request flow
Every request: parse → bound (core) → auth `check_token` with the WASI clock → nonce check-and-record in
`kv/local` (only after the admit rule passes, as native) → handler over kv → response built by core helpers.
GET records pages with `list-keys("rec/<acct>/<ns>/", start-after = cursor, limit)` (uuid order = native cursor
semantics) and the core's response limits. "The last device cannot be removed" (409) and owner rules use the
local view.

**Concurrency edge (stated):** two first registrations of the same new account on different nodes during a
split are both "owner" until heal; afterwards the earlier one is owner and the other an approved device.

### 5.3 Build and deploy
`scripts/build-services.sh` builds `services/aegis-sync` → `target/wasm32-wasip2/release/aegis_sync.wasm`;
deploy with `meshd deploy wasm sync <file>` (or the UI). Default limits: 64 MB memory, 30 s timeout, 8 MiB body.
Port mapping `7801 → sync` already exists, so Aegis uses `http://127.0.0.1:7801`.

## 6. Vendoring
- `vendor/aegis-sync-core/` = the Aegis `sync-server/core` tree, byte-identical, plus `VENDORED.md` with the
  Aegis commit hash.
- `scripts/sync-aegis-core.sh <aegis-checkout>` re-copies and rewrites the hash; `--check` diffs the vendored
  tree against that commit's tree in a given checkout (CI runs it only when an Aegis checkout is present;
  otherwise it verifies `VENDORED.md` exists and the hash is well-formed).

## 7. Licence change (Meshnode)
Root `LICENSE` → AGPL-3.0 text; `[workspace.package] license = "AGPL-3.0-only"`; every crate/app manifest and the
README updated; `apps/ui/package.json` `"license"`; `patches/ndk-context` and other vendored third-party code keep
their licences; `vendor/aegis-sync-core` is AGPL-3.0-only already.

## 8. Testing
### 8.1 Aegis
Existing 82 sync-server tests; core unit tests; vectors via the core runner and the native-server harness;
client tests for Meshnode detection and the network security config.

### 8.2 `kv/local` (mesh-runtime / mesh-services)
Not replicated between two in-process nodes; TTL expiry; caps return `err`; purge makes entries unreachable;
GC sweep deletes expired entries.

### 8.3 Contract vectors against the WASM service
A mesh-services test builds `services/aegis-sync` (cached like the fixtures) and runs every vector against it
in-process; the identical vector set as Aegis.

### 8.4 Mesh behaviour
- Two-replica divergence: concurrent pushes to one uuid (different HLCs; equal HLCs, different content) → the
  same winner on both nodes after sync.
- Split first-registration → one owner after heal (the earlier).
- Replay: a token reused on one node → second request `401`.
- Revocation: revoke on node A → node B refuses the device after sync.

### 8.5 CI
Meshnode: a job that installs `wasm32-wasip2`, builds the service, runs §8.2–8.4, and the vendored check. Aegis:
§2.3.

### 8.6 Manual check (`docs/manual-checks/sub-spec-7.md`, owner, Windows + S23)
1. Build and deploy `aegis-sync` from the desktop UI; it runs on both nodes.
2. Aegis desktop: accept the Meshnode suggestion; register (owner).
3. Aegis on the S23: accept the suggestion; register; approve it on desktop.
4. Sync bookmarks both ways.
5. Airplane mode on the phone; edit on both; reconnect; both converge to the same set (success criterion 4).
6. Revoke the phone from desktop; the phone is refused within 60 s of the change reaching it.

## 9. Back-ports to the master spec
§3 crate table (`services/aegis-sync`, `vendor/aegis-sync-core`, the `kv/local` interface); §5/§8 WIT notes for
`kv/local`; §11 decisions (vendored, open registration v1, nonces in `kv/local`, no migration); licence note;
§14 row 7 status.

## 10. Out of scope
Embedding `mesh-client` in Aegis; Aegis release builds/signing; a registration secret; migration from a native
server; iOS; public access.
