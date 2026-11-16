# Sub-spec 8a: `mesh-client` SDK and partial replication

> Master spec: `docs/superpowers/specs/2026-11-08-meshnode-design.md` (§3 crate table, §5.8 catch-up, §6
> membership, §8 client access and the `mesh-client` library, §14 row 8, success criterion 5). Previous
> sub-spec: `2026-11-11-aegis-sync-design.md`.

## 1. Intent

**What the owner asked for:** start sub-spec 8. Decisions taken in brainstorming:

- **Split row 8** into **8a** (`mesh-client` + partial replication, this spec) and **8b** (installers, release
  signing, and the full §1 success-criteria run as an owner checklist in `docs/e2e-results.md`).
- `mesh-client` is a **general SDK for the owner's future apps**, not only a test aid.
- Bindings: **Rust + Kotlin (UniFFI)**. Swift/iOS later.
- **Approach A:** a thin client over the real node core, in client role, with **partial replication**
  (`_mesh` only) implemented in the sync engine; existing client-role `meshd` nodes get it too.

**Assumptions (designer):** every node in a mesh runs the same build; the Kotlin side ships as a locally built
AAR (no Maven publishing); no sample Android app in 8a.

### 1.1 Done criteria
1. Partial replication works and is safe (§3 tests pass, including the "new host syncs only from a client"
   case).
2. `mesh-client` passes its in-process tests, including criterion 5 in-process (client + two hosts, one
   down).
3. `mesh-client-ffi` builds for Android (arm64-v8a, x86_64), the committed Kotlin bindings are current, and
   the JVM binding test passes (or its pinned fallback, §6).
4. `cargo tree -p mesh-client` contains no `wasmtime`; CI enforces it.
5. Workspace tests, fmt and clippy green; manual check written; master spec back-ported (§8).

## 2. Crates

| Crate | Change |
|---|---|
| `crates/mesh-client` (new) | the Rust SDK (§4); depends on mesh-core, mesh-proto and the shared forward module; **no** wasmtime, gateway server, service runtime or blob fetching |
| `crates/mesh-client-ffi` (new) | UniFFI wrapper (cdylib) → Kotlin bindings; built for Android like `mesh-ffi` |
| `crates/mesh-core` | sync scope in `Hello`, scoped diffs for partial peers, ignore `Covered` from partial peers, role-upgrade re-bootstrap |
| `crates/mesh-store` | `diff_since_scoped`, a store scope with `StoreError::OutOfScope` |
| `crates/mesh-proto` | `SyncMsg::Hello` gains `scope` (absent = Full) |
| `crates/mesh-gateway` | the `httpfwd` forwarding logic (host picking, attempts, connection cache) moves to a module `mesh-client` can use without the HTTP server; the gateway keeps using it unchanged |

Identity and storage are the node's layout: a data dir with `node.key` and a redb store holding only `_mesh`.
Service manifests and placement already live in `_mesh`, so a client can pick hosts from its own replica.

## 3. Partial replication

- **Scope.** `Scope = Full | Partial(Vec<String>)`; v1 has exactly `Partial(["_mesh"])`. Host-role nodes are
  Full; client-role nodes are Partial. A node's scope comes from its effective role.
- **Hello.** `SyncMsg::Hello { mesh, vv, scope }`; a missing `scope` decodes as Full. A peer whose announced
  scope contradicts its role in `_mesh` is refused like an unknown member.
- **Sending to a partial peer.** `Store::diff_since_scoped(theirs, limit, scope)`: as `diff_since`, but each
  run of ops whose namespace is outside the scope becomes `Covered { origin, upto }` (merged with adjacent
  compacted runs). The partial peer's version vector advances past them.
- **Receiving from a partial peer.** A node **ignores every `Covered` marker from a Partial peer** and applies
  only its ops; gaps wait in `pending` until a Full peer fills them. (A partial node's markers mean
  "filtered", not "compacted".)
- **Writes.** A Partial store refuses writes to any namespace outside its scope with `StoreError::OutOfScope`
  (no code path can create ops others would need).
- **Re-bootstrap** of a partial node uses the scoped diff.
- **Role upgrade.** If a partial node's effective role becomes host (re-invited as host; host outranks
  client), it must re-bootstrap as Full: reset its store and full catch-up through the existing machinery.
- **Unchanged:** compaction, tombstone GC, removal, the write gate.

## 4. Rust API (`mesh-client`)

```rust
pub struct ClientOptions { pub name: String, pub platform: String, pub request_timeout: Duration /* 30 s */ }
impl MeshClient {
  pub async fn join(dir: &Path, ticket: &str, opts: ClientOptions) -> Result<MeshClient, ClientError>;
  pub async fn open(dir: &Path, opts: ClientOptions) -> Result<MeshClient, ClientError>;
  pub async fn request(&self, service: &str, req: http::Request<Bytes>) -> Result<http::Response<Bytes>, ClientError>;
  pub fn status(&self) -> ClientStatus;       // mesh id, own id, online hosts, first sync done, removed
  pub fn members(&self) -> Vec<Member>;       // id, name, role, platform, online, rtt
  pub fn services(&self) -> Vec<ServiceInfo>; // name, kind, hosts running it
  pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent>; // MembersChanged, ServicesChanged, Removed, Online, Offline
  pub async fn invite(&self, role: Role, ttl: Duration) -> Result<String, ClientError>;
  pub async fn remove(&self, id: NodeId) -> Result<(), ClientError>;
  pub async fn shutdown(self);
}
```

- `ClientError`: `InvalidTicket`, `AlreadyJoined`, `NotJoined`, `AlreadyOpen`, `Removed`, `NoHost`, `Timeout`,
  `Io`, `Internal`.
- `request`: path + query, method, headers, body (≤ 16 MiB); hop-by-hop headers dropped; forwarded over
  `httpfwd` to the lowest-RTT online host in the service's placement, next host on failure, up to 3 attempts.
  The host's response (including 4xx/5xx) is returned as-is; `NoHost` only when no host is reachable.
- One client per data dir (lock file → `AlreadyOpen`).
- A client never runs services, never serves `httpfwd`, never opens a gateway, never fetches blobs. It may
  invite and remove (any member may).
- Example: `crates/mesh-client/examples/fetch.rs` — join with a ticket (or open), fetch
  `<service> <path>`, print the response.

## 5. Kotlin (`mesh-client-ffi`)

- `MeshClient.join(dir, ticket, options)`, `open(...)`, `request(service, MeshRequest)`, `invite`, `remove`,
  `shutdown` are `suspend` functions; `status()`, `members()`, `services()` return records.
- `MeshRequest` / `MeshResponse` records: method, path, headers, body.
- Events: callback interface `MeshClientListener.onEvent(event)`.
- Errors: sealed `MeshClientException` mirroring `ClientError`.
- Build: `libmesh_client_ffi.so` for arm64-v8a and x86_64 plus generated bindings, by a script following
  `apps/android/gen-bindings.sh`; committed bindings are checked current in CI. `scripts/build-client-aar.sh`
  packages `.so` + bindings into an AAR locally.

## 6. Testing
1. **mesh-store:** `diff_since_scoped` (out-of-scope runs → `Covered`, compacted gaps still covered, `limit`);
   Partial store refuses out-of-scope writes.
2. **mesh-core (in-process nodes):** client store never holds a service namespace, holds members/manifests;
   Full node ignores `Covered` from a Partial peer — a new host syncing first from the client still gets all
   data later from a Full host; contradictory scope refused; role upgrade → full re-bootstrap; scope-less
   `Hello` = Full; convergence tests still hold with a partial node present.
3. **mesh-client:** join + `request` to a fixture service; failover between two hosts; `NoHost`; removal →
   `Removed` event and request errors; `open` reuses identity, second open refused; `invite` from a client;
   **criterion 5 in-process** (client + hosts A and B, A shut down, requests still served via B).
4. **mesh-client-ffi:** Rust tests of the wrapper; a Kotlin JVM test loading the host-built library through
   JNA, running join → request against an in-process mesh. **Fallback (decided in the plan if the JVM load is
   impractical on CI):** a binding-shape test plus the Rust-side coverage.
5. **CI:** a `mesh-client` job (tests + `cargo tree` no-wasmtime check); an FFI job (bindings current,
   Android `.so` build, JVM test).

## 7. Manual check (`docs/manual-checks/sub-spec-8a.md`)
Join a client from a second machine or user profile with `examples/fetch.rs`; fetch a site; with the desktop
node off, fetch through the phone; confirm the client's store holds only `_mesh` (size, `meshd`-style
inspection); remove the client and confirm it is refused.

## 8. Back-ports to the master spec
§3 crate table (`mesh-client-ffi`), §5.8 (scoped catch-up, `Covered` from partial peers ignored), §6
(scope by role, role-upgrade re-bootstrap), §8 (`mesh-client` API as built), §14 (row 8 split into 8a/8b).

## 9. Out of scope
Swift/iOS bindings; Maven publishing; a sample Android app; embedding in Aegis; installers and release
signing (8b); the full §1 E2E run (8b).

## 10. As built (differences from this design)

Where this section and the text above disagree, this section is what was built. Master spec §3, §5, §6, §8,
§13 and §14 carry the same changes.

### 10.1 Sync protocol and partial replication (§3)
- **`Hello { mesh, vv, scope, freshness }`** (ruling R4). `scope` is a required field. There is no
  scope-less `Hello`: wire compatibility with earlier builds was not kept (the owner's decision; no mesh
  held data), and the ALPN stays `meshnode/sync/1`.
- **`freshness`** is `Fresh | Stale { since_ms } | Empty`. `Empty` means the node never completed a full
  sync; a re-bootstrapping node announces `Stale`.
- **`Advertise { vv }`** is a new message. It raises the bound the peer checks `Covered` markers against.
  In live mode a node sends it before a marker above what its `Hello` announced, so a client never gets a
  marker above what it was told the host holds. **`Advertise` before the peer's first `Hello` is a protocol
  error** (ruling A1).
- **Scoped diffs** (ruling R5). `diff_since_scoped` walks at most 4 × `limit` op rows per call, filtered
  ones included. When that budget runs out inside a filtered run, the page ends with a `Covered` up to the
  last row walked. The join reply's catch-up to a client joiner is scoped too (ruling R1).
- **Scope at open.** The scope comes from the node's role in `_mesh` and from `role.json`, the role it
  joined with, written before `mesh.json`. The store is Partial if either says client. A client data dir
  without `role.json` opens Partial once its `members` record says client.
- **`check_scope`** (ruling R7). A peer listed as both client and host (a client re-invited as a host) may
  announce Partial; it is then served scoped and its markers are ignored. A peer with no role yet is taken
  at its word.
- **M4 (known, heals by itself).** A host that does not yet hold a node's host record refuses that node's
  `Full` `Hello` as contradicting its client role. This heals once the record reaches the host from another
  peer and the node redials.

### 10.2 Staleness and re-bootstrap
The design did not say how staleness works with partial peers. What was built (rulings C1c, C1d, C1e, A2,
I2; Task 3 fix rounds 1–7, Task 4 fix rounds 1–3):
- **What a completed round records**, by the freshness the peer announced:
  - **a Fresh Full peer:** a full sync (the time, and `synced_self_seq` to the peer's `vv[me]`);
  - **a Fresh Partial peer:** the time only, and only when the store already has a time that is not stale.
    A client never rescues a stale node and never makes an `Empty` one Fresh;
  - **a Stale peer:** nothing is refreshed, but an `Empty` store inherits the peer's staleness, with
    `since_ms` clamped to at most now − 80 days − 1 ms;
  - **an `Empty` peer:** nothing (two `Empty` stores stay `Empty`).

  A round in which the store applied ops from a Stale peer then lowers its time to that peer's clamped
  `since_ms` (taint). The founder starts Fresh. So only the founder or a Fresh Full peer makes a host
  Fresh.
- **Staleness never blocks syncing.** The earlier `PeerStale` refusal is gone. The only reset trigger is a
  **Fresh Full** peer's `Hello` reaching a stale node, and nothing is served to that peer first. A stale
  client resets on it too; its `_mesh` data can be fetched again. A mesh in which every node is stale stays
  stale but fully working until a Fresh Full node appears.
- **A rebuild takes data only from Fresh Full peers** (and `_mesh` from clients). It completes only after a
  Fresh Full peer's `Done` answered a `Hello` sent after the reset (ruling I2). The finished store records a
  full sync time.
- **Epoch atomicity** (Task 3 rounds 6–7, Task 4 round 2, ruling R9). The reset and `begin_epoch` are one
  store call under the `FullPulls::decide` lock. A session takes the same lock to decide on a reset and to
  record a completed round. A round whose `Hello` predates the current epoch records nothing, and sessions
  whose last `Hello` predates a reset are restarted.
- **`SessionEnd::Reset`** (Task 4 round 3, ruling R10). A session whose first `Hello` went out before the
  current epoch ends with `SessionEnd::Reset` if the peer is Full or not yet known, and is redialed at
  `backoff_min`.
  - The check runs before every incoming message and every writer pass, and the reset wakes every writer.
  - The `Covered` apply re-checks the epoch under `decide`, so a marker computed against a pre-reset
    `Hello` never lands in the emptied store.
- **`check_stale` while rebuilding** (Task 4 round 2, a narrower form of ruling R9 (a)). While the store is
  actually being rebuilt (reset done, run not finished), a Fresh Full peer's `Hello` decides nothing. The
  rebuild's sources are never turned away, even when a stale time was written into the store being rebuilt.
  Before the reset (paused, reset decided but not yet done), a Fresh Full peer still ends the session, which
  restarts with a fresh `Hello`. The ruling first said to skip the check for the whole paused period, but
  that broke sessions kept up across the reset.
- **`declare-fresh`** (ruling R6). A host alone for over 80 days stays Stale for good (ruling C1d), so
  tombstone GC is skipped and storage grows. The owner can declare it up to date:
  - core `Node::declare_fresh()`, which refreshes the time only and never `synced_self_seq`;
  - admin request `DeclareFresh`;
  - `meshd declare-fresh --yes` (without `--yes` it prints the risk and changes nothing);
  - the UI: Settings, **Declare up to date…**, behind a type-to-confirm dialog that states the risk.

  It is refused while a re-bootstrap runs or a session has decided on one (`Rebootstrapping`), and once the
  node is removed. Beyond the ruling, it is also refused for a store that never completed a full sync
  (`NeverSynced`).

### 10.3 Role upgrade (§3)
- **When it starts.** The upgrade uses the stale re-bootstrap's steps and starts only when a session meets a
  **Fresh Full** peer (ruling R8, like C1). A client re-invited in a mesh whose only host is stale stays a
  working client until a Fresh Full host appears.
- **The flip.** From the reset on, its `Hello`s announce `Full` (and `Stale`, as every rebuild does). The
  store's scope becomes Full and `role.json` becomes `host` **only when the rebuild completes**:
  - the scope flips before serving resumes;
  - `role.json` is rewritten before `rebootstrap.bin` is removed.

  A failed run, including one that fails after the flip, is retried and resumes from `rebootstrap.bin`.
- **Serving meanwhile.** While a node's store is not Full or is re-bootstrapping, its **gateway forwards**
  every request (Task 4 M3). Its **`httpfwd` answers the retryable `503 Node re-bootstrapping`** (ruling
  R9). This also closed an older gap: a host in the middle of a rebuild used to serve forwarded requests from
  its incomplete store.
- **No product path promotes a client yet.** `join` refuses a data dir already in a mesh, and there is no
  promote request. The tests write the host record directly.
- **Purge needs a full replica** (final review I1, ruling R24). `Services::purge` on a node whose store is
  not Full (a client-role `meshd`, or a client re-invited as a host before its rebuild) is refused with
  `ServicesError::NotFullReplica` before anything is written; the admin API answers `WrongState` with that
  message, which `meshd purge` and the UI show. Otherwise the generation bump would replicate while the
  old namespace's tombstones could not be written (`OutOfScope`), leaving the old data on every host for
  good. No other admin operation writes outside `_mesh` after a `_mesh` write: deploy adds blobs to the
  local blob store (not the replicated store) and writes the manifest in `_mesh`; placement, limits,
  outbound and undeploy are `_mesh` only; service KV writes run only in services, which a non-Full node
  never serves.

### 10.4 `mesh-forward` (§2)
- **The crate.** The gateway's forwarding moved into a new crate, `crates/mesh-forward`. It holds
  placement, manifest reads, `RemoteForwarder` and the `_ping` probe, with no WASM runtime, services crate
  or HTTP server. `mesh-gateway` and `mesh-services` use it through re-exports. `hyper` is banned only as a
  direct dependency, because iroh pulls it in transitively.
- **Attempt limits.** `forward(service, req, slack)` gives each attempt the manifest's `request_timeout_ms`
  plus `slack`; the gateway and the client both pass 5 s. The connect phase of an attempt is capped at
  **5 s**, and a connect timeout counts as a failure to connect (ruling R15 M6).
- **Results.** When no host answered, `forward` returns **`Timeout` if some attempt reached its host and
  timed out, else `NoHost`** (ruling R11). It can also return `NotDeployed` and `Unframeable`
  (431/414/413/400).

### 10.5 `mesh-client` (§4)
- **Local answers.** `request` answers locally with `400` for a path that does not start with `/`, `413`
  for a body over 16 MiB, and `431`/`414` for headers or a path too large for an `httpfwd` frame.
- **Timeout.** `request_timeout` bounds the whole call, all attempts included, and gives `Timeout` (ruling
  R12).
- **Join retry** (ruling R15). A directory counts as joined once it holds `mesh.json`. `node.key` is never
  deleted:
  - after a failed join, or one whose reply was lost, the next `join` reuses the key, so a retry with the
    same ticket succeeds;
  - a dir holding only `node.key` accepts a join.

  `join` returns before the first sync completes.
- **`open`** needs `node.key`, `mesh.json` and `role.json` = client. A host's data dir gives `NotJoined`.
- **Locking and teardown.** `client.lock` is an exclusive OS file lock. `shutdown()` releases it after the
  node has torn down. A dropped client tears down on the Tokio runtime it started on (ruling R16), also
  when it is dropped off that runtime, and keeps the lock until it is done. A redb "already open" error
  maps to `AlreadyOpen`.
- **Events.** `Online` and `Offline` follow whether any host is reachable. `ServicesChanged` follows
  changes to `service/*` keys.

### 10.6 `mesh-client-ffi` (§5, §6.4)
- **`MeshClientAndroid.init(context)`** is a hand-written Kotlin object over a raw JNI export,
  `Java_net_meshnode_client_MeshClientAndroid_initAndroid`. iroh's networking reads the Android context,
  so on Android `join` and `open` fail with `NotInitialised` until `init` has run. Once the library is
  loaded, `init` never throws; if it fails, the reason is in `NotInitialised`'s message. Loading the library
  happens in the object's initializer, so a missing or wrong-ABI `.so` surfaces from the first `init` as
  `UnsatisfiedLinkError` (or `ExceptionInInitializerError`).
- **Extra error variants.**
  - `InvalidArgument`: a bad hex node id, or `requestTimeoutMs` = 0.
  - `Closed`: any call during or after `shutdown`. `status()`, `members()`, `services()` and
    `setListener` throw it too.
  - `NotInitialised`: see above.

  A request that cannot be built gets a local `400`.
- **The AAR** (`scripts/build-client-aar.sh`, a release build with symbols stripped) contains
  `proguard.txt`, the consumer R8 rules that keep JNA and `net.meshnode.client`. It does **not** declare
  its runtime dependencies: the app adds JNA 5.14.0 (`@aar`) and `kotlinx-coroutines-android` itself, as
  the README says.
- **The bindings** are generated unformatted (`--no-format`, no ktlint), so a regeneration and `--check`
  produce the same bytes.
- **The JVM test is narrower than §6.4.** `crates/mesh-client-ffi/jvm-test` (Gradle, JDK 17, JNA) loads
  the host library and checks error mapping: `InvalidTicket` for a bad ticket, `NotJoined` for an empty
  dir. It also compiles the README's Kotlin example. It does not run join → request against an in-process
  mesh; the wrapper's Rust tests (`tests/ffi.rs`) cover that.
- **Threads and cancellation.** `onEvent` runs on a library worker thread. A coroutine cancelled during
  `join` does not stop the join: it completes in the background, and the client is then torn down.

### 10.7 Tests and CI (§6)
- **Success criterion 5.** The in-process test is `criterion5_client_via_remaining_host` (mesh-client).
  The real-device run is in `docs/manual-checks/sub-spec-8a.md` (ruling R23).
- **CI** has two new jobs:
  - **`mesh-client`:** the tests of mesh-forward, mesh-client and mesh-client-ffi, plus a
    `cargo tree -p … -e normal` check on mesh-client and mesh-forward. Plain `cargo tree` shows wasmtime
    through the dev-dependencies, which is expected (ruling R20).
  - **`mesh-client-ffi-android`:** the bindings `--check`, the AAR built for both ABIs with `cargo ndk`,
    and the Gradle JVM test against the host library built on Linux (ruling R21).

  Neither job has run yet (ruling R22).

### 10.8 Known limits and deferred minors
- **Lone stale host.** It stays Stale until a Fresh Full peer appears or the owner runs `declare-fresh`.
- **Joiner trusts its ticket's role.** If that role ever differs from the inviter's record, the joiner is
  refused and stuck.
- **Taint dropped mid-round.** The per-session stale-op taint is dropped if a session ends in the middle of
  a round. In a race, stale data may then pass as Fresh. No data is lost.
- **Restart after a re-bootstrap.** Every session restarts once after a re-bootstrap, and the redial
  backoff can reach 5 minutes.
- **Scope check outside the clock mutex** (Task 2). Harmless, because at runtime the scope only ever
  widens (the upgrade).
- **Test gaps** (Task 4):
  - `marker_against_a_pre_reset_hello_never_lands` was RED only through timing, and the `decide` re-check
    in the `Covered` apply is untested (t1);
  - the record-path tests no longer reach `maybe_complete` (t2);
  - no test pins the round-6 restart for a Partial peer spanning a reset (t3);
  - some upgrade tests overlap (m2).
- **Rare stalls in `stale_partial` tests.** `NotEffective` at `c.invite` happens about once in 60 runs.
  `stale_host_not_refreshed_by_client_fresh_via_other_host` stalled before the round-2 fix. No cause was
  found beyond the mechanisms already fixed.
- **Duplication around mesh-forward** (Task 5, Low):
  - two different `read_manifest` functions, one in mesh-services and one in mesh-forward;
  - the paged `service/` listing exists three times;
  - `mesh-gateway/src/error.rs` has a doubled doc line.
- **F1** (Task 6). A lone `node.key` kept after a failed join can be reused to join another mesh, so one
  node id becomes a (ghost) member of two meshes. If the ghost is removed before a retry, the dir is stuck
  for that mesh and reports `InvalidTicket`. Fix later: a `joining.json {mesh}` next to the key, with a new
  key for another mesh.
- **F3** (Task 6). The 5 s connect cap may abandon a slow cold connect (relay plus hole-punching on a poor
  mobile link). Make it a timing setting if field reports show `NoHost` on poor links.
- **M1: no stale indicator** (final review). Neither `meshd status` nor the UI shows that a host is stale,
  so an owner whose lone host stays stale (tombstone GC skipped) has no cue to run `declare-fresh`.
  Follow-up: a freshness field in the admin `Status` view, shown in `meshd status` and next to
  **Declare up to date…**.
- **M2: an SDK client unused for over 80 days resets** on the next Fresh host's `Hello`. Until its `_mesh`
  is rebuilt (seconds), `request` gives `NoHost`, `services()` is empty, and `invite`/`remove` give
  `Internal("…re-bootstrapping")`. It heals by itself.
- **M5: node-state errors surface as `Internal`** in the SDK. `Rebootstrapping`, `NotEffective` and
  `NeverSynced` all map to `ClientError::Internal(String)`, so an app cannot tell "try again shortly" from
  a bug. Follow-up together with M2: a dedicated, retryable variant.
- **Android untested on a device.** The AAR, `MeshClientAndroid.init` and JNA on Android have never run on
  a device; the manual check covers this. A listener that blocks holds one of the library's two worker
  threads.
