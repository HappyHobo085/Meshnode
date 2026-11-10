//! The daemon (spec §6.3): a node with its services, the localhost gateway and the admin
//! API, or, without a mesh yet, the admin API alone. It answers every admin request.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use mesh_admin::{
    Access, AdminBackend, AdminError, AdminRequest, AdminResponse, AdminServer, DaemonState,
    Endpoint, ErrorCode, RequestIo, connect_raw, endpoint_for,
};
use mesh_core::local::read_genesis;
use mesh_core::{Node, NodeConfig, NodeError, NodeHandle, NodeStatus, Timings};
use mesh_gateway::{Forwarder, Gateway, GatewayConfig, GatewayError, HttpFwdHandler};
use mesh_proto::{ALPN_HTTPFWD, InviteTicket};
use mesh_services::{Services, ServicesConfig, ServicesError};
use tokio::sync::{RwLock, RwLockReadGuard};
use tokio::time::{Instant, sleep, timeout};
use tracing::{error, info, warn};

use crate::config::{CONFIG_FILE, ConfigError, MeshdConfig};
use crate::paths::{logs_dir, tmp_dir};

mod requests;

use requests::Fail;

/// The whole shutdown (spec §6.3).
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(20);
const ADMIN_STOP: Duration = Duration::from_secs(3);
const GATEWAY_STOP: Duration = Duration::from_secs(6);
/// How long to wait for the last other holders of the services (a WASM blocking thread, an
/// in-flight `httpfwd` request) to let go.
const SERVICES_RELEASE: Duration = Duration::from_secs(3);
const SERVICES_STOP: Duration = Duration::from_secs(3);
/// Entries of the data directory that `ResetIdentity` keeps.
const KEPT_ON_RESET: [&str; 3] = [CONFIG_FILE, "logs", "admin.sock"];

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("another meshd is already running for {}", .0.display())]
    AlreadyRunning(PathBuf),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("data directory {}: {source}", .path.display())]
    DataDir { path: PathBuf, source: io::Error },
    #[error("cannot start the node: {0}")]
    Node(#[from] NodeError),
    #[error("cannot start the services: {0}")]
    Services(#[from] ServicesError),
    #[error("cannot start the gateway: {0}")]
    Gateway(#[from] GatewayError),
    #[error("cannot serve the admin API: {0}")]
    Admin(#[from] AdminError),
}

/// Settings that only tests change.
#[derive(Clone, Default)]
struct StartOptions {
    timings: Timings,
    bind_addr: Option<SocketAddr>,
    #[cfg(feature = "test-hooks")]
    deployer: Option<mesh_services::DeployerTimings>,
}

/// Test-only start settings for [`Daemon::start_with`].
#[cfg(feature = "test-hooks")]
#[derive(Clone)]
pub struct TestOptions {
    pub timings: Timings,
    pub bind_addr: Option<SocketAddr>,
    pub deployer: mesh_services::DeployerTimings,
}

#[cfg(feature = "test-hooks")]
impl TestOptions {
    /// Fast node timings, the node bound to `127.0.0.1:0`, quick deployer rescans.
    pub fn fast() -> TestOptions {
        TestOptions {
            timings: Timings::fast_for_tests(),
            bind_addr: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            deployer: mesh_services::DeployerTimings {
                rescan_every: Duration::from_secs(2),
                retry_min: Duration::from_millis(200),
                retry_max: Duration::from_secs(1),
            },
        }
    }
}

/// A running daemon. Stop it with [`Daemon::shutdown`] or [`Daemon::run_until`].
pub struct Daemon {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    endpoint: Endpoint,
    opts: StartOptions,
    /// Serializes changes of `meshd.toml`.
    cfg: tokio::sync::Mutex<MeshdConfig>,
    /// Serializes Init, Join, ResetIdentity and shutdown.
    transition: tokio::sync::Mutex<()>,
    run: RwLock<Option<Running>>,
    /// The running node, for the synchronous [`AdminBackend::state`].
    handle: Mutex<Option<NodeHandle>>,
    /// The gateway's listeners while it runs: the main one (`None`) first.
    gateway_addrs: Mutex<Vec<(SocketAddr, Option<String>)>>,
    admin: Mutex<Option<AdminServer>>,
}

/// The node and what runs on it.
struct Running {
    node: Node,
    services: Arc<Services>,
    gateway: Option<Gateway>,
}

/// How the node comes up.
enum How {
    Open,
    Create,
    Join(InviteTicket),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Daemon {
    /// Prepares the data directory (creating it, emptying `tmp/`), then, if it holds a
    /// mesh, starts the node, its services and the gateway; finally serves the admin API.
    /// Without a mesh only the admin API runs (`Uninitialised`) until `Init` or `Join`.
    pub async fn start(data_dir: PathBuf, cfg: MeshdConfig) -> Result<Daemon, DaemonError> {
        Self::start_inner(data_dir, cfg, StartOptions::default()).await
    }

    /// [`Daemon::start`] with test timings and bindings.
    #[cfg(feature = "test-hooks")]
    pub async fn start_with(
        data_dir: PathBuf,
        cfg: MeshdConfig,
        test: TestOptions,
    ) -> Result<Daemon, DaemonError> {
        let opts = StartOptions {
            timings: test.timings,
            bind_addr: test.bind_addr,
            deployer: Some(test.deployer),
        };
        Self::start_inner(data_dir, cfg, opts).await
    }

    async fn start_inner(
        data_dir: PathBuf,
        cfg: MeshdConfig,
        opts: StartOptions,
    ) -> Result<Daemon, DaemonError> {
        cfg.validate()?;
        // The directory exists before its endpoint is named (Windows names the pipe after
        // the resolved path).
        std::fs::create_dir_all(&data_dir).map_err(|e| dir_error(&data_dir, e))?;
        let endpoint = endpoint_for(&data_dir);
        // Before touching any file: another daemon may be using them.
        if served(&endpoint).await {
            return Err(DaemonError::AlreadyRunning(data_dir));
        }
        prepare_dirs(&data_dir).map_err(|e| dir_error(&data_dir, e))?;
        let access = access_for(&cfg);
        let initialised = read_genesis(&data_dir)?.is_some();
        let inner = Arc::new(Inner {
            data_dir,
            endpoint,
            opts,
            cfg: tokio::sync::Mutex::new(cfg.clone()),
            transition: tokio::sync::Mutex::new(()),
            run: RwLock::new(None),
            handle: Mutex::new(None),
            gateway_addrs: Mutex::new(Vec::new()),
            admin: Mutex::new(None),
        });
        if initialised {
            let gateway = inner.start_node(&cfg, How::Open).await?;
            if let Some(e) = gateway {
                inner.stop_node(Instant::now() + SHUTDOWN_LIMIT).await;
                return Err(e.into());
            }
        }
        let backend = Arc::new(Backend(Arc::downgrade(&inner)));
        match AdminServer::bind(&inner.endpoint, access, backend) {
            Ok(server) => *lock(&inner.admin) = Some(server),
            Err(e) => {
                inner.stop_node(Instant::now() + SHUTDOWN_LIMIT).await;
                return Err(match e {
                    AdminError::InUse(_) => DaemonError::AlreadyRunning(inner.data_dir.clone()),
                    e => e.into(),
                });
            }
        }
        info!(
            data = %inner.data_dir.display(),
            endpoint = %inner.endpoint,
            state = %Backend::state_of(&inner),
            "meshd started"
        );
        Ok(Daemon { inner })
    }

    /// The admin endpoint this daemon serves.
    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    /// The gateway's main listener, while it runs.
    pub fn gateway_addr(&self) -> Option<SocketAddr> {
        lock(&self.inner.gateway_addrs).first().map(|(a, _)| *a)
    }

    /// Waits for `shutdown` (Ctrl-C, SIGTERM, a service stop), then shuts down.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) {
        shutdown.await;
        self.shutdown().await;
    }

    /// Stops accepting admin connections, then stops the gateway, the services and the
    /// node, within 20 s.
    pub async fn shutdown(self) {
        let deadline = Instant::now() + SHUTDOWN_LIMIT;
        let inner = self.inner;
        let server = lock(&inner.admin).take();
        if let Some(server) = server
            && timeout(ADMIN_STOP, server.shutdown()).await.is_err()
        {
            warn!("the admin server did not stop in time");
        }
        // Any Init or Join in progress was a request, cancelled with the admin server.
        let _t = inner.transition.lock().await;
        inner.stop_node(deadline).await;
        info!("meshd stopped");
    }
}

/// Deletes everything in the data directory but `meshd.toml` and the logs (what
/// `reset-identity` does; the CLI uses it when no daemon runs).
pub fn wipe_identity(dir: &Path) -> io::Result<()> {
    requests::wipe(dir)
}

fn dir_error(path: &Path, source: io::Error) -> DaemonError {
    DaemonError::DataDir {
        path: path.to_path_buf(),
        source,
    }
}

/// Creates the log and staging directories; empties the staging directory.
fn prepare_dirs(data: &Path) -> io::Result<()> {
    std::fs::create_dir_all(logs_dir(data))?;
    let tmp = tmp_dir(data);
    match std::fs::remove_dir_all(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            warn!("cannot clean {}: {e}", tmp.display());
        }
        _ => {}
    }
    std::fs::create_dir_all(&tmp)
}

/// Whether a daemon already serves `endpoint`.
async fn served(endpoint: &Endpoint) -> bool {
    matches!(
        connect_raw(endpoint).await,
        Ok(_) | Err(AdminError::Untrusted { .. } | AdminError::Busy)
    )
}

/// Who may open the admin endpoint (spec §5.2).
fn access_for(cfg: &MeshdConfig) -> Access {
    #[cfg(windows)]
    {
        match &cfg.admin.windows_user_sid {
            Some(sid) => Access::WindowsUser { sid: sid.clone() },
            None => Access::CurrentUserOnly,
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = cfg;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            Access::UnixGroup {
                group: "admin".into(),
            }
        } else {
            Access::CurrentUserOnly
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = cfg;
        if current_user_name().as_deref() == Some("meshnode") {
            Access::UnixGroup {
                group: "meshnode".into(),
            }
        } else {
            Access::CurrentUserOnly
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn current_user_name() -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call; `buf` outlives the use of `pwd`.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_name is a NUL-terminated string inside `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

impl Inner {
    /// Starts the node, its services and the gateway, and installs them. A gateway that
    /// cannot start is returned as the error while the rest keeps running.
    async fn start_node(
        &self,
        cfg: &MeshdConfig,
        how: How,
    ) -> Result<Option<GatewayError>, DaemonError> {
        let (handler, slot) = HttpFwdHandler::new();
        let mut nc = NodeConfig::new(&self.data_dir, cfg.name.clone());
        nc.tags = cfg.tags.clone();
        nc.relay = cfg.relay_choice();
        nc.discovery = cfg.discovery;
        nc.bind_addr = self.opts.bind_addr;
        nc.timings = self.opts.timings.clone();
        nc.extra_protocols
            .push((ALPN_HTTPFWD.to_vec(), Arc::new(handler)));
        let node = match how {
            How::Open => Node::open(nc).await,
            How::Create => Node::create_mesh(nc).await,
            How::Join(ticket) => Node::join(nc, ticket).await,
        }?;
        let services = match self.start_services(&node, cfg).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                node.shutdown().await;
                return Err(e.into());
            }
        };
        slot.attach(&services);
        let forwarder = Forwarder::new(node.handle(), services.clone());
        let gw_cfg = GatewayConfig {
            listen: cfg.gateway.listen,
            ports: cfg.gateway.ports.clone(),
        };
        let (gateway, failed) =
            match Gateway::start(gw_cfg, forwarder, node.handle(), services.clone()).await {
                Ok(g) => (Some(g), None),
                Err(e) => {
                    error!("{e}");
                    (None, Some(e))
                }
            };
        let addrs = gateway.as_ref().map(Gateway::local_addrs);
        let handle = node.handle();
        // `run` first: `state()` (from `handle`) never says Running while `run` is empty.
        let mut run = self.run.write().await;
        *run = Some(Running {
            node,
            services,
            gateway,
        });
        *lock(&self.gateway_addrs) = addrs.unwrap_or_default();
        *lock(&self.handle) = Some(handle);
        Ok(failed)
    }

    async fn start_services(
        &self,
        node: &Node,
        cfg: &MeshdConfig,
    ) -> Result<Services, ServicesError> {
        let sc = ServicesConfig {
            max_memory_mb: cfg.services.max_memory_mb,
            ..ServicesConfig::default()
        };
        #[cfg(feature = "test-hooks")]
        if let Some(t) = self.opts.deployer {
            return Services::start_with_timings(node, sc, t).await;
        }
        Services::start(node, sc).await
    }

    /// Stops the gateway, the services and the node, if running, by `deadline`. False when
    /// the services were still in use and the node was stopped without them.
    async fn stop_node(&self, deadline: Instant) -> bool {
        let running = {
            let mut run = self.run.write().await;
            lock(&self.handle).take();
            lock(&self.gateway_addrs).clear();
            run.take()
        };
        let Some(running) = running else {
            return true;
        };
        let Running {
            node,
            services,
            gateway,
        } = running;
        if let Some(g) = gateway
            && timeout(GATEWAY_STOP, g.shutdown()).await.is_err()
        {
            warn!("the gateway did not stop in time");
        }
        // The gateway dropped the forwarder; the httpfwd slot holds only a weak reference.
        let released = match release(services, Instant::now() + SERVICES_RELEASE).await {
            Some(s) => {
                if timeout(SERVICES_STOP, s.shutdown()).await.is_err() {
                    warn!("the services did not stop in time");
                }
                true
            }
            None => {
                error!("the services are still in use; stopping the node without them");
                false
            }
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if timeout(left, node.shutdown()).await.is_err() {
            error!("the node did not stop in time");
        }
        released
    }

    fn running_or<'a>(
        &self,
        guard: RwLockReadGuard<'a, Option<Running>>,
    ) -> Result<RwLockReadGuard<'a, Running>, Fail> {
        RwLockReadGuard::try_map(guard, Option::as_ref)
            .map_err(|_| Fail(ErrorCode::NotRunning, "the node is not running".into()))
    }
}

/// Waits until `services` has no other holder, then takes it.
async fn release(mut services: Arc<Services>, until: Instant) -> Option<Services> {
    loop {
        match Arc::try_unwrap(services) {
            Ok(s) => return Some(s),
            Err(s) if Instant::now() < until => {
                services = s;
                sleep(Duration::from_millis(20)).await;
            }
            Err(_) => return None,
        }
    }
}

/// What the admin server holds: a weak reference, so that dropping the [`Daemon`] stops
/// everything.
struct Backend(Weak<Inner>);

impl Backend {
    fn state_of(inner: &Inner) -> DaemonState {
        match lock(&inner.handle).as_ref().map(NodeHandle::status) {
            None => DaemonState::Uninitialised,
            Some(NodeStatus::Running) => DaemonState::Running,
            Some(NodeStatus::Rebootstrapping { .. }) => DaemonState::Rebootstrapping,
            Some(NodeStatus::Removed { .. } | NodeStatus::RefusedAsRemoved { .. }) => {
                DaemonState::Removed
            }
        }
    }
}

#[async_trait]
impl AdminBackend for Backend {
    fn state(&self) -> DaemonState {
        self.0
            .upgrade()
            .map_or(DaemonState::Uninitialised, |i| Backend::state_of(&i))
    }

    async fn handle(&self, req: AdminRequest, io: &mut RequestIo<'_>) -> AdminResponse {
        match self.0.upgrade() {
            Some(inner) => Daemon { inner }.handle(req, io).await,
            None => AdminResponse::error(ErrorCode::NotRunning, "meshd is stopping"),
        }
    }
}
