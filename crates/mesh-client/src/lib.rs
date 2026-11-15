//! `mesh-client`: the Meshnode SDK for apps (sub-spec 8a §4).
//!
//! A [`MeshClient`] is a client-role mesh member: it keeps its identity in a data
//! directory, replicates only the membership namespace (`_mesh`), and sends HTTP requests
//! to the services that the mesh's hosts run, over `httpfwd`, nearest online host first.
//! It never runs services, never serves `httpfwd`, never opens a gateway and never fetches
//! blobs. See the README for a walk-through.

mod client;
mod error;
mod events;

pub use client::{ClientOptions, ClientStatus, Member, MeshClient, ServiceInfo};
pub use error::ClientError;
pub use events::ClientEvent;
/// Types in this crate's API.
pub use mesh_proto::{NodeId, Role};
