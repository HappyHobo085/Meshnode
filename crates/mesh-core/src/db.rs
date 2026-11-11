//! Async facade over the blocking `mesh-store`.
//!
//! Every local write of the public API goes through the write gate ([`StoreHandle::set`]
//! and the other write methods): it is refused once this node is removed and while a
//! re-bootstrap runs. [`StoreHandle::call`] only gets a read-only [`StoreReader`], so it
//! cannot bypass the gate. Node-local entries (`local_*`) write no op and are not gated.

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

    // Node-local entries (`svc_local`, see `mesh_store::Store::local_set`). They write no
    // op, so they do not go through the write gate: the gate keeps a removed or
    // re-bootstrapping node from issuing own ops, and a local entry is not one. Per-node
    // bookkeeping such as replay nonces keeps working (and keeps refusing replays) while
    // replicated writes are refused.

    pub async fn local_get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>, NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.call_raw(move |s| s.local_get(&ns, &key)).await
    }

    /// The inner `Err` is a cap breach (nothing written).
    pub async fn local_set(
        &self,
        ns: &str,
        key: &str,
        value: Vec<u8>,
        ttl_ms: u64,
    ) -> Result<Result<(), String>, NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.call_raw(move |s| s.local_set(&ns, &key, &value, ttl_ms))
            .await
    }

    pub async fn local_delete(&self, ns: &str, key: &str) -> Result<(), NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.call_raw(move |s| s.local_delete(&ns, &key)).await
    }

    /// Deletes every local entry expired at `now_ms`; returns how many.
    pub async fn local_sweep(&self, now_ms: u64) -> Result<usize, NodeError> {
        self.call_raw(move |s| s.local_sweep(now_ms)).await
    }

    /// Deletes every local entry of `ns`; returns how many.
    pub async fn local_drop_namespace(&self, ns: &str) -> Result<usize, NodeError> {
        let ns = ns.to_owned();
        self.call_raw(move |s| s.local_drop_namespace(&ns)).await
    }

    /// The namespaces holding local entries.
    pub async fn local_namespaces(&self) -> Result<Vec<String>, NodeError> {
        self.call_raw(|s| s.local_namespaces()).await
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use mesh_hlc::ManualClock;
    use mesh_proto::MeshId;

    use super::*;

    fn handle(dir: &std::path::Path, clock: &ManualClock) -> StoreHandle {
        let wall = DynClock(Arc::new(clock.clone()));
        let key = SigningKey::from_bytes(&[3; 32]);
        let store = CoreStore::open(&dir.join("store.redb"), MeshId([5; 32]), key, wall).unwrap();
        StoreHandle::new(Arc::new(store))
    }

    /// Local entries are node-local bookkeeping (e.g. replay nonces): they write no op,
    /// so the write gate, which guards own ops, does not refuse them.
    #[tokio::test]
    async fn local_writes_bypass_the_write_gate() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::new(1_700_000_000_000);
        let h = handle(dir.path(), &clock);
        h.local_set("svc", "a", b"1".to_vec(), 0)
            .await
            .unwrap()
            .unwrap();

        h.refuse_writes_rebootstrapping(true);
        assert!(matches!(
            h.set("svc", "r", b"x".to_vec()).await,
            Err(NodeError::Rebootstrapping)
        ));
        h.local_set("svc", "b", b"2".to_vec(), 0)
            .await
            .unwrap()
            .unwrap();
        h.refuse_writes_rebootstrapping(false);

        h.refuse_writes_removed();
        assert!(matches!(
            h.set("svc", "r", b"x".to_vec()).await,
            Err(NodeError::Removed)
        ));
        h.local_set("svc", "c", b"3".to_vec(), 1_000)
            .await
            .unwrap()
            .unwrap();
        h.local_delete("svc", "a").await.unwrap();
        assert_eq!(h.local_get("svc", "a").await.unwrap(), None);
        assert_eq!(h.local_get("svc", "b").await.unwrap(), Some(b"2".to_vec()));
        clock.advance(1_000);
        assert_eq!(h.local_get("svc", "c").await.unwrap(), None);
        assert_eq!(h.local_sweep(1_700_000_001_000).await.unwrap(), 1);
        // None of it wrote an op.
        assert_eq!(h.call(|r| r.next_seq()).await.unwrap(), 1);
    }
}
