//! The storage behind `meshnode:kv/store`, supplied per request by the embedder.

/// A service's key-value data. `mesh-services` implements it over the replicated store.
#[async_trait::async_trait]
pub trait KvBackend: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError>;
    async fn set(&self, key: &str, value: Vec<u8>) -> Result<(), KvError>;
    async fn set_ordered(
        &self,
        key: &str,
        order: Vec<u8>,
        value: Option<Vec<u8>>,
    ) -> Result<(), KvError>;
    async fn delete(&self, key: &str) -> Result<(), KvError>;
    async fn list_keys(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<String>, KvError>;
    async fn set_add(&self, set: &str, member: Vec<u8>) -> Result<(), KvError>;
    async fn set_contains(&self, set: &str, member: &[u8]) -> Result<bool, KvError>;
    async fn set_members(
        &self,
        set: &str,
        start_after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<Vec<u8>>, KvError>;
}

/// Why a KV call failed. The guest sees a trap; the request gets the mapped HTTP status.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KvError {
    #[error("store is full")]
    Full,
    #[error("node was removed from the mesh")]
    Removed,
    #[error("node is re-bootstrapping")]
    Rebootstrapping,
    #[error("{0}")]
    Other(String),
}

#[cfg(any(test, feature = "test-hooks"))]
pub use mem::MemKv;

#[cfg(any(test, feature = "test-hooks"))]
mod mem {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ops::Bound;
    use std::sync::Mutex;

    use super::{KvBackend, KvError};

    /// An in-memory [`KvBackend`] for tests: one node, no replication.
    #[derive(Default)]
    pub struct MemKv {
        inner: Mutex<Inner>,
    }

    #[derive(Default)]
    struct Inner {
        fail: Option<KvError>,
        max_value_len: Option<usize>,
        registers: BTreeMap<String, Register>,
        sets: BTreeMap<String, BTreeSet<Vec<u8>>>,
    }

    /// `value: None` is a deletion; `order` is set by ordered writes only.
    struct Register {
        order: Option<Vec<u8>>,
        value: Option<Vec<u8>>,
    }

    impl MemKv {
        fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
            f(&mut self.inner.lock().unwrap_or_else(|e| e.into_inner()))
        }

        /// Makes every following call return `err` (`None` restores normal service).
        pub fn fail_with(&self, err: Option<KvError>) {
            self.with(|i| i.fail = err);
        }

        /// Makes writes of values longer than `len` bytes return [`KvError::Other`].
        pub fn set_max_value_len(&self, len: Option<usize>) {
            self.with(|i| i.max_value_len = len);
        }

        /// The injected failure, if any.
        fn check(&self) -> Result<(), KvError> {
            self.with(|i| i.fail.clone()).map_or(Ok(()), Err)
        }

        /// The injected failure or an oversized `value`.
        fn check_write(&self, value: Option<&[u8]>) -> Result<(), KvError> {
            self.check()?;
            let max = self.with(|i| i.max_value_len);
            match (max, value) {
                (Some(max), Some(v)) if v.len() > max => Err(KvError::Other(format!(
                    "value of {} bytes exceeds {max}",
                    v.len()
                ))),
                _ => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl KvBackend for MemKv {
        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
            self.check()?;
            Ok(self.with(|i| i.registers.get(key).and_then(|r| r.value.clone())))
        }

        async fn set(&self, key: &str, value: Vec<u8>) -> Result<(), KvError> {
            self.check_write(Some(&value))?;
            let reg = Register {
                order: None,
                value: Some(value),
            };
            self.with(|i| i.registers.insert(key.to_string(), reg));
            Ok(())
        }

        async fn set_ordered(
            &self,
            key: &str,
            order: Vec<u8>,
            value: Option<Vec<u8>>,
        ) -> Result<(), KvError> {
            self.check_write(value.as_deref())?;
            self.with(|i| {
                // A later write with an equal order wins, as a later HLC would.
                let loses = i
                    .registers
                    .get(key)
                    .and_then(|r| r.order.as_ref())
                    .is_some_and(|current| *current > order);
                if !loses {
                    let reg = Register {
                        order: Some(order),
                        value,
                    };
                    i.registers.insert(key.to_string(), reg);
                }
            });
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), KvError> {
            self.check()?;
            self.with(|i| {
                if let Some(r) = i.registers.get_mut(key) {
                    r.value = None;
                }
            });
            Ok(())
        }

        async fn list_keys(
            &self,
            prefix: &str,
            start_after: Option<&str>,
            limit: u32,
        ) -> Result<Vec<String>, KvError> {
            self.check()?;
            let lower = match start_after {
                Some(after) => Bound::Excluded(after.to_string()),
                None => Bound::Unbounded,
            };
            Ok(self.with(|i| {
                i.registers
                    .range((lower, Bound::Unbounded))
                    .filter(|(k, r)| k.starts_with(prefix) && r.value.is_some())
                    .map(|(k, _)| k.clone())
                    .take(limit as usize)
                    .collect()
            }))
        }

        async fn set_add(&self, set: &str, member: Vec<u8>) -> Result<(), KvError> {
            self.check_write(Some(&member))?;
            self.with(|i| i.sets.entry(set.to_string()).or_default().insert(member));
            Ok(())
        }

        async fn set_contains(&self, set: &str, member: &[u8]) -> Result<bool, KvError> {
            self.check()?;
            Ok(self.with(|i| i.sets.get(set).is_some_and(|s| s.contains(member))))
        }

        async fn set_members(
            &self,
            set: &str,
            start_after: Option<&[u8]>,
            limit: u32,
        ) -> Result<Vec<Vec<u8>>, KvError> {
            self.check()?;
            let lower = match start_after {
                Some(after) => Bound::Excluded(after.to_vec()),
                None => Bound::Unbounded,
            };
            Ok(self.with(|i| {
                i.sets.get(set).map_or_else(Vec::new, |s| {
                    s.range((lower, Bound::Unbounded))
                        .take(limit as usize)
                        .cloned()
                        .collect()
                })
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KvBackend, MemKv};

    #[tokio::test]
    async fn mem_kv_registers_and_sets() {
        let kv = MemKv::default();
        kv.set("a/1", b"x".to_vec()).await.unwrap();
        kv.set("a/2", b"y".to_vec()).await.unwrap();
        kv.set("b/1", b"z".to_vec()).await.unwrap();
        kv.delete("a/2").await.unwrap();
        assert_eq!(kv.get("a/1").await.unwrap(), Some(b"x".to_vec()));
        assert_eq!(kv.get("a/2").await.unwrap(), None);
        assert_eq!(kv.list_keys("a/", None, 10).await.unwrap(), ["a/1"]);
        assert_eq!(kv.list_keys("", Some("a/1"), 10).await.unwrap(), ["b/1"]);

        kv.set_ordered("o", vec![2], Some(b"two".to_vec()))
            .await
            .unwrap();
        kv.set_ordered("o", vec![1], Some(b"one".to_vec()))
            .await
            .unwrap();
        assert_eq!(kv.get("o").await.unwrap(), Some(b"two".to_vec()));
        kv.set_ordered("o", vec![3], None).await.unwrap();
        assert_eq!(kv.get("o").await.unwrap(), None);

        for m in [b"m2", b"m1", b"m3"] {
            kv.set_add("s", m.to_vec()).await.unwrap();
        }
        assert!(kv.set_contains("s", b"m1").await.unwrap());
        assert!(!kv.set_contains("s", b"m4").await.unwrap());
        let after: &[u8] = b"m1";
        assert_eq!(
            kv.set_members("s", Some(after), 1).await.unwrap(),
            [b"m2".to_vec()]
        );
    }
}
