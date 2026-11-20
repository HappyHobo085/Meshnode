//! Gateway configuration (spec §4.1).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use mesh_proto::valid_service_name;

use crate::error::GatewayError;

/// Where the gateway listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayConfig {
    /// The main listener; must be loopback.
    pub listen: SocketAddr,
    /// Extra listeners on `listen`'s IP: port to the service it serves. A port of 0 picks
    /// a free one (tests).
    pub ports: BTreeMap<u16, String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7777),
            ports: BTreeMap::new(),
        }
    }
}

impl GatewayConfig {
    /// Rejects a non-loopback address, an invalid service name, or a mapping that
    /// collides with the main port.
    pub fn validate(&self) -> Result<(), GatewayError> {
        if !self.listen.ip().is_loopback() {
            return Err(GatewayError::Invalid(format!(
                "gateway.listen {} is not a loopback address",
                self.listen
            )));
        }
        for (port, svc) in &self.ports {
            if !valid_service_name(svc) {
                return Err(GatewayError::Invalid(format!(
                    "port mapping {port}: invalid service name {svc:?}"
                )));
            }
            if *port != 0 && *port == self.listen.port() {
                return Err(GatewayError::Invalid(format!(
                    "port mapping {port} is the main gateway port"
                )));
            }
        }
        Ok(())
    }
}
