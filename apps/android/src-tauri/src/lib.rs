//! The Meshnode Android app's one native library, `libmeshnode_app.so`: Tauri's runtime with
//! the shared management UI's commands (`mesh-ui-tauri`, over the in-process node) and the
//! Android ones, and mesh-ffi's UniFFI scaffolding (used by the Kotlin foreground service).
//! The webview and the service share this library, so one node.

#[cfg(target_os = "android")]
mod saf;

use std::sync::Arc;

use mesh_ffi::{FfiError, FfiPowerPolicy};
use mesh_ui::{
    AdminTransport, DeployOptsDto, InProcessTransport, NoNode, OnRuntime, Platform, ServiceDto, Ui,
    UiError, UiErrorCode,
};
use mesh_ui_tauri::StreamRegistry;
use tauri::ipc::Channel;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, State};

// mesh-ffi's UniFFI exports, re-exported from this cdylib (Kotlin loads `meshnode_app`).
mesh_ffi::uniffi_reexport_scaffolding!();

/// The transport of one call: the in-process node while it runs (on the host's runtime,
/// where the daemon's tasks live), else `NoNode` (every request is `NotRunning`).
fn transport() -> Arc<dyn AdminTransport> {
    match mesh_ffi::in_process_admin() {
        Some(admin) => Arc::new(OnRuntime::new(
            mesh_ffi::host_runtime(),
            Arc::new(InProcessTransport(admin)),
        )),
        None => Arc::new(NoNode),
    }
}

fn not_running() -> UiError {
    UiError::new(UiErrorCode::NotRunning, "the node is not running")
}

fn ffi_err(e: FfiError) -> UiError {
    match e {
        FfiError::NotRunning => not_running(),
        other => UiError::internal(other.to_string()),
    }
}

/// The power policy as the page names it (ruling R6).
fn policy_name(p: FfiPowerPolicy) -> &'static str {
    match p {
        FfiPowerPolicy::Balanced => "balanced",
        FfiPowerPolicy::AlwaysOn => "always-on",
        FfiPowerPolicy::Saver => "saver",
    }
}

fn parse_policy(s: &str) -> Result<FfiPowerPolicy, UiError> {
    match s {
        "balanced" => Ok(FfiPowerPolicy::Balanced),
        "always-on" => Ok(FfiPowerPolicy::AlwaysOn),
        "saver" => Ok(FfiPowerPolicy::Saver),
        other => Err(UiError::invalid(format!("unknown power policy {other:?}"))),
    }
}

/// Whether the node runs (admin calls would reach it).
#[tauri::command]
fn node_running() -> bool {
    mesh_ffi::in_process_admin().is_some()
}

/// Starts the foreground service (which starts the node).
#[tauri::command]
async fn service_start() -> Result<(), UiError> {
    control()?.start_service();
    Ok(())
}

/// Stops the foreground service (and the node).
#[tauri::command]
async fn service_stop() -> Result<(), UiError> {
    control()?.stop_service();
    Ok(())
}

fn control() -> Result<Arc<dyn mesh_ffi::FfiServiceControl>, UiError> {
    mesh_ffi::service_control()
        .ok_or_else(|| UiError::internal("the service control is not registered"))
}

/// The running node's power policy: "balanced", "always-on" or "saver".
#[tauri::command]
fn power_policy_get() -> Result<&'static str, UiError> {
    mesh_ffi::status()
        .power_policy
        .map(policy_name)
        .ok_or_else(not_running)
}

/// Changes the power policy (saved to meshd.toml by mesh-ffi). `NotRunning` while stopped.
#[tauri::command]
fn power_policy_set(policy: String) -> Result<(), UiError> {
    mesh_ffi::set_power_policy(parse_policy(&policy)?).map_err(ffi_err)
}

#[cfg(not(target_os = "android"))]
fn android_only() -> UiError {
    UiError::invalid("only available on Android")
}

/// A folder (SAF document tree URI), or `null` when nothing was chosen.
#[tauri::command]
async fn pick_tree(app: AppHandle) -> Result<Option<String>, UiError> {
    #[cfg(target_os = "android")]
    {
        app.state::<saf::FolderPicker>().pick_tree().await
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err(android_only())
    }
}

/// A `.wasm` file copied to the app cache (its path), or `null` (ruling R5).
#[tauri::command]
async fn pick_wasm(app: AppHandle) -> Result<Option<String>, UiError> {
    #[cfg(target_os = "android")]
    {
        app.state::<saf::FolderPicker>()
            .pick_file("application/wasm")
            .await
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err(android_only())
    }
}

/// Deploys the document tree at `tree_uri` as a static site; `progress` gets the cumulative
/// bytes read. The upload rules apply to the listed sizes before any file is read.
#[tauri::command]
async fn deploy_static_tree(
    app: AppHandle,
    ui: State<'_, Ui>,
    tree_uri: String,
    name: String,
    opts: DeployOptsDto,
    progress: Channel<u64>,
) -> Result<ServiceDto, UiError> {
    #[cfg(target_os = "android")]
    {
        let picker = app.state::<saf::FolderPicker>();
        let src = Box::new(picker.source(tree_uri.clone()));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(16);
        let deploy = ui.deploy_static(name, opts, src, tx);
        let forward = async {
            while let Some(done) = rx.recv().await {
                // A page gone mid-upload does not stop the upload.
                let _ = progress.send(done);
            }
        };
        let (result, ()) = tokio::join!(deploy, forward);
        picker.release(&tree_uri).await;
        result
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, ui, tree_uri, name, opts, progress);
        Err(android_only())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Keeps mesh-ffi's raw JNI exports (`MeshNative.initAndroid`) in the library.
    std::hint::black_box(mesh_ffi::jni_symbols_anchor());
    let builder = tauri::Builder::default();
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(saf::init());
    builder
        .setup(|app| {
            app.manage(Ui::new(Arc::new(transport), Platform::Android));
            app.manage(StreamRegistry::new());
            Ok(())
        })
        // A reloaded page has new channels: end the old page's live views.
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started
                && let Some(streams) = webview.try_state::<StreamRegistry>()
            {
                streams.cancel_all();
            }
        })
        .invoke_handler(tauri::generate_handler![
            mesh_ui_tauri::commands::status,
            mesh_ui_tauri::commands::members,
            mesh_ui_tauri::commands::services,
            mesh_ui_tauri::commands::init,
            mesh_ui_tauri::commands::join,
            mesh_ui_tauri::commands::invite,
            mesh_ui_tauri::commands::remove,
            mesh_ui_tauri::commands::set_info,
            mesh_ui_tauri::commands::set_tags_of,
            mesh_ui_tauri::commands::deploy_wasm_file,
            mesh_ui_tauri::commands::undeploy,
            mesh_ui_tauri::commands::purge,
            mesh_ui_tauri::commands::set_placement,
            mesh_ui_tauri::commands::set_outbound,
            mesh_ui_tauri::commands::set_limits,
            mesh_ui_tauri::commands::config_get,
            mesh_ui_tauri::commands::config_set,
            mesh_ui_tauri::commands::reset_identity,
            mesh_ui_tauri::commands::declare_fresh,
            mesh_ui_tauri::commands::platform,
            mesh_ui_tauri::commands::logs_follow,
            mesh_ui_tauri::commands::probe,
            mesh_ui_tauri::commands::cancel_stream,
            node_running,
            service_start,
            service_stop,
            power_policy_get,
            power_policy_set,
            pick_tree,
            pick_wasm,
            deploy_static_tree,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Meshnode app");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_policy_names_round_trip() {
        for p in [
            FfiPowerPolicy::Balanced,
            FfiPowerPolicy::AlwaysOn,
            FfiPowerPolicy::Saver,
        ] {
            assert_eq!(parse_policy(policy_name(p)).unwrap(), p);
        }
        assert_eq!(policy_name(FfiPowerPolicy::AlwaysOn), "always-on");
        assert!(parse_policy("performance").is_err());
    }
}
