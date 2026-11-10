//! `meshd`: the Meshnode desktop daemon (spec §6) — data directory and `meshd.toml`,
//! logging, and the [`Daemon`] that runs a node, its services, the localhost gateway and
//! the admin API.

pub mod config;
mod daemon;
pub mod logging;
pub mod paths;

pub use config::{ConfigError, MeshdConfig};
#[cfg(feature = "test-hooks")]
pub use daemon::TestOptions;
pub use daemon::{Daemon, DaemonError};
