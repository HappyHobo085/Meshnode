//! `mesh-client-ffi`: the Kotlin bindings of [`mesh_client`] (sub-spec 8a §5), through
//! UniFFI (package `net.meshnode.client`, library `mesh_client_ffi`).
//!
//! Kotlin gets the `MeshClient` object (suspend `join`/`open`/`request`/`invite`/
//! `remove`/`shutdown`, and `status`/`members`/`services`), the `MeshRequest`/
//! `MeshResponse` records, the `MeshClientListener` callback interface and the sealed
//! `MeshClientException`. On Android, `MeshClientAndroid.init(context)` (a raw JNI export,
//! see `android.rs` and `android/` beside the crate) must run before the first `join` or
//! `open`: the networking crates read the Android context.

#[cfg(target_os = "android")]
mod android;
mod client;
mod types;

#[cfg(target_os = "android")]
pub use android::Java_net_meshnode_client_MeshClientAndroid_initAndroid;
pub use client::{MeshClient, MeshClientListener};
pub use types::*;

uniffi::setup_scaffolding!();
