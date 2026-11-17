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
    Endpoint, ErrorCode, InProcessAdmin, RequestIo, connect_raw, endpoint_for,
};
use mesh_core::local::read_genesis;
use mesh_core::{Node, NodeConfig, NodeError, NodeHandle, NodeStatus, Timings, WakeOutcome};
use mesh_gateway::{Forwarder, Gateway, GatewayConfig, GatewayError, HttpFwdHandler, HttpFwdSlot};
use mesh_proto::{ALPN_HTTPFWD, InviteTicket};
use mesh_services::{LocalStatus, Services, ServicesConfig, ServicesError};
use tokio::sync::{RwLock, RwLockReadGuard};
use tokio::time::{Instant, sleep, timeout};
use tracing::{error, info, warn};

use crate::config::{CONFIG_FILE, ConfigError, HostConfig};
use crate::lock::{DataLock, LOCK_FILE, LockError};
use crate::options::HostOptions;
use crate::paths::{self, logs_dir, tmp_dir};
use crate::power::{CATCH_UP_AFTER, Periodic, Power, PowerState, WakeReason, saver_paused};

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
/// How long [`Daemon::wake`] waits for the catch-up.
const WAKE_LIMIT: Duration = Duration::from_secs(10);
/// How long shutdown waits for a pending save of the power policy.
const POLICY_SAVE: Duration = Duration::from_secs(3);
/// Entries of the data directory that `ResetIdentity` keeps.
const KEPT_ON_RESET: [&str; 4] = [CONFIG_FILE, "logs", "admin.sock", LOCK_FILE];

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

impl DaemonError {
    /// True when the data (or `meshd.toml`) was written by an incompatible Meshnode
    /// version or is invalid for this one: the daemon must refuse, not retry.
    pub fn is_incompatible(&self) -> bool {
        matches!(
            self,
            DaemonError::Node(NodeError::Incompatible { .. })
                | DaemonError::Config(ConfigError::Parse { .. })
        )
    }
}

/// After a failed start: when `err` is an incompatible-data refusal, its message goes to
/// `<data_dir>/start-error.txt` for `meshd status` and the desktop app. Hosts that load
/// `meshd.toml` themselves call this for a load error too.
pub fn record_start_failure(data_dir: &Path, err: &DaemonError) {
    if !err.is_incompatible() {
        return;
    }
    if let Err(e) = paths::write_start_error(data_dir, &err.to_string()) {
        warn!("cannot write {}: {e}", paths::START_ERROR_FILE);
    }
}

/// Settings that only tests change.
#[derive(Clone, Default)]
struct StartOptions {
    timings: Timings,
    bind_addr: Option<SocketAddr>,
    host: HostOptions,
    #[cfg(feature = "test-hooks")]
    deployer: Option<mesh_services::DeployerTimings>,
    #[cfg(feature = "test-hooks")]
    wall: Option<mesh_core::DynClock>,
}

/// Test-only start settings for [`Daemon::start_with`].
#[cfg(feature = "test-hooks")]
#[derive(Clone)]
pub struct TestOptions {
    pub timings: Timings,
    pub bind_addr: Option<SocketAddr>,
    pub deployer: mesh_services::DeployerTimings,
    pub host: HostOptions,
    /// The node's wall clock (`None`: the system clock).
    pub wall: Option<mesh_core::DynClock>,
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
            host: HostOptions::default(),
            wall: None,
        }
    }
}

/// What an embedding host shows (Android notification, placeholder page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostStatus {
    pub state: DaemonState,
    /// Hex, while a node runs.
    pub node_id: Option<String>,
    /// The mesh id in hex (a mesh has no other name), while a node runs.
    pub mesh_name: Option<String>,
    /// The services this node serves itself (ready here).
    pub services_hosted: u32,
    /// Other members online.
    pub peers_online: u32,
    /// The gateway's listening ports: the main one first, then the mapped ones.
    pub gateway_ports: Vec<u16>,
}

/// A running daemon. Stop it with [`Daemon::shutdown`] or [`Daemon::run_until`].
pub struct Daemon {
    inner: Arc<Inner>,
}

struct Inner {
    /// Held for the daemon's lifetime (taken first in `start`).
    _lock: DataLock,
    data_dir: PathBuf,
    endpoint: Endpoint,
    opts: StartOptions,
    /// Serializes changes of `meshd.toml`.
    cfg: tokio::sync::Mutex<HostConfig>,
    /// Serializes Init, Join, ResetIdentity and shutdown.
    transition: tokio::sync::Mutex<()>,
    run: RwLock<Option<Running>>,
    /// The running node, for the synchronous [`AdminBackend::state`].
    handle: Mutex<Option<NodeHandle>>,
    /// The gateway's listeners while it runs: the main one (`None`) first.
    gateway_addrs: Mutex<Vec<(SocketAddr, Option<String>)>>,
    admin: Mutex<Option<AdminServer>>,
    /// The runtime the daemon started on (for work started from synchronous calls).
    rt: tokio::runtime::Handle,
    /// The power state and wake locks; applied to every node started.
    power: Arc<Power>,
    /// `[android] periodic_wake_s`, as started.
    periodic_wake_s: u32,
    /// The latest save of `[android] power` started by `set_power`.
    policy_save: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.power.close();
    }
}

/// The node and what runs on it.
struct Running {
    node: Node,
    services: Arc<Services>,
    gateway: Option<Gateway>,
    slot: HttpFwdSlot,
}

/// How [`Inner::stop_node`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stopped {
    /// Nothing runs any more (or nothing ran).
    Clean,
    /// The node was stopped, but the services were still in use and were not shut down.
    ServicesInUse,
    /// The node could not be taken in time: it may still be running.
    Busy,
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
    pub async fn start(data_dir: PathBuf, cfg: HostConfig) -> Result<Daemon, DaemonError> {
        Self::start_opts(data_dir, cfg, HostOptions::default()).await
    }

    /// [`Daemon::start`] with host-specific options.
    pub async fn start_opts(
        data_dir: PathBuf,
        cfg: HostConfig,
        host: HostOptions,
    ) -> Result<Daemon, DaemonError> {
        let opts = StartOptions {
            host,
            ..StartOptions::default()
        };
        Self::start_inner(data_dir, cfg, opts).await
    }

    /// [`Daemon::start`] with test timings and bindings.
    #[cfg(feature = "test-hooks")]
    pub async fn start_with(
        data_dir: PathBuf,
        cfg: HostConfig,
        test: TestOptions,
    ) -> Result<Daemon, DaemonError> {
        let opts = StartOptions {
            timings: test.timings,
            bind_addr: test.bind_addr,
            host: test.host,
            deployer: Some(test.deployer),
            wall: test.wall,
        };
        Self::start_inner(data_dir, cfg, opts).await
    }

    /// [`Daemon::start_serving`], then `start-error.txt` recorded (an incompatible
    /// refusal) or removed (a successful start).
    async fn start_inner(
        data_dir: PathBuf,
        cfg: HostConfig,
        opts: StartOptions,
    ) -> Result<Daemon, DaemonError> {
        let r = Self::start_serving(data_dir.clone(), cfg, opts).await;
        match &r {
            Ok(_) => {
                if let Err(e) = paths::clear_start_error(&data_dir) {
                    warn!("cannot remove {}: {e}", paths::START_ERROR_FILE);
                }
            }
            Err(e) => record_start_failure(&data_dir, e),
        }
        r
    }

    async fn start_serving(
        data_dir: PathBuf,
        cfg: HostConfig,
        opts: StartOptions,
    ) -> Result<Daemon, DaemonError> {
        cfg.validate()?;
        // The directory exists before its endpoint is named (Windows names the pipe after
        // the resolved path).
        std::fs::create_dir_all(&data_dir).map_err(|e| dir_error(&data_dir, e))?;
        let data_lock = DataLock::acquire(&data_dir).map_err(|e| match e {
            LockError::Held(_) => DaemonError::AlreadyRunning(data_dir.clone()),
            LockError::Io { path, source } => DaemonError::DataDir { path, source },
        })?;
        let endpoint = endpoint_for(&data_dir);
        // Before touching any file: another daemon may be using them.
        if served(&endpoint).await {
            return Err(DaemonError::AlreadyRunning(data_dir));
        }
        prepare_dirs(&data_dir).map_err(|e| dir_error(&data_dir, e))?;
        // Spec §6.3 step 1: `meshd.toml` is created with the defaults when it is missing.
        if !data_dir.join(CONFIG_FILE).exists() {
            cfg.save(&data_dir)?;
        }
        let access = access_for(&cfg);
        let initialised = read_genesis(&data_dir)?.is_some();
        let rt = tokio::runtime::Handle::current();
        // The platform reports the real state through `set_power`.
        let power_state = PowerState {
            policy: cfg.android.power,
            charging: false,
            screen_on: true,
        };
        let power = Arc::new(Power::new(opts.host.power.clone(), rt.clone(), power_state));
        let inner = Arc::new(Inner {
            _lock: data_lock,
            data_dir,
            endpoint,
            opts,
            cfg: tokio::sync::Mutex::new(cfg.clone()),
            transition: tokio::sync::Mutex::new(()),
            run: RwLock::new(None),
            handle: Mutex::new(None),
            gateway_addrs: Mutex::new(Vec::new()),
            admin: Mutex::new(None),
            rt,
            power,
            periodic_wake_s: cfg.android.periodic_wake_s,
            policy_save: Mutex::new(None),
        });
        if initialised {
            let gateway = inner.start_node(&cfg, How::Open).await?;
            if let Some(e) = gateway {
                inner.stop_node(Instant::now() + SHUTDOWN_LIMIT).await;
                return Err(e.into());
            }
        }
        if inner.opts.host.admin_socket {
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
        }
        inner.power.apply_standing();
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
        let left = deadline.saturating_duration_since(Instant::now());
        let _t = match timeout(left, inner.transition.lock()).await {
            Ok(t) => Some(t),
            Err(_) => {
                warn!("an Init or Join did not end in time; stopping anyway");
                None
            }
        };
        inner.stop_node(deadline).await;
        inner.power.close();
        let save = lock(&inner.policy_save).take();
        if let Some(save) = save
            && timeout(POLICY_SAVE, save).await.is_err()
        {
            warn!("the power policy was not saved in time");
        }
        info!("meshd stopped");
    }

    /// Catches up now (Android: alarm, network change, boot, incoming push): redials and
    /// syncs with every connected peer, for at most 10 s (the rest goes on in the
    /// background). Holds the `"catchup"` wake lock meanwhile and, on an alarm, the
    /// `"alarm"` lock for its timeout, as the power policy says. Without a running node it
    /// does nothing.
    pub async fn wake(&self, reason: WakeReason) -> WakeOutcome {
        let inner = &self.inner;
        let run = inner.run.read().await;
        let Some(run) = run.as_ref() else {
            return WakeOutcome {
                peers: 0,
                rounds_completed: 0,
                timed_out: false,
            };
        };
        if reason == WakeReason::Alarm {
            inner.power.alarm(inner.periodic_wake_s);
        }
        let _lock = inner.power.catch_up_lock();
        let outcome = run.node.wake(WAKE_LIMIT).await;
        info!(?reason, ?outcome, "wake");
        outcome
    }

    /// The device's network changed: rebinds and redials now. Without a running node it
    /// does nothing.
    pub fn network_changed(&self) {
        self.inner.with_node(|_, run| run.node.network_change());
    }

    /// Records the power state (policy, charging, screen) and applies it: in Saver with
    /// the screen off, inbound `httpfwd` answers 503 and the incoming-traffic catch-up is
    /// off; the `"always"` / `"charging"` wake lock is taken or let go. The state is kept
    /// across node starts and applies to each. A new policy is saved to `meshd.toml`
    /// (`[android] power`), so that it survives a restart.
    pub fn set_power(&self, state: PowerState) {
        let old = self.inner.power.set_state(state);
        self.inner.with_node(Inner::apply_power);
        if old.policy != state.policy {
            self.inner.save_policy();
        }
    }

    /// The power state in force.
    pub fn power_state(&self) -> PowerState {
        self.inner.power.state()
    }

    /// The admin API, called in-process (no socket).
    pub fn in_process_admin(&self) -> InProcessAdmin {
        InProcessAdmin::new(Arc::new(Backend(Arc::downgrade(&self.inner))))
    }

    /// What `Status` reports, summarised. While a node starts or stops, its details may be
    /// missing.
    pub fn status(&self) -> HostStatus {
        let inner = &*self.inner;
        let run = inner.run.try_read().ok();
        let cfg = inner.cfg.try_lock().ok();
        let running = run.as_deref().and_then(Option::as_ref);
        let s = self.status_info(running, cfg.as_deref());
        let hosted = running.map_or(0, |r| {
            let all = r.services.services();
            all.iter()
                .filter(|s| matches!(s.local, LocalStatus::Ready { .. }))
                .count() as u32
        });
        let main = s
            .gateway
            .as_deref()
            .and_then(|a| a.parse::<SocketAddr>().ok())
            .map(|a| a.port());
        HostStatus {
            state: s.state,
            node_id: s.node_id.map(|id| hex::encode(id.0)),
            mesh_name: s.mesh_id.map(|id| hex::encode(id.0)),
            services_hosted: hosted,
            // This node counts itself online.
            peers_online: s.members_online.saturating_sub(1),
            gateway_ports: main
                .into_iter()
                .chain(s.ports.iter().map(|p| p.0))
                .collect(),
        }
    }

    /// Inbound `httpfwd` streams being served (counted only with a wake-lock sink).
    #[cfg(feature = "test-hooks")]
    pub fn requests_in_flight(&self) -> u32 {
        self.inner.power.in_flight()
    }

    /// The running node's handle.
    #[cfg(feature = "test-hooks")]
    pub fn node_handle_for_test(&self) -> Option<NodeHandle> {
        lock(&self.inner.handle).clone()
    }

    /// iroh network-change calls completed by the running node (0 without one).
    #[cfg(feature = "test-hooks")]
    pub async fn network_changes_for_test(&self) -> u64 {
        let run = self.inner.run.read().await;
        run.as_ref()
            .map_or(0, |r| r.node.network_changes_for_test())
    }

    /// `httpfwd` requests the running node answered (0 without one).
    #[cfg(feature = "test-hooks")]
    pub async fn httpfwd_served_for_test(&self) -> u64 {
        let run = self.inner.run.read().await;
        run.as_ref().map_or(0, |r| r.slot.served())
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
fn access_for(cfg: &HostConfig) -> Access {
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
        cfg: &HostConfig,
        how: How,
    ) -> Result<Option<GatewayError>, DaemonError> {
        let (handler, slot) = HttpFwdHandler::new();
        if self.power.has_sink() {
            let power = self.power.clone();
            slot.set_activity(Arc::new(move |a| power.activity(a)));
        }
        let power_state = self.power.state();
        slot.set_paused(saver_paused(&power_state));
        let mut nc = NodeConfig::new(&self.data_dir, cfg.name.clone());
        nc.tags = cfg.tags.clone();
        nc.relay = cfg.relay_choice();
        nc.discovery = cfg.discovery;
        nc.bind_addr = self.opts.bind_addr;
        nc.timings = self.opts.timings.clone();
        nc.catch_up_on_incoming_after = self.catch_up_after(&power_state);
        if self.power.has_sink() {
            // The incoming-traffic catch-up holds the "catchup" wake lock, like a wake.
            let power = self.power.clone();
            nc.on_catch_up = Some(Arc::new(move || Box::new(power.catch_up_lock())));
        }
        #[cfg(feature = "test-hooks")]
        if let Some(wall) = &self.opts.wall {
            nc.wall = Arc::new(wall.clone());
        }
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
        let running = Running {
            node,
            services,
            gateway,
            slot,
        };
        // Under the write lock: the state may have changed since it was read above, and a
        // `set_power` from now on finds this node (or waits for it).
        self.apply_power(&running);
        *run = Some(running);
        *lock(&self.gateway_addrs) = addrs.unwrap_or_default();
        *lock(&self.handle) = Some(handle);
        Ok(failed)
    }

    async fn start_services(
        &self,
        node: &Node,
        cfg: &HostConfig,
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
    async fn stop_node(&self, deadline: Instant) -> Stopped {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(mut run) = timeout(left, self.run.write()).await else {
            error!("the node is still in use; it cannot be stopped in time");
            return Stopped::Busy;
        };
        lock(&self.handle).take();
        lock(&self.gateway_addrs).clear();
        let running = run.take();
        drop(run);
        let Some(running) = running else {
            return Stopped::Clean;
        };
        let Running {
            node,
            services,
            gateway,
            slot,
        } = running;
        if let Some(g) = gateway
            && timeout(GATEWAY_STOP, g.shutdown()).await.is_err()
        {
            warn!("the gateway did not stop in time");
        }
        // Peers' new httpfwd requests are refused from here on, so only those already in
        // flight can still hold the services.
        slot.detach();
        // The gateway dropped the forwarder; the httpfwd slot holds only a weak reference.
        let released = match release(services, Instant::now() + SERVICES_RELEASE).await {
            Some(s) => {
                if timeout(SERVICES_STOP, s.shutdown()).await.is_err() {
                    warn!("the services did not stop in time");
                }
                Stopped::Clean
            }
            None => {
                error!("the services are still in use; stopping the node without them");
                Stopped::ServicesInUse
            }
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if timeout(left, node.shutdown()).await.is_err() {
            error!("the node did not stop in time");
            return Stopped::Busy;
        }
        released
    }

    /// The incoming-traffic catch-up for `state`: on with `Periodic::Driven`, except while
    /// Saver pauses serving.
    fn catch_up_after(&self, state: &PowerState) -> Option<Duration> {
        (self.opts.host.periodic == Periodic::Driven && !saver_paused(state))
            .then_some(CATCH_UP_AFTER)
    }

    /// Applies the current power state to a running node.
    fn apply_power(&self, run: &Running) {
        let state = self.power.state();
        run.slot.set_paused(saver_paused(&state));
        run.node
            .set_catch_up_on_incoming(self.catch_up_after(&state));
    }

    /// Calls `f` with the running node, if any: at once when the node is free, otherwise
    /// on a task once the start or stop in progress ends.
    /// `f` runs inside the daemon's runtime, even when called from a foreign thread
    /// (the node spawns work from it).
    fn with_node(self: &Arc<Self>, f: impl FnOnce(&Inner, &Running) + Send + 'static) {
        if let Ok(run) = self.run.try_read() {
            if let Some(run) = run.as_ref() {
                let _rt = self.rt.enter();
                f(self, run);
            }
            return;
        }
        let me = Arc::downgrade(self);
        self.rt.spawn(async move {
            if let Some(inner) = me.upgrade()
                && let Some(run) = inner.run.read().await.as_ref()
            {
                f(&inner, run);
            }
        });
    }

    /// Saves the current power policy to `meshd.toml` if it differs, on a task (the
    /// latest policy wins when calls overlap). Shutdown waits for the last one.
    fn save_policy(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let task = self.rt.spawn(async move {
            let Some(inner) = me.upgrade() else { return };
            let mut cfg = inner.cfg.lock().await;
            let policy = inner.power.state().policy;
            if cfg.android.power == policy {
                return;
            }
            let mut new = cfg.clone();
            new.android.power = policy;
            match requests::save_config(&new, &inner.data_dir).await {
                Ok(()) => *cfg = new,
                Err(e) => warn!("cannot save the power policy: {e}"),
            }
        });
        // An earlier save is not aborted (it may be mid-write): this one queues behind it
        // on the cfg lock, so waiting for the last one waits for all.
        *lock(&self.policy_save) = Some(task);
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
            None => AdminResponse::error(ErrorCode::NotRunning, mesh_admin::DAEMON_STOPPING),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `reset-identity` must not wipe the data directory when the node could not be taken
    /// (here: a read guard on `run` stands in for a request still using it).
    #[tokio::test]
    async fn reset_refused_while_the_node_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            relay: "disabled".into(),
            discovery: false,
            ..HostConfig::default()
        };
        let daemon = Daemon::start(dir.path().to_path_buf(), cfg).await.unwrap();
        let inner = daemon.inner.clone();
        let marker = dir.path().join("keep-me");
        std::fs::write(&marker, b"x").unwrap();
        {
            let _busy = inner.run.read().await;
            let soon = Instant::now() + Duration::from_millis(200);
            assert_eq!(inner.stop_node(soon).await, Stopped::Busy);
            let soon = Instant::now() + Duration::from_millis(200);
            match daemon.reset_identity_by(soon).await {
                AdminResponse::Error { message, .. } => {
                    assert!(message.contains("reset refused"), "{message}")
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
            assert!(marker.exists(), "the data directory was wiped");
        }
        // Free again: the reset goes through.
        let soon = Instant::now() + Duration::from_secs(5);
        assert_eq!(daemon.reset_identity_by(soon).await, AdminResponse::Ok);
        assert!(!marker.exists());
        daemon.shutdown().await;
    }
}
