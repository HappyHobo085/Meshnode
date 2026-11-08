# Meshnode — Mesh Node MVP Design

**Date:** 2026-11-08
**Status:** Draft, awaiting owner review
**Scope:** Sub-projects 1–3 (node core, WASM runtime, gateway and client library) of the Meshnode roadmap (see §11)

---

## 1. Intent

### What the owner asked for
- One app/system that installs on any device and turns your devices into an **always-on, self-hosted server mesh**.
- **A redundant mesh:** several devices run it and replicate the hosted services and their data. Services stay up while at least one node is alive.
- **Phones are full hosts** (Android). iOS is out of scope as a host, because it doesn't allow long-running background servers.
- **Private only for v1.** Only the owner's own devices reach services. Public internet access is deferred (§11).
- Hosted services are **WASM modules** (the Aegis sync-server after a port, other apps) plus **static websites**.
- When the network splits, **every node keeps serving** and replicas merge later (no leader, no quorum).
- Networking is **iroh** (self-contained, addressed by key, punches through NAT, relay fallback). No Tailscale dependency.
- Apps reach services through **both** a localhost gateway and an embeddable iroh client library.
- Devices join with **invite tickets, and any member can invite or remove**.

### Assumptions (stated by the designer, not the owner)
- Mesh size: 2–10 devices. Data per service: MBs to low GBs.
- Single owner. Everyone in the mesh is trusted, but devices can be lost or stolen.
- A conflicting concurrent write resolves as last-writer-wins by HLC. That is acceptable for the services in scope.

### Success criteria for this MVP
1. Two desktop nodes (Linux + Windows) form a mesh through an invite ticket over the real internet (NAT, relay fallback).
2. A static site and a test WASM service, deployed from one node, are served by **both** nodes' gateways.
3. A write made through node A's gateway shows up on node B. Writes made on both sides while they are disconnected converge after they reconnect.
4. Turning node A off doesn't interrupt service through node B's gateway.
5. A removed node is refused by every member and receives no further updates.

---

## 2. Architecture overview

```
 ┌─────────────────────────── meshd (one process per device) ───────────────────────────┐
 │                                                                                        │
 │  Gateway (HTTP 127.0.0.1)     Peer protocols (iroh ALPNs)        CLI / admin socket    │
 │        │                     join · sync · http-forward · blobs          │             │
 │        ▼                                │                                 ▼             │
 │  Service Router ──────────► Runtime (wasmtime, wasi:http) ◄──── Deployer (manifests)    │
 │        │                         │ mesh:kv imports                       │             │
 │        │ static sites            ▼                                       │             │
 │        └──────────────► Replicated Store (redb, HLC LWW + grow-only sets) ◄┘            │
 │                                  │  ▲                                                  │
 │                         Replicator (gossip push + version-vector catch-up)              │
 │                                  │                                                     │
 │                         Membership (signed member list, removed set)                   │
 │                                  │                                                     │
 │                         Transport (iroh Endpoint, identity = node key)                 │
 └────────────────────────────────────────────────────────────────────────────────────────┘
```

**Workspace layout** (Rust, one Cargo workspace):

| Crate | Purpose | Depends on |
|---|---|---|
| `mesh-hlc` | Hybrid logical clock: timestamp type and ordering. Pure, no I/O. | — |
| `mesh-store` | Replicated store: LWW registers + grow-only sets on `redb`; merge function; per-origin op log | `mesh-hlc` |
| `mesh-proto` | Wire types for every ALPN (serde + postcard), the invite ticket format, signatures | `mesh-hlc` |
| `mesh-core` | Transport, membership, replicator, deployer and blob fetch, glued around an iroh `Endpoint` | store, proto, `iroh`, `iroh-gossip`, `iroh-blobs` |
| `mesh-runtime` | wasmtime host: `wasi:http` incoming handler plus the `mesh:kv` host implementation, limits | store, `wasmtime`, `wasmtime-wasi-http` |
| `mesh-gateway` | localhost HTTP server, Host-based routing, static file serving, forwarding to a peer | core, runtime |
| `mesh-client` | Embeddable library: send HTTP requests to a mesh service over iroh (the same `http-forward` ALPN) | proto, `iroh` |
| `meshd` | Binary: daemon + CLI (`meshd init/invite/join/members/remove/deploy/status/run`) | all |

Each crate is testable on its own. `mesh-hlc`, `mesh-store` and `mesh-proto` have no networking.

---

## 3. Identity and transport

- Each node has an **Ed25519 node key** (its iroh `NodeId`), created on first run and stored at `<data>/node.key` with owner-only file permissions.
- A **mesh** is identified by a random 32-byte `MeshId`, created by `meshd init`.
- All peer traffic is iroh QUIC with these ALPNs, each versioned:
  - `meshnode/join/1`: redeem an invite ticket
  - `meshnode/sync/1`: catch-up (version-vector exchange + missing ops)
  - `meshnode/http/1`: forward an HTTP request to a node that hosts the service
  - iroh-blobs' own ALPN: fetch module and site content by BLAKE3 hash
  - iroh-gossip: live op broadcast on topic `blake3("meshnode/ops" ‖ MeshId)`
- **Every incoming connection is checked against membership** right after the handshake, using the remote `NodeId` that iroh has authenticated. Non-members and removed nodes are closed with an error code. The only exception is `join/1`, which needs a valid ticket instead.
- iroh relays are used only as a NAT-traversal fallback. Traffic is end-to-end encrypted, so relays see ciphertext only. The relay URL is configurable, and self-hosting a relay is possible later.

---

## 4. Replicated store

### Data model
Data is split into **namespaces**: one per service, plus the reserved `_mesh` namespace for system state. A namespace holds two kinds of entries:

- **LWW register:** `(ns, key) → Entry { hlc, origin: NodeId, value: Option<bytes> }`. `None` is a tombstone, kept so a delete can win over an older write.
- **Grow-only set:** `(ns, set, member_bytes) → { hlc, origin }`. Members can be added, never removed. Used for anything that must never be undone by a merge (revocations, consumed one-time tokens).

**The merge rule** is one pure function in `mesh-store`. For a register, the higher `(hlc.physical_ms, hlc.counter, origin)` wins, compared as a tuple, so ties resolve deterministically. For a set, a member is in the set if any node ever added it. The merge is commutative, associative and idempotent, and property tests check those three properties (§10).

### HLC
The same scheme the Aegis sync-server already uses: 48-bit physical milliseconds + 16-bit counter. On receiving an op, the local clock advances to `max(local, remote)`. A remote timestamp more than **5 minutes** ahead of local wall-clock time is rejected and logged, so one device with a bad clock can't win every conflict forever.

### Op log and catch-up
- Every local write becomes an **Op** `{ origin, seq, hlc, ns, payload, sig }`. `seq` is a per-origin counter that only goes up, and `sig` is the origin's Ed25519 signature over the rest.
- Each node stores ops indexed by `(origin, seq)`. **Compaction:** when an origin overwrites the same register key, its older op for that key is deleted. That's safe because the newer op from the same origin has a higher HLC and would win the merge anyway. Grow-only set ops are never compacted.
- Each node keeps a **version vector**: `origin → highest seq seen with no gaps`.
- **Live path:** a new op is broadcast on the gossip topic. A receiver checks the signature and membership (§5), merges the op, and updates its version vector.
- **Catch-up path** (`sync/1`): when connecting to a peer, and every 60 s to a random peer, the nodes swap version vectors, and each side streams the ops the other is missing. Because of compaction a range may have gaps, so the sender reports the "seq covered up to" value. Gossip is best-effort. Catch-up is what guarantees convergence.

### Storage
One `redb` file per node at `<data>/store.redb`, with tables for registers, sets, ops and version vectors. Each merge, together with its op-log insert, is one redb transaction.

---

## 5. Membership

All of this lives in the `_mesh` namespace, so it replicates and merges like any other data.

- **Members:** grow-only set `members` with entries `{ node_id, name, role: host|client, added_by, added_hlc }`.
- **Removed:** grow-only set `removed` with entries `{ node_id, removed_by, cutoff_hlc }`.
- **Effective members** = members − removed. A removed `NodeId` can never come back; a device can rejoin only with a new key and a new invite.
- **Op acceptance:** an op from origin `X` is accepted if `X` is a member and either (a) `X` is not removed, or (b) the op's `hlc ≤ cutoff_hlc`. This is deterministic, so every node accepts the same ops whatever order they arrive in. If `X` was removed more than once concurrently, the earliest cutoff applies.
- **Removing:** any effective member can remove any member, including itself (sign-out). The **last effective host can't be removed** (the CLI refuses), because a mesh with no host can't serve anything and can't recover in-band.
- **Honest limits** (shown in the CLI help):
  - A removed device keeps whatever data it already replicated.
  - Removal takes effect on each node only once that node has received it. Until then, the removed node can still talk to that node.
  - A removed node could backdate HLCs to below its cutoff, but only within that propagation window, because afterwards every member refuses its connections.

### Invites
- `meshd invite [--role host|client] [--ttl 15m]` prints an **invite ticket** (base32 string, also shown as a terminal QR code): `{ mesh_id, inviter_node_addr, token: 32 random bytes, role, expires_at }`.
- The inviter stores `blake3(token)` locally with its expiry (local only, not replicated).
- The joining node runs `meshd join <ticket>`. It connects over `join/1`, sends the token, and the inviter checks it is unexpired and unused. The inviter then adds the joiner to `members`, adds `blake3(token)` to the grow-only set `consumed_invites` (so the token can't be reused anywhere), and replies with `mesh_id`, a list of peer addresses, and a full catch-up.
- Every node runs `meshd init` or `meshd join`, never both. A node belongs to exactly one mesh.

---

## 6. Services and deployment

### Service manifest
Stored as the LWW register `_mesh/service/<name>`:

```
ServiceManifest {
  name: "sync",                  // [a-z0-9-]{1,32}; also the gateway hostname label
  kind: Wasm { module: Blake3Hash } | Static { root: Blake3Hash /* iroh-blobs collection */ },
  version: u64,                  // increases on every deploy
  limits: { memory_mb: 64, request_timeout_ms: 10_000, max_body_bytes: 8 MiB },
  run_on: AllHosts,              // MVP: every host node runs every service
}
```

- `meshd deploy wasm <name> <file.wasm>` / `meshd deploy static <name> <dir>` adds the content to the local iroh-blobs store, then writes the manifest.
- **Deployer loop:** whenever a manifest changes, each host node fetches any blobs it doesn't have from peers. After the hash is verified, it atomically swaps to the new version. Until the fetch finishes, the previous version keeps serving. A service that has never been fetched answers `503 Service not yet replicated`.
- `meshd undeploy <name>` writes a tombstone manifest. Service data stays in its namespace until `meshd purge <name>`.
- Unreferenced blobs are garbage-collected 24 h after their last manifest reference goes away.

### WASM runtime
- **wasmtime, Component Model.** A service is a component that targets a world `meshnode:service/service` with:
  - **exports** `wasi:http/incoming-handler` (the standard WASI HTTP proxy interface, so any `wasi:http`-capable toolchain works)
  - **imports** `wasi:clocks`, `wasi:random`, `wasi:logging` (routed to the meshd log), and `meshnode:kv/store`:

```wit
interface store {
  get: func(key: string) -> option<list<u8>>;
  set: func(key: string, value: list<u8>);
  delete: func(key: string);
  list-keys: func(prefix: string, start-after: option<string>, limit: u32) -> list<string>;
  set-add: func(set: string, member: list<u8>);         // grow-only
  set-contains: func(set: string, member: list<u8>) -> bool;
  set-members: func(set: string, start-after: option<list<u8>>, limit: u32) -> list<list<u8>>;
}
```

  Each service can reach **only its own namespace**. It has **no** outgoing network, no filesystem, and no `wasi:http/outgoing-handler` in the MVP.
- **One fresh instance per request** (the wasi:http proxy model), using a pre-compiled `InstancePre` cached by module hash. The compiled code is cached on disk under `<data>/cache/`.
- **Limits:** a memory cap through `ResourceLimiter`, a time cap through epoch interruption (ticks every 10 ms, deadline = `request_timeout_ms`), and the request body cap is enforced before the module runs. A trap or timeout returns `500`/`504` to the caller and is logged with the service name. The node itself never crashes because of a service.
- Writes a module makes through `mesh:kv` are applied locally right away (read-your-writes on that node) and become ops for replication.

### Static sites
Not WASM: a built-in native handler serves files from the iroh-blobs collection. It returns `index.html` for directories and the right `Content-Type` by extension, supports `ETag` = blob hash, and returns `404` for missing paths.

---

## 7. Gateway and client access

### Local gateway
- Listens on `127.0.0.1:7777` by default (configurable; never on a non-loopback address in the MVP).
- **Routing by Host header:** `<service>.mesh.localhost[:port]` → service `<service>`. Browsers resolve `*.localhost` to loopback by themselves.
- **Port mappings** for clients that can't resolve `*.localhost` (some native HTTP stacks on Windows/Android): node-local config `ports = { 7801 = "sync" }` binds `127.0.0.1:7801` straight to a service, so Aegis's sync URL becomes `http://127.0.0.1:7801`.
- **Dispatch:**
  - On a **host** node with the service replicated, the request is handled locally.
  - On a **client-role** node, or a host where the service isn't replicated yet, it's forwarded over `http/1` to the reachable host with the lowest recent round-trip time. If that fails, the next host is tried, up to 3 attempts. If none work: `502`.
- `http://mesh.localhost:7777/` serves a read-only status page: members, online state, services and versions, replication lag.
- **Security note:** any local process can reach the gateway, the same as any localhost server. Services handle their own user-level auth (the sync-server already does).

### `http/1` forward protocol
One bidirectional QUIC stream per request: a length-prefixed `RequestHead { service, method, path, headers }`, then the body bytes, then `ResponseHead { status, headers }` and the body back. Hosts only accept it from effective members.

### `mesh-client` library
A Rust crate exposing `MeshClient::connect(ticket_or_existing_identity)` and `client.request(service, http::Request) -> http::Response`. It's a client-role node without the gateway: it replicates **only the `_mesh` namespace** (to learn the current hosts and members), in memory or in a small store file. It talks `join/1` once to become a member (role `client`), then `http/1`. It's intended for embedding in Aegis later (sub-project 5) and in other Rust apps.

---

## 8. Node lifecycle and configuration

- Data directory: `$XDG_DATA_HOME/meshnode` (Linux), `%LOCALAPPDATA%\meshnode` (Windows). Override with `--data`.
- `meshd run` starts the daemon. The CLI's other subcommands talk to a running daemon over a local admin socket: a Unix socket on Linux, a named pipe on Windows, with access restricted to the current user. If no daemon is running, they operate on the data directory directly.
- Config: `<data>/config.toml` (gateway address, port mappings, relay URL, log level).
- Running as a service in the MVP: an example systemd unit (system unit, `After=network-online.target`; no tailnet-IP race any more, since iroh binds a wildcard UDP socket) and a Windows Task Scheduler / `sc.exe` recipe in the README. Proper installers and service management belong to sub-project 4.
- Logs: `tracing` to stderr + a rotating file under `<data>/logs/`.

---

## 9. Error handling

| Situation | Behaviour |
|---|---|
| Peer unreachable | Gossip and catch-up retry with exponential backoff (1 s → 5 min cap). The gateway falls back to the next host. |
| Op with a bad signature, a non-member origin, or an HLC skewed > 5 min | Dropped, logged at `warn` with the origin. Never merged. A dropped op doesn't advance the version vector, so an op that was refused only because its origin's membership hadn't arrived yet is picked up again by a later catch-up. |
| Client-role node | Replicates only `_mesh`. It's never chosen as a forward target and never runs services. |
| Blob fetch fails or the hash doesn't match | Retried from another peer, with backoff. The previous service version keeps serving. |
| WASM trap, timeout, or memory cap hit | `500`/`504` for that request only. Logged with the service name. |
| redb error or disk full | That write fails (`507` from the gateway to the service caller). The node keeps serving reads. |
| Expired, used, or unknown invite token | `join/1` refuses with a specific error code. The CLI prints a readable reason. |
| Data dir from a different mesh, or a corrupt `node.key` | The daemon refuses to start, with a clear message. It never silently creates a new identity. |

---

## 10. Testing

- **`mesh-hlc` / `mesh-store`:** unit tests plus `proptest` convergence tests. Random op sets are applied in random orders, with duplicates, on N simulated replicas, and the final states must be byte-for-byte equal. Compaction must not change the merged state. The removal-cutoff rule must be order-independent.
- **`mesh-proto`:** round-trip and wrong-version decode tests, and checks that tampered signatures are rejected.
- **`mesh-core`:** in-process multi-node tests using local iroh endpoints (no relay, no discovery, direct addresses). Covered scenarios: invite/join; using a ticket twice; removal (connection refused, ops after the cutoff dropped); split-and-heal convergence (stop the transport between two groups, write on both sides, reconnect, compare stores); catch-up for a node that joins late.
- **`mesh-runtime`:** small test components built from `tests/fixtures/` (a Rust `wasi:http` counter service, an infinite loop, a memory hog), checking KV behaviour, namespace isolation, timeouts and memory caps.
- **`mesh-gateway`:** Host routing, port mappings, static serving, the forward fallback when the local node doesn't have the service.
- **End-to-end (manual, from the success criteria in §1):** a Linux machine + a Windows machine on different networks, with a scripted checklist in `docs/e2e-checklist.md`.
- CI: `cargo test --workspace`, `clippy -D warnings`, `fmt --check` on Linux and Windows.

---

## 11. Roadmap and deferred items

| # | Sub-project | Notes |
|---|---|---|
| 1–3 | **This spec**: node core, runtime, gateway, client lib | |
| 4 | **Platform shells** | An Android foreground service embedding `mesh-core` through JNI (with battery-optimisation exemption and a permanent notification); a Windows service; installers; a Tauri 2 management UI (desktop + Android) with QR scanning for invites. |
| 5 | **Port Aegis sync-server to a Meshnode WASM service** | Rewrite owner/revoked/pending/caps as merge-safe rules over `mesh:kv` (`revoked` is already a grow-only set; "owner = earliest registration" becomes "lowest HLC wins"; server-wide caps become per-node soft limits, since a global cap can't be enforced without coordination). Optionally embed `mesh-client` in Aegis. |
| 6 | **Public access (owner reminder)** | A mix of private and public: selected sites reachable from the public internet at a domain through a public entry point that follows live nodes (a relay/tunnel VPS or DNS failover). Deferred from v1 at the owner's request. |
| 7 | Placement | `run_on` subsets (e.g. "desktops only" for heavy services); per-service replication factor. |
| 8 | Outgoing network for services | `wasi:http/outgoing-handler`, with a per-service allowlist. |

### Explicitly out of scope for this MVP
iOS hosting; public access; Android; GUI; outgoing network from services; stronger consistency than LWW; multiple meshes per node; encryption of data at rest (left to the OS disk encryption, revisit in sub-project 4).
