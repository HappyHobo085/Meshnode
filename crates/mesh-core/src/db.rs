//! Async facade over the blocking `mesh-store`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mesh_store::{Store, StoreError};
use tokio::sync::RwLock;

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

    /// Direct access, e.g. for `subscribe`. Do not call blocking methods from async code.
    pub fn inner(&self) -> &Arc<CoreStore> {
        &self.0
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
    async fn write(
        &self,
        f: impl FnOnce(&CoreStore) -> Result<(), StoreError> + Send + 'static,
    ) -> Result<(), NodeError> {
        let guard = self.1.gate.clone().read_owned().await;
        self.check_writable()?;
        // The guard moves into the blocking call: it is released only once the write is
        // done, even if this future is dropped meanwhile.
        self.call(move |s| {
            let _guard = guard;
            f(s)
        })
        .await
        .map_err(|e| match e {
            NodeError::Store(StoreError::Rebootstrapping) => NodeError::Rebootstrapping,
            e => e,
        })
    }

    /// Run `f` against the store in one `spawn_blocking`.
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&CoreStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, NodeError> {
        let store = self.0.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| NodeError::Io(std::io::Error::other(e)))?
            .map_err(NodeError::from)
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

    pub async fn set_add(&self, ns: &str, set: &str, member: Vec<u8>) -> Result<(), NodeError> {
        let (ns, set) = (ns.to_owned(), set.to_owned());
        self.write(move |s| s.set_add(&ns, &set, &member).map(drop))
            .await
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
