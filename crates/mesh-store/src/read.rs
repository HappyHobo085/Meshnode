//! Reads: registers, keys, sets, namespace sizes.

use crate::error::StoreError;
use crate::keys::{self, ORIGIN_LEN, SEP};
use crate::tables::{self, REG, RegRow, SET};
use crate::{Store, merge};
use mesh_hlc::{Hlc, WallClock};
use mesh_proto::NodeId;
use redb::{ReadableDatabase, ReadableTable};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// The winning register of a key, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegEntry {
    pub order: Vec<u8>,
    pub hlc: Hlc,
    pub origin: NodeId,
    pub seq: u64,
    /// `None` is a tombstone.
    pub value: Option<Vec<u8>>,
}

fn has_nul(s: &str) -> bool {
    s.as_bytes().contains(&SEP)
}

fn utf8(table: &'static str, b: &[u8]) -> Result<String, StoreError> {
    String::from_utf8(b.to_vec()).map_err(|_| StoreError::corrupt(table, "name is not UTF-8"))
}

/// Calls `f(ns, key, winner_origin, winner_row)` for each key whose `reg` rows lie in
/// `[start, ..)` and start with `within`, in key order. Stops early when `f` returns `false`.
/// `start` must be at a key boundary (a key's rows are never split by it).
pub(crate) fn scan_reg_winners<T, F>(
    reg: &T,
    start: &[u8],
    within: &[u8],
    mut f: F,
) -> Result<(), StoreError>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
    F: FnMut(&[u8], &[u8], NodeId, RegRow) -> Result<bool, StoreError>,
{
    /// Best row so far for the key whose rows are being scanned.
    struct Best {
        /// `ns ‖ 0 ‖ key ‖ 0`.
        group: Vec<u8>,
        ns_len: usize,
        origin: NodeId,
        row: RegRow,
    }
    fn emit<F>(b: Best, f: &mut F) -> Result<bool, StoreError>
    where
        F: FnMut(&[u8], &[u8], NodeId, RegRow) -> Result<bool, StoreError>,
    {
        let ns = &b.group[..b.ns_len];
        let key = &b.group[b.ns_len + 1..b.group.len() - 1];
        f(ns, key, b.origin, b.row)
    }

    let mut cur: Option<Best> = None;
    for item in reg.range::<&[u8]>(start..)? {
        let (k, v) = item?;
        let k = k.value();
        if !k.starts_with(within) {
            break;
        }
        let rk = keys::split_reg_key(k)?;
        let row: RegRow = tables::decode("reg", v.value())?;
        match &mut cur {
            Some(b) if b.group.as_slice() == rk.group => {
                if merge::beats(
                    (&row.order, row.hlc, rk.origin),
                    (&b.row.order, b.row.hlc, b.origin),
                ) {
                    b.origin = rk.origin;
                    b.row = row;
                }
            }
            _ => {
                let next = Best {
                    group: rk.group.to_vec(),
                    ns_len: rk.ns.len(),
                    origin: rk.origin,
                    row,
                };
                if let Some(done) = cur.replace(next) {
                    if !emit(done, &mut f)? {
                        return Ok(());
                    }
                }
            }
        }
    }
    if let Some(done) = cur {
        emit(done, &mut f)?;
    }
    Ok(())
}

impl<W: WallClock> Store<W> {
    /// The live value of `ns/key`, or `None` if absent or deleted.
    pub fn get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.get_entry(ns, key)?.and_then(|e| e.value))
    }

    /// The winning register of `ns/key` (including tombstones), or `None` if never written.
    pub fn get_entry(&self, ns: &str, key: &str) -> Result<Option<RegEntry>, StoreError> {
        if has_nul(ns) || has_nul(key) {
            return Ok(None);
        }
        let tx = self.db.begin_read()?;
        let reg = tx.open_table(REG)?;
        let prefix = keys::item_prefix(ns, key);
        let mut rows = Vec::new();
        for item in reg.range::<&[u8]>(prefix.as_slice()..)? {
            let (k, v) = item?;
            let k = k.value();
            if !k.starts_with(&prefix) {
                break;
            }
            let origin = keys::split_reg_key(k)?.origin;
            rows.push((origin, tables::decode::<RegRow>("reg", v.value())?));
        }
        Ok(merge::winner(rows).map(|(origin, r)| RegEntry {
            order: r.order,
            hlc: r.hlc,
            origin,
            seq: r.seq,
            value: r.value,
        }))
    }

    /// Live keys of `ns` starting with `prefix`, sorted, strictly after `start_after`
    /// (which need not exist), at most `limit`.
    pub fn list_keys(
        &self,
        ns: &str,
        prefix: &str,
        start_after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<String>, StoreError> {
        let mut out = Vec::new();
        if limit == 0 || has_nul(ns) || has_nul(prefix) {
            return Ok(out);
        }
        let mut within = keys::ns_prefix(ns);
        within.extend_from_slice(prefix.as_bytes());
        let mut start = within.clone();
        if let Some(after) = start_after {
            // Stored keys contain no NUL, so "after `x\0…`" is the same as "after `x`".
            let after = after.split('\0').next().unwrap_or("");
            // Every row of a key > `after` sorts at or above `ns ‖ 0 ‖ after ‖ 0x01`,
            // and every row of `after` itself (`ns ‖ 0 ‖ after ‖ 0 ‖ origin`) below it.
            let mut s = keys::ns_prefix(ns);
            s.extend_from_slice(after.as_bytes());
            s.push(1);
            start = start.max(s);
        }
        let reg = self.db.begin_read()?.open_table(REG)?;
        scan_reg_winners(&reg, &start, &within, |_, key, _, row| {
            if row.value.is_some() {
                out.push(utf8("reg", key)?);
            }
            Ok(out.len() < limit as usize)
        })?;
        Ok(out)
    }

    /// True if `member` is in the add-only set `ns/set`.
    pub fn set_contains(&self, ns: &str, set: &str, member: &[u8]) -> Result<bool, StoreError> {
        if has_nul(ns) || has_nul(set) {
            return Ok(false);
        }
        let mut p = keys::item_prefix(ns, set);
        p.extend_from_slice(member);
        let table = self.db.begin_read()?.open_table(SET)?;
        for item in table.range::<&[u8]>(p.as_slice()..)? {
            let (k, _) = item?;
            let k = k.value();
            if !k.starts_with(&p) {
                break;
            }
            // Longer members extending `member` share the prefix; only an exact-length row matches.
            if k.len() == p.len() + ORIGIN_LEN {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Members of `ns/set`, byte-sorted, strictly after `start_after`, at most `limit`.
    ///
    /// Rows are keyed `member ‖ origin`, so a member's rows can sort after a longer member
    /// that extends it; this scans every row of the set at or above `start_after` and keeps
    /// the `limit` smallest members.
    pub fn set_members(
        &self,
        ns: &str,
        set: &str,
        start_after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        if limit == 0 || has_nul(ns) || has_nul(set) {
            return Ok(Vec::new());
        }
        let prefix = keys::item_prefix(ns, set);
        let mut start = prefix.clone();
        if let Some(after) = start_after {
            // Every row of a member > `after` sorts at or above `prefix ‖ after`.
            start.extend_from_slice(after);
        }
        let table = self.db.begin_read()?.open_table(SET)?;
        let mut best: BTreeSet<Vec<u8>> = BTreeSet::new();
        for item in table.range::<&[u8]>(start.as_slice()..)? {
            let (k, _) = item?;
            let k = k.value();
            if !k.starts_with(&prefix) {
                break;
            }
            let member = keys::split_set_key(k)?.member;
            if start_after.is_some_and(|a| member <= a) {
                continue;
            }
            if best.len() == limit as usize
                && best.last().is_some_and(|max| member >= max.as_slice())
            {
                continue;
            }
            best.insert(member.to_vec());
            if best.len() > limit as usize {
                best.pop_last();
            }
        }
        Ok(best.into_iter().collect())
    }

    /// Per namespace: total bytes of live register values plus distinct set members.
    pub fn namespace_sizes(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        let tx = self.db.begin_read()?;
        let mut sizes: BTreeMap<String, u64> = BTreeMap::new();

        let reg = tx.open_table(REG)?;
        scan_reg_winners(&reg, &[], &[], |ns, _, _, row| {
            if let Some(v) = row.value {
                *sizes.entry(utf8("reg", ns)?).or_default() += v.len() as u64;
            }
            Ok(true)
        })?;

        let set = tx.open_table(SET)?;
        let mut group: Vec<u8> = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        for item in set.iter()? {
            let (k, _) = item?;
            let sk = keys::split_set_key(k.value())?;
            if sk.group != group.as_slice() {
                group = sk.group.to_vec();
                seen.clear();
            }
            if seen.insert(sk.member.to_vec()) {
                *sizes.entry(utf8("set", sk.ns)?).or_default() += sk.member.len() as u64;
            }
        }
        Ok(sizes)
    }
}
