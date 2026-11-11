//! The Meshnode Android app's one native library, `libmeshnode_app.so`: Tauri's runtime
//! with the placeholder page's commands, and mesh-ffi's UniFFI scaffolding (used by the
//! Kotlin foreground service). The webview and the service share this library, so one node.

use mesh_admin::{AdminRequest, AdminResponse};
use mesh_ffi::FfiStatus;

// mesh-ffi's UniFFI exports, re-exported from this cdylib (Kotlin loads `meshnode_app`).
mesh_ffi::uniffi_reexport_scaffolding!();

/// The host's status (stopped while the service has not started it).
#[tauri::command]
fn status() -> FfiStatus {
    mesh_ffi::status()
}

/// Joins a mesh with an invite (`AdminRequest::Join`, through `mesh_ffi::admin`). The page
/// trims both values.
#[tauri::command]
async fn join(invite: String, name: String) -> Result<(), String> {
    let request = postcard::to_allocvec(&AdminRequest::Join {
        ticket: invite,
        name,
    })
    .map_err(|e| e.to_string())?;
    let response = mesh_ffi::admin(request).await.map_err(|e| e.to_string())?;
    match postcard::from_bytes(&response).map_err(|e| e.to_string())? {
        AdminResponse::Ok => Ok(()),
        AdminResponse::Error { code, message } => Err(format!("{code}: {message}")),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Starts the foreground service (which starts the node).
#[tauri::command]
async fn service_start() -> Result<(), String> {
    control()?.start_service();
    Ok(())
}

/// Stops the foreground service (and the node).
#[tauri::command]
async fn service_stop() -> Result<(), String> {
    control()?.stop_service();
    Ok(())
}

fn control() -> Result<std::sync::Arc<dyn mesh_ffi::FfiServiceControl>, String> {
    mesh_ffi::service_control().ok_or_else(|| "the service control is not registered".to_owned())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Keeps mesh-ffi's raw JNI exports (`MeshNative.initAndroid`) in the library.
    std::hint::black_box(mesh_ffi::jni_symbols_anchor());
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            status,
            join,
            service_start,
            service_stop
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Meshnode app");
}
