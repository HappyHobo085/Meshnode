# Sub-spec 1 handoff: open items for sub-spec 2 (node core / sync sessions)

Sub-spec 1 (`mesh-hlc`, `mesh-proto`, `mesh-store`) is done on branch `feat/store-hlc-proto`. The items below were found during its reviews and deliberately left to the next layer. **Sub-spec 2 must address the "must" items.**

## Must

1. **Old offline edits can resurrect deleted keys (R19).**
   - A node that was stale for more than 80 days re-bootstraps. `export_unsynced_local_ops` re-applies its own old `Set` ops.
   - Suppose one of those ops has an HLC older than a tombstone that other replicas already garbage-collected (90 days, aged by both HLC and arrival).
   - Then that op resurrects the key on some replicas, and the replicas stay permanently different.
   - Fix: have the export leave out own `Set` ops with `hlc.ms < now - TOMBSTONE_TTL_MS`, and return them separately so the UI can show them as "edits made offline too long ago". Add this to spec §5.7.

2. **Re-bootstrap ordering (R10, R20).** The order is:
   1. Read `synced_self_seq()` **before** `reset_to_empty()`, which deletes it.
   2. Run the full catch-up.
   3. Apply the exported `Covered` items only once the catch-up has `vv[self] >= synced_self_seq`.
   4. Then re-apply the exported ops.

   Local writes are refused with `StoreError::Rebootstrapping` until then (R18).

3. **`mark_full_sync(synced_self_seq)` (R15).** mesh-core must call it after every completed two-way exchange, including the join catch-up. Pass the peer's confirmed `vv[self]` as the argument. A store that has never synced is never stale and never garbage-collects its own keys, which also applies to a single-node mesh.

4. **`apply_covered(origin, upto, advertised, acc)` (R16).** `advertised` must be the sender's Hello `vv[origin]`, and the session must not pass markers above it. Members are trusted to this extent; a lying member can still freeze an origin up to its own advertised vv.

5. **Removal of this node itself.** If this node's own id is removed, the store keeps accepting local writes. mesh-core must stop the node and offer "reset identity".

6. **Store calls block and fsync.** On tokio, call them from `spawn_blocking`. For catch-up, use `apply_batch` (one transaction per `SyncMsg::Ops`): about 12,900 ops/s against 875 ops/s for one at a time on the dev machine.

7. **`Acceptor::judge` runs inside the write transaction with the clock lock held.** It must read only through `ReadView` (`get`, `set_contains`, `set_members`, `list_keys`) and must never call `Store`.

8. **Change events are lossy** (a broadcast channel). On `Lagged`, rescan: `on_removal` and `retry_pending` are both idempotent.

## Should

- Receive-side limits: reject `SyncMsg::Ops` batches over 256 ops or 1 MiB.
- Own ops can be deferred for clock skew after a reset if the wall clock was set back. With R18, that blocks local writes until the clock catches up. Consider exempting own-origin ops with `hlc <= clock.last()` from the skew check.
- `clock_floor_ahead_ms()` returns `Some` when the persisted clock floor is far ahead of the wall clock. Show it in the UI as a warning.
- R3: `RemovalReport.unrestorable_keys` over-reports. Log it; don't show it to users as data loss.
- Known performance costs:
  - a first removal scans the whole `reg` and `set` tables;
  - `gc` scans `superseded` once per run;
  - `set_members` paging costs O(remaining rows).

  All of these are acceptable at 2–10 nodes.

## Minor (deferred, low priority)

- A corrupt `meta.last_hlc` (`ms ≥ 2^48 − 1`) can panic on `tick`; reject it as `Corrupt` on open.
- `Corrupt` errors from value decodes leave out the key.
- `merge_op` keeps the first-arriving `SetAdd` row's seq and hlc (metadata only).
- Crash test: values are all zeros, and an early child exit is reported as a timeout.
- Missing tests: resuming `diff_since` after a limit cut, a two-replica cutoff-raise, a ticket CRC failure in the middle of the text, and nested `SyncMsg(Ops(invalid))` decoding.
