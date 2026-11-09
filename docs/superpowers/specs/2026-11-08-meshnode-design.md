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
- Implementation stack (approach A): **one Rust core** (iroh, iroh-gossip, iroh-blobs, wasmtime, our own LWW store on redb) in a desktop daemon and an Android foreground service, with a **Tauri 2 management UI**.

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
 │        │                  join · sync · http-forward · blobs · gossip     (socket / in-process) ◄── UI      │
 │        ▼                                    │                                         │                   │
 │  Service Router ─────────► Runtime (wasmtime, wasi:http) ◄──────── Deployer + Placement (manifests)       │
 │        │                         │ meshnode:kv imports                                │                   │
 │        │ static sites            ▼                                                    │                   │
 │        └──────────────► Replicated Store (redb: ordered LWW registers + grow-only sets) ◄┘                  │
 │                                  │  ▲                                                                     │
 │                         Replicator (gossip push + version-vector catch-up + tombstone GC)                  │
 │                                  │                                                                        │
 │                         Membership (signed member set, removed set)                                       │
 │                                  │                                                                        │
 │                         Transport (iroh Endpoint, identity = node key)                                    │
 └───────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### Repository layout (one Cargo workspace + Android project)

| Path | Purpose | Depends on |
|---|---|---|
| `crates/mesh-hlc` | Hybrid logical clock. Pure, no I/O. | — |
| `crates/mesh-store` | Replicated store: ordered LWW registers + grow-only sets on `redb`; merge function; per-origin op log; tombstone GC | `mesh-hlc` |
| `crates/mesh-proto` | Wire types for every ALPN (serde + postcard), invite tickets, signatures | `mesh-hlc` |
| `crates/mesh-core` | `Node`: transport, membership, replicator, deployer, placement, blob fetch | store, proto, `iroh`, `iroh-gossip`, `iroh-blobs` |
| `crates/mesh-runtime` | wasmtime host: the `wasi:http` incoming handler, `meshnode:kv`, the outgoing-HTTP allowlist, limits | store, `wasmtime`, `wasmtime-wasi-http` |
| `crates/mesh-gateway` | localhost HTTP server, Host and port routing, static serving, forwarding to a peer | core, runtime |
| `crates/mesh-admin` | Admin API: request/response types plus two transports (local socket, in-process channel) | core |
| `crates/mesh-client` | Embeddable client: become a client-role member, send HTTP to services over iroh | proto, `iroh` |
| `crates/mesh-ffi` | UniFFI bindings exposing `Node` start/stop/status + admin calls to Kotlin | core, gateway, admin |
| `apps/meshd` | Desktop daemon + CLI; Windows-service / launchd / systemd integration | core, gateway, admin |
| `apps/ui` | Tauri 2 management UI (desktop + Android) | admin |
| `apps/android` | The Android Tauri project + Kotlin foreground service, boot receiver, notification | `mesh-ffi`, `apps/ui` |
| `services/aegis-sync` | The Aegis sync-server as a Meshnode WASM component (§11) | `aegis-sync-core` (from the Aegis repo) |
| `packaging/` | systemd unit, launchd plist, WiX fragments, deb/rpm metadata | — |

Each crate is testable on its own. `mesh-hlc`, `mesh-store` and `mesh-proto` have no networking.

---

## 4. Identity and transport

- Each node has an **Ed25519 node key** (its iroh `NodeId`), created on first run and stored at `<data>/node.key` with owner-only file permissions (on Android, in app-private storage).
- A **mesh** is identified by a random 32-byte `MeshId`, created when the first node starts a mesh. A node belongs to exactly one mesh.
- All peer traffic is iroh QUIC with these ALPNs, each versioned:
  - `meshnode/join/1`: redeem an invite ticket
  - `meshnode/sync/1`: catch-up (version-vector exchange + missing ops)
  - `meshnode/http/1`: forward an HTTP request to a node that hosts the service
  - iroh-blobs' ALPN: fetch module and site content by BLAKE3 hash
  - iroh-gossip: live op broadcast on topic `blake3("meshnode/ops" ‖ MeshId)`
- **Every incoming connection is checked against membership** right after the handshake, using the remote `NodeId` iroh has authenticated. Non-members and removed nodes are closed with an error code. The only exception is `join/1`, which needs a valid ticket instead.
- **Finding peers:** each member's current direct addresses and home relay are stored in `_mesh/addr/<node_id>` (LWW, refreshed on change and every 10 min), so the mesh carries its own address book. iroh's DNS/pkarr discovery is enabled as a second path. On startup a node dials every known member.
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
48-bit physical milliseconds + 16-bit counter, the same scheme the Aegis sync-server uses. On receiving an op, the local clock advances to `max(local, remote)`. An op whose HLC is more than **5 minutes** ahead of the receiver's wall clock is **deferred, not merged**: it's held and retried as the wall clock catches up, and dropped after 24 h. That way one device with a bad clock can't win every conflict forever or drag every other clock forward.

### Op log, version vectors, catch-up
- Every local write becomes an **Op** `{ origin, seq, hlc, ns, payload, sig }`. `seq` is a per-origin counter that only goes up, and `sig` is the origin's Ed25519 signature.
- Ops are stored indexed by `(origin, seq)`. **Compaction:** an origin's older register op for a key is deleted once a newer op from the **same origin for that key** beats it under the merge rule. Grow-only set ops are never compacted.
- **Version vector:** `origin → highest seq covered`. A compacted range counts as covered.
- **Live path:** ops go out on gossip. A receiver checks the signature and acceptance (§6), then merges.
- **Catch-up** (`sync/1`): when connecting to a peer, and every 60 s with a random online peer (on Android, also triggered by incoming traffic and a wall-clock alarm, because in-process timers stall in deep sleep; see §10.2), the nodes swap version vectors and each streams what the other is missing. Gossip is best-effort; catch-up is what guarantees convergence. An op that is refused or deferred doesn't advance the version vector, so a later catch-up retries it.

### Tombstone GC
- A register tombstone, and the op behind it, is purged once it is older than **90 days** (by HLC).
- Every node records `last_full_sync_ms`: the last time it completed catch-up with at least one host.
- A node whose `last_full_sync_ms` is older than **80 days** (the GC horizon minus a margin) is **stale**. On reconnect it must **re-bootstrap**: it sends ops written after its last sync to a peer, then replaces its replica with a full catch-up from that peer. Older local data is dropped, because otherwise the node could bring back deleted keys. The UI shows a warning before this happens and records it in the log.

### Storage
One `redb` file per node at `<data>/store.redb`. A merge and its op-log insert happen in one transaction. The UI shows the size of each namespace.

---

## 6. Membership and invites

All of this lives in the `_mesh` namespace, so it replicates and merges like any other data.

- **Members:** grow-only set with entries `{ node_id, name, role: host|client, platform, added_by, added_hlc }`.
- **Removed:** grow-only set with entries `{ node_id, removed_by, cutoff_hlc }`.
- **Effective members** = members − removed. A removed `NodeId` can never come back; a device can rejoin only with a new key and a new invite (the UI calls this "reset identity").
- **Op acceptance:** an op from origin `X` is accepted if `X` is a member and either (a) `X` is not removed, or (b) the op's `hlc ≤ cutoff_hlc` (the earliest cutoff if `X` was removed more than once concurrently). This is deterministic, so every node accepts the same ops whatever order they arrive in.
- **Removing:** any effective member can remove any member, including itself (sign-out). The **last effective host can't be removed**, because a mesh with no host can't serve and can't recover in-band.
- **Removal is a hard cutoff:** existing connections to a newly removed node are closed as soon as the removal is merged.
- **Honest limits** (shown in the UI and CLI help):
  - A removed device keeps whatever data it already replicated.
  - Removal takes effect on each node only once that node has received it.
  - Within that window, a removed node could backdate its writes to before the cutoff.

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

- **Placement:** each host has tags (`desktop`, `android`, `always-plugged`, plus any the user adds) stored with its member entry. A host runs a service only if it matches the placement. Hosts that don't run a service still replicate its **data** (so they can take it over immediately when placement changes) but don't fetch its blobs or run it. Their gateways forward requests for it (§8). If placement matches no host, the UI warns and the service returns `503`.
- **Deploy:** the UI or CLI adds the module or site folder to the local iroh-blobs store, then writes the manifest. `undeploy` writes a tombstone manifest; the data stays until `purge`, which writes a namespace-tombstone marker and deletes all keys.
- **Deployer loop:** whenever a manifest changes, each matching host fetches missing blobs from any peer, verifies the hash, and atomically swaps to the new version. Until then the previous version keeps serving. A service that has never been fetched answers `503 Service not yet replicated`.
- **Blob GC:** blobs no manifest has referenced for 24 h are deleted.

### WASM runtime
- **wasmtime, Component Model.** A service is a component targeting world `meshnode:service/service`:
  - **exports** `wasi:http/incoming-handler` (the standard WASI HTTP proxy interface, so any toolchain that supports `wasi:http` works)
  - **imports** `wasi:clocks`, `wasi:random`, `wasi:logging` (routed to the node log), `meshnode:kv/store`, and `wasi:http/outgoing-handler`, which is **always linked** but refuses every destination not in `outbound`.

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
```

- **Isolation:** each service can reach only its own namespace. No filesystem access. Outgoing network only to the allowlist.
- **One fresh instance per request** (the wasi:http proxy model), from a cached `InstancePre` keyed by module hash.
- **Compiled-code cache:** on desktop, a file cache under `<data>/cache/`. **On Android, compiled code is cached as bytes and loaded with `Module::deserialize` into anonymous memory, never memory-mapped from an app-data file**, because Android blocks executing code mapped from files in app-writable storage. To be verified in the Android spike (§14). The fallback is wasmtime's Pulley interpreter.
- **Limits:** memory cap through `ResourceLimiter`, time cap through epoch interruption (10 ms ticks), body cap enforced before the module runs. A trap or timeout returns `500`/`504` and is logged; the node never crashes because of a service.
- Writes made through `meshnode:kv` apply locally right away (read-your-writes on that node) and become ops for replication.

### Static sites
A built-in native handler serves files from an iroh-blobs collection: `index.html` for directories, `Content-Type` by extension, `ETag` = blob hash, `404` for missing paths.

---

## 8. Client access

### Local gateway
- Listens on `127.0.0.1:7777` (configurable; loopback only).
- **Routing by Host header:** `<service>.mesh.localhost[:port]` → that service. Browsers resolve `*.localhost` to loopback by themselves.
- **Port mappings** for clients that can't resolve `*.localhost`: node-local `ports = { 7801 = "sync" }` binds `127.0.0.1:7801` straight to a service. **Default mapping: `7801 → sync`** on every node, so Aegis's sync URL is always `http://127.0.0.1:7801`.
- **Dispatch:** if this node runs the service, it's handled locally. Otherwise (client role, not in placement, or not yet replicated) it's forwarded over `http/1` to the online host running it with the lowest recent round-trip time, trying the next host on failure, up to 3 attempts. If none work: `502`.
- `http://mesh.localhost:7777/` serves a read-only status page.
- **Security:** any local process can reach the gateway, the same as any localhost server. Services handle their own user-level auth (the Aegis sync service does).

### `http/1` forward protocol
One bidirectional QUIC stream per request: a length-prefixed `RequestHead { service, method, path, headers }`, the streamed body, then `ResponseHead { status, headers }` and the streamed body back. Hosts accept it only from effective members.

### `mesh-client` library
A Rust crate: `MeshClient::join(ticket)` / `MeshClient::open(identity)` and `client.request(service, http::Request) -> http::Response`. It's a client-role member that replicates only `_mesh` (to learn the hosts) and talks `http/1`. Intended for embedding in apps on devices that don't run the full node.

---

## 9. Admin API and management UI

### Admin API (`mesh-admin`)
One set of typed requests, used by the CLI and the UI:
`status`, `members`, `invite`, `join`, `remove`, `set_tags`, `services`, `deploy_wasm`, `deploy_static`, `undeploy`, `purge`, `set_placement`, `set_outbound`, `config_get/set`, `logs_tail`, `reset_identity`.

- **Desktop:** served by `meshd` on a Unix socket (`<data>/admin.sock`, mode 0600) or a Windows named pipe (with an ACL limited to the installing user and Administrators).
- **Android:** an in-process channel. The UI and the foreground service share one process and one Rust library.

### `meshd` CLI
`meshd init | join <ticket> | invite | members | remove <id> | deploy wasm|static … | services | status | logs | run`, plus `meshd service install|uninstall` for the OS service. Each command calls the admin API (or works on the data directory directly when no daemon is running).

### Tauri 2 UI (`apps/ui`, one codebase for desktop + Android)
- **Home:** mesh status, a card for each member (online, platform, tags, last seen, replication lag), the node's own gateway URL.
- **Add device:** shows the invite QR + text, with a countdown until it expires.
- **Join a mesh:** scan a QR code (Tauri barcode-scanner plugin on Android; paste text on desktop).
- **Services:** list with version, placement, which hosts are running it and data size. Deploy (pick a `.wasm` file or a folder), change placement and outbound allowlist, undeploy and purge (with confirmation).
- **Device settings:** name, tags, gateway port mappings, relay URL, Android power policy, reset identity.
- **Logs:** a live tail.
- On desktop the UI is a normal app that connects to the running daemon and offers to install or start the service if none is running.

---

## 10. Platform shells

### 10.1 Desktop daemon (`apps/meshd`)
- **Linux:** a systemd **system** unit `meshnode.service` running as a dedicated `meshnode` user, data in `/var/lib/meshnode`, `After=network-online.target`, `Restart=always`. iroh binds a wildcard UDP socket, so there's no bind-address race like the one behind the Aegis sync-server's tailnet outage.
- **Windows:** a Windows Service (`windows-service` crate) running as `LocalService`, data in `%ProgramData%\meshnode`, automatic (delayed) start, restart on failure. The MSI installs it along with the UI.
- **macOS:** a LaunchDaemon plist with `KeepAlive`, data in `/Library/Application Support/meshnode`.
- Logs go through `tracing` to a rotating file under `<data>/logs/` and to journald or the Windows Event Log.

### 10.2 Android (`apps/android`)
> Findings from the feasibility spike (`spikes/android/RESULTS.md`, Galaxy S23 / Android 17) are folded in below and marked *(spike)*.

- **One APK, one process:** the Tauri UI activity plus a Kotlin **foreground service** (`MeshService`). Both call the same Rust library (`mesh-ffi`, built with `cargo-ndk`, bindings generated by UniFFI). The service owns the `Node`; the UI talks to it in-process.
- **Android context setup *(spike)*:** when `mesh-ffi` first loads, it must call `ndk_context::initialize_android_context(vm, context)` with a global ref to the application `Context`, before any iroh endpoint is created. iroh's DNS resolver and network monitor need it; without it a worker thread panics (`android context was not initialized`).
- **WASM *(spike)*:** Cranelift compilation works inside the app sandbox (1.7–8 ms per request on the S23), and the in-memory compiled-code cache (§7) works as designed. Pulley stays as an unused fallback.
- **Foreground service type `specialUse`** (`FOREGROUND_SERVICE_SPECIAL_USE`), with a permanent notification ("Meshnode: hosting N services · M peers online"), and Stop/Open actions. `specialUse` because `dataSync` is time-limited on Android 15+ and can't start from boot. Sideloaded distribution means no Play Store review of this type.
- **Starting automatically:** a `BOOT_COMPLETED` receiver starts the service after the first unlock (verified in the spike: it reconnected 4 s after boot). A `WorkManager` watchdog every 15 minutes restarts it if the system killed it.
- **Swipe-away recovery *(spike)*:** on One UI, swiping the app out of recents **stops the node** despite the foreground service and battery exemption, and `START_STICKY` doesn't restart it. So `onTaskRemoved` schedules an exact alarm (`setExactAndAllowWhileIdle`, about 5 s out) whose receiver restarts the service, which is an allowed way to start a foreground service from the background. The WorkManager watchdog is the backstop. Onboarding tells the user to lock the app in recents. To verify on the S23 in sub-project 5.
- **Battery:** first-run onboarding asks for **battery-optimisation exemption** (`REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`) and links to per-manufacturer guidance (Samsung, Xiaomi, etc.) for aggressive app killers. *(spike)*: a foreground service kept network access in forced Doze even **without** the exemption, so the exemption is insurance against OEM app sleeping rather than a requirement.
- **Timers vs deep sleep *(spike)*:** with the screen off, the CPU was awake only ~15 % of the time. In-process timers run on the monotonic clock, which stops during suspend, so a 300 s timer fired every 18–43 min of wall time. Yet the QUIC connection survived overnight (one reconnect in 10 h), so incoming traffic does reach the sleeping phone. Design rule: **on Android, periodic work (catch-up, address refresh) must not depend on in-process timers alone.** It runs (a) whenever incoming traffic wakes the process, i.e. a catch-up is piggybacked on any incoming connection or gossip message after ≥ 60 s of silence, and (b) on a wall-clock `AlarmManager` while-idle alarm (inexact, every 15 min).
- **Power policy** (setting):
  - **Balanced (default):** keep peer connections open with iroh's default keepalives. Hold a partial wake lock **only** while charging or while serving an incoming request or a catch-up. *(spike)*: with no wake lock at all, the spike used **~0.3 % battery per 24 h** (5.0 mAh over 10.2 h). **Still to measure early in sub-project 5:** how quickly a sleeping phone answers an incoming request, on Wi-Fi and on mobile data. If it's too slow for success criterion 7, Balanced gains a short periodic wake lock.
  - **Always on:** hold a partial wake lock all the time.
  - **Saver:** run catch-up every 15 minutes through WorkManager and serve only the device itself while the screen is off.

  This matters less than it sounds: every host serves from its own local copy, so the phone being asleep only delays replication and forwarding through the phone. It never blocks another host's own users.
- **Gateway on Android:** other apps on the phone (Aegis Android) reach `http://127.0.0.1:7801` / `:7777`. The client app must allow cleartext traffic to `127.0.0.1` through its network security config (an Aegis change, §11).
- Wi-Fi ↔ mobile data handover is handled by iroh's network-change handling. The service passes Android `ConnectivityManager` callbacks to `Endpoint::network_change()`.

---

## 11. Aegis sync-server as a Meshnode service (`services/aegis-sync`)

Goal: the same HTTP contract (`/v1/records`, `/v1/devices*`, `/healthz`), unchanged for Aegis clients, served by every host from its local replica.

### Code structure
- In the **Aegis repo**, move the protocol logic out of `sync-server/src/main.rs` (auth `canonical()` + verification, request bounds, `canonicalize_body`, stamp-poisoning checks, pagination) into a new pure crate **`aegis-sync-core`**, which has no axum and no tokio. The native axum server keeps working on top of it, and its existing tests keep passing.
- `services/aegis-sync` is a thin `wasi:http` handler over `aegis-sync-core`, with storage through `meshnode:kv`. Ed25519 verification and `getrandom` work under WASI (`getrandom` uses `wasi:random`).

### State mapping (merge-safe)
| sync-server state | Meshnode representation |
|---|---|
| records `(account, ns, uuid)` | `set-ordered("rec/<acct>/<ns>/<uuid>", order = encode(client hlc wall, counter, node), value)` — the client HLC decides, exactly as server-side LWW does today. Deletes are ordered writes whose value is the client's tombstone record. |
| server tie-break `ord` | derived **deterministically** from `(hlc, uuid)` when served, instead of being stored. A counter assigned per node would differ between replicas. Aegis must accept the derived value (to verify against `src-tauri/src/sync_stores.rs` in the sub-spec). |
| devices (registered) | grow-only set `devices/<acct>` of `{ device_id, label, registered_hlc }`; `last_seen` as a per-node LWW register (the latest across nodes is shown) |
| owner | **computed**, not stored: the earliest-registered non-revoked device. That reproduces both "first device is owner" and D5's "owner sign-out hands off to the oldest remaining device". |
| pending / approved | grow-only sets `pending/<acct>` and `approved/<acct>`; a device is pending if it's in `pending` and not in `approved` and not revoked |
| revoked | grow-only set `revoked/<acct>` (already grow-only today) |
| registration secret | service config value (a `_mesh` register set through the admin UI), not an environment variable |
| server-wide caps | **per-node soft caps**: each node enforces them over its local view. A global hard cap would need coordination, which this design avoids, so the mesh total can briefly go over by up to (nodes − 1) × the per-request allowance. |
| replay nonce set | node-local and in memory, as today. Requests come from the local gateway, so a captured token would have to be replayed on another node's loopback, which is not reachable. |
| tombstone retention (count + 90-day age) | kept as service logic over `list-keys`. The mesh's own 90-day tombstone GC (§5) matches the age floor. |

**Concurrency edge, stated:** two devices registering the first device of the **same new account** on different nodes during a network split both become "owner" until the split heals. Afterwards the earlier one is the owner, and the other stays an approved device.

### Aegis client changes
- Default sync URL suggestion `http://127.0.0.1:7801` when Meshnode is detected (desktop: an admin socket exists; Android: the Meshnode package is installed).
- Android: a network-security-config entry allowing cleartext traffic to `127.0.0.1` only.
- Optional later: embed `mesh-client` so Aegis can sync on devices without Meshnode installed.

---

## 12. Error handling

| Situation | Behaviour |
|---|---|
| Peer unreachable | Gossip, catch-up and dialling retry with exponential backoff (1 s → 5 min). The gateway falls back to the next host. |
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
- **`mesh-gateway`:** Host and port routing, static serving, forwarding fallback.
- **`services/aegis-sync`:** the native sync-server's HTTP contract test suite, run against the WASM service (shared through `aegis-sync-core` test vectors), plus a two-replica divergence test (concurrent pushes to the same uuid on two nodes → the same winner on both).
- **Android:** instrumented tests on an emulator (service start, boot receiver, FFI round trip). The battery test is manual on a real device.
- **End-to-end:** `docs/e2e-checklist.md` mirrors the §1 success criteria and is run on real hardware (desktop + Android phone) before each release. Results go in `docs/e2e-results.md`.
- **CI:** `cargo test --workspace`, `clippy -D warnings`, `fmt --check` on Linux + Windows; an Android build (`cargo-ndk` + Gradle assemble) on Linux.

---

## 14. Implementation decomposition

Each row is its own sub-spec → plan → implementation cycle, in this order. Each one ends with something that can be demonstrated.

| # | Sub-spec | Delivers / demonstrates |
|---|---|---|
| 0 | ~~**Android feasibility spike**~~ **done 2026-11-09** | Feasible. Results in `spikes/android/RESULTS.md`; changes folded into §10.2. Still open for 5: how quickly a sleeping phone answers incoming requests, and overnight on mobile data. |
| 1 | Store + HLC + proto | merge, op log, compaction, tombstone GC; proptest convergence |
| 2 | Node core | transport, membership, invites, replicator, blobs; split-and-heal between two desktop nodes |
| 3 | Runtime + services | wasmtime host, `meshnode:kv`, manifests, deployer, placement, static sites |
| 4 | Gateway + admin API + `meshd` CLI + desktop OS services | two desktop nodes serving a site, installed as system services |
| 5 | Android shell | `mesh-ffi`, the foreground service, boot/watchdog, power policy |
| 6 | Tauri UI (desktop + Android) | the full management UI, QR invite/join |
| 7 | Aegis sync port | `aegis-sync-core` extraction in Aegis, `services/aegis-sync`, Aegis client changes |
| 8 | `mesh-client` + packaging + E2E | installers (MSI, deb/rpm, dmg, APK), the full §1 success-criteria run |

---

## 15. Deferred and out of scope

- **Public access (owner reminder):** a mix of private and public, where selected sites are reachable from the public internet at a domain through a public entry point that follows live nodes (a relay/tunnel VPS or DNS failover). Deferred at the owner's request.
- iOS hosting; Play Store distribution; multiple meshes per node; consistency stronger than per-key LWW / grow-only sets; encryption of the replica at rest beyond OS disk/app-sandbox encryption; multi-user meshes.
