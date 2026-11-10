//! `mesh-host`: the platform-neutral Meshnode host — data directory and `meshd.toml`,
//! logging, and the [`Daemon`] that runs a node, its services, the localhost gateway and
//! the admin API.

pub mod config;
mod daemon;
pub mod lock;
pub mod logging;
pub mod paths;

pub use config::{ConfigError, MeshdConfig};
#[cfg(feature = "test-hooks")]
pub use daemon::TestOptions;
pub use daemon::{Daemon, DaemonError, wipe_identity};
