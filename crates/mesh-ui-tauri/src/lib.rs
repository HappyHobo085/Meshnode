//! The `#[tauri::command]` wrappers over [`mesh_ui::Ui`], shared by the desktop and Android
//! apps: one command per `Ui` method, the live views over Tauri channels, and
//! `cancel_stream`. Each app manages a [`Ui`] and a [`StreamRegistry`] as state and lists
//! these commands (by path, e.g. `mesh_ui_tauri::commands::status`) in its
//! `generate_handler!`.

pub mod commands;
mod commands_list;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use mesh_ui::{StreamEvent, UiError};
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, Runtime};
use tokio::sync::{mpsc, oneshot};

pub use commands_list::COMMANDS;

/// The live views (log follow, probe) by stream id, so `cancel_stream` can end one.
#[derive(Clone, Default)]
pub struct StreamRegistry {
    inner: Arc<Registry>,
}

#[derive(Default)]
struct Registry {
    next: AtomicU32,
    live: Mutex<HashMap<u32, oneshot::Sender<()>>>,
}

impl StreamRegistry {
    pub fn new() -> StreamRegistry {
        StreamRegistry::default()
    }

    fn live(&self) -> MutexGuard<'_, HashMap<u32, oneshot::Sender<()>>> {
        self.inner.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new stream id and the receiver its task watches for cancellation.
    pub fn register(&self) -> (u32, oneshot::Receiver<()>) {
        let id = self
            .inner
            .next
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let (tx, rx) = oneshot::channel();
        self.live().insert(id, tx);
        (id, rx)
    }

    /// Ends stream `id`; an unknown or finished one is fine.
    pub fn cancel(&self, id: u32) {
        if let Some(tx) = self.live().remove(&id) {
            let _ = tx.send(());
        }
    }

    /// Ends every stream (the page was reloaded: its channels are gone).
    pub fn cancel_all(&self) {
        for (_, tx) in self.live().drain() {
            let _ = tx.send(());
        }
    }

    /// Forgets a stream that has ended.
    pub fn finished(&self, id: u32) {
        self.live().remove(&id);
    }

    /// How many streams are live.
    pub fn len(&self) -> usize {
        self.live().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Runs a live view on its own task and returns its stream id. `run` gets the sender its
/// items go to; each item is sent on `channel` as `Item`, then `End` or `Error`. The view
/// ends when `run` returns, when `cancel_stream(id)` is called, or when sending on
/// `channel` fails (the page is gone); ending drops the admin connection.
pub fn spawn_stream<R, T, F, Fut>(
    app: &AppHandle<R>,
    channel: Channel<StreamEvent<T>>,
    run: F,
) -> u32
where
    R: Runtime,
    T: Serialize + Clone + Send + Sync + 'static,
    F: FnOnce(AppHandle<R>, mpsc::Sender<T>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), UiError>> + Send + 'static,
{
    let registry = app.state::<StreamRegistry>().inner().clone();
    let (id, cancelled) = registry.register();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<T>(64);
        let work = run(app, tx);
        let forward = async {
            while let Some(value) = rx.recv().await {
                if channel.send(StreamEvent::Item { value }).is_err() {
                    // Dropping the receiver stops `work`.
                    return false;
                }
            }
            true
        };
        let both = async { tokio::join!(work, forward) };
        let end = tokio::select! {
            _ = cancelled => Some(StreamEvent::End),
            (result, delivered) = both => delivered.then(|| match result {
                Ok(()) => StreamEvent::End,
                Err(error) => StreamEvent::Error { error },
            }),
        };
        if let Some(end) = end {
            let _ = channel.send(end);
        }
        registry.finished(id);
    });
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names of the `#[tauri::command]` functions defined in commands.rs.
    fn wrapper_names() -> Vec<String> {
        let src = include_str!("commands.rs");
        let mut names = Vec::new();
        let mut lines = src.lines();
        while let Some(line) = lines.next() {
            if line.trim() != "#[tauri::command]" {
                continue;
            }
            let decl = lines.next().expect("a function after #[tauri::command]");
            let decl = decl.trim();
            let rest = decl
                .strip_prefix("pub async fn ")
                .or_else(|| decl.strip_prefix("pub fn "))
                .unwrap_or_else(|| panic!("not a pub fn: {decl}"));
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            names.push(name);
        }
        names
    }

    #[test]
    fn commands_lists_every_shared_wrapper() {
        let mut wrappers = wrapper_names();
        assert!(wrappers.contains(&"deploy_static_dir".to_string()));
        // Registered by the desktop app only.
        wrappers.retain(|n| n != "deploy_static_dir");
        wrappers.sort();
        let mut listed: Vec<String> = COMMANDS.iter().map(|s| s.to_string()).collect();
        listed.sort();
        assert_eq!(wrappers, listed);
        let mut dedup = listed.clone();
        dedup.dedup();
        assert_eq!(dedup, listed, "no duplicates");
    }

    #[tokio::test]
    async fn registry_cancels_and_forgets() {
        let reg = StreamRegistry::new();
        let (a, ra) = reg.register();
        let (b, rb) = reg.register();
        assert_ne!(a, b);
        assert_eq!(reg.len(), 2);
        reg.cancel(a);
        assert!(ra.await.is_ok(), "a is told to stop");
        reg.cancel(a); // twice is fine
        reg.cancel(999); // unknown is fine
        reg.finished(b);
        assert!(reg.is_empty());
        // Forgotten without a cancel: the receiver sees its sender dropped.
        assert!(rb.await.is_err());
        let (_c, rc) = reg.register();
        reg.cancel_all();
        assert!(rc.await.is_ok());
        assert!(reg.is_empty());
    }

    #[test]
    fn stream_events_on_the_wire() {
        let item = serde_json::to_value(StreamEvent::Item {
            value: "a".to_string(),
        })
        .unwrap();
        assert_eq!(item, serde_json::json!({"kind": "item", "value": "a"}));
        let end = serde_json::to_value(StreamEvent::<String>::End).unwrap();
        assert_eq!(end, serde_json::json!({"kind": "end"}));
        let err = serde_json::to_value(StreamEvent::<String>::Error {
            error: UiError::invalid("x"),
        })
        .unwrap();
        assert_eq!(err["kind"], "error");
        assert_eq!(err["error"]["code"], "invalidArgument");
    }
}
