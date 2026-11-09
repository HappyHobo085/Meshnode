//! A service's key-value data over the gated store.

use mesh_core::{NodeError, StoreHandle};
use mesh_runtime::{KvBackend, KvError};
use mesh_store::StoreError;

/// [`KvBackend`] over one namespace of the replicated store. The namespace is fixed at
/// construction, so a service cannot reach another one. Writes go through the store's
/// write gate, which refuses them on a removed or re-bootstrapping node.
#[derive(Clone)]
pub struct ServiceKv {
    store: StoreHandle,
    ns: String,
}

impl ServiceKv {
    pub fn new(store: StoreHandle, ns: String) -> Self {
        ServiceKv { store, ns }
    }
}

fn map_err(e: NodeError) -> KvError {
    match e {
        NodeError::Store(StoreError::Full) => KvError::Full,
        NodeError::Removed => KvError::Removed,
        NodeError::Rebootstrapping => KvError::Rebootstrapping,
        e => KvError::Other(e.to_string()),
    }
}

#[async_trait::async_trait]
impl KvBackend for ServiceKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        self.store.get(&self.ns, key).await.map_err(map_err)
    }

    async fn set(&self, key: &str, value: Vec<u8>) -> Result<(), KvError> {
        self.store.set(&self.ns, key, value).await.map_err(map_err)
    }

    async fn set_ordered(
        &self,
        key: &str,
        order: Vec<u8>,
        value: Option<Vec<u8>>,
    ) -> Result<(), KvError> {
        self.store
            .set_ordered(&self.ns, key, order, value)
            .await
            .map_err(map_err)
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.store.delete(&self.ns, key).await.map_err(map_err)
    }

    async fn list_keys(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<String>, KvError> {
        self.store
            .list_keys(&self.ns, prefix, start_after.map(str::to_owned), limit)
            .await
            .map_err(map_err)
    }

    async fn set_add(&self, set: &str, member: Vec<u8>) -> Result<(), KvError> {
        self.store
            .set_add(&self.ns, set, member)
            .await
            .map_err(map_err)
    }

    async fn set_contains(&self, set: &str, member: &[u8]) -> Result<bool, KvError> {
        let (ns, set, member) = (self.ns.clone(), set.to_owned(), member.to_vec());
        self.store
            .call(move |r| r.set_contains(&ns, &set, &member))
            .await
            .map_err(map_err)
    }

    async fn set_members(
        &self,
        set: &str,
        start_after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<Vec<u8>>, KvError> {
        self.store
            .set_members(&self.ns, set, start_after.map(<[u8]>::to_vec), limit)
            .await
            .map_err(map_err)
    }
}
