//! The Storage Access Framework, through the Kotlin `FolderPickerPlugin` (spec §7.2): pick a
//! document tree or a file, and [`SafSource`], the static-deploy [`FileSource`] over a tree.

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mesh_admin::FileMeta;
use mesh_ui::{FileSource, UiError, UiErrorCode, check_listing, read_sized};
use serde::{Deserialize, Serialize};
use tauri::plugin::{PluginHandle, TauriPlugin};
use tauri::{Manager, Wry};

/// One `read` asks the plugin for at most this many bytes.
const CHUNK: usize = 1 << 20;

/// The plugin, as managed state.
pub struct FolderPicker(PluginHandle<Wry>);

/// Registers the Kotlin plugin and manages its [`FolderPicker`].
pub fn init() -> TauriPlugin<Wry> {
    tauri::plugin::Builder::new("folder-picker")
        .setup(|app, api| {
            let handle = api.register_android_plugin("net.meshnode.app", "FolderPickerPlugin")?;
            app.manage(FolderPicker(handle));
            Ok(())
        })
        .build()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TreeArgs<'a> {
    tree_uri: &'a str,
}

#[derive(Serialize)]
struct ReadArgs<'a> {
    uri: &'a str,
    offset: u64,
    len: usize,
}

#[derive(Serialize)]
struct PickFileArgs<'a> {
    mime: &'a str,
}

#[derive(Deserialize)]
struct Picked {
    uri: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
struct Listed {
    files: Vec<Listing>,
}

#[derive(Deserialize)]
struct Listing {
    path: String,
    /// -1 when the provider does not know it.
    size: i64,
    uri: String,
}

#[derive(Deserialize)]
struct Chunk {
    data: String,
}

fn plugin_err(e: tauri::plugin::mobile::PluginInvokeError) -> UiError {
    match e {
        // The plugin rejects with code "invalid" what breaks the upload rules.
        tauri::plugin::mobile::PluginInvokeError::InvokeRejected(r) => {
            let code = match r.code.as_deref() {
                Some("invalid") => UiErrorCode::InvalidArgument,
                _ => UiErrorCode::Io,
            };
            let message = r
                .message
                .unwrap_or_else(|| "the folder picker failed".into());
            UiError::new(code, message)
        }
        other => UiError::new(UiErrorCode::Io, other.to_string()),
    }
}

impl FolderPicker {
    /// A document tree (its URI), or `None` when nothing was chosen.
    pub async fn pick_tree(&self) -> Result<Option<String>, UiError> {
        let picked: Picked = self
            .0
            .run_mobile_plugin_async("pickTree", ())
            .await
            .map_err(plugin_err)?;
        Ok(picked.uri)
    }

    /// One file of type `mime`, copied to the app cache (its path), or `None`.
    pub async fn pick_file(&self, mime: &str) -> Result<Option<String>, UiError> {
        let picked: Picked = self
            .0
            .run_mobile_plugin_async("pickFile", PickFileArgs { mime })
            .await
            .map_err(plugin_err)?;
        Ok(picked.path)
    }

    /// Gives back the tree's persisted read grant (best effort).
    pub async fn release(&self, tree_uri: &str) {
        let _ = self
            .0
            .run_mobile_plugin_async::<serde::de::IgnoredAny>("release", TreeArgs { tree_uri })
            .await;
    }

    /// The files of the tree at `tree_uri`.
    pub fn source(&self, tree_uri: String) -> SafSource {
        SafSource {
            picker: self.0.clone(),
            tree_uri,
            files: HashMap::new(),
        }
    }
}

/// The files of a picked document tree. Its methods block (they wait for the plugin), as
/// `FileSource` callers expect: they run on a blocking thread.
pub struct SafSource {
    picker: PluginHandle<Wry>,
    tree_uri: String,
    /// Listed path -> (document URI, listed size).
    files: HashMap<String, (String, u64)>,
}

impl FileSource for SafSource {
    /// Lists the tree and applies the upload rules to the sizes the provider reports, before
    /// any file is read.
    fn list(&mut self) -> Result<Vec<FileMeta>, UiError> {
        let listed: Listed = self
            .picker
            .run_mobile_plugin(
                "list",
                TreeArgs {
                    tree_uri: &self.tree_uri,
                },
            )
            .map_err(plugin_err)?;
        let mut metas = Vec::with_capacity(listed.files.len());
        let mut files = HashMap::with_capacity(listed.files.len());
        for f in listed.files {
            let size = u64::try_from(f.size)
                .map_err(|_| UiError::invalid(format!("The size of {} is unknown", f.path)))?;
            metas.push(FileMeta {
                path: f.path.clone(),
                size,
            });
            files.insert(f.path, (f.uri, size));
        }
        let metas = check_listing(metas)?;
        self.files = files;
        Ok(metas)
    }

    /// Reads exactly the listed size, in chunks, into one buffer of that size.
    fn read(&mut self, path: &str) -> Result<Vec<u8>, UiError> {
        let (uri, size) = self
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| UiError::invalid(format!("{path} was not listed")))?;
        let picker = &self.picker;
        read_sized(path, size, CHUNK, |offset, len| {
            let chunk: Chunk = picker
                .run_mobile_plugin(
                    "read",
                    ReadArgs {
                        uri: &uri,
                        offset,
                        len,
                    },
                )
                .map_err(plugin_err)?;
            STANDARD
                .decode(chunk.data)
                .map_err(|e| UiError::internal(format!("{path}: {e}")))
        })
    }
}
