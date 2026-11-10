//! Host-side tests of the UniFFI surface (no JNI). The `Host` is process-global, so the
//! tests run one at a time (`SERIAL`) and each leaves it stopped.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mesh_admin::{AdminRequest, AdminResponse, DaemonState};
use mesh_ffi::{FfiError, FfiPowerPolicy, FfiState, FfiWakeLockSink, FfiWakeReason};
use tempfile::TempDir;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Takes the test lock and makes sure no earlier (failed) test left the host running.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().await;
    mesh_ffi::stop().await;
    guard
}

fn dir_str(dir: &TempDir) -> String {
    dir.path().to_str().unwrap().to_owned()
}

#[tokio::test]
async fn start_is_idempotent() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    let before = mesh_ffi::starts_for_test();
    let first = mesh_ffi::start(dir_str(&dir)).await.unwrap();
    assert_eq!(first.state, FfiState::Uninitialised);
    let second = mesh_ffi::start(dir_str(&dir)).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(mesh_ffi::starts_for_test() - before, 1);
    assert_eq!(mesh_ffi::status(), first);
    // Another data directory while running is refused.
    let other = TempDir::new().unwrap();
    let err = mesh_ffi::start(dir_str(&other)).await.unwrap_err();
    assert!(matches!(err, FfiError::AlreadyRunning { .. }), "{err:?}");
    mesh_ffi::stop().await;
    assert_eq!(mesh_ffi::status().state, FfiState::Stopped);
}

#[tokio::test]
async fn concurrent_start_is_single() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    let before = mesh_ffi::starts_for_test();
    let (a, b) = tokio::join!(
        mesh_ffi::start(dir_str(&dir)),
        mesh_ffi::start(dir_str(&dir))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a, b);
    assert_eq!(a.state, FfiState::Uninitialised);
    assert_eq!(mesh_ffi::starts_for_test() - before, 1, "one daemon");
    mesh_ffi::stop().await;
}

#[tokio::test]
async fn stop_then_start() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    mesh_ffi::start(dir_str(&dir)).await.unwrap();
    mesh_ffi::stop().await;
    assert_eq!(mesh_ffi::status().state, FfiState::Stopped);
    // Stopping twice is a no-op.
    mesh_ffi::stop().await;
    // The data lock was released: the same directory starts again.
    let st = mesh_ffi::start(dir_str(&dir)).await.unwrap();
    assert_eq!(st.state, FfiState::Uninitialised);
    assert_eq!(st.last_error, None);
    mesh_ffi::stop().await;
}

#[tokio::test]
async fn stop_during_start_ends_stopped() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    let (started, ()) = tokio::join!(mesh_ffi::start(dir_str(&dir)), mesh_ffi::stop());
    started.unwrap();
    assert_eq!(mesh_ffi::status().state, FfiState::Stopped);
    // And the host still starts afterwards.
    let st = mesh_ffi::start(dir_str(&dir)).await.unwrap();
    assert_eq!(st.state, FfiState::Uninitialised);
    mesh_ffi::stop().await;
}

#[tokio::test]
async fn admin_bytes_round_trip() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    mesh_ffi::start(dir_str(&dir)).await.unwrap();
    let req = postcard::to_allocvec(&AdminRequest::Status).unwrap();
    let resp = mesh_ffi::admin(req).await.unwrap();
    let resp: AdminResponse = postcard::from_bytes(&resp).unwrap();
    match resp {
        AdminResponse::Status(info) => assert_eq!(info.state, DaemonState::Uninitialised),
        other => panic!("{other:?}"),
    }
    // Garbage is a decode error, not a panic.
    let err = mesh_ffi::admin(vec![0xff, 0xff, 0xff]).await.unwrap_err();
    assert!(matches!(err, FfiError::Decode { .. }), "{err:?}");
    mesh_ffi::stop().await;
}

#[tokio::test]
async fn admin_when_stopped_is_not_running() {
    let _g = serial().await;
    let req = postcard::to_allocvec(&AdminRequest::Status).unwrap();
    let err = mesh_ffi::admin(req).await.unwrap_err();
    assert!(matches!(err, FfiError::NotRunning), "{err:?}");
}

#[tokio::test]
async fn panic_in_start_leaves_stopped() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    mesh_ffi::panic_in_next_start_for_test();
    let err = mesh_ffi::start(dir_str(&dir)).await.unwrap_err();
    assert!(matches!(err, FfiError::Panic { .. }), "{err:?}");
    let st = mesh_ffi::status();
    assert_eq!(st.state, FfiState::Stopped);
    assert!(
        st.last_error
            .as_deref()
            .is_some_and(|e| e.contains("test panic")),
        "{st:?}"
    );
    // The daemon started before the panic let go of its data directory.
    let st = mesh_ffi::start(dir_str(&dir)).await.unwrap();
    assert_eq!(st.state, FfiState::Uninitialised);
    assert_eq!(st.last_error, None);
    mesh_ffi::stop().await;
}

#[derive(Default)]
struct Rec(Mutex<Vec<String>>);

impl FfiWakeLockSink for Rec {
    fn acquire(&self, tag: String, max_ms: u64) {
        self.0.lock().unwrap().push(format!("+{tag}:{max_ms}"));
    }
    fn release(&self, tag: String) {
        self.0.lock().unwrap().push(format!("-{tag}"));
    }
}

impl Rec {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

#[tokio::test]
async fn wake_lock_sink_receives_always_on() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    let rec = Arc::new(Rec::default());
    mesh_ffi::set_wake_lock_sink(rec.clone());
    mesh_ffi::set_power(FfiPowerPolicy::Balanced, false, true);
    mesh_ffi::start(dir_str(&dir)).await.unwrap();
    assert!(rec.take().is_empty());
    mesh_ffi::set_power(FfiPowerPolicy::AlwaysOn, false, true);
    assert_eq!(rec.take(), [format!("+always:{}", u64::MAX)]);
    // A wake without a node does nothing (and does not fail).
    mesh_ffi::wake(FfiWakeReason::Alarm).await;
    mesh_ffi::network_changed();
    mesh_ffi::stop().await;
    assert_eq!(rec.take(), ["-always"]);
    // The policy was persisted to meshd.toml.
    let toml = std::fs::read_to_string(dir.path().join("meshd.toml")).unwrap();
    assert!(toml.contains("always_on"), "{toml}");
    mesh_ffi::set_power(FfiPowerPolicy::Balanced, false, true);
}

#[tokio::test]
async fn wake_lock_sink_set_after_start_gets_held_locks() {
    let _g = serial().await;
    let dir = TempDir::new().unwrap();
    let early = Arc::new(Rec::default());
    mesh_ffi::set_wake_lock_sink(early.clone());
    mesh_ffi::set_power(FfiPowerPolicy::AlwaysOn, false, true);
    mesh_ffi::start(dir_str(&dir)).await.unwrap();
    assert_eq!(early.take(), [format!("+always:{}", u64::MAX)]);
    // A sink registered later (the service restarted) takes the locks held and gets the
    // later events; the old one lets go of them and gets nothing more.
    let late = Arc::new(Rec::default());
    mesh_ffi::set_wake_lock_sink(late.clone());
    assert_eq!(early.take(), ["-always"]);
    assert_eq!(late.take(), [format!("+always:{}", u64::MAX)]);
    mesh_ffi::set_power(FfiPowerPolicy::Balanced, false, true);
    assert_eq!(late.take(), ["-always"]);
    assert!(early.take().is_empty());
    mesh_ffi::stop().await;
}

#[tokio::test]
async fn status_when_stopped() {
    let _g = serial().await;
    let st = mesh_ffi::status();
    assert_eq!(st.state, FfiState::Stopped);
    assert_eq!(st.node_id, None);
    assert!(st.gateway_ports.is_empty());
    // Not relevant without a node, but must not hang.
    tokio::time::timeout(Duration::from_secs(5), mesh_ffi::wake(FfiWakeReason::Boot))
        .await
        .unwrap();
}

struct Lines {
    lines: Mutex<Vec<String>>,
    end: Mutex<Option<tokio::sync::oneshot::Sender<Vec<u8>>>>,
}

impl mesh_ffi::FfiLineSink for Lines {
    fn line(&self, line: String) -> bool {
        self.lines.lock().unwrap().push(line);
        true
    }
    fn end(&self, response: Vec<u8>) {
        let tx = self.end.lock().unwrap().take().expect("end called once");
        let _ = tx.send(response);
    }
}

async fn logs(req: &AdminRequest) -> AdminResponse {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sink = Arc::new(Lines {
        lines: Mutex::new(Vec::new()),
        end: Mutex::new(Some(tx)),
    });
    mesh_ffi::admin_logs(postcard::to_allocvec(req).unwrap(), sink);
    let end = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .unwrap()
        .unwrap();
    postcard::from_bytes(&end).unwrap()
}

#[tokio::test]
async fn admin_logs_and_deploy_static() {
    let _g = serial().await;
    let tail = AdminRequest::LogsTail {
        lines: 10,
        follow: false,
    };
    match logs(&tail).await {
        AdminResponse::Error { code, .. } => assert_eq!(code, mesh_admin::ErrorCode::NotRunning),
        other => panic!("{other:?}"),
    }
    let dir = TempDir::new().unwrap();
    mesh_ffi::start(dir_str(&dir)).await.unwrap();
    // No log files yet: the tail still ends.
    let _ = logs(&tail).await;
    // A static deploy needs a running node.
    let opts = postcard::to_allocvec(&mesh_admin::DeployOpts::default()).unwrap();
    let files = vec![mesh_ffi::FfiFile {
        path: "index.html".into(),
        data: b"<p>hi</p>".to_vec(),
    }];
    let resp = mesh_ffi::admin_deploy_static("site".into(), opts, files)
        .await
        .unwrap();
    let resp: AdminResponse = postcard::from_bytes(&resp).unwrap();
    assert!(matches!(resp, AdminResponse::Error { .. }), "{resp:?}");
    mesh_ffi::stop().await;
}
