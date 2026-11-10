//! The daemon (spec §6.3): a node with its services, the localhost gateway and the admin
//! API, or, without a mesh yet, the admin API alone. It answers every admin request.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use mesh_admin::{
    Access, AdminBackend, AdminError, AdminRequest, AdminResponse, AdminServer, DaemonState,
    Endpoint, ErrorCode, LocalStateView, MemberInfoView, RequestIo, ServiceInfoView,
    ServiceKindView, StatusInfo, UploadError, connect_raw, endpoint_for,
};
use mesh_core::local::read_genesis;
use mesh_core::{MemberStatus, Node, NodeConfig, NodeError, NodeHandle, NodeStatus, Timings};
use mesh_gateway::{Forwarder, Gateway, GatewayConfig, GatewayError, HttpFwdHandler};
use mesh_proto::{ALPN_HTTPFWD, InviteTicket, NodeId, RefuseReason, ServiceKind, ServiceManifest};
use mesh_services::{LocalStatus, ServiceStatus, Services, ServicesConfig, ServicesError};
use tokio::sync::{RwLock, RwLockReadGuard};
use tokio::time::{Instant, sleep, timeout};
use tracing::{error, info, warn};

use crate::config::{CONFIG_FILE, ConfigError, MeshdConfig};
use crate::paths::{logs_dir, tmp_dir};

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
        *lock(&self.gateway_addrs) = gateway
            .as_ref()
            .map(Gateway::local_addrs)
            .unwrap_or_default();
        *lock(&self.handle) = Some(node.handle());
        *self.run.write().await = Some(Running {
            node,
            services,
            gateway,
        });
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

    /// Stops the gateway, the services and the node, if running, by `deadline`.
    async fn stop_node(&self, deadline: Instant) {
        let Some(running) = self.run.write().await.take() else {
            return;
        };
        lock(&self.handle).take();
        lock(&self.gateway_addrs).clear();
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
        match release(services, Instant::now() + SERVICES_RELEASE).await {
            Some(s) => {
                if timeout(SERVICES_STOP, s.shutdown()).await.is_err() {
                    warn!("the services did not stop in time");
                }
            }
            None => error!("the services are still in use; stopping the node without them"),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if timeout(left, node.shutdown()).await.is_err() {
            error!("the node did not stop in time");
        }
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

#[async_trait]
impl AdminBackend for Daemon {
    fn state(&self) -> DaemonState {
        Backend::state_of(&self.inner)
    }

    async fn handle(&self, req: AdminRequest, io: &mut RequestIo<'_>) -> AdminResponse {
        use AdminRequest as R;
        let inner = &*self.inner;
        match req {
            R::Status => self.status().await,
            R::Init { name } => self.init(name, None).await,
            R::Join { ticket, name } => match InviteTicket::from_text(&ticket) {
                Ok(t) => self.init(name, Some(t)).await,
                Err(e) => AdminResponse::error(ErrorCode::InvalidArgument, e.to_string()),
            },
            R::ResetIdentity { .. } => self.reset_identity().await,
            R::ConfigGet { key } => {
                let cfg = inner.cfg.lock().await;
                match key {
                    None => AdminResponse::Config(cfg.entries()),
                    Some(k) => match cfg.get(&k) {
                        Some(v) => AdminResponse::Config(vec![(k, v)]),
                        None => AdminResponse::error(
                            ErrorCode::InvalidArgument,
                            ConfigError::UnknownKey(k).to_string(),
                        ),
                    },
                }
            }
            R::ConfigSet { key, value } => self.config_set(&key, &value).await,
            R::LogsTail { lines, follow } => {
                crate::logging::tail(&logs_dir(&inner.data_dir), lines, follow, io).await
            }
            other => {
                let run = match inner.running_or(inner.run.read().await) {
                    Ok(r) => r,
                    Err(e) => return e.into(),
                };
                self.node_request(&run, other, io)
                    .await
                    .unwrap_or_else(Into::into)
            }
        }
    }
}

/// An error answer: its code and message.
struct Fail(ErrorCode, String);

impl From<Fail> for AdminResponse {
    fn from(Fail(code, message): Fail) -> Self {
        AdminResponse::Error { code, message }
    }
}

type Answer = Result<AdminResponse, Fail>;

impl Daemon {
    async fn status(&self) -> AdminResponse {
        let inner = &*self.inner;
        let mut s = StatusInfo {
            state: self.state(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            data_dir: inner.data_dir.display().to_string(),
            node_id: None,
            mesh_id: None,
            name: None,
            role: None,
            tags: Vec::new(),
            members_total: 0,
            members_online: 0,
            services: 0,
            gateway: None,
            ports: Vec::new(),
        };
        if let Some(run) = inner.run.read().await.as_ref() {
            let me = run.node.node_id();
            let members = run.node.members();
            s.node_id = Some(me);
            s.mesh_id = Some(run.node.mesh());
            let own = members.iter().find(|m| m.id == me);
            s.role = own.map(|m| m.role);
            // Until the membership shows this node's info, `meshd.toml` has it.
            match own.and_then(|m| m.info.as_ref()) {
                Some(i) => (s.name, s.tags) = (Some(i.name.clone()), i.tags.clone()),
                None => {
                    let cfg = inner.cfg.lock().await;
                    (s.name, s.tags) = (Some(cfg.name.clone()), cfg.tags.clone());
                }
            }
            s.members_total = members.len() as u32;
            s.members_online = members.iter().filter(|m| m.online).count() as u32;
            s.services = run.services.services().len() as u32;
            for (addr, svc) in lock(&inner.gateway_addrs).iter() {
                match svc {
                    None => s.gateway = Some(addr.to_string()),
                    Some(svc) => s.ports.push((addr.port(), svc.clone())),
                }
            }
        }
        AdminResponse::Status(s)
    }

    /// `Init` (no ticket) or `Join`: starts the node in place and records the name.
    async fn init(&self, name: String, ticket: Option<InviteTicket>) -> AdminResponse {
        let inner = &*self.inner;
        let _t = inner.transition.lock().await;
        if inner.run.read().await.is_some() {
            return AdminResponse::error(
                ErrorCode::WrongState,
                "this node already belongs to a mesh",
            );
        }
        let mut cfg = inner.cfg.lock().await.clone();
        if let Err(e) = cfg.set("name", &name) {
            return AdminResponse::error(ErrorCode::InvalidArgument, e.to_string());
        }
        let how = ticket.map_or(How::Create, How::Join);
        let gateway = match inner.start_node(&cfg, how).await {
            Ok(g) => g,
            Err(DaemonError::Node(e)) => return node_error(e).into(),
            Err(e) => return AdminResponse::error(ErrorCode::Internal, e.to_string()),
        };
        if let Err(Fail(code, e)) = self.remember_info(Some(name), None).await {
            return AdminResponse::error(code, format!("the node runs, but {e}"));
        }
        match gateway {
            None => AdminResponse::Ok,
            Some(e) => AdminResponse::error(
                ErrorCode::Internal,
                format!("the node runs, but the gateway did not start: {e}"),
            ),
        }
    }

    /// Stops the node and deletes everything in the data directory but `meshd.toml` and
    /// the logs (the server allows this only when removed or uninitialised).
    async fn reset_identity(&self) -> AdminResponse {
        let inner = &*self.inner;
        let _t = inner.transition.lock().await;
        if !matches!(
            self.state(),
            DaemonState::Removed | DaemonState::Uninitialised
        ) {
            return AdminResponse::error(ErrorCode::WrongState, "this node was not removed");
        }
        inner.stop_node(Instant::now() + SHUTDOWN_LIMIT).await;
        let dir = inner.data_dir.clone();
        match tokio::task::spawn_blocking(move || wipe(&dir)).await {
            Ok(Ok(())) => {
                info!("identity reset");
                AdminResponse::Ok
            }
            Ok(Err(e)) => AdminResponse::error(ErrorCode::Internal, format!("reset failed: {e}")),
            Err(e) => AdminResponse::error(ErrorCode::Internal, format!("reset failed: {e}")),
        }
    }

    async fn config_set(&self, key: &str, value: &str) -> AdminResponse {
        let inner = &*self.inner;
        // Lock order everywhere: `run` before `cfg`.
        let run = inner.run.read().await;
        let mut cfg = inner.cfg.lock().await;
        let mut new = cfg.clone();
        let restart_required = match new.set(key, value) {
            Ok(r) => r,
            Err(e) => return AdminResponse::error(ErrorCode::InvalidArgument, e.to_string()),
        };
        if !restart_required && let Some(run) = run.as_ref() {
            let name = (key == "name").then(|| new.name.clone());
            let tags = (key == "tags").then(|| new.tags.clone());
            if let Err(e) = run.node.set_info(name, tags).await {
                return node_error(e).into();
            }
        }
        if let Err(e) = new.save(&inner.data_dir) {
            return AdminResponse::error(ErrorCode::Internal, e.to_string());
        }
        *cfg = new;
        AdminResponse::ConfigSet { restart_required }
    }

    /// A request that needs the running node.
    async fn node_request(
        &self,
        run: &Running,
        req: AdminRequest,
        io: &mut RequestIo<'_>,
    ) -> Answer {
        use AdminRequest as R;
        let (node, svcs) = (&run.node, &run.services);
        Ok(match req {
            R::Members => {
                let me = node.node_id();
                let mut views: Vec<_> = node
                    .members()
                    .into_iter()
                    .map(|m| member_view(m, me))
                    .collect();
                // Until the membership shows this node's info, `meshd.toml` has it.
                if let Some(v) = views
                    .iter_mut()
                    .find(|v| v.is_self && v.platform.is_empty())
                {
                    let cfg = self.inner.cfg.lock().await;
                    (v.name, v.tags) = (cfg.name.clone(), cfg.tags.clone());
                    v.platform = std::env::consts::OS.into();
                }
                AdminResponse::Members(views)
            }
            R::Services => {
                AdminResponse::Services(svcs.services().into_iter().map(service_view).collect())
            }
            R::Invite { role, ttl_secs } => {
                if ttl_secs == 0 {
                    return Err(Fail(
                        ErrorCode::InvalidArgument,
                        "the ttl must be positive".into(),
                    ));
                }
                let t = node
                    .invite(role, Duration::from_secs(ttl_secs.into()))
                    .await
                    .map_err(node_error)?;
                AdminResponse::Ticket { text: t.to_text() }
            }
            R::Remove { id } => {
                is_member(node, id)?;
                node.remove(id).await.map_err(node_error)?;
                AdminResponse::Ok
            }
            R::SetInfo { name, tags } => {
                node.set_info(name.clone(), tags.clone())
                    .await
                    .map_err(node_error)?;
                self.remember_info(name, tags).await?;
                AdminResponse::Ok
            }
            R::SetTagsOf { id, tags } => {
                is_member(node, id)?;
                node.set_tags_of(id, tags.clone())
                    .await
                    .map_err(node_error)?;
                if id == node.node_id() {
                    self.remember_info(None, Some(tags)).await?;
                }
                AdminResponse::Ok
            }
            R::DeployWasm { name, wasm, opts } => manifest(
                svcs.deploy_wasm(&name, Bytes::from(wasm), services_opts(opts))
                    .await,
            )?,
            R::DeployStatic { name, opts, .. } => self.deploy_static(svcs, &name, opts, io).await?,
            R::Undeploy { name } => {
                svcs.undeploy(&name).await.map_err(services_error)?;
                AdminResponse::Ok
            }
            R::Purge { name } => {
                svcs.purge(&name).await.map_err(services_error)?;
                AdminResponse::Ok
            }
            R::SetPlacement { name, placement } => {
                manifest(svcs.set_placement(&name, placement).await)?
            }
            R::SetOutbound { name, outbound } => {
                manifest(svcs.set_outbound(&name, outbound).await)?
            }
            R::SetLimits { name, limits } => manifest(svcs.set_limits(&name, limits).await)?,
            R::Status
            | R::Init { .. }
            | R::Join { .. }
            | R::ResetIdentity { .. }
            | R::ConfigGet { .. }
            | R::ConfigSet { .. }
            | R::LogsTail { .. } => unreachable!("answered without the node"),
        })
    }

    /// Writes this node's own name and tags to `meshd.toml` after the node took them.
    async fn remember_info(
        &self,
        name: Option<String>,
        tags: Option<Vec<String>>,
    ) -> Result<(), Fail> {
        let mut cfg = self.inner.cfg.lock().await;
        let mut new = cfg.clone();
        if let Some(n) = name {
            new.name = n;
        }
        if let Some(t) = tags {
            new.tags = t;
        }
        new.save(&self.inner.data_dir)
            .map_err(|e| Fail(ErrorCode::Internal, e.to_string()))?;
        *cfg = new;
        Ok(())
    }

    /// Receives the uploaded files into a fresh directory under `<data>/tmp`, deploys
    /// them, and deletes the directory.
    async fn deploy_static(
        &self,
        svcs: &Services,
        name: &str,
        opts: mesh_admin::DeployOpts,
        io: &mut RequestIo<'_>,
    ) -> Answer {
        let dir =
            tmp_dir(&self.inner.data_dir).join(format!("upload-{:016x}", rand::random::<u64>()));
        let result = async {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| Fail(ErrorCode::Internal, e.to_string()))?;
            io.save_files(&dir).await.map_err(|e| match e {
                UploadError::Invalid(m) => Fail(ErrorCode::InvalidArgument, m),
                e => Fail(ErrorCode::Internal, e.to_string()),
            })?;
            manifest(svcs.deploy_static(name, &dir, services_opts(opts)).await)
        }
        .await;
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("cannot remove {}: {e}", dir.display());
        }
        result
    }
}

/// Deletes every entry of `dir` except [`KEPT_ON_RESET`]. Retries briefly: on Windows a
/// file closed a moment ago may still be in use.
fn wipe(dir: &Path) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        let r = (|| {
            for e in std::fs::read_dir(dir)? {
                let e = e?;
                if KEPT_ON_RESET.iter().any(|k| e.file_name() == **k) {
                    continue;
                }
                if e.file_type()?.is_dir() {
                    std::fs::remove_dir_all(e.path())?;
                } else {
                    std::fs::remove_file(e.path())?;
                }
            }
            Ok(())
        })();
        match r {
            Err(_) if attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

fn is_member(node: &Node, id: NodeId) -> Result<(), Fail> {
    if node.members().iter().any(|m| m.id == id) {
        Ok(())
    } else {
        let message = format!("{} is not a member", hex::encode(id.0));
        Err(Fail(ErrorCode::NotFound, message))
    }
}

fn services_opts(o: mesh_admin::DeployOpts) -> mesh_services::DeployOpts {
    mesh_services::DeployOpts {
        limits: o.limits,
        placement: o.placement,
        outbound: o.outbound,
    }
}

fn manifest(r: Result<ServiceManifest, ServicesError>) -> Answer {
    r.map(AdminResponse::Manifest).map_err(services_error)
}

fn member_view(m: MemberStatus, me: NodeId) -> MemberInfoView {
    let (name, platform, tags) = m
        .info
        .map(|i| (i.name, i.platform, i.tags))
        .unwrap_or_default();
    MemberInfoView {
        id: m.id,
        name,
        role: m.role,
        platform,
        tags,
        is_self: m.id == me,
        effective: m.effective,
        online: m.online,
        rtt_ms: m.rtt_ms,
        last_seen_ms: m.last_seen_ms,
        lag: m.lag,
    }
}

fn service_view(s: ServiceStatus) -> ServiceInfoView {
    let m = s.manifest;
    ServiceInfoView {
        kind: match m.kind {
            ServiceKind::Wasm { .. } => ServiceKindView::Wasm,
            ServiceKind::Static { .. } => ServiceKindView::Static,
        },
        name: m.name,
        version: m.version,
        placement: m.placement,
        placed_hosts: s.placed_hosts,
        local: match s.local {
            LocalStatus::NotPlaced => LocalStateView::NotPlaced,
            LocalStatus::Fetching => LocalStateView::Fetching,
            LocalStatus::Ready { version } => LocalStateView::Ready { version },
            LocalStatus::Failed { error, .. } => LocalStateView::Failed { error },
        },
        limits: m.limits,
        outbound: m.outbound,
        data_bytes: s.data_bytes,
    }
}

fn node_error(e: NodeError) -> Fail {
    let code = match &e {
        NodeError::JoinRefused(r) => ErrorCode::JoinRefused(*r),
        NodeError::InviterUnreachable => ErrorCode::JoinRefused(RefuseReason::InviterUnavailable),
        NodeError::LastHost => ErrorCode::LastHost,
        NodeError::NotEffective
        | NodeError::Removed
        | NodeError::Rebootstrapping
        | NodeError::AlreadyInMesh
        | NodeError::NotInitialised
        | NodeError::WrongMesh => ErrorCode::WrongState,
        NodeError::Proto(_) => ErrorCode::InvalidArgument,
        NodeError::ShuttingDown => ErrorCode::NotRunning,
        _ => ErrorCode::Internal,
    };
    Fail(code, e.to_string())
}

fn services_error(e: ServicesError) -> Fail {
    match e {
        ServicesError::Node(e) => node_error(e),
        ServicesError::InvalidName
        | ServicesError::InvalidManifest(_)
        | ServicesError::InvalidComponent(_) => Fail(ErrorCode::InvalidArgument, e.to_string()),
        ServicesError::NotFound => Fail(ErrorCode::NotFound, e.to_string()),
        e => Fail(ErrorCode::Internal, e.to_string()),
    }
}
