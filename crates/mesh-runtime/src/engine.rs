//! The wasmtime engine configuration and the epoch ticker that drives request deadlines.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use wasmtime::{Config, Engine, Strategy};

use crate::RuntimeError;

/// How often the engine's epoch advances; request deadlines are counted in these ticks.
pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(10);

/// One engine per node: Cranelift, epoch interruption for deadlines. Async host calls and
/// async instantiation come with wasmtime's `async` feature (`Config::async_support` is a
/// deprecated no-op in wasmtime 50).
pub(crate) fn new_engine() -> Result<Engine, RuntimeError> {
    let mut cfg = Config::new();
    cfg.strategy(Strategy::Cranelift);
    cfg.epoch_interruption(true);
    Engine::new(&cfg).map_err(|e| RuntimeError::Compile(format!("create engine: {e:#}")))
}

/// Advances the engine's epoch every [`EPOCH_TICK`] on a dedicated thread until dropped.
/// A plain thread, not a tokio task, so `Runtime::new` needs no async context.
pub(crate) struct EpochTicker {
    stop: Arc<AtomicBool>,
}

impl EpochTicker {
    pub(crate) fn start(engine: &Engine) -> Result<Self, RuntimeError> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let engine = engine.weak();
        std::thread::Builder::new()
            .name("mesh-runtime-epoch".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(EPOCH_TICK);
                    match engine.upgrade() {
                        Some(engine) => engine.increment_epoch(),
                        None => break,
                    }
                }
            })
            .map_err(|e| RuntimeError::Compile(format!("start epoch ticker: {e}")))?;
        Ok(EpochTicker { stop })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
