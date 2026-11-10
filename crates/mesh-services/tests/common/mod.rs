//! Harness for the in-process node tests: loopback only, relay and discovery off, fast timings.
#![allow(dead_code, unused_imports)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mesh_core::{NodeConfig, RelayChoice, Timings};
use mesh_hlc::ManualClock;
use tokio::time::{Instant, sleep};

pub use mesh_runtime::test_fixtures::fixture;

/// Default timeout for a single await in the tests.
pub const T: Duration = Duration::from_secs(30);

pub fn test_cfg(dir: &Path, name: &str) -> NodeConfig {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1_700_000_000_000);
    let mut cfg = NodeConfig::new(dir, name);
    cfg.relay = RelayChoice::Disabled;
    cfg.discovery = false;
    cfg.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    cfg.timings = Timings::fast_for_tests();
    cfg.wall = Arc::new(ManualClock::new(now));
    cfg
}

/// Polls `cond` until it holds. Panics after `limit`.
pub async fn wait_until<F: FnMut() -> bool>(what: &str, limit: Duration, mut cond: F) {
    let deadline = Instant::now() + limit;
    while !cond() {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        sleep(Duration::from_millis(25)).await;
    }
}
