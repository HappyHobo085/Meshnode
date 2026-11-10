//! What [`Services::services`] reports (spec §5.4).

use std::time::SystemTime;

use mesh_proto::{NodeId, ServiceManifest};

use crate::Services;
use crate::placement::hosts_among;

/// One deployed service as this node sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatus {
    pub manifest: ServiceManifest,
    /// The current data generation (its namespace is `data_ns(name, generation)`).
    pub generation: u64,
    pub local: LocalStatus,
    /// Effective hosts the placement selects; empty is a warning for the UI.
    pub placed_hosts: Vec<NodeId>,
    /// Bytes in the current data namespace, as of the deployer's last periodic rescan.
    pub data_bytes: u64,
}

/// The service on this node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalStatus {
    NotPlaced,
    /// Placed here; the wanted module or root is being fetched and loaded. A previous
    /// version, if any, serves meanwhile.
    Fetching,
    /// Serving the manifest at `version`.
    Ready {
        version: u64,
    },
    /// The last fetch or load failed; it is retried at `retry_at`. A previous version, if
    /// any, still serves.
    Failed {
        error: String,
        retry_at: SystemTime,
    },
}

impl Services {
    /// Every deployed service, sorted by name, as of the deployer's last scan (placed hosts
    /// are computed now).
    pub fn services(&self) -> Vec<ServiceStatus> {
        let members = self.shared.node.members();
        self.shared.with_known(|known| {
            known
                .values()
                .map(|k| ServiceStatus {
                    manifest: k.manifest.clone(),
                    generation: k.generation,
                    local: k.local.clone(),
                    placed_hosts: hosts_among(&k.manifest, &members),
                    data_bytes: k.data_bytes,
                })
                .collect()
        })
    }
}
