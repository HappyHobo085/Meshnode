//! Which hosts run a service (spec §5.1): the rules live in `mesh-forward`, which a thin
//! client shares; the deployer and dispatch use them here.

pub(crate) use mesh_forward::{hosts_among, placed_among};
pub use mesh_forward::{placed, placed_hosts};
