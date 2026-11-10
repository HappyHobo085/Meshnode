//! Meshnode's WASM service host: runs `wasi:http` components targeting world
//! `meshnode:service/service` (`wit/world.wit`) on wasmtime, one fresh instance per request.

mod cache;
mod engine;
mod error;
mod handle;
mod host;
mod kv;
mod outbound;
mod ram;
mod stdio;
#[cfg(any(test, feature = "test-hooks"))]
pub mod test_fixtures;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::Ordering;
use std::time::Instant;

use bytes::Bytes;
use mesh_proto::{Limits, NodeId};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wasmtime::Engine;
use wasmtime::component::Linker;
use wasmtime_wasi_http::p2::bindings::ProxyPre;

pub use error::RuntimeError;
#[cfg(any(test, feature = "test-hooks"))]
pub use kv::MemKv;
pub use kv::{KvBackend, KvError};
pub use ram::default_memory_budget_mb;

use cache::Cache;
use engine::EpochTicker;
use handle::Shared;
use host::State;

/// Runtime settings.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub cache: CodeCache,
    /// Requests served at once by this node (at least 1); further requests wait, and the
    /// wait counts against their timeout.
    pub max_in_flight: usize,
    /// The node-wide memory budget in MiB (at least 1): every WASM request reserves its
    /// `limits.memory_mb` from it before instantiation. Default:
    /// [`default_memory_budget_mb`].
    pub max_memory_mb: u32,
    /// Small, movable linear memories instead of a 4 GiB address-space reservation each
    /// (see `engine::new_engine`). Default: on Android only.
    pub compact_memory: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            cache: CodeCache::None,
            max_in_flight: 64,
            max_memory_mb: default_memory_budget_mb(),
            compact_memory: cfg!(target_os = "android"),
        }
    }
}

/// One of the runtime's in-flight slots, held by work served outside WASM (static sites);
/// released when dropped.
#[derive(Debug)]
pub struct SlotGuard {
    _permit: OwnedSemaphorePermit,
}

/// Where compiled components are cached between runs.
#[derive(Clone, Debug)]
pub enum CodeCache {
    None,
    /// Desktop: cache files are memory-mapped.
    Mmap(PathBuf),
    /// Android: cache files are read into memory and deserialized, never memory-mapped.
    Read(PathBuf),
}

/// Everything a request needs from the embedder.
#[derive(Clone)]
pub struct RequestCtx {
    pub service: String,
    pub node_id: NodeId,
    pub kv: Arc<dyn KvBackend>,
    pub limits: Limits,
    /// `host:port` destinations the service may call.
    pub outbound: Vec<String>,
}

/// One engine and linker per node; owns the epoch ticker.
pub struct Runtime {
    engine: Engine,
    linker: Linker<State>,
    shared: Shared,
    cache: Arc<Cache>,
    _ticker: EpochTicker,
}

/// A validated, compiled component ready to serve requests. Cheap to clone.
#[derive(Clone)]
pub struct Loaded {
    hash: [u8; 32],
    pre: ProxyPre<State>,
}

impl fmt::Debug for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hash: String = self.hash[..5].iter().map(|b| format!("{b:02x}")).collect();
        f.debug_struct("Loaded").field("hash", &hash).finish()
    }
}

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Result<Runtime, RuntimeError> {
        let RuntimeConfig {
            cache,
            max_in_flight,
            max_memory_mb,
            compact_memory,
        } = config;
        let engine = engine::new_engine(compact_memory)?;
        let linker = host::world_linker(&engine)?;
        let ticker = EpochTicker::start(&engine)?;
        let cache = Arc::new(Cache::new(cache, &engine));
        let shared = Shared {
            in_flight: Arc::new(Semaphore::new(
                max_in_flight.clamp(1, Semaphore::MAX_PERMITS),
            )),
            max_memory_mb: max_memory_mb.max(1),
            memory: Arc::new(Semaphore::new(max_memory_mb.max(1) as usize)),
            instantiations: Arc::default(),
            ticker: ticker.handle(),
        };
        Ok(Runtime {
            engine,
            linker,
            shared,
            cache,
            _ticker: ticker,
        })
    }

    /// Compiles `wasm` (or takes it from the code cache) and validates it against the
    /// world: it must export `wasi:http/incoming-handler` and import nothing outside the
    /// world. `hash` is trusted to identify `wasm` (callers pass the blob hash); it keys
    /// the cache and is not checked against the bytes.
    pub async fn load(&self, hash: [u8; 32], wasm: &[u8]) -> Result<Loaded, RuntimeError> {
        let engine = self.engine.clone();
        let wasm = wasm.to_vec();
        let cache = Arc::clone(&self.cache);
        let component = tokio::task::spawn_blocking(move || cache.load(&engine, &hash, &wasm))
            .await
            .map_err(|e| RuntimeError::Compile(format!("compile task: {e}")))?
            .map_err(|e| RuntimeError::Compile(format!("{e:#}")))?;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .map_err(|e| RuntimeError::Invalid(format!("{e:#}")))?;
        let pre = ProxyPre::new(pre).map_err(|e| RuntimeError::Invalid(format!("{e:#}")))?;
        Ok(Loaded { hash, pre })
    }

    /// Serves one request in a fresh instance. Never fails: errors become HTTP statuses.
    pub async fn handle(
        &self,
        svc: &Loaded,
        req: http::Request<Bytes>,
        ctx: RequestCtx,
    ) -> http::Response<Bytes> {
        handle::serve(&svc.pre, &self.shared, req, ctx).await
    }

    /// Takes an in-flight slot, waiting at most until `deadline`; `None` if none freed up
    /// by then. For requests served without WASM, which count against `max_in_flight`
    /// like WASM requests do.
    pub async fn acquire_slot(&self, deadline: Instant) -> Option<SlotGuard> {
        let acquire = self.shared.in_flight.clone().acquire_owned();
        match tokio::time::timeout_at(deadline.into(), acquire).await {
            Ok(Ok(permit)) => Some(SlotGuard { _permit: permit }),
            // The semaphore is never closed; a timeout is the only real outcome here.
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Code cache `(hits, misses)` since this runtime was created.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn cache_stats(&self) -> (u64, u64) {
        self.cache.stats()
    }

    /// How many times the epoch ticker has advanced the engine's epoch (it only ticks
    /// while a request is in flight).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn epoch_ticks(&self) -> u64 {
        self.shared.ticker.ticks()
    }

    /// How many component instances this runtime has created.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn instantiations(&self) -> u64 {
        self.shared.instantiations.load(Ordering::Relaxed)
    }
}
