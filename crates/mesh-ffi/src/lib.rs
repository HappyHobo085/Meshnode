//! `mesh-ffi`: the UniFFI surface the Android app (Kotlin foreground service, Tauri
//! commands) uses to run a Meshnode host in-process (spec §3). One process-global host
//! runs one [`mesh_host::Daemon`] on its own tokio runtime; every export works on it.
//!
//! On Android, `MeshNative.initAndroid(context)` (a raw JNI export, see `android.rs`) must
//! run before [`start`].

#[cfg(target_os = "android")]
mod android;
mod host;

pub use host::*;

uniffi::setup_scaffolding!();
