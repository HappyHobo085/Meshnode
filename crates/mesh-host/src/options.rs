//! Host-specific start options: what differs between a desktop daemon and an embedded host.

use std::sync::Arc;

use crate::power::{Periodic, WakeLockSink};

/// Options that differ between hosts; the default is the desktop daemon.
#[derive(Clone)]
pub struct HostOptions {
    /// Serve the admin API on `<data>/admin.sock` (or the named pipe).
    pub admin_socket: bool,
    pub periodic: Periodic,
    /// Wake locks for the power policy (Android); `None` takes none.
    pub power: Option<Arc<dyn WakeLockSink>>,
    /// The product version the node reports to peers (`NodeConfig::product_version`). A
    /// host sets its own package version, which `scripts/set-version.sh` maintains; the
    /// default is this crate's version.
    pub product_version: String,
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions {
            admin_socket: true,
            periodic: Periodic::Timers,
            power: None,
            product_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}
