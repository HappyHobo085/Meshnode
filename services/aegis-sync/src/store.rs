//! Typed access to `meshnode:kv` in the layout of [`crate::keys`].
use std::collections::BTreeSet;

use aegis_sync_core::devices::{AccountFacts, DeviceFacts};
use aegis_sync_core::order::order_bytes;
use aegis_sync_core::wire::{WireRecord, hlc_key};
use serde_json::Value;

use crate::keys;
use crate::meshnode::kv::store;

/// The most entries one `list-keys` / `set-members` call returns (the host's cap).
pub const PAGE: u32 = 1000;

/// Every member of a set, in byte order.
fn members(set: &str) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    loop {
        let batch = store::set_members(set, out.last().map(Vec::as_slice), PAGE);
        let done = batch.len() < PAGE as usize;
        out.extend(batch);
        if done {
            return out;
        }
    }
}

fn id_set(set: &str) -> BTreeSet<String> {
    members(set)
        .into_iter()
        .filter_map(|m| String::from_utf8(m).ok())
        .collect()
}

/// The account's device facts as this node sees them (labels are not read: no record route
/// needs them).
///
/// READ ORDER IS LOAD-BEARING: `devices` first, then `pending`, `approved`, `revoked`. A joiner
/// is a member only once it is not pending, so if its `devices` entry were visible while its
/// `pending` entry was not, it would be admitted before the owner approved it. The writer (the
/// devices endpoints, Task 8) MUST therefore write a joiner's `pending` entry no later than
/// its `devices` entry; every device this read sees in `devices` then already has its
/// `pending` entry visible to the reads that follow. Reading the other sets later can only
/// show more approvals and more revocations, both of which are real.
pub fn account_facts(acct: &str) -> AccountFacts {
    let devices: Vec<DeviceFacts> = members(&keys::devices_set(acct))
        .into_iter()
        .filter_map(|m| serde_json::from_slice(&m).ok())
        .collect();
    let pending = id_set(&keys::pending_set(acct));
    let approved = id_set(&keys::approved_set(acct));
    let revoked = id_set(&keys::revoked_set(acct));
    AccountFacts {
        devices,
        pending,
        approved,
        revoked,
        labels: Vec::new(),
    }
}

/// The stored record at `key`, if any. A value that does not parse (none is ever written)
/// reads as a record with every field defaulted.
pub fn record(key: &str) -> Option<WireRecord> {
    store::get(key).map(|v| serde_json::from_slice(&v).unwrap_or_default())
}

/// Just the stored `hlc` at `key`: `Some(Value::Null)` for an unparseable value.
pub fn stored_hlc(key: &str) -> Option<Value> {
    #[derive(serde::Deserialize)]
    struct HlcOnly {
        #[serde(default)]
        hlc: Value,
    }
    store::get(key).map(|v| {
        serde_json::from_slice::<HlcOnly>(&v)
            .map(|h| h.hlc)
            .unwrap_or(Value::Null)
    })
}

/// Writes `rec` (whose `uuid` is set and `ord` is none) as `(acct, ns, rec.uuid)`, ordered by
/// its client `hlc`. The tie-candidate index entry goes first, so a replica that has the
/// record also has the entry.
pub fn put_record(acct: &str, ns: &str, rec: &WireRecord) {
    let bytes = serde_json::to_vec(rec).expect("a WireRecord serializes");
    store::set_add(&keys::hlcidx_set(acct, ns, &rec.hlc), rec.uuid.as_bytes());
    store::set_ordered(
        &keys::rec_key(acct, ns, &rec.uuid),
        &order_bytes(&rec.hlc),
        Some(&bytes),
    );
}

/// The mesh tie rule: does another live record (tombstones included) of `(acct, ns)` with a
/// LOWER uuid hold exactly `rec.hlc`? Then `rec` is served with `order::tie_ord`.
///
/// The index set is a grow-only candidate list (a uuid stays listed after it moves to a newer
/// hlc), so each candidate's CURRENT record is checked. Members are in byte order, which is
/// uuid order, so the scan stops at `rec.uuid`.
pub fn tied_below(acct: &str, ns: &str, rec: &WireRecord) -> bool {
    let set = keys::hlcidx_set(acct, ns, &rec.hlc);
    let want = hlc_key(&rec.hlc);
    let mut after: Option<Vec<u8>> = None;
    loop {
        let batch = store::set_members(&set, after.as_deref(), PAGE);
        let done = batch.len() < PAGE as usize;
        for m in &batch {
            if m.as_slice() >= rec.uuid.as_bytes() {
                return false;
            }
            let Ok(uuid) = std::str::from_utf8(m) else {
                continue;
            };
            let current = stored_hlc(&keys::rec_key(acct, ns, uuid));
            if current.is_some_and(|h| hlc_key(&h) == want) {
                return true;
            }
        }
        if done {
            return false;
        }
        after = batch.last().cloned();
    }
}

/// The record keys under `prefix`, counted up to `cap` (counting stops there).
pub fn count_keys(prefix: &str, cap: usize) -> usize {
    let mut n = 0;
    let mut after: Option<String> = None;
    while n < cap {
        let batch = store::list_keys(prefix, after.as_deref(), PAGE);
        n += batch.len();
        if batch.len() < PAGE as usize {
            break;
        }
        after = batch.last().cloned();
    }
    n.min(cap)
}

/// Every uuid stored in `(acct, ns)`.
pub fn uuids(acct: &str, ns: &str) -> Vec<String> {
    let prefix = keys::rec_prefix(acct, ns);
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let batch = store::list_keys(&prefix, after.as_deref(), PAGE);
        let done = batch.len() < PAGE as usize;
        after = batch.last().cloned();
        out.extend(batch.into_iter().map(|k| k[prefix.len()..].to_string()));
        if done {
            return out;
        }
    }
}
