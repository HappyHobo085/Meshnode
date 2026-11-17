//! `meshd`: the Meshnode desktop daemon (spec §6) — the CLI and OS services around the
//! platform-neutral [`mesh_host`] daemon.

pub mod service;

pub use mesh_host::HostConfig as MeshdConfig;
#[cfg(feature = "test-hooks")]
pub use mesh_host::TestOptions;
pub use mesh_host::{
    ConfigError, Daemon, DaemonError, config, lock, logging, paths, record_start_failure,
    wipe_identity,
};
