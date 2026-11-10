//! `mesh-ffi`: the UniFFI surface the Android app (Kotlin foreground service, Tauri
//! commands) uses to run a Meshnode host in-process (spec §3). One process-global host
//! runs one [`mesh_host::Daemon`] on its own tokio runtime; every export works on it.
//!
//! On Android, `MeshNative.initAndroid(context)` (a raw JNI export, see `android.rs`) must
//! run before [`start`], and `MeshNative.releaseAndroidContext()` just before
//! `MainActivity`'s `super.onCreate` (tao sets the context there).

#[cfg(target_os = "android")]
mod android;
mod host;

#[cfg(target_os = "android")]
pub use android::{
    Java_net_meshnode_app_MeshNative_initAndroid,
    Java_net_meshnode_app_MeshNative_releaseAndroidContext,
};
pub use host::*;

/// The addresses of the raw JNI exports (`initAndroid`, `releaseAndroidContext`; empty off
/// Android). The app's `cdylib` calls this once (e.g. in its `run`), next to
/// `mesh_ffi::uniffi_reexport_scaffolding!()`, so that the linker keeps those symbols.
pub fn jni_symbols_anchor() -> Vec<usize> {
    #[cfg(target_os = "android")]
    return std::hint::black_box(android::jni_symbols().to_vec());
    #[cfg(not(target_os = "android"))]
    Vec::new()
}

uniffi::setup_scaffolding!();
