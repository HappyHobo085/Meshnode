//! Host-specific start options: what differs between a desktop daemon and an embedded host.

use std::sync::Arc;

/// How periodic work is scheduled. Stub: filled in with the Android host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Periodic {
    /// The host runs its own timers.
    #[default]
    Timers,
    /// The embedding platform drives periodic work.
    Driven,
}

/// Receives wake-lock requests from the host. Stub: filled in with the Android host.
pub trait WakeLockSink: Send + Sync {
    fn acquire(&self, tag: &str, max_ms: u64);
    fn release(&self, tag: &str);
}

/// Options that differ between hosts; the default is the desktop daemon.
#[derive(Clone)]
pub struct HostOptions {
    /// Serve the admin API on `<data>/admin.sock` (or the named pipe).
    pub admin_socket: bool,
    pub periodic: Periodic,
    pub power: Option<Arc<dyn WakeLockSink>>,
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions {
            admin_socket: true,
            periodic: Periodic::Timers,
            power: None,
        }
    }
}
