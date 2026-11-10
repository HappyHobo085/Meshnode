//! The Windows service (spec §6.4): installed with the service control manager (SCM) as
//! `meshnode`, running `%ProgramFiles%\Meshnode\meshd.exe run --service --data <data>` as
//! `NT AUTHORITY\LocalService`; and the service side of `meshd run --service`, which enters
//! the service dispatcher, reports its state to the SCM and maps a service stop to the
//! daemon's orderly shutdown.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing::{error, info};
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SUCCESS, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_ELEVATION, TOKEN_QUERY,
    TOKEN_USER, TokenElevation, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::eventlog::{self, Event, EventLog};
use super::{SERVICE_NAME, ServiceError, ServiceStatus, io_err};
use crate::{Daemon, MeshdConfig, logging};

/// The service's display name.
pub const DISPLAY_NAME: &str = "Meshnode";
/// The account the service runs as.
pub const ACCOUNT: &str = r"NT AUTHORITY\LocalService";
/// The data directory's DACL: full control for LocalService and Administrators only, not
/// inherited from `%ProgramData%`.
pub const DATA_DIR_SDDL: &str = "D:P(A;OICI;FA;;;LS)(A;OICI;FA;;;BA)";
/// Restarts after a failure, and the delay before each.
pub const RESTARTS: usize = 3;
pub const RESTART_DELAY: Duration = Duration::from_secs(5);
/// When the failure count goes back to zero.
const FAILURE_RESET: Duration = Duration::from_secs(24 * 3600);
/// How long the SCM should wait for a start (opening the node) and for a stop (the
/// daemon's shutdown takes at most 20 s).
const START_WAIT_HINT: Duration = Duration::from_secs(60);
pub const STOP_WAIT_HINT: Duration = Duration::from_secs(25);
/// How long `stop` and `uninstall` wait for the service to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);

// --- configuration (pure) ----------------------------------------------------------------

/// `%ProgramFiles%\Meshnode`, where the binary is installed.
pub fn install_dir() -> PathBuf {
    let program_files = std::env::var_os("ProgramFiles")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| r"C:\Program Files".into());
    PathBuf::from(program_files).join("Meshnode")
}

/// The service's registration for the installed binary `exe` and the data directory
/// `data`.
pub fn service_info(exe: &Path, data: &Path) -> ServiceInfo {
    ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec![
            "run".into(),
            "--service".into(),
            "--data".into(),
            data.as_os_str().to_owned(),
        ],
        dependencies: Vec::new(),
        account_name: Some(ACCOUNT.into()),
        account_password: None,
    }
}

/// Restart three times, 5 s after each failure.
pub fn failure_actions() -> ServiceFailureActions {
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: RESTART_DELAY,
            };
            RESTARTS
        ]),
    }
}

/// The SCM's state of an installed service, as `service status` reports it.
pub fn status_of(state: ServiceState) -> ServiceStatus {
    match state {
        ServiceState::Running => ServiceStatus::Running,
        ServiceState::StartPending | ServiceState::ContinuePending => ServiceStatus::Starting,
        ServiceState::StopPending | ServiceState::PausePending => ServiceStatus::Stopping,
        ServiceState::Stopped | ServiceState::Paused => ServiceStatus::Stopped,
    }
}

fn describe(e: &windows_service::Error) -> String {
    match e {
        windows_service::Error::Winapi(io) => io.to_string(),
        e => e.to_string(),
    }
}

fn scm_err(what: &'static str) -> impl FnOnce(windows_service::Error) -> ServiceError {
    move |e| ServiceError::Failed(format!("{what}: {}", describe(&e)))
}

fn os_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

// --- install, uninstall, start, stop, status ---------------------------------------------

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, ServiceError> {
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(scm_err("connecting to the service control manager"))
}

/// The installed service, or `None` when there is none.
fn open(manager: &ServiceManager, access: ServiceAccess) -> Result<Option<Service>, ServiceError> {
    match manager.open_service(SERVICE_NAME, access) {
        Ok(s) => Ok(Some(s)),
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) =>
        {
            Ok(None)
        }
        Err(e) => Err(scm_err("opening the meshnode service")(e)),
    }
}

pub fn install(data: &Path) -> Result<(), ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    if open(&scm, ServiceAccess::QUERY_STATUS)?.is_some() {
        return Err(ServiceError::AlreadyInstalled);
    }
    let exe = install_binary()?;
    protect_data_dir(data)?;
    // The admin pipe also admits the installing user (spec §5.2).
    let mut cfg = MeshdConfig::load_or_default(data)?;
    cfg.admin.windows_user_sid =
        Some(current_user_sid().map_err(io_err("reading the installing user's SID"))?);
    cfg.save(data)?;
    eventlog::register().map_err(io_err("registering the Meshnode Event Log source"))?;

    let service = scm
        .create_service(
            &service_info(&exe, data),
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(scm_err("creating the meshnode service"))?;
    service
        .set_description("Meshnode mesh daemon: the node, its services and the localhost gateway")
        .map_err(scm_err("describing the meshnode service"))?;
    service
        .set_delayed_auto_start(true)
        .map_err(scm_err("setting the delayed automatic start"))?;
    service
        .update_failure_actions(failure_actions())
        .map_err(scm_err("setting the restart-on-failure actions"))?;
    // A failed start ends in Stopped with an error code rather than a crash; restart then too.
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(scm_err("setting the restart-on-failure actions"))?;
    service
        .start::<&OsStr>(&[])
        .map_err(scm_err("starting the meshnode service"))
}

/// Copies the running executable to `%ProgramFiles%\Meshnode\meshd.exe`, unless it is that
/// file, and returns the installed path.
fn install_binary() -> Result<PathBuf, ServiceError> {
    let dir = install_dir();
    let target = dir.join("meshd.exe");
    let source = std::env::current_exe().map_err(io_err("finding the meshd executable"))?;
    if same_file(&source, &target) {
        return Ok(target);
    }
    std::fs::create_dir_all(&dir).map_err(io_err(format!("creating {}", dir.display())))?;
    std::fs::copy(&source, &target).map_err(io_err(format!(
        "copying {} to {}",
        source.display(),
        target.display()
    )))?;
    Ok(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Creates the data directory if needed and replaces its DACL with [`DATA_DIR_SDDL`]
/// (protected, so `%ProgramData%`'s inherited entries are dropped; the entries are
/// inherited by everything inside).
fn protect_data_dir(data: &Path) -> Result<(), ServiceError> {
    std::fs::create_dir_all(data).map_err(io_err(format!("creating {}", data.display())))?;
    set_protected_dacl(data, DATA_DIR_SDDL).map_err(io_err(format!(
        "setting the permissions of {}",
        data.display()
    )))
}

fn set_protected_dacl(path: &Path, sddl: &str) -> io::Result<()> {
    let sddl = wide(sddl);
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; `sd` is a valid out pointer; the size is optional.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `sd` is the descriptor just created; the out pointers are valid.
    let ok = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) };
    let result = if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        let name = wide(&path.as_os_str().to_string_lossy());
        // SAFETY: `name` is NUL-terminated; `dacl` points into `sd`, alive until freed below.
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(os_error(code))
        }
    };
    // SAFETY: allocated with LocalAlloc by the conversion above, freed once.
    unsafe { LocalFree(sd) };
    result
}

pub fn uninstall() -> Result<(), ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE;
    if let Some(service) = open(&scm, access)? {
        stop_and_wait(&service)?;
        service
            .delete()
            .map_err(scm_err("deleting the meshnode service"))?;
    }
    eventlog::unregister().map_err(io_err("removing the Meshnode Event Log source"))?;
    remove_binary()
}

/// Removes the installed binary and its directory — unless this process runs from it
/// (Windows cannot delete a running executable); it is then left in place.
fn remove_binary() -> Result<(), ServiceError> {
    let dir = install_dir();
    let target = dir.join("meshd.exe");
    let running = std::env::current_exe().is_ok_and(|exe| same_file(&exe, &target));
    if running || !target.exists() {
        return Ok(());
    }
    std::fs::remove_file(&target).map_err(io_err(format!("removing {}", target.display())))?;
    // Only when empty.
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

pub fn start() -> Result<(), ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT)?;
    let service = open(&scm, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?
        .ok_or(ServiceError::NotInstalled)?;
    let state = service
        .query_status()
        .map_err(scm_err("querying the meshnode service"))?
        .current_state;
    if state != ServiceState::Stopped {
        return Ok(());
    }
    service
        .start::<&OsStr>(&[])
        .map_err(scm_err("starting the meshnode service"))
}

pub fn stop() -> Result<(), ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT)?;
    let service = open(&scm, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?
        .ok_or(ServiceError::NotInstalled)?;
    stop_and_wait(&service)
}

/// Asks the service to stop (unless it is stopped) and waits until it is.
fn stop_and_wait(service: &Service) -> Result<(), ServiceError> {
    let query = || {
        service
            .query_status()
            .map(|s| s.current_state)
            .map_err(scm_err("querying the meshnode service"))
    };
    let mut state = query()?;
    if state == ServiceState::Stopped {
        return Ok(());
    }
    if state != ServiceState::StopPending {
        state = service
            .stop()
            .map_err(scm_err("stopping the meshnode service"))?
            .current_state;
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while state != ServiceState::Stopped {
        if Instant::now() >= deadline {
            return Err(ServiceError::Failed(format!(
                "the meshnode service did not stop within {} s",
                STOP_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(POLL);
        state = query()?;
    }
    Ok(())
}

pub fn status() -> Result<ServiceStatus, ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT)?;
    let Some(service) = open(&scm, ServiceAccess::QUERY_STATUS)? else {
        return Ok(ServiceStatus::NotInstalled);
    };
    let state = service
        .query_status()
        .map_err(scm_err("querying the meshnode service"))?
        .current_state;
    Ok(status_of(state))
}

// --- running as the service --------------------------------------------------------------

/// What the service does with a control from the SCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlReply {
    /// Begin the daemon's orderly shutdown.
    Stop,
    /// Acknowledge (the SCM asks for the current status).
    Acknowledge,
    NotImplemented,
}

pub fn control_reply(control: &ServiceControl) -> ControlReply {
    match control {
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            ControlReply::Stop
        }
        ServiceControl::Interrogate => ControlReply::Acknowledge,
        _ => ControlReply::NotImplemented,
    }
}

/// The status reported to the SCM in `state`: while running it accepts stop and system
/// shutdown; pending states carry a wait hint.
pub fn reported_status(
    state: ServiceState,
    exit_code: ServiceExitCode,
) -> windows_service::service::ServiceStatus {
    let (controls_accepted, wait_hint) = match state {
        ServiceState::Running => (
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            Duration::ZERO,
        ),
        ServiceState::StartPending => (ServiceControlAccept::empty(), START_WAIT_HINT),
        ServiceState::StopPending => (ServiceControlAccept::empty(), STOP_WAIT_HINT),
        _ => (ServiceControlAccept::empty(), Duration::ZERO),
    };
    windows_service::service::ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted,
        exit_code,
        checkpoint: u32::from(!wait_hint.is_zero()),
        wait_hint,
        process_id: None,
    }
}

/// What `service_main` needs, set before entering the dispatcher.
struct ServiceContext {
    data: PathBuf,
    runtime: tokio::runtime::Handle,
}

static CONTEXT: OnceLock<ServiceContext> = OnceLock::new();

windows_service::define_windows_service!(ffi_service_main, service_main);

/// `meshd run --service`: hands this thread to the SCM's service dispatcher, which runs
/// the service on its own thread (driving the daemon on `runtime`) and returns when the
/// service has stopped. Fails when the process was not started by the SCM.
pub fn run_dispatcher(data: PathBuf, runtime: tokio::runtime::Handle) -> Result<(), ServiceError> {
    if CONTEXT.set(ServiceContext { data, runtime }).is_err() {
        return Err(ServiceError::Failed(
            "the service dispatcher was already started".into(),
        ));
    }
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| match e {
        windows_service::Error::Winapi(e)
            if e.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) =>
        {
            ServiceError::Failed(
                "`meshd run --service` is started by the Windows service manager; \
                 use `meshd run` in a terminal"
                    .into(),
            )
        }
        e => scm_err("starting the service dispatcher")(e),
    })
}

fn service_main(_arguments: Vec<OsString>) {
    let Some(ctx) = CONTEXT.get() else { return };
    let events = EventLog::open();
    let report = |event: Event, message: &str| {
        if let Some(log) = &events {
            log.report(event, message);
        }
    };

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop_tx = Mutex::new(Some(stop_tx));
    let handler = move |control: ServiceControl| match control_reply(&control) {
        ControlReply::Stop => {
            if let Some(tx) = stop_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ControlReply::Acknowledge => ServiceControlHandlerResult::NoError,
        ControlReply::NotImplemented => ServiceControlHandlerResult::NotImplemented,
    };
    let status = match service_control_handler::register(SERVICE_NAME, handler) {
        Ok(h) => h,
        Err(e) => {
            report(
                Event::StartFailed,
                &format!(
                    "Meshnode could not register with the service manager: {}",
                    describe(&e)
                ),
            );
            return;
        }
    };
    let set = |state, exit| {
        let _ = status.set_service_status(reported_status(state, exit));
    };
    set(ServiceState::StartPending, ServiceExitCode::NO_ERROR);

    let result = ctx.runtime.block_on(async {
        let cfg = MeshdConfig::load_or_default(&ctx.data).map_err(|e| e.to_string())?;
        let _log = logging::init(&ctx.data, &cfg.log_level);
        let daemon = match Daemon::start(ctx.data.clone(), cfg).await {
            Ok(d) => d,
            Err(e) => {
                error!("meshd failed to start: {e}");
                return Err(e.to_string());
            }
        };
        set(ServiceState::Running, ServiceExitCode::NO_ERROR);
        info!("running as the {SERVICE_NAME} Windows service");
        report(
            Event::Started,
            &format!("Meshnode started (data directory {}).", ctx.data.display()),
        );
        daemon
            .run_until(async {
                // A dropped sender (no handler any more) also stops the daemon.
                let _ = stop_rx.await;
                set(ServiceState::StopPending, ServiceExitCode::NO_ERROR);
            })
            .await;
        Ok::<(), String>(())
    });

    match result {
        Ok(()) => {
            report(Event::Stopped, "Meshnode stopped.");
            set(ServiceState::Stopped, ServiceExitCode::NO_ERROR);
        }
        Err(message) => {
            report(
                Event::StartFailed,
                &format!("Meshnode failed to start: {message}"),
            );
            set(ServiceState::Stopped, ServiceExitCode::ServiceSpecific(1));
        }
    }
}

// --- the process token -------------------------------------------------------------------

/// Whether this process's token is elevated (an Administrator past UAC).
pub fn token_is_elevated() -> bool {
    with_token(|token| {
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        // SAFETY: `elevation` is a valid TOKEN_ELEVATION buffer of the given size.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(elevation.TokenIsElevated != 0)
    })
    .unwrap_or(false)
}

/// The string SID of the user this process runs as. Under an elevated prompt this is the
/// account that elevated (the same user after a UAC consent prompt; another
/// administrator's account when that one's credentials were entered).
pub fn current_user_sid() -> io::Result<String> {
    with_token(|token| {
        let mut len = 0u32;
        // SAFETY: a size query with a null buffer; `len` is a valid out pointer.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
        // `u64` elements keep the buffer aligned for TOKEN_USER.
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // SAFETY: `buf` holds at least `len` bytes, aligned for TOKEN_USER.
        let ok = unsafe {
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call above filled `buf` with a TOKEN_USER whose SID points into `buf`.
        sid_string(unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid })
    })
}

/// Runs `f` with this process's token, open for querying.
fn with_token<T>(f: impl FnOnce(HANDLE) -> io::Result<T>) -> io::Result<T> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo handle of this process needs no closing; `token` is a valid out
    // pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = f(token);
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(token) };
    result
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the caller passes a valid SID; `text` is a valid out pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated string.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` u16s were just read.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: allocated with LocalAlloc by ConvertSidToStringSidW, freed once.
    unsafe { LocalFree(text.cast()) };
    Ok(s)
}

/// A NUL-terminated UTF-16 string.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_registration_matches_the_spec() {
        let exe = Path::new(r"C:\Program Files\Meshnode\meshd.exe");
        let data = Path::new(r"C:\ProgramData\meshnode");
        let info = service_info(exe, data);
        assert_eq!(info.name, "meshnode");
        assert_eq!(info.display_name, "Meshnode");
        assert_eq!(
            info.account_name.as_deref(),
            Some(OsStr::new(r"NT AUTHORITY\LocalService"))
        );
        assert_eq!(info.account_password, None);
        assert_eq!(info.start_type, ServiceStartType::AutoStart);
        assert_eq!(info.service_type, ServiceType::OWN_PROCESS);
        assert_eq!(info.executable_path, exe);
        assert_eq!(
            info.launch_arguments,
            ["run", "--service", "--data", r"C:\ProgramData\meshnode"]
                .map(OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn failure_actions_restart_three_times_after_5s() {
        let actions = failure_actions().actions.unwrap();
        assert_eq!(actions.len(), 3);
        for a in actions {
            assert_eq!(a.action_type, ServiceActionType::Restart);
            assert_eq!(a.delay, Duration::from_secs(5));
        }
    }

    #[test]
    fn install_dir_is_under_program_files() {
        assert!(install_dir().ends_with("Meshnode"));
    }

    #[test]
    fn stop_and_shutdown_stop_the_daemon() {
        assert_eq!(control_reply(&ServiceControl::Stop), ControlReply::Stop);
        assert_eq!(control_reply(&ServiceControl::Shutdown), ControlReply::Stop);
        assert_eq!(
            control_reply(&ServiceControl::Preshutdown),
            ControlReply::Stop
        );
        assert_eq!(
            control_reply(&ServiceControl::Interrogate),
            ControlReply::Acknowledge
        );
        assert_eq!(
            control_reply(&ServiceControl::Pause),
            ControlReply::NotImplemented
        );
    }

    #[test]
    fn reported_statuses() {
        let running = reported_status(ServiceState::Running, ServiceExitCode::NO_ERROR);
        assert_eq!(
            running.controls_accepted,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        );
        assert_eq!((running.checkpoint, running.wait_hint), (0, Duration::ZERO));
        let stopping = reported_status(ServiceState::StopPending, ServiceExitCode::NO_ERROR);
        assert!(stopping.controls_accepted.is_empty());
        // The daemon's shutdown takes at most 20 s.
        assert!(stopping.wait_hint > Duration::from_secs(20));
        assert_eq!(stopping.checkpoint, 1);
        let failed = reported_status(ServiceState::Stopped, ServiceExitCode::ServiceSpecific(1));
        assert_eq!(failed.exit_code, ServiceExitCode::ServiceSpecific(1));
        assert!(failed.controls_accepted.is_empty());
    }

    #[test]
    fn scm_states_map_to_status() {
        assert_eq!(status_of(ServiceState::Running), ServiceStatus::Running);
        assert_eq!(status_of(ServiceState::Stopped), ServiceStatus::Stopped);
        assert_eq!(
            status_of(ServiceState::StartPending),
            ServiceStatus::Starting
        );
        assert_eq!(
            status_of(ServiceState::StopPending),
            ServiceStatus::Stopping
        );
    }

    #[test]
    fn current_user_sid_is_a_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        // What `service install` records must pass `config set`'s check.
        MeshdConfig::default()
            .set("admin.windows_user_sid", &sid)
            .unwrap();
    }

    #[test]
    fn protected_dacl_is_applied_to_a_directory() {
        // A temporary directory only: grants this test's own user too, so it can clean up.
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("data");
        std::fs::create_dir(&sub).unwrap();
        let me = current_user_sid().unwrap();
        set_protected_dacl(&sub, &format!("{DATA_DIR_SDDL}(A;OICI;FA;;;{me})")).unwrap();
        let sddl = dacl_sddl(&sub);
        assert!(sddl.starts_with("D:P"), "{sddl}");
        for ace in ["(A;OICI;FA;;;LS)", "(A;OICI;FA;;;BA)"] {
            assert!(sddl.contains(ace), "{sddl}");
        }
        // Nothing inherited from the parent.
        assert!(!sddl.contains("ID;"), "{sddl}");
        std::fs::write(sub.join("f"), b"x").unwrap();
        assert_eq!(std::fs::read(sub.join("f")).unwrap(), b"x");
    }

    /// A file's DACL as SDDL.
    fn dacl_sddl(path: &Path) -> String {
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        };
        let name = wide(&path.to_string_lossy());
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; unused out pointers are null; `sd` is valid.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(code, ERROR_SUCCESS);
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sd` is a valid descriptor; `text` is a valid out pointer.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0);
        let mut len = 0;
        // SAFETY: a NUL-terminated string from the conversion.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` u16s were just read; both buffers are LocalAlloc'ed, freed once.
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        unsafe {
            LocalFree(text.cast());
            LocalFree(sd);
        }
        s
    }
}
