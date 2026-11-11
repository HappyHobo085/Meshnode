//! The commands. They live in a module, not at the crate root: `#[tauri::command]` both
//! exports and re-exports each public command's macro, which clash at the root.

use std::path::PathBuf;

use mesh_ui::{
    ConfigEntry, ConfigSetDto, DeployOptsDto, DirSource, InviteDto, LimitsDto, MemberDto,
    PlacementDto, Platform, ProbeSampleDto, RoleDto, ServiceDto, StatusDto, StreamEvent, Ui,
    UiError,
};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, Runtime, State};
use tokio::sync::mpsc;

use crate::{StreamRegistry, spawn_stream};

#[tauri::command]
pub async fn status(ui: State<'_, Ui>) -> Result<StatusDto, UiError> {
    ui.status().await
}

#[tauri::command]
pub async fn members(ui: State<'_, Ui>) -> Result<Vec<MemberDto>, UiError> {
    ui.members().await
}

#[tauri::command]
pub async fn services(ui: State<'_, Ui>) -> Result<Vec<ServiceDto>, UiError> {
    ui.services().await
}

#[tauri::command]
pub async fn init(ui: State<'_, Ui>, name: String) -> Result<StatusDto, UiError> {
    ui.init(name).await
}

#[tauri::command]
pub async fn join(ui: State<'_, Ui>, ticket: String, name: String) -> Result<StatusDto, UiError> {
    ui.join(ticket, name).await
}

#[tauri::command]
pub async fn invite(ui: State<'_, Ui>, role: RoleDto, ttl_s: u32) -> Result<InviteDto, UiError> {
    ui.invite(role, ttl_s).await
}

#[tauri::command]
pub async fn remove(ui: State<'_, Ui>, id_hex: String) -> Result<(), UiError> {
    ui.remove(id_hex).await
}

#[tauri::command]
pub async fn set_info(
    ui: State<'_, Ui>,
    name: Option<String>,
    tags: Option<Vec<String>>,
) -> Result<(), UiError> {
    ui.set_info(name, tags).await
}

#[tauri::command]
pub async fn set_tags_of(
    ui: State<'_, Ui>,
    id_hex: String,
    tags: Vec<String>,
) -> Result<(), UiError> {
    ui.set_tags_of(id_hex, tags).await
}

#[tauri::command]
pub async fn deploy_wasm_file(
    ui: State<'_, Ui>,
    path: PathBuf,
    name: String,
    opts: DeployOptsDto,
) -> Result<ServiceDto, UiError> {
    ui.deploy_wasm_file(name, path, opts).await
}

/// Deploys the folder at `path` as a static site; `progress` gets the cumulative bytes
/// read. Desktop only (Android deploys from a document tree).
#[tauri::command]
pub async fn deploy_static_dir(
    ui: State<'_, Ui>,
    path: PathBuf,
    name: String,
    opts: DeployOptsDto,
    progress: Channel<u64>,
) -> Result<ServiceDto, UiError> {
    let (tx, mut rx) = mpsc::channel::<u64>(16);
    let deploy = ui.deploy_static(name, opts, Box::new(DirSource::new(path)), tx);
    let forward = async {
        while let Some(done) = rx.recv().await {
            // A page gone mid-upload does not stop the upload.
            let _ = progress.send(done);
        }
    };
    let (result, ()) = tokio::join!(deploy, forward);
    result
}

#[tauri::command]
pub async fn undeploy(ui: State<'_, Ui>, name: String) -> Result<(), UiError> {
    ui.undeploy(name).await
}

#[tauri::command]
pub async fn purge(ui: State<'_, Ui>, name: String) -> Result<(), UiError> {
    ui.purge(name).await
}

#[tauri::command]
pub async fn set_placement(
    ui: State<'_, Ui>,
    name: String,
    placement: PlacementDto,
) -> Result<(), UiError> {
    ui.set_placement(name, placement).await
}

#[tauri::command]
pub async fn set_outbound(
    ui: State<'_, Ui>,
    name: String,
    outbound: Vec<String>,
) -> Result<(), UiError> {
    ui.set_outbound(name, outbound).await
}

#[tauri::command]
pub async fn set_limits(ui: State<'_, Ui>, name: String, limits: LimitsDto) -> Result<(), UiError> {
    ui.set_limits(name, limits).await
}

#[tauri::command]
pub async fn config_get(ui: State<'_, Ui>) -> Result<Vec<ConfigEntry>, UiError> {
    ui.config_get().await
}

#[tauri::command]
pub async fn config_set(
    ui: State<'_, Ui>,
    key: String,
    value: String,
) -> Result<ConfigSetDto, UiError> {
    ui.config_set(key, value).await
}

#[tauri::command]
pub async fn reset_identity(ui: State<'_, Ui>) -> Result<StatusDto, UiError> {
    ui.reset_identity().await
}

#[tauri::command]
pub fn platform(ui: State<'_, Ui>) -> Platform {
    ui.platform()
}

/// Follows the daemon log on `on_event`; returns the stream id for `cancel_stream`.
#[tauri::command]
pub fn logs_follow<R: Runtime>(
    app: AppHandle<R>,
    lines: u32,
    on_event: Channel<StreamEvent<String>>,
) -> u32 {
    spawn_stream(&app, on_event, move |app, tx| async move {
        app.state::<Ui>().logs_follow(lines, tx).await
    })
}

/// Probes a member on `on_event`; returns the stream id for `cancel_stream`.
#[tauri::command]
pub fn probe<R: Runtime>(
    app: AppHandle<R>,
    node_prefix: String,
    count: u32,
    interval_ms: u32,
    timeout_ms: u32,
    on_event: Channel<StreamEvent<ProbeSampleDto>>,
) -> u32 {
    spawn_stream(&app, on_event, move |app, tx| async move {
        app.state::<Ui>()
            .probe(node_prefix, count, interval_ms, timeout_ms, tx)
            .await
    })
}

/// Ends a live view; an unknown or finished one is fine.
#[tauri::command]
pub fn cancel_stream(streams: State<'_, StreamRegistry>, stream_id: u32) {
    streams.cancel(stream_id);
}
