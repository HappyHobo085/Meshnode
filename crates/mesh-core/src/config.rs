//! Node configuration and timings.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mesh_hlc::{SystemClock, WallClock};

/// Wrapper that lets a shared `dyn WallClock` be used as the store's clock type.
#[derive(Clone)]
pub struct DynClock(pub Arc<dyn WallClock>);

impl WallClock for DynClock {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

#[derive(Clone, Debug)]
pub struct Timings {
    pub sync_interval: Duration,
    pub silence: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub retry_pending: Duration,
    pub compact: Duration,
    pub gc: Duration,
    pub invite_prune: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            sync_interval: Duration::from_secs(60),
            silence: Duration::from_secs(60),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
            retry_pending: Duration::from_secs(60),
            compact: Duration::from_secs(3600),
            gc: Duration::from_secs(24 * 3600),
            invite_prune: Duration::from_secs(3600),
        }
    }
}

impl Timings {
    /// Short intervals for tests; maintenance jobs are set to 1 h so tests trigger them explicitly.
    pub fn fast_for_tests() -> Timings {
        let hour = Duration::from_secs(3600);
        Timings {
            sync_interval: Duration::from_millis(200),
            silence: Duration::from_secs(1),
            backoff_min: Duration::from_millis(50),
            backoff_max: Duration::from_secs(1),
            retry_pending: hour,
            compact: hour,
            gc: hour,
            invite_prune: hour,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayChoice {
    Default,
    Custom(String),
    Disabled,
}

#[derive(Clone)]
pub struct NodeConfig {
    pub data_dir: PathBuf,
    pub name: String,
    pub platform: String,
    pub tags: Vec<String>,
    pub relay: RelayChoice,
    pub discovery: bool,
    pub bind_addr: Option<SocketAddr>,
    pub timings: Timings,
    pub wall: Arc<dyn WallClock>,
}

impl NodeConfig {
    pub fn new(data_dir: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        NodeConfig {
            data_dir: data_dir.into(),
            name: name.into(),
            platform: std::env::consts::OS.to_owned(),
            tags: Vec::new(),
            relay: RelayChoice::Default,
            discovery: true,
            bind_addr: None,
            timings: Timings::default(),
            wall: Arc::new(SystemClock),
        }
    }
}
