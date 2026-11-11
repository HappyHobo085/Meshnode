//! The service's key layout in `meshnode:kv`. Pure (std + `aegis-sync-core` only), so the
//! Meshnode test harness compiles this same file (`#[path]`) to seed state exactly where the
//! service reads it.
//!
//! Replicated (`meshnode:kv/store`):
//! - `rec/<acct>/<ns>/<uuid>`: one record, written with `set-ordered(order_bytes(hlc))`; the
//!   value is the record's canonical `WireRecord` JSON (never an `ord`; see `records.rs`).
//! - `hlcidx/<acct>/<ns>/<hex(order_bytes(hlc))>`: grow-only set of uuids that were ever
//!   written with that exact `hlc`. A candidate list for tie detection only.
//! - `devices/<acct>`: grow-only set of `devices::DeviceFacts` JSON.
//! - `pending/<acct>`, `approved/<acct>`, `revoked/<acct>`: grow-only sets of device ids.
//! - `labels/<acct>`: grow-only set of `devices::LabelFact` JSON.
//!
//! Node-local (`meshnode:kv/local`):
//! - `nonce/<device>/<nonce>`: a spent replay nonce.
//!
//! `<acct>` and `<device>` are canonical lower-case hex (`check_token` refuses anything else);
//! `<ns>` goes through [`enc_ns`] so it never contains `/`. `<uuid>` is stored raw: it is the
//! last component, and keeping it raw keeps `list-keys` order equal to the native uuid order.
use aegis_sync_core::order::order_bytes;
use serde_json::Value;

/// The replicated store's longest key or set name (`mesh_proto::op::MAX_KEY`).
pub const MAX_KEY: usize = 512;
/// The replicated store's longest `set-ordered` order (`mesh_proto::op::MAX_ORDER`).
pub const MAX_ORDER: usize = 64;

/// `ns` with `%`, `/` and NUL percent-escaped: injective, and never contains `/` (so one
/// namespace's prefix cannot match another's keys) or NUL (which the store refuses).
pub fn enc_ns(ns: &str) -> String {
    let mut out = String::with_capacity(ns.len());
    for c in ns.chars() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            '\0' => out.push_str("%00"),
            c => out.push(c),
        }
    }
    out
}

/// Every record key of one `(acct, ns)` starts with this.
pub fn rec_prefix(acct: &str, ns: &str) -> String {
    format!("rec/{acct}/{}/", enc_ns(ns))
}

/// The key of record `uuid` in `(acct, ns)`.
pub fn rec_key(acct: &str, ns: &str, uuid: &str) -> String {
    format!("{}{uuid}", rec_prefix(acct, ns))
}

/// Every record key of one account, across its namespaces, starts with this.
pub fn account_prefix(acct: &str) -> String {
    format!("rec/{acct}/")
}

/// The tie-candidate set for `hlc` in `(acct, ns)`.
pub fn hlcidx_set(acct: &str, ns: &str, hlc: &Value) -> String {
    format!("hlcidx/{acct}/{}/{}", enc_ns(ns), hex(&order_bytes(hlc)))
}

pub fn devices_set(acct: &str) -> String {
    format!("devices/{acct}")
}

pub fn pending_set(acct: &str) -> String {
    format!("pending/{acct}")
}

pub fn approved_set(acct: &str) -> String {
    format!("approved/{acct}")
}

pub fn revoked_set(acct: &str) -> String {
    format!("revoked/{acct}")
}

#[allow(dead_code)] // written by the devices endpoints (Task 8) and the test harness
pub fn labels_set(acct: &str) -> String {
    format!("labels/{acct}")
}

/// The `kv/local` key recording a spent `(device, nonce)`.
pub fn nonce_key(device: &str, nonce: &str) -> String {
    format!("nonce/{device}/{nonce}")
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    out
}
