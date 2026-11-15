//! Catch-up support (§5.8): version vectors, diffs for a peer, and covered ranges.

use crate::apply::{Acceptor, Pipeline, vv_get, vv_set};
use crate::error::StoreError;
use crate::tables::{self, META, OPS, OpRow, VV, meta};
use crate::{Store, keys, meta_u64};
use mesh_hlc::WallClock;
use mesh_proto::{NodeId, Scope, SignedOp, VersionVector};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};

/// One element of a catch-up stream (serializable so mesh-core can persist a re-bootstrap
/// export).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncItem {
    Op(SignedOp),
    /// Everything of `origin` up to and including `upto` that the sender does not send:
    /// seqs it holds no op row for (compacted or GC'd) and, in a scoped diff
    /// ([`Store::diff_since_scoped`]), ops outside the peer's scope. From a Full sender the
    /// receiver may treat it as seen. A Partial sender's markers may mean "filtered", so a
    /// Full receiver must ignore every marker a Partial peer sends (sub-spec 8a §3).
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
    ///
    /// Same as [`Store::diff_since_scoped`] with [`Scope::Full`].
    pub fn diff_since(
        &self,
        theirs: &VersionVector,
        limit: usize,
    ) -> Result<Vec<SyncItem>, StoreError> {
        self.diff_since_scoped(theirs, limit, &Scope::Full)
    }

    /// [`Store::diff_since`] for a peer replicating only `scope` (sub-spec 8a §3). Within
    /// one origin, a maximal run of seqs that are either without a row (compacted) or
    /// ops whose namespace `scope` does not admit becomes ONE `Covered { origin, upto }`;
    /// admitted ops are sent as [`SyncItem::Op`]. Items count toward `limit` exactly as
    /// in `diff_since`; with `Scope::Full` the output is identical to it.
    ///
    /// Scan budget: one call walks at most `4 × limit` op rows, filtered rows included, so
    /// a long filtered run never holds one long read transaction. When the budget runs out
    /// inside a filtered or compacted run, the call ends with a `Covered` marker up to the
    /// last seq walked, and the next page (from the peer's raised vector) resumes there.
    /// (Each admitted row yields an item, so with `Full` the budget is never reached.)
    pub fn diff_since_scoped(
        &self,
        theirs: &VersionVector,
        limit: usize,
        scope: &Scope,
    ) -> Result<Vec<SyncItem>, StoreError> {
        let mut items = Vec::new();
        if limit == 0 {
            return Ok(items);
        }
        let budget = limit.saturating_mul(4);
        let mut walked = 0usize;
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
            // The first seq not yet sent nor covered: everything in `expected..seq` before
            // an admitted op at `seq` is compacted or filtered out, so one marker covers it.
            // `have < ours <= u64::MAX`, so `have + 1` cannot overflow.
            let mut expected = have + 1;
            let lo = keys::op_key(&origin, expected);
            let hi = keys::op_key(&origin, ours);
            for r in ops.range(lo.as_slice()..=hi.as_slice())? {
                let (ok, ov) = r?;
                let (_, seq) = keys::split_op_key("ops", ok.value())?;
                let row: OpRow = tables::decode("ops", ov.value())?;
                walked += 1;
                if !scope.admits(row.op.body.payload.ns()) {
                    if walked >= budget {
                        // `expected..=seq` is all filtered or compacted; `items.len() <
                        // limit` here, so the marker keeps the page within `limit`.
                        items.push(SyncItem::Covered { origin, upto: seq });
                        return Ok(items);
                    }
                    continue;
                }
                if seq > expected {
                    items.push(SyncItem::Covered {
                        origin,
                        upto: seq - 1,
                    });
                    if items.len() >= limit {
                        return Ok(items);
                    }
                }
                items.push(SyncItem::Op(row.op));
                if items.len() >= limit || walked >= budget {
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

    /// A store that never synced completed a round with a stale peer whose last full sync
    /// was at `at_ms` (sub-spec 8a C1c): records that time as its own, clamped to at most
    /// `now - STALE_MS - 1`, so the store is stale like its source (whatever the peer
    /// claims) and never a restore source for that data. Does nothing once a full sync is
    /// recorded. `synced_self_seq` is not touched: a stale source proves nothing about this
    /// node's own ops.
    pub fn inherit_full_sync(&self, at_ms: u64) -> Result<(), StoreError> {
        let clock = self.lock_clock();
        let latest_stale = clock
            .wall()
            .now_ms()
            .saturating_sub(crate::STALE_MS)
            .saturating_sub(1);
        let tx = self.db.begin_write()?;
        {
            let mut m = tx.open_table(META)?;
            if meta_u64(&m, meta::LAST_FULL_SYNC_MS)?.is_none() {
                m.insert(
                    meta::LAST_FULL_SYNC_MS.as_bytes(),
                    keys::u64_value(at_ms.min(latest_stale)).as_slice(),
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// This store applied ops from a stale peer whose last full sync was at `at_ms`
    /// (sub-spec 8a C1e): lowers `last_full_sync_ms` to `at_ms` clamped to at most
    /// `now - STALE_MS - 1`, or sets it, so the store is stale like the data it took, even
    /// if it had a recent time. `synced_self_seq` is not touched.
    pub fn taint_full_sync(&self, at_ms: u64) -> Result<(), StoreError> {
        let clock = self.lock_clock();
        let latest_stale = clock
            .wall()
            .now_ms()
            .saturating_sub(crate::STALE_MS)
            .saturating_sub(1);
        let tx = self.db.begin_write()?;
        {
            let mut m = tx.open_table(META)?;
            let t = at_ms.min(latest_stale);
            let t = meta_u64(&m, meta::LAST_FULL_SYNC_MS)?.map_or(t, |cur| cur.min(t));
            m.insert(
                meta::LAST_FULL_SYNC_MS.as_bytes(),
                keys::u64_value(t).as_slice(),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A round with a Fresh peer that is no full replica (a client, sub-spec 8a C1c):
    /// `last_full_sync_ms = wall.now_ms()`, like [`Store::mark_full_sync`], but
    /// `synced_self_seq` is left as it is (a Partial peer's vector proves nothing about
    /// this node's own ops).
    pub fn refresh_full_sync_time(&self) -> Result<(), StoreError> {
        let clock = self.lock_clock();
        let now = clock.wall().now_ms();
        let tx = self.db.begin_write()?;
        {
            let mut m = tx.open_table(META)?;
            m.insert(
                meta::LAST_FULL_SYNC_MS.as_bytes(),
                keys::u64_value(now).as_slice(),
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
