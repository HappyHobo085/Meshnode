//! `meshd`: the Meshnode desktop daemon (spec §6) — the CLI and OS services around the
//! platform-neutral [`mesh_host`] daemon.

pub mod service;

pub use mesh_host::{
    ConfigError, Daemon, DaemonError, MeshdConfig, config, lock, logging, paths, wipe_identity,
};
#[cfg(feature = "test-hooks")]
pub use mesh_host::TestOptions;
