use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use mesh_host::config::GatewaySection;
use mesh_host::{Daemon, HostConfig, TestOptions};
use mesh_ui::{
    AdminTransport, InProcessTransport, PipeTransport, Platform, ProbeSampleDto, Ui, UiErrorCode,
    parse_probe_line,
};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn test_config() -> HostConfig {
    HostConfig {
        relay: "disabled".into(),
        discovery: false,
        gateway: GatewaySection {
            listen: SocketAddr::from(([127, 0, 0, 1], free_port())),
            ports: BTreeMap::new(),
        },
        ..HostConfig::default()
    }
}

async fn daemon() -> (Daemon, TempDir) {
    let dir = TempDir::new().unwrap();
    let d = Daemon::start_with(dir.path().to_path_buf(), test_config(), TestOptions::fast())
        .await
        .expect("daemon");
    (d, dir)
}

fn ui_with(t: Arc<dyn AdminTransport>) -> Arc<Ui> {
    Arc::new(Ui::new(
        Arc::new(move || t.clone()),
        Platform::DesktopWindows,
    ))
}

fn in_process(d: &Daemon) -> Arc<Ui> {
    ui_with(Arc::new(InProcessTransport(d.in_process_admin())))
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_follow_streams_lines() {
    let (d, dir) = daemon().await;
    let ui = in_process(&d);
    // The test daemon installs no log subscriber, so give it a log file to follow.
    let logs = dir.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("meshd.log.2999-01-01"),
        "first
",
    )
    .unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let u = ui.clone();
    let task = tokio::spawn(async move { u.logs_follow(10, tx).await });
    ui.config_set("log_level".into(), "info".into())
        .await
        .unwrap();
    let line = timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("a line within 10 s");
    assert!(line.is_some());
    drop(rx);
    timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_ends_when_receiver_dropped() {
    let (d, _dir) = daemon().await;
    let ui = in_process(&d);
    let (tx, rx) = mpsc::channel(1);
    let u = ui.clone();
    let task = tokio::spawn(async move { u.logs_follow(0, tx).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(rx);
    let r = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r, Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn fifth_stream_is_refused() {
    let (d, _dir) = daemon().await;
    let ui = in_process(&d);
    let mut live = Vec::new();
    for _ in 0..4 {
        let (tx, rx) = mpsc::channel(1);
        let u = ui.clone();
        let task = tokio::spawn(async move { u.logs_follow(0, tx).await });
        live.push((rx, task));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (tx, _rx) = mpsc::channel(1);
    let err = ui.logs_follow(0, tx).await.unwrap_err();
    assert_eq!(err.code, UiErrorCode::Busy);
    assert_eq!(err.message, "Too many live views open");
    let (rx, task) = live.pop().unwrap();
    drop(rx);
    timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (tx, rx) = mpsc::channel(1);
    let u = ui.clone();
    let task = tokio::spawn(async move { u.logs_follow(0, tx).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!task.is_finished());
    drop(rx);
    timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn parse_probe_line_cases() {
    assert_eq!(
        parse_probe_line("seq=3 ms=41 connect=12"),
        Some(ProbeSampleDto {
            seq: 3,
            ms: Some(41),
            connect_ms: Some(12),
            error: None
        })
    );
    assert_eq!(
        parse_probe_line("seq=4 error=timed out after 30s"),
        Some(ProbeSampleDto {
            seq: 4,
            ms: None,
            connect_ms: None,
            error: Some("timed out after 30s".into())
        })
    );
    assert_eq!(parse_probe_line("garbage"), None);
}

fn pipe_ui(d: &Daemon) -> Arc<Ui> {
    ui_with(Arc::new(PipeTransport::new(d.endpoint().clone())))
}

#[tokio::test(flavor = "multi_thread")]
async fn pipe_transport_status_and_unreachable() {
    let (d, _dir) = daemon().await;
    let ui = pipe_ui(&d);
    ui.status().await.unwrap();
    d.shutdown().await;
    let err = ui.status().await.unwrap_err();
    assert_eq!(err.code, UiErrorCode::DaemonUnreachable);
}

#[tokio::test(flavor = "multi_thread")]
async fn pipe_stream_closes_connection_on_drop() {
    let (d, _dir) = daemon().await;
    let ui = pipe_ui(&d);
    let (tx, rx) = mpsc::channel(1);
    let u = ui.clone();
    let task = tokio::spawn(async move { u.logs_follow(0, tx).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(rx);
    let r = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r, Ok(()));
    ui.status().await.unwrap();
}
