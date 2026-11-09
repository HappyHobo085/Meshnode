//! Meshnode services: manifests in `_mesh`, deploy/undeploy/purge, and the KV adapter that
//! gives each service its namespace of the replicated store.

mod error;
mod kv;
mod manifest;
mod ops;

pub use error::ServicesError;
pub use kv::ServiceKv;
pub use manifest::{current_generation, data_ns, gen_key, manifest_key};
pub use ops::{DeployOpts, Services, ServicesConfig};
