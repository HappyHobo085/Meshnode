//! Async facade over the blocking `mesh-store`.

use std::sync::Arc;

use mesh_store::{Store, StoreError};

use crate::config::DynClock;
use crate::error::NodeError;

pub type CoreStore = Store<DynClock>;

/// Cloneable handle; every call runs on the blocking pool so the async runtime is never stalled.
#[derive(Clone)]
pub struct StoreHandle(Arc<CoreStore>);

impl StoreHandle {
    pub fn new(store: Arc<CoreStore>) -> Self {
        StoreHandle(store)
    }

    /// Direct access, e.g. for `subscribe`. Do not call blocking methods from async code.
    pub fn inner(&self) -> &Arc<CoreStore> {
        &self.0
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
        self.call(move |s| s.set(&ns, &key, &value).map(drop)).await
    }

    pub async fn delete(&self, ns: &str, key: &str) -> Result<(), NodeError> {
        let (ns, key) = (ns.to_owned(), key.to_owned());
        self.call(move |s| s.delete(&ns, &key).map(drop)).await
    }

    pub async fn set_add(&self, ns: &str, set: &str, member: Vec<u8>) -> Result<(), NodeError> {
        let (ns, set) = (ns.to_owned(), set.to_owned());
        self.call(move |s| s.set_add(&ns, &set, &member).map(drop))
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
