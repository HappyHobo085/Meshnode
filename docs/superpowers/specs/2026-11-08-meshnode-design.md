# Meshnode — Design (whole project)

**Date:** 2026-11-08
**Status:** Draft, awaiting owner review
**Scope:** The finished system. Implementation is split into smaller sub-specs; see §14.

---

## 1. Intent

### What the owner asked for
- One app/system that installs on any device and turns the owner's devices into an **always-on, self-hosted server mesh**.
- **A redundant mesh:** several devices run it and replicate the hosted services and their data. Services stay up while at least one node is alive.
- **Phones are full hosts** (Android). iOS is out of scope as a host, because it doesn't allow long-running background servers.
- **Private only for this version.** Only the owner's own devices reach services. Public internet access is deferred (§15).
- Hosted services are **WASM modules** (the Aegis sync-server after a port, other apps) plus **static websites**.
- When the network splits, **every node keeps serving** and replicas merge later (no leader, no quorum).
- Networking is **iroh** (self-contained, addressed by key, punches through NAT, relay fallback). No Tailscale dependency.
- Apps reach services through **both** a localhost gateway and an embeddable iroh client library.
- Devices join with **invite tickets, and any member can invite or remove**.
- Implementation stack (approach A): **one Rust core** (iroh, iroh-blobs, wasmtime, our own LWW store on redb) in a desktop daemon and an Android foreground service, with a **Tauri 2 management UI**.

### Assumptions (stated by the designer, not the owner)
- Mesh size: 2–10 devices. Data per service: MBs to low GBs.
- Single owner. Everyone in the mesh is trusted, but devices can be lost or stolen.
- A conflicting concurrent write resolves deterministically (last-writer-wins, §5). That is acceptable for the services in scope.

### Success criteria for the finished project
Run on **one desktop node (Linux or Windows) and one Android phone**, on different networks (the phone on mobile data at least once):

1. **Install and always on.** The desktop installer sets up a system service that starts at boot without anyone logging in. The Android APK runs as a foreground service that comes back after a reboot (after the first unlock) and after the app is swiped away.
2. **Pairing.** The desktop UI shows an invite QR code, the phone app scans it, and both show each other as online members within 30 s, including when the phone is on mobile data (the relay fallback is fine).
3. **Hosting on both.** A static site and the Aegis sync service, deployed from either device, are served by **both** gateways. The phone's browser loads the site from `http://<site>.mesh.localhost:7777`.
4. **Real use.** Aegis desktop and Aegis Android each sync bookmarks through **their own device's** gateway. Edits made on both while the phone is in airplane mode converge after it reconnects, with the same bookmark set on both.
5. **Survives a node going down.** With the desktop powered off, the phone keeps serving every service, both locally and to a client-role device that forwards through it.
6. **Removal.** Removing the phone from the desktop (or the reverse) makes every member refuse it within 60 s of the removal reaching them, and it receives no further updates.
7. **Battery (measured, not guessed).** With the screen off on battery for 24 h on Wi-Fi, the phone stays a reachable member and applies a desktop write within 60 s. Idle drain is recorded in `docs/e2e-results.md`. The target is ≤ 5 % per 24 h with the default power policy (§10.2).

---

## 2. Platforms

| Platform | Role | Packaging | Background mechanism |
|---|---|---|---|
| Linux (x86_64, aarch64) | host / client | `.deb`, `.rpm`, tarball | systemd system unit |
| Windows 10/11 (x86_64) | host / client | MSI (WiX via Tauri bundler) | Windows Service (`windows-service` crate) |
| macOS (aarch64, x86_64) | host / client, best-effort | `.dmg` | launchd LaunchDaemon |
| Android 10+ (arm64-v8a, x86_64 emulator) | host / client | APK (sideloaded; Play Store out of scope) | foreground service, type `specialUse` |
| iOS | not supported as a host; may use `mesh-client` inside an app later | — | — |

---

## 3. Architecture overview

```
 ┌──────────────────────────── node (meshd on desktop / in-process on Android) ─────────────────────────────┐
 │                                                                                                           │
 │  Gateway (HTTP 127.0.0.1)          Peer protocols (iroh ALPNs)                    Admin API               │
 │        │                  join · sync · http-forward · blobs              (socket / in-process) ◄── UI      │
 │        ▼                                    │                                         │                   │
 │  Service Router ─────────► Runtime (wasmtime, wasi:http) ◄──────── Deployer + Placement (manifests)       │
 │        │                         │ meshnode:kv imports                                │                   │
 │        │ static sites            ▼                                                    │                   │
 │        └──────────────► Replicated Store (redb: ordered LWW registers + grow-only sets) ◄┘                  │
 │                                  │  ▲                                                                     │
 │                         Replicator (live push + version-vector catch-up + tombstone GC)                    │
 │                                  │                                                                        │
 │                         Membership (signed member set, removed set)                                       │
 │                                  │                                                                        │
 │                         Transport (iroh Endpoint, identity = node key)                                    │
 └───────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

The in-process admin transport (`InProcessAdmin`, with the same request dispatch, state gating and upload validation as the socket server) exists, and the node's lifecycle (`Daemon`) is shared by `meshd` and the Android library through `mesh-host` *(back-ported from sub-spec 5)*.

### Repository layout (one Cargo workspace + Android project)

| Path | Purpose | Depends on |
|---|---|---|
| `crates/mesh-hlc` | Hybrid logical clock. Pure, no I/O. | — |
| `crates/mesh-store` | Replicated store: ordered LWW registers + grow-only sets on `redb`; merge function; per-origin op log; tombstone GC; a store scope (`Full` or `Partial`, writes outside it refused with `StoreError::OutOfScope`) and scoped catch-up diffs (`diff_since_scoped`) *(back-ported from sub-spec 8a)* | `mesh-hlc` |
| `crates/mesh-proto` | Wire types for every ALPN (serde + postcard), invite tickets, signatures | `mesh-hlc` |
| `crates/mesh-core` | `Node`: transport, membership, replicator (partial replication for client-role nodes, the role-upgrade re-bootstrap and `declare_fresh` *(back-ported from sub-spec 8a)*), blob fetch; stores and replicates member tags; extra gated protocols (`NodeConfig.extra_protocols`, registered on the `Router` behind the membership `Gate`) and `NodeHandle::connect(peer, alpn)`, so it stays HTTP-agnostic *(back-ported from sub-spec 4)*. The deployer and placement live in `mesh-services` *(back-ported from sub-spec 3)* | store, proto, `iroh`, `iroh-blobs` |
| `crates/mesh-runtime` | Pure wasmtime host with no `mesh-core` or `mesh-store` dependency: the `wasi:http` incoming handler, `meshnode:kv` (through a trait the embedder implements), the outgoing-HTTP allowlist, limits *(back-ported from sub-spec 3)* | `mesh-proto`, `wasmtime`, `wasmtime-wasi-http` *(`mesh-proto` amended in final review)* |
| `crates/mesh-services` | Manifests, deploy/undeploy/purge, placement, the deployer, static sites, service blob GC; service KV writes go through `mesh-core`'s gated `StoreHandle` *(back-ported from sub-spec 3)* | core, runtime |
| `crates/mesh-gateway` | localhost HTTP server, Host and port routing, the `httpfwd` serving side; forwards to a peer through `mesh-forward` *(back-ported from sub-spec 8a)*; serves through `mesh-services` | core, services, proto, `mesh-forward` |
| `crates/mesh-forward` | Placement, manifest reads and the remote `httpfwd` forwarder (host picking by RTT, up to 3 attempts, connection cache, the `_ping` probe) without the WASM runtime, the services crate or an HTTP server, so a thin client can link it; the gateway and `mesh-services` use it too *(back-ported from sub-spec 8a)* | core, proto, `iroh` |
| `crates/mesh-host` | Everything `meshd`'s `Daemon` does except transports and OS services: start / run / shutdown, init / join / reset-identity, config (`HostConfig`, file `meshd.toml`), the `meshd.lock` data-dir lock, logging setup, the admin backend, `HostOptions` (periodic mode, power hooks). Platform-neutral; builds for `aarch64-linux-android` *(back-ported from sub-spec 5)* | core, services, gateway, admin, proto |
| `crates/mesh-admin` | Admin API: request/response types plus two transports (local socket, in-process channel) *(back-ported from sub-spec 5)* | core, proto *(not `mesh-services`: amended in final review)* |
| `crates/mesh-client` | The SDK for apps (§8): a client-role node that replicates only `_mesh` and sends HTTP to services over `httpfwd`. It never runs services, serves `httpfwd`, opens a gateway or fetches blobs, and links no WASM runtime (`cargo tree -p mesh-client -e normal` has no `wasmtime`, checked by a test and in CI) *(back-ported from sub-spec 8a)* | core, proto, `mesh-forward` *(back-ported from sub-spec 8a)* |
| `crates/mesh-client-ffi` | UniFFI wrapper of `mesh-client` (cdylib) for Kotlin, package `net.meshnode.client`. The generated bindings are committed (`scripts/gen-client-bindings.sh`, `--check` in CI); `scripts/build-client-aar.sh` builds a local AAR for `arm64-v8a` and `x86_64`; `jvm-test/` is a Gradle JVM test of the bindings *(back-ported from sub-spec 8a)* | `mesh-client` |
| `crates/mesh-ffi` | UniFFI bindings exposing the host's start/stop/status, wake, power and admin calls to Kotlin, plus the raw JNI `init_android` export *(back-ported from sub-spec 5)* | host, admin *(back-ported from sub-spec 5)* |
| `apps/meshd` | Desktop daemon + CLI (a thin layer over `mesh-host`); Windows-service / launchd / systemd integration; `meshd probe` *(back-ported from sub-spec 5)* | host, admin *(back-ported from sub-spec 5)* |
| `crates/mesh-ui` | The shared UI command layer: typed commands over an `AdminTransport` trait (desktop admin pipe or socket; Android in-process), UI-shaped DTOs and errors with hints, the invite QR as SVG, bounded streams (logs, probe), deploy file sources; ts-rs generates the TypeScript types *(back-ported from sub-spec 6)* | admin |
| `crates/mesh-ui-tauri` | The `#[tauri::command]` wrappers and `Channel` plumbing over `mesh-ui`, shared by both shells. Its own Cargo workspace, outside the root one, so root CI needs no webview packages *(back-ported from sub-spec 6)* | `mesh-ui`, `tauri` |
| `apps/ui` | The Svelte 5 + Vite + TypeScript frontend (one codebase for desktop and Android), built to `apps/ui/dist` and loaded by both shells under a strict CSP; the generated ts-rs types are checked in *(back-ported from sub-spec 6)* | `mesh-ui` types (generated) |
| `apps/desktop` | The desktop Tauri 2 app. `apps/desktop/src-tauri` is its own Cargo workspace and `Cargo.lock`; finds or starts the daemon, dialogs, `open_url`, elevated service install *(back-ported from sub-spec 6)* | `mesh-ui-tauri`, `apps/ui` |
| `apps/android` | The Android Tauri 2 project + Kotlin foreground service, receivers, workers, onboarding. `apps/android/src-tauri` is its own Cargo workspace (so desktop CI needs no webview system packages), builds the single `cdylib` `libmeshnode_app.so` that links `mesh-ffi`, and carries its own `Cargo.lock` *(back-ported from sub-spec 5)* | `mesh-ffi`, `mesh-ui-tauri`, `apps/ui` *(sub-spec 6 replaced the placeholder page)* |
| `services/aegis-sync` | The Aegis sync-server as a Meshnode WASM component (§11). Its own Cargo workspace, built by `scripts/build-services.sh` to `wasm32-wasip2` *(back-ported from sub-spec 7)* | `aegis-sync-core` (vendored) |
| `vendor/aegis-sync-core` | A byte-identical copy of the Aegis `sync-server/core` crate (pure protocol logic and the contract vectors), with the Aegis commit recorded in `vendor/VENDORED.md`. Refreshed only by `scripts/sync-aegis-core.sh <aegis-checkout>`, verified with `--check` *(back-ported from sub-spec 7)* | — |
| `packaging/` | systemd unit, launchd plist, WiX fragments, deb/rpm metadata | — |

**Licence** *(back-ported from sub-spec 7)*: Meshnode is **AGPL-3.0-only** (root `LICENSE`, `[workspace.package] license`, every manifest). The vendored Aegis core is AGPL-3.0-only too; other vendored third-party code (for example `patches/ndk-context`) keeps its own licence.

Each crate is testable on its own. `mesh-hlc`, `mesh-store` and `mesh-proto` have no networking.

---

## 4. Identity and transport

- Each node has an **Ed25519 node key** (its iroh `NodeId`), created on first run and stored at `<data>/node.key` with owner-only file permissions (on Android, in app-private storage).
- A **mesh** is identified by a 32-byte `MeshId = blake3("meshnode/mesh/1" ‖ founder NodeId ‖ nonce[32])`, derived from a genesis the founder creates when it starts the mesh. The founder is an implicit member, and the join reply carries `MeshGenesis { founder, nonce }`, which the joiner checks against the ticket's `mesh` *(back-ported from sub-spec 2)*. A node belongs to exactly one mesh.
- All peer traffic is iroh QUIC with these ALPNs, each versioned:
  - `meshnode/join/1`: redeem an invite ticket
  - `meshnode/sync/1`: catch-up (version-vector exchange + missing ops)
  - `meshnode/httpfwd/1`: forward an HTTP request to a node that hosts the service
  - iroh-blobs' ALPN: fetch module and site content by BLAKE3 hash
  - There is no gossip protocol: live replication runs over the per-peer `sync/1` sessions (a direct connection per peer; push on every local change) *(back-ported from sub-spec 2)*.
- **Every incoming connection is checked against membership** right after the handshake, using the remote `NodeId` iroh has authenticated. Non-members and removed nodes are closed with an error code. The only exception is `join/1`, which needs a valid ticket instead.
- **Finding peers:** each member's current direct addresses and home relay are stored in `_mesh/addr/<node_id>` (LWW, rewritten only when the node's address changes *(back-ported from sub-spec 2)*; `NodeAddrBytes` is the postcard encoding of iroh's `EndpointAddr`, converted in `mesh-core`), so the mesh carries its own address book. iroh's DNS/pkarr discovery is enabled as a second path. On startup a node dials every known member.
- **Relays:** iroh's public relays by default. The relay URL is configurable, and self-hosting a relay is supported (`iroh-relay` binary, documented). Traffic is end-to-end encrypted, so relays see ciphertext only.
- **Keepalive:** desktop nodes keep a connection open to every member. Android follows its power policy (§10.2).

---

## 5. Replicated store

### Data model
Data is split into **namespaces**: one per service, plus the reserved `_mesh` namespace for system state.

- **Ordered LWW register:** `(ns, key) → Entry { order: bytes, hlc, origin: NodeId, value: Option<bytes> }`. `None` is a tombstone. The winner is the highest **`(order, hlc, origin)`**, compared as a tuple (`order` byte by byte). `order` lets a service impose its own ordering. The Aegis sync service puts the client's record HLC there, so a stale push that arrives later in mesh time still loses (§11). Plain writes use an empty `order`, which reduces to normal HLC last-writer-wins. A service must not mix the two kinds of write on the same key (documented in the WIT).
- **Grow-only set:** `(ns, set, member_bytes) → { hlc, origin }`. Members are never removed. Used for anything a merge must never undo (revocations, consumed tokens, registrations).

**The merge rule** is one pure function in `mesh-store`. It is commutative, associative and idempotent, and property tests check all three (§13).

### HLC
48-bit physical milliseconds + 16-bit counter, the same scheme the Aegis sync-server uses. On receiving an op, the local clock advances to `max(local, remote)`. An op whose HLC is more than **5 minutes** ahead of the receiver's wall clock is **deferred, not merged**: it's held and retried as the wall clock catches up, and dropped after 24 h. That way one device with a bad clock can't win every conflict forever or drag every other clock forward. A node's own ops are exempt from this check, so a re-bootstrap after the wall clock moved back does not block local writes *(back-ported from sub-spec 2)*.

### Op log, version vectors, catch-up
- Every local write becomes an **Op** `{ origin, seq, hlc, ns, payload, sig }`. `seq` is a per-origin counter that only goes up, and `sig` is the origin's Ed25519 signature.
- Ops are stored indexed by `(origin, seq)`. **Compaction:** an origin's older register op for a key is deleted once a newer op from the **same origin for that key** beats it under the merge rule. Grow-only set ops are never compacted.
- **Version vector:** `origin → highest seq covered`. A compacted range counts as covered.
- **Live path:** ops are pushed over the `sync/1` session to each connected peer *(back-ported from sub-spec 2)*. A receiver checks the signature and acceptance (§6), then merges.
- **Catch-up** (`sync/1`): when connecting to a peer, and every 60 s with a random online peer (on Android, also triggered by incoming traffic and a wall-clock alarm, because in-process timers stall in deep sleep; see §10.2), the nodes swap version vectors and each streams what the other is missing. Live push is best-effort; catch-up is what guarantees convergence. An op that is refused or deferred doesn't advance the version vector, so a later catch-up retries it.
- **Sync messages** *(back-ported from sub-spec 8a)*: `Hello { mesh, vv, scope, freshness }` opens a round. `scope` is `Full` or `Partial(namespaces)` (§6: by role); `freshness` is `Fresh`, `Stale { since_ms }` or `Empty` (never completed a full sync), see Tombstone GC. `Ops`, `Covered { origin, upto }` and `Done` carry the round, and `Advertise { vv }` raises the bound the receiver checks `Covered` markers against: in live mode a node sends it before a marker above what its `Hello` announced, so a live marker never exceeds what the peer was told. `Advertise` before the peer's first `Hello` is a protocol error and ends the session. `scope` and `freshness` are required: wire compatibility with earlier builds was not kept (the owner's decision; no mesh held data yet), and the ALPN stays `meshnode/sync/1`.
- **Partial replication** *(back-ported from sub-spec 8a)*: a client-role node's store is `Partial(["_mesh"])` (the only partial scope in v1). To a peer that announced a Partial scope a node serves `diff_since_scoped`: like the full diff, but each run of ops outside the scope becomes a `Covered { origin, upto }` marker (merged with adjacent compacted runs), so the peer's version vector moves past them. One call walks at most 4 × `limit` op rows, filtered ones included; when that budget runs out inside a filtered run, the page ends with a marker up to the last row walked and the next page resumes there, so a client's first catch-up never holds one long read transaction. The join reply's catch-up to a client joiner is scoped the same way. A node **ignores every `Covered` marker from a Partial peer** (from it a marker may mean "filtered", not "compacted") and applies only its ops; gaps wait in `pending` until a Full peer fills them, and a Partial peer's `vv[me]` is no proof that it holds our ops. A Partial store refuses local writes outside its scope (`StoreError::OutOfScope`), so no code path on a client creates ops other nodes would need. Compaction, tombstone GC, removal and the write gate are unchanged.

### Tombstone GC
- A register tombstone, and the op behind it, is purged once it is older than **90 days** (by HLC).
- Every node records `last_full_sync_ms`: the last time it completed catch-up with at least one host. Which rounds count is refined below *(sub-spec 8a)*.
- A node whose `last_full_sync_ms` is older than **80 days** (the GC horizon minus a margin) is **stale**. On reconnect it must **re-bootstrap**: it sends ops written after its last sync to a peer, then replaces its replica with a full catch-up from that peer. Older local data is dropped, because otherwise the node could bring back deleted keys. Own `Set` ops older than the tombstone TTL are not re-sent (a tombstone other replicas already GC'd would be resurrected); they are reported to the user as expired offline edits *(back-ported from sub-spec 2)*. The UI shows a warning before this happens and records it in the log.
- **Staleness and re-bootstrap as built** *(back-ported from sub-spec 8a, rulings C1c, C1d, C1e)*. Each `Hello` announces the sender's freshness: `Fresh`, `Stale { since_ms }` (also while it re-bootstraps) or `Empty`. A completed round records, by the peer's announced freshness:
  - **a Fresh Full peer:** a full sync (the time, and `synced_self_seq` advances to the peer's `vv[me]`);
  - **a Fresh Partial peer (a client):** the time only, and only to keep an existing, non-stale time current. A client never rescues a stale node and never makes an `Empty` one Fresh, so only the founder (which starts Fresh) or a Fresh Full peer makes a host Fresh, and a stale node turns Fresh only by re-bootstrapping from a Fresh Full peer;
  - **a Stale peer:** nothing, except that an `Empty` store inherits the peer's staleness, its `since_ms` clamped to at most now − 80 days − 1 ms;
  - **an Empty peer:** nothing (two Empty stores stay Empty).
  
  A round in which this store applied ops from a Stale peer then lowers its time to that peer's (clamped) `since_ms`, even if a time was set: it holds stale data now.
- **Staleness never blocks syncing.** The only reset trigger is a **Fresh Full** peer's `Hello` reaching a stale Full node (or a client due for a role upgrade, §6), which then re-bootstraps before serving that peer anything. Stale nodes keep syncing with stale, Empty and Partial peers, so a mesh in which every node is stale stays stale but fully working until a Fresh Full node appears; a lone host with only clients stays Fresh. A stale client also resets on a Fresh host's `Hello` (its `_mesh` data is fully re-derivable). A stale store skips tombstone GC.
- **During a re-bootstrap** (serving paused) the store takes data only from Fresh Full peers (and `_mesh` from clients); a session with any other Full peer ends before anything from it is applied. The run completes only after a Fresh Full peer answered with `Done` a `Hello` sent after the reset, and the finished store records a full sync. The reset and the start of the new re-bootstrap epoch are one store call under one lock, which a session also holds when it decides on a reset and when it records a completed round. So:
  - a round whose `Hello` went out before the reset records nothing (no time, no inheritance, no taint);
  - a session whose first `Hello` predates the reset ends with `SessionEnd::Reset` when the peer is Full (or not yet known) and is redialed at once. A `Covered` marker computed against the pre-reset vector therefore never lands in the emptied store; the marker's apply re-checks the epoch under the same lock;
  - while the store is being rebuilt (after the reset, until the run finishes) a Fresh Full peer's `Hello` decides nothing: the sources the rebuild needs are never turned away, even by a stale time written into the store being rebuilt. Before the reset (decided, not yet done) a Fresh Full peer still ends the session, which restarts with a fresh `Hello` once the store is reset.
- **Declare up to date** *(back-ported from sub-spec 8a)*: a host left on its own for over 80 days stays Stale for good (no Fresh Full peer can come), which skips tombstone GC. The owner can declare it fresh: `Node::declare_fresh()`, admin request `DeclareFresh`, `meshd declare-fresh --yes` (without `--yes` it prints the risk and changes nothing), and in the UI Settings, **Declare up to date…** behind a type-to-confirm dialog. It refreshes only the time, never `synced_self_seq`. It is refused while a re-bootstrap runs or a session has decided on one, once removed, and for a store that never completed a full sync (`NeverSynced`). The risk, which `meshd` and the UI state: deletions other hosts already cleaned up may come back through this node.

### Storage
One `redb` file per node at `<data>/store.redb`. A merge and its op-log insert happen in one transaction. The UI shows the size of each namespace.

---

## 6. Membership and invites

All of this lives in the `_mesh` namespace, so it replicates and merges like any other data.

- **Members:** grow-only set with entries `{ node_id, name, role: host|client, platform, added_by, added_hlc }`.
- **Removed:** grow-only set with entries `{ node_id, removed_by, cutoff_seq }`, where `cutoff_seq` is the highest seq of that node the remover had seen with no gaps.
- **Effective members** = members − removed. A removed `NodeId` can never come back; a device can rejoin only with a new key and a new invite (the UI calls this "reset identity").
- **Op acceptance:** an op from origin `X` is accepted if `X` is a member and either (a) `X` is not removed, or (b) the op's `seq ≤ cutoff_seq` (the **highest** cutoff if `X` was removed more than once). A node that learns of a removal retracts `X`'s ops above the cutoff, and compaction waits a 7-day settle period so retraction can restore older values. Full rules: sub-spec 1 §2 and §5.6.
- **Removing:** any effective member can remove any member, including itself (sign-out). The **last effective host can't be removed**, because a mesh with no host can't serve and can't recover in-band.
- **Removal records are never retracted** *(back-ported from sub-spec 2)*: the `_mesh` removed-set rows are exempt from the cutoff rules, so the effective cutoff only rises and every node computes the same removed set in any arrival order. (Sub-spec 1 §5.6.)
- **Removal is a hard cutoff:** existing connections to a newly removed node are closed as soon as the removal is merged.
- **Scope by role** *(back-ported from sub-spec 8a)*: host-role nodes replicate everything (`Full`), client-role nodes only `_mesh` (`Partial(["_mesh"])`, §5). A node's scope is set when it opens, from its role in `_mesh` and from `role.json`, the role it joined with (the founder `host`, a joiner its ticket's role), written before `mesh.json`, so a joiner knows its scope before its own `members` record arrives. Either one saying client makes the store Partial. Every `Hello` announces the scope; a peer whose scope contradicts its role in `_mesh` (host ⇔ `Full`, client ⇔ `Partial(["_mesh"])`) is refused like an unknown member, and a peer with no role yet (a joiner) is taken at its word. Two cases follow from that:
  - **Listed as both client and host** (ruling R7): `members` is grow-only, so a client re-invited as a host stays listed as a client too. Such a peer may announce either scope; as Partial it is served scoped and its markers are ignored (always safe). Without this, a client re-invited while offline could never learn it, because every host would refuse its Partial `Hello`.
  - **A host that does not yet hold a node's host record** refuses that node's `Full` `Hello` as contradicting its (client) role. This heals by itself once the record reaches that host from another peer and the node redials.
- **Role upgrade** *(back-ported from sub-spec 8a)*: a node that joined as a client and whose effective role becomes host (host outranks client) re-bootstraps as Full, through the stale re-bootstrap's steps (§5). It starts only when a session meets a **Fresh Full** peer, as a stale reset does; until then it stays a working client. From its reset on, its `Hello`s announce `Full` (and `Stale`, as any re-bootstrap does) and it rebuilds only from Fresh Full peers. Only when the rebuild is done does the store's scope flip to `Full` (before serving resumes) and `role.json` become `host` (before `rebootstrap.bin` is removed, so a crash resumes as Full); a failed run, including one that fails after the flip, is retried. While a node's store is not Full or is re-bootstrapping, its gateway forwards every request instead of serving it, and its `httpfwd` answers the retryable `503 Node re-bootstrapping`, so a forwarder tries the next host (§8). **No product path promotes a client yet:** `join` refuses a data directory that is already in a mesh, and there is no promote request. The tests write the host record directly; the machinery is ready for a future re-invite or promote command.
- **Honest limits** (shown in the UI and CLI help):
  - A removed device keeps whatever data it already replicated.
  - Removal takes effect on each node only once that node has received it.
  - Two nodes that remove each other during a split are both removed only if both records reach a node; a node that applies one removal closes the other remover, so the second record may never spread and then only the first-removed node is removed everywhere. If the nodes were the last two effective hosts, the mesh has no host and cannot recover in-band. A stolen device that removes other members *before its own removal reaches every node* has those removals honoured, and a removal record it authored after its own cutoff is honoured mesh-wide if it reaches any node still unaware of its removal; its data writes are still retracted *(back-ported from sub-spec 2)*.
  - If a removal reaches a node more than 7 days after it compacted the removed node's older value for a key, that node can't restore it (logged; sub-spec 1 §5.6).

### Invites
- `invite(role, ttl = 15 min)` returns a ticket `{ mesh_id, inviter_node_addr, token: 32 random bytes, role, expires_at }`, encoded as base32 text and as a QR code (shown in the UI and in the terminal).
- The inviter keeps `blake3(token)` and its expiry locally. Only the issuing node can redeem it.
- The joining node calls `join(ticket)`. Over `join/1` it presents the token, and the inviter checks it is unexpired and unused. The inviter then adds the joiner to `members`, adds `blake3(token)` to the grow-only set `consumed_invites`, and replies with the `mesh_id`, current member addresses, and a full catch-up (client-role nodes get only `_mesh`).

---

## 7. Services

### Manifest
Stored as the LWW register `_mesh/service/<name>`:

```
ServiceManifest {
  name: "sync",                     // [a-z0-9-]{1,32}; also the gateway hostname label
  kind: Wasm { module: Blake3Hash } | Static { root: Blake3Hash /* iroh-blobs collection */ },
  version: u64,                     // increases on every deploy
  limits: { memory_mb: 64, request_timeout_ms: 10_000, max_body_bytes: 8 MiB },
  placement: AllHosts | Tags(["desktop"]) | Nodes([NodeId…]),
  outbound: [ "api.example.com:443", … ],   // allowlist; empty = no network (default)
}
```

- **Placement:** only effective host-role members run services; client-role nodes never fetch or run them and forward requests *(back-ported from sub-spec 3)*. Each host has tags (`desktop`, `android`, `always-plugged`, plus any the user adds) stored with its member entry. A host runs a service only if it matches the placement. Hosts that don't run a service still replicate its **data** (so they can take it over immediately when placement changes) but don't fetch its blobs or run it. Their gateways forward requests for it (§8). If placement matches no host, the UI warns and the service returns `503`.
- **Deploy:** the UI or CLI adds the module or site folder to the local iroh-blobs store, then writes the manifest. `undeploy` deletes the manifest; the data stays until `purge`. A service's data lives in namespace `<name>` for generation 0 and `<name>.<gen>` after a purge; `purge` starts a new generation (so keys and grow-only sets written concurrently on other nodes are hidden too) instead of tombstoning key by key *(back-ported from sub-spec 3)*.
- **Deployer loop:** whenever a manifest changes, each matching host fetches missing blobs from any peer, verifies the hash, and atomically swaps to the new version. Until then the previous version keeps serving. A service that has never been fetched answers `503 Service not yet replicated`.
- **Blob GC:** blobs no manifest has referenced for 24 h are deleted.

### WASM runtime
- **wasmtime, Component Model.** A service is a component targeting world `meshnode:service/service`:
  - **exports** `wasi:http/incoming-handler` (the standard WASI HTTP proxy interface, so any toolchain that supports `wasi:http` works)
  - **imports** `wasi:clocks`, `wasi:random`, `wasi:cli` stdio (the guest's stdout and stderr go to the node log line by line, tagged with the service name; wasmtime-wasi has no `wasi:logging`) *(back-ported from sub-spec 3)*, `meshnode:kv/store`, `meshnode:kv/local` *(back-ported from sub-spec 7)*, and `wasi:http/outgoing-handler`, which is **always linked** but refuses every destination not in `outbound`.

```wit
package meshnode:kv;

interface store {
  get: func(key: string) -> option<list<u8>>;
  set: func(key: string, value: list<u8>);
  // Ordered write: wins only over entries whose (order, hlc, origin) is lower. Do not mix with `set` on one key.
  set-ordered: func(key: string, order: list<u8>, value: option<list<u8>>);
  delete: func(key: string);
  list-keys: func(prefix: string, start-after: option<string>, limit: u32) -> list<string>;
  set-add: func(set: string, member: list<u8>);   // grow-only
  set-contains: func(set: string, member: list<u8>) -> bool;
  set-members: func(set: string, start-after: option<list<u8>>, limit: u32) -> list<list<u8>>;
  node-id: func() -> list<u8>;                    // for per-node bookkeeping (e.g. soft caps)
}

// Node-local, never replicated *(back-ported from sub-spec 7)*
interface local {
  get: func(key: string) -> option<list<u8>>;
  set: func(key: string, value: list<u8>, ttl-ms: u64) -> result<_, string>;  // 0 = no expiry
  delete: func(key: string);
}
```

- **`kv/local`** *(back-ported from sub-spec 7)*: entries live in a node-local redb table keyed `(service, generation, key)` and are not in the op log or catch-up. A TTL entry is invisible once expired and is swept in the GC tick. Caps per service: value at most 64 KiB, key at most 512 bytes, at most 100 000 live keys (a breach returns `err`, not a trap). A count table and a due time keep the cap check and the sweep from scanning. Local writes **bypass the write gate** (they are not ops and consume no sequence number) and **survive `reset_to_empty`**, so replay nonces outlive a re-bootstrap. A purge (new generation) makes the old generation unreachable and the GC drops its namespaces. Services that do not import it are unaffected.

- **Isolation:** each service can reach only its own namespace. No filesystem access. Outgoing network only to the allowlist.
- **One fresh instance per request** (the wasi:http proxy model), from a cached `InstancePre` keyed by module hash.
- **Compiled-code cache:** on desktop, a file cache under `<data>/services/cache/` *(path amended in final review)*. **On Android, compiled code is cached as bytes and loaded with `Module::deserialize` into anonymous memory, never memory-mapped from an app-data file**, because Android blocks executing code mapped from files in app-writable storage. Verified in the Android spike (`spikes/android/RESULTS.md`). There is no interpreter fallback: Cranelift works in the app sandbox, so Pulley is left out *(back-ported from sub-spec 3)*.
- **Limits:** memory cap through `ResourceLimiter`, time cap through epoch interruption (10 ms ticks), body cap enforced before the module runs. A trap or timeout returns `500`/`504` and is logged; the node never crashes because of a service.
- Request and response bodies are buffered, not streamed, for WASM and static responses (bounded by the limits) *(back-ported from sub-spec 3)*. `max_body_bytes` (8 MiB by default) caps **responses** as well as requests, so a service must page its large responses *(back-ported from sub-spec 7)*.
- Writes made through `meshnode:kv` apply locally right away (read-your-writes on that node) and become ops for replication.

### Static sites
A built-in native handler serves files from an iroh-blobs collection: `index.html` for directories, `Content-Type` by extension, `ETag` = blob hash, `404` for missing paths.

---

## 8. Client access

### Local gateway
- Listens on `127.0.0.1:7777` (configurable; loopback only).
- **Routing by Host header:** `<service>.mesh.localhost[:port]` → that service. Browsers resolve `*.localhost` to loopback by themselves.
- **Port mappings** for clients that can't resolve `*.localhost`: node-local `ports = { 7801 = "sync" }` binds `127.0.0.1:7801` straight to a service. **Default mapping: `7801 → sync`** on every node, so Aegis's sync URL is always `http://127.0.0.1:7801`.
- **Dispatch:** if this node runs the service, it's handled locally. Otherwise (client role, not in placement, or not yet replicated) it's forwarded over `httpfwd` to the online host running it with the lowest recent round-trip time, trying the next host on failure, up to 3 attempts. If none work: `502`. A node whose store is not a full replica, or is re-bootstrapping, forwards every request too (its own data is incomplete) *(back-ported from sub-spec 8a)*. The forwarding code lives in `mesh-forward`, shared with `mesh-client` *(back-ported from sub-spec 8a)*.
- `http://mesh.localhost:7777/` serves a read-only status page.
- **Security:** any local process can reach the gateway, the same as any localhost server. Services handle their own user-level auth (the Aegis sync service does).

### `httpfwd` forward protocol
The protocol is named `httpfwd`, ALPN `meshnode/httpfwd/1` *(back-ported from sub-spec 4)*. One bidirectional QUIC stream per request: one length-prefixed `ForwardRequest { service, method, path, headers, body }` frame, answered by one length-prefixed `ForwardResponse { status, headers, body }` frame *(back-ported from sub-spec 4)*. Frames are capped at 64 MiB plus 64 KiB; deserialisation checks the limits (at most 128 headers and 64 KiB of header bytes, `path_and_query` at most 8 KiB and starting with `/`), the method, the service name and, for responses, a status of 100..=999. Header names and values are checked for CR/LF/NUL when converted to `http` types, and the forwarder refuses a response status below 200. The hop-by-hop headers (`connection`, `keep-alive`, `transfer-encoding`, `te`, `upgrade`, `proxy-authenticate`, `proxy-authorization`, `proxy-connection`) are stripped in both directions. Hosts accept it only from effective members. Hosts serve a forwarded request locally only and never forward it again. A host that is re-bootstrapping, or whose store is not Full (a client re-invited as a host, before its rebuild), answers `503 Node re-bootstrapping`; like `503 Service not placed on this node` and `503 Service not yet replicated`, a forwarder treats it as retryable and tries the next host *(back-ported from sub-spec 8a)*. Each attempt gets the manifest's `request_timeout_ms` plus a slack, of which connecting may take at most 5 s (a host listed online that no longer answers is left for the next one). When no host answered, the forwarder reports a timeout if some attempt reached its host and timed out, and no host otherwise *(back-ported from sub-spec 8a)*. Bodies stay buffered end to end (gateway, `httpfwd`, services), with caps *(back-ported from sub-spec 4)*.
- **Resource limits** *(back-ported from sub-spec 4)*: WASM requests share a node-wide memory budget (`ServicesConfig.max_memory_mb`; default half of physical RAM, 64 MiB to 4 GiB); a request that cannot get its share is answered `503 Memory budget exhausted`. Static-site requests count as in flight like WASM requests. On Android the runtime reserves memory compactly (`compact_memory`).
- **Reserved service `_ping`**: the `httpfwd` handler answers a `ForwardRequest` for service `_ping` with `204` before any service lookup (`_` is not valid in service names, so there is no collision); it is gated like all `httpfwd`, members only. It backs the latency probe *(back-ported from sub-spec 5)*.
- The gateway's HTTP keep-alive is 30 s, and a request body that does not arrive by its deadline is answered `408` *(back-ported from sub-spec 4)*.

### `mesh-client` library
*(Back-ported from sub-spec 8a; replaces the earlier sketch.)* The SDK for the owner's apps: a client-role member that replicates only `_mesh` (members, service manifests, placement; §5 partial replication) and sends HTTP to services over `httpfwd`. It never runs services, serves `httpfwd`, opens a gateway or fetches blobs, and it does not link the WASM runtime. Rust API (`crates/mesh-client`):

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

- **Errors** (`ClientError`): `InvalidTicket`, `AlreadyJoined`, `NotJoined`, `AlreadyOpen`, `Removed`, `NoHost`, `Timeout`, `Io`, `Internal`. A host's own answer, `4xx`/`5xx` included, is never an error.
- **`request`** forwards the method, path and query, headers (hop-by-hop ones dropped) and body to the online host with the lowest RTT in the service's placement, the next one on failure, up to 3 attempts (the `mesh-forward` code the gateway uses). Answered locally instead: `400` for a path that does not start with `/`, `413` for a body over 16 MiB, and `431`/`414` for headers or a path too large for an `httpfwd` frame. `NoHost`: no online host runs the service, it is not deployed, or no host could be connected to. `Timeout`: a host was reached but did not answer in time, or the whole call took longer than `request_timeout`, which bounds the call, all attempts included.
- **`join`** accepts only a client ticket (`InvalidTicket` before anything is written otherwise). A directory is joined once it holds `mesh.json`; such a directory gives `AlreadyJoined`. A failed or interrupted join keeps `node.key`, and the next `join` reuses it, so retrying with the same ticket completes a join whose reply was lost. `join` returns once joined; the first sync finishes in the background (`status().first_sync_done`).
- **`open`** needs a directory joined as a client (`node.key`, `mesh.json`, and `role.json` saying client): anything else, a host's data directory included, gives `NotJoined`. A role upgrade while a client runs is out of scope.
- **One client per data directory**: `client.lock` is held while it is open (`AlreadyOpen`). `shutdown()` releases the directory before it returns. Dropping a client tears its node down in the background on the Tokio runtime it started on, also when it is dropped on a thread without one; the directory stays locked until then.
- **Removal**: a removed client gets `ClientEvent::Removed` once, and `request`, `invite` and `remove` then fail with `Removed`. A client may invite and remove (any member may).
- **Example**: `crates/mesh-client/examples/fetch.rs` (`fetch join <dir> <ticket> <service> <path>`, `fetch open <dir> <service> <path>`).
- **Kotlin** (`crates/mesh-client-ffi`, UniFFI, package `net.meshnode.client`): `MeshClient.join`, `open`, `request(service, MeshRequest)`, `invite`, `remove` and `shutdown` are `suspend` functions; `status()`, `members()`, `services()` return records; events go to `MeshClientListener.onEvent` (on a library thread). Errors are the sealed `MeshClientException`, one per `ClientError` variant plus `InvalidArgument` (a bad node id, a `requestTimeoutMs` of 0), `Closed` (any call during or after `shutdown`) and `NotInitialised`. On Android, `MeshClientAndroid.init(context)` must run once before the first `join`/`open` (the networking code needs the application context); until then they fail with `NotInitialised`, whose message says why an `init` failed. A request that cannot be built (bad method, header or path) gets a local `400`. The AAR (`scripts/build-client-aar.sh`, built locally, not published) carries `libmesh_client_ffi.so` for `arm64-v8a` and `x86_64`, the compiled bindings, and `proguard.txt` (consumer R8 rules keeping JNA and `net.meshnode.client`). It does **not** declare its runtime dependencies: the app adds `net.java.dev.jna:jna:5.14.0@aar` and `kotlinx-coroutines-android` itself. Details: `crates/mesh-client/README.md`.

---

## 9. Admin API and management UI

### Admin API (`mesh-admin`)
One set of typed requests, used by the CLI and the UI:
`status`, `members`, `invite`, `join`, `remove`, `set_tags`, `services`, `deploy_wasm`, `deploy_static`, `undeploy`, `purge`, `set_placement`, `set_outbound`, `config_get/set`, `logs_tail`, `reset_identity`, `probe` *(back-ported from sub-spec 5)*.

- **Desktop:** served by `meshd` on a Unix socket (`<data>/admin.sock`, mode 0600) or a Windows named pipe (with an ACL limited to the installing user and Administrators).
- **Android:** an in-process channel (`InProcessAdmin`, called from Kotlin through `mesh-ffi`'s `admin`, `admin_deploy_static` and `admin_logs`) *(back-ported from sub-spec 5)*. The UI and the foreground service share one process and one Rust library.

### `meshd` CLI
`meshd init | join <ticket> | invite | members | remove <id> | deploy wasm|static … | services | status | logs | run`, plus `meshd service install|uninstall|start|stop|status` for the OS service, and `meshd probe <node-id-prefix> [--count 20] [--interval 5s] [--timeout 30s]` (latency probe *(back-ported from sub-spec 5)*: sends `_ping` requests directly to that member over a fresh connection per sample, and prints per-sample connect and exchange times, then p50 / p95 / max and failures; `--timeout` 1 s to 2 min). Each command calls the running daemon's admin API; except `init`, `join` and `reset-identity` (through the daemon when it is reachable, otherwise directly on the data directory) and `service …` (the OS), a command without a running daemon prints `meshd is not running (start it with "meshd run" or "meshd service install")` *(back-ported from sub-spec 4)*.

**Uninitialised daemon** *(back-ported from sub-spec 4)*: `meshd run` without `mesh.json` serves only the admin API, accepting `Init` and `Join` (plus `Status`, `ConfigGet`, `ConfigSet`, `LogsTail` and `ResetIdentity`; every other request is refused with a wrong-state error); on success it starts the node in place. `StatusInfo` includes the gateway's mapped `ports`.

**Data directory and services** *(back-ported from sub-spec 4)*: `meshd` takes an exclusive lock on the data directory (`meshd.lock`). `meshd service uninstall --purge-data` purges only the data directory read back from the installed registration, and refuses on a mismatch or when nothing is installed. `meshd service install` refuses a reparse point (symlink or junction) as the data directory and resets the owner to Builtin Administrators (BA) and the children's ACLs.

### Tauri 2 UI (`apps/ui`, one codebase for desktop + Android)
- **Home:** mesh status, a card for each member (online, platform, tags, last seen, replication lag), the node's own gateway URL.
- **Add device:** shows the invite QR + text, with a countdown until it expires.
- **Join a mesh:** scan a QR code (Tauri barcode-scanner plugin on Android; paste text on desktop).
- **Services:** list with version, placement, which hosts are running it and data size. Deploy (pick a `.wasm` file or a folder), change placement and outbound allowlist, undeploy and purge (with confirmation).
- **Device settings:** name, tags, gateway port mappings, relay URL, Android power policy, reset identity.
- **Logs:** a live tail.
- On desktop the UI is a normal app that connects to the running daemon and offers to install or start the service if none is running.

**Decisions** *(back-ported from sub-spec 6)*:
- **Polling, no events.** The UI polls `status`, `members` and `services` every 2 s while the page is visible, pauses when hidden, never overlaps requests and refreshes after each mutation. There is no daemon event push; Add device detects the joined member through the poll.
- **No-daemon actions (desktop).** The app probes the installed service's data directory, then the per-user daemon. With neither answering it offers **Run without installing** (`meshd run` as the user, detached, outliving the app), **Install as system service** (elevated: UAC on Windows, `pkexec` on Linux, `osascript` on macOS; a declined prompt is "Installation cancelled") and **Locate meshd…**. If both daemons answer, the service wins and the UI notes the second node.
- **Per-user daemon directory.** `%LOCALAPPDATA%\Meshnode`, `$XDG_DATA_HOME/meshnode` (else `~/.local/share/meshnode`) or `~/Library/Application Support/Meshnode`; its log is `meshd.log` there.
- **Hostname default.** First run offers the machine's host name (cleaned, at most 64 characters) as the device name, else "Windows PC" / "Mac" / "Linux PC"; on Android the phone model when the WebView user agent carries it, else "Android phone".
- **Removed device.** A device whose state is `removed` shows a dedicated screen instead of Home: it was removed by another device, can never rejoin with the same identity, and can only reset its identity (then create or join a mesh). Removing a device asks the user to type its name.
- **Streams.** Logs and the latency probe run as streams carrying `StreamEvent` (item, end, error) over a Tauri `Channel`; at most 4 concurrent streams per window; a lost stream shows "Disconnected — Resume".
- **Android.** While the node is stopped, the UI shows **Node stopped** with Start and admin calls return `NotRunning`. The camera permission is requested before scanning; a denial leaves the paste field.

---

## 10. Platform shells

### 10.1 Desktop daemon (`apps/meshd`)
- **Linux:** a systemd **system** unit `meshnode.service` running as a dedicated `meshnode` user, data in `/var/lib/meshnode`, `After=network-online.target`, `Restart=always`. iroh binds a wildcard UDP socket, so there's no bind-address race like the one behind the Aegis sync-server's tailnet outage.
- **Windows:** a Windows Service (`windows-service` crate) running as `LocalService`, data in `%ProgramData%\meshnode`, automatic (delayed) start, restart on failure. The MSI installs it along with the UI.
- **macOS:** a LaunchDaemon plist with `KeepAlive`, data in `/Library/Application Support/meshnode`.
- Logs go through `tracing` to a daily rolling file `<data>/logs/meshd.log.<date>` (7 kept) and to stdout; under systemd stdout reaches journald. On Windows only service start/stop and fatal startup errors go to the Event Log (source `Meshnode`) *(back-ported from sub-spec 4)*. macOS is best-effort: the launchd plist and install code are built in CI but not verified on a Mac *(back-ported from sub-spec 4)*.

### 10.2 Android (`apps/android`)
> Findings from the feasibility spike (`spikes/android/RESULTS.md`, Galaxy S23 / Android 17) are folded in below and marked *(spike)*.

- **One APK, one process:** the Tauri UI activity plus a Kotlin **foreground service** (`MeshService`). Both call the same Rust library (`mesh-ffi`, built with `cargo-ndk`, bindings generated by UniFFI). The service owns the `Node`; the UI talks to it in-process.
- **Android context setup *(spike)*:** when `mesh-ffi` first loads, it must call `ndk_context::initialize_android_context(vm, context)` with a global ref to the application `Context`, before any iroh endpoint is created. iroh's DNS resolver and network monitor need it; without it a worker thread panics (`android context was not initialized`). `init_android` is a raw JNI export of `mesh-ffi` (UniFFI cannot pass a `Context`), and `start` returns `NotInitialised` without it. Tauri's `tao` initialises the same global when the activity comes first, so the workspace patches `ndk-context` (`patches/ndk-context`, a ~100-line vendored 0.1.1 whose `initialize` is idempotent and race-free and keeps the first context): the service and the activity can start in either order *(back-ported from sub-spec 5)*.
- **WASM *(spike)*:** Cranelift compilation works inside the app sandbox (1.7–8 ms per request on the S23), and the in-memory compiled-code cache (§7) works as designed. Pulley is left out (no interpreter fallback).
- **Foreground service type `specialUse`** (`FOREGROUND_SERVICE_SPECIAL_USE`), with a permanent notification ("Meshnode: hosting N services · M peers online"), and Stop/Open actions. `specialUse` because `dataSync` is time-limited on Android 15+ and can't start from boot. Sideloaded distribution means no Play Store review of this type. The notification has **Stop** and **Open** actions; on a failed start it reads "Meshnode couldn't start: <reason> — tap to open" and the service stays in the foreground *(back-ported from sub-spec 5)*.
- **Starting automatically:** a `BOOT_COMPLETED` receiver starts the service after the first unlock (verified in the spike: it reconnected 4 s after boot). A `WorkManager` watchdog every 15 minutes restarts it if the system killed it. A `MY_PACKAGE_REPLACED` receiver restarts it after an update. **Stop** (the notification or the page) sets a persisted `user_stopped` flag: while set, boot, alarm, watchdog and package-replaced do not start the service; pressing Start in the app clears it. The restart decision `(trigger, user_stopped, running)` is a pure, unit-tested Kotlin function *(back-ported from sub-spec 5)*.
- **Swipe-away recovery *(spike)*:** on One UI, swiping the app out of recents **stops the node** despite the foreground service and battery exemption, and `START_STICKY` doesn't restart it. So `onTaskRemoved` schedules an exact alarm (`setExactAndAllowWhileIdle`, about 5 s out) whose receiver restarts the service, which is an allowed way to start a foreground service from the background. The WorkManager watchdog is the backstop. Onboarding tells the user to lock the app in recents. Without the exact-alarm permission the restart uses an inexact alarm. Android 12+ refuses a foreground-service start from the background unless an exemption applies: boot, package-replaced and an exact alarm are exempt, but the inexact alarm and the WorkManager watchdog are **refused without the battery-optimisation exemption**; a refused restart posts a notification ("Meshnode stopped in the background — tap to restart") instead of failing silently *(back-ported from sub-spec 5)*. To verify on the S23 (manual check of sub-spec 5).
- **Battery:** first-run onboarding (Kotlin, shown before the Tauri page) requests `POST_NOTIFICATIONS` (Android 13+), exact alarms and the **battery-optimisation exemption** (`REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`), each shown as granted or missing and none blocking the start *(back-ported from sub-spec 5)*; it links to per-manufacturer guidance (Samsung, Xiaomi, etc.) for aggressive app killers. *(spike)*: a foreground service kept network access in forced Doze even **without** the exemption, so the exemption is insurance against OEM app sleeping rather than a requirement.
- **Timers vs deep sleep *(spike)*:** with the screen off, the CPU was awake only ~15 % of the time. In-process timers run on the monotonic clock, which stops during suspend, so a 300 s timer fired every 18–43 min of wall time. Yet the QUIC connection survived overnight (one reconnect in 10 h), so incoming traffic does reach the sleeping phone. Design rule: **on Android, periodic work (catch-up, address refresh) must not depend on in-process timers alone.** It runs (a) whenever incoming traffic wakes the process, i.e. a catch-up is piggybacked on any incoming connection or `sync/1` message after ≥ 60 s of silence, and (b) on a wall-clock `AlarmManager` while-idle alarm (`setAndAllowWhileIdle`, inexact, every 15 min, re-armed each time; it calls `wake(Alarm)` under a 10 s wake lock), plus an optional extra wake lock of `[android] periodic_wake_s` seconds on each alarm (default 0 = off) *(back-ported from sub-spec 5)*. In Rust the host runs with `periodic: Driven`: the periodic catch-up and address refresh also run on `Daemon::wake()` and on incoming traffic after ≥ 60 s of silence.
- **Power policy** (setting):
  - **Balanced (default):** keep peer connections open with iroh's default keepalives. Hold a partial wake lock **only** while charging or while serving an incoming request or a catch-up. *(spike)*: with no wake lock at all, the spike used **~0.3 % battery per 24 h** (5.0 mAh over 10.2 h). **Still to measure early in sub-project 5:** how quickly a sleeping phone answers an incoming request, on Wi-Fi and on mobile data. If it's too slow for success criterion 7, Balanced gains a short periodic wake lock: this is built in as `periodic_wake_s` (default 0), and the sub-spec 5 manual check sets the default from the measured p95 (if p95 on a sleeping phone exceeds 60 s) using `meshd probe` *(back-ported from sub-spec 5)*.
  - **Always on:** hold a partial wake lock all the time.
  - **Saver:** no wake lock beyond the alarm receiver's 10 s. While the screen is off, the gateway serves only the phone's own apps and inbound `httpfwd` from peers is answered `503 Service not yet replicated` (the forwarder tries other hosts; a desktop gateway with no other host reports `502`); no piggybacked catch-up, only the 15-minute alarm. With the screen on it behaves normally *(back-ported from sub-spec 5)*.

  The policy is stored in `meshd.toml` (`[android] power = "balanced" | "always_on" | "saver"`, default `balanced`; the file is the single source of truth and Rust persists a change) and applied by a pure decision function `(policy, charging, screen_on, activity) -> Option<WakeLock>`; Kotlin only holds and releases the locks it is told to, and reports charging and screen state through `set_power_state`. The policy changes through `set_power_policy` *(back-ported from sub-spec 5)*.

  This matters less than it sounds: every host serves from its own local copy, so the phone being asleep only delays replication and forwarding through the phone. It never blocks another host's own users.
- **Gateway on Android:** other apps on the phone (Aegis Android) reach `http://127.0.0.1:7801` / `:7777`. Aegis Android needs **no** cleartext waiver: Aegis sync uses Rust sockets rather than Android's HTTP stack, and its `sync::validated_base` already allows loopback `http` *(as built, sub-spec 7; the design had planned a network security config entry)*. A client that does use Android's HTTP stack must allow cleartext to `127.0.0.1` itself.
- Wi-Fi ↔ mobile data handover is handled by iroh's network-change handling. The service passes Android `ConnectivityManager` callbacks to `Endpoint::network_change()`.
- **Build** *(back-ported from sub-spec 5)*: Tauri 2 CLI (pinned in CI), NDK 27.2.12479018, compileSdk 36, build-tools 36.0.0, targetSdk 35, minSdk 29, arm64-v8a; the release `.so` is stripped with thin LTO, `codegen-units = 1`, `opt-level = "s"` and never `panic = "abort"` (`mesh-ffi` catches panics at the boundary). Debug signing for now; release signing is sub-spec 8. The size of the release APK is recorded in `docs/manual-checks/sub-spec-5.md`. CI builds the release APK, runs the Kotlin JVM tests and lint, and checks that the committed UniFFI Kotlin bindings are current.

---

## 11. Aegis sync-server as a Meshnode service (`services/aegis-sync`)

Goal: the same HTTP contract (`/v1/records`, `/v1/devices*`, `/healthz`), unchanged for Aegis clients, served by every host from its local replica.

### Code structure
- In the **Aegis repo**, move the protocol logic out of `sync-server/src/main.rs` (auth `canonical()` + verification, request bounds, `canonicalize_body`, stamp-poisoning checks, pagination) into a new pure crate **`aegis-sync-core`**, which has no axum and no tokio. The native axum server keeps working on top of it, and its existing tests keep passing.
- `services/aegis-sync` is a thin `wasi:http` handler over `aegis-sync-core` (vendored under `vendor/aegis-sync-core`, §3), with storage through `meshnode:kv` *(as built, sub-spec 7)*. Ed25519 verification and `getrandom` work under WASI (`getrandom` uses `wasi:random`).

### State mapping (merge-safe)
| sync-server state | Meshnode representation |
|---|---|
| records `(account, ns, uuid)` | `set-ordered("rec/<acct>/<ns>/<uuid>", order = encode(client hlc wall, counter, node), value)` — the client HLC decides, exactly as server-side LWW does today. Deletes are ordered writes whose value is the client's tombstone record. |
| server tie-break `ord` | derived **deterministically** from `(hlc, uuid)` when served, instead of being stored. A counter assigned per node would differ between replicas. *As built (sub-spec 7):* every record in a tie group except the lowest uuid gets a `tie_ord`, and tombstones count as tie partners. |
| devices (registered) | grow-only set `devices/<acct>` of `{ device_id, registered_ms }`; labels are separate grow-only `LabelFact`s; `last_seen` as a per-node register (the latest across nodes is shown) *(as built, sub-spec 7)* |
| owner | **computed**, not stored. *As built (sub-spec 7):* revoked devices are skipped; devices that never had to ask to join rank first, by `(registered_ms, id)`; approved joiners rank after them, by `(approved_ms, id)`. Approving a device therefore never takes the role from the current owner. On owner sign-out the mesh hands the role to the earliest-approved joiner, while the native server hands it to the earliest-registered device: a known divergence. |
| pending / approved | grow-only sets `pending/<acct>` and `approved/<acct>`; a device is pending if it's in `pending` and not in `approved` and not revoked. `approved` members carry `{device_id, approved_ms}`, stamped `max(now, latest approval in the account + 1)` so that an approval always ranks after earlier ones even when the node clock is behind *(as built, sub-spec 7)* |
| revoked | grow-only set `revoked/<acct>` (already grow-only today) |
| registration secret | none in v1: **open registration**. The gateway is reachable only from the owner's own devices, and a warning is logged once per node per day. A secret is deferred *(decided in sub-spec 7)* |
| server-wide caps | **per-node soft caps**: each node enforces them over its local view. A global hard cap would need coordination, which this design avoids, so the mesh total can briefly go over by up to (nodes − 1) × the per-request allowance. *As built (sub-spec 7):* the sync service's tie index (`hlcidx`) is one plain key per stored record, and an update deletes the key of the hlc it moved off, so record churn leaves only delete tombstones, which the mesh GC removes after 90 days; no extra cap is needed. |
| replay nonce set | `meshnode:kv/local` (node-local, not replicated, survives restarts and a re-bootstrap), key `nonce/<device>/<nonce>`, TTL `max(MAX_TTL_MS, expires_ms - now + 1)` so a token issued ahead of the node clock cannot be replayed before it expires. Replay protection is **per node**: a token captured on one node's loopback could be replayed once on another node. Accepted for v1 *(as built, sub-spec 7)*. |
| tombstone retention (count + 90-day age) | service logic over `list-keys`, run only on the `(acct, ns)` being pushed. A prune is an **ordered delete**, so a stale copy of a pruned record is not resurrected (unlike native). Each pruned record leaves a permanent empty row of about 100 to 300 bytes that the mesh GC never removes. The 90-day age counts from when a node **first stored** the tombstone (kept in its index entry), not from the client's `hlc.wall_ms`, so a client cannot get past-dated tombstones pruned at once *(as built, sub-spec 7)*. |
| response paging | `GET` pages are capped at **6 MiB** on the mesh (native: 32 MiB) because the runtime's 8 MiB `max_body_bytes` also caps responses; the client follows `next` *(as built, sub-spec 7)*. |
| mesh-only refusals | `413` for a key over 512 bytes, an order over 64 bytes, `hlc.node` over 48 bytes, or `ns` + `uuid` over about 442 bytes; `400` for a NUL in `ns`, `uuid` or nonce. Keys percent-escape `ns` (`%`, `/`, NUL) so that `ns "a/b"` + `uuid "c"` differs from `ns "a"` + `uuid "b/c"` *(as built, sub-spec 7)*. |

**Concurrency edge, stated:** two devices registering the first device of the **same new account** on different nodes during a network split both become "owner" until the split heals. Afterwards the earlier one is the owner, and the other stays an approved device.

**Revocation on a mesh, stated** *(as built, sub-spec 7)*: revoking a device in Aegis stops only that device's Aegis client. A device that is itself a mesh member (every device that syncs through its own gateway) still has a node that receives the account's replicated records, which the device can read with the account's data key. As a mesh member it can also write the sync service's store directly. Natively, revocation stops the pulls. To cut off a lost or stolen device, the owner must **also remove it from the mesh** (`meshd remove <id>` or the UI).

**Deploy limits** *(as built, sub-spec 7)*: the service is deployed with `meshd deploy wasm sync <file> --memory-mb 64 --timeout-ms 30000 --max-body 8388608`; the daemon's default request timeout is 10 s.

### Aegis client changes
- Default sync URL suggestion `http://127.0.0.1:7801` when Meshnode is detected (desktop: an admin socket exists; Android: the Meshnode package is installed).
- Android: **no** network-security-config waiver is needed (as built, sub-spec 7): Aegis sync uses Rust sockets and `sync::validated_base` already allows loopback `http`. The Meshnode detection (package `net.meshnode.app`, manifest `<queries>`) stays. If the manual check shows Aegis Android cannot reach `http://127.0.0.1:7801`, re-add the waiver.
- Optional later: embed `mesh-client` so Aegis can sync on devices without Meshnode installed.

---

## 12. Error handling

| Situation | Behaviour |
|---|---|
| Peer unreachable | Live push, catch-up and dialling retry with exponential backoff (1 s → 5 min). The gateway falls back to the next host. |
| Bad signature or non-member origin | Op dropped, `warn` logged with the origin. |
| Op HLC > 5 min ahead | Deferred and retried as the clock catches up; dropped after 24 h (§5). |
| Blob fetch fails or the hash doesn't match | Retried from another peer. The previous version keeps serving. |
| WASM trap, timeout, memory cap, or refused outbound call | `500`/`504` for that request only; the outbound refusal is logged with the destination. |
| redb error or disk full | That write fails (`507`). Reads continue. The UI shows a banner. |
| Invite expired, used or unknown | `join/1` refuses with a specific code. The UI or CLI shows a readable reason. |
| Stale node (past the GC horizon) | Re-bootstrap flow (§5), with a warning in the UI. |
| Data dir belongs to another mesh, or `node.key` is corrupt | The node refuses to start with a clear message. It never silently creates a new identity. |
| Android service killed | Restarted by `START_STICKY` / the WorkManager watchdog. Catch-up resumes from the version vector. |

---

## 13. Testing

- **`mesh-hlc` / `mesh-store`:** unit tests plus `proptest` convergence tests. Random op sets (ordered and plain registers, sets, tombstones) are applied in random orders with duplicates on N replicas, and the final states must be equal. Compaction and tombstone GC must not change the merged state of non-stale replicas. The removal-cutoff rule must be order-independent.
- **`mesh-proto`:** round-trip and wrong-version decode tests; tampered signatures are rejected.
- **`mesh-core`:** in-process multi-node tests over local iroh endpoints (relay and discovery off), covering: invite/join; using a ticket twice; removal; split-and-heal; late join; placement changes; the stale-node re-bootstrap; clock-skew deferral.
- **`mesh-runtime`:** fixture components in `tests/fixtures/` (a counter service, an infinite loop, a memory hog, an outbound-call probe), checking KV semantics, namespace isolation, limits and the outbound allowlist.
- **`mesh-gateway`:** Host and port routing, forwarding fallback (static serving is in `mesh-services`).
- **`services/aegis-sync`:** the native sync-server's HTTP contract test suite, run against the WASM service (shared through `aegis-sync-core` test vectors), plus a two-replica divergence test (concurrent pushes to the same uuid on two nodes → the same winner on both).
- **Android:** instrumented tests on an emulator (service start, boot receiver, FFI round trip). The battery test is manual on a real device.
- **End-to-end:** `docs/e2e-checklist.md` mirrors the §1 success criteria and is run on real hardware (desktop + Android phone) before each release. Results go in `docs/e2e-results.md`.
- **CI:** `cargo test --workspace`, `clippy -D warnings`, `fmt --check` on Linux + Windows; an Android build (`cargo-ndk` + Gradle assemble) on Linux. For the client SDK *(back-ported from sub-spec 8a)*: a `mesh-client` job (the tests of `mesh-forward`, `mesh-client` and `mesh-client-ffi`, and `cargo tree -e normal` showing no WASM runtime, services or gateway in `mesh-client` and `mesh-forward`) and a `mesh-client-ffi-android` job (Kotlin bindings current, the AAR built for both ABIs, the Gradle JVM test of the bindings).

---

## 14. Implementation decomposition

Each row is its own sub-spec → plan → implementation cycle, in this order. Each one ends with something that can be demonstrated.

| # | Sub-spec | Delivers / demonstrates |
|---|---|---|
| 0 | ~~**Android feasibility spike**~~ **done 2026-11-09** | Feasible. Results in `spikes/android/RESULTS.md`; changes folded into §10.2. Still open for 5: how quickly a sleeping phone answers incoming requests, and overnight on mobile data. |
| 1 | Store + HLC + proto | merge, op log, compaction, tombstone GC; proptest convergence |
| 2 | Node core *(implemented, manual check pending; see `docs/manual-checks/sub-spec-2.md`)* | transport, membership, invites, replicator, blobs; split-and-heal between two desktop nodes |
| 3 | Runtime + services *(implemented, manual check pending; see `docs/manual-checks/sub-spec-3.md`)* | wasmtime host, `meshnode:kv`, manifests, deployer, placement, static sites |
| 4 | Gateway + admin API + `meshd` CLI + desktop OS services *(implemented, manual check pending; see `docs/manual-checks/sub-spec-4.md`)* | two desktop nodes serving a site, installed as system services |
| 5 | Android shell *(implemented, manual check pending; see `docs/manual-checks/sub-spec-5.md`)* | `mesh-ffi`, the foreground service, boot/watchdog, power policy |
| 6 | Tauri UI (desktop + Android) *(implemented, manual check pending; see `docs/manual-checks/sub-spec-6.md`)* | the full management UI, QR invite/join |
| 7 | Aegis sync port *(implemented, manual check pending; see `docs/manual-checks/sub-spec-7.md`)* | `aegis-sync-core` extraction in Aegis, `services/aegis-sync`, Aegis client changes |
| 8a | `mesh-client` SDK + partial replication *(implemented, manual check pending; see `docs/manual-checks/sub-spec-8a.md`)* | `mesh-client` (Rust) and `mesh-client-ffi` (Kotlin, AAR), `_mesh`-only replication for client-role nodes, role-upgrade re-bootstrap, `declare-fresh`; success criterion 5 in-process |
| 8b | Packaging + E2E *(pending)* | installers (MSI, deb/rpm, dmg, APK), release signing, the full §1 success-criteria run (`docs/e2e-results.md`) |

---

## 15. Deferred and out of scope

- **Public access (owner reminder):** a mix of private and public, where selected sites are reachable from the public internet at a domain through a public entry point that follows live nodes (a relay/tunnel VPS or DNS failover). Deferred at the owner's request.
- iOS hosting; Play Store distribution; multiple meshes per node; consistency stronger than per-key LWW / grow-only sets; encryption of the replica at rest beyond OS disk/app-sandbox encryption; multi-user meshes.
