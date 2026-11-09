//! Async facade over the blocking `mesh-store`.
//!
//! Every local write of the public API goes through the write gate ([`StoreHandle::set`]
//! and the other write methods): it is refused once this node is removed and while a
//! re-bootstrap runs. [`StoreHandle::call`] only gets a read-only [`StoreReader`], so it
//! cannot bypass the gate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mesh_proto::{MeshId, NodeId, SignedOp, VersionVector};
use mesh_store::{Change, RegEntry, Store, StoreError, SyncItem};
use tokio::sync::{RwLock, broadcast};

use crate::config::DynClock;
use crate::error::NodeError;

pub type CoreStore = Store<DynClock>;

/// Cloneable handle; every call runs on the blocking pool so the async runtime is never stalled.
#[derive(Clone)]
pub struct StoreHandle(Arc<CoreStore>, Arc<Refuse>);

/// Why local writes are refused; shared by every clone of a handle.
#[derive(Default)]
struct Refuse {
    removed: AtomicBool,
    rebootstrapping: AtomicBool,
    /// Every local write holds a read guard from its refusal check until its store call
    /// has returned; [`StoreHandle::drain_writes`] takes the write side to wait them out.
    gate: Arc<RwLock<()>>,
}

impl StoreHandle {
    pub fn new(store: Arc<CoreStore>) -> Self {
        StoreHandle(store, Arc::default())
    }

    /// Direct access for the node's own tasks (sync, removal, maintenance), which are not
    /// local writes. Do not call blocking methods from async code.
    pub(crate) fn raw(&self) -> &Arc<CoreStore> {
        &self.0
    }

    /// Direct access, bypassing the write gate (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn inner(&self) -> &Arc<CoreStore> {
        &self.0
    }

    /// Store change events (wake-up signals; see `mesh_store::Change`).
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.0.subscribe()
    }

    /// From now on `set`, `delete` and `set_add` fail with `NodeError::Removed` (spec §3.3
    /// step 6). Shared by every clone of this handle.
    pub(crate) fn refuse_writes_removed(&self) {
        self.1.removed.store(true, Ordering::SeqCst);
    }

    /// While `on`, `set`, `delete` and `set_add` fail with `NodeError::Rebootstrapping`
    /// (spec §7: from the export until the re-bootstrap is done, so no own op is written
    /// after the export was taken).
    pub(crate) fn refuse_writes_rebootstrapping(&self, on: bool) {
        self.1.rebootstrapping.store(on, Ordering::SeqCst);
    }

    /// Waits until every local write that passed its refusal check has finished in the
    /// store. After `refuse_writes_rebootstrapping(true)` and this, no own op can be
    /// written until the refusal is lifted.
    pub(crate) async fn drain_writes(&self) {
        drop(self.1.gate.write().await);
    }

    fn check_writable(&self) -> Result<(), NodeError> {
        if self.1.removed.load(Ordering::SeqCst) {
            return Err(NodeError::Removed);
        }
        if self.1.rebootstrapping.load(Ordering::SeqCst) {
            return Err(NodeError::Rebootstrapping);
        }
        Ok(())
    }

    /// A local write: refused as above, and the store's own refusal after a reset is
    /// reported as `NodeError::Rebootstrapping`.
    async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&CoreStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, NodeError> {
        let guard = self.1.gate.clone().read_owned().await;
        self.check_writable()?;
        // The guard moves into the blocking call: it is released only once the write is
        // done, even if this future is dropped meanwhile.
        self.call_raw(move |s| {
            let _guard = guard;
            f(s)
        })
        .await
        .map_err(|e| match e {
            NodeError::Store(StoreError::Rebootstrapping) => NodeError::Rebootstrapping,
            e => e,
        })
    }

    /// Runs `f` against a read-only view of the store in one `spawn_blocking` (several
    /// reads at once; writes go through the write methods).
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&StoreReader<'_>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, NodeError> {
        self.call_raw(move |s| f(&StoreReader(s))).await
    }

    /// Runs `f` against the whole store in one `spawn_blocking`, for the node's own tasks
    /// (sync, removal, maintenance); its local writes go through `write` instead.
    pub(crate) async fn call_raw<T: Send + 'static>(
        &self,
        f: impl FnOnce(&CoreStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, NodeError> {
        let store = self.0.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| NodeError::Io(std::io::Error::other(e)))?
            .map_err(NodeError::from)
    }

    /// [`StoreHandle::call`] with the whole store, bypassing the write gate (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn call_unchecked<T: Send + 'static>(
        &self,
        f: impl FnOnce(&CoreStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, NodeError> {
        self.call_raw(f).await
    }

    pub async fn get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>, NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.call(move |s| s.get(&ns, &key)).await
    }

    pub async fn set(&self, ns: &str, key: &str, value: Vec<u8>) -> Result<(), NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.write(move |s| s.set(&ns, &key, &value).map(drop))
            .await
    }

    pub async fn delete(&self, ns: &str, key: &str) -> Result<(), NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.write(move |s| s.delete(&ns, &key).map(drop)).await
    }

    /// Writes `ns/key` with an explicit `order` (compared before the HLC); `None` writes a
    /// tombstone.
    pub async fn set_ordered(
        &self,
        ns: &str,
        key: &str,
        order: Vec<u8>,
        value: Option<Vec<u8>>,
    ) -> Result<(), NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.write(move |s| s.set_ordered(&ns, &key, &order, value.as_deref()).map(drop))
            .await
    }

    pub async fn set_add(&self, ns: &str, set: &str, member: Vec<u8>) -> Result<(), NodeError> {
        let (ns, set) = (ns.to_owned(), set.to_owned());
        self.write(move |s| s.set_add(&ns, &set, &member).map(drop))
            .await
    }

    /// Writes a local tombstone for every live key of `ns` and returns the ops written
    /// (see `mesh_store::Store::purge_namespace`).
    pub async fn purge_namespace(&self, ns: &str) -> Result<Vec<SignedOp>, NodeError> {
        let ns = ns.to_owned();
        self.write(move |s| s.purge_namespace(&ns)).await
    }

    pub async fn list_keys(
        &self,
        ns: &str,
        prefix: &str,
        start_after: Option<String>,
        limit: u32,
    ) -> Result<Vec<String>, NodeError> {
        let (ns, prefix) = (ns.to_owned(), prefix.to_owned());
        self.call(move |s| s.list_keys(&ns, &prefix, start_after.as_deref(), limit))
            .await
    }

    pub async fn set_members(
        &self,
        ns: &str,
        set: &str,
        start_after: Option<Vec<u8>>,
        limit: u32,
    ) -> Result<Vec<Vec<u8>>, NodeError> {
        let (ns, set) = (ns.to_owned(), set.to_owned());
        self.call(move |s| s.set_members(&ns, &set, start_after.as_deref(), limit))
            .await
    }
}

/// A read-only view of the store, as [`StoreHandle::call`] hands it out. Blocking: it is
/// only reachable inside `call`'s `spawn_blocking`.
pub struct StoreReader<'a>(&'a CoreStore);

impl StoreReader<'_> {
    pub fn node_id(&self) -> NodeId {
        self.0.node_id()
    }

    pub fn mesh(&self) -> MeshId {
        self.0.mesh()
    }

    pub fn get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.0.get(ns, key)
    }

    pub fn get_entry(&self, ns: &str, key: &str) -> Result<Option<RegEntry>, StoreError> {
        self.0.get_entry(ns, key)
    }

    pub fn list_keys(
        &self,
        ns: &str,
        prefix: &str,
        start_after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<String>, StoreError> {
        self.0.list_keys(ns, prefix, start_after, limit)
    }

    pub fn set_contains(&self, ns: &str, set: &str, member: &[u8]) -> Result<bool, StoreError> {
        self.0.set_contains(ns, set, member)
    }

    pub fn set_members(
        &self,
        ns: &str,
        set: &str,
        start_after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.0.set_members(ns, set, start_after, limit)
    }

    pub fn set_member_origins(
        &self,
        ns: &str,
        set: &str,
        member: &[u8],
    ) -> Result<Vec<NodeId>, StoreError> {
        self.0.set_member_origins(ns, set, member)
    }

    pub fn namespace_sizes(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        self.0.namespace_sizes()
    }

    pub fn version_vector(&self) -> Result<VersionVector, StoreError> {
        self.0.version_vector()
    }

    pub fn diff_since(
        &self,
        theirs: &VersionVector,
        limit: usize,
    ) -> Result<Vec<SyncItem>, StoreError> {
        self.0.diff_since(theirs, limit)
    }

    pub fn last_full_sync_ms(&self) -> Result<Option<u64>, StoreError> {
        self.0.last_full_sync_ms()
    }

    pub fn synced_self_seq(&self) -> Result<Option<u64>, StoreError> {
        self.0.synced_self_seq()
    }

    pub fn next_seq(&self) -> Result<u64, StoreError> {
        self.0.next_seq()
    }

    pub fn is_stale(&self) -> Result<bool, StoreError> {
        self.0.is_stale()
    }

    pub fn clock_floor_ahead_ms(&self) -> Option<u64> {
        self.0.clock_floor_ahead_ms()
    }

    #[cfg(feature = "test-hooks")]
    pub fn vv_of(&self, origin: NodeId) -> Result<u64, StoreError> {
        self.0.vv_of(origin)
    }

    #[cfg(feature = "test-hooks")]
    pub fn pending_len(&self) -> Result<u64, StoreError> {
        self.0.pending_len()
    }
}
