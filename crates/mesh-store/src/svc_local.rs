//! `svc_local`: node-local service entries (`meshnode:kv/local`). Never replicated: they
//! create no op, touch neither the op log nor the version vector, are not in catch-up,
//! and survive [`Store::reset_to_empty`].
//!
//! Rows: `ns \0 key` -> `expiry_ms` (u64 big-endian, 0 = none) `++ value`. An entry is
//! live while `expiry_ms == 0 || now < expiry_ms`; expired entries are invisible on read
//! and deleted by [`Store::local_sweep`].
//!
//! `svc_local_count`: `ns` -> `count ++ due` (two u64 big-endian). `count` is the number of
//! rows of `ns` (live or not yet swept). `due` is 0 when no row of `ns` can expire, else a
//! lower bound on the earliest expiry among its rows: nothing in `ns` has expired before
//! `due`. So a new key at the cap is refused in O(1) unless `now >= due`, and only then is
//! the namespace scanned (which frees a slot or makes `due` exact again).
//!
//! "Now" is read from the store's wall clock directly, never through the HLC mutex, so
//! local reads do not wait behind replicated commits.

use std::collections::BTreeMap;

use crate::error::StoreError;
use crate::tables::{SVC_LOCAL, SVC_LOCAL_COUNT};
use crate::{Store, keys};
use mesh_hlc::WallClock;
use mesh_proto::ProtoError;
use redb::{ReadableDatabase, ReadableTable, Table};

/// The largest local value, in bytes.
pub const LOCAL_MAX_VALUE: usize = 64 * 1024;
/// The most live local keys one namespace may hold.
pub const LOCAL_MAX_KEYS: u64 = 100_000;
/// The longest local key, in bytes (as for replicated keys).
pub const LOCAL_MAX_KEY: usize = 512;
/// The longest namespace, in bytes (as for replicated namespaces).
const MAX_NS: usize = 64;

type T<'t> = Table<'t, &'static [u8], &'static [u8]>;

fn check_ns(ns: &str) -> Result<(), StoreError> {
    if ns.len() > MAX_NS {
        return Err(ProtoError::Limit("ns").into());
    }
    if ns.as_bytes().contains(&0) {
        return Err(ProtoError::Nul("ns").into());
    }
    Ok(())
}

/// `ns \0 key`.
fn row_key(ns: &str, key: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(ns.len() + 1 + key.len());
    k.extend_from_slice(ns.as_bytes());
    k.push(0);
    k.extend_from_slice(key.as_bytes());
    k
}

/// The key range of namespace `ns`: `[ns \0, ns \1)`.
fn ns_range(ns: &str) -> (Vec<u8>, Vec<u8>) {
    let mut lo = ns.as_bytes().to_vec();
    let mut hi = lo.clone();
    lo.push(0);
    hi.push(1);
    (lo, hi)
}

/// The namespace part of a row key.
fn ns_of(k: &[u8]) -> Result<&[u8], StoreError> {
    k.iter()
        .position(|b| *b == 0)
        .map(|i| &k[..i])
        .ok_or_else(|| StoreError::corrupt("svc_local", "row key without a separator"))
}

/// `(expiry_ms, value)` of a row value.
fn split_row(v: &[u8]) -> Result<(u64, &[u8]), StoreError> {
    if v.len() < 8 {
        return Err(StoreError::corrupt(
            "svc_local",
            "row shorter than its expiry",
        ));
    }
    let (e, value) = v.split_at(8);
    Ok((keys::read_u64_value("svc_local", e)?, value))
}

fn live(expiry_ms: u64, now_ms: u64) -> bool {
    expiry_ms == 0 || now_ms < expiry_ms
}

/// The earlier of two expiries, where 0 means "none".
fn earliest(a: u64, b: u64) -> u64 {
    match (a, b) {
        (0, x) | (x, 0) => x,
        (a, b) => a.min(b),
    }
}

/// A `svc_local_count` row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NsMeta {
    count: u64,
    /// 0: no row can expire; else no row expires before this.
    due: u64,
}

fn meta_of(
    c: &impl ReadableTable<&'static [u8], &'static [u8]>,
    ns: &[u8],
) -> Result<NsMeta, StoreError> {
    let Some(g) = c.get(ns)? else {
        return Ok(NsMeta::default());
    };
    let v = g.value();
    if v.len() != 16 {
        return Err(StoreError::corrupt(
            "svc_local_count",
            format!("row of {} bytes, expected 16", v.len()),
        ));
    }
    Ok(NsMeta {
        count: keys::read_u64_value("svc_local_count", &v[..8])?,
        due: keys::read_u64_value("svc_local_count", &v[8..])?,
    })
}

fn set_meta(c: &mut T<'_>, ns: &[u8], m: NsMeta) -> Result<(), StoreError> {
    if m.count == 0 {
        c.remove(ns)?;
    } else {
        let mut v = [0u8; 16];
        v[..8].copy_from_slice(&keys::u64_value(m.count));
        v[8..].copy_from_slice(&keys::u64_value(m.due));
        c.insert(ns, v.as_slice())?;
    }
    Ok(())
}

/// What a [`sweep`] did.
struct Swept {
    removed: usize,
    /// Rows read.
    visited: u64,
    /// Whether any row or count row changed (worth a commit).
    changed: bool,
}

/// Deletes the expired rows of `ns` (of every namespace when `ns` is `None`), and sets
/// each scanned namespace's count and an exact `due` (the earliest surviving expiry).
fn sweep(t: &mut T<'_>, c: &mut T<'_>, ns: Option<&str>, now_ms: u64) -> Result<Swept, StoreError> {
    let mut doomed = Vec::new();
    // Per namespace: (rows removed, earliest surviving expiry).
    let mut seen: BTreeMap<Vec<u8>, (u64, u64)> = BTreeMap::new();
    let mut visited = 0;
    {
        let iter = match ns {
            Some(ns) => {
                let (lo, hi) = ns_range(ns);
                t.range(lo.as_slice()..hi.as_slice())?
            }
            None => t.iter()?,
        };
        for item in iter {
            let (k, v) = item?;
            visited += 1;
            let (expiry, _) = split_row(v.value())?;
            let entry = seen.entry(ns_of(k.value())?.to_vec()).or_default();
            if live(expiry, now_ms) {
                entry.1 = earliest(entry.1, expiry);
            } else {
                entry.0 += 1;
                doomed.push(k.value().to_vec());
            }
        }
    }
    for k in &doomed {
        t.remove(k.as_slice())?;
    }
    let mut changed = !doomed.is_empty();
    for (ns, (removed, due)) in seen {
        let old = meta_of(c, &ns)?;
        let new = NsMeta {
            count: old.count.saturating_sub(removed),
            due,
        };
        if new != old {
            set_meta(c, &ns, new)?;
            changed = true;
        }
    }
    Ok(Swept {
        removed: doomed.len(),
        visited,
        changed,
    })
}

impl<W: WallClock> Store<W> {
    /// The store's wall clock, the clock local expiries are measured with (read without
    /// the HLC mutex).
    fn local_now_ms(&self) -> u64 {
        self.wall.now_ms()
    }

    #[cfg(feature = "test-hooks")]
    fn note_scanned(&self, rows: u64) {
        self.local_scanned
            .fetch_add(rows, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(not(feature = "test-hooks"))]
    fn note_scanned(&self, _rows: u64) {}

    /// Rows the local sweeps of this store have read so far (test-hooks only).
    #[cfg(feature = "test-hooks")]
    pub fn local_rows_scanned(&self) -> u64 {
        self.local_scanned
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The live value of `ns/key`, if any.
    pub fn local_get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        check_ns(ns)?;
        if key.len() > LOCAL_MAX_KEY {
            return Ok(None);
        }
        let now = self.local_now_ms();
        let tx = self.db.begin_read()?;
        let t = tx.open_table(SVC_LOCAL)?;
        let Some(g) = t.get(row_key(ns, key).as_slice())? else {
            return Ok(None);
        };
        let (expiry, value) = split_row(g.value())?;
        Ok(live(expiry, now).then(|| value.to_vec()))
    }

    /// Sets `ns/key` to `value`, expiring `ttl_ms` from now (0 = never); an overwrite
    /// replaces the expiry. The inner `Err` is a cap breach (key over [`LOCAL_MAX_KEY`]
    /// bytes, value over [`LOCAL_MAX_VALUE`] bytes, or a new key when `ns` already holds
    /// [`LOCAL_MAX_KEYS`] live keys); the entry is not written then. A refusal costs no
    /// scan unless some entry of `ns` may have expired.
    pub fn local_set(
        &self,
        ns: &str,
        key: &str,
        value: &[u8],
        ttl_ms: u64,
    ) -> Result<Result<(), String>, StoreError> {
        check_ns(ns)?;
        if key.len() > LOCAL_MAX_KEY {
            return Ok(Err(format!(
                "key of {} bytes exceeds {LOCAL_MAX_KEY}",
                key.len()
            )));
        }
        if value.len() > LOCAL_MAX_VALUE {
            return Ok(Err(format!(
                "value of {} bytes exceeds {LOCAL_MAX_VALUE}",
                value.len()
            )));
        }
        let now = self.local_now_ms();
        let expiry = match ttl_ms {
            0 => 0,
            ttl => now.saturating_add(ttl),
        };
        let mut row = Vec::with_capacity(8 + value.len());
        row.extend_from_slice(&keys::u64_value(expiry));
        row.extend_from_slice(value);
        let k = row_key(ns, key);

        let tx = self.db.begin_write()?;
        let refused = {
            let mut t = tx.open_table(SVC_LOCAL)?;
            let mut c = tx.open_table(SVC_LOCAL_COUNT)?;
            let mut m = meta_of(&c, ns.as_bytes())?;
            let is_new = t.get(k.as_slice())?.is_none();
            let mut swept = false;
            if is_new && m.count >= LOCAL_MAX_KEYS && m.due != 0 && now >= m.due {
                // Something may have expired: expired rows do not count, so drop them.
                let s = sweep(&mut t, &mut c, Some(ns), now)?;
                self.note_scanned(s.visited);
                swept = s.changed;
                m = meta_of(&c, ns.as_bytes())?;
            }
            if is_new && m.count >= LOCAL_MAX_KEYS {
                Some(swept)
            } else {
                if is_new {
                    m.count += 1;
                }
                m.due = earliest(m.due, expiry);
                set_meta(&mut c, ns.as_bytes(), m)?;
                t.insert(k.as_slice(), row.as_slice())?;
                None
            }
        };
        match refused {
            None => tx.commit()?,
            // Keep what a sweep did (an exact `due`), so the next refusal is O(1) again.
            Some(true) => tx.commit()?,
            Some(false) => tx.abort()?,
        }
        if refused.is_some() {
            return Ok(Err(format!(
                "namespace holds {LOCAL_MAX_KEYS} live keys, the most allowed"
            )));
        }
        Ok(Ok(()))
    }

    /// Deletes `ns/key` (a no-op if absent). `due` stays a valid lower bound.
    pub fn local_delete(&self, ns: &str, key: &str) -> Result<(), StoreError> {
        check_ns(ns)?;
        if key.len() > LOCAL_MAX_KEY {
            return Ok(());
        }
        let tx = self.db.begin_write()?;
        let removed = {
            let mut t = tx.open_table(SVC_LOCAL)?;
            let removed = t.remove(row_key(ns, key).as_slice())?.is_some();
            if removed {
                let mut c = tx.open_table(SVC_LOCAL_COUNT)?;
                let mut m = meta_of(&c, ns.as_bytes())?;
                m.count = m.count.saturating_sub(1);
                set_meta(&mut c, ns.as_bytes(), m)?;
            }
            removed
        };
        if removed {
            tx.commit()?;
        } else {
            tx.abort()?;
        }
        Ok(())
    }

    /// Deletes every entry expired at `now_ms`, in all namespaces. Returns how many.
    pub fn local_sweep(&self, now_ms: u64) -> Result<usize, StoreError> {
        let tx = self.db.begin_write()?;
        let s = {
            let mut t = tx.open_table(SVC_LOCAL)?;
            let mut c = tx.open_table(SVC_LOCAL_COUNT)?;
            sweep(&mut t, &mut c, None, now_ms)?
        };
        self.note_scanned(s.visited);
        if s.changed {
            tx.commit()?;
        } else {
            tx.abort()?;
        }
        Ok(s.removed)
    }

    /// Deletes every entry of `ns`, live or not (for a purged data generation), and its
    /// count row. Returns how many entries.
    pub fn local_drop_namespace(&self, ns: &str) -> Result<usize, StoreError> {
        check_ns(ns)?;
        let (lo, hi) = ns_range(ns);
        let tx = self.db.begin_write()?;
        let (removed, had_meta) = {
            let mut t = tx.open_table(SVC_LOCAL)?;
            let mut removed = 0;
            for item in t.extract_from_if(lo.as_slice()..hi.as_slice(), |_, _| true)? {
                item?;
                removed += 1;
            }
            let had_meta = tx
                .open_table(SVC_LOCAL_COUNT)?
                .remove(ns.as_bytes())?
                .is_some();
            (removed, had_meta)
        };
        if removed > 0 || had_meta {
            tx.commit()?;
        } else {
            tx.abort()?;
        }
        Ok(removed)
    }

    /// The namespaces holding local entries (live or not yet swept), sorted.
    pub fn local_namespaces(&self) -> Result<Vec<String>, StoreError> {
        let tx = self.db.begin_read()?;
        let c = tx.open_table(SVC_LOCAL_COUNT)?;
        let mut out = Vec::new();
        for item in c.iter()? {
            let (k, _) = item?;
            let ns = String::from_utf8(k.value().to_vec())
                .map_err(|_| StoreError::corrupt("svc_local_count", "namespace is not UTF-8"))?;
            out.push(ns);
        }
        Ok(out)
    }

    /// The number of live entries of `ns` now (a scan).
    pub fn local_count(&self, ns: &str) -> Result<u64, StoreError> {
        check_ns(ns)?;
        let now = self.local_now_ms();
        let (lo, hi) = ns_range(ns);
        let tx = self.db.begin_read()?;
        let t = tx.open_table(SVC_LOCAL)?;
        let mut n = 0;
        for item in t.range(lo.as_slice()..hi.as_slice())? {
            let (_, v) = item?;
            if live(split_row(v.value())?.0, now) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// The number of stored entries of `ns`, expired ones not yet swept included.
    pub fn local_count_all(&self, ns: &str) -> Result<u64, StoreError> {
        check_ns(ns)?;
        let tx = self.db.begin_read()?;
        let c = tx.open_table(SVC_LOCAL_COUNT)?;
        Ok(meta_of(&c, ns.as_bytes())?.count)
    }

    /// Inserts `n` entries `<prefix><i>` (value `[1]`, absolute `expiry_ms`) in one
    /// transaction, bypassing the caps (to reach them quickly in tests).
    #[cfg(feature = "test-hooks")]
    pub fn local_fill_for_test(
        &self,
        ns: &str,
        prefix: &str,
        n: u64,
        expiry_ms: u64,
    ) -> Result<(), StoreError> {
        check_ns(ns)?;
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(SVC_LOCAL)?;
            let mut c = tx.open_table(SVC_LOCAL_COUNT)?;
            let mut row = keys::u64_value(expiry_ms).to_vec();
            row.push(1);
            let mut m = meta_of(&c, ns.as_bytes())?;
            for i in 0..n {
                let k = row_key(ns, &format!("{prefix}{i:06}"));
                if t.insert(k.as_slice(), row.as_slice())?.is_none() {
                    m.count += 1;
                }
            }
            m.due = earliest(m.due, expiry_ms);
            set_meta(&mut c, ns.as_bytes(), m)?;
        }
        tx.commit()?;
        Ok(())
    }
}
