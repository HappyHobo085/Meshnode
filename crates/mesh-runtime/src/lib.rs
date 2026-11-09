//! Meshnode's WASM service host: runs `wasi:http` components targeting world
//! `meshnode:service/service` (`wit/world.wit`) on wasmtime, one fresh instance per request.

mod engine;
mod error;
mod handle;
mod host;
mod kv;
mod outbound;
mod stdio;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::Ordering;

use bytes::Bytes;
use mesh_proto::{Limits, NodeId};
use tokio::sync::Semaphore;
use wasmtime::Engine;
use wasmtime::component::{Component, Linker};
use wasmtime_wasi_http::p2::bindings::ProxyPre;

pub use error::RuntimeError;
#[cfg(any(test, feature = "test-hooks"))]
pub use kv::MemKv;
pub use kv::{KvBackend, KvError};

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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            cache: CodeCache::None,
            max_in_flight: 64,
        }
    }
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
        // `config.cache` is wired in with the code cache.
        let RuntimeConfig {
            cache: _,
            max_in_flight,
        } = config;
        let engine = engine::new_engine()?;
        let linker = host::world_linker(&engine)?;
        let ticker = EpochTicker::start(&engine)?;
        let shared = Shared {
            in_flight: Arc::new(Semaphore::new(
                max_in_flight.clamp(1, Semaphore::MAX_PERMITS),
            )),
            instantiations: Arc::default(),
        };
        Ok(Runtime {
            engine,
            linker,
            shared,
            _ticker: ticker,
        })
    }

    /// Compiles `wasm` and validates it against the world: it must export
    /// `wasi:http/incoming-handler` and import nothing outside the world.
    pub async fn load(&self, hash: [u8; 32], wasm: &[u8]) -> Result<Loaded, RuntimeError> {
        let engine = self.engine.clone();
        let wasm = wasm.to_vec();
        let component = tokio::task::spawn_blocking(move || Component::from_binary(&engine, &wasm))
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

    /// How many component instances this runtime has created.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn instantiations(&self) -> u64 {
        self.shared.instantiations.load(Ordering::Relaxed)
    }
}
