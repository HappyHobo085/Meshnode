//! `mesh-host`: the platform-neutral Meshnode host — data directory and `meshd.toml`,
//! logging, and the [`Daemon`] that runs a node, its services, the localhost gateway and
//! the admin API, plus the Android hooks (periodic wake, power policy).

pub mod config;
mod daemon;
pub mod lock;
pub mod logging;
mod options;
pub mod paths;
mod power;

pub use config::{AndroidConfig, ConfigError, HostConfig};
#[cfg(feature = "test-hooks")]
pub use daemon::TestOptions;
pub use daemon::{Daemon, DaemonError, HostStatus, record_start_failure, wipe_identity};
pub use mesh_core::WakeOutcome;
pub use options::HostOptions;
pub use power::{
    CATCH_UP_AFTER, LockActivity, Periodic, PowerPolicy, PowerState, WakeLockSink, WakeReason,
    saver_paused, wake_lock_for,
};
