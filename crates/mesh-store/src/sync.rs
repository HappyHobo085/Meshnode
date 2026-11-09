//! Catch-up support (§5.8): version vectors, diffs for a peer, and covered ranges.

use crate::apply::{Acceptor, Pipeline, vv_get, vv_set};
use crate::error::StoreError;
use crate::tables::{self, META, OPS, OpRow, VV, meta};
use crate::{Store, keys, meta_u64};
use mesh_hlc::WallClock;
use mesh_proto::{NodeId, SignedOp, VersionVector};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};

/// One element of a catch-up stream (serializable so mesh-core can persist a re-bootstrap
/// export).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncItem {
    Op(SignedOp),
    /// Everything of `origin` up to and including `upto` that the sender holds no op row
    /// for (compacted or GC'd); the receiver may treat it as seen.
    Covered {
        origin: NodeId,
        upto: u64,
    },
}

impl<W: WallClock> Store<W> {
    /// Highest contiguous seq held per origin (includes this node once it has written).
    pub fn version_vector(&self) -> Result<VersionVector, StoreError> {
        let tx = self.db.begin_read()?;
        let vv = tx.open_table(VV)?;
        let mut out = VersionVector::default();
        for row in vv.iter()? {
            let (k, v) = row?;
            let origin = keys::split_origin_key("vv", k.value())?;
            out.set(origin, keys::read_u64_value("vv", v.value())?);
        }
        Ok(out)
    }

    /// Items the peer (whose vector is `theirs`) lacks: origins in `NodeId` order, seqs
    /// ascending, at most `limit` items (ops and covered markers both count). Only rows
    /// of `ops` at or below `vv[origin]` are served, never `pending`. A run of seqs
    /// without a row becomes one `Covered` marker. Origins we don't know are ignored.
    pub fn diff_since(
        &self,
        theirs: &VersionVector,
        limit: usize,
    ) -> Result<Vec<SyncItem>, StoreError> {
        let mut items = Vec::new();
        if limit == 0 {
            return Ok(items);
        }
        let tx = self.db.begin_read()?;
        let vv = tx.open_table(VV)?;
        let ops = tx.open_table(OPS)?;
        for row in vv.iter()? {
            let (k, v) = row?;
            let origin = keys::split_origin_key("vv", k.value())?;
            let ours = keys::read_u64_value("vv", v.value())?;
            let have = theirs.get(&origin);
            if ours <= have {
                continue;
            }
            // `have < ours <= u64::MAX`, so `have + 1` cannot overflow.
            let mut expected = have + 1;
            let lo = keys::op_key(&origin, expected);
            let hi = keys::op_key(&origin, ours);
            for r in ops.range(lo.as_slice()..=hi.as_slice())? {
                let (ok, ov) = r?;
                let (_, seq) = keys::split_op_key("ops", ok.value())?;
                if seq > expected {
                    items.push(SyncItem::Covered {
                        origin,
                        upto: seq - 1,
                    });
                    if items.len() >= limit {
                        return Ok(items);
                    }
                }
                let row: OpRow = tables::decode("ops", ov.value())?;
                items.push(SyncItem::Op(row.op));
                if items.len() >= limit {
                    return Ok(items);
                }
                expected = seq + 1;
            }
            if expected <= ours {
                items.push(SyncItem::Covered { origin, upto: ours });
                if items.len() >= limit {
                    return Ok(items);
                }
            }
        }
        Ok(items)
    }

    /// The sender compacted or GC'd everything of `origin` up to `upto`: raise
    /// `vv[origin]` to it (if higher), then drain the origin's unblocked pending ops.
    /// Pending entries now covered are dropped. For this node's own origin, `next_seq`
    /// is raised to `upto + 1` so a seq is never reused. Publishes change events for
    /// anything merged by the drain.
    ///
    /// `advertised` is the sender's `vv[origin]` from its `Hello` (Ruling R16): a marker
    /// above it is a lie or a bug and is ignored (no-op, `tracing::warn!`), as is a marker
    /// for this node's own origin above `u64::MAX / 2` (it would exhaust `next_seq`).
    /// When re-applying this node's own [`Store::export_unsynced_local_ops`] items, pass
    /// `upto` itself.
    pub fn apply_covered(
        &self,
        origin: NodeId,
        upto: u64,
        advertised: u64,
        acc: &dyn Acceptor,
    ) -> Result<(), StoreError> {
        if upto > advertised {
            tracing::warn!(
                origin = %origin.short(),
                upto,
                advertised,
                "ignoring a Covered marker above the sender's advertised version vector"
            );
            return Ok(());
        }
        if origin == self.node && upto > u64::MAX / 2 {
            tracing::warn!(
                upto,
                "ignoring a Covered marker for this node's own origin above u64::MAX / 2"
            );
            return Ok(());
        }
        let mut clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        if upto <= vv_get(&tx, &origin)? {
            tx.abort()?;
            return Ok(());
        }
        vv_set(&tx, &origin, upto)?;
        if origin == self.node {
            let mut m = tx.open_table(META)?;
            let next = crate::meta_u64_req(&m, meta::NEXT_SEQ)?;
            if upto >= next {
                let new = upto
                    .checked_add(1)
                    .ok_or_else(|| StoreError::corrupt("meta", "next_seq overflow"))?;
                m.insert(meta::NEXT_SEQ.as_bytes(), keys::u64_value(new).as_slice())?;
            }
        }
        let changes = {
            let mut p = Pipeline::new(&tx, &mut clock, acc, self.mesh, self.node);
            p.drain(origin)?;
            p.changes
        };
        tx.commit()?;
        self.publish(changes);
        Ok(())
    }

    /// Records that a **full sync** just completed (Ruling R15). A full sync is a completed
    /// two-way exchange with a peer: both sides sent everything the other lacked. mesh-core
    /// must call this after each one, including the catch-up right after joining.
    ///
    /// `synced_self_seq` is the peer's `vv[self]` at the end of the exchange: the highest
    /// of this node's own seqs the peer confirmed holding. Two values are stored:
    ///
    /// - `last_full_sync_ms = wall.now_ms()`, used only for staleness ([`Store::is_stale`]);
    /// - `synced_self_seq`, the boundary for "unsynced own ops": [`Store::gc`] never
    ///   deletes own ops with a seq above it, and [`Store::export_unsynced_local_ops`]
    ///   returns every one of them (exported, or listed in [`crate::Export::expired`] if
    ///   it is a `Set` older than [`crate::TOMBSTONE_TTL_MS`]). An own write made during
    ///   the session after the peer's diff was computed is above it, so it is never
    ///   counted as synced.
    ///
    /// The stored boundary only grows (a later sync with a peer that confirms less does not
    /// lower it) and is capped at the last own seq this node used (`next_seq - 1`). A store
    /// that never synced has no boundary (every own op is unsynced) and is never stale.
    pub fn mark_full_sync(&self, synced_self_seq: u64) -> Result<(), StoreError> {
        let clock = self.lock_clock();
        let now = clock.wall().now_ms();
        let tx = self.db.begin_write()?;
        {
            let mut m = tx.open_table(META)?;
            let last_used = crate::meta_u64_req(&m, meta::NEXT_SEQ)?.saturating_sub(1);
            let old = meta_u64(&m, meta::SYNCED_SELF_SEQ)?.unwrap_or(0);
            let seq = synced_self_seq.min(last_used).max(old);
            m.insert(
                meta::LAST_FULL_SYNC_MS.as_bytes(),
                keys::u64_value(now).as_slice(),
            )?;
            m.insert(
                meta::SYNCED_SELF_SEQ.as_bytes(),
                keys::u64_value(seq).as_slice(),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Wall time of the last `mark_full_sync`, if any.
    pub fn last_full_sync_ms(&self) -> Result<Option<u64>, StoreError> {
        let tx = self.db.begin_read()?;
        let m = tx.open_table(META)?;
        meta_u64(&m, meta::LAST_FULL_SYNC_MS)
    }

    /// The seq the next local write will use (kept by [`Store::reset_to_empty`]).
    pub fn next_seq(&self) -> Result<u64, StoreError> {
        let tx = self.db.begin_read()?;
        let m = tx.open_table(META)?;
        crate::meta_u64_req(&m, meta::NEXT_SEQ)
    }

    /// The own-seq boundary recorded by `mark_full_sync` (`None` if never synced).
    pub fn synced_self_seq(&self) -> Result<Option<u64>, StoreError> {
        let tx = self.db.begin_read()?;
        let m = tx.open_table(META)?;
        meta_u64(&m, meta::SYNCED_SELF_SEQ)
    }

    /// Test hook simulating compaction: deletes only the `ops` row of `(origin, seq)`
    /// (`test-hooks` feature).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn __test_drop_op(&self, origin: NodeId, seq: u64) -> Result<(), StoreError> {
        let _clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        {
            let mut ops = tx.open_table(OPS)?;
            ops.remove(keys::op_key(&origin, seq).as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Test hook: overwrites a raw `meta` u64 row (`test-hooks` feature).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn __test_set_meta_u64(&self, name: &str, v: u64) -> Result<(), StoreError> {
        let _clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        {
            let mut m = tx.open_table(META)?;
            m.insert(name.as_bytes(), keys::u64_value(v).as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }
}
