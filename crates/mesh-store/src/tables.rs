//! redb table definitions and postcard value structs.
//!
//! Value encodings:
//! - `meta`: `mesh`/`node` = 32 raw bytes; `next_seq`, `last_hlc`, `schema` = u64 big-endian.
//! - `reg`: [`RegRow`]; `set`: [`SetRow`]; `ops`: [`OpRow`]; `pending`: [`PendingRow`] (postcard).
//! - `vv` (highest gap-free seq), `removed` (max cutoff seq), `superseded` (superseded-at ms): u64 big-endian.

use crate::error::StoreError;
use mesh_hlc::Hlc;
use mesh_proto::SignedOp;
use redb::{TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub(crate) type Def = TableDefinition<'static, &'static [u8], &'static [u8]>;

pub(crate) const META: Def = TableDefinition::new("meta");
pub(crate) const REG: Def = TableDefinition::new("reg");
pub(crate) const SET: Def = TableDefinition::new("set");
pub(crate) const OPS: Def = TableDefinition::new("ops");
pub(crate) const VV: Def = TableDefinition::new("vv");
pub(crate) const PENDING: Def = TableDefinition::new("pending");
pub(crate) const REMOVED: Def = TableDefinition::new("removed");
pub(crate) const SUPERSEDED: Def = TableDefinition::new("superseded");

pub(crate) const ALL: [Def; 8] = [META, REG, SET, OPS, VV, PENDING, REMOVED, SUPERSEDED];

/// `meta` row names.
pub(crate) mod meta {
    pub const MESH: &str = "mesh";
    pub const NODE: &str = "node";
    pub const NEXT_SEQ: &str = "next_seq";
    pub const LAST_HLC: &str = "last_hlc";
    pub const SCHEMA: &str = "schema";
    pub const LAST_FULL_SYNC_MS: &str = "last_full_sync_ms";
}

pub(crate) const SCHEMA_VERSION: u64 = 1;

/// One origin's current winner for a key (`reg` value; origin is in the key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegRow {
    pub order: Vec<u8>,
    pub hlc: Hlc,
    pub seq: u64,
    pub value: Option<Vec<u8>>,
}

/// Who added a set member (`set` value; origin is in the key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SetRow {
    pub hlc: Hlc,
    pub seq: u64,
}

/// A stored op (`ops` value).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OpRow {
    pub op: SignedOp,
    pub received_ms: u64,
    pub invalid: bool,
}

/// An op held back from merging (`pending` value). `reason`: 0 = gap, 1 = unknown
/// origin, 2 = skew (`DeferReason::code`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingRow {
    pub op: SignedOp,
    pub reason: u8,
    pub first_seen_ms: u64,
}

pub(crate) fn encode<T: Serialize>(table: &'static str, v: &T) -> Result<Vec<u8>, StoreError> {
    postcard::to_allocvec(v).map_err(|e| StoreError::corrupt(table, format!("encode: {e}")))
}

pub(crate) fn decode<T: DeserializeOwned>(table: &'static str, b: &[u8]) -> Result<T, StoreError> {
    postcard::from_bytes(b).map_err(|e| StoreError::corrupt(table, format!("decode: {e}")))
}

/// Creates every table (no-op for existing ones) inside `tx`.
pub(crate) fn create_all(tx: &WriteTransaction) -> Result<(), StoreError> {
    for def in ALL {
        tx.open_table(def)?;
    }
    Ok(())
}
