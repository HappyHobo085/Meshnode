//! Replicated key-value store for a Meshnode mesh, backed by one redb file.
//!
//! Registers are multi-origin last-writer-wins by `(order, hlc, origin)`; sets are
//! add-only. See the sub-spec §5 for the full model.
//!
//! # Contracts for callers
//!
//! - **Blocking I/O.** Every [`Store`] method is synchronous; writing methods commit a
//!   redb transaction and fsync before returning, and all of them may block on the
//!   store's internal writer lock. On a tokio runtime, call them from
//!   `tokio::task::spawn_blocking` (or a dedicated thread), never directly from an async
//!   task.
//! - **Acceptor.** [`Acceptor::judge`] runs inside the write transaction with the clock
//!   mutex held: it reads only through its [`ReadView`] and never calls the `Store`.
//! - **Change events are lossy.** [`Store::subscribe`] is a bounded broadcast channel; on
//!   `Lagged`, rescan. `on_removal` and `retry_pending` are idempotent, so re-running
//!   them after a missed event is safe.
//! - **Full syncs.** Call [`Store::mark_full_sync`] after every completed two-way
//!   exchange (including the catch-up after joining) with the peer's `vv[self]`.
//! - **Covered markers.** Pass the sender's `Hello` `vv[origin]` as `advertised` to
//!   [`Store::apply_covered`].
//! - **Removal records are exempt from cutoffs.** Adds to the set [`CUTOFF_EXEMPT_SET`]
//!   (`_mesh`/`removed`) are never retracted by `on_removal` nor ignored above a cutoff,
//!   even when their issuer is removed; other ops are unaffected.

mod apply;
mod error;
mod keys;
mod local;
mod maint;
pub mod merge;
mod read;
mod removal;
mod svc_local;
mod sync;
mod tables;

pub use apply::{
    Acceptor, Change, ChangeKind, DeferReason, Outcome, PENDING_TTL_MS, ReadView, RejectReason,
    Verdict,
};
pub use error::StoreError;
pub use maint::{Export, SETTLE_MS, STALE_MS, TOMBSTONE_TTL_MS};
pub use read::RegEntry;
pub use removal::{CUTOFF_EXEMPT_SET, RemovalReport};
pub use svc_local::{LOCAL_MAX_KEY, LOCAL_MAX_KEYS, LOCAL_MAX_VALUE};
pub use sync::SyncItem;

use ed25519_dalek::SigningKey;
use mesh_hlc::{Clock, Hlc, SystemClock, WallClock};
use mesh_proto::{MeshId, NodeId};
use redb::{Database, ReadableTable};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use tables::{META, SCHEMA_VERSION, meta};

/// The replicated store. All methods take `&self`; writers are serialized internally.
/// Every method blocks (and writing methods fsync): see the crate docs for using it
/// from async code.
pub struct Store<W: WallClock = SystemClock> {
    db: Database,
    mesh: MeshId,
    node: NodeId,
    key: SigningKey,
    /// Held for the whole of every write transaction (local write, apply, retry), so seq
    /// and HLC order agree and change events are published in commit order.
    clock: Mutex<Clock<W>>,
    /// Change events, sent after each commit.
    events: tokio::sync::broadcast::Sender<Change>,
    /// A second handle to the wall clock, for readers that must not wait on `clock`
    /// (node-local entry TTLs).
    wall: W,
    /// Rows read by local-entry sweeps (test-hooks only).
    #[cfg(feature = "test-hooks")]
    local_scanned: std::sync::atomic::AtomicU64,
}

/// Reads a u64 `meta` row.
pub(crate) fn meta_u64<T>(table: &T, name: &str) -> Result<Option<u64>, StoreError>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
{
    match table.get(name.as_bytes())? {
        Some(g) => keys::read_u64_value("meta", g.value()).map(Some),
        None => Ok(None),
    }
}

/// Reads a u64 `meta` row that must exist.
pub(crate) fn meta_u64_req<T>(table: &T, name: &str) -> Result<u64, StoreError>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
{
    meta_u64(table, name)?.ok_or_else(|| StoreError::corrupt("meta", format!("missing `{name}`")))
}

impl<W: WallClock> Store<W> {
    /// Opens (creating if new) the store at `path` for `mesh`, owned by `key`.
    ///
    /// Errors: `WrongMesh` if the file belongs to another mesh; `Corrupt { table: "meta" }`
    /// if it belongs to another node or its metadata is malformed; `Io` if the file is
    /// already open (by this or another process) or cannot be opened.
    pub fn open(path: &Path, mesh: MeshId, key: SigningKey, wall: W) -> Result<Self, StoreError>
    where
        W: Clone,
    {
        let db = Database::create(path)?;
        let node = NodeId(key.verifying_key().to_bytes());
        let tx = db.begin_write()?;
        tables::create_all(&tx)?;
        let last_hlc = {
            let mut m = tx.open_table(META)?;
            let stored_mesh = m.get(meta::MESH.as_bytes())?.map(|g| g.value().to_vec());
            match stored_mesh {
                None => {
                    m.insert(meta::MESH.as_bytes(), mesh.0.as_slice())?;
                    m.insert(meta::NODE.as_bytes(), node.0.as_slice())?;
                    m.insert(meta::NEXT_SEQ.as_bytes(), keys::u64_value(1).as_slice())?;
                    m.insert(meta::LAST_HLC.as_bytes(), keys::u64_value(0).as_slice())?;
                    let schema = keys::u64_value(SCHEMA_VERSION);
                    m.insert(meta::SCHEMA.as_bytes(), schema.as_slice())?;
                    0
                }
                Some(stored) => {
                    if stored.len() != 32 {
                        return Err(StoreError::corrupt("meta", "mesh id is not 32 bytes"));
                    }
                    if stored != mesh.0 {
                        return Err(StoreError::WrongMesh);
                    }
                    let stored_node = m.get(meta::NODE.as_bytes())?.map(|g| g.value().to_vec());
                    if stored_node.as_deref() != Some(node.0.as_slice()) {
                        return Err(StoreError::corrupt(
                            "meta",
                            "store belongs to a different node id",
                        ));
                    }
                    let schema = meta_u64_req(&m, meta::SCHEMA)?;
                    if schema != SCHEMA_VERSION {
                        return Err(StoreError::corrupt(
                            "meta",
                            format!("unsupported schema version {schema}"),
                        ));
                    }
                    if meta_u64_req(&m, meta::NEXT_SEQ)? == 0 {
                        return Err(StoreError::corrupt("meta", "next_seq is 0"));
                    }
                    let last = meta_u64_req(&m, meta::LAST_HLC)?;
                    if Hlc::from_u64(last).ms() >= (1 << 48) - 1 {
                        return Err(StoreError::corrupt("meta", "last_hlc is out of range"));
                    }
                    last
                }
            }
        };
        tx.commit()?;
        let (events, _) = tokio::sync::broadcast::channel(apply::EVENT_CAPACITY);
        Ok(Store {
            db,
            mesh,
            node,
            key,
            clock: Mutex::new(Clock::new(wall.clone(), Hlc::from_u64(last_hlc))),
            events,
            wall,
            #[cfg(feature = "test-hooks")]
            local_scanned: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn node_id(&self) -> NodeId {
        self.node
    }

    pub fn mesh(&self) -> MeshId {
        self.mesh
    }

    /// How far this node's HLC floor (the highest HLC it has issued or observed, persisted
    /// across restarts) is ahead of its wall clock, if by more than `MAX_SKEW_MS`; `None`
    /// otherwise. A `Some` means the wall clock was set back (or was far ahead earlier):
    /// local writes keep working but carry HLCs ahead of real time, and peers defer them as
    /// skewed until their clocks catch up. The UI should warn the user.
    pub fn clock_floor_ahead_ms(&self) -> Option<u64> {
        let clock = self.lock_clock();
        let floor = clock.last().ms();
        let wall = clock.wall().now_ms();
        (floor > wall.saturating_add(mesh_hlc::MAX_SKEW_MS)).then(|| floor - wall)
    }

    /// Locks the clock; held for every write transaction (local writes, apply, retry).
    /// A poisoned lock is recovered: the clock holds no invariant a panic could break
    /// (it only ever moves forward).
    pub(crate) fn lock_clock(&self) -> MutexGuard<'_, Clock<W>> {
        self.clock.lock().unwrap_or_else(|e| e.into_inner())
    }
}
