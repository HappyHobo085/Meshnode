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
    let app = app.clone();
    let (id, task) = stream_task(&registry, channel, move |tx| run(app, tx));
    tauri::async_runtime::spawn(task);
    id
}

/// The part of [`spawn_stream`] that needs no app: registers the stream and returns its id
/// and the task to run.
pub fn stream_task<T, F, Fut>(
    registry: &StreamRegistry,
    channel: Channel<StreamEvent<T>>,
    run: F,
) -> (u32, impl Future<Output = ()> + Send + 'static)
where
    T: Serialize + Clone + Send + Sync + 'static,
    F: FnOnce(mpsc::Sender<T>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), UiError>> + Send + 'static,
{
    let registry = registry.clone();
    let (id, cancelled) = registry.register();
    let task = async move {
        let (tx, mut rx) = mpsc::channel::<T>(64);
        let work = run(tx);
        let items = channel.clone();
        // Owns `rx`: when a send fails (the page is gone) this returns and drops the
        // receiver, so `work` sees its sender closed and ends, freeing its stream slot and
        // admin connection.
        let forward = async move {
            while let Some(value) = rx.recv().await {
                if items.send(StreamEvent::Item { value }).is_err() {
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
    };
    (id, task)
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

    /// A daemon whose log follow sends lines until its receiver is dropped.
    struct EndlessLog;

    #[async_trait::async_trait]
    impl mesh_ui::AdminTransport for EndlessLog {
        async fn request(
            &self,
            _req: mesh_admin::AdminRequest,
        ) -> Result<mesh_admin::AdminResponse, UiError> {
            Err(UiError::internal("not used"))
        }

        async fn stream(
            &self,
            _req: mesh_admin::AdminRequest,
            tx: mpsc::Sender<String>,
        ) -> Result<(), UiError> {
            loop {
                if tx.send("line".into()).await.is_err() {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }

        async fn deploy_static(
            &self,
            _name: String,
            _opts: mesh_admin::DeployOpts,
            _src: Box<dyn mesh_ui::FileSource>,
            _progress: mpsc::Sender<u64>,
        ) -> Result<mesh_admin::AdminResponse, UiError> {
            Err(UiError::internal("not used"))
        }
    }

    fn follow(
        ui: &Arc<mesh_ui::Ui>,
        reg: &StreamRegistry,
        channel: Channel<StreamEvent<String>>,
    ) -> tokio::task::JoinHandle<()> {
        let ui = ui.clone();
        let (_id, task) = stream_task(reg, channel, move |tx| async move {
            ui.logs_follow(10, tx).await
        });
        tokio::spawn(task)
    }

    /// A failed send (the page is gone) ends the stream: its slot, its registry entry and
    /// its connection are freed, so more than 4 such views never make later ones `Busy`.
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_send_ends_the_stream() {
        use std::time::Duration;
        let transport: Arc<dyn mesh_ui::AdminTransport> = Arc::new(EndlessLog);
        let ui = Arc::new(mesh_ui::Ui::new(
            Arc::new(move || transport.clone()),
            mesh_ui::Platform::DesktopWindows,
        ));
        let reg = StreamRegistry::new();
        for _ in 0..mesh_ui::streams::MAX_STREAMS {
            let gone = Channel::new(|_| Err(tauri::Error::FailedToReceiveMessage));
            let task = follow(&ui, &reg, gone);
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("the stream ends after a failed send")
                .unwrap();
        }
        assert!(reg.is_empty(), "every ended stream is forgotten");

        // A 5th view gets lines, not Busy.
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<String>();
        let live = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                let _ = seen_tx.send(json);
            }
            Ok(())
        });
        let task = follow(&ui, &reg, live);
        let first = tokio::time::timeout(Duration::from_secs(5), seen_rx.recv())
            .await
            .expect("an event")
            .expect("an event");
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(first["kind"], "item", "{first}");
        reg.cancel_all();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancel ends it")
            .unwrap();
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
