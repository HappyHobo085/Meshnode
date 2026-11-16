//! Meshnode forwarding without a runtime: which hosts run a service (placement), how
//! manifests are read from `_mesh`, and the client side of `httpfwd` (a connection cache,
//! RTT-ranked failover, frame conversion). The services and gateway crates build on it; a
//! thin client uses [`RemoteForwarder`] alone.

mod frame;
mod placement;
mod remote;

pub use frame::{FrameConvError, Unframeable, copy_headers, frame_headers};
pub use placement::{
    hosts_among, list_manifests, manifest_key, placed, placed_among, placed_hosts, read_manifest,
};
pub use remote::{ForwardError, PingError, PingTiming, RemoteForwarder, ping};
