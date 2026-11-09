# Sub-spec 2 — Node core

**Date:** 2026-11-09
**Status:** Draft, awaiting owner review
**Parent:** `2026-11-08-meshnode-design.md` (§4 transport, §5 replication, §6 membership and invites, §14 row 2)
**Builds on:** `2026-11-09-store-hlc-proto-design.md` (sub-spec 1) and `../notes/2026-11-09-sub-spec-1-handoff.md`
**Delivers:** crate `mesh-core` (transport, membership, invites, replicator, minimal blobs), additions to `mesh-proto` and `mesh-store`, and the throwaway `meshdev` example.

---

## 1. Goal and success criteria

Turn the store into a running node: a `Node` that creates or joins a mesh, keeps a connection to every online member, replicates every op to every member, enforces membership on every connection, removes members, re-bootstraps when stale, and fetches content-addressed blobs from peers.

Done when:
1. Every test in §11 passes.
2. `cargo test --workspace`, `clippy -D warnings`, `fmt --check` pass on Windows and Linux; `cargo ndk build -t arm64-v8a` still compiles with iroh and iroh-blobs added.
3. The manual check in §11.3 (two desktops on different networks, split and heal) has been run, and the result is recorded in `docs/manual-checks/sub-spec-2.md`.
4. The back-ports in §2 are made to the master spec and sub-spec 1.

Dependencies: `iroh = "1.4"`, `iroh-blobs = "0.104"` (uses redb 4, like the store), `tokio` (rt-multi-thread, macros, time, sync), `blake3`, `tracing`, `thiserror`, `rand`. All handoff "must" items (handoff §Must 1–8) are addressed; where is listed in §12.

## 2. Changes to the parent specs (made here, to be back-ported)

1. **iroh-gossip is dropped.** Live replication runs over the per-peer `sync/1` sessions (§6). The gossip ALPN and topic leave master §4 and §5. Reason: at 2–10 nodes a direct connection per peer is simpler, gates membership in one place, gives per-peer backpressure, and uses one connection per peer on Android. It does not scale to hundreds of nodes, which are out of scope.
2. **Genesis-derived `MeshId` (trust root).** `MeshId = blake3("meshnode/mesh/1" ‖ founder NodeId ‖ nonce[32])`. The founder is an implicit member. Without this, a joiner's empty store can accept no op at all (every origin is `UnknownOrigin`, including the ops that add members). The join reply carries `MeshGenesis { founder, nonce }`, which the joiner checks against the ticket's `mesh`.
3. **Removal records are never retracted.** Set rows in `_mesh` set `removed` are exempt from sub-spec 1 §5.6 steps 1 and 3 (a removal record whose origin is later removed stays valid and stays in the set). The effective cutoff therefore only rises, and every node computes the same removed set whatever order records arrive in.
   - Without this, two nodes that remove each other during a split diverge permanently: each record lies above the cutoff set by the other, so whichever record a node merged first wins on that node.
   - **Documented limits:** a mutual removal during a split removes both nodes only when both records reach a node. A node that applies one removal closes the other remover (§3.3 step 4), so the second record may never spread; then only the first-removed node is removed everywhere *(amended during implementation)*. If both were the last two effective hosts, the mesh has no host and cannot recover in-band. A stolen device that removes other members *before its own removal reaches them* has those removals honoured. Its data writes are still retracted.
4. **R19 export filter** (handoff Must 1). `export_unsynced_local_ops(now) -> Export { items: Vec<SyncItem>, expired: Vec<SignedOp> }`. Own `Set` ops with `hlc.ms < now − TOMBSTONE_TTL_MS` go to `expired` instead of `items`, so a re-bootstrap never resurrects a key whose tombstone other replicas already GC'd. The `Covered` markers in `items` are computed as before over the seqs that remain. Added to sub-spec 1 §5.7.
5. **Own-op skew exemption** (handoff Should). Own-origin ops with `hlc ≤ clock.last()` skip the skew check in `apply`, so a re-bootstrap after the wall clock moved back does not block local writes until the clock catches up.
6. **Corrupt `meta.last_hlc`** (`ms ≥ 2^48 − 1`) is rejected as `Corrupt` on open instead of panicking on `tick`.
7. **Client role.** The role exists in the data and in invites. **Partial replication for client nodes (`_mesh` only) moves to sub-spec 8.** In sub-spec 2 a client member replicates everything. Reason: filtering by namespace leaves holes in the per-origin version vector; doing it safely needs rules such as "never catch a host up from a client", which belong with `mesh-client`.
8. **Placement and the deployer** stay in sub-spec 3 (master §3 lists them under `mesh-core`; they will live there, but not yet). Sub-spec 2 only stores and replicates member tags.
9. **`NodeAddrBytes`** is defined as postcard-encoded iroh `EndpointAddr` bytes. Conversion lives in `mesh-core`; `mesh-proto` still does not depend on iroh.
10. **Address book refresh:** `_mesh/addr/<id>` is rewritten only when the node's address changes, not every 10 minutes (an unchanged rewrite only creates ops).

## 3. Crate structure

| Module | Job |
|---|---|
| `config` | `NodeConfig { data_dir, name, platform, tags, relay: RelayChoice, discovery: bool, bind_addr, timings: Timings, wall: Arc<dyn WallClock> }`. `RelayChoice = Default \| Custom(Url) \| Disabled`. `Timings` (sync interval, backoff bounds, maintenance intervals, silence threshold) and `wall` let tests shorten intervals and move the clock. |
| `local` | Node-local files in `<data>/` (§3.1). Atomic writes: write to a temp file, fsync, rename. |
| `db` | `StoreHandle`: `Arc<Store>` with `async` wrappers that run every store call on `spawn_blocking` (handoff Must 6). |
| `membership` | The `_mesh` schema (§4), `MeshAcceptor`, and `MembershipSnapshot`. |
| `transport` | The iroh `Endpoint` and `Router`; `Gate`, which wraps every protocol handler except `join/1`. |
| `peers` | `PeerManager`: one task per target peer; dial, backoff, connection ownership, RTT and last-seen. |
| `sync` | The `sync/1` session (§6). |
| `join` | Issuing invites and both sides of `join/1` (§5). |
| `rebootstrap` | The stale-node flow (§7). |
| `blobs` | Minimal iroh-blobs integration (§8). |
| `maint` | Scheduled work (§9). |
| `node` | The public API (§3.2), the supervisor that owns all tasks, `NodeEvent`, `NodeStatus`. |

### 3.1 Data directory

| File | Content |
|---|---|
| `node.key` | Ed25519 secret key; created on first run with owner-only permissions (Unix mode 0600; on Windows an ACL for the owner only). A file that exists but does not parse makes startup fail with `BadKey`; a new identity is never created silently. |
| `mesh.json` | `MeshGenesis` (founder, nonce). Written only once membership is established (create, or an `Accepted` join reply). |
| `store.redb` | The `mesh-store` file. |
| `invites.json` | `hex(blake3(token)) → { role, expires_ms, redeemed_by: Option<NodeId> }`. Expired, unredeemed entries are pruned by `maint`. |
| `rebootstrap.bin` | The in-progress re-bootstrap export (§7). |
| `expired-edits.bin` | Own edits left out of a re-bootstrap by the R19 filter, for the UI. |
| `blobs/` | The iroh-blobs `FsStore`. |

### 3.2 Public API

```rust
impl Node {
    pub async fn create_mesh(cfg: NodeConfig) -> Result<Node, NodeError>;              // founder
    pub async fn join(cfg: NodeConfig, ticket: InviteTicket) -> Result<Node, NodeError>;
    pub async fn open(cfg: NodeConfig) -> Result<Node, NodeError>;                     // existing data dir
    pub fn first_sync_complete(&self) -> bool;                                          // sticky (amended during implementation)
    pub async fn wait_first_sync(&self, timeout: Duration) -> bool;
    pub async fn clock_floor_ahead_ms(&self) -> Option<u64>;                            // sticky query
    pub async fn invite(&self, role: Role, ttl: Duration) -> Result<InviteTicket, NodeError>;
    pub async fn remove(&self, id: NodeId) -> Result<(), NodeError>;
    pub async fn set_info(&self, name: Option<String>, tags: Option<Vec<String>>) -> Result<(), NodeError>;
    pub async fn set_tags_of(&self, id: NodeId, tags: Vec<String>) -> Result<(), NodeError>;
    pub fn node_id(&self) -> NodeId;
    pub fn mesh(&self) -> MeshId;
    pub fn members(&self) -> Vec<MemberStatus>;   // id, role, info, effective, online, rtt, last_seen, lag
    pub fn status(&self) -> NodeStatus;
    pub fn store(&self) -> &StoreHandle;          // reads + local writes; sub-spec 3 builds meshnode:kv on this
    pub fn blobs(&self) -> &Blobs;
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent>;
    pub fn sync_now(&self);                       // start a round with every connected peer (Android alarm hook)
    pub fn network_change(&self);                 // forwards to Endpoint::network_change (Android hook)
    pub async fn shutdown(self);
}

pub enum NodeStatus {
    Running,
    Rebootstrapping { waiting_for_self_seq: u64 },
    Removed { by: NodeId },
    RefusedAsRemoved { by: Vec<NodeId> },        // peers closed with REMOVED; the record itself has not arrived
}

pub enum NodeEvent {
    PeerUp(NodeId), PeerDown(NodeId), MembersChanged, RoundComplete(NodeId), FirstSyncComplete,
    RemovedSelf { by: NodeId }, RefusedAsRemoved(NodeId), NoEffectiveHost,
    RebootstrapStarting, RebootstrapDone, ExpiredOfflineEdits { count: usize, keys: Vec<(String, String)> },
    ClockFloorAhead { ms: u64 }, RetractionUnrestorable { origin: NodeId, keys: usize },
}
```

- `open` fails with `WrongMesh` if `store.redb` belongs to a different mesh than `mesh.json`, with `NotInitialised` if there is no `mesh.json`, and with `BadKey` as above.
- `create_mesh` and `join` fail with `AlreadyInMesh` if `mesh.json` exists.
- `StoreHandle`'s local writes fail with `NodeError::Removed` once the node is `Removed`, and with `Rebootstrapping` while the store refuses them.
- `ClockFloorAhead` is emitted only at open, when `store.clock_floor_ahead_ms()` returns `Some` (handoff Should). The state is queried, not re-emitted: use `clock_floor_ahead_ms()`; likewise `first_sync_complete()` / `wait_first_sync(timeout)` for `FirstSyncComplete` *(amended during implementation)*.
- `RetractionUnrestorable` is logged at `info` and emitted for diagnostics only; it over-reports (handoff Should, R3) and must not be shown to users as data loss.

### 3.3 Reacting to changes

- Nobody passes ops around by hand. The node subscribes to the store's `Change` events and treats each event only as a wake-up signal:
  - every event wakes every session's live-push loop (§6.4);
  - an event in namespace `_mesh` (or any `Lagged` error) triggers a **membership rescan**.
- A membership rescan (always a full rebuild, so lost events cost nothing; handoff Must 8):
  1. Rebuild `MembershipSnapshot` from the store.
  2. For every origin whose effective cutoff is above the cutoff last applied, call `store.on_removal(origin, cutoff)`. Applied cutoffs are cached in memory; the cache is cleared on `Lagged` and on restart, which is safe because `on_removal` is idempotent.
  3. `store.retry_pending(acc)`.
  4. Close connections to peers that are no longer effective members (code `REMOVED`).
  5. Update `PeerManager`'s target set.
  6. If this node is no longer effective: `status = Removed { by }`, refuse local writes, emit `RemovedSelf` (handoff Must 5). Removal by another node stops every session and the peer manager at once. **Sign-out** (this node removing itself) shows `Removed` at once but keeps its sessions until a peer proves it holds the record (a `REMOVED` close, or a peer `Hello` advertising `vv[self]` ≥ the record's seq); then it stops, so the record is not stranded on this node *(amended during implementation)*. The node stays in this state across restarts; leaving it is `reset identity` (sub-spec 4).
  7. If no effective host remains, emit `NoEffectiveHost`.

## 4. Membership in `_mesh`

`<hex>` is the 64-character lower-case hex of a `NodeId`.

| Item | Kind | Content | Written by |
|---|---|---|---|
| set `members` | grow-only set | `node_id[32] ‖ role[1]` (`0` = host, `1` = client) | the founder for itself at creation; the inviter at join |
| set `removed` | grow-only set, exempt from retraction (§2.3) | `postcard(RemovalRecord { node_id, cutoff_seq })`; the op's origin is the remover | the remover |
| set `consumed_invites` | grow-only set | `blake3(token)` | the inviter at join |
| register `info/<hex>` | LWW | `postcard(MemberInfo { name, platform, tags })` | the inviter at join (from the request), then the node itself; any member may change tags |
| register `addr/<hex>` | LWW | `postcard(EndpointAddr)` | the node itself, when its address changes |

**Derived values (`MembershipSnapshot`):**
- `member(X)` ⇔ `X == genesis.founder` or `members` contains `X‖0` or `X‖1`.
- `role(X)` = host if `X` is the founder or `members` contains `X‖0`; otherwise client.
- `cutoff(X)` = the maximum `cutoff_seq` over removal records naming `X`. `removed(X)` ⇔ at least one such record.
- `effective(X)` ⇔ `member(X)` and not `removed(X)`. `removed_by(X)` = the origin of the record with the highest cutoff, ties broken by lowest origin.

**`MeshAcceptor`** holds the genesis and judges membership only (the cutoff check stays in the store):
`judge(op, view) = Valid` if `op.origin == founder` or `view.set_contains("_mesh", "members", origin‖0 | origin‖1)`; otherwise `UnknownOrigin`. It reads only through `ReadView` and never calls `Store` (handoff Must 7).

**`remove(X)`**, in order:
1. Refuse with `NotEffective` if this node is not effective, or if `X` is not an effective member.
2. Refuse with `LastHost` if `X` is the only effective host.
3. `cutoff_seq = store.version_vector()[X]`, the highest seq of `X` seen with no gaps.
4. `set_add("_mesh", "removed", postcard(RemovalRecord { X, cutoff_seq }))`.

The resulting `_mesh` change runs the rescan (§3.3), which calls `on_removal`, closes `X`'s connection and drops it from the dial set. A node may remove itself (sign-out), subject to rule 2.

**Learning of one's own removal.** Every peer refuses a removed node, so the record may never reach it. A connection closed with `REMOVED` sets `status = RefusedAsRemoved { by }` (accumulating peers) and emits `RefusedAsRemoved`. The close code is a peer's claim, not a signed record, so the node does not stop on it: it keeps reconnecting and its local writes still work. A `REMOVED` close sets that peer's backoff to the maximum *(amended during implementation)*. If the removal record does arrive (from a peer that has not heard of the removal yet), the status becomes `Removed` (§3.3 step 6).

**Documented limits** (for the UI and the CLI help): those in §2.3; plus master §6's: a removed device keeps the data it already replicated, and a removal takes effect on each node only once that node has received it. A device invited by `X` after `X`'s cutoff loses its membership once `X`'s removal arrives (the member-add op is retracted like any other).

## 5. Invites and `join/1`

### 5.1 Messages (`mesh-proto/src/join.rs`)

```rust
pub struct MeshGenesis { pub founder: NodeId, pub nonce: [u8; 32] }
impl MeshGenesis { pub fn mesh_id(&self) -> MeshId; }   // blake3("meshnode/mesh/1" ‖ founder ‖ nonce)

pub struct JoinRequest { pub mesh: MeshId, pub token: [u8; 32], pub info: MemberInfo }
pub enum JoinResponse {
    Accepted { genesis: MeshGenesis, peers: Vec<(NodeId, NodeAddrBytes)> },
    Refused(RefuseReason),
}
pub enum RefuseReason { Expired, Used, Unknown, WrongMesh, Removed, InviterUnavailable }

pub struct MemberInfo { pub name: String, pub platform: String, pub tags: Vec<String> }
pub struct RemovalRecord { pub node_id: NodeId, pub cutoff_seq: u64 }
```

Every top-level message carries the version byte (sub-spec 1 §4). Limits on decode: `name` ≤ 64 bytes, `platform` ≤ 32 bytes, ≤ 16 tags of ≤ 32 bytes each, `peers` ≤ 64 entries of ≤ 2 KiB each. Frames are a `u32` big-endian length followed by the message; a frame over 256 KiB is refused.

`blake3` becomes a `mesh-proto` dependency (for `mesh_id`).

### 5.2 Issuing: `invite(role, ttl = 15 min)`

- Refused with `NotEffective` unless the node is effective and `Running`.
- Generates a random 32-byte token and writes `hex(blake3(token)) → { role, expires_ms = now + ttl, redeemed_by: None }` to `invites.json`.
- Returns `InviteTicket { mesh, inviter: own EndpointAddr bytes, token, role, expires_ms }`. Only this node can redeem it, so it must be online when the joiner uses it.

### 5.3 Redeeming

1. **Joiner:** `Node::join(cfg, ticket)` fails with `AlreadyInMesh` if `mesh.json` exists. It loads or creates `node.key`, binds the endpoint, and dials `ticket.inviter` on ALPN `meshnode/join/1`. If the dial fails: `InviterUnreachable`.
2. **Joiner:** sends `JoinRequest { mesh: ticket.mesh, token, info }`.
3. **Inviter:** handles join requests one at a time (a mutex) and checks, in order, replying `Refused(reason)` on the first failure:
   1. `mesh` equals its own → else `WrongMesh`;
   2. `blake3(token)` is in `invites.json` → else `Unknown`;
   3. `now ≤ expires_ms` by the inviter's clock → else `Expired`;
   4. the joiner's `NodeId` (authenticated by iroh) has no removal record → else `Removed`;
   5. `blake3(token)` is not in `consumed_invites`, **or** `redeemed_by == joiner` (a retry after a lost reply), **or** the token is consumed, `redeemed_by` is unset, and `members` already holds `joiner‖role` (the crash window between the store writes and `redeemed_by`; the writes are re-applied) *(amended during implementation)* → else `Used`;
   6. the inviter is effective and `Running` → else `InviterUnavailable`.
4. **Inviter, on success** (skipped on an idempotent retry, which only re-sends the reply): `set_add(members, joiner‖role)`, `set_add(consumed_invites, blake3(token))`, `set(info/<joiner>, request.info)`; then `redeemed_by = joiner` in `invites.json`. It replies `Accepted { genesis, peers }`, where `peers` is every effective member except the joiner, with the addresses from `addr/<hex>` (its own current address for itself).
5. **Joiner:** checks `genesis.mesh_id() == ticket.mesh` (else `JoinRefused(WrongMesh)`, nothing written). Then it writes `mesh.json`, opens the store, starts the node, and gives `PeerManager` the listed peers' addresses. A crash before `mesh.json` is written leaves nothing behind, and the same ticket can be retried.
6. The first completed round (§6.3) emits `FirstSyncComplete`. The joiner then writes its own `addr/<hex>` and `info/<hex>`.

`Node::join` returns once the node has started. Callers that need the full state wait for `FirstSyncComplete`.

**Founder:** `create_mesh` picks a random nonce, writes `mesh.json`, opens the store with `genesis.mesh_id()`, and writes `set_add(members, self‖0)`, `info/<self>` and `addr/<self>`.

## 6. Replication: the `sync/1` session

### 6.1 Connections

- `PeerManager` holds a target set (effective members except this node). Each target has one task that keeps **one** connection on ALPN `meshnode/sync/1`.
- Both sides dial when they have no connection. If two connections exist between a pair, the one dialled by the lower `NodeId` is kept and the other is closed with `SHUTDOWN`. After a 2 s grace, a newer connection replaces an existing one whoever dialled it (crash/restart recovery); within the grace the lower-`NodeId` rule applies *(amended during implementation)*.
- Dial addresses: the `addr/<hex>` register, the addresses from the join reply, and iroh discovery when `discovery` is on.
- Backoff on failure: 1 s doubling to 5 min, reset on a completed round. `network_change()` resets every backoff.
- `PeerUp` / `PeerDown` events on connect and disconnect.

### 6.2 Gate

Every incoming connection on `sync/1` and the blobs ALPN is checked right after the handshake against the current `MembershipSnapshot`, using the `NodeId` iroh authenticated:
- not a member → close with `NOT_MEMBER`;
- removed → close with `REMOVED`;
- otherwise hand it to the protocol handler.

`join/1` is not gated; it needs a valid ticket instead. Outgoing connections are only made to effective members. Close codes: `NOT_MEMBER = 1`, `REMOVED = 2`, `PROTOCOL = 3`, `SHUTDOWN = 4`.

### 6.3 Session protocol

The dialer opens one bidirectional stream. Frames are a `u32` big-endian length followed by a postcard `SyncMsg`. The connection is closed with `PROTOCOL` on: a frame over 1 MiB + 1 KiB; an `Ops` message with more than 256 ops, or with more than 1 MiB of encoded ops *and* more than one op (a single max-size op, about 1 MiB + 800 B, must replicate) *(amended during implementation)* (handoff Should); a decode error; a `Hello` whose `mesh` differs. The peer task then reconnects with backoff.

Per-session state:
- `advertised: VersionVector` — the vv from the peer's latest `Hello`;
- `known: VersionVector` — what this side believes the peer holds. Reset to `advertised` on each `Hello`; raised by every op or marker sent, and by every op received from the peer.
- `hello_sent: bool`, `done_sent: bool`, `done_received: bool` for the current round.

**Rounds.** A round is a full two-way exchange.
1. Either side starts a round by sending `Hello { mesh, vv: store.version_vector() }` and setting `hello_sent`.
2. On receiving a `Hello`: if `hello_sent` is false for the current round, reply with this side's `Hello`. (If both sides send at the same time, each takes the other's as the reply.) Set `advertised = known = their vv`, reset `done_*`.
3. Stream `diff_since(known, 256)` page by page as `Ops` / `Covered` messages, raising `known` as it goes, until a page is empty. Then send `Done`.
A side never starts a round while its own `Hello` is unanswered or a reply is due; the dialer's interval tick restarts a round that has been in progress longer than `sync_interval` *(amended during implementation)*.
4. The round is complete on this side when it has sent `Done` and received `Done`. Call `mark_full_sync(advertised[self])`, the peer's confirmed seq for this node (handoff Must 3, R15), emit `RoundComplete`, and (first time only) `FirstSyncComplete`. Clear `hello_sent` for the next round.

**When rounds start:**
- when the connection opens (the dialer starts);
- every `timings.sync_interval` (default 60 s; the dialer starts);
- on `sync_now()` (either side);
- on any message received after more than `timings.silence` (default 60 s) without one (the receiver starts). This is the Android rule from master §10.2: tokio timers stall in deep sleep, but incoming traffic wakes the process.

### 6.4 Live mode

- After the first `Hello` from the peer has been received, a store `Change` wake-up (coalesced through a `tokio::sync::Notify`) runs `diff_since(known, 256)` and sends what it returns, raising `known`, until a page is empty.
- This also runs during a round; ops are ops.
- Backpressure: sending awaits the QUIC stream. A slow peer gets a larger diff later; nothing is queued per op.
- Relaying follows from this: ops applied from peer B wake the session with C, and `known` keeps them from being sent back to B. Two nodes that cannot reach each other converge through any third.

### 6.5 Receiving

- `Ops { ops }`: `apply_batch(ops, acc)` on `spawn_blocking` (one transaction per message; handoff Must 6). Then raise `known[origin]` for each op.
- `Covered { origin, upto }`: `apply_covered(origin, upto, advertised[origin], acc)`, always with the bound from the peer's latest `Hello` (handoff Must 4, R16). A marker above it (possible in live mode) is ignored by the store and repaired by the next round.
- `Hello`, `Done`: §6.3.
- Bad signatures and equivocation are logged by the store; the session continues. A `StoreError` closes the session, and it is retried with backoff.

A peer-side deferred or refused op leaves `known` too high until the next round resets it from `Hello`. Rounds are what guarantee convergence; live mode is what makes it fast.

### 6.6 Per-peer metrics

- **Lag** = Σ over origins of `max(0, own_vv[o] − known[o])`: ops this node has that the peer may lack.
- **RTT** from iroh's connection stats.
- **Last seen** = the time of the last message received.

## 7. Stale-node re-bootstrap

The check runs when the node opens and before each round. If `store.is_stale()` (handoff Must 2; R10, R18, R19, R20):

1. Emit `RebootstrapStarting` and log a `warn`.
2. `s = store.synced_self_seq()` — **before** the reset, which deletes it. `export = store.export_unsynced_local_ops(now)` (§2.4). Write `{ s, export.items, export.expired }` to `rebootstrap.bin` and fsync.
3. `store.reset_to_empty()`. The store refuses local writes with `Rebootstrapping` from here until the own ops are back (sub-spec 1 §5.7). `status = Rebootstrapping { waiting_for_self_seq: s }`.
4. Wait until this node is a member in the store *(amended during implementation)*, then run ordinary rounds (§6.3) with online peers. The emptied store has nothing to serve, so in practice this is a pull. After each completed round, check `version_vector()[self] ≥ s`. If not, the peer lacked this node's synced ops; continue with the next round and other peers on the normal schedule.
5. Once `vv[self] ≥ s`: apply the export's items in their own order *(amended during implementation)* (a `Covered` marker precedes the op after each hole; trusted local data, `advertised = upto`). A self `Covered` item is never applied until `vv[self]` covers every exported op before it, and the sequence ends with a self `Covered` up to the export-time last seq (a trailing hole). Local writes are drained (in-flight writes finish) before the export. `rebootstrap.bin` also carries peer address hints. A present self-removal record ends the run as `Removed`.
6. If `export.expired` is non-empty, write it to `expired-edits.bin` and emit `ExpiredOfflineEdits { count, keys }`.
7. Delete `rebootstrap.bin`, set `status = Running`, emit `RebootstrapDone`. Live mode pushes the re-applied ops.

**Crash safety:** if `rebootstrap.bin` exists when the node opens, the flow resumes at step 3 (repeating `reset_to_empty` is harmless). The export is never recomputed from a half-reset store.

**Single-node mesh:** it never completes a round, so it is never stale (sub-spec 1 §5.7) and never re-bootstraps.

## 8. Blobs (minimal)

- An iroh-blobs `FsStore` at `<data>/blobs/`, its protocol registered on the router behind the gate (§6.2).
- API:

  ```rust
  impl Blobs {
      pub async fn add_bytes(&self, bytes: Bytes) -> Result<Hash, NodeError>;
      pub async fn add_path(&self, path: &Path) -> Result<Hash, NodeError>;   // file → blob; directory → collection
      pub async fn has(&self, hash: Hash) -> Result<bool, NodeError>;
      pub async fn fetch(&self, hash: Hash) -> Result<(), NodeError>;         // raw blob only; from peers; no-op if present
      pub async fn fetch_collection(&self, hash: Hash) -> Result<(), NodeError>;  // collection root and children (amended during implementation)
      pub async fn has_collection(&self, hash: Hash) -> Result<bool, NodeError>;
      pub async fn read(&self, hash: Hash) -> Result<Bytes, NodeError>;
      pub async fn export(&self, hash: Hash, dest: &Path) -> Result<(), NodeError>;
  }
  ```
- The blob kind is supplied by the caller: `fetch(hash)` is raw-only; `fetch_collection(hash)` and `has_collection(hash)` handle collections. There is no content sniffing. A collection root and its names blob are capped at 16 MiB *(amended during implementation)*.
- `fetch` tries online effective members in RTT order and moves to the next on failure; iroh-blobs verifies the hash while streaming. It fails with `BlobUnavailable` when every peer fails.
- Everything added or fetched is protected by a named tag `meshnode/pin/<hash>`. iroh-blobs' own GC is disabled. Unpinning and reference-based GC are sub-spec 3.

## 9. Scheduled work (`maint`)

| Job | Interval | Notes |
|---|---|---|
| `retry_pending` | 60 s, and after every membership rescan | |
| `compact` | 1 h | skipped by the scheduler while re-bootstrapping *(amended during implementation)* |
| `gc` | 24 h | the store does nothing while stale; skipped while re-bootstrapping *(amended during implementation)* |
| address refresh | on iroh's address-change watcher | writes `addr/<self>` only if the encoded value changed |
| prune `invites.json` | 1 h | drops expired, unredeemed entries |

All intervals come from `Timings`. Store calls go through `spawn_blocking`. Late timers (Android deep sleep) are harmless.

## 10. Errors

```rust
pub enum NodeError {
    Io(std::io::Error), Store(StoreError), Proto(ProtoError), Iroh(String),
    BadKey, NotInitialised, WrongMesh, AlreadyInMesh,
    NotEffective, LastHost, Removed, Rebootstrapping,
    JoinRefused(RefuseReason), InviterUnreachable, BlobUnavailable,
}
```

| Situation | Behaviour |
|---|---|
| Dial or session failure | Backoff 1 s → 5 min; never stops the node. |
| Panic in a peer, session or maint task | Caught by the supervisor (`JoinHandle` error), logged at `error`, task restarted with backoff. |
| Protocol violation by a peer | Close with `PROTOCOL`, log `warn` with the peer id. |
| Connection from a non-member or removed node | Close with `NOT_MEMBER` / `REMOVED`, log `info`. |
| `StoreError::Full` on a local write | Returned to the caller; reads continue. |
| Join refused | `NodeError::JoinRefused(reason)`. |

## 11. Testing

### 11.1 Harness

In-process tests in `crates/mesh-core/tests/`. Each node gets: a temp data dir, `RelayChoice::Disabled`, `discovery: false`, a loopback bind, peers' addresses passed directly, short `Timings` (sync interval 200 ms, silence 1 s, maint jobs triggered explicitly), and its own `ManualClock`. A `test-hooks` feature adds:
- `Node::partition(&[NodeId])` / `Node::heal()`: close connections to, and refuse dials and accepts from, the listed peers;
- `Node::maint_now(job)`;
- a way to force a `Lagged` on the change subscription;
- a one-shot "drop the next join reply" switch on the inviter;
- a way to send a raw `SyncMsg` frame (for the protocol-violation test).

A helper `converged(&[&Node])` waits (with a timeout) until all nodes return equal `get` / `list_keys` / `set_members` results over every namespace.

### 11.2 Tests

| Test | Checks |
|---|---|
| `create_then_invite_join` | the joiner sees every key and member; `FirstSyncComplete` fires; the genesis check passes |
| `ticket_reuse_refused` | a second node using the same ticket gets `Used` |
| `expired_ticket_refused` | `Expired` |
| `join_retry_same_node_idempotent` | reply dropped (test hook) and retried with the same ticket → `Accepted`, one member record |
| `forged_ticket_wrong_genesis` | a ticket whose `mesh` doesn't match the inviter's genesis → refused, nothing written |
| `removed_node_cannot_rejoin` | an invite redeemed by a removed id → `Removed` |
| `live_push_latency` | a write on A is visible on B within 1 s without waiting for a round |
| `split_and_heal` | 3 nodes; partition {A} / {B, C}; concurrent writes, deletes and set-adds on both sides; heal → `converged` |
| `relay_through_third_node` | A and C partitioned from each other, both connected to B; writes on A reach C and vice versa |
| `late_join_gets_history` | 1 000 ops before the join; the joiner converges |
| `removal_closes_and_refuses` | B removes C: C's connections close; C's reconnects get `REMOVED`; C's ops above the cutoff are retracted on every node; C's status becomes `RefusedAsRemoved` |
| `removed_self_stops` | the removal record reaches C: `Removed`, local writes refused |
| `mutual_removal_converges` | A and B remove each other while partitioned (C is a third host); after heal, every node shows both removed, for both delivery orders |
| `last_host_cannot_be_removed` | `LastHost` |
| `stale_rebootstrap` | A goes offline; B and C work on; A's clock jumps 81 days, with one A edit older than 90 days and one recent unsynced edit; A reconnects → the old edit is in `expired`, the recent edit reaches B and C, no key deleted on B and C comes back |
| `rebootstrap_resumes_after_crash` | the node is dropped after step 3; reopening resumes and completes |
| `rebootstrap_waits_for_self_seq` | the first peer lacks A's synced ops; A waits until a peer that has them completes a round |
| `skew_deferred_then_applied` | B's clock 10 min ahead: its ops are deferred on A, then applied once A's clock catches up |
| `lagged_events_rescan` | a forced `Lagged` during a removal; the removal still takes effect |
| `blob_add_fetch_gated` | a member fetches a blob and a collection; a non-member endpoint is refused |
| `oversized_batch_closes_session` | a crafted `Ops` message with 257 ops → `PROTOCOL` close |
| `restart_resumes` | a node shut down and reopened reconnects and converges; `BadKey` and `WrongMesh` refuse to start |

Plus:
- `mesh-proto`: round-trip and version tests for every §5.1 type; `MeshGenesis::mesh_id` test vector; limit violations rejected.
- `mesh-store`: `removal_record_survives_issuer_removal`, `export_puts_old_sets_in_expired`, `own_op_below_clock_skips_skew`, `corrupt_last_hlc_rejected`.

### 11.3 Demo and manual check

`cargo run -p mesh-core --example meshdev -- <data-dir> init | join <ticket> | run`. `init` and `join` drop into the same REPL as `run`:

`invite [host|client]`, `members`, `put <ns> <key> <value>`, `get <ns> <key>`, `del <ns> <key>`, `ls <ns> [prefix]`, `remove <id-prefix>`, `status`, `sync`, `blob add <path>`, `blob get <hash> <out>`, `quit`.

It uses real relays and discovery and logs `NodeEvent`s as they happen. It is replaced by `meshd` in sub-spec 4.

Manual check, recorded in `docs/manual-checks/sub-spec-2.md`:
1. Desktop 1 `init` and `invite`; Desktop 2, on a different network, `join`. Both show each other online within 30 s.
2. Disconnect Desktop 2's network. Write and delete different keys on both, and the same key on both.
3. Reconnect. Within 60 s both show the same `ls` results, and the same winner for the shared key.
4. `blob add` on one, `blob get` on the other.
5. `remove` Desktop 2 from Desktop 1; Desktop 2 shows `RefusedAsRemoved` or `Removed` within 60 s.

## 12. Handoff items: where they are handled

| Handoff item | Where |
|---|---|
| Must 1, R19 resurrection | §2.4, §7 step 6 |
| Must 2, re-bootstrap ordering | §7 |
| Must 3, `mark_full_sync` | §6.3 step 4 |
| Must 4, `apply_covered` bound | §6.5 |
| Must 5, removal of this node | §3.3 step 6, §4 |
| Must 6, `spawn_blocking` + `apply_batch` | §3 `db`, §6.5 |
| Must 7, `ReadView`-only acceptor | §4 |
| Must 8, lossy events | §3.3 |
| Should: receive-side limits | §6.3 |
| Should: own-op skew exemption | §2.5 |
| Should: `clock_floor_ahead_ms` | §3.2 (`ClockFloorAhead`) |
| Should: R3 over-reporting | §3.2 (`RetractionUnrestorable`, not shown as data loss) |
| Minor: corrupt `last_hlc` | §2.6 |

The other minor items (Corrupt errors without the key, `SetAdd` row metadata, crash-test details, missing store tests) stay open.

## 13. Out of scope here

Placement, manifests and the deployer, and blob GC (sub-spec 3); the admin API, `meshd`, reset identity, and the OS services (sub-spec 4); Android power policy beyond the `sync_now()` and `network_change()` hooks (sub-spec 5); partial replication for client-role nodes (sub-spec 8); public access (master §15).
