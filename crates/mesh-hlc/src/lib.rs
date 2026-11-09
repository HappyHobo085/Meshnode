//! Hybrid logical clock: 48-bit Unix milliseconds | 16-bit logical counter.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum tolerated remote-ahead-of-wall skew (5 minutes).
pub const MAX_SKEW_MS: u64 = 300_000;

const MS_LIMIT: u64 = 1 << 48;

/// 48-bit Unix milliseconds | 16-bit logical counter, totally ordered as a `u64`.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
pub struct Hlc(u64);

impl Hlc {
    pub const ZERO: Hlc = Hlc(0);

    /// Panics if `ms >= 2^48`.
    pub fn new(ms: u64, counter: u16) -> Hlc {
        assert!(ms < MS_LIMIT, "Hlc ms out of 48-bit range");
        Hlc((ms << 16) | u64::from(counter))
    }

    pub fn ms(self) -> u64 {
        self.0 >> 16
    }

    pub fn counter(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn from_u64(v: u64) -> Hlc {
        Hlc(v)
    }
}

/// Source of wall-clock milliseconds.
pub trait WallClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Wall clock backed by the system time.
#[derive(Copy, Clone, Debug, Default)]
pub struct SystemClock;

impl WallClock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Settable clock for tests; clones share the same time.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn new(ms: u64) -> Self {
        ManualClock(Arc::new(AtomicU64::new(ms)))
    }

    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl WallClock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Returned by [`Clock::observe`] when the remote timestamp is too far ahead.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct Skew {
    pub remote_ms: u64,
    pub wall_ms: u64,
}

impl std::fmt::Display for Skew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "remote clock {} ms is too far ahead of wall clock {} ms",
            self.remote_ms, self.wall_ms
        )
    }
}

impl std::error::Error for Skew {}

pub struct Clock<W: WallClock> {
    last: Hlc,
    wall: W,
}

/// Build the next timestamp at `ms`, where `prev` is the highest counter already
/// used at that `ms` (if any). Overflow carries into `ms`.
fn next(ms: u64, prev: Option<u16>) -> Hlc {
    match prev {
        None => Hlc::new(ms, 0),
        Some(c) => match c.checked_add(1) {
            Some(n) => Hlc::new(ms, n),
            None => Hlc::new(ms + 1, 0),
        },
    }
}

impl<W: WallClock> Clock<W> {
    /// `floor` is the highest HLC already persisted; the clock never goes below it.
    pub fn new(wall: W, floor: Hlc) -> Self {
        Clock { last: floor, wall }
    }

    /// Local event.
    pub fn tick(&mut self) -> Hlc {
        let ms = self.wall.now_ms().max(self.last.ms());
        let prev = (ms == self.last.ms()).then(|| self.last.counter());
        self.last = next(ms, prev);
        self.last
    }

    /// Receive event. Errs (without advancing) if `remote` is more than
    /// [`MAX_SKEW_MS`] ahead of the wall clock.
    pub fn observe(&mut self, remote: Hlc) -> Result<Hlc, Skew> {
        let wall = self.wall.now_ms();
        if remote.ms() > wall.saturating_add(MAX_SKEW_MS) {
            return Err(Skew {
                remote_ms: remote.ms(),
                wall_ms: wall,
            });
        }
        let ms = wall.max(self.last.ms()).max(remote.ms());
        let in_last = (ms == self.last.ms()).then(|| self.last.counter());
        let in_remote = (ms == remote.ms()).then(|| remote.counter());
        let prev = match (in_last, in_remote) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.last = next(ms, prev);
        Ok(self.last)
    }

    pub fn last(&self) -> Hlc {
        self.last
    }

    pub fn wall(&self) -> &W {
        &self.wall
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_ms_then_counter() {
        assert!(Hlc::new(5, 9) < Hlc::new(6, 0));
        assert!(Hlc::new(5, 1) < Hlc::new(5, 2));
    }

    #[test]
    fn tick_is_monotonic_when_wall_goes_backwards() {
        let w = ManualClock::new(1_000);
        let mut c = Clock::new(w.clone(), Hlc::ZERO);
        let a = c.tick();
        w.set(500);
        let b = c.tick();
        assert!(b > a);
        assert_eq!(b.ms(), 1_000);
        assert_eq!(b.counter(), a.counter() + 1);
    }

    #[test]
    fn counter_overflow_carries_into_ms() {
        let w = ManualClock::new(1_000);
        let mut c = Clock::new(w, Hlc::new(1_000, u16::MAX));
        assert_eq!(c.tick(), Hlc::new(1_001, 0));
    }

    #[test]
    fn observe_rejects_skew_and_does_not_advance() {
        let w = ManualClock::new(1_000_000);
        let mut c = Clock::new(w, Hlc::ZERO);
        let before = c.last();
        assert!(c.observe(Hlc::new(1_000_000 + MAX_SKEW_MS + 1, 0)).is_err());
        assert_eq!(c.last(), before);
        assert!(c.observe(Hlc::new(1_000_000 + MAX_SKEW_MS, 0)).is_ok());
    }

    #[test]
    fn observe_result_exceeds_remote_and_local() {
        let w = ManualClock::new(1_000);
        let mut c = Clock::new(w, Hlc::ZERO);
        let r = Hlc::new(1_200, 7);
        let got = c.observe(r).unwrap();
        assert!(got > r);
    }

    #[test]
    fn floor_is_respected() {
        let w = ManualClock::new(10);
        let mut c = Clock::new(w, Hlc::new(5_000, 3));
        assert!(c.tick() > Hlc::new(5_000, 3));
    }
}
