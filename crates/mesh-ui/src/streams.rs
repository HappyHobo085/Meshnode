//! Live views (log follow, probe): a cap on how many run at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{UiError, UiErrorCode};

/// Most live views open at once.
pub const MAX_STREAMS: usize = 4;

#[derive(Clone, Default)]
pub struct Streams {
    live: Arc<AtomicUsize>,
}

/// Holds one stream slot until dropped.
pub struct StreamGuard {
    live: Arc<AtomicUsize>,
}

impl Streams {
    pub fn new() -> Streams {
        Streams::default()
    }

    /// Claims a slot, or refuses with `Busy` when [`MAX_STREAMS`] are already live.
    pub fn begin(&self) -> Result<StreamGuard, UiError> {
        let mut n = self.live.load(Ordering::Acquire);
        let claimed = loop {
            if n >= MAX_STREAMS {
                break false;
            }
            match self
                .live
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break true,
                Err(now) => n = now,
            }
        };
        match claimed {
            true => Ok(StreamGuard {
                live: self.live.clone(),
            }),
            false => Err(UiError::new(UiErrorCode::Busy, "Too many live views open")),
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}
