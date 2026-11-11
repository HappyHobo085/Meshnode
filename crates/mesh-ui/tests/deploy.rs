use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use mesh_admin::MAX_UPLOAD_FILE;
use mesh_host::config::GatewaySection;
use mesh_host::{Daemon, HostConfig, TestOptions};
use mesh_ui::{
    AdminTransport, DeployOptsDto, DirSource, FileSource, InProcessTransport, LimitsDto,
    PipeTransport, PlacementDto, Platform, ServiceKindDto, Ui, UiErrorCode,
};
use tempfile::TempDir;
use tokio::sync::mpsc;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn daemon() -> (Daemon, TempDir) {
    let dir = TempDir::new().unwrap();
    let cfg = HostConfig {
        relay: "disabled".into(),
        discovery: false,
        gateway: GatewaySection {
            listen: SocketAddr::from(([127, 0, 0, 1], free_port())),
            ports: BTreeMap::new(),
        },
        ..HostConfig::default()
    };
    let d = Daemon::start_with(dir.path().to_path_buf(), cfg, TestOptions::fast())
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

fn opts() -> DeployOptsDto {
    DeployOptsDto {
        limits: LimitsDto {
            memory_mb: 64,
            request_timeout_ms: 5000,
            max_body_bytes: 1 << 20,
        },
        placement: PlacementDto::AllHosts,
        outbound: vec![],
    }
}

fn site() -> (TempDir, u64) {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("index.html"), b"<h1>hi</h1>").unwrap();
    std::fs::create_dir_all(dir.path().join("css")).unwrap();
    std::fs::write(dir.path().join("css/a.css"), vec![b'x'; 5000]).unwrap();
    (dir, 11 + 5000)
}

fn paths(src: &mut DirSource) -> Vec<String> {
    src.list().unwrap().into_iter().map(|m| m.path).collect()
}

#[test]
fn dir_source_nested_and_sorted() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::write(dir.path().join("index.html"), b"i").unwrap();
    std::fs::write(dir.path().join("a/b/c.txt"), b"c").unwrap();
    let mut s = DirSource::new(dir.path().to_path_buf());
    assert_eq!(paths(&mut s), ["a/b/c.txt", "index.html"]);
    assert_eq!(s.read("a/b/c.txt").unwrap(), b"c");
    assert!(s.read("../x").is_err());
}

#[test]
fn dir_source_non_ascii_names() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("café")).unwrap();
    std::fs::write(dir.path().join("café/ü.html"), b"u").unwrap();
    let mut s = DirSource::new(dir.path().to_path_buf());
    assert_eq!(paths(&mut s), ["café/ü.html"]);
    assert_eq!(s.read("café/ü.html").unwrap(), b"u");
}

#[test]
fn dir_source_skips_os_files_and_symlinks() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("index.html"), b"i").unwrap();
    for n in [".DS_Store", "Thumbs.db", "desktop.ini"] {
        std::fs::write(dir.path().join(n), b"x").unwrap();
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("index.html"), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
    }
    let mut s = DirSource::new(dir.path().to_path_buf());
    assert_eq!(paths(&mut s), ["index.html"]);
}

#[test]
fn dir_source_empty_folder_refused() {
    let dir = TempDir::new().unwrap();
    let e = DirSource::new(dir.path().to_path_buf()).list().unwrap_err();
    assert_eq!(e.code, UiErrorCode::InvalidArgument);
    assert!(e.message.contains("The folder is empty"), "{}", e.message);
}

#[test]
fn dir_source_oversize_names_file() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("big")).unwrap();
    let f = std::fs::File::create(dir.path().join("big/huge.bin")).unwrap();
    f.set_len(MAX_UPLOAD_FILE + 1).unwrap();
    let e = DirSource::new(dir.path().to_path_buf()).list().unwrap_err();
    assert_eq!(e.code, UiErrorCode::InvalidArgument);
    assert!(e.message.contains("big/huge.bin"), "{}", e.message);
}

async fn deploy_and_check(ui: Arc<Ui>) {
    let (dir, total) = site();
    let (tx, mut rx) = mpsc::channel(256);
    let svc = ui
        .deploy_static(
            "mysite".into(),
            opts(),
            Box::new(DirSource::new(dir.path().to_path_buf())),
            tx,
        )
        .await
        .unwrap();
    assert_eq!(svc.name, "mysite");
    assert_eq!(svc.kind, ServiceKindDto::Static);
    let listed = ui.services().await.unwrap();
    assert!(
        listed
            .iter()
            .any(|s| s.name == "mysite" && s.kind == ServiceKindDto::Static)
    );
    let mut last = 0;
    while let Ok(v) = rx.try_recv() {
        assert!(v >= last);
        last = v;
    }
    assert_eq!(last, total);
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_static_in_process_serves_site() {
    let (d, _dir) = daemon().await;
    let ui = ui_with(Arc::new(InProcessTransport(d.in_process_admin())));
    ui.init("test".into()).await.unwrap();
    deploy_and_check(ui).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_static_over_pipe() {
    let (d, _dir) = daemon().await;
    let ui = ui_with(Arc::new(PipeTransport::new(d.endpoint().clone())));
    ui.init("test".into()).await.unwrap();
    deploy_and_check(ui).await;
}
