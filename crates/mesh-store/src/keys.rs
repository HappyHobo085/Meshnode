//! Byte-key encoding for every table.
//!
//! | Table | Key bytes |
//! |---|---|
//! | `reg` | `ns ‖ 0x00 ‖ key ‖ 0x00 ‖ origin[32]` |
//! | `set` | `ns ‖ 0x00 ‖ set ‖ 0x00 ‖ member ‖ origin[32]` (origin = the last 32 bytes) |
//! | `ops`, `pending`, `superseded` | `origin[32] ‖ seq (u64 big-endian)` |
//! | `vv`, `removed` | `origin[32]` |
//! | `meta` | UTF-8 name |
//!
//! `ns`, `key` and `set` never contain `0x00` (enforced by `OpBody::validate`), so for
//! `reg` the rows of one key are exactly the rows starting with `ns ‖ 0 ‖ key ‖ 0`, and
//! their byte order matches the string order of keys.

use crate::error::StoreError;
use mesh_proto::NodeId;

pub(crate) const SEP: u8 = 0;
pub(crate) const ORIGIN_LEN: usize = 32;
pub(crate) const OP_KEY_LEN: usize = ORIGIN_LEN + 8;

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b.iter().take(64) {
        let _ = write!(s, "{x:02x}");
    }
    if b.len() > 64 {
        s.push('…');
    }
    s
}

fn bad_key(table: &'static str, k: &[u8]) -> StoreError {
    StoreError::corrupt(table, format!("malformed key {}", hex(k)))
}

fn origin_from(table: &'static str, b: &[u8]) -> Result<NodeId, StoreError> {
    <[u8; ORIGIN_LEN]>::try_from(b)
        .map(NodeId)
        .map_err(|_| bad_key(table, b))
}

/// `ns ‖ 0`: prefix of every `reg` / `set` row of a namespace.
pub(crate) fn ns_prefix(ns: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(ns.len() + 1);
    k.extend_from_slice(ns.as_bytes());
    k.push(SEP);
    k
}

/// `ns ‖ 0 ‖ name ‖ 0`: prefix of every `reg` row of a key, or `set` row of a set.
pub(crate) fn item_prefix(ns: &str, name: &str) -> Vec<u8> {
    let mut k = ns_prefix(ns);
    k.extend_from_slice(name.as_bytes());
    k.push(SEP);
    k
}

pub(crate) fn reg_key(ns: &str, key: &str, origin: &NodeId) -> Vec<u8> {
    let mut k = item_prefix(ns, key);
    k.extend_from_slice(&origin.0);
    k
}

/// A decoded `reg` key, borrowing from the raw bytes.
pub(crate) struct RegKey<'a> {
    pub ns: &'a [u8],
    pub key: &'a [u8],
    /// `ns ‖ 0 ‖ key ‖ 0`: identical for all rows of one key.
    pub group: &'a [u8],
    pub origin: NodeId,
}

pub(crate) fn split_reg_key(k: &[u8]) -> Result<RegKey<'_>, StoreError> {
    let group_len = k
        .len()
        .checked_sub(ORIGIN_LEN)
        .ok_or_else(|| bad_key("reg", k))?;
    let (group, origin) = k.split_at(group_len);
    let inner = match group.split_last() {
        Some((&SEP, inner)) => inner,
        _ => return Err(bad_key("reg", k)),
    };
    let sep = inner
        .iter()
        .position(|&b| b == SEP)
        .ok_or_else(|| bad_key("reg", k))?;
    let (ns, key) = (&inner[..sep], &inner[sep + 1..]);
    if key.contains(&SEP) {
        return Err(bad_key("reg", k));
    }
    Ok(RegKey {
        ns,
        key,
        group,
        origin: origin_from("reg", origin)?,
    })
}

pub(crate) fn set_key(ns: &str, set: &str, member: &[u8], origin: &NodeId) -> Vec<u8> {
    let mut k = item_prefix(ns, set);
    k.extend_from_slice(member);
    k.extend_from_slice(&origin.0);
    k
}

/// A decoded `set` key, borrowing from the raw bytes.
pub(crate) struct SetKey<'a> {
    pub ns: &'a [u8],
    /// `ns ‖ 0 ‖ set ‖ 0`: identical for all rows of one set.
    pub group: &'a [u8],
    pub member: &'a [u8],
    pub origin: NodeId,
}

pub(crate) fn split_set_key(k: &[u8]) -> Result<SetKey<'_>, StoreError> {
    let body_len = k
        .len()
        .checked_sub(ORIGIN_LEN)
        .ok_or_else(|| bad_key("set", k))?;
    let (body, origin) = k.split_at(body_len);
    let s1 = body
        .iter()
        .position(|&b| b == SEP)
        .ok_or_else(|| bad_key("set", k))?;
    let s2 = body[s1 + 1..]
        .iter()
        .position(|&b| b == SEP)
        .map(|p| s1 + 1 + p)
        .ok_or_else(|| bad_key("set", k))?;
    Ok(SetKey {
        ns: &body[..s1],
        group: &body[..=s2],
        member: &body[s2 + 1..],
        origin: origin_from("set", origin)?,
    })
}

/// Key of `ops`, `pending` and `superseded`.
pub(crate) fn op_key(origin: &NodeId, seq: u64) -> [u8; OP_KEY_LEN] {
    let mut k = [0u8; OP_KEY_LEN];
    k[..ORIGIN_LEN].copy_from_slice(&origin.0);
    k[ORIGIN_LEN..].copy_from_slice(&seq.to_be_bytes());
    k
}

pub(crate) fn split_op_key(table: &'static str, k: &[u8]) -> Result<(NodeId, u64), StoreError> {
    if k.len() != OP_KEY_LEN {
        return Err(bad_key(table, k));
    }
    let (o, s) = k.split_at(ORIGIN_LEN);
    let seq = <[u8; 8]>::try_from(s).map_err(|_| bad_key(table, k))?;
    Ok((origin_from(table, o)?, u64::from_be_bytes(seq)))
}

/// Key of `vv` and `removed`.
pub(crate) fn origin_key(origin: &NodeId) -> [u8; ORIGIN_LEN] {
    origin.0
}

pub(crate) fn split_origin_key(table: &'static str, k: &[u8]) -> Result<NodeId, StoreError> {
    origin_from(table, k)
}

/// Encodes a `u64` value (used by `meta` counters, `vv`, `removed`, `superseded`).
pub(crate) fn u64_value(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

pub(crate) fn read_u64_value(table: &'static str, v: &[u8]) -> Result<u64, StoreError> {
    <[u8; 8]>::try_from(v)
        .map(u64::from_be_bytes)
        .map_err(|_| StoreError::corrupt(table, format!("bad u64 value {}", hex(v))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reg_roundtrip() {
        let o = NodeId([3; 32]);
        let k = reg_key("ns", "key", &o);
        let d = split_reg_key(&k).unwrap();
        assert_eq!(d.ns, b"ns");
        assert_eq!(d.key, b"key");
        assert_eq!(d.group, &item_prefix("ns", "key")[..]);
        assert_eq!(d.origin, o);
        assert!(split_reg_key(&k[..k.len() - 1]).is_err());
        assert!(split_reg_key(&[0; 10]).is_err());
    }

    #[test]
    fn set_roundtrip_with_nul_in_member() {
        let o = NodeId([0; 32]);
        let k = set_key("ns", "s", b"a\0b", &o);
        let d = split_set_key(&k).unwrap();
        assert_eq!(d.ns, b"ns");
        assert_eq!(d.group, &item_prefix("ns", "s")[..]);
        assert_eq!(d.member, b"a\0b");
        assert_eq!(d.origin, o);
        let e = set_key("ns", "s", b"", &o);
        assert_eq!(split_set_key(&e).unwrap().member, b"");
    }

    #[test]
    fn op_key_roundtrip_and_order() {
        let o = NodeId([9; 32]);
        assert_eq!(split_op_key("ops", &op_key(&o, 77)).unwrap(), (o, 77));
        assert!(op_key(&o, 255) < op_key(&o, 256));
        assert!(split_op_key("ops", &[1, 2]).is_err());
        assert_eq!(split_origin_key("vv", &origin_key(&o)).unwrap(), o);
        assert_eq!(read_u64_value("vv", &u64_value(5)).unwrap(), 5);
        assert!(read_u64_value("vv", &[1]).is_err());
    }
}
