//! Retracting a removed origin (§5.6): `on_removal(origin, cutoff_seq)`.
//!
//! The `removed` table holds each removed origin's effective cutoff, the highest one seen.
//! A first removal retracts the origin's ops above the cutoff and restores, per key, that
//! origin's best remaining op at or below it. A raised cutoff re-merges the ops in
//! `(old, new]` that are still stored. An equal or lower cutoff changes nothing.

use crate::apply::{Change, ChangeKind, removed_get, vv_get};
use crate::error::StoreError;
use crate::keys::{self, OP_KEY_LEN};
use crate::tables::{self, OPS, OpRow, REG, REMOVED, RegRow, SET, SUPERSEDED, SetRow};
use crate::{Store, merge};
use mesh_hlc::WallClock;
use mesh_proto::{NodeId, Payload};
use redb::{ReadableTable, WriteTransaction};
use std::collections::{BTreeMap, BTreeSet};

/// The set holding removal records: `(ns, set)`. Its adds are never retracted by a
/// removal cutoff, even when their issuer is itself removed (node-core spec §2.3).
pub const CUTOFF_EXEMPT_SET: (&str, &str) = ("_mesh", "removed");

/// True for a removal record (`set_add` into [`CUTOFF_EXEMPT_SET`]): exempt from cutoffs.
pub(crate) fn is_cutoff_exempt(p: &Payload) -> bool {
    matches!(p, Payload::SetAdd { ns, set, .. }
        if (ns.as_str(), set.as_str()) == CUTOFF_EXEMPT_SET)
}

/// What [`Store::on_removal`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemovalReport {
    /// Ops of the origin newly marked invalid (first removal only).
    pub retracted_ops: usize,
    /// Register keys whose row for the origin now holds a different op: an older value
    /// restored by a first removal, or a re-validated op merged by a raised cutoff.
    pub restored_keys: usize,
    /// `(ns, key)` whose row for the origin was deleted while some of the origin's ops at
    /// or below the cutoff were already compacted, so an older value may have been lost
    /// (§5.6 "remaining limit"). Each is also logged with `tracing::warn!`.
    pub unrestorable_keys: Vec<(String, String)>,
}

fn utf8(table: &'static str, b: &[u8]) -> Result<String, StoreError> {
    String::from_utf8(b.to_vec()).map_err(|_| StoreError::corrupt(table, "name is not UTF-8"))
}

/// A `reg` row of the removed origin above the cutoff, and its replacement (if any).
struct RegFix {
    ns: String,
    key: String,
    best: Option<(u64, RegRow)>,
}

/// A `set` row of the removed origin above the cutoff, and the origin's lowest valid add
/// of the same member at or below the cutoff (if any).
struct SetFix {
    ns: String,
    set: String,
    keep: Option<SetRow>,
}

/// `ops` keys of `origin` with seq in `lo..=hi`.
fn op_range(origin: &NodeId, lo: u64, hi: u64) -> ([u8; OP_KEY_LEN], [u8; OP_KEY_LEN]) {
    (keys::op_key(origin, lo), keys::op_key(origin, hi))
}

/// First removal of `origin` with `cutoff` (§5.6 steps 1–3). Returns the report and the
/// changes to publish.
fn retract(
    tx: &WriteTransaction,
    origin: &NodeId,
    cutoff: u64,
    now_ms: u64,
) -> Result<(RemovalReport, Vec<Change>), StoreError> {
    let mut report = RemovalReport::default();
    let now = keys::u64_value(now_ms);

    // 1. Mark every op above the cutoff invalid; compaction ages it from now (R9).
    if let Some(lo) = cutoff.checked_add(1) {
        let (lo, hi) = op_range(origin, lo, u64::MAX);
        // Keys first, then rows one at a time (values can be up to 1 MiB each).
        let ks: Vec<Vec<u8>> = {
            let ops = tx.open_table(OPS)?;
            ops.range::<&[u8]>(lo.as_slice()..=hi.as_slice())?
                .map(|item| item.map(|(k, _)| k.value().to_vec()))
                .collect::<Result<_, _>>()?
        };
        let mut ops = tx.open_table(OPS)?;
        let mut sup = tx.open_table(SUPERSEDED)?;
        let mut marked = 0;
        for k in ks {
            let mut row: OpRow = match ops.get(k.as_slice())? {
                Some(g) => tables::decode("ops", g.value())?,
                None => continue,
            };
            if row.invalid || is_cutoff_exempt(&row.op.body.payload) {
                continue;
            }
            row.invalid = true;
            ops.insert(k.as_slice(), tables::encode("ops", &row)?.as_slice())?;
            sup.insert(k.as_slice(), now.as_slice())?;
            marked += 1;
        }
        report.retracted_ops = marked;
    }

    // 2–3. The origin's `reg` and `set` rows above the cutoff (keyed by their table key).
    let mut reg_fix: BTreeMap<Vec<u8>, RegFix> = BTreeMap::new();
    {
        let reg = tx.open_table(REG)?;
        for item in reg.iter()? {
            let (k, v) = item?;
            let rk = keys::split_reg_key(k.value())?;
            if rk.origin != *origin {
                continue;
            }
            let row: RegRow = tables::decode("reg", v.value())?;
            if row.seq > cutoff {
                reg_fix.insert(
                    k.value().to_vec(),
                    RegFix {
                        ns: utf8("reg", rk.ns)?,
                        key: utf8("reg", rk.key)?,
                        best: None,
                    },
                );
            }
        }
    }
    let mut set_fix: BTreeMap<Vec<u8>, SetFix> = BTreeMap::new();
    {
        let set = tx.open_table(SET)?;
        for item in set.iter()? {
            let (k, v) = item?;
            let sk = keys::split_set_key(k.value())?;
            if sk.origin != *origin {
                continue;
            }
            let row: SetRow = tables::decode("set", v.value())?;
            if row.seq > cutoff {
                // `group` is `ns ‖ 0 ‖ set ‖ 0`.
                let name = sk
                    .group
                    .get(sk.ns.len() + 1..sk.group.len() - 1)
                    .ok_or_else(|| StoreError::corrupt("set", "malformed key"))?;
                // Removal records are never retracted.
                let (ens, eset) = CUTOFF_EXEMPT_SET;
                if sk.ns == ens.as_bytes() && name == eset.as_bytes() {
                    continue;
                }
                set_fix.insert(
                    k.value().to_vec(),
                    SetFix {
                        ns: utf8("set", sk.ns)?,
                        set: utf8("set", name)?,
                        keep: None,
                    },
                );
            }
        }
    }

    // The origin's best remaining valid op per affected key / member, and whether any
    // of its seqs in `1..=min(cutoff, vv)` is missing from `ops` (compacted or GC'd).
    let bound = cutoff.min(vv_get(tx, origin)?);
    let mut present: u64 = 0;
    if cutoff >= 1 {
        let (lo, hi) = op_range(origin, 1, cutoff);
        let ops = tx.open_table(OPS)?;
        for item in ops.range::<&[u8]>(lo.as_slice()..=hi.as_slice())? {
            let (_, v) = item?;
            let row: OpRow = tables::decode("ops", v.value())?;
            let b = &row.op.body;
            if b.seq <= bound {
                present += 1;
            }
            if row.invalid {
                continue;
            }
            match &b.payload {
                Payload::Set {
                    ns,
                    key,
                    order,
                    value,
                } => {
                    let Some(fix) = reg_fix.get_mut(&keys::reg_key(ns, key, origin)) else {
                        continue;
                    };
                    let better = fix.best.as_ref().is_none_or(|(_, cur)| {
                        merge::beats((order, b.hlc, *origin), (&cur.order, cur.hlc, *origin))
                    });
                    if better {
                        let row = RegRow {
                            order: order.clone(),
                            hlc: b.hlc,
                            seq: b.seq,
                            value: value.clone(),
                        };
                        fix.best = Some((b.seq, row));
                    }
                }
                Payload::SetAdd { ns, set, member } => {
                    let Some(fix) = set_fix.get_mut(&keys::set_key(ns, set, member, origin)) else {
                        continue;
                    };
                    // Ascending seq order: the first one found is the earliest add.
                    if fix.keep.is_none() {
                        fix.keep = Some(SetRow {
                            hlc: b.hlc,
                            seq: b.seq,
                        });
                    }
                }
            }
        }
    }
    let missing = present < bound;

    let mut changes = Vec::new();
    {
        let mut reg = tx.open_table(REG)?;
        let mut sup = tx.open_table(SUPERSEDED)?;
        for (k, fix) in reg_fix {
            match fix.best {
                Some((seq, row)) => {
                    reg.insert(k.as_slice(), tables::encode("reg", &row)?.as_slice())?;
                    // The new winner must not be compacted away.
                    sup.remove(keys::op_key(origin, seq).as_slice())?;
                    report.restored_keys += 1;
                }
                None => {
                    reg.remove(k.as_slice())?;
                    if missing {
                        tracing::warn!(
                            origin = %origin.short(),
                            cutoff,
                            "retraction could not restore {}/{}",
                            fix.ns,
                            fix.key
                        );
                        report
                            .unrestorable_keys
                            .push((fix.ns.clone(), fix.key.clone()));
                    }
                }
            }
            changes.push(Change {
                ns: fix.ns,
                kind: ChangeKind::Key(fix.key),
                local: false,
            });
        }
    }
    {
        let mut set = tx.open_table(SET)?;
        let mut changed_sets: BTreeSet<(String, String)> = BTreeSet::new();
        for (k, fix) in set_fix {
            match fix.keep {
                Some(row) => {
                    set.insert(k.as_slice(), tables::encode("set", &row)?.as_slice())?;
                }
                None => {
                    set.remove(k.as_slice())?;
                    changed_sets.insert((fix.ns, fix.set));
                }
            }
        }
        changes.extend(changed_sets.into_iter().map(|(ns, set)| Change {
            ns,
            kind: ChangeKind::Set(set),
            local: false,
        }));
    }
    Ok((report, changes))
}

/// A raised cutoff `old → new`: ops of `origin` in `(old, new]` still in `ops` lose their
/// invalid mark and `superseded` entry and are merged again (§5.4), in seq order.
fn revalidate(
    tx: &WriteTransaction,
    origin: &NodeId,
    old: u64,
    new: u64,
    now_ms: u64,
) -> Result<(RemovalReport, Vec<Change>), StoreError> {
    let mut report = RemovalReport::default();
    let Some(lo) = old.checked_add(1) else {
        return Ok((report, Vec::new()));
    };
    let (lo, hi) = op_range(origin, lo, new);
    // Keys only: rows are loaded one at a time (values can be up to 1 MiB each).
    let ks: Vec<Vec<u8>> = {
        let ops = tx.open_table(OPS)?;
        ops.range::<&[u8]>(lo.as_slice()..=hi.as_slice())?
            .map(|item| item.map(|(k, _)| k.value().to_vec()))
            .collect::<Result<_, _>>()?
    };
    // `(ns, is_set, name)` of every changed key or set, deduplicated.
    let mut changed: BTreeSet<(String, bool, String)> = BTreeSet::new();
    for k in ks {
        let row = {
            let mut ops = tx.open_table(OPS)?;
            let mut row: OpRow = match ops.get(k.as_slice())? {
                Some(g) => tables::decode("ops", g.value())?,
                None => continue,
            };
            if !row.invalid || is_cutoff_exempt(&row.op.body.payload) {
                continue;
            }
            row.invalid = false;
            ops.insert(k.as_slice(), tables::encode("ops", &row)?.as_slice())?;
            tx.open_table(SUPERSEDED)?.remove(k.as_slice())?;
            row
        };
        // A losing op goes back into `superseded` inside `merge_op`.
        if merge::merge_op(tx, &row.op, now_ms)? {
            changed.insert(match &row.op.body.payload {
                Payload::Set { ns, key, .. } => (ns.clone(), false, key.clone()),
                Payload::SetAdd { ns, set, .. } => (ns.clone(), true, set.clone()),
            });
        }
    }
    report.restored_keys = changed.iter().filter(|(_, is_set, _)| !is_set).count();
    let changes = changed
        .into_iter()
        .map(|(ns, is_set, name)| Change {
            ns,
            kind: if is_set {
                ChangeKind::Set(name)
            } else {
                ChangeKind::Key(name)
            },
            local: false,
        })
        .collect();
    Ok((report, changes))
}

impl<W: WallClock> Store<W> {
    /// Applies a removal of `origin` with `cutoff_seq` (§5.6), in one write transaction:
    /// its ops with `seq > cutoff_seq` are invalid everywhere. The effective cutoff is
    /// the highest one seen (`removed[origin] = max(existing, cutoff_seq)`):
    ///
    /// - **first removal**: marks the origin's ops above the cutoff invalid, replaces each
    ///   of its `reg` rows above the cutoff with its best remaining valid op at or below
    ///   it (or deletes the row; see [`RemovalReport::unrestorable_keys`]) and deletes
    ///   its `set` rows above the cutoff unless it has a valid add of the member at or
    ///   below it;
    /// - **raised cutoff**: the origin's stored ops in `(old, new]` become valid and are
    ///   merged again;
    /// - **equal or lower cutoff**: nothing (an all-zero report, no commit).
    ///
    /// Removal records (set rows in ns `_mesh`, set `removed`, see [`CUTOFF_EXEMPT_SET`])
    /// are exempt: they are never marked invalid and their set rows are never deleted,
    /// even when their issuer is the removed origin.
    ///
    /// The version vector is not changed. `Change` events (`local: false`) are published
    /// after commit for every affected key and set.
    pub fn on_removal(&self, origin: NodeId, cutoff_seq: u64) -> Result<RemovalReport, StoreError> {
        let clock = self.lock_clock();
        let now_ms = clock.wall().now_ms();
        let tx = self.db.begin_write()?;
        let old = removed_get(&tx, &origin)?;
        if old.is_some_and(|old| old >= cutoff_seq) {
            tx.abort()?;
            return Ok(RemovalReport::default());
        }
        tx.open_table(REMOVED)?.insert(
            keys::origin_key(&origin).as_slice(),
            keys::u64_value(cutoff_seq).as_slice(),
        )?;
        let (report, changes) = match old {
            None => retract(&tx, &origin, cutoff_seq, now_ms)?,
            Some(old) => revalidate(&tx, &origin, old, cutoff_seq, now_ms)?,
        };
        tx.commit()?;
        // Published under the clock lock, like `apply`, so events stay in commit order.
        self.publish(changes);
        drop(clock);
        Ok(report)
    }
}
