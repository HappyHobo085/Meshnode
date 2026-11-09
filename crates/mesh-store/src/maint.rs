//! Maintenance (§5.5, §5.7): compaction, tombstone GC, staleness, re-bootstrap support
//! and namespace purge.

use crate::apply::{Change, ChangeKind, vv_get};
use crate::error::StoreError;
use crate::keys::{self, OP_KEY_LEN};
use crate::read::scan_reg_winners;
use crate::tables::{self, ALL, META, OPS, OpRow, REG, RegRow, SUPERSEDED, meta};
use crate::{Store, meta_u64};
use mesh_hlc::WallClock;
use mesh_proto::{Payload, SignedOp};
use redb::{ReadableDatabase, ReadableTable, TableHandle, WriteTransaction};
use std::collections::HashSet;

/// How long an op must have been superseded (or stored invalid) before compaction may
/// delete it (§5.5). Retraction of a removed origin can restore ops younger than this.
pub const SETTLE_MS: u64 = 7 * 86_400_000;
/// Age (by the winner's HLC wall time) after which a winning tombstone is GC'd (§5.7).
pub const TOMBSTONE_TTL_MS: u64 = 90 * 86_400_000;
/// A store whose last full sync is older than this must re-bootstrap (§5.7).
pub const STALE_MS: u64 = 80 * 86_400_000;

/// `meta` rows kept by [`Store::reset_to_empty`].
const KEEP_ON_RESET: [&str; 5] = [
    meta::MESH,
    meta::NODE,
    meta::SCHEMA,
    meta::NEXT_SEQ,
    meta::LAST_HLC,
];

/// True if the `ops` row under `k` may be deleted by compaction: it exists, is not a
/// `SetAdd`, is at or below `vv[origin]` (so its absence is reported as `Covered`), and
/// is not its origin's current `reg` row for its key.
fn compactable(tx: &WriteTransaction, k: &[u8; OP_KEY_LEN]) -> Result<bool, StoreError> {
    let (origin, seq) = keys::split_op_key("ops", k)?;
    let row: OpRow = {
        let ops = tx.open_table(OPS)?;
        match ops.get(k.as_slice())? {
            Some(g) => tables::decode("ops", g.value())?,
            None => return Ok(false),
        }
    };
    let (ns, key) = match &row.op.body.payload {
        Payload::Set { ns, key, .. } => (ns, key),
        Payload::SetAdd { .. } => return Ok(false),
    };
    if seq > vv_get(tx, &origin)? {
        return Ok(false);
    }
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

impl<W: WallClock> Store<W> {
    /// Compaction (§5.5), in one write transaction. Deletes an `ops` row if it has been
    /// superseded for at least [`SETTLE_MS`], or was stored invalid at least
    /// [`SETTLE_MS`] ago, together with its `superseded` entry. Never deletes a `SetAdd`
    /// op, an op above its origin's vv, or an op that is its origin's current `reg` row.
    /// Peers asking for a deleted seq receive a `Covered` marker from `diff_since`.
    /// Returns the number of `ops` rows deleted.
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
            let ops = tx.open_table(OPS)?;
            for item in ops.iter()? {
                let (k, v) = item?;
                let row: OpRow = tables::decode("ops", v.value())?;
                if row.invalid && now.saturating_sub(row.received_ms) >= SETTLE_MS {
                    let (origin, seq) = keys::split_op_key("ops", k.value())?;
                    candidates.push(keys::op_key(&origin, seq));
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
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

    /// Tombstone GC (§5.7), in one write transaction. For every key whose winner (over
    /// all origins) is a tombstone with `hlc.ms() < now - TOMBSTONE_TTL_MS`, deletes
    /// **all** of the key's `reg` rows (deleting only the tombstone would bring back an
    /// older value of another origin) and every valid `ops` row of the key with its
    /// `superseded` entry — including superseded ops compaction has not removed yet, so
    /// a fresh peer catching up from this store can't resurrect an old value. Invalid ops
    /// are left to compaction. Set rows are untouched. Publishes one `Change` per GC'd key
    /// after commit (`local: false`: nothing to broadcast). Returns the number of keys.
    pub fn gc(&self) -> Result<usize, StoreError> {
        let clock = self.lock_clock();
        let cutoff = clock.wall().now_ms().saturating_sub(TOMBSTONE_TTL_MS);
        let tx = self.db.begin_write()?;

        // `ns ‖ 0 ‖ key ‖ 0` of each doomed key, plus its change event.
        let mut groups: HashSet<Vec<u8>> = HashSet::new();
        let mut changes = Vec::new();
        {
            let reg = tx.open_table(REG)?;
            scan_reg_winners(&reg, &[], &[], |ns, key, _, row| {
                if row.value.is_none() && row.hlc.ms() < cutoff {
                    let mut g = Vec::with_capacity(ns.len() + key.len() + 2);
                    g.extend_from_slice(ns);
                    g.push(keys::SEP);
                    g.extend_from_slice(key);
                    g.push(keys::SEP);
                    groups.insert(g);
                    changes.push(Change {
                        ns: utf8(ns)?,
                        kind: ChangeKind::Key(utf8(key)?),
                        local: false,
                    });
                }
                Ok(true)
            })?;
        }
        if groups.is_empty() {
            tx.abort()?;
            return Ok(0);
        }

        // Every reg row of each doomed key (all origins).
        let mut reg_rows: Vec<Vec<u8>> = Vec::new();
        {
            let reg = tx.open_table(REG)?;
            for g in &groups {
                for item in reg.range::<&[u8]>(g.as_slice()..)? {
                    let (k, _) = item?;
                    if !k.value().starts_with(g) {
                        break;
                    }
                    reg_rows.push(k.value().to_vec());
                }
            }
        }
        // Every valid op of each doomed key (the reg rows' ops and superseded ones).
        let mut doomed_ops: Vec<[u8; OP_KEY_LEN]> = Vec::new();
        {
            let ops = tx.open_table(OPS)?;
            for item in ops.iter()? {
                let (k, v) = item?;
                let row: OpRow = tables::decode("ops", v.value())?;
                if row.invalid {
                    continue;
                }
                if let Payload::Set { ns, key, .. } = &row.op.body.payload
                    && groups.contains(&keys::item_prefix(ns, key))
                {
                    let (origin, seq) = keys::split_op_key("ops", k.value())?;
                    doomed_ops.push(keys::op_key(&origin, seq));
                }
            }
        }
        {
            let mut reg = tx.open_table(REG)?;
            for k in &reg_rows {
                reg.remove(k.as_slice())?;
            }
        }
        delete_ops(&tx, &doomed_ops)?;
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

    /// This node's own ops received (written) after the last full sync — all of them if
    /// it never synced — in seq order. Re-bootstrap re-applies them after the catch-up.
    /// Own ops already compacted (superseded) are not returned.
    pub fn export_unsynced_local_ops(&self) -> Result<Vec<SignedOp>, StoreError> {
        let tx = self.db.begin_read()?;
        let since = meta_u64(&tx.open_table(META)?, meta::LAST_FULL_SYNC_MS)?;
        let ops = tx.open_table(OPS)?;
        let lo = keys::op_key(&self.node, 0);
        let hi = keys::op_key(&self.node, u64::MAX);
        let mut out = Vec::new();
        for item in ops.range(lo.as_slice()..=hi.as_slice())? {
            let (_, v) = item?;
            let row: OpRow = tables::decode("ops", v.value())?;
            if since.is_none_or(|t| row.received_ms > t) {
                out.push(row.op);
            }
        }
        Ok(out)
    }

    /// Clears every table except `meta`, in one write transaction. `meta` keeps the mesh
    /// and node ids, schema, `next_seq` and `last_hlc` (so seqs and HLCs never repeat);
    /// `last_full_sync_ms` is removed because the store is now empty. Publishes nothing.
    pub fn reset_to_empty(&self) -> Result<(), StoreError> {
        let _clock = self.lock_clock();
        let tx = self.db.begin_write()?;
        for def in ALL {
            if def.name() != META.name() {
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

    /// Writes a local tombstone (via [`Store::delete`], so each gets its own seq and op)
    /// for every live key of `ns`, and returns the ops in key order for broadcasting.
    /// Not atomic: keys are listed first, then deleted one by one.
    ///
    /// Set members are untouched: sets are grow-only, so they survive a purge until the
    /// namespace itself is tombstoned in `_mesh` (sub-spec 3). A `ns` containing NUL has
    /// no keys, so nothing is written.
    pub fn purge_namespace(&self, ns: &str) -> Result<Vec<SignedOp>, StoreError> {
        const PAGE: u32 = 1024;
        let mut live: Vec<String> = Vec::new();
        loop {
            let page = self.list_keys(ns, "", live.last().map(String::as_str), PAGE)?;
            let done = page.len() < PAGE as usize;
            live.extend(page);
            if done {
                break;
            }
        }
        live.into_iter().map(|k| self.delete(ns, &k)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::{Acceptor, Outcome, ReadView, Verdict};
    use crate::tables::REMOVED;
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
}
