//! Merge rule (§5.4): pure comparison helpers plus the in-transaction merge.

use crate::error::StoreError;
use crate::keys;
use crate::tables::{self, REG, RegRow, SET, SUPERSEDED, SetRow};
use mesh_hlc::Hlc;
use mesh_proto::{NodeId, Payload, SignedOp};
use redb::{ReadableTable, WriteTransaction};

/// Lexicographic `(order, hlc, origin)` comparison: true if `a > b`.
pub fn beats(a: (&[u8], Hlc, NodeId), b: (&[u8], Hlc, NodeId)) -> bool {
    a > b
}

/// The winning register among one key's `reg` rows (max by `(order, hlc, origin)`).
pub(crate) fn winner<I>(rows: I) -> Option<(NodeId, RegRow)>
where
    I: IntoIterator<Item = (NodeId, RegRow)>,
{
    let mut best: Option<(NodeId, RegRow)> = None;
    for (origin, row) in rows {
        let better = match &best {
            None => true,
            Some((bo, br)) => beats((&row.order, row.hlc, origin), (&br.order, br.hlc, *bo)),
        };
        if better {
            best = Some((origin, row));
        }
    }
    best
}

/// Merges `op` into `reg` / `set` inside `tx` (§5.4). Does **not** touch `ops`, `vv` or `meta`.
///
/// - `Set`: if `op` beats this origin's current `reg` row (by `(order, hlc)`), it replaces
///   it and the old `(origin, seq)` goes into `superseded` at `now_ms`; otherwise `op`
///   itself goes into `superseded` and `reg` is untouched.
/// - `SetAdd`: inserts the `set` row if absent (the first adder's row is kept).
///
/// Opens `reg`, `set` and `superseded` itself and closes them before returning, so the
/// caller must not hold any of those tables open. Returns true if a `reg`/`set` row changed.
pub(crate) fn merge_op(
    tx: &WriteTransaction,
    op: &SignedOp,
    now_ms: u64,
) -> Result<bool, StoreError> {
    let b = &op.body;
    match &b.payload {
        Payload::Set {
            ns,
            key,
            order,
            value,
        } => {
            let mut reg = tx.open_table(REG)?;
            let rk = keys::reg_key(ns, key, &b.origin);
            let old: Option<RegRow> = match reg.get(rk.as_slice())? {
                Some(g) => Some(tables::decode("reg", g.value())?),
                None => None,
            };
            let mut superseded = tx.open_table(SUPERSEDED)?;
            let now = keys::u64_value(now_ms);
            let loses = old.as_ref().is_some_and(|old| {
                !beats((order, b.hlc, b.origin), (&old.order, old.hlc, b.origin))
            });
            if loses {
                superseded.insert(keys::op_key(&b.origin, b.seq).as_slice(), now.as_slice())?;
                return Ok(false);
            }
            let row = RegRow {
                order: order.clone(),
                hlc: b.hlc,
                seq: b.seq,
                value: value.clone(),
            };
            reg.insert(rk.as_slice(), tables::encode("reg", &row)?.as_slice())?;
            if let Some(old) = old {
                superseded.insert(keys::op_key(&b.origin, old.seq).as_slice(), now.as_slice())?;
            }
            Ok(true)
        }
        Payload::SetAdd { ns, set, member } => {
            let mut table = tx.open_table(SET)?;
            let sk = keys::set_key(ns, set, member, &b.origin);
            if table.get(sk.as_slice())?.is_some() {
                return Ok(false);
            }
            let row = SetRow {
                hlc: b.hlc,
                seq: b.seq,
            };
            table.insert(sk.as_slice(), tables::encode("set", &row)?.as_slice())?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(order: &[u8], hlc: u64, seq: u64) -> RegRow {
        RegRow {
            order: order.to_vec(),
            hlc: Hlc::from_u64(hlc),
            seq,
            value: None,
        }
    }

    #[test]
    fn beats_is_lexicographic() {
        let (a, b) = (NodeId([1; 32]), NodeId([2; 32]));
        let h = Hlc::from_u64;
        assert!(beats((&[1], h(1), a), (&[], h(9), b)));
        assert!(beats((&[1, 0], h(1), a), (&[1], h(9), b)));
        assert!(beats((&[1], h(2), a), (&[1], h(1), b)));
        assert!(beats((&[1], h(1), b), (&[1], h(1), a)));
        assert!(!beats((&[1], h(1), a), (&[1], h(1), a)));
    }

    #[test]
    fn winner_is_max() {
        let (a, b, c) = (NodeId([1; 32]), NodeId([2; 32]), NodeId([3; 32]));
        let w = winner([
            (a, row(&[2], 1, 1)),
            (b, row(&[2], 5, 1)),
            (c, row(&[1], 9, 1)),
        ]);
        assert_eq!(w.map(|(o, _)| o), Some(b));
        assert!(winner(Vec::new()).is_none());
    }
}
