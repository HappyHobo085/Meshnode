//! The wasmtime engine configuration and the epoch ticker that drives request deadlines.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use wasmtime::{Config, Engine, Strategy};

use crate::RuntimeError;

/// How often the engine's epoch advances: a running guest yields and checks its request
/// deadline once per tick.
pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Guard region after (and before) each linear memory with compact memories.
const COMPACT_GUARD: u64 = 64 << 10;
/// Address space reserved for growth when a compact memory is relocated.
const COMPACT_GROWTH: u64 = 16 << 20;

/// One engine per node: Cranelift, epoch interruption for deadlines. Async host calls and
/// async instantiation come with wasmtime's `async` feature (`Config::async_support` is a
/// deprecated no-op in wasmtime 50).
///
/// `compact` trades bounds-check elision for address space. By default (64-bit hosts)
/// every linear memory reserves 4 GiB plus a 32 MiB guard on each side, and 2 GiB more
/// when it moves; Android limits a process's address space far below what many such
/// memories need. Compact: `memory_reservation(0)` allocates only the memory's current
/// size, `memory_may_move(true)` lets it be relocated on growth (copying its contents),
/// `memory_reservation_for_growth(16 MiB)` bounds the slack after each move, and
/// `memory_guard_size(64 KiB)` keeps a small guard (compiled code bounds-checks every
/// access then). The per-service `memory_mb` cap is enforced by the store's limiter
/// either way.
pub(crate) fn new_engine(compact: bool) -> Result<Engine, RuntimeError> {
    let mut cfg = Config::new();
    cfg.strategy(Strategy::Cranelift);
    cfg.epoch_interruption(true);
    if compact {
        cfg.memory_reservation(0)
            .memory_may_move(true)
            .memory_reservation_for_growth(COMPACT_GROWTH)
            .memory_guard_size(COMPACT_GUARD);
    }
    Engine::new(&cfg).map_err(|e| RuntimeError::Compile(format!("create engine: {e:#}")))
}

/// Advances the engine's epoch every [`EPOCH_TICK`] on a dedicated thread while at least
/// one request is in flight, and parks it otherwise; stops when dropped. A plain thread,
/// not a tokio task, so `Runtime::new` needs no async context. Deadlines are compared
/// against the wall clock in the epoch callback, so ticks only need to keep coming, not
/// to be counted exactly.
pub(crate) struct EpochTicker {
    ctl: Arc<Ctl>,
}

struct Ctl {
    state: Mutex<TickState>,
    wake: Condvar,
    /// Epoch increments so far (a test hook reads it).
    ticks: AtomicU64,
}

#[derive(Default)]
struct TickState {
    /// Requests in flight.
    busy: usize,
    stop: bool,
}

impl Ctl {
    fn lock(&self) -> MutexGuard<'_, TickState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EpochTicker {
    pub(crate) fn start(engine: &Engine) -> Result<Self, RuntimeError> {
        let ctl = Arc::new(Ctl {
            state: Mutex::default(),
            wake: Condvar::new(),
            ticks: AtomicU64::new(0),
        });
        let thread_ctl = ctl.clone();
        let engine = engine.weak();
        std::thread::Builder::new()
            .name("mesh-runtime-epoch".into())
            .spawn(move || {
                let ctl = thread_ctl;
                loop {
                    {
                        let mut st = ctl.lock();
                        while st.busy == 0 && !st.stop {
                            st = ctl.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
                        }
                        if st.stop {
                            break;
                        }
                    }
                    std::thread::sleep(EPOCH_TICK);
                    let Some(engine) = engine.upgrade() else {
                        break;
                    };
                    engine.increment_epoch();
                    ctl.ticks.fetch_add(1, Ordering::Relaxed);
                }
            })
            .map_err(|e| RuntimeError::Compile(format!("start epoch ticker: {e}")))?;
        Ok(EpochTicker { ctl })
    }

    /// A handle requests use to keep the ticker running.
    pub(crate) fn handle(&self) -> TickerHandle {
        TickerHandle(self.ctl.clone())
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.ctl.lock().stop = true;
        self.ctl.wake.notify_all();
    }
}

#[derive(Clone)]
pub(crate) struct TickerHandle(Arc<Ctl>);

impl TickerHandle {
    /// Keeps the epoch ticking until the returned guard is dropped.
    pub(crate) fn busy(&self) -> Busy {
        let mut st = self.0.lock();
        st.busy += 1;
        if st.busy == 1 {
            self.0.wake.notify_all();
        }
        Busy(self.0.clone())
    }

    /// Epoch increments so far.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn ticks(&self) -> u64 {
        self.0.ticks.load(Ordering::Relaxed)
    }
}

/// One request in flight; see [`TickerHandle::busy`].
pub(crate) struct Busy(Arc<Ctl>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.lock().busy -= 1;
    }
}
