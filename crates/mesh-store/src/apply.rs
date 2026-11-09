//! Remote apply pipeline (§5.3), held-back (`pending`) ops, and change events.
//!
//! Every remote op goes through the steps of §5.3 inside one write transaction, in this
//! order: mesh check and signature → duplicate / equivocation (against `ops` **and**
//! `pending`) → `Clock::observe` (skew defers) → removal cutoff (`removed` table) →
//! `Acceptor::judge` → gap check against `vv[origin]` → merge, record, advance the vv,
//! and drain this origin's now-unblocked `pending` ops (each re-running observe, the
//! removal check and judge).

use crate::error::StoreError;
use crate::tables::{self, META, OPS, OpRow, PENDING, PendingRow, REG, REMOVED, SET, VV, meta};
use crate::{Store, keys, merge, read};
use mesh_hlc::{Clock, WallClock};
use mesh_proto::{MeshId, NodeId, OpBody, Payload, SignedOp};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, WriteTransaction};

/// Pending ops older than this (by `first_seen_ms`) are dropped by `retry_pending`.
pub const PENDING_TTL_MS: u64 = 86_400_000;

/// Capacity of the change-event broadcast channel.
pub(crate) const EVENT_CAPACITY: usize = 1024;

/// The acceptor's judgement of a remote op's origin.
///
/// Narrower than sub-spec §5.2: there is no `Invalid`, because the removal cutoff is
/// checked inside the store (the `removed` table), so the acceptor judges membership only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    /// The origin is not a member (yet); the op is held in `pending`.
    UnknownOrigin,
}

/// Decides whether a remote op's origin is acceptable (implemented by `mesh-core`).
pub trait Acceptor: Send + Sync {
    fn judge(&self, op: &OpBody, view: &ReadView<'_>) -> Verdict;
}

/// Read access to the store from inside the apply transaction (sees earlier writes of
/// the same transaction, e.g. a membership op drained just before).
pub struct ReadView<'a> {
    tx: &'a WriteTransaction,
}

impl ReadView<'_> {
    /// The live value of `ns/key`, or `None` if absent or deleted.
    pub fn get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let reg = self.tx.open_table(REG)?;
        Ok(read::reg_entry_in(&reg, ns, key)?.and_then(|e| e.value))
    }

    /// True if `member` is in the add-only set `ns/set`.
    pub fn set_contains(&self, ns: &str, set: &str, member: &[u8]) -> Result<bool, StoreError> {
        let table = self.tx.open_table(SET)?;
        read::set_contains_in(&table, ns, set, member)
    }
}

/// Result of [`Store::apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Merged, recorded and counted in the version vector.
    Applied,
    /// Already have it (identical op stored or pending, or its seq is already covered).
    Duplicate,
    /// Held in `pending`; `retry_pending` (or a later op filling the gap) applies it.
    Deferred(DeferReason),
    /// Above its removed origin's cutoff: stored marked invalid (covered), not merged.
    Ignored,
    /// Not stored and not counted.
    Rejected(RejectReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferReason {
    /// A lower seq of the same origin is missing.
    Gap,
    /// The acceptor does not know the origin yet.
    UnknownOrigin,
    /// The op's HLC is more than `MAX_SKEW_MS` ahead of our wall clock.
    Skew,
}

impl DeferReason {
    /// `PendingRow::reason` encoding.
    pub(crate) fn code(self) -> u8 {
        match self {
            DeferReason::Gap => 0,
            DeferReason::UnknownOrigin => 1,
            DeferReason::Skew => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// The signature does not verify.
    BadSig,
    /// The op belongs to another mesh.
    WrongMesh,
    /// A different op with the same `(origin, seq)` is already stored or pending.
    Equivocation,
}

/// A change to the store, published after the transaction that made it commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub ns: String,
    pub kind: ChangeKind,
    /// True for this node's own writes, false for applied remote ops.
    pub local: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// A register key.
    Key(String),
    /// An add-only set.
    Set(String),
}

/// The change event for a merged op.
pub(crate) fn change_for(op: &OpBody, local: bool) -> Change {
    match &op.payload {
        Payload::Set { ns, key, .. } => Change {
            ns: ns.clone(),
            kind: ChangeKind::Key(key.clone()),
            local,
        },
        Payload::SetAdd { ns, set, .. } => Change {
            ns: ns.clone(),
            kind: ChangeKind::Set(set.clone()),
            local,
        },
    }
}

/// `vv[origin]` (0 if absent).
pub(crate) fn vv_get(tx: &WriteTransaction, origin: &NodeId) -> Result<u64, StoreError> {
    let vv = tx.open_table(VV)?;
    match vv.get(keys::origin_key(origin).as_slice())? {
        Some(g) => keys::read_u64_value("vv", g.value()),
        None => Ok(0),
    }
}

pub(crate) fn vv_set(tx: &WriteTransaction, origin: &NodeId, seq: u64) -> Result<(), StoreError> {
    let mut vv = tx.open_table(VV)?;
    vv.insert(
        keys::origin_key(origin).as_slice(),
        keys::u64_value(seq).as_slice(),
    )?;
    Ok(())
}

/// `removed[origin]`: the origin's effective removal cutoff seq, if it was removed.
pub(crate) fn removed_get(
    tx: &WriteTransaction,
    origin: &NodeId,
) -> Result<Option<u64>, StoreError> {
    let removed = tx.open_table(REMOVED)?;
    match removed.get(keys::origin_key(origin).as_slice())? {
        Some(g) => keys::read_u64_value("removed", g.value()).map(Some),
        None => Ok(None),
    }
}

/// Inserts `op` into `ops`.
pub(crate) fn insert_op(
    tx: &WriteTransaction,
    op: &SignedOp,
    received_ms: u64,
    invalid: bool,
) -> Result<(), StoreError> {
    let row = OpRow {
        op: op.clone(),
        received_ms,
        invalid,
    };
    let mut ops = tx.open_table(OPS)?;
    ops.insert(
        keys::op_key(&op.body.origin, op.body.seq).as_slice(),
        tables::encode("ops", &row)?.as_slice(),
    )?;
    Ok(())
}

/// The apply pipeline over one write transaction. Collects the `Change`s to publish
/// after commit and counts applied ops.
///
/// Callers hold the store's clock lock for the whole transaction and must not keep any
/// table open while calling into it.
pub(crate) struct Pipeline<'a, W: WallClock> {
    tx: &'a WriteTransaction,
    clock: &'a mut Clock<W>,
    acc: &'a dyn Acceptor,
    mesh: MeshId,
    /// Wall time at the start of the transaction (`received_ms`, `first_seen_ms`, merge time).
    now_ms: u64,
    pub(crate) changes: Vec<Change>,
    pub(crate) applied: usize,
}

impl<'a, W: WallClock> Pipeline<'a, W> {
    pub(crate) fn new(
        tx: &'a WriteTransaction,
        clock: &'a mut Clock<W>,
        acc: &'a dyn Acceptor,
        mesh: MeshId,
    ) -> Self {
        let now_ms = clock.wall().now_ms();
        Pipeline {
            tx,
            clock,
            acc,
            mesh,
            now_ms,
            changes: Vec::new(),
            applied: 0,
        }
    }

    /// All of §5.3 for a newly received op.
    pub(crate) fn admit(&mut self, op: SignedOp) -> Result<Outcome, StoreError> {
        // 1. Mesh, then signature. Ops decoded from the wire are already validated; a
        //    hand-built op with bad fields (e.g. a NUL in a key) is a caller error.
        if op.body.mesh != self.mesh {
            return Ok(Outcome::Rejected(RejectReason::WrongMesh));
        }
        if op.verify().is_err() {
            return Ok(Outcome::Rejected(RejectReason::BadSig));
        }
        op.body.validate()?;

        // 2. Duplicate or equivocation, against `ops` and `pending`.
        let (origin, seq) = (op.body.origin, op.body.seq);
        if let Some(first) = self.stored_or_pending(&origin, seq)? {
            if first == op {
                return Ok(Outcome::Duplicate);
            }
            tracing::error!(
                origin = %origin.short(),
                seq,
                "equivocation: a different op with this (origin, seq) is already held; keeping the first"
            );
            return Ok(Outcome::Rejected(RejectReason::Equivocation));
        }

        // 3–7.
        let out = self.step(op, self.now_ms)?;
        if matches!(out, Outcome::Applied | Outcome::Ignored) {
            self.drain(origin)?;
        }
        Ok(out)
    }

    /// The op held in `ops` or `pending` under `(origin, seq)`, if any.
    fn stored_or_pending(&self, origin: &NodeId, seq: u64) -> Result<Option<SignedOp>, StoreError> {
        let k = keys::op_key(origin, seq);
        {
            let ops = self.tx.open_table(OPS)?;
            if let Some(g) = ops.get(k.as_slice())? {
                return Ok(Some(tables::decode::<OpRow>("ops", g.value())?.op));
            }
        }
        let pending = self.tx.open_table(PENDING)?;
        match pending.get(k.as_slice())? {
            Some(g) => Ok(Some(tables::decode::<PendingRow>("pending", g.value())?.op)),
            None => Ok(None),
        }
    }

    /// Steps 3–7 for an op that is in neither `ops` nor `pending` (does not drain).
    /// A deferral stores the op in `pending` with `first_seen_ms`.
    fn step(&mut self, op: SignedOp, first_seen_ms: u64) -> Result<Outcome, StoreError> {
        let (origin, seq) = (op.body.origin, op.body.seq);
        let vv = vv_get(self.tx, &origin)?;
        // Already covered (seq 0, compacted or reported `Covered`): nothing to merge.
        if seq <= vv {
            return Ok(Outcome::Duplicate);
        }

        // 3. Observe; persist the clock so a reopen never goes backwards.
        if let Err(skew) = self.clock.observe(op.body.hlc) {
            tracing::debug!(origin = %origin.short(), seq, "deferring op: {skew}");
            return self.defer(op, DeferReason::Skew, first_seen_ms);
        }
        {
            let mut m = self.tx.open_table(META)?;
            m.insert(
                meta::LAST_HLC.as_bytes(),
                keys::u64_value(self.clock.last().as_u64()).as_slice(),
            )?;
        }

        // 4. Above a removed origin's cutoff: keep as invalid (covered), don't merge.
        if removed_get(self.tx, &origin)?.is_some_and(|cutoff| seq > cutoff) {
            insert_op(self.tx, &op, self.now_ms, true)?;
            if seq == vv.saturating_add(1) {
                vv_set(self.tx, &origin, seq)?;
            }
            return Ok(Outcome::Ignored);
        }

        // 5. Judge.
        let verdict = self.acc.judge(&op.body, &ReadView { tx: self.tx });
        if verdict == Verdict::UnknownOrigin {
            return self.defer(op, DeferReason::UnknownOrigin, first_seen_ms);
        }

        // 6. Gap.
        if seq > vv.saturating_add(1) {
            return self.defer(op, DeferReason::Gap, first_seen_ms);
        }

        // 7. Merge, record, advance the vv.
        let changed = merge::merge_op(self.tx, &op, self.now_ms)?;
        insert_op(self.tx, &op, self.now_ms, false)?;
        vv_set(self.tx, &origin, seq)?;
        if changed {
            self.changes.push(change_for(&op.body, false));
        }
        self.applied += 1;
        Ok(Outcome::Applied)
    }

    fn defer(
        &mut self,
        op: SignedOp,
        reason: DeferReason,
        first_seen_ms: u64,
    ) -> Result<Outcome, StoreError> {
        let k = keys::op_key(&op.body.origin, op.body.seq);
        let row = PendingRow {
            op,
            reason: reason.code(),
            first_seen_ms,
        };
        let mut pending = self.tx.open_table(PENDING)?;
        pending.insert(k.as_slice(), tables::encode("pending", &row)?.as_slice())?;
        Ok(Outcome::Deferred(reason))
    }

    /// Step 7's loop for `origin`: drops `pending` entries already covered by `vv[origin]`,
    /// then repeatedly advances the vv over an op already stored at `vv + 1` (an invalid
    /// op stored ahead of a gap) or takes `pending[(origin, vv + 1)]` and re-runs steps
    /// 3–7 on it, keeping its `first_seen_ms`. Stops at the first missing or re-deferred op.
    pub(crate) fn drain(&mut self, origin: NodeId) -> Result<(), StoreError> {
        let mut vv = vv_get(self.tx, &origin)?;
        self.drop_covered_pending(&origin, vv)?;
        while let Some(next) = vv.checked_add(1) {
            let k = keys::op_key(&origin, next);
            let stored = self.tx.open_table(OPS)?.get(k.as_slice())?.is_some();
            if stored {
                vv_set(self.tx, &origin, next)?;
                vv = next;
                continue;
            }
            let row = {
                let mut pending = self.tx.open_table(PENDING)?;
                let removed = pending.remove(k.as_slice())?;
                match removed {
                    Some(g) => tables::decode::<PendingRow>("pending", g.value())?,
                    None => break,
                }
            };
            match self.step(row.op, row.first_seen_ms)? {
                Outcome::Applied | Outcome::Ignored => vv = vv_get(self.tx, &origin)?,
                _ => break,
            }
        }
        Ok(())
    }

    /// Removes `pending[(origin, 1..=vv)]`: those seqs are already covered.
    fn drop_covered_pending(&mut self, origin: &NodeId, vv: u64) -> Result<(), StoreError> {
        if vv == 0 {
            return Ok(());
        }
        let (lo, hi) = (keys::op_key(origin, 0), keys::op_key(origin, vv));
        let mut pending = self.tx.open_table(PENDING)?;
        let covered: Vec<Vec<u8>> = pending
            .range::<&[u8]>(lo.as_slice()..=hi.as_slice())?
            .map(|item| item.map(|(k, _)| k.value().to_vec()))
            .collect::<Result<_, _>>()?;
        for k in covered {
            pending.remove(k.as_slice())?;
        }
        Ok(())
    }

    /// `retry_pending`: walks `pending` in key order. Entries older than
    /// [`PENDING_TTL_MS`] are dropped (logged); the rest are re-run through steps 3–7
    /// (and drained on success). Entries already drained by an earlier one are skipped.
    pub(crate) fn retry_all(&mut self) -> Result<(), StoreError> {
        let all: Vec<Vec<u8>> = {
            let pending = self.tx.open_table(PENDING)?;
            pending
                .iter()?
                .map(|item| item.map(|(k, _)| k.value().to_vec()))
                .collect::<Result<_, _>>()?
        };
        for k in all {
            let row = {
                let mut pending = self.tx.open_table(PENDING)?;
                let removed = pending.remove(k.as_slice())?;
                match removed {
                    Some(g) => tables::decode::<PendingRow>("pending", g.value())?,
                    None => continue,
                }
            };
            let (origin, seq) = (row.op.body.origin, row.op.body.seq);
            let age_ms = self.now_ms.saturating_sub(row.first_seen_ms);
            if age_ms > PENDING_TTL_MS {
                tracing::warn!(
                    origin = %origin.short(),
                    seq,
                    reason = row.reason,
                    age_ms,
                    "dropping pending op held for more than 24 h"
                );
                continue;
            }
            let out = self.step(row.op, row.first_seen_ms)?;
            if matches!(out, Outcome::Applied | Outcome::Ignored) {
                self.drain(origin)?;
            }
        }
        Ok(())
    }
}

impl<W: WallClock> Store<W> {
    /// Applies a remote op (§5.3) in one write transaction and publishes its changes
    /// after commit. See [`Outcome`] for the possible results.
    ///
    /// Errors: `Proto` if a (hand-built) op fails `OpBody::validate`; `Io`/`Full`/`Corrupt`
    /// from the database.
    pub fn apply(&self, op: SignedOp, acc: &dyn Acceptor) -> Result<Outcome, StoreError> {
        let mut clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        let (out, changes) = {
            let mut p = Pipeline::new(&tx, &mut clock, acc, self.mesh);
            let out = p.admit(op)?;
            (out, p.changes)
        };
        if matches!(out, Outcome::Duplicate | Outcome::Rejected(_)) && changes.is_empty() {
            // Nothing was written; skip the commit's fsync.
            tx.abort()?;
        } else {
            tx.commit()?;
        }
        self.publish(changes);
        Ok(out)
    }

    /// Retries every `pending` op in key order (after a membership change, and about once
    /// a minute so resolved skew is picked up). Returns how many ops were applied,
    /// including ones unblocked behind them. Entries that still can't be applied stay with
    /// their original `first_seen_ms`; entries first seen more than 24 h ago are dropped.
    pub fn retry_pending(&self, acc: &dyn Acceptor) -> Result<usize, StoreError> {
        let mut clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        let (applied, changes) = {
            let mut p = Pipeline::new(&tx, &mut clock, acc, self.mesh);
            p.retry_all()?;
            (p.applied, p.changes)
        };
        tx.commit()?;
        self.publish(changes);
        Ok(applied)
    }

    /// Subscribes to change events (capacity 1024; a slow receiver sees `Lagged`).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Change> {
        self.events.subscribe()
    }

    /// Publishes committed changes. Having no subscriber is not an error.
    pub(crate) fn publish(&self, changes: Vec<Change>) {
        for c in changes {
            let _ = self.events.send(c);
        }
    }

    /// `vv[origin]` (0 if absent). Test helper until `version_vector()` exists.
    #[doc(hidden)]
    pub fn vv_of(&self, origin: NodeId) -> Result<u64, StoreError> {
        let tx = self.db.begin_read()?;
        let vv = tx.open_table(VV)?;
        match vv.get(keys::origin_key(&origin).as_slice())? {
            Some(g) => keys::read_u64_value("vv", g.value()),
            None => Ok(0),
        }
    }

    /// Number of `pending` entries. Test helper.
    #[doc(hidden)]
    pub fn pending_len(&self) -> Result<u64, StoreError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(PENDING)?.len()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use mesh_hlc::{Hlc, ManualClock};

    struct All;
    impl Acceptor for All {
        fn judge(&self, _: &OpBody, _: &ReadView<'_>) -> Verdict {
            Verdict::Valid
        }
    }

    fn op(key: &SigningKey, seq: u64, k: &str) -> SignedOp {
        SignedOp::sign(
            OpBody {
                mesh: MeshId([7; 32]),
                origin: NodeId(key.verifying_key().to_bytes()),
                seq,
                hlc: Hlc::new(1_000 + seq, 0),
                payload: Payload::Set {
                    ns: "s".into(),
                    key: k.into(),
                    order: Vec::new(),
                    value: Some(vec![seq as u8]),
                },
            },
            key,
        )
    }

    #[test]
    fn ops_above_removal_cutoff_are_ignored_but_covered() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(10_000);
        let s = Store::open(
            &dir.path().join("b.redb"),
            MeshId([7; 32]),
            SigningKey::from_bytes(&[2; 32]),
            clock,
        )
        .unwrap();
        let x = SigningKey::from_bytes(&[1; 32]);
        let xid = NodeId(x.verifying_key().to_bytes());
        {
            let tx = s.db.begin_write().unwrap();
            tx.open_table(REMOVED)
                .unwrap()
                .insert(
                    keys::origin_key(&xid).as_slice(),
                    keys::u64_value(1).as_slice(),
                )
                .unwrap();
            tx.commit().unwrap();
        }
        // Seq 3 is above the cutoff but not contiguous: stored invalid, vv unchanged.
        assert_eq!(s.apply(op(&x, 3, "c"), &All).unwrap(), Outcome::Ignored);
        assert_eq!(s.vv_of(xid).unwrap(), 0);
        assert_eq!(s.apply(op(&x, 1, "a"), &All).unwrap(), Outcome::Applied);
        assert_eq!(s.vv_of(xid).unwrap(), 1);
        // Seq 2 fills the gap: ignored, and the vv advances over the stored seq 3.
        assert_eq!(s.apply(op(&x, 2, "b"), &All).unwrap(), Outcome::Ignored);
        assert_eq!(s.vv_of(xid).unwrap(), 3);
        assert_eq!(s.get("s", "a").unwrap(), Some(vec![1]));
        assert_eq!(s.get("s", "b").unwrap(), None);
        assert_eq!(s.get("s", "c").unwrap(), None);
        assert_eq!(s.apply(op(&x, 3, "c"), &All).unwrap(), Outcome::Duplicate);
        let tx = s.db.begin_read().unwrap();
        let ops = tx.open_table(OPS).unwrap();
        for (seq, invalid) in [(1, false), (2, true), (3, true)] {
            let g = ops
                .get(keys::op_key(&xid, seq).as_slice())
                .unwrap()
                .unwrap();
            let row: OpRow = tables::decode("ops", g.value()).unwrap();
            assert_eq!(row.invalid, invalid, "seq {seq}");
            assert_eq!(row.received_ms, 10_000);
        }
    }
}
