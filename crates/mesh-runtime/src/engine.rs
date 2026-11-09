//! The wasmtime engine configuration and the epoch ticker that drives request deadlines.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wasmtime::{Config, Engine, Strategy};

use crate::RuntimeError;

/// How often the engine's epoch advances; request deadlines are counted in these ticks.
pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(10);

/// The epoch deadline, in ticks from now, for a request with `left` to run: at least one
/// tick.
pub(crate) fn deadline_ticks(left: Duration) -> u64 {
    u64::try_from(left.as_nanos() / EPOCH_TICK.as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

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
///
/// Ticks follow elapsed time rather than counting sleeps, so a coarse OS timer (about
/// 15.6 ms on Windows) or a late wake-up does not stretch request deadlines.
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
                let start = Instant::now();
                let mut ticks: u128 = 0;
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(EPOCH_TICK);
                    let Some(engine) = engine.upgrade() else {
                        break;
                    };
                    let due = start.elapsed().as_nanos() / EPOCH_TICK.as_nanos();
                    while ticks < due {
                        engine.increment_epoch();
                        ticks += 1;
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
