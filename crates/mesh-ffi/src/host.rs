//! The process-global host and its UniFFI exports.
//!
//! Every export runs its work on the host's own runtime (a UniFFI async call is polled by
//! the foreign executor, so the work is spawned there and its handle awaited) and catches
//! panics: a panic is logged, recorded as [`FfiStatus::last_error`], and leaves the host
//! in a defined state (stopped, if it happened during [`start`]).

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
#[cfg(feature = "test-hooks")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{FutureExt, StreamExt};
use mesh_admin::{
    AdminRequest, AdminResponse, DaemonState, DeployOpts, ErrorCode, FileMeta, InProcessAdmin,
};
use mesh_host::logging::LoggingGuard;
use mesh_host::{
    Daemon, DaemonError, HostConfig, HostOptions, HostStatus, Periodic, PowerPolicy, PowerState,
    WakeLockSink, WakeReason,
};
use tracing::{error, info, warn};

/// The host runtime's worker threads.
const WORKER_THREADS: usize = 2;
/// How often [`stop`] looks whether the last other user of the daemon (a wake) let go.
const RELEASE_POLL: Duration = Duration::from_millis(20);
/// When [`stop`] warns that it is still waiting for the daemon.
const RELEASE_WARN: Duration = Duration::from_secs(15);

/// Kotlin gets a flat exception (`FfiException.Start` etc.) whose `message` is this
/// error's text: a variant field named `message` would clash with `Throwable.message` in
/// the generated Kotlin.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum FfiError {
    /// Android: `MeshNative.initAndroid(context)` has not run.
    #[error("the Android context is not initialised (call MeshNative.initAndroid first)")]
    NotInitialised,
    /// Another host (in this process with another data directory, or another process)
    /// runs.
    #[error("already running: {message}")]
    AlreadyRunning { message: String },
    #[error("cannot start the host: {message}")]
    Start { message: String },
    #[error("the host is not running")]
    NotRunning,
    #[error("invalid bytes: {message}")]
    Decode { message: String },
    #[error("{message}")]
    Panic { message: String },
}

/// The host's state: `Stopped` when no daemon runs, otherwise the daemon's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FfiState {
    Stopped,
    Uninitialised,
    Running,
    Removed,
    Rebootstrapping,
}

/// [`mesh_host::HostStatus`], plus the last start error or panic.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FfiStatus {
    pub state: FfiState,
    /// Hex, while a node runs.
    pub node_id: Option<String>,
    /// The mesh id in hex, while a node runs.
    pub mesh_name: Option<String>,
    pub services_hosted: u32,
    pub peers_online: u32,
    /// The gateway's main port first, then the mapped ones.
    pub gateway_ports: Vec<u16>,
    /// Why the last start failed, why `initAndroid` failed, or the last panic; cleared
    /// by a successful start.
    pub last_error: Option<String>,
    /// The power policy in force (from `meshd.toml`, changed by [`set_power`]); `None`
    /// when stopped.
    pub power_policy: Option<FfiPowerPolicy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FfiWakeReason {
    Alarm,
    Network,
    Boot,
    Incoming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FfiPowerPolicy {
    Balanced,
    AlwaysOn,
    Saver,
}

/// One file of a static site.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FfiFile {
    pub path: String,
    pub data: Vec<u8>,
}

/// The platform's wake locks (Android `PowerManager.PARTIAL_WAKE_LOCK`, one per tag).
/// `acquire` on a held tag refreshes its timeout; `max_ms == u64::MAX` (Kotlin
/// `ULong.MAX_VALUE`) means no timeout. Both must return quickly and must not call back
/// into this library.
#[uniffi::export(with_foreign)]
pub trait FfiWakeLockSink: Send + Sync {
    fn acquire(&self, tag: String, max_ms: u64);
    fn release(&self, tag: String);
}

/// Starts and stops the Android foreground service (used by the Tauri commands).
#[uniffi::export(with_foreign)]
pub trait FfiServiceControl: Send + Sync {
    fn start_service(&self);
    fn stop_service(&self);
}

/// Receives the lines of [`admin_logs`]. `line` returns `false` to end the stream; `end`
/// is called once, last, with the postcard `AdminResponse` that ended it (`Ok` or
/// `Error`).
#[uniffi::export(with_foreign)]
pub trait FfiLineSink: Send + Sync {
    fn line(&self, line: String) -> bool;
    fn end(&self, response: Vec<u8>);
}

/// Hands the host's wake-lock events to the sink registered last. It remembers the locks
/// held, so a sink registered after a lock was taken is told about it.
#[derive(Default)]
struct SinkSlot(Mutex<SinkSt>);

#[derive(Default)]
struct SinkSt {
    sink: Option<Arc<dyn FfiWakeLockSink>>,
    /// The locks held and when each times out (`None`: never).
    held: BTreeMap<String, Option<Instant>>,
}

/// The timeout left until `deadline`, as `acquire` takes it: `u64::MAX` for none, at
/// least 1 ms otherwise.
fn remaining_ms(deadline: Option<Instant>, now: Instant) -> u64 {
    match deadline {
        None => u64::MAX,
        Some(d) => {
            let ms = d.saturating_duration_since(now).as_millis();
            u64::try_from(ms).unwrap_or(u64::MAX - 1).max(1)
        }
    }
}

impl SinkSlot {
    /// Registers `sink`: the previous sink lets go of the locks held, the new one takes
    /// them for the time they have left.
    fn set(&self, sink: Arc<dyn FfiWakeLockSink>) {
        let mut st = lock(&self.0);
        if let Some(old) = &st.sink {
            if Arc::ptr_eq(old, &sink) {
                return;
            }
            for tag in st.held.keys() {
                old.release(tag.clone());
            }
        }
        let now = Instant::now();
        for (tag, deadline) in &st.held {
            sink.acquire(tag.clone(), remaining_ms(*deadline, now));
        }
        st.sink = Some(sink);
    }
}

impl WakeLockSink for SinkSlot {
    fn acquire(&self, tag: &str, max_ms: u64) {
        let mut st = lock(&self.0);
        let deadline = if max_ms == u64::MAX {
            None
        } else {
            Instant::now().checked_add(Duration::from_millis(max_ms))
        };
        st.held.insert(tag.to_owned(), deadline);
        if let Some(sink) = &st.sink {
            sink.acquire(tag.to_owned(), max_ms);
        }
    }

    fn release(&self, tag: &str) {
        let mut st = lock(&self.0);
        st.held.remove(tag);
        if let Some(sink) = &st.sink {
            sink.release(tag.to_owned());
        }
    }
}

/// The daemon started by [`start`].
struct Running {
    daemon: Arc<Daemon>,
    data_dir: PathBuf,
}

struct Host {
    rt: tokio::runtime::Runtime,
    /// Serialises [`start`] and [`stop`]; holds the daemon while it runs.
    daemon: tokio::sync::Mutex<Option<Running>>,
    /// The running daemon, for the other calls (set once a start completed, cleared when
    /// a stop begins). A wake holds a clone while it runs; [`stop`] waits for it.
    live: Mutex<Option<Arc<Daemon>>>,
    /// The `stops` generation `live` was published under (read and written with the
    /// `live` lock held): a different current generation means a stop has begun.
    live_stops: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// Bumped by every [`stop`]: a start asked for before a stop does not start after it,
    /// and `admin_logs` streams end.
    stops: tokio::sync::watch::Sender<u64>,
    /// The last power state reported (applied to every daemon started).
    power: Mutex<Option<PowerState>>,
    sink: Arc<SinkSlot>,
    control: Mutex<Option<Arc<dyn FfiServiceControl>>>,
    /// The global log subscriber's file writer, once installed (first start).
    log: Mutex<Option<LoggingGuard>>,
    #[cfg(feature = "test-hooks")]
    test: TestHooks,
}

#[cfg(feature = "test-hooks")]
#[derive(Default)]
struct TestHooks {
    starts: AtomicU64,
    panic_next: AtomicBool,
    pause_next: AtomicBool,
    paused: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

static HOST: OnceLock<Host> = OnceLock::new();

fn host() -> &'static Host {
    HOST.get_or_init(|| Host {
        rt: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("meshnode")
            .enable_all()
            .build()
            .expect("cannot build the host runtime"),
        daemon: tokio::sync::Mutex::new(None),
        live: Mutex::new(None),
        live_stops: AtomicU64::new(0),
        last_error: Mutex::new(None),
        stops: tokio::sync::watch::Sender::new(0),
        power: Mutex::new(None),
        sink: Arc::new(SinkSlot::default()),
        control: Mutex::new(None),
        log: Mutex::new(None),
        #[cfg(feature = "test-hooks")]
        test: TestHooks::default(),
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Logs a panic in `what` and records it as the last error. Returns the message.
fn record_panic(what: &str, msg: &str) -> String {
    let msg = format!("{what} panicked: {msg}");
    error!("{msg}");
    if let Some(h) = HOST.get() {
        *lock(&h.last_error) = Some(msg.clone());
    }
    msg
}

/// Calls `make` with the host now (when the export is first polled), then runs the
/// future it returns on the host's runtime. `Err` is a panic (or the runtime going away).
async fn on_host<F, Fut, T>(make: F) -> Result<T, String>
where
    F: FnOnce(&'static Host) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let task = catch_unwind(AssertUnwindSafe(|| {
        let h = host();
        h.rt.spawn(AssertUnwindSafe(make(h)).catch_unwind())
    }))
    .map_err(|p| panic_message(&*p))?;
    match task.await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(p)) => Err(panic_message(&*p)),
        Err(e) => Err(e.to_string()),
    }
}

/// Runs a synchronous export, turning a panic into `fallback(message)`.
fn guarded<T>(what: &str, f: impl FnOnce() -> T, fallback: impl FnOnce(String) -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(p) => fallback(record_panic(what, &panic_message(&*p))),
    }
}

fn panicked<T>(what: &str) -> impl FnOnce(String) -> Result<T, FfiError> + '_ {
    move |msg| {
        Err(FfiError::Panic {
            message: record_panic(what, &msg),
        })
    }
}

impl Host {
    fn status(&self) -> FfiStatus {
        let live = lock(&self.live).clone();
        let last_error = lock(&self.last_error).clone();
        match live {
            Some(d) => status_of(d.status(), d.power_state().policy, last_error),
            None => stopped(last_error),
        }
    }

    fn admin(&self) -> Result<InProcessAdmin, FfiError> {
        lock(&self.live)
            .as_ref()
            .map(|d| d.in_process_admin())
            .ok_or(FfiError::NotRunning)
    }

    /// Installs the global log subscriber (`<data>/logs`, plus logcat on Android) once.
    fn init_logging(&self, dir: &Path, level: &str) {
        // The tests' data directories are temporary: no log files are kept open in them.
        if cfg!(feature = "test-hooks") {
            return;
        }
        let mut log = lock(&self.log);
        if log.is_none() {
            #[cfg(target_os = "android")]
            let extra = crate::android::log_layer();
            #[cfg(not(target_os = "android"))]
            let extra = None;
            *log = Some(mesh_host::logging::init_with(dir, level, extra));
        }
    }

    async fn start(&'static self, data_dir: String, stops: u64) -> Result<FfiStatus, FfiError> {
        #[cfg(target_os = "android")]
        if !crate::android::initialised() {
            return Err(FfiError::NotInitialised);
        }
        let dir = PathBuf::from(data_dir);
        let mut slot = self.daemon.lock().await;
        if let Some(run) = slot.as_ref() {
            if run.data_dir != dir {
                return Err(FfiError::AlreadyRunning {
                    message: format!("the host runs with {}", run.data_dir.display()),
                });
            }
            return Ok(self.status());
        }
        if *self.stops.borrow() != stops {
            info!("a stop was asked for after this start: not starting");
            return Ok(self.status());
        }
        let daemon = match self.start_daemon(&dir).await {
            Ok(d) => Arc::new(d),
            Err(e) => {
                warn!("cannot start the host: {e}");
                *lock(&self.last_error) = Some(e.to_string());
                return Err(e);
            }
        };
        if *self.stops.borrow() != stops {
            info!("a stop was asked for while starting: stopping again");
            shutdown(daemon).await;
            return Ok(self.status());
        }
        let published = catch_unwind(AssertUnwindSafe(|| {
            #[cfg(feature = "test-hooks")]
            if self.test.panic_next.swap(false, Ordering::SeqCst) {
                panic!("test panic in start");
            }
            // Under the power lock, so that a concurrent `set_power` is not lost. The
            // policy is the one in meshd.toml (the daemon loaded it); charging and screen
            // are the platform's last report.
            let mut power = lock(&self.power);
            if let Some(reported) = *power {
                let state = PowerState {
                    policy: daemon.power_state().policy,
                    ..reported
                };
                daemon.set_power(state);
                *power = Some(state);
            }
            let mut live = lock(&self.live);
            *live = Some(daemon.clone());
            self.live_stops.store(stops, Ordering::SeqCst);
        }));
        if let Err(p) = published {
            *lock(&self.live) = None;
            shutdown(daemon).await;
            std::panic::resume_unwind(p);
        }
        *slot = Some(Running {
            daemon,
            data_dir: dir,
        });
        *lock(&self.last_error) = None;
        drop(slot);
        let st = self.status();
        info!(state = ?st.state, "host started");
        Ok(st)
    }

    async fn start_daemon(&self, dir: &Path) -> Result<Daemon, FfiError> {
        let cfg = HostConfig::load_or_default(dir).map_err(|e| FfiError::Start {
            message: e.to_string(),
        })?;
        self.init_logging(dir, &cfg.log_level);
        let sink: Arc<dyn WakeLockSink> = self.sink.clone();
        let opts = HostOptions {
            admin_socket: false,
            periodic: Periodic::Driven,
            power: Some(sink),
        };
        let daemon = Daemon::start_opts(dir.to_owned(), cfg, opts)
            .await
            .map_err(|e| match e {
                DaemonError::AlreadyRunning(_) => FfiError::AlreadyRunning {
                    message: e.to_string(),
                },
                e => FfiError::Start {
                    message: e.to_string(),
                },
            })?;
        #[cfg(feature = "test-hooks")]
        self.test.starts.fetch_add(1, Ordering::SeqCst);
        #[cfg(feature = "test-hooks")]
        if self.test.pause_next.swap(false, Ordering::SeqCst) {
            self.test.paused.notify_one();
            self.test.resume.notified().await;
        }
        Ok(daemon)
    }

    async fn stop(&'static self) {
        let mut slot = self.daemon.lock().await;
        let Some(run) = slot.take() else { return };
        *lock(&self.live) = None;
        shutdown(run.daemon).await;
        info!("host stopped");
    }
}

/// Shuts `daemon` down once no wake holds it any more.
async fn shutdown(mut daemon: Arc<Daemon>) {
    let mut waited = Duration::ZERO;
    loop {
        match Arc::try_unwrap(daemon) {
            Ok(d) => return d.shutdown().await,
            Err(shared) => {
                daemon = shared;
                if waited == RELEASE_WARN {
                    warn!("stop is still waiting for a wake to end");
                }
                tokio::time::sleep(RELEASE_POLL).await;
                waited += RELEASE_POLL;
            }
        }
    }
}

fn stopped(last_error: Option<String>) -> FfiStatus {
    FfiStatus {
        state: FfiState::Stopped,
        node_id: None,
        mesh_name: None,
        services_hosted: 0,
        peers_online: 0,
        gateway_ports: Vec::new(),
        last_error,
        power_policy: None,
    }
}

fn status_of(s: HostStatus, policy: PowerPolicy, last_error: Option<String>) -> FfiStatus {
    FfiStatus {
        state: match s.state {
            DaemonState::Uninitialised => FfiState::Uninitialised,
            DaemonState::Running => FfiState::Running,
            DaemonState::Removed => FfiState::Removed,
            DaemonState::Rebootstrapping => FfiState::Rebootstrapping,
        },
        node_id: s.node_id,
        mesh_name: s.mesh_name,
        services_hosted: s.services_hosted,
        peers_online: s.peers_online,
        gateway_ports: s.gateway_ports,
        last_error,
        power_policy: Some(policy.into()),
    }
}

impl From<FfiWakeReason> for WakeReason {
    fn from(r: FfiWakeReason) -> Self {
        match r {
            FfiWakeReason::Alarm => WakeReason::Alarm,
            FfiWakeReason::Network => WakeReason::Network,
            FfiWakeReason::Boot => WakeReason::Boot,
            FfiWakeReason::Incoming => WakeReason::Incoming,
        }
    }
}

impl From<PowerPolicy> for FfiPowerPolicy {
    fn from(p: PowerPolicy) -> Self {
        match p {
            PowerPolicy::Balanced => FfiPowerPolicy::Balanced,
            PowerPolicy::AlwaysOn => FfiPowerPolicy::AlwaysOn,
            PowerPolicy::Saver => FfiPowerPolicy::Saver,
        }
    }
}

impl From<FfiPowerPolicy> for PowerPolicy {
    fn from(p: FfiPowerPolicy) -> Self {
        match p {
            FfiPowerPolicy::Balanced => PowerPolicy::Balanced,
            FfiPowerPolicy::AlwaysOn => PowerPolicy::AlwaysOn,
            FfiPowerPolicy::Saver => PowerPolicy::Saver,
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, FfiError> {
    postcard::from_bytes(bytes).map_err(|e| FfiError::Decode {
        message: e.to_string(),
    })
}

fn encode(resp: &AdminResponse) -> Result<Vec<u8>, FfiError> {
    postcard::to_allocvec(resp).map_err(|e| FfiError::Decode {
        message: format!("cannot encode the response: {e}"),
    })
}

/// Starts the host on `data_dir`: reads `meshd.toml` and starts the daemon (no admin
/// socket; periodic work driven by the platform; wake locks through the registered sink).
/// Idempotent: while running (with the same directory) it returns the status; concurrent
/// calls start one daemon. A [`stop`] called after this start was called wins: then this
/// returns `Ok` with state `Stopped`. The power policy is the one in `meshd.toml`; a
/// [`set_power`] before the start only contributes charging and screen state.
#[uniffi::export]
pub async fn start(data_dir: String) -> Result<FfiStatus, FfiError> {
    on_host(move |h| {
        let stops = *h.stops.borrow();
        h.start(data_dir, stops)
    })
    .await
    .unwrap_or_else(panicked("start"))
}

/// Shuts the daemon down (within 20 s) and releases the data directory. No-op if
/// stopped.
#[uniffi::export]
pub async fn stop() {
    let r = on_host(|h| {
        // Ends the `admin_logs` streams (they watch this), and any start called earlier.
        h.stops.send_modify(|n| *n += 1);
        h.stop()
    })
    .await;
    if let Err(msg) = r {
        record_panic("stop", &msg);
    }
}

#[uniffi::export]
pub fn status() -> FfiStatus {
    guarded("status", || host().status(), |msg| stopped(Some(msg)))
}

/// Catches up now (redial and sync), for at most 10 s. No-op while stopped.
#[uniffi::export]
pub async fn wake(reason: FfiWakeReason) {
    let r = on_host(move |h| {
        let daemon = lock(&h.live).clone();
        async move {
            if let Some(d) = daemon {
                d.wake(reason.into()).await;
            }
        }
    })
    .await;
    if let Err(msg) = r {
        record_panic("wake", &msg);
    }
}

/// The device's network changed: rebind and redial. No-op while stopped.
#[uniffi::export]
pub fn network_changed() {
    guarded(
        "network_changed",
        || {
            if let Some(d) = lock(&host().live).clone() {
                d.network_changed();
            }
        },
        drop,
    )
}

/// Reports the power state. While running, it is applied, and a policy change is saved
/// to `meshd.toml`. Before a start, only charging and screen are kept for it (the policy
/// in `meshd.toml` wins).
#[uniffi::export]
pub fn set_power(policy: FfiPowerPolicy, charging: bool, screen_on: bool) {
    guarded(
        "set_power",
        || {
            let h = host();
            let state = PowerState {
                policy: policy.into(),
                charging,
                screen_on,
            };
            let mut power = lock(&h.power);
            *power = Some(state);
            let daemon = lock(&h.live).clone();
            if let Some(d) = daemon {
                d.set_power(state);
            }
        },
        drop,
    )
}

/// Registers the wake-lock sink (before or after [`start`]); it replaces the previous one,
/// which releases the locks held while the new one takes them.
#[uniffi::export]
pub fn set_wake_lock_sink(sink: Arc<dyn FfiWakeLockSink>) {
    guarded("set_wake_lock_sink", || host().sink.set(sink), drop)
}

/// Registers how to start and stop the foreground service (see [`service_control`]).
#[uniffi::export]
pub fn set_service_control(ctl: Arc<dyn FfiServiceControl>) {
    guarded(
        "set_service_control",
        || *lock(&host().control) = Some(ctl),
        drop,
    )
}

/// The registered [`FfiServiceControl`], for the Tauri commands (not exported).
pub fn service_control() -> Option<Arc<dyn FfiServiceControl>> {
    HOST.get().and_then(|h| lock(&h.control).clone())
}

/// One admin request: postcard `AdminRequest` in, postcard `AdminResponse` out (which may
/// be `AdminResponse::Error`). `DeployStatic`, `LogsTail` and `Probe` go through
/// [`admin_deploy_static`] and [`admin_logs`].
#[uniffi::export]
pub async fn admin(request: Vec<u8>) -> Result<Vec<u8>, FfiError> {
    on_host(|h| {
        let admin = h.admin();
        async move {
            let admin = admin?;
            let req: AdminRequest = decode(&request)?;
            encode(&admin.request(req).await)
        }
    })
    .await
    .unwrap_or_else(panicked("admin"))
}

/// Deploys a static site: `opts` is a postcard `DeployOpts`; returns a postcard
/// `AdminResponse`.
#[uniffi::export]
pub async fn admin_deploy_static(
    name: String,
    opts: Vec<u8>,
    files: Vec<FfiFile>,
) -> Result<Vec<u8>, FfiError> {
    on_host(|h| {
        let admin = h.admin();
        async move {
            let admin = admin?;
            let opts: DeployOpts = decode(&opts)?;
            let files = files
                .into_iter()
                .map(|f| {
                    let meta = FileMeta {
                        path: f.path,
                        size: f.data.len() as u64,
                    };
                    (meta, f.data)
                })
                .collect();
            encode(&admin.deploy_static(name, opts, files).await)
        }
    })
    .await
    .unwrap_or_else(panicked("admin_deploy_static"))
}

/// Streams a line-streaming request (postcard `AdminRequest::LogsTail` or `Probe`) to
/// `sink`, on the host's runtime; returns at once. `sink.end` gets the ending response,
/// also when the request cannot start (not running, invalid bytes).
#[uniffi::export]
pub fn admin_logs(request: Vec<u8>, sink: Arc<dyn FfiLineSink>) {
    let end_with = |sink: &dyn FfiLineSink, resp: AdminResponse| {
        if let Ok(bytes) = encode(&resp) {
            sink.end(bytes);
        }
    };
    let started = catch_unwind(AssertUnwindSafe(|| {
        let h = host();
        // Subscribe first, then check that no stop began since the daemon was published:
        // a stop either happened before (refused here) or wakes `stopping` later.
        let mut stopping = h.stops.subscribe();
        let current = *stopping.borrow_and_update();
        let admin = {
            let live = lock(&h.live);
            match live.as_ref() {
                Some(d) if h.live_stops.load(Ordering::SeqCst) == current => d.in_process_admin(),
                _ => return Err((ErrorCode::NotRunning, FfiError::NotRunning.to_string())),
            }
        };
        let req: AdminRequest = match decode(&request) {
            Ok(r) => r,
            Err(e) => {
                return Err((ErrorCode::InvalidArgument, e.to_string()));
            }
        };
        let sink = sink.clone();
        // A stop ends the stream (a `follow` would otherwise wait for its next line).
        h.rt.spawn(async move {
            let run = AssertUnwindSafe(async {
                let mut lines = std::pin::pin!(admin.logs_tail(req));
                loop {
                    tokio::select! {
                        next = lines.next() => match next {
                            Some(Ok(line)) => {
                                if !sink.line(line) {
                                    break AdminResponse::Ok;
                                }
                            }
                            Some(Err(resp)) => break resp,
                            None => break AdminResponse::Ok,
                        },
                        _ = stopping.changed() => {
                            break AdminResponse::error(ErrorCode::NotRunning, "the host stopped");
                        }
                    }
                }
            });
            let resp = match run.catch_unwind().await {
                Ok(resp) => resp,
                Err(p) => {
                    let msg = record_panic("admin_logs", &panic_message(&*p));
                    AdminResponse::error(ErrorCode::Internal, msg)
                }
            };
            let _ = catch_unwind(AssertUnwindSafe(|| end_with(&*sink, resp)));
        });
        Ok(())
    }));
    let resp = match started {
        Ok(Ok(())) => return,
        Ok(Err((code, msg))) => AdminResponse::error(code, msg),
        Err(p) => {
            let msg = record_panic("admin_logs", &panic_message(&*p));
            AdminResponse::error(ErrorCode::Internal, msg)
        }
    };
    let _ = catch_unwind(AssertUnwindSafe(|| end_with(&*sink, resp)));
}

/// Daemons started so far.
#[cfg(feature = "test-hooks")]
pub fn starts_for_test() -> u64 {
    host().test.starts.load(Ordering::SeqCst)
}

/// Makes the next start panic once its daemon is up.
#[cfg(feature = "test-hooks")]
pub fn panic_in_next_start_for_test() {
    host().test.panic_next.store(true, Ordering::SeqCst);
}

/// Makes the next start wait, once its daemon is up (holding the data lock), until
/// [`resume_start_for_test`]; [`start_paused_for_test`] resolves when it waits.
#[cfg(feature = "test-hooks")]
pub fn pause_next_start_for_test() {
    host().test.pause_next.store(true, Ordering::SeqCst);
}

#[cfg(feature = "test-hooks")]
pub async fn start_paused_for_test() {
    host().test.paused.notified().await;
}

#[cfg(feature = "test-hooks")]
pub fn resume_start_for_test() {
    host().test.resume.notify_one();
}

/// Records why the platform setup failed (shown as [`FfiStatus::last_error`]).
#[cfg(target_os = "android")]
pub(crate) fn record_error(msg: String) {
    error!("{msg}");
    *lock(&host().last_error) = Some(msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec(Mutex<Vec<(String, u64)>>);

    impl FfiWakeLockSink for Rec {
        fn acquire(&self, tag: String, max_ms: u64) {
            lock(&self.0).push((tag, max_ms));
        }
        fn release(&self, _tag: String) {}
    }

    #[test]
    fn a_new_sink_gets_the_time_left() {
        let slot = SinkSlot::default();
        slot.acquire("request", 60_000);
        slot.acquire("always", u64::MAX);
        slot.acquire("alarm", 0);
        std::thread::sleep(Duration::from_millis(20));
        let rec = Arc::new(Rec::default());
        slot.set(rec.clone());
        let got: BTreeMap<_, _> = lock(&rec.0).iter().cloned().collect();
        assert_eq!(got["always"], u64::MAX);
        assert_eq!(got["alarm"], 1, "expired: at least 1 ms");
        assert!((1..60_000).contains(&got["request"]), "{got:?}");
        assert!(got["request"] > 50_000, "{got:?}");
        let now = Instant::now();
        assert_eq!(remaining_ms(Some(now), now + Duration::from_secs(1)), 1);
        assert_eq!(remaining_ms(None, now), u64::MAX);
    }
}
