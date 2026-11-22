//! `meshd`: the Meshnode desktop daemon (spec §6) — the CLI and OS services around the
//! platform-neutral [`mesh_host`] daemon.

pub mod service;

pub use mesh_host::HostConfig as MeshdConfig;

/// The Meshnode product version: this crate's, kept by `scripts/set-version.sh`.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The daemon's host options: the desktop defaults, reporting [`PRODUCT_VERSION`] to peers.
pub fn host_options() -> mesh_host::HostOptions {
    mesh_host::HostOptions {
        product_version: PRODUCT_VERSION.into(),
        ..mesh_host::HostOptions::default()
    }
}
#[cfg(feature = "test-hooks")]
pub use mesh_host::TestOptions;
pub use mesh_host::{
    ConfigError, Daemon, DaemonError, config, lock, logging, paths, record_start_failure,
    wipe_identity,
};
