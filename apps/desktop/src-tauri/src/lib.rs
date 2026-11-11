//! Meshnode for desktop: the management app for a `meshd` daemon. The page talks to the
//! daemon through the shared commands of `mesh-ui-tauri` (over a [`RelocatingTransport`]);
//! the commands here find, start or install that daemon, open dialogs and the browser.

pub mod launch;
pub mod locator;
pub mod relocating;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mesh_ui::{
    AdminTransport, ConnectionDto, ConnectionKindDto, Platform, Ui, UiError, UiErrorCode,
};
use mesh_ui_tauri::StreamRegistry;
use serde::{Deserialize, Serialize};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::oneshot;

use crate::locator::{DaemonLocator, default_candidates, service_data_dir};
use crate::relocating::RelocatingTransport;

#[cfg(windows)]
const PLATFORM: Platform = Platform::DesktopWindows;
#[cfg(target_os = "macos")]
const PLATFORM: Platform = Platform::DesktopMacos;
#[cfg(not(any(windows, target_os = "macos")))]
const PLATFORM: Platform = Platform::DesktopLinux;

/// How long a started daemon has to answer.
const RUN_USER_WAIT: Duration = Duration::from_secs(10);
/// How long an installed service has to answer.
const INSTALL_WAIT: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_millis(250);

/// What the app remembers between runs (`settings.json` in its config directory).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    /// The `meshd` chosen with Locate meshd…
    meshd_path: Option<PathBuf>,
}

impl Settings {
    fn load(config_dir: &Path) -> Settings {
        std::fs::read(config_dir.join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self, config_dir: &Path) -> Result<(), UiError> {
        let io = |e: std::io::Error| UiError::new(UiErrorCode::Io, e.to_string());
        std::fs::create_dir_all(config_dir).map_err(io)?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| UiError::internal(e.to_string()))?;
        std::fs::write(config_dir.join("settings.json"), json).map_err(io)
    }
}

/// The desktop app's state, besides the `Ui`.
pub struct Desktop {
    transport: Arc<RelocatingTransport>,
    settings: Mutex<Settings>,
    config_dir: PathBuf,
}

impl Desktop {
    fn meshd(&self) -> Result<PathBuf, UiError> {
        let remembered = lock(&self.settings).meshd_path.clone();
        launch::locate_meshd(remembered).ok_or_else(|| UiError {
            code: UiErrorCode::NotFound,
            message: "meshd was not found next to the app or on PATH".into(),
            hint: Some("Use Locate meshd… to choose it.".into()),
            reason: None,
        })
    }

    /// Reads the installed service's data directory (through `meshd`, read-only) and
    /// makes it the system candidate.
    async fn refresh_candidates(&self) {
        let dir = match self.meshd() {
            Ok(meshd) => tokio::task::spawn_blocking(move || service_data_dir(&meshd))
                .await
                .ok()
                .flatten(),
            Err(_) => None,
        };
        self.transport.set_candidates(default_candidates(dir));
    }

    /// Locates until `kind` answers, for at most `wait`.
    async fn wait_for(
        &self,
        kind: ConnectionKindDto,
        wait: Duration,
    ) -> Result<ConnectionDto, UiError> {
        let deadline = Instant::now() + wait;
        loop {
            self.transport.relocate().await;
            let c = self.transport.connection();
            if c.kind == kind {
                return Ok(c);
            }
            if Instant::now() >= deadline {
                return Err(UiError::new(
                    UiErrorCode::DaemonStarting,
                    "Meshnode has not answered yet",
                ));
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether `open_url` may open `url`: only `http://<name>.localhost:<port>` (the gateway's
/// service addresses) and `http://127.0.0.1:<port>`, with an optional path.
pub fn open_url_allowed(url: &str) -> bool {
    let Some(rest) = url
        .get(..7)
        .filter(|s| s.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
    else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, _path) = rest.split_at(end);
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    let port_ok = !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|p| p != 0);
    if !port_ok {
        return false;
    }
    let host = host.to_ascii_lowercase();
    if host == "127.0.0.1" {
        return true;
    }
    let Some(name) = host.strip_suffix(".localhost") else {
        return false;
    };
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// The daemon in use: locates again (the page polls this while no daemon answers).
#[tauri::command]
async fn connection(desktop: State<'_, Desktop>) -> Result<ConnectionDto, UiError> {
    desktop.transport.relocate().await;
    Ok(desktop.transport.connection())
}

/// The `meshd` the app would run, if one is found.
#[tauri::command]
fn locate_meshd(desktop: State<'_, Desktop>) -> Option<String> {
    desktop.meshd().ok().map(|p| p.display().to_string())
}

/// Remembers the `meshd` chosen with Locate meshd…
#[tauri::command]
async fn set_meshd_path(desktop: State<'_, Desktop>, path: PathBuf) -> Result<(), UiError> {
    if !path.is_file() {
        return Err(UiError::invalid(format!(
            "{} is not a file",
            path.display()
        )));
    }
    let settings = {
        let mut s = lock(&desktop.settings);
        s.meshd_path = Some(path);
        s.clone()
    };
    settings.save(&desktop.config_dir)?;
    desktop.refresh_candidates().await;
    Ok(())
}

/// Run without installing: starts `meshd run` as the user, detached (it outlives the app),
/// and waits up to 10 s for it to answer.
#[tauri::command]
async fn daemon_run_user(desktop: State<'_, Desktop>) -> Result<ConnectionDto, UiError> {
    if desktop.transport.relocate().await.is_some() {
        return Ok(desktop.transport.connection());
    }
    let meshd = desktop.meshd()?;
    let data = launch::user_data_dir();
    let cmd = launch::run_user_command(&meshd, &data);
    launch::spawn_detached(&cmd, &data.join("meshd.log")).map_err(|e| {
        UiError::new(
            UiErrorCode::Io,
            format!("could not start {}: {e}", meshd.display()),
        )
    })?;
    desktop
        .wait_for(ConnectionKindDto::User, RUN_USER_WAIT)
        .await
}

/// Install as a system service: `meshd service install` with administrator rights (UAC,
/// pkexec or osascript), then waits up to 30 s for the service to answer.
#[tauri::command]
async fn daemon_install(desktop: State<'_, Desktop>) -> Result<ConnectionDto, UiError> {
    let meshd = desktop.meshd()?;
    let (program, args) = launch::install_command(&meshd, None);
    tokio::task::spawn_blocking(move || launch::run_elevated(&program, &args))
        .await
        .map_err(|e| UiError::internal(e.to_string()))??;
    desktop.refresh_candidates().await;
    desktop
        .wait_for(ConnectionKindDto::System, INSTALL_WAIT)
        .await
}

fn picker_cancelled() -> UiError {
    UiError::new(UiErrorCode::PickerCancelled, "Nothing was chosen")
}

fn picked(path: Option<tauri_plugin_dialog::FilePath>) -> Result<String, UiError> {
    let path = path.ok_or_else(picker_cancelled)?;
    let path = path
        .into_path()
        .map_err(|e| UiError::internal(e.to_string()))?;
    Ok(path.display().to_string())
}

/// A folder chosen in the system dialog.
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Result<String, UiError> {
    let (tx, rx) = oneshot::channel();
    app.dialog().file().pick_folder(move |p| {
        let _ = tx.send(p);
    });
    picked(rx.await.ok().flatten())
}

/// A file chosen in the system dialog, filtered to the extension `ext` (e.g. `wasm`).
#[tauri::command]
async fn pick_file(app: AppHandle, ext: String) -> Result<String, UiError> {
    let ext = ext.trim_start_matches('.').to_owned();
    let (tx, rx) = oneshot::channel();
    let mut dialog = app.dialog().file();
    if !ext.is_empty() {
        dialog = dialog.add_filter(ext.clone(), &[ext.as_str()]);
    }
    dialog.pick_file(move |p| {
        let _ = tx.send(p);
    });
    picked(rx.await.ok().flatten())
}

/// Opens a gateway address in the default browser (see [`open_url_allowed`]).
#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), UiError> {
    if !open_url_allowed(&url) {
        return Err(UiError::invalid(
            "Only Meshnode's local addresses can be opened",
        ));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| UiError::new(UiErrorCode::Io, e.to_string()))
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let settings = Settings::load(&config_dir);
            let transport = Arc::new(RelocatingTransport::new(DaemonLocator {
                candidates: default_candidates(None),
            }));
            let t = transport.clone();
            app.manage(Ui::new(
                Arc::new(move || t.clone() as Arc<dyn AdminTransport>),
                PLATFORM,
            ));
            app.manage(StreamRegistry::new());
            app.manage(Desktop {
                transport,
                settings: Mutex::new(settings),
                config_dir,
            });
            // Learn the service's data directory, then find the daemon.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let desktop = handle.state::<Desktop>();
                desktop.refresh_candidates().await;
                desktop.transport.relocate().await;
            });
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
            mesh_ui_tauri::commands::platform,
            mesh_ui_tauri::commands::logs_follow,
            mesh_ui_tauri::commands::probe,
            mesh_ui_tauri::commands::cancel_stream,
            mesh_ui_tauri::commands::deploy_static_dir,
            connection,
            locate_meshd,
            set_meshd_path,
            daemon_run_user,
            daemon_install,
            pick_folder,
            pick_file,
            open_url,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Meshnode app");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_url_allowlist() {
        assert!(open_url_allowed("http://site.mesh.localhost:7777/"));
        assert!(open_url_allowed("http://site.mesh.localhost:7777/a/b?c#d"));
        assert!(open_url_allowed("http://127.0.0.1:7777"));
        assert!(open_url_allowed("http://127.0.0.1:7777/x"));
        assert!(open_url_allowed("HTTP://Site.Localhost:80/"));
        for bad in [
            "https://evil.com",
            "http://localhost.evil.com:1",
            "file:///c:/",
            "http://localhost:7777/",
            "http://.localhost:7777/",
            "http://a..b.localhost:7777/",
            "http://a.localhost/",
            "http://a.localhost:/",
            "http://a.localhost:0/",
            "http://a.localhost:99999/",
            "http://a.localhost:12x/",
            "http://a.localhost:1\\@evil.com/",
            "http://user@a.localhost:1/",
            "http://evil.com@127.0.0.1:1/",
            "http://127.0.0.1.evil.com:1/",
            "http://127.0.0.1/",
            "https://127.0.0.1:7777",
            "http://a_b.localhost:1/",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!open_url_allowed(bad), "{bad}");
        }
    }
}
