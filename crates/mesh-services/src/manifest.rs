//! Where manifests and data generations live in `_mesh`, and how they are read.

use mesh_core::membership::MESH_NS;
use mesh_core::{NodeError, StoreHandle};
use mesh_proto::{ServiceManifest, decode_raw};

use crate::error::ServicesError;

const PAGE: u32 = 256;

/// The largest WASM module a service may have (64 MiB): bigger ones are refused at deploy
/// and never read into memory by the deployer.
pub(crate) const MAX_MODULE_BYTES: u64 = 64 << 20;

/// Why a module of `len` bytes is refused, if it is.
pub(crate) fn module_too_large(len: u64) -> Option<String> {
    (len > MAX_MODULE_BYTES).then(|| {
        format!(
            "module of {len} bytes exceeds the {} MiB limit",
            MAX_MODULE_BYTES >> 20
        )
    })
}

/// `_mesh` register holding the manifest of `name`.
pub fn manifest_key(name: &str) -> String {
    format!("service/{name}")
}

/// `_mesh` grow-only set of `name`'s data generations (`u64`, 8 bytes big-endian).
pub fn gen_key(name: &str) -> String {
    format!("service-gen/{name}")
}

/// The namespace holding `name`'s data in generation `gen`.
pub fn data_ns(name: &str, gen_no: u64) -> String {
    if gen_no == 0 {
        name.to_owned()
    } else {
        format!("{name}.{gen_no}")
    }
}

/// The current data generation of `name`: the largest set member, 0 if there is none.
pub async fn current_generation(store: &StoreHandle, name: &str) -> Result<u64, NodeError> {
    let key = gen_key(name);
    let mut after: Option<Vec<u8>> = None;
    let mut current = 0;
    loop {
        let page = store.set_members(MESH_NS, &key, after.take(), PAGE).await?;
        for m in &page {
            if let Ok(b) = <[u8; 8]>::try_from(m.as_slice()) {
                current = current.max(u64::from_be_bytes(b));
            }
        }
        if page.len() < PAGE as usize {
            return Ok(current);
        }
        after = page.last().cloned();
    }
}

/// The live manifest of `name`, `None` if undeployed (or never deployed).
pub(crate) async fn read_manifest(
    store: &StoreHandle,
    name: &str,
) -> Result<Option<ServiceManifest>, ServicesError> {
    let Some(raw) = store.get(MESH_NS, &manifest_key(name)).await? else {
        return Ok(None);
    };
    decode_raw(&raw)
        .map(Some)
        .map_err(|e| ServicesError::InvalidManifest(e.to_string()))
}
