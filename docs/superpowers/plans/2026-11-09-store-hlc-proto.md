# Sub-spec 1 (HLC, wire formats, replicated store) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `mesh-hlc`, `mesh-proto` and `mesh-store`: a clock, signed ops, and a redb-backed store that every replica converges on whatever order ops arrive in.

**Architecture:** A Cargo workspace at the repo root with three library crates, `mesh-store → mesh-proto → mesh-hlc`. The store keeps **one register row per (namespace, key, origin)**, and the winner is computed when the key is read, so merging is "replace this origin's row if the new op beats it". Every write and remote apply is one redb write transaction. Composite keys are byte strings with `0x00` separators, ordered so prefix scans work.

**Tech Stack:** Rust (edition 2024), redb 4, postcard 1, serde 1, ed25519-dalek 3 (same major as iroh 1.4), thiserror 2, tokio 1 (`sync` only), data-encoding 2, crc32fast 1; tests: proptest 1, tempfile 3.

**Spec:** `docs/superpowers/specs/2026-11-09-store-hlc-proto-design.md` (parent: `docs/superpowers/specs/2026-11-08-meshnode-design.md`). Read the sub-spec before each task; section numbers below (§) refer to it.

## Global Constraints

- Workspace root `<meshnode-checkout>\Cargo.toml`, `resolver = "3"`, crates under `crates/`. `spikes/` is **not** a workspace member (add `exclude = ["spikes"]`).
- `edition = "2024"`, `rust-version = "1.85"` in `[workspace.package]`; all dependency versions declared once in `[workspace.dependencies]`.
- `MAX_SKEW_MS = 300_000`; `SETTLE = 7 days`; tombstone GC age = 90 days; stale threshold = 80 days; pending ops dropped after 24 h.
- Size limits (checked on decode **and** on local writes): `ns` ≤ 64 bytes, `key`/`set` ≤ 512 bytes, `order` ≤ 64 bytes, `value`/`member` ≤ 1 MiB (1_048_576). **`ns`, `key` and `set` must not contain a `0x00` byte** (needed by the key encoding; added here, enforced in proto validation).
- Signature domain: Ed25519 over `b"meshnode/op/1" ‖ postcard(OpBody)`.
- Every top-level encoded message starts with format version byte `1`.
- `SyncMsg::Ops` batches: ≤ 256 ops **and** ≤ 1 MiB encoded.
- Ticket text: `"mesh1"` + base32 (RFC 4648, lower-case, no padding) of `1u8 ‖ postcard(InviteTicket) ‖ crc32(IEEE, big-endian, over the preceding bytes)`.
- No `unwrap()`/`expect()` in non-test code paths that handle on-disk or network data; return `StoreError`/`ProtoError`.
- Must compile for `aarch64-linux-android` (`cargo ndk -t arm64-v8a build -p mesh-store`).
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Git needs `-c safe.directory=<meshnode-checkout>` on this machine.

## Review Focus

1. **Restart after writes.** Reopening a store must continue `next_seq` and never issue an HLC ≤ the last persisted one; reusing a seq would be self-equivocation. Test: `reopen_continues_seq_and_hlc` (Task 3).
2. **Namespace prefix bleed.** `list_keys("a", "", ..)` must not return keys of namespace `"ab"`, and a key prefix `"x"` must not match a key in another namespace. Test: `list_keys_does_not_bleed_across_namespaces` (Task 3).
3. **Local writes over the limits.** A service writing a 1 MiB + 1 value, or a key containing `\0`, must get `StoreError::Proto(..)`, not a stored op that peers then reject. Test: `local_write_over_limit_is_rejected` (Task 3).
4. **Own ops echoed back.** Gossip will deliver a node's own ops back to it. `apply` must return `Duplicate` and change nothing. Test: `own_op_echo_is_duplicate` (Task 4).
5. **Second open of the same file** (two daemons, or a CLI while the daemon runs) must fail cleanly with `StoreError::Io`, not corrupt the file. Test: `second_open_fails_cleanly` (Task 3).

---

## File Structure

```
Cargo.toml                         workspace
.github/workflows/ci.yml           Linux + Windows: test, clippy, fmt; Android build check
crates/mesh-hlc/src/lib.rs         Hlc, WallClock, SystemClock, ManualClock, Clock, Skew, MAX_SKEW_MS
crates/mesh-proto/src/lib.rs       re-exports + ProtoError + encode/decode helpers (version byte)
crates/mesh-proto/src/ids.rs       MeshId, NodeId, NodeAddrBytes, Role
crates/mesh-proto/src/op.rs        Payload, OpBody, SignedOp (sign/verify), validate() limits
crates/mesh-proto/src/sync.rs      VersionVector, SyncMsg, batch_ops()
crates/mesh-proto/src/ticket.rs    InviteTicket, to_text()/from_text()
crates/mesh-store/src/lib.rs       Store (public API), re-exports
crates/mesh-store/src/error.rs     StoreError
crates/mesh-store/src/keys.rs      byte-key encoding/decoding for every table
crates/mesh-store/src/tables.rs    redb TableDefinitions + value structs (postcard)
crates/mesh-store/src/merge.rs     pure: beats(), winner() over register rows
crates/mesh-store/src/local.rs     local writes (sign, tick, merge)
crates/mesh-store/src/read.rs      get/get_entry/list_keys/set_* /namespace_sizes
crates/mesh-store/src/apply.rs     remote apply pipeline, pending, retry_pending, Acceptor
crates/mesh-store/src/sync.rs      version_vector, diff_since, apply_covered, mark_full_sync
crates/mesh-store/src/maint.rs     compact, gc, is_stale, export_unsynced_local_ops, reset_to_empty, purge_namespace
crates/mesh-store/src/removal.rs   on_removal (retract / re-merge)
crates/mesh-store/tests/rules.rs   one test per rule (sub-spec §6 names)
crates/mesh-store/tests/props.rs   proptest properties
crates/mesh-store/tests/crash.rs   kill-mid-write test (spawns the test binary itself as a child)
crates/mesh-store/tests/common/mod.rs  helpers: test keys, ManualClock store, AcceptAll
```

---

### Task 1: Workspace + `mesh-hlc`

**Files:**
- Create: `Cargo.toml`, `crates/mesh-hlc/Cargo.toml`, `crates/mesh-hlc/src/lib.rs`
- Modify: `.gitignore` (add `/target/`)

**Interfaces:**
- Produces:
  - `pub struct Hlc(u64)`: `Copy, Eq, Ord, Hash, Debug, Serialize, Deserialize`; `Hlc::ZERO`, `Hlc::new(ms: u64, counter: u16) -> Hlc` (panics if `ms >= 1 << 48`), `ms()`, `counter()`, `as_u64()`, `from_u64(u64)`.
  - `pub trait WallClock: Send + Sync { fn now_ms(&self) -> u64; }`, `SystemClock`, and `ManualClock` (an `Arc<AtomicU64>` inside; `new(ms)`, `set(ms)`, `advance(ms)`; `Clone` shares the time).
  - `pub struct Clock<W: WallClock>`: `new(wall: W, floor: Hlc)`, `tick(&mut self) -> Hlc`, `observe(&mut self, remote: Hlc) -> Result<Hlc, Skew>`, `last(&self) -> Hlc`, `wall(&self) -> &W`.
  - `pub struct Skew { pub remote_ms: u64, pub wall_ms: u64 }`, `pub const MAX_SKEW_MS: u64 = 300_000;`

- [ ] **Step 1: Create the workspace.** Root `Cargo.toml` with `members = ["crates/*"]`, `exclude = ["spikes"]`, `[workspace.package]` and `[workspace.dependencies]` per Global Constraints. `mesh-hlc` depends only on `serde` (derive).
- [ ] **Step 2: Write failing tests** in `crates/mesh-hlc/src/lib.rs` (`#[cfg(test)] mod tests`):

```rust
#[test] fn ordering_is_ms_then_counter() { assert!(Hlc::new(5, 9) < Hlc::new(6, 0)); assert!(Hlc::new(5, 1) < Hlc::new(5, 2)); }
#[test] fn tick_is_monotonic_when_wall_goes_backwards() {
    let w = ManualClock::new(1_000); let mut c = Clock::new(w.clone(), Hlc::ZERO);
    let a = c.tick(); w.set(500); let b = c.tick();
    assert!(b > a); assert_eq!(b.ms(), 1_000); assert_eq!(b.counter(), a.counter() + 1);
}
#[test] fn counter_overflow_carries_into_ms() {
    let w = ManualClock::new(1_000); let mut c = Clock::new(w, Hlc::new(1_000, u16::MAX));
    assert_eq!(c.tick(), Hlc::new(1_001, 0));
}
#[test] fn observe_rejects_skew_and_does_not_advance() {
    let w = ManualClock::new(1_000_000); let mut c = Clock::new(w, Hlc::ZERO);
    let before = c.last();
    assert!(c.observe(Hlc::new(1_000_000 + MAX_SKEW_MS + 1, 0)).is_err());
    assert_eq!(c.last(), before);
    assert!(c.observe(Hlc::new(1_000_000 + MAX_SKEW_MS, 0)).is_ok());
}
#[test] fn observe_result_exceeds_remote_and_local() {
    let w = ManualClock::new(1_000); let mut c = Clock::new(w, Hlc::ZERO);
    let r = Hlc::new(1_200, 7); let got = c.observe(r).unwrap(); assert!(got > r);
}
#[test] fn floor_is_respected() {
    let w = ManualClock::new(10); let mut c = Clock::new(w, Hlc::new(5_000, 3));
    assert!(c.tick() > Hlc::new(5_000, 3));
}
```

- [ ] **Step 3: Run** `cargo test -p mesh-hlc`. Expected: compile errors (the types don't exist yet).
- [ ] **Step 4: Implement** per sub-spec §3. `observe`: skew check first; then `ms = max(wall, last.ms, remote.ms)`; counter = `max(last.c, remote.c) + 1` if `ms` equals both, `last.c + 1` if equal to last only, `remote.c + 1` if equal to remote only, otherwise `0`; overflow carries like `tick`.
- [ ] **Step 5: Run** `cargo test -p mesh-hlc` and `cargo clippy -p mesh-hlc -- -D warnings`. Expected: 6 passed, no warnings.
- [ ] **Step 6: Commit** `feat(hlc): hybrid logical clock`.

---

### Task 2: `mesh-proto`

**Files:**
- Create: `crates/mesh-proto/Cargo.toml`, `src/lib.rs`, `src/ids.rs`, `src/op.rs`, `src/sync.rs`, `src/ticket.rs`

**Interfaces:**
- Consumes: `mesh_hlc::Hlc`.
- Produces:
  - `MeshId(pub [u8; 32])`, `NodeId(pub [u8; 32])` (both `Copy, Eq, Ord, Hash, Debug, Serialize, Deserialize`; `NodeId::short() -> String` = first 10 hex chars), `NodeAddrBytes(pub Vec<u8>)`, `enum Role { Host, Client }`.
  - `enum Payload { Set { ns, key, order: Vec<u8>, value: Option<Vec<u8>> }, SetAdd { ns, set, member: Vec<u8> } }` with `fn ns(&self) -> &str`.
  - `struct OpBody { mesh, origin, seq: u64, hlc: Hlc, payload }`; `OpBody::validate(&self) -> Result<(), ProtoError>` (limits + no-NUL).
  - `struct SignedOp { body: OpBody, sig: [u8; 64] }`; `SignedOp::sign(body: OpBody, key: &ed25519_dalek::SigningKey) -> SignedOp`; `SignedOp::verify(&self) -> Result<(), ProtoError>` (origin bytes as verifying key).
  - `VersionVector(pub BTreeMap<NodeId, u64>)` with `get(&NodeId) -> u64` (0 if absent), `set(NodeId, u64)`.
  - `enum SyncMsg { Hello { mesh, vv }, Ops { ops: Vec<SignedOp> }, Covered { origin: NodeId, upto: u64 }, Done }`; `pub fn batch_ops(ops: Vec<SignedOp>) -> Vec<SyncMsg>` (≤ 256 ops and ≤ 1 MiB each).
  - `InviteTicket { mesh, inviter: NodeAddrBytes, token: [u8; 32], role: Role, expires_ms: u64 }`, `to_text(&self) -> String`, `InviteTicket::from_text(&str) -> Result<Self, ProtoError>`.
  - `pub fn encode<T: Serialize>(v: &T) -> Vec<u8>` (version byte + postcard), `pub fn decode<T: DeserializeOwned>(b: &[u8]) -> Result<T, ProtoError>`. `decode::<SignedOp>` also runs `validate()`.
  - `enum ProtoError { Version(u8), Decode(String), Limit(&'static str), Nul(&'static str), BadSig, Ticket(&'static str) }` (thiserror).

- [ ] **Step 1: Write failing tests** (unit tests in each module):

```rust
// op.rs
#[test] fn sign_verify_roundtrip() { let k = key(1); let op = SignedOp::sign(body(&k, 1), &k); assert!(op.verify().is_ok()); }
#[test] fn tampered_payload_fails_verify() { let k = key(1); let mut op = SignedOp::sign(body(&k, 1), &k);
    if let Payload::Set { value, .. } = &mut op.body.payload { *value = Some(b"x".to_vec()) }
    assert!(matches!(op.verify(), Err(ProtoError::BadSig))); }
#[test] fn signature_covers_mesh_id() { let k = key(1); let mut op = SignedOp::sign(body(&k, 1), &k); op.body.mesh = MeshId([9; 32]);
    assert!(matches!(op.verify(), Err(ProtoError::BadSig))); }
#[test] fn limits() { // exact boundaries
    assert!(set_op("n".repeat(64), "k", vec![0; 1_048_576]).validate().is_ok());
    assert!(matches!(set_op("n".repeat(65), "k", vec![]).validate(), Err(ProtoError::Limit(_))));
    assert!(matches!(set_op("n", "k".repeat(513), vec![]).validate(), Err(ProtoError::Limit(_))));
    assert!(matches!(set_op("n", "k", vec![0; 1_048_577]).validate(), Err(ProtoError::Limit(_))));
    assert!(matches!(set_op("n", "a\0b", vec![]).validate(), Err(ProtoError::Nul(_))));
}
// lib.rs
#[test] fn wrong_version_rejected() { let mut b = encode(&SyncMsg::Done); b[0] = 2; assert!(matches!(decode::<SyncMsg>(&b), Err(ProtoError::Version(2)))); }
#[test] fn every_syncmsg_roundtrips() { /* Hello, Ops(1 op), Covered, Done: decode(encode(m)) == m */ }
// sync.rs
#[test] fn batch_ops_respects_count_and_size() { /* 600 small ops -> 3 batches (256,256,88); 3 ops of 600 KiB -> 3 batches */ }
// ticket.rs
#[test] fn ticket_text_roundtrip_and_prefix() { let t = ticket(); let s = t.to_text(); assert!(s.starts_with("mesh1")); assert_eq!(InviteTicket::from_text(&s).unwrap(), t); }
#[test] fn ticket_crc_failure() { let mut s = ticket().to_text(); flip_last_char(&mut s); assert!(matches!(InviteTicket::from_text(&s), Err(ProtoError::Ticket(_)))); }
```

  (`key(n)` = `SigningKey::from_bytes(&[n; 32])`; `body(k, seq)` builds a `Set` op with `origin = NodeId(k.verifying_key().to_bytes())`, mesh `[7; 32]`. Add `PartialEq` derives as the tests need.)
- [ ] **Step 2: Run** `cargo test -p mesh-proto`. Expected: compile errors.
- [ ] **Step 3: Implement** the modules. Signing message = `b"meshnode/op/1"` followed by `postcard::to_allocvec(&body)`. Ticket base32 uses `data_encoding::BASE32_NOPAD` and lower-cases the output (and upper-cases before decoding).
- [ ] **Step 4: Run** `cargo test -p mesh-proto` and clippy. Expected: all pass.
- [ ] **Step 5: Commit** `feat(proto): ids, signed ops, sync messages, invite tickets`.

---

### Task 3: Store core — open, local writes, reads

**Files:**
- Create: `crates/mesh-store/Cargo.toml`, `src/lib.rs`, `src/error.rs`, `src/keys.rs`, `src/tables.rs`, `src/merge.rs`, `src/local.rs`, `src/read.rs`, `tests/common/mod.rs`, `tests/rules.rs`

**Interfaces:**
- Consumes: Task 1 `Clock`, `WallClock`, `Hlc`; Task 2 `SignedOp`, `OpBody`, `Payload`, `MeshId`, `NodeId`, `ProtoError`.
- Produces:
  - `pub struct Store<W: WallClock = SystemClock>`: `Send + Sync`, all methods `&self`.
  - `Store::open(path: &Path, mesh: MeshId, key: ed25519_dalek::SigningKey, wall: W) -> Result<Self, StoreError>`: creates the file if new, writing `meta.mesh`, `meta.node`, `next_seq = 1`, `last_hlc = 0`, `schema = 1`. Existing file with another mesh → `WrongMesh`; another node id → `Corrupt`.
  - `fn node_id(&self) -> NodeId`, `fn mesh(&self) -> MeshId`.
  - Local writes, each returning `Result<SignedOp, StoreError>`: `set(ns, key, value: &[u8])`, `delete(ns, key)`, `set_ordered(ns, key, order: &[u8], value: Option<&[u8]>)`, `set_add(ns, set, member: &[u8])`.
  - Reads: `get(ns, key) -> Result<Option<Vec<u8>>>`, `get_entry(ns, key) -> Result<Option<RegEntry>>`, `list_keys(ns, prefix, start_after: Option<&str>, limit: u32) -> Result<Vec<String>>`, `set_contains(ns, set, member) -> Result<bool>`, `set_members(ns, set, start_after: Option<&[u8]>, limit: u32) -> Result<Vec<Vec<u8>>>`, `namespace_sizes() -> Result<BTreeMap<String, u64>>` (sum of encoded value bytes of live winners and set members).
  - `pub struct RegEntry { pub order: Vec<u8>, pub hlc: Hlc, pub origin: NodeId, pub seq: u64, pub value: Option<Vec<u8>> }`.
  - `merge::beats(a: (&[u8], Hlc, NodeId), b: (&[u8], Hlc, NodeId)) -> bool`: lexicographic tuple comparison, true if a > b.
  - `enum StoreError { Io(String), Corrupt { table: &'static str, detail: String }, Full, WrongMesh, Proto(#[from] ProtoError) }`.
  - Test helper `common::store(dir, n: u8, clock: &ManualClock) -> Store<ManualClock>` (mesh `[7; 32]`, key `[n; 32]`).

**Key encoding (`keys.rs`).** All tables are `TableDefinition<&[u8], &[u8]>`:

| Table | Key bytes |
|---|---|
| `reg` | `ns ‖ 0x00 ‖ key ‖ 0x00 ‖ origin[32]` |
| `set` | `ns ‖ 0x00 ‖ set ‖ 0x00 ‖ member ‖ origin[32]` (origin = the last 32 bytes) |
| `ops`, `pending`, `superseded` | `origin[32] ‖ seq (u64 big-endian)` |
| `vv`, `removed` | `origin[32]` |
| `meta` | UTF-8 name |

Values are postcard structs in `tables.rs`:
- `RegRow { order, hlc, seq, value }`
- `SetRow { hlc, seq }`
- `OpRow { op: SignedOp, received_ms: u64, invalid: bool }`
- `PendingRow { op, reason: u8, first_seen_ms }`

- [ ] **Step 1: Write failing tests** in `tests/rules.rs`:

```rust
#[test] fn local_set_get_delete() { /* set("s","a",b"1") -> get == Some(b"1"); delete -> get == None; list_keys excludes "a" */ }
#[test] fn local_ops_have_increasing_seq_and_hlc() { /* three writes -> seq 1,2,3; hlc strictly increasing; each op.verify() ok */ }
#[test] fn ordered_write_beats_newer_plain_hlc_only_when_order_higher() {
    // same origin, key "k": set_ordered(order=[5], v="old") then set_ordered(order=[3], v="new") -> get == "old"
    // then set_ordered(order=[6], v="newest") -> get == "newest"
}
#[test] fn tombstone_wins_over_older_value() { /* set then delete -> get None, get_entry().value == None */ }
#[test] fn set_add_is_idempotent_and_members_sorted() { /* add b"b", b"a", b"a" -> set_members == [b"a", b"b"]; contains(b"a") */ }
#[test] fn list_keys_pages_with_start_after_even_if_deleted() { /* keys a,b,c,d; delete b; list(start_after=Some("b"), limit=2) == [c,d] */ }
#[test] fn list_keys_does_not_bleed_across_namespaces() { /* ns "a" key "x", ns "ab" key "y": list_keys("a","",..)==["x"]; list_keys("ab","x",..)==[] */ }
#[test] fn local_write_over_limit_is_rejected() {
    assert!(matches!(s.set("s", "k", &vec![0; 1_048_577]), Err(StoreError::Proto(_))));
    assert!(matches!(s.set("s", "a\0b", b"v"), Err(StoreError::Proto(_))));
    assert_eq!(s.version_vector_local_seq(), 0); // nothing was consumed (helper: next_seq - 1)
}
#[test] fn reopen_continues_seq_and_hlc() { /* write 2 ops, drop store, set ManualClock back 1 h, reopen, write -> seq 3, hlc > op2.hlc */ }
#[test] fn second_open_fails_cleanly() { /* open; open same path again -> Err(StoreError::Io(_)); first store still works */ }
#[test] fn wrong_mesh_on_open() { /* create with mesh A; reopen with mesh B -> Err(WrongMesh) */ }
```

  (`version_vector_local_seq` is a `#[doc(hidden)] pub fn` for tests; Task 5 replaces it with `version_vector()`.)
- [ ] **Step 2: Run** `cargo test -p mesh-store --test rules`. Expected: compile errors.
- [ ] **Step 3: Implement.**
  - The `Clock` lives in a `Mutex` inside `Store`, created with `floor = meta.last_hlc`.
  - A local write: one write transaction that reads `next_seq`, calls `tick()`, builds and validates the `OpBody` (validation runs before anything is consumed), signs it, merges, inserts `ops`, sets `vv[self] = seq`, and updates `next_seq` and `last_hlc`.
  - Merge per §5.4: read this origin's `reg` row. If the new op `beats` it, write it, and put the old op's `(origin, seq)` into `superseded` with `wall.now_ms()`. Otherwise put the new op itself in `superseded`.
  - The winner on read is the max over the key's `reg` rows, using `beats`.
  - Map a redb "database already open" error to `StoreError::Io`.
- [ ] **Step 4: Run** `cargo test -p mesh-store --test rules`. Expected: all pass.
- [ ] **Step 5: Commit** `feat(store): open, local writes, reads`.

---

### Task 4: Remote apply pipeline + change events

**Files:**
- Create: `crates/mesh-store/src/apply.rs`
- Modify: `src/lib.rs` (export `Acceptor`, `Verdict`, `ReadView`, `Outcome`, `Change`)
- Test: `tests/rules.rs`, `tests/common/mod.rs` (`AcceptAll`, `MembersOnly(HashSet<NodeId>)` test acceptors)

**Interfaces:**
- Consumes: Task 3 internals (`merge`, tables, `local` merge function shared).
- Produces:
  - `pub enum Verdict { Valid, UnknownOrigin }`. The removal check stays inside the store (`removed` table), so the acceptor only judges membership. This narrows sub-spec §5.2 deliberately; note it in the PR description.
  - `pub trait Acceptor: Send + Sync { fn judge(&self, op: &OpBody, view: &ReadView<'_>) -> Verdict; }`.
  - `pub struct ReadView<'a>`: `get(ns, key)` and `set_contains(ns, set, member)`, reading inside the current transaction.
  - `pub enum Outcome { Applied, Duplicate, Deferred(DeferReason), Ignored, Rejected(RejectReason) }`, with `DeferReason { Gap, UnknownOrigin, Skew }` and `RejectReason { BadSig, WrongMesh, Equivocation }`.
  - `fn apply(&self, op: SignedOp, acc: &dyn Acceptor) -> Result<Outcome, StoreError>`.
  - `fn retry_pending(&self, acc: &dyn Acceptor) -> Result<usize, StoreError>`: returns how many were applied. Drops entries whose `first_seen_ms` is more than 24 h old.
  - `fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Change>`, with `pub struct Change { pub ns: String, pub kind: ChangeKind, pub local: bool }` and `enum ChangeKind { Key(String), Set(String) }`. Channel capacity 1024, published **after** commit, and also for local writes from Task 3.

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn remote_op_applies_and_converges_with_local_reader() { /* A writes; B.apply(op, AcceptAll) == Applied; B.get == A.get */ }
#[test] fn own_op_echo_is_duplicate() { /* A writes op; A.apply(op.clone()) == Duplicate; seq/vv unchanged */ }
#[test] fn bad_signature_rejected_and_not_counted() { /* tamper; Rejected(BadSig); B vv for A == 0 */ }
#[test] fn equivocation_keeps_first() { /* two different ops with A's key, same seq 1: first Applied, second Rejected(Equivocation); get == first value */ }
#[test] fn gap_is_deferred_then_applied() { /* A writes 1,2,3; B applies 3 -> Deferred(Gap); 2 -> Deferred(Gap); 1 -> Applied and B now has all three; vv[A]==3 */ }
#[test] fn unknown_origin_deferred_until_member() { /* acc = MembersOnly({}) -> Deferred(UnknownOrigin); acc2 = MembersOnly({A}); retry_pending(acc2)==1; get works */ }
#[test] fn skew_deferred_then_applied_when_clock_catches_up() {
    // A's clock = B's clock + 300_001 ms; B.apply(op) == Deferred(Skew); advance B clock 2 ms; retry_pending == 1
}
#[test] fn pending_dropped_after_24h() { /* defer via Gap; advance clock 24h + 1ms; retry_pending -> 0 applied and pending empty */ }
#[test] fn change_events_after_commit() { /* subscribe; local set + remote apply -> two Change events, local flag true then false */ }
```

- [ ] **Step 2: Run** `cargo test -p mesh-store --test rules`. Expected: the new tests fail to compile.
- [ ] **Step 3: Implement `apply`** in exactly the order of §5.3:
  1. mesh check, then `verify`.
  2. Duplicate or equivocation, checked against `ops` and `pending`.
  3. `observe` (on skew, defer).
  4. If `removed[origin]` exists and `seq > cutoff`: store with `invalid = true`, advance the vv if contiguous, return `Ignored`.
  5. Call `judge`.
  6. Gap check against `vv[origin]`. Defer if there's a gap.
  7. Merge, insert into `ops`, set `vv[origin] = seq`, then loop: apply `pending[(origin, vv+1)]` while it's present and passes steps 3–5.

  `retry_pending` walks `pending` in key order, calling the same internal function per entry. Any op it fails to apply stays pending, with its original `first_seen_ms`.
- [ ] **Step 4: Run** `cargo test -p mesh-store`. Expected: all pass.
- [ ] **Step 5: Commit** `feat(store): remote apply, pending ops, change events`.

---

### Task 5: Catch-up support

**Files:**
- Create: `crates/mesh-store/src/sync.rs`
- Test: `tests/rules.rs`

**Interfaces:**
- Consumes: Task 4 `apply`.
- Produces:
  - `fn version_vector(&self) -> Result<VersionVector, StoreError>` (includes self).
  - `pub enum SyncItem { Op(SignedOp), Covered { origin: NodeId, upto: u64 } }`.
  - `fn diff_since(&self, theirs: &VersionVector, limit: usize) -> Result<Vec<SyncItem>, StoreError>`.
  - `fn apply_covered(&self, origin: NodeId, upto: u64, acc: &dyn Acceptor) -> Result<(), StoreError>`.
  - `fn mark_full_sync(&self) -> Result<(), StoreError>` (stores `wall.now_ms()`), `fn last_full_sync_ms(&self) -> Result<Option<u64>, StoreError>`.
  - Remove `version_vector_local_seq` and update Task 3's test to `version_vector()?.get(&self_id)`.

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn diff_since_returns_missing_ops_in_origin_seq_order() { /* A 3 ops, C 2 ops applied at B; diff_since(vv{A:1}) -> A2,A3,C1,C2 */ }
#[test] fn diff_since_respects_limit() { /* limit 2 -> exactly 2 items */ }
#[test] fn covered_marker_lets_peer_advance() {
    // At B: A's ops 1..=5 exist except 2 and 3 (delete those rows via a #[doc(hidden)] test hook simulating compaction).
    // diff_since(empty) for A == [Op(1), Covered{A, upto:3}, Op(4), Op(5)]
    // A fresh store C fed that list in order (apply / apply_covered) ends with vv[A] == 5 and the same reads as B.
}
#[test] fn pending_ops_are_not_served() { /* deferred op at B is absent from B.diff_since(empty) */ }
```

- [ ] **Step 2: Run.** Expected: compile errors.
- [ ] **Step 3: Implement.** `diff_since`: for each origin in our vv, range-scan `ops` from `(origin, theirs+1)` up to `(origin, vv[origin])`; any seq hole becomes one `Covered { upto: last missing seq in the run }`. `apply_covered`: if `upto > vv[origin]`, set `vv[origin] = upto` and drain `pending` for that origin as in Task 4 step 7.
- [ ] **Step 4: Run** `cargo test -p mesh-store`. Expected: all pass.
- [ ] **Step 5: Commit** `feat(store): version vectors and catch-up diff`.

---

### Task 6: Maintenance — compaction, tombstone GC, staleness, purge

**Files:**
- Create: `crates/mesh-store/src/maint.rs`
- Test: `tests/rules.rs`

**Interfaces:**
- Consumes: Tasks 3–5.
- Produces:
  - `pub const SETTLE_MS: u64 = 7 * 86_400_000; pub const TOMBSTONE_TTL_MS: u64 = 90 * 86_400_000; pub const STALE_MS: u64 = 80 * 86_400_000;`
  - `fn compact(&self) -> Result<usize, StoreError>`: deletes `ops` rows whose `superseded` entry, or whose `invalid` row, is older than `SETTLE_MS`. Removes the matching `superseded` entry. Never touches `SetAdd` ops.
  - `fn gc(&self) -> Result<usize, StoreError>`: for each key whose **winner** is a tombstone with `hlc.ms() < now - TOMBSTONE_TTL_MS`, deletes **all** of that key's `reg` rows (every origin) and their ops. Deleting only the tombstone row would bring back an older value from another origin.
  - `fn is_stale(&self) -> Result<bool, StoreError>`: `last_full_sync_ms` is `Some(t)` with `t < now - STALE_MS`. A store that has never synced is not stale.
  - `fn export_unsynced_local_ops(&self) -> Result<Vec<SignedOp>, StoreError>`: own ops with `received_ms > last_full_sync_ms`. `fn reset_to_empty(&self) -> Result<(), StoreError>` clears every table except `meta`, which keeps `next_seq` and `last_hlc` so seqs are never reused.
  - `fn purge_namespace(&self, ns: &str) -> Result<Vec<SignedOp>, StoreError>`: a `delete` for every live key, plus nothing for sets, which are grow-only. Document that set members survive a purge until the namespace is tombstoned in `_mesh` (sub-spec 3).

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn compaction_waits_for_settle_period() {
    // set k=1 then k=2 (op1 superseded). advance 7d - 1ms: compact()==0, op1 still in diff_since.
    // advance 2ms: compact()==1; diff_since(empty) == [Covered{A,1}, Op(2)]; get == 2
}
#[test] fn compaction_never_removes_set_add_ops() { /* set_add twice same member; advance 8d; compact()==0 */ }
#[test] fn tombstone_gc_after_90_days() {
    // A: set k=v (hlc t0). B (other origin, applied at A) older set k=w. A: delete k. advance 90d+1ms: gc()==1;
    // get(k)==None AND no reg rows remain (B's older "w" must NOT reappear); list_keys excludes k
}
#[test] fn stale_after_80_days() { /* mark_full_sync; advance 80d: false; +1ms: true; never-synced store: false */ }
#[test] fn reset_keeps_seq() { /* write 3 ops; reset_to_empty; next write has seq 4; reads empty */ }
#[test] fn purge_namespace_tombstones_live_keys() { /* 3 keys; purge -> 3 ops returned; list_keys empty; other ns untouched */ }
```

- [ ] **Step 2: Run.** Expected: compile errors.
- [ ] **Step 3: Implement** per the Interfaces above (all clock reads go through `wall.now_ms()`).
- [ ] **Step 4: Run** `cargo test -p mesh-store`. Expected: all pass.
- [ ] **Step 5: Commit** `feat(store): compaction, tombstone GC, staleness, purge`.

---

### Task 7: Removal retraction

**Files:**
- Create: `crates/mesh-store/src/removal.rs`
- Test: `tests/rules.rs`

**Interfaces:**
- Consumes: Tasks 3–6.
- Produces: `fn on_removal(&self, origin: NodeId, cutoff_seq: u64) -> Result<RemovalReport, StoreError>`. It stores `removed[origin] = max(existing, cutoff_seq)` and works out the effective cutoff:
  - If it **dropped** (a first removal), retract per §5.6 steps 1–4.
  - If it **rose**, re-merge the now-valid ops in `(old, new]`.
  - If it's unchanged, do nothing.

  `pub struct RemovalReport { pub retracted_ops: usize, pub restored_keys: usize, pub unrestorable_keys: Vec<(String, String)> }`. Each unrestorable key also logs `tracing::warn!` (add `tracing` as a dependency).

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn removal_retracts_and_restores_older_value() {
    // X writes k=1 (seq1), k=2 (seq2); B applies both; B.on_removal(X, 1) -> get(k)==1; restored_keys==1
}
#[test] fn ops_above_cutoff_arriving_later_are_ignored_but_covered() {
    // B.on_removal(X,1) first; then apply X seq1 -> Applied, seq2 -> Ignored; vv[X]==2; get(k)==1
}
#[test] fn removal_before_or_after_ops_gives_same_result() { /* stores B and C, opposite order of removal vs ops -> equal get/list_keys/set_members */ }
#[test] fn max_cutoff_wins_across_removals() { /* on_removal(X,1) then on_removal(X,2) -> k==2 again (re-merged); then on_removal(X,1) -> no change */ }
#[test] fn set_adds_above_cutoff_are_retracted() { /* X set_add m (seq2); removal cutoff 1 -> set_contains false unless another origin also added m */ }
#[test] fn removal_after_settle_cannot_restore_and_warns() {
    // X: k=1, k=2 at B; advance 7d+1ms; compact (op1 gone); on_removal(X,1) -> get(k)==None; unrestorable_keys==[("s","k")]
}
```

- [ ] **Step 2: Run.** Expected: compile errors.
- [ ] **Step 3: Implement.** Retraction scans `ops` for `origin` with `seq > cutoff` (mark invalid) and collects the affected `(ns, key)` and `(ns, set, member)` pairs. For each register key, the best remaining op of that origin with `seq ≤ cutoff` that isn't invalid becomes the row, and it's removed from `superseded`. If there's none, delete the row: if this origin's rows for the key ever had an op with `seq ≤ cutoff` that is now compacted, report the key as unrestorable; otherwise it's a plain delete. Re-merge on a raised cutoff: clear `invalid` on ops in `(old, new]` and run the normal merge for each.
- [ ] **Step 4: Run** `cargo test -p mesh-store`. Expected: all pass.
- [ ] **Step 5: Commit** `feat(store): seq-cutoff removal retraction`.

---

### Task 8: Property tests

**Files:**
- Create: `crates/mesh-store/tests/props.rs`

**Interfaces:**
- Consumes: the whole public `Store` API; `common` helpers.

- [ ] **Step 1: Write the three properties** (`proptest!` with `ProptestConfig { cases: 10_000, ..}` taken from env `PROPTEST_CASES`, default 256 locally; CI sets 10 000):
  - **`convergence`:**
    - Strategy: N ∈ 2..=5 writer stores and a script of ≤ 40 local actions (writer index, `set`/`delete`/`set_ordered(order ∈ 0..4)`/`set_add`, key ∈ 4 keys, ns ∈ 2 namespaces).
    - Run the script on the writers to collect all ops.
    - Then for M ∈ 2..=4 fresh reader stores, each gets a random permutation with ≤ 20 % duplicates, interleaved with random partial `diff_since` exchanges with another reader.
    - Finally, do a full pairwise exchange until vvs are equal.
    - Assert: every reader's `list_keys` + `get` + `set_members` over all ns/keys/sets equal the writers' union view (build it from a reader that applied everything in order).
  - **`compaction_invisible`:** same setup with one reader. At random points, advance its clock by `SETTLE_MS + 1` and call `compact()`. Final reads equal a reader that never compacted.
  - **`retraction_deterministic`:** one origin is the "removed" X with cutoff c chosen among its seqs. Two readers get the same ops, but `on_removal(X, c)` is placed at a random position in each delivery sequence (no clock advance, i.e. within SETTLE). Assert equal reads.
- [ ] **Step 2: Run** `cargo test -p mesh-store --test props --release`. Expected: PASS. If a property fails, **the shrunk counterexample is a bug in Tasks 3–7**: add it as a named rule test in `rules.rs`, fix the code, and re-run both.
- [ ] **Step 3: Run with CI depth:** `PROPTEST_CASES=10000 cargo test -p mesh-store --test props --release`. Expected: PASS. Record the wall time in the commit message.
- [ ] **Step 4: Commit** `test(store): convergence, compaction and retraction properties`.

---

### Task 9: Crash safety, CI, Android check

**Files:**
- Create: `crates/mesh-store/tests/crash.rs`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the public `Store` API.

- [ ] **Step 1: Write the crash test.**
  - `crash_mid_write_leaves_consistent_store`: when env `MESH_CRASH_CHILD=<dir>` is set, the test body runs as the child. It opens the store and loops `set("s", format!("k{i}"), &[0u8; 4096])` forever, printing `i` after each commit.
  - Otherwise, as the parent: spawn `std::env::current_exe()` with args `--exact crash_mid_write_leaves_consistent_store --nocapture` and the env var set. Read stdout until it has printed ≥ 200, then `child.kill()`. Repeat 5 times on the same dir.
  - After each kill, reopen and assert:
    - every key `k0..=k{last printed}` exists
    - `version_vector()[self] == next written seq - 1`
    - every `ops` row's key decodes and its `seq ≤ vv`
- [ ] **Step 2: Run** `cargo test -p mesh-store --test crash -- --nocapture`. Expected: PASS on Windows (and later on Linux in CI).
- [ ] **Step 3: Add CI** `.github/workflows/ci.yml`:
  - Matrix `ubuntu-latest` and `windows-latest`, steps `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `PROPTEST_CASES=10000 cargo test -p mesh-store --test props --release`.
  - A separate `android` job on ubuntu: `rustup target add aarch64-linux-android`, `cargo install cargo-ndk --locked`, `cargo ndk -t arm64-v8a build -p mesh-store`.
- [ ] **Step 4: Verify locally:**
  - `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` all pass.
  - `ANDROID_NDK_HOME="$LOCALAPPDATA/Android/Sdk/ndk/27.2.12479018" cargo ndk -t arm64-v8a build -p mesh-store` prints `Finished`.
- [ ] **Step 5: Bench (informational).** Add an `#[ignore]` test `bench_apply_100k` in `rules.rs` that applies 100 000 remote ops and times 10 000 `get`s. Run it once with `cargo test -p mesh-store --release -- --ignored bench_apply_100k --nocapture` and paste the numbers into the commit message.
- [ ] **Step 6: Commit** `test(store): crash safety; ci: workspace + android build`.

---

## Self-review notes

- **Coverage:** §3 → Task 1; §4 → Task 2; §5.1, §5.4, §5.9, §5.10 → Task 3; §5.2, §5.3 → Task 4; §5.8 → Task 5; §5.5, §5.7 → Task 6; §5.6 → Task 7; §6 property and crash tests → Tasks 8 and 9; §1 criterion 4 → Task 9.
- **Deliberate narrowing:** the `Acceptor` judges only membership; the removal cutoff check lives in the store, next to the retraction it must agree with (Task 4).
- **Added constraint:** no `0x00` byte in `ns`/`key`/`set` (key encoding), enforced in proto validation and tested in Tasks 2 and 3.
