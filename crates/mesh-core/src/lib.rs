//! Meshnode node core: configuration, node-local files, errors, the async store handle,
//! the `sync/1` session, the peer manager and the `Node` that runs them.

pub mod blobs;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod frame;
mod join;
pub mod local;
mod maint;
pub mod membership;
pub mod node;
mod peers;
mod reactor;
mod rebootstrap;
#[doc(hidden)]
pub mod sync;
#[doc(hidden)]
pub mod transport;
pub mod version;
#[cfg(windows)]
mod winfs;

pub use blobs::{BlobHash, BlobKind, Blobs};
pub use config::{CatchUpHook, DynClock, NodeConfig, RelayChoice, Timings};
pub use db::{CoreStore, StoreHandle, StoreReader};
pub use error::NodeError;
pub use events::NodeEvent;
/// Types that appear in the extra-protocol and `NodeHandle::connect` API.
pub use iroh::endpoint::Connection;
pub use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler};
pub use maint::MaintJob;
/// A member's versions (see [`NodeHandle::members`]).
pub use mesh_proto::PeerVersion;
/// Store types that appear in this crate's API (store errors and change events).
pub use mesh_store::{Change, ChangeKind, StoreError};
pub use node::{MemberStatus, Node, NodeHandle, NodeStatus, WakeOutcome};
pub use version::CurrentVersion;
