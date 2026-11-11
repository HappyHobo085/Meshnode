use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use mesh_admin::{AdminRequest, AdminResponse, DeployOpts, MAX_UPLOAD_FILE};
use mesh_host::config::GatewaySection;
use mesh_host::{Daemon, HostConfig, TestOptions};
use mesh_ui::{
    AdminTransport, DeployOptsDto, DirSource, FileSource, InProcessTransport, LimitsDto,
    LocalStateDto, PipeTransport, PlacementDto, Platform, ServiceKindDto, Ui, UiError, UiErrorCode,
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
fn dir_source_io_errors_name_the_folder_or_file() {
    // A folder that can't be read (here: gone) is named.
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-such-folder");
    let e = DirSource::new(missing.clone()).list().unwrap_err();
    assert_eq!(e.code, UiErrorCode::Io);
    assert!(
        e.message.contains(&missing.display().to_string()),
        "{}",
        e.message
    );
    // A listed file that can't be read (here: removed after the listing) is named.
    std::fs::create_dir_all(dir.path().join("site/css")).unwrap();
    std::fs::write(dir.path().join("site/css/app.css"), "a{}").unwrap();
    let mut s = DirSource::new(dir.path().join("site"));
    s.list().unwrap();
    std::fs::remove_file(dir.path().join("site/css/app.css")).unwrap();
    let e = s.read("css/app.css").unwrap_err();
    assert_eq!(e.code, UiErrorCode::Io);
    assert!(e.message.contains("app.css"), "{}", e.message);
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

/// Wraps a transport so the daemon's service list lags behind a deploy, as it does while the
/// deployer's scan hasn't run yet: for the first `lag` listings every service shows its
/// previous version (a brand-new one is missing).
struct Lagging {
    inner: Arc<dyn AdminTransport>,
    lag: Mutex<usize>,
}

impl Lagging {
    fn new(inner: Arc<dyn AdminTransport>, lag: usize) -> Arc<Self> {
        Arc::new(Lagging {
            inner,
            lag: Mutex::new(lag),
        })
    }
}

#[async_trait]
impl AdminTransport for Lagging {
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError> {
        let is_list = matches!(req, AdminRequest::Services);
        let resp = self.inner.request(req).await?;
        let lagging = is_list && {
            let mut lag = self.lag.lock().unwrap();
            let on = *lag > 0;
            *lag = lag.saturating_sub(1);
            on
        };
        match resp {
            AdminResponse::Services(list) if lagging => Ok(AdminResponse::Services(
                list.into_iter()
                    .filter_map(|mut s| {
                        s.version -= 1;
                        (s.version > 0).then_some(s)
                    })
                    .collect(),
            )),
            other => Ok(other),
        }
    }

    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError> {
        self.inner.stream(req, tx).await
    }

    async fn deploy_static(
        &self,
        name: String,
        opts: DeployOpts,
        src: Box<dyn FileSource>,
        progress: mpsc::Sender<u64>,
    ) -> Result<AdminResponse, UiError> {
        self.inner.deploy_static(name, opts, src, progress).await
    }
}

async fn deploy_site(ui: &Ui, name: &str) -> Result<mesh_ui::ServiceDto, UiError> {
    let (dir, _) = site();
    let (tx, _rx) = mpsc::channel(256);
    ui.deploy_static(
        name.into(),
        opts(),
        Box::new(DirSource::new(dir.path().to_path_buf())),
        tx,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_waits_for_the_service_to_be_listed() {
    let (d, _dir) = daemon().await;
    let inner: Arc<dyn AdminTransport> = Arc::new(InProcessTransport(d.in_process_admin()));
    let ui = ui_with(Lagging::new(inner, 5));
    ui.init("test".into()).await.unwrap();
    let svc = deploy_site(&ui, "mysite").await.unwrap();
    assert_eq!((svc.name.as_str(), svc.version), ("mysite", 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn redeploy_returns_the_new_version_not_the_stale_one() {
    let (d, _dir) = daemon().await;
    let inner: Arc<dyn AdminTransport> = Arc::new(InProcessTransport(d.in_process_admin()));
    let plain = ui_with(inner.clone());
    plain.init("test".into()).await.unwrap();
    assert_eq!(deploy_site(&plain, "mysite").await.unwrap().version, 1);
    let ui = ui_with(Lagging::new(inner, 5));
    let svc = deploy_site(&ui, "mysite").await.unwrap();
    assert_eq!(svc.version, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_never_listed_answers_from_the_manifest() {
    let (d, _dir) = daemon().await;
    let inner: Arc<dyn AdminTransport> = Arc::new(InProcessTransport(d.in_process_admin()));
    let ui = ui_with(Lagging::new(inner, usize::MAX));
    ui.init("test".into()).await.unwrap();
    let svc = deploy_site(&ui, "mysite").await.unwrap();
    assert_eq!((svc.name.as_str(), svc.version), ("mysite", 1));
    assert_eq!(svc.kind, ServiceKindDto::Static);
    assert_eq!(svc.local, LocalStateDto::Fetching);
    assert_eq!(svc.data_bytes, 0);
    assert!(svc.placed_hosts.is_empty());
}
