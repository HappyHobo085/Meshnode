//! Node configuration and timings.

use std::any::Any;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iroh::protocol::DynProtocolHandler;
use mesh_hlc::{SystemClock, WallClock};

/// Called when the incoming-traffic catch-up starts its rounds (see
/// [`NodeConfig::on_catch_up`]); the value it returns is held until they end.
pub type CatchUpHook = Arc<dyn Fn() -> Box<dyn Any + Send> + Send + Sync>;

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
    /// How often the blob store's GC loop wakes (it only marks, unless a `Blobs::remove`
    /// armed it). `remove` can take about 3 ticks.
    pub blob_gc_tick: Duration,
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
            blob_gc_tick: Duration::from_secs(30),
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
            blob_gc_tick: Duration::from_millis(200),
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
    /// Further protocols (ALPN, handler), served behind the membership gate like `sync/1`.
    pub extra_protocols: Vec<(Vec<u8>, Arc<dyn DynProtocolHandler>)>,
    /// Incoming peer traffic (an accepted `sync/1` connection or message) after at least
    /// this much silence, measured on `wall`, starts a round with every connected peer: a
    /// phone that slept catches up. `None` (the default) turns it off; see
    /// `Node::set_catch_up_on_incoming`.
    pub catch_up_on_incoming_after: Option<Duration>,
    /// Called each time the incoming-traffic catch-up starts a round with every connected
    /// peer; the value returned (a host's wake-lock guard) is dropped once each of those
    /// peers completed a round or disconnected, after at most 60 s, or at shutdown.
    /// `None` (the default): nothing is called.
    pub on_catch_up: Option<CatchUpHook>,
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
            extra_protocols: Vec::new(),
            catch_up_on_incoming_after: None,
            on_catch_up: None,
        }
    }
}
