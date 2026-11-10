//! Meshnode services: manifests in `_mesh`, deploy/undeploy/purge, and the KV adapter that
//! gives each service its namespace of the replicated store; placement, the deployer that
//! fetches and loads what this node runs, request dispatch and status.

mod deployer;
mod dispatch;
mod error;
mod kv;
mod manifest;
mod ops;
mod placement;
mod status;

pub use deployer::DeployerTimings;
pub use error::ServicesError;
pub use kv::ServiceKv;
pub use manifest::{current_generation, data_ns, gen_key, manifest_key};
pub use ops::{DeployOpts, Services, ServicesConfig};
pub use placement::{placed, placed_hosts};
pub use status::{LocalStatus, ServiceStatus};
