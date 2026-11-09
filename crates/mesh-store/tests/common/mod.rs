//! Shared helpers for mesh-store integration tests.
#![allow(dead_code)]

use ed25519_dalek::SigningKey;
use mesh_hlc::ManualClock;
use mesh_proto::{MeshId, NodeId};
use mesh_store::Store;
use std::path::{Path, PathBuf};

pub const MESH: MeshId = MeshId([7; 32]);

/// Deterministic signing key for test node `n`.
pub fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

pub fn node(n: u8) -> NodeId {
    NodeId(key(n).verifying_key().to_bytes())
}

/// Database file used by test node `n` inside `dir`.
pub fn db_path(dir: &Path, n: u8) -> PathBuf {
    dir.join(format!("node{n}.redb"))
}

/// Opens (or creates) node `n`'s store in `dir`: mesh `[7; 32]`, key `[n; 32]`.
pub fn store(dir: &Path, n: u8, clock: &ManualClock) -> Store<ManualClock> {
    Store::open(&db_path(dir, n), MESH, key(n), clock.clone()).expect("open store")
}
