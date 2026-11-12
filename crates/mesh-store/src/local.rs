//! Local writes: build, sign, merge and record an op in one transaction.

use crate::apply::{change_for, vv_get};
use crate::error::StoreError;
use crate::tables::{self, META, OPS, OpRow, PENDING, VV, meta};
use crate::{Store, keys, merge, meta_u64_req};
use mesh_hlc::{Hlc, WallClock};
use mesh_proto::{OpBody, Payload, SignedOp};
use redb::ReadableTable;

impl<W: WallClock> Store<W> {
    /// Sets `ns/key` to `value` with the empty order.
    pub fn set(&self, ns: &str, key: &str, value: &[u8]) -> Result<SignedOp, StoreError> {
        self.set_ordered(ns, key, &[], Some(value))
    }

    /// Writes a tombstone for `ns/key` with the empty order.
    pub fn delete(&self, ns: &str, key: &str) -> Result<SignedOp, StoreError> {
        self.set_ordered(ns, key, &[], None)
    }

    /// Writes `ns/key` with an explicit `order` (compared before the HLC); `None` is a tombstone.
    pub fn set_ordered(
        &self,
        ns: &str,
        key: &str,
        order: &[u8],
        value: Option<&[u8]>,
    ) -> Result<SignedOp, StoreError> {
        self.local_write(Payload::Set {
            ns: ns.to_owned(),
            key: key.to_owned(),
            order: order.to_vec(),
            value: value.map(<[u8]>::to_vec),
        })
    }

    /// Adds `member` to the add-only set `ns/set`.
    pub fn set_add(&self, ns: &str, set: &str, member: &[u8]) -> Result<SignedOp, StoreError> {
        self.local_write(Payload::SetAdd {
            ns: ns.to_owned(),
            set: set.to_owned(),
            member: member.to_vec(),
        })
    }

    /// Validates `payload` (before consuming a seq or ticking the clock), then in one
    /// write transaction: reads `next_seq`, ticks the clock, signs, merges, inserts into
    /// `ops`, sets `vv[self] = seq`, and advances `next_seq` / `last_hlc`. After commit,
    /// publishes a local `Change` if the merge changed a row.
    ///
    /// Refused with [`StoreError::Rebootstrapping`] while `vv[self] != next_seq - 1`
    /// (Ruling R18): some of this node's own seqs are not held contiguously (after
    /// `reset_to_empty`, or with an echoed own op waiting in `pending`), and writing now
    /// would set `vv[self]` past them, so peers would skip them as covered.
    ///
    /// Refused with [`StoreError::OutOfScope`] when the store's scope does not admit the
    /// payload's namespace (sub-spec 8a §3): a partial node never creates ops that only
    /// full nodes would hold.
    fn local_write(&self, payload: Payload) -> Result<SignedOp, StoreError> {
        self.check_scope(payload.ns())?;
        let mut body = OpBody {
            mesh: self.mesh,
            origin: self.node,
            seq: 0,
            hlc: Hlc::ZERO,
            payload,
        };
        body.validate()?;

        let mut clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        let seq = {
            let m = tx.open_table(META)?;
            meta_u64_req(&m, meta::NEXT_SEQ)?
        };
        if vv_get(&tx, &self.node)?.checked_add(1) != Some(seq) {
            return Err(StoreError::Rebootstrapping);
        }
        // Never reuse a seq held in `ops` or `pending` (would be equivocation).
        // Unreachable while apply raises `next_seq` for echoed own ops (stored or
        // pending); kept as a guard.
        let k = keys::op_key(&self.node, seq);
        if tx.open_table(OPS)?.get(k.as_slice())?.is_some()
            || tx.open_table(PENDING)?.get(k.as_slice())?.is_some()
        {
            return Err(StoreError::corrupt(
                "meta",
                format!("next_seq {seq} is already used by a stored op"),
            ));
        }
        let next_seq = seq
            .checked_add(1)
            .ok_or_else(|| StoreError::corrupt("meta", "next_seq overflow"))?;
        body.seq = seq;
        body.hlc = clock.tick();
        let now_ms = clock.wall().now_ms();
        let op = SignedOp::sign(body, &self.key);

        let changed = merge::merge_op(&tx, &op, now_ms)?;
        {
            let row = OpRow {
                op: op.clone(),
                received_ms: now_ms,
                invalid: false,
            };
            let mut ops = tx.open_table(OPS)?;
            ops.insert(
                keys::op_key(&self.node, seq).as_slice(),
                tables::encode("ops", &row)?.as_slice(),
            )?;
            let mut vv = tx.open_table(VV)?;
            vv.insert(
                keys::origin_key(&self.node).as_slice(),
                keys::u64_value(seq).as_slice(),
            )?;
            let mut m = tx.open_table(META)?;
            m.insert(
                meta::NEXT_SEQ.as_bytes(),
                keys::u64_value(next_seq).as_slice(),
            )?;
            m.insert(
                meta::LAST_HLC.as_bytes(),
                keys::u64_value(op.body.hlc.as_u64()).as_slice(),
            )?;
        }
        tx.commit()?;
        if changed {
            self.publish(vec![change_for(&op.body, true)]);
        }
        Ok(op)
    }
}
