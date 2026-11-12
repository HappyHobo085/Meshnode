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
