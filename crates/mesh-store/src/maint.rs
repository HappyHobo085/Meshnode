//! Maintenance (§5.5, §5.7): compaction, tombstone GC, staleness, re-bootstrap support
//! and namespace purge.

use crate::apply::{Change, ChangeKind, vv_get};
use crate::error::StoreError;
use crate::keys::{self, OP_KEY_LEN};
use crate::read::scan_reg_winners;
use crate::sync::SyncItem;
use crate::tables::{
    self, ALL, META, OPS, OpRow, REG, REMOVED, RegRow, SUPERSEDED, SVC_LOCAL, SVC_LOCAL_COUNT, meta,
};
use crate::{Store, merge, meta_u64};
use mesh_hlc::{Hlc, WallClock};
use mesh_proto::{NodeId, Payload, SignedOp};
use redb::{ReadableDatabase, ReadableTable, TableHandle, WriteTransaction};
use std::collections::{BTreeMap, BTreeSet};

/// How long an op must have been superseded (or invalid) before compaction may delete
/// it (§5.5). Retraction of a removed origin can restore ops younger than this.
pub const SETTLE_MS: u64 = 7 * 86_400_000;
/// Age after which a winning tombstone with the empty order is GC'd (§5.7). The age is
/// measured both by the tombstone's HLC wall time and by when this store received it
/// (Ruling R14), so a tombstone arriving late from an offline device is kept here for
/// this long after its arrival.
pub const TOMBSTONE_TTL_MS: u64 = 90 * 86_400_000;
/// A store whose last full sync is older than this must re-bootstrap (§5.7).
pub const STALE_MS: u64 = 80 * 86_400_000;

/// Result of [`Store::export_unsynced_local_ops`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Export {
    /// Items to re-apply, in order.
    pub items: Vec<SyncItem>,
    /// Own `Set` ops too old to re-apply (older than [`TOMBSTONE_TTL_MS`]).
    pub expired: Vec<SignedOp>,
}

/// Tables [`Store::reset_to_empty`] leaves alone: `svc_local*` hold node-local service
/// entries (replay nonces and the like), which no catch-up would bring back.
const KEEP_TABLES_ON_RESET: [tables::Def; 4] = [META, REMOVED, SVC_LOCAL, SVC_LOCAL_COUNT];

/// `meta` rows kept by [`Store::reset_to_empty`].
const KEEP_ON_RESET: [&str; 5] = [
    meta::MESH,
    meta::NODE,
    meta::SCHEMA,
    meta::NEXT_SEQ,
    meta::LAST_HLC,
];

/// True if the `ops` row under `k` may be deleted by compaction: it exists, is not a
/// valid `SetAdd` (an invalid op was never merged, so any kind may go), is at or below
/// `vv[origin]` (so its absence is reported as `Covered`), and is not its origin's
/// current `reg` row for its key.
fn compactable(tx: &WriteTransaction, k: &[u8; OP_KEY_LEN]) -> Result<bool, StoreError> {
    let (origin, seq) = keys::split_op_key("ops", k)?;
    let row: OpRow = {
        let ops = tx.open_table(OPS)?;
        match ops.get(k.as_slice())? {
            Some(g) => tables::decode("ops", g.value())?,
            None => return Ok(false),
        }
    };
    if seq > vv_get(tx, &origin)? {
        return Ok(false);
    }
    match &row.op.body.payload {
        Payload::SetAdd { .. } => Ok(row.invalid),
        Payload::Set { ns, key, .. } => {
            // Defensive: the merge guard keeps a current winner out of `superseded`, but
            // compaction must never delete the op a `reg` row points at.
            let reg = tx.open_table(REG)?;
            if let Some(g) = reg.get(keys::reg_key(ns, key, &origin).as_slice())? {
                let cur: RegRow = tables::decode("reg", g.value())?;
                if cur.seq == seq {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

/// Deletes the `ops` row and the `superseded` entry under each key of `doomed`.
fn delete_ops(tx: &WriteTransaction, doomed: &[[u8; OP_KEY_LEN]]) -> Result<(), StoreError> {
    let mut ops = tx.open_table(OPS)?;
    let mut sup = tx.open_table(SUPERSEDED)?;
    for k in doomed {
        ops.remove(k.as_slice())?;
        sup.remove(k.as_slice())?;
    }
    Ok(())
}

fn utf8(b: &[u8]) -> Result<String, StoreError> {
    String::from_utf8(b.to_vec()).map_err(|_| StoreError::corrupt("reg", "name is not UTF-8"))
}

/// A key due for GC: its winner and what deleting it would remove.
struct Doomed {
    order: Vec<u8>,
    hlc: Hlc,
    origin: NodeId,
    /// Seq of the winning tombstone's op.
    seq: u64,
    change: Change,
    ops: BTreeSet<[u8; OP_KEY_LEN]>,
    /// Deleting would drop one of this node's own ops not yet covered by a full sync.
    unsafe_own: bool,
}

impl<W: WallClock> Store<W> {
    /// Compaction (§5.5), in one write transaction. Deletes an `ops` row whose
    /// `superseded` entry is at least [`SETTLE_MS`] old, together with that entry. Valid
    /// ops enter `superseded` when they lose; invalid ops when they become invalid. Never
    /// deletes a valid `SetAdd` op, an op above its origin's vv, or an op that is its
    /// origin's current `reg` row. Peers asking for a deleted seq receive a `Covered`
    /// marker from `diff_since`. Returns the number of `ops` rows deleted.
    pub fn compact(&self) -> Result<usize, StoreError> {
        let clock = self.lock_clock();
        let now = clock.wall().now_ms();
        let tx = self.db.begin_write()?;
        let mut candidates: Vec<[u8; OP_KEY_LEN]> = Vec::new();
        {
            let sup = tx.open_table(SUPERSEDED)?;
            for item in sup.iter()? {
                let (k, v) = item?;
                let at = keys::read_u64_value("superseded", v.value())?;
                if now.saturating_sub(at) >= SETTLE_MS {
                    let (origin, seq) = keys::split_op_key("superseded", k.value())?;
                    candidates.push(keys::op_key(&origin, seq));
                }
            }
        }
        let mut doomed = Vec::new();
        for k in candidates {
            if compactable(&tx, &k)? {
                doomed.push(k);
            }
        }
        delete_ops(&tx, &doomed)?;
        tx.commit()?;
        Ok(doomed.len())
    }

    /// Tombstone GC (§5.7), in one write transaction. Does nothing while the store is
    /// stale. For every key whose winner (over all origins) is a tombstone with the empty
    /// order that is older than [`TOMBSTONE_TTL_MS`] both by `hlc.ms()` and by its local
    /// `received_ms` (Ruling R14; ordered tombstones are never GC'd), deletes **all** of
    /// the key's `reg` rows
    /// (deleting only the tombstone would bring back an older value of another origin),
    /// every valid `ops` row of the key (including superseded ops compaction has not
    /// removed yet, so a fresh peer catching up from this store can't resurrect an old
    /// value) and every invalid op of the key that does not beat the winner, each with its
    /// `superseded` entry. A key is skipped if that would delete one of this node's own
    /// ops with a seq above the `synced_self_seq` of the last full sync (any own op if it
    /// never synced): the op may not have reached any peer yet. Set rows are untouched. Publishes one `Change` per GC'd key
    /// after commit (`local: false`: nothing to broadcast). Returns the number of keys.
    pub fn gc(&self) -> Result<usize, StoreError> {
        let clock = self.lock_clock();
        let now = clock.wall().now_ms();
        let cutoff = now.saturating_sub(TOMBSTONE_TTL_MS);
        let tx = self.db.begin_write()?;
        let (synced_ms, synced_seq) = {
            let m = tx.open_table(META)?;
            (
                meta_u64(&m, meta::LAST_FULL_SYNC_MS)?,
                meta_u64(&m, meta::SYNCED_SELF_SEQ)?,
            )
        };
        if synced_ms.is_some_and(|t| t < now.saturating_sub(STALE_MS)) {
            tx.abort()?;
            return Ok(0);
        }

        // Keyed by `ns ‖ 0 ‖ key ‖ 0`.
        let mut doomed: BTreeMap<Vec<u8>, Doomed> = BTreeMap::new();
        {
            let reg = tx.open_table(REG)?;
            scan_reg_winners(&reg, &[], &[], |ns, key, origin, row| {
                // Ordered tombstones are never GC'd (R14): a later write that loses to
                // their order would win on a replica that GC'd and lose on one that did not.
                if row.value.is_none() && row.order.is_empty() && row.hlc.ms() < cutoff {
                    let mut g = Vec::with_capacity(ns.len() + key.len() + 2);
                    g.extend_from_slice(ns);
                    g.push(keys::SEP);
                    g.extend_from_slice(key);
                    g.push(keys::SEP);
                    let change = Change {
                        ns: utf8(ns)?,
                        kind: ChangeKind::Key(utf8(key)?),
                        local: false,
                    };
                    doomed.insert(
                        g,
                        Doomed {
                            order: row.order,
                            hlc: row.hlc,
                            origin,
                            seq: row.seq,
                            change,
                            ops: BTreeSet::new(),
                            unsafe_own: false,
                        },
                    );
                }
                Ok(true)
            })?;
        }
        // Old by arrival too (R14): a tombstone received less than the TTL ago may not
        // have reached the other replicas yet. A winner without an op row (never expected)
        // is kept.
        {
            let ops = tx.open_table(OPS)?;
            let mut young = Vec::new();
            for (g, d) in &doomed {
                let received = match ops.get(keys::op_key(&d.origin, d.seq).as_slice())? {
                    Some(v) => Some(tables::decode::<OpRow>("ops", v.value())?.received_ms),
                    None => None,
                };
                if received.is_none_or(|r| r >= cutoff) {
                    young.push(g.clone());
                }
            }
            for g in young {
                doomed.remove(&g);
            }
        }
        if doomed.is_empty() {
            tx.abort()?;
            return Ok(0);
        }

        // The ops of each doomed key that go with it, without scanning `ops`: every op of
        // a key is either its origin's current `reg` row (valid) or has a `superseded`
        // entry (a valid op that lost or was replaced, or an invalid op, Ruling R9), until
        // compaction deletes both together. So the key's `reg` rows give the winners, and
        // one pass over `superseded` (decoding only those ops) gives the rest. The cost is
        // O(rows of the doomed keys + |superseded|); `superseded` holds only ops superseded
        // within the last SETTLE_MS plus ops compaction may never delete (invalid ops above
        // their origin's vv).
        {
            let reg = tx.open_table(REG)?;
            for (g, d) in doomed.iter_mut() {
                for item in reg.range::<&[u8]>(g.as_slice()..)? {
                    let (k, v) = item?;
                    if !k.value().starts_with(g) {
                        break;
                    }
                    let origin = keys::split_reg_key(k.value())?.origin;
                    let seq = tables::decode::<RegRow>("reg", v.value())?.seq;
                    if origin == self.node && synced_seq.is_none_or(|s| seq > s) {
                        d.unsafe_own = true;
                    }
                    d.ops.insert(keys::op_key(&origin, seq));
                }
            }
        }
        {
            let sup = tx.open_table(SUPERSEDED)?;
            let ops = tx.open_table(OPS)?;
            for item in sup.iter()? {
                let (k, _) = item?;
                let row: OpRow = match ops.get(k.value())? {
                    Some(g) => tables::decode("ops", g.value())?,
                    None => continue,
                };
                let b = &row.op.body;
                let Payload::Set { ns, key, order, .. } = &b.payload else {
                    continue;
                };
                let Some(d) = doomed.get_mut(&keys::item_prefix(ns, key)) else {
                    continue;
                };
                if row.invalid
                    && merge::beats((order, b.hlc, b.origin), (&d.order, d.hlc, d.origin))
                {
                    // Would win if a raised cutoff re-validated it: keep.
                    continue;
                }
                if b.origin == self.node && synced_seq.is_none_or(|s| b.seq > s) {
                    d.unsafe_own = true;
                }
                let (origin, seq) = keys::split_op_key("superseded", k.value())?;
                d.ops.insert(keys::op_key(&origin, seq));
            }
        }
        doomed.retain(|_, d| !d.unsafe_own);
        if doomed.is_empty() {
            tx.abort()?;
            return Ok(0);
        }

        let mut changes = Vec::with_capacity(doomed.len());
        {
            let mut reg = tx.open_table(REG)?;
            for (g, d) in &doomed {
                let mut rows = Vec::new();
                for item in reg.range::<&[u8]>(g.as_slice()..)? {
                    let (k, _) = item?;
                    if !k.value().starts_with(g) {
                        break;
                    }
                    rows.push(k.value().to_vec());
                }
                for k in rows {
                    reg.remove(k.as_slice())?;
                }
                changes.push(d.change.clone());
            }
        }
        let all_ops: Vec<_> = doomed
            .values()
            .flat_map(|d| d.ops.iter().copied())
            .collect();
        delete_ops(&tx, &all_ops)?;
        tx.commit()?;
        let n = changes.len();
        self.publish(changes);
        Ok(n)
    }

    /// True if the last full sync is older than [`STALE_MS`]; the caller must then
    /// re-bootstrap (§5.7). A store that has never completed a full sync is not stale.
    pub fn is_stale(&self) -> Result<bool, StoreError> {
        let now = self.lock_clock().wall().now_ms();
        Ok(self
            .last_full_sync_ms()?
            .is_some_and(|t| t < now.saturating_sub(STALE_MS)))
    }

    /// This node's own ops with a seq above the `synced_self_seq` of the last full sync
    /// (Ruling R15) — all of them if it never synced — in seq order, for re-applying after
    /// re-bootstrap. Own ops already compacted leave holes; for a hole strictly above the
    /// boundary (before an exported op that does not directly follow the previous exported
    /// one or the boundary) a `Covered { origin: self, upto: seq - 1 }` is emitted, so
    /// re-applying never parks an op behind a gap. Seqs at or below the boundary are never
    /// covered by an export: the catch-up from the peers brings them back. Re-apply `Op`
    /// items with `apply` and `Covered` items with `apply_covered(self, upto, upto, ..)`,
    /// in order.
    ///
    /// Own `Set` ops whose `hlc.ms()` is older than [`TOMBSTONE_TTL_MS`] (by the store's
    /// wall clock) are not exported: they come back in [`Export::expired`] and their seqs
    /// count as holes. `SetAdd` ops never expire.
    pub fn export_unsynced_local_ops(&self) -> Result<Export, StoreError> {
        let cutoff = self
            .lock_clock()
            .wall()
            .now_ms()
            .saturating_sub(TOMBSTONE_TTL_MS);
        let tx = self.db.begin_read()?;
        let synced = meta_u64(&tx.open_table(META)?, meta::SYNCED_SELF_SEQ)?.unwrap_or(0);
        let ops = tx.open_table(OPS)?;
        let Some(first) = synced.checked_add(1) else {
            return Ok(Export::default());
        };
        let lo = keys::op_key(&self.node, first);
        let hi = keys::op_key(&self.node, u64::MAX);
        let mut out = Export::default();
        let mut prev: u64 = synced;
        for item in ops.range(lo.as_slice()..=hi.as_slice())? {
            let (_, v) = item?;
            let row: OpRow = tables::decode("ops", v.value())?;
            let seq = row.op.body.seq;
            if matches!(row.op.body.payload, Payload::Set { .. }) && row.op.body.hlc.ms() < cutoff {
                out.expired.push(row.op);
                continue;
            }
            // Seqs are ascending and above `synced`, so `seq - 1 >= prev` here.
            if seq != prev.saturating_add(1) {
                out.items.push(SyncItem::Covered {
                    origin: self.node,
                    upto: seq - 1,
                });
            }
            prev = seq;
            out.items.push(SyncItem::Op(row.op));
        }
        Ok(out)
    }

    /// Clears every table except `meta` and `removed`, in one write transaction. `meta`
    /// keeps the mesh and node ids, schema, `next_seq` and `last_hlc` (so seqs and HLCs
    /// never repeat); `last_full_sync_ms` and `synced_self_seq` are removed because the
    /// store is now empty.
    /// `removed` is kept so the catch-up that follows still stores ops above a removal
    /// cutoff as invalid. Publishes nothing.
    ///
    /// Local writes are refused with [`StoreError::Rebootstrapping`] from here until
    /// `vv[self]` is back at `next_seq - 1` (Ruling R18, enforcing the R10 contract): the
    /// catch-up from the peers plus re-applying the items from
    /// [`Store::export_unsynced_local_ops`] restore it. A local write in between would set
    /// `vv[self]` past seqs this store no longer holds, so `diff_since` would report the
    /// unsynced ops as `Covered` and peers would skip them.
    pub fn reset_to_empty(&self) -> Result<(), StoreError> {
        let _clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        for def in ALL {
            if !KEEP_TABLES_ON_RESET.iter().any(|k| k.name() == def.name()) {
                tx.open_table(def)?.retain(|_, _| false)?;
            }
        }
        {
            let mut m = tx.open_table(META)?;
            let mut drop_rows = Vec::new();
            for item in m.iter()? {
                let (k, _) = item?;
                let name = k.value();
                if !KEEP_ON_RESET.iter().any(|n| n.as_bytes() == name) {
                    drop_rows.push(name.to_vec());
                }
            }
            for name in drop_rows {
                m.remove(name.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Writes a local tombstone for every live key of `ns` (via [`Store::set_ordered`]
    /// with the winner's order, so it beats ordered winners too; each gets its own seq
    /// and op) and returns the ops in key order for broadcasting. Not atomic: keys are
    /// listed first, then deleted one by one.
    ///
    /// Set members are untouched: sets are grow-only, so they survive a purge until the
    /// namespace itself is tombstoned in `_mesh` (sub-spec 3). A `ns` containing NUL has
    /// no keys, so nothing is written.
    ///
    /// Refused with [`StoreError::OutOfScope`] before anything is written when the store's
    /// scope does not admit `ns`.
    pub fn purge_namespace(&self, ns: &str) -> Result<Vec<SignedOp>, StoreError> {
        const PAGE: u32 = 1024;
        self.check_scope(ns)?;
        let mut live: Vec<String> = Vec::new();
        loop {
            let page = self.list_keys(ns, "", live.last().map(String::as_str), PAGE)?;
            let done = page.len() < PAGE as usize;
            live.extend(page);
            if done {
                break;
            }
        }
        let mut out = Vec::with_capacity(live.len());
        for k in live {
            // The clock ticks past every observed HLC, so the same order with a newer
            // HLC always beats the current winner.
            if let Some(e) = self.get_entry(ns, &k)?
                && e.value.is_some()
            {
                out.push(self.set_ordered(ns, &k, &e.order, None)?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::{Acceptor, Outcome, ReadView, Verdict};
    use ed25519_dalek::SigningKey;
    use mesh_hlc::{Hlc, ManualClock};
    use mesh_proto::{MeshId, NodeId, OpBody, VersionVector};

    const DAY: u64 = 86_400_000;

    struct All;
    impl Acceptor for All {
        fn judge(&self, _: &OpBody, _: &ReadView<'_>) -> Verdict {
            Verdict::Valid
        }
    }

    fn open(dir: &std::path::Path, n: u8, clock: &ManualClock) -> Store<ManualClock> {
        Store::open(
            &dir.join(format!("n{n}.redb")),
            MeshId([7; 32]),
            SigningKey::from_bytes(&[n; 32]),
            clock.clone(),
        )
        .unwrap()
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

    fn has_op(s: &Store<ManualClock>, origin: &NodeId, seq: u64) -> bool {
        let tx = s.db.begin_read().unwrap();
        let ops = tx.open_table(OPS).unwrap();
        ops.get(keys::op_key(origin, seq).as_slice())
            .unwrap()
            .is_some()
    }

    #[test]
    fn compaction_never_deletes_current_winner() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(10_000);
        let s = open(dir.path(), 1, &clock);
        s.set("s", "k", b"1").unwrap();
        // Corrupt state the merge guard should prevent: the winner listed as superseded.
        {
            let tx = s.db.begin_write().unwrap();
            tx.open_table(SUPERSEDED)
                .unwrap()
                .insert(
                    keys::op_key(&s.node, 1).as_slice(),
                    keys::u64_value(10_000).as_slice(),
                )
                .unwrap();
            tx.commit().unwrap();
        }
        clock.advance(30 * DAY);
        assert_eq!(s.compact().unwrap(), 0);
        assert!(has_op(&s, &s.node, 1));
        assert_eq!(s.get("s", "k").unwrap(), Some(b"1".to_vec()));
        let items = s.diff_since(&VersionVector::default(), 10).unwrap();
        assert!(matches!(items.as_slice(), [crate::SyncItem::Op(_)]));
    }

    #[test]
    fn compaction_removes_invalid_ops_after_settle_but_not_above_vv() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(10_000);
        let s = open(dir.path(), 2, &clock);
        let x = SigningKey::from_bytes(&[1; 32]);
        let xid = NodeId(x.verifying_key().to_bytes());
        {
            let tx = s.db.begin_write().unwrap();
            tx.open_table(REMOVED)
                .unwrap()
                .insert(
                    keys::origin_key(&xid).as_slice(),
                    keys::u64_value(0).as_slice(),
                )
                .unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(s.apply(op(&x, 1, "a"), &All).unwrap(), Outcome::Ignored);
        assert_eq!(s.apply(op(&x, 3, "c"), &All).unwrap(), Outcome::Ignored);
        assert_eq!(s.vv_of(xid).unwrap(), 1);
        clock.advance(SETTLE_MS - 1);
        assert_eq!(s.compact().unwrap(), 0);
        clock.advance(1);
        assert_eq!(s.compact().unwrap(), 1);
        assert!(!has_op(&s, &xid, 1));
        // Seq 3 is above vv[x]: its absence would not be reported as covered.
        assert!(has_op(&s, &xid, 3));
        let items = s.diff_since(&VersionVector::default(), 10).unwrap();
        assert_eq!(
            items,
            vec![crate::SyncItem::Covered {
                origin: xid,
                upto: 1
            }]
        );
    }

    fn set_removed(s: &Store<ManualClock>, origin: &NodeId, cutoff: u64) {
        let tx = s.db.begin_write().unwrap();
        tx.open_table(REMOVED)
            .unwrap()
            .insert(
                keys::origin_key(origin).as_slice(),
                keys::u64_value(cutoff).as_slice(),
            )
            .unwrap();
        tx.commit().unwrap();
    }

    fn hand(key: &SigningKey, seq: u64, hlc_ms: u64, payload: Payload) -> SignedOp {
        SignedOp::sign(
            OpBody {
                mesh: MeshId([7; 32]),
                origin: NodeId(key.verifying_key().to_bytes()),
                seq,
                hlc: Hlc::new(hlc_ms, 0),
                payload,
            },
            key,
        )
    }

    fn set_k(value: Option<&[u8]>) -> Payload {
        Payload::Set {
            ns: "s".into(),
            key: "k".into(),
            order: Vec::new(),
            value: value.map(<[u8]>::to_vec),
        }
    }

    fn has_superseded(s: &Store<ManualClock>, origin: &NodeId, seq: u64) -> bool {
        let tx = s.db.begin_read().unwrap();
        let t = tx.open_table(SUPERSEDED).unwrap();
        t.get(keys::op_key(origin, seq).as_slice())
            .unwrap()
            .is_some()
    }

    #[test]
    fn ignored_op_is_aged_from_arrival_including_set_add() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(10_000);
        let s = open(dir.path(), 2, &clock);
        let x = SigningKey::from_bytes(&[1; 32]);
        let xid = NodeId(x.verifying_key().to_bytes());
        set_removed(&s, &xid, 0);
        clock.advance(DAY);
        let add = Payload::SetAdd {
            ns: "s".into(),
            set: "m".into(),
            member: b"x".to_vec(),
        };
        assert_eq!(
            s.apply(hand(&x, 1, 1_000, add), &All).unwrap(),
            Outcome::Ignored
        );
        assert!(has_superseded(&s, &xid, 1));
        clock.advance(SETTLE_MS - 1);
        assert_eq!(s.compact().unwrap(), 0);
        clock.advance(1);
        // An invalid SetAdd was never merged, so it is compacted too.
        assert_eq!(s.compact().unwrap(), 1);
        assert!(!has_op(&s, &xid, 1));
        assert!(!has_superseded(&s, &xid, 1));
        assert!(!s.set_contains("s", "m", b"x").unwrap());
    }

    #[test]
    fn reset_keeps_removed() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(10_000);
        let s = open(dir.path(), 2, &clock);
        let x = SigningKey::from_bytes(&[1; 32]);
        let xid = NodeId(x.verifying_key().to_bytes());
        set_removed(&s, &xid, 0);
        s.reset_to_empty().unwrap();
        assert_eq!(s.apply(op(&x, 1, "a"), &All).unwrap(), Outcome::Ignored);
        assert_eq!(s.get("s", "a").unwrap(), None);
    }

    #[test]
    fn gc_deletes_invalid_ops_that_lose_and_keeps_ones_that_win() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(10_000);
        let s = open(dir.path(), 2, &clock);
        let x = SigningKey::from_bytes(&[1; 32]);
        let xid = NodeId(x.verifying_key().to_bytes());
        let y = SigningKey::from_bytes(&[3; 32]);
        let yid = NodeId(y.verifying_key().to_bytes());
        set_removed(&s, &xid, 0);
        // y's tombstone (hlc 5000) is the winner; x's invalid ops straddle it.
        assert_eq!(
            s.apply(hand(&y, 1, 5_000, set_k(None)), &All).unwrap(),
            Outcome::Applied
        );
        assert_eq!(
            s.apply(hand(&x, 1, 4_000, set_k(Some(b"old"))), &All)
                .unwrap(),
            Outcome::Ignored
        );
        assert_eq!(
            s.apply(hand(&x, 2, 6_000, set_k(Some(b"new"))), &All)
                .unwrap(),
            Outcome::Ignored
        );
        // Received at 10_000: exactly TTL old by arrival is not yet old enough.
        clock.advance(TOMBSTONE_TTL_MS);
        assert_eq!(s.gc().unwrap(), 0);
        clock.advance(1);
        assert_eq!(s.gc().unwrap(), 1);
        assert!(s.get_entry("s", "k").unwrap().is_none());
        assert!(!has_op(&s, &yid, 1));
        // Loses to the GC'd tombstone: could only resurrect k, so it goes.
        assert!(!has_op(&s, &xid, 1));
        assert!(!has_superseded(&s, &xid, 1));
        // Would beat the tombstone if re-validated: kept (compaction ages it).
        assert!(has_op(&s, &xid, 2));
        assert!(has_superseded(&s, &xid, 2));
    }
}
