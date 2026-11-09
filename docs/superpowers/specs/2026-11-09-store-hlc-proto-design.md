# Sub-spec 1 — HLC, wire formats, replicated store

**Date:** 2026-11-09
**Status:** Draft, awaiting owner review
**Parent:** `2026-11-08-meshnode-design.md` (§4 identity, §5 store, §6 membership acceptance, §14 row 1)
**Delivers:** crates `mesh-hlc`, `mesh-proto`, `mesh-store`. No networking. Fully testable in-process.

---

## 1. Goal and success criteria

Build the data layer every other part rests on: a clock, the signed operation format, and a store that **any number of replicas converge on, whatever order operations arrive in, with duplicates and gaps**.

Done when:
1. Property tests show convergence across random op sets, delivery orders, duplicates, partial catch-ups and compaction, with ≥ 10 000 cases per property in CI.
2. Every rule in §5 below has at least one example test named after the rule.
3. The store survives a crash at any point in a write (redb transaction) with no lost or half-applied op. This is tested by killing a child process mid-write and reopening.
4. `cargo test`, `clippy -D warnings`, `fmt --check` pass on Linux and Windows. `cargo ndk build -t arm64-v8a` compiles. The workspace MSRV is `rust-version = "1.90"` (raised from 1.85 during implementation: redb 4.x needs 1.90).

## 2. Change to the parent spec (made here, to be back-ported)

**Removal cutoff by sequence number, not HLC.** The parent spec §6 defined a removal's cutoff as `cutoff_hlc`. That has two flaws:
- A removed device can backdate HLCs to get under the cutoff.
- A node that merged a removed device's late ops **before** hearing of the removal can disagree forever with a node that heard first, because compaction can delete the older value it would need to fall back to.

New rule:
- A removal records **`cutoff_seq`**: the highest `seq` of the removed origin the remover had seen with no gaps.
- Ops from that origin with `seq ≤ cutoff_seq` are valid; ops above it are invalid. With several removal records for the same origin, the **highest** `cutoff_seq` wins: every op some remover had seen counts, which is deterministic and causes the fewest retractions.
- A node that learns of a removal **retracts** that origin's ops above the cutoff and recomputes the affected keys (§5.6).
- To keep retraction exact, **an op is only compacted away after it has been superseded for at least 7 days** (§5.5).

The parent spec's §6 "honest limit" about backdating goes away.

**Crate dependencies change:** `mesh-store → mesh-proto → mesh-hlc`. The store needs the op type, which the parent spec placed in proto.

## 3. `mesh-hlc`

```rust
/// 48-bit Unix milliseconds | 16-bit logical counter, totally ordered as a u64.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
pub struct Hlc(u64);

impl Hlc {
    pub const ZERO: Hlc;
    pub fn new(ms: u64, counter: u16) -> Hlc;   // panics if ms ≥ 2^48 (year 10889)
    pub fn ms(self) -> u64;
    pub fn counter(self) -> u16;
}

pub trait WallClock: Send + Sync { fn now_ms(&self) -> u64; }   // SystemClock + ManualClock (tests)

pub struct Clock<W: WallClock> { /* last: Hlc, wall: W */ }
impl<W: WallClock> Clock<W> {
    pub fn new(wall: W, floor: Hlc) -> Self;        // floor = highest HLC already persisted (never go backwards)
    pub fn tick(&mut self) -> Hlc;                   // local event
    pub fn observe(&mut self, remote: Hlc) -> Result<Hlc, Skew>;  // receive; Err if remote > wall + MAX_SKEW
}
pub const MAX_SKEW_MS: u64 = 5 * 60 * 1000;
```

- `tick`: `ms = max(wall, last.ms)`. If `ms == last.ms`, then `counter = last.counter + 1`, otherwise `0`. If the counter would overflow, `ms` is bumped by 1 and the counter resets to 0 (the same "carry into the wall" fix the Aegis client uses).
- `observe`: if `remote.ms > wall + MAX_SKEW_MS` it returns `Err(Skew)` and the clock is **not** advanced. Otherwise it applies the standard HLC receive rule and then ticks.

## 4. `mesh-proto` (this sub-project's part)

Encoding: `postcard` (serde). Every top-level message starts with a `u8` format version, currently `1`. Unknown versions are rejected with `ProtoError::Version`.

```rust
pub struct MeshId(pub [u8; 32]);
pub struct NodeId(pub [u8; 32]);            // Ed25519 public key; same bytes as iroh's EndpointId

pub enum Payload {
    Set    { ns: String, key: String, order: Vec<u8>, value: Option<Vec<u8>> }, // plain set = empty order; None = tombstone
    SetAdd { ns: String, set: String, member: Vec<u8> },
}

pub struct OpBody { pub mesh: MeshId, pub origin: NodeId, pub seq: u64, pub hlc: Hlc, pub payload: Payload }
pub struct SignedOp { pub body: OpBody, pub sig: [u8; 64] }   // Ed25519 over b"meshnode/op/1" ‖ postcard(body)

pub struct VersionVector(pub BTreeMap<NodeId, u64>);           // origin -> highest seq covered with no gaps

// meshnode/sync/1 messages
pub enum SyncMsg {
    Hello   { mesh: MeshId, vv: VersionVector },
    Ops     { ops: Vec<SignedOp> },                        // ≤ 256 ops or ≤ 1 MiB per message
    Covered { origin: NodeId, upto: u64 },                 // "ops up to `upto` you didn't get were compacted/GC'd"
    Done,
}

pub struct InviteTicket { pub mesh: MeshId, pub inviter: NodeAddrBytes, pub token: [u8; 32], pub role: Role, pub expires_ms: u64 }
// Text form: "mesh1" + base32 (RFC 4648, no padding, lower-case) of (version byte ‖ postcard(ticket) ‖ crc32).
```

- Domain separation: the signature covers `b"meshnode/op/1"` plus the body, and the body includes `mesh`, so an op can't be replayed into another mesh or another protocol.
- `NodeAddrBytes` is an opaque `Vec<u8>` here. Sub-spec 2 fills it with iroh's serialized address, so proto doesn't depend on iroh.
- Size limits are checked on decode: `ns` ≤ 64 bytes, `key` and `set` ≤ 512 bytes, `order` ≤ 64 bytes, `value` and `member` ≤ 1 MiB. `ns`, `key` and `set` must not contain a `0x00` byte, because the store uses it as a key separator. The same checks apply to local writes.
- `join/1` and `http/1` message types are added to this crate by sub-specs 2 and 4.

## 5. `mesh-store`

### 5.1 Storage layout (one redb file)

| Table | Key | Value |
|---|---|---|
| `meta` | `&str` | mesh id, local node id, local `next_seq`, `last_hlc`, `last_full_sync_ms`, `synced_self_seq` (§5.8), schema version |
| `ops` | `(origin, seq)` | `SignedOp` bytes + `received_ms` |
| `reg` | `(ns, key, origin)` | `{ order, hlc, seq, value: Option }`: **one entry per origin per key** (that origin's current winner) |
| `set` | `(ns, set, member, origin)` | `{ hlc, seq }`: who added this member |
| `vv` | `origin` | highest seq covered with no gaps |
| `pending` | `(origin, seq)` | ops held back: out of order (gap below), unknown origin, or HLC skew; plus `reason`, `first_seen_ms` |
| `removed` | `origin` | `max cutoff_seq` (cache of `_mesh` removal records) |
| `superseded` | `(origin, seq)` | `superseded_at_ms`, for compaction after the settle period |

Reads compute the **winning register** for `(ns, key)` as the max over that key's `reg` rows by `(order, hlc, origin)`. A tombstone winner means "absent". For 2–10 origins this is a short scan of a contiguous key range.

### 5.2 Validity of an op (`Acceptor`)

The store doesn't understand membership. `mesh-core` passes it an acceptor, and the store calls it for every remote op:

```rust
pub enum Verdict { Valid, UnknownOrigin }
pub trait Acceptor: Send + Sync { fn judge(&self, op: &OpBody, view: &ReadView<'_>) -> Verdict; }
```

The acceptor judges **membership only**; the removal cutoff is checked inside the store, against its `removed` table (§5.3 step 4), next to the retraction logic. `mesh-core`'s acceptor (specified here, implemented in sub-spec 2):
- `Valid` if the origin is in `_mesh/members`.
- `UnknownOrigin` if the origin isn't a member **yet**.

`judge` runs inside the store's write transaction with the clock mutex held: it reads only through `ReadView` (`get`, `set_contains`, `set_members`, `list_keys`, all seeing the transaction's own earlier writes) and never calls the `Store`.

A test acceptor that accepts every op is provided for property tests.

### 5.3 Applying a remote op: `apply(op) -> Outcome`

All of these steps run in one write transaction:
1. Check `op.body.mesh` matches and the signature verifies. Otherwise `Rejected(BadSig)`. The op isn't stored and doesn't count toward the vv.
2. If `(origin, seq)` already exists: an identical op gives `Duplicate`. **A different op with the same seq is equivocation:** keep the first, log `error`, and return `Rejected(Equivocation)`.
3. `Clock::observe(hlc)`. If that fails with `Skew`, put the op in `pending` (reason `Skew`) and return `Deferred`. **Exemption:** an own-origin op with `hlc ≤ clock.last()` skips the skew check, so a re-bootstrap after the wall clock moved back does not block local writes until the clock catches up *(back-ported from sub-spec 2)*.
4. If the origin is in `removed` and `seq > cutoff_seq`: store it in `ops` **marked invalid** (so it counts as covered and isn't fetched again), don't merge, and return `Ignored`. Otherwise `judge`:
   - `UnknownOrigin` → put it in `pending` and return `Deferred`.
   - `Valid` → continue.
5. If `seq > vv[origin] + 1` and the range in between isn't covered, put it in `pending` (reason `Gap`) and return `Deferred`.
6. Merge (§5.4). Insert into `ops`, advance `vv[origin]`, then apply any `pending` ops of this origin that are now unblocked, in order.
7. Return `Applied`, and publish `Change` events after commit.

**Batches:** `apply_batch(ops, acc) -> Vec<Outcome>` applies a batch in one write transaction (one fsync) with exactly the outcomes and resulting state of calling `apply` on each op in turn; events are published after the commit. Sync sessions use it for `SyncMsg::Ops` batches.

**Retrying pending ops:** `retry_pending()` runs after any `_mesh` change (a new member) and once a minute, which also covers skew that has resolved. Pending entries older than 24 h are dropped (and logged). They don't advance the vv, so a later catch-up can bring them back.

### 5.4 Merge

- **`Set`:** look up `reg[(ns, key, origin)]`.
  - If the incoming op beats it by `(order, hlc)`, replace it, and record the old op's `(origin, seq)` in `superseded` with the current time.
  - If it doesn't beat it, store the op but don't touch `reg`; the op itself goes straight into `superseded`.
  - A key's winner is always derivable from `reg` alone.
- **`SetAdd`:** insert `set[(ns, set, member, origin)]` if absent. Membership = "at least one row exists".

**Local writes** (`set`, `set_ordered`, `delete`, `set_add`) are built and signed by the store with `next_seq` and `Clock::tick()`, then go through the same merge in one transaction with the `next_seq` increment. The new `SignedOp` is returned for the caller to broadcast.

### 5.5 Compaction (`compact(now)`, run hourly)

- Delete `ops` rows listed in `superseded` that are older than **`SETTLE = 7 days`**. Each delete leaves a "covered" marker, so `ops_since` can report `Covered { origin, upto }` to peers that ask for that range.
- `SetAdd` ops are never compacted.
- Invalid ops (step 4) are compacted after `SETTLE` like superseded ones.

### 5.6 Retracting a removed origin (`on_removal(origin, cutoff_seq)`)

`mesh-core` calls this when a `_mesh/removed` record is merged or its effective cutoff changes. In one transaction:
**Exemption *(back-ported from sub-spec 2)*:** adds to the set `CUTOFF_EXEMPT_SET` (`_mesh`/`removed`, the removal records) are exempt from steps 1 and 3 here, and from the "ignored above the cutoff" rule in §5.3 step 4: a removal record whose origin is later removed stays valid and stays in the set. Other ops are unaffected.

1. For every `ops` row of `origin` with `seq > cutoff_seq`: mark it invalid.
2. For every `reg` row of `origin` whose `seq > cutoff_seq`: replace it with that origin's best remaining op for the key with `seq ≤ cutoff_seq` (found in `ops`, which still holds it unless it was superseded more than `SETTLE` ago), or delete the row if there is none. A restored op is removed from `superseded`, so compaction can't delete the new winner.
3. For every `set` row of `origin` with `seq > cutoff_seq`: delete it.
4. Publish `Change` events for the affected keys.

If a later removal record **raises** the effective cutoff (the highest one wins), ops of that origin with `seq` in `(old, new]` that are still in `ops` lose their invalid mark and are merged again under §5.4.

**Remaining limit (documented, tested):** if a removal reaches a node more than **7 days** after that node compacted the removed origin's older value for a key, step 2 deletes that origin's entry for the key instead of restoring the older value. Other replicas that kept the older value disagree on **that origin's contribution to that key**, and only when that origin's entry was the winner. The node logs `warn: retraction could not restore <ns>/<key>`. This needs a device removed while a node that holds its data was partitioned for over a week. A dedicated test shows the bound.

### 5.7 Tombstone GC (`gc(now)`, daily)

- A key whose winner is a tombstone with the **empty order**, older than 90 days by **both** its `hlc.ms` and its local `received_ms`, is deleted: all of the key's `reg` rows and its ops (leaving "covered" markers). Measuring arrival too keeps a tombstone that reaches this node late (from a device that was offline) long enough to reach the other replicas. **Tombstones with a non-empty order are never GC'd:** later writes that lose to that order would otherwise resolve differently on replicas that GC'd and replicas that did not. GC does nothing while the store is stale, and skips a key that holds an own op above `synced_self_seq` (§5.8).
- **Accepted limit:** a non-stale replica whose last full sync was with a peer that also lacked a tombstone may keep the value after the tombstone is GC'd elsewhere. A stale node must not serve `diff_since`.
- `is_stale(now)` is true when `last_full_sync_ms < now − 80 days`; a store that never completed a full sync is never stale. `mesh-core` then runs the re-bootstrap from parent spec §5: `export_unsynced_local_ops()`, `reset_to_empty()`, full catch-up, then re-apply the exported items. `export_unsynced_local_ops(now)` returns `Export { items: Vec<SyncItem>, expired: Vec<SignedOp> }` *(back-ported from sub-spec 2)*. `items` holds the own ops above `synced_self_seq` in seq order, with a `Covered { origin: self, upto: seq − 1 }` before each op that follows a hole above `synced_self_seq` (own ops already compacted), so re-applying never parks an op behind a gap; `Op` items are re-applied with `apply`, `Covered` items with `apply_covered`. **R19 filter:** own `Set` ops with `hlc.ms < now − TOMBSTONE_TTL_MS` go to `expired` instead of `items`, so a re-bootstrap never resurrects a key whose tombstone other replicas already GC'd; the `Covered` markers in `items` are computed over the seqs that remain.
- A corrupt `meta.last_hlc` (`ms ≥ 2^48 − 1`) is rejected as `Corrupt` on open instead of panicking on `tick` *(back-ported from sub-spec 2)*.
- Local writes return `StoreError::Rebootstrapping` while `vv[self] ≠ next_seq − 1` (after `reset_to_empty` until the catch-up and the re-applied export restore the own ops): a write in between would set `vv[self]` past seqs the store no longer holds, and peers would skip them as covered.

### 5.8 Catch-up support

```rust
pub fn version_vector(&self) -> VersionVector;
/// Ops the peer lacks, in (origin, seq) order, at most `limit`, plus Covered markers for compacted ranges.
pub fn diff_since(&self, theirs: &VersionVector, limit: usize) -> Vec<SyncItem>;   // SyncItem = Op(SignedOp) | Covered{origin, upto}
pub fn apply_covered(&self, origin: NodeId, upto: u64, advertised: u64, acc: &dyn Acceptor);   // raise vv[origin] to `upto` (the sender compacted or GC'd that range), then drain unblocked `pending` ops of that origin
pub fn mark_full_sync(&self, synced_self_seq: u64);
```

- `apply_covered`: `advertised` is the sender's `Hello` `vv[origin]`. A marker with `upto > advertised` is ignored (no-op, `warn`), and so is one for this node's own origin with `upto > u64::MAX / 2` (it would exhaust `next_seq`).
- `mark_full_sync` is called after every **full sync**, a completed two-way exchange with a peer, including the catch-up right after joining. `synced_self_seq` is the peer's `vv[self]`: the highest own seq it confirmed holding. The store records `last_full_sync_ms = now` (for staleness) and `synced_self_seq` (the boundary for "unsynced own ops" used by GC and `export_unsynced_local_ops`, so an own write made during the session after the diff was computed never counts as synced).

The store serves ops it received from other origins too. Any replica can catch up any other.

### 5.9 Reads and events

```rust
pub fn get(&self, ns: &str, key: &str) -> Option<Vec<u8>>;
pub fn get_entry(&self, ns: &str, key: &str) -> Option<RegEntry>;     // includes order/hlc/origin; for tests and ordered services
pub fn list_keys(&self, ns: &str, prefix: &str, start_after: Option<&str>, limit: u32) -> Vec<String>; // live keys only, sorted
pub fn set_contains(&self, ns: &str, set: &str, member: &[u8]) -> bool;
pub fn set_members(&self, ns: &str, set: &str, start_after: Option<&[u8]>, limit: u32) -> Vec<Vec<u8>>;
pub fn set_member_origins(&self, ns: &str, set: &str, member: &[u8]) -> Result<Vec<NodeId>, StoreError>;   // origins that added `member`, ascending by id *(back-ported from sub-spec 2)*
pub fn next_seq(&self) -> Result<u64, StoreError>;   // the seq the next local write will use *(back-ported from sub-spec 2)*
pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Change>;   // Change { ns, kind: Key(String) | Set(String), local: bool }
pub fn namespace_sizes(&self) -> BTreeMap<String, u64>;
pub fn purge_namespace(&self, ns: &str) -> Vec<SignedOp>;             // tombstones every live key; returns ops to broadcast
```

`list_keys` with a `start_after` that has been deleted still pages correctly (redb range scan).

### 5.10 Errors and limits

- All public functions return `Result<_, StoreError>` (`Io`, `Corrupt`, `Full`, `WrongMesh`, `Rebootstrapping`, `Proto`). Read functions never panic on bad data on disk; they return `Corrupt` with the table and key.
- Opening a store whose `meta.mesh` doesn't match the expected mesh fails with `WrongMesh` (parent spec §12).
- A disk-full error rolls the transaction back and returns `Full`.

## 6. Testing

| Kind | What |
|---|---|
| Unit (`mesh-hlc`) | tick monotonic across a wall clock going backwards; counter overflow carries; observe rejects skew > 5 min and doesn't advance; floor respected after restart |
| Unit (`mesh-proto`) | round-trip every type; version byte rejected; size limits; signature covers mesh id (an op from another mesh fails); ticket text round-trip and CRC failure |
| Rule tests (`mesh-store`) | one per rule: `ordered_write_beats_newer_plain_hlc_only_when_order_higher`, `tombstone_wins_over_older_value`, `equivocation_keeps_first`, `gap_is_deferred_then_applied`, `unknown_origin_deferred_until_member`, `invalid_op_counts_as_covered`, `skew_deferred_then_applied_when_clock_catches_up`, `compaction_waits_for_settle_period`, `removal_retracts_and_restores_older_value`, `removal_after_settle_cannot_restore_and_warns`, `max_cutoff_wins_across_removals`, `tombstone_gc_after_90_days`, `stale_after_80_days`, `covered_marker_lets_peer_advance` |
| Property (`proptest`) | **Convergence:** N ∈ 2..5 replicas, random writers/keys/orders/tombstones/set-adds; deliver to each replica in a random permutation with random duplicates and random partial catch-ups via `diff_since`; after a final full exchange all replicas give equal `get` / `list_keys` / `set_members` results. **Compaction invisibility:** compacting at random points (clock advanced past SETTLE) never changes the read results of a replica that saw every op. **Retraction determinism:** applying a removal before, during or after the removed origin's ops gives equal results (within SETTLE). **GC invisibility:** random `gc()` calls after the TTL, with late tombstones and later writes, never change the live reads of non-stale replicas after a final exchange. |
| Crash safety | a child process does writes in a loop and is killed; on reopen there are no half-applied ops, and `vv` matches `ops` |
| Bench (informational) | 100 k ops apply throughput and `get` latency on desktop; recorded, not gated |

## 7. Out of scope here

Networking, gossip and the `sync/1` session logic (sub-spec 2); the membership data model in `_mesh` and the real acceptor (sub-spec 2); `join/1` and `http/1` messages (sub-specs 2 and 4); the WASM-facing KV interface (sub-spec 3 wraps §5.9).
