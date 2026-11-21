//! The Windows service (spec §6.4): installed with the service control manager (SCM) as
//! `meshnode`, running `%ProgramFiles%\Meshnode\meshd.exe run --service --data <data>` as
//! `NT AUTHORITY\LocalService`; and the service side of `meshd run --service`, which enters
//! the service dispatcher, reports its state to the SCM and maps a service stop to the
//! daemon's orderly shutdown.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
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
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ProgressInvokeNever, SDDL_REVISION_1, SE_FILE_OBJECT, TREE_SEC_INFO_RESET,
    TreeSetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    GetTokenInformation, LookupAccountNameW, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE, SidTypeUser,
    TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows_sys::core::GUID;

use super::eventlog::{self, Event, EventLog};
use super::{
    Installed, SERVICE_NAME, ServiceError, ServiceExit, ServiceStatus, apply_admin_sid, io_err,
};
use crate::{Daemon, DaemonError, MeshdConfig, logging};

/// The service's display name.
pub const DISPLAY_NAME: &str = "Meshnode";
/// The account the service runs as.
pub const ACCOUNT: &str = r"NT AUTHORITY\LocalService";
/// The data directory's owner (Administrators) and DACL: full control for LocalService and
/// Administrators only, not inherited from `%ProgramData%`.
pub const DATA_DIR_SDDL: &str = "O:BAD:P(A;OICI;FA;;;LS)(A;OICI;FA;;;BA)";
/// Restarts after a failure, and the delay before each.
pub const RESTARTS: usize = 3;
pub const RESTART_DELAY: Duration = Duration::from_secs(5);
/// When the failure count goes back to zero.
const FAILURE_RESET: Duration = Duration::from_secs(24 * 3600);
/// How long the SCM should wait for a start (opening the node) and for a stop (the
/// daemon's shutdown takes at most 20 s).
const START_WAIT_HINT: Duration = Duration::from_secs(15);
/// While the daemon starts, the start checkpoint advances this often.
const START_CHECKPOINT_EVERY: Duration = Duration::from_secs(5);
pub const STOP_WAIT_HINT: Duration = Duration::from_secs(25);
/// How long `stop` and `uninstall` wait for the service to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);

// --- configuration (pure) ----------------------------------------------------------------

/// `<Program Files>\Meshnode`, where the binary is installed (from the known folder, not
/// the environment).
pub fn install_dir() -> io::Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles)?.join("Meshnode"))
}

/// `<ProgramData>\meshnode`, the default data directory (from the known folder).
pub fn default_data_dir() -> io::Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramData)?.join("meshnode"))
}

fn known_folder(id: &GUID) -> io::Result<PathBuf> {
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: `id` is a valid GUID; a null token is the current user; `path` is a valid
    // out pointer.
    let hr = unsafe {
        SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut path)
    };
    let result = if hr < 0 {
        Err(io::Error::from_raw_os_error(hr))
    } else {
        let mut len = 0;
        // SAFETY: on success `path` is a NUL-terminated string.
        while unsafe { *path.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` u16s were just read.
        let wide = unsafe { std::slice::from_raw_parts(path, len) };
        Ok(PathBuf::from(OsString::from_wide(wide)))
    };
    // SAFETY: allocated with CoTaskMemAlloc; freed once, whether or not the call succeeded.
    unsafe { CoTaskMemFree(path.cast()) };
    result
}

/// The data directory in the service's command line (`… --data <dir>`, quoted as the SCM
/// stores it); `None` when there is none.
pub fn data_dir_from_command_line(command_line: &str) -> Option<PathBuf> {
    let args = split_command_line(command_line);
    let i = args.iter().position(|a| a == "--data")?;
    args.get(i + 1).map(PathBuf::from)
}

/// Splits a command line by the Microsoft C runtime's rules: whitespace separates
/// arguments outside double quotes; `2n` backslashes before a quote are `n` backslashes
/// and the quote toggles quoting, `2n + 1` are `n` backslashes and a literal quote;
/// other backslashes are literal.
fn split_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            match c {
                '\\' => {
                    let mut n = 0;
                    while chars.next_if_eq(&'\\').is_some() {
                        n += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        arg.extend(std::iter::repeat_n('\\', n / 2));
                        if n % 2 == 1 {
                            arg.push('"');
                            chars.next();
                        }
                    } else {
                        arg.extend(std::iter::repeat_n('\\', n));
                    }
                }
                '"' => {
                    chars.next();
                    quoted = !quoted;
                }
                c if c.is_whitespace() && !quoted => break,
                c => {
                    arg.push(c);
                    chars.next();
                }
            }
        }
        args.push(arg);
    }
    args
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

pub fn install(data: &Path, admin_user: Option<&str>) -> Result<(), ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    if open(&scm, ServiceAccess::QUERY_STATUS)?.is_some() {
        return Err(ServiceError::AlreadyInstalled);
    }
    let exe = install_binary()?;
    protect_data_dir(data)?;
    // The admin pipe also admits the installing user (spec §5.2): the account the MSI names,
    // else the one this process runs as. An SID already recorded (an upgrade) stays.
    let mut cfg = MeshdConfig::load_or_default(data)?;
    let recorded = match admin_user {
        Some(account) => apply_admin_sid(&mut cfg, || {
            account_sid(account).map_err(|_| {
                ServiceError::Failed(format!("cannot resolve the installing user {account}"))
            })
        })
        .map(drop),
        None => apply_admin_sid(&mut cfg, || {
            current_user_sid().map_err(io_err("reading the installing user's SID"))
        })
        .map(drop),
    };
    // Only an account SID is accepted; anything else is skipped, so the pipe stays closed
    // to the installing user (the CLI then needs an elevated shell). An account the MSI
    // named that cannot be resolved fails the install instead.
    match (recorded, admin_user) {
        (Ok(()), _) => {}
        (Err(e), Some(_)) => return Err(e),
        (Err(e), None) => eprintln!("warning: the installing user is not recorded: {e}"),
    }
    cfg.save(data)?;
    eventlog::register().map_err(io_err("registering the Meshnode Event Log source"))?;

    let service = scm
        .create_service(
            &service_info(&exe, data),
            ServiceAccess::CHANGE_CONFIG
                | ServiceAccess::START
                | ServiceAccess::QUERY_STATUS
                | ServiceAccess::DELETE,
        )
        .map_err(scm_err("creating the meshnode service"))?;
    let configured = configure_and_start(&service);
    if configured.is_err() {
        // Best-effort: leave no half-configured service behind.
        let _ = service.delete();
    }
    configured
}

fn configure_and_start(service: &Service) -> Result<(), ServiceError> {
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
    // Incompatible data reports NO_ERROR instead, which this flag leaves alone
    // (`super::service_exit`).
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(scm_err("setting the restart-on-failure actions"))?;
    service
        .start::<&OsStr>(&[])
        .map_err(scm_err("starting the meshnode service"))
}

fn program_files_dir() -> Result<PathBuf, ServiceError> {
    install_dir().map_err(io_err("finding the Program Files folder"))
}

/// Copies the running executable to `<Program Files>\Meshnode\meshd.exe`, unless it is
/// that file, and returns the installed path.
fn install_binary() -> Result<PathBuf, ServiceError> {
    let dir = program_files_dir()?;
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

/// Prepares the data directory with [`DATA_DIR_SDDL`].
fn protect_data_dir(data: &Path) -> Result<(), ServiceError> {
    prepare_data_dir(data, DATA_DIR_SDDL)
}

/// Creates the data directory if needed and secures it before anything is written there:
/// - a junction or symbolic link at its path is refused (it could point the service, and
///   the DACL change, at another directory);
/// - its owner and protected DACL are set from `sddl`, so `%ProgramData%`'s inherited
///   entries are dropped;
/// - everything already inside is reset to the owner and to only the directory's
///   inherited entries (`TreeSetNamedSecurityInfoW` with `TREE_SEC_INFO_RESET`), so explicit
///   entries placed there beforehand are removed.
fn prepare_data_dir(data: &Path, sddl: &str) -> Result<(), ServiceError> {
    refuse_link(data)?;
    std::fs::create_dir_all(data).map_err(io_err(format!("creating {}", data.display())))?;
    refuse_link(data)?;
    set_tree_security(data, sddl).map_err(io_err(format!(
        "setting the permissions of {}",
        data.display()
    )))
}

/// Refuses a reparse point (junction, symbolic link) or a non-directory at `path`; a
/// missing path is fine.
fn refuse_link(path: &Path) -> Result<(), ServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
            Err(ServiceError::Failed(format!(
                "{} is a junction or symbolic link; refusing to use it as the data directory",
                path.display()
            )))
        }
        Ok(m) if !m.is_dir() => Err(ServiceError::Failed(format!(
            "{} is not a directory",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(format!("inspecting {}", path.display()))(e)),
    }
}

/// Sets the owner and the protected DACL of `sddl` on `path` and resets everything inside.
fn set_tree_security(path: &Path, sddl: &str) -> io::Result<()> {
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
    let mut owner: PSID = std::ptr::null_mut();
    // SAFETY: as above.
    let ok = ok != 0 && unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) } != 0;
    let result = if !ok {
        Err(io::Error::last_os_error())
    } else if owner.is_null() || dacl.is_null() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the security descriptor needs an owner and a DACL",
        ))
    } else {
        let name = wide(&path.as_os_str().to_string_lossy());
        // SAFETY: `name` is NUL-terminated; `owner` and `dacl` point into `sd`, alive until
        // freed below; no progress callback.
        let code = unsafe {
            TreeSetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                owner,
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
                TREE_SEC_INFO_RESET,
                None,
                ProgressInvokeNever,
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

/// What is installed, read back from the SCM's command line for the service.
pub fn installed() -> Result<Installed, ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT)?;
    let command_line = match open(&scm, ServiceAccess::QUERY_CONFIG)? {
        None => None,
        Some(service) => Some(
            service
                .query_config()
                .map_err(scm_err("reading the meshnode service's configuration"))?
                .executable_path
                .to_string_lossy()
                .into_owned(),
        ),
    };
    Ok(Installed::from_registration(
        command_line.as_deref(),
        data_dir_from_command_line,
    ))
}

pub fn uninstall() -> Result<(), ServiceError> {
    let scm = manager(ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE;
    let service = open(&scm, access)?.ok_or(ServiceError::NotInstalled)?;
    stop_and_wait(&service)?;
    service
        .delete()
        .map_err(scm_err("deleting the meshnode service"))?;
    eventlog::unregister().map_err(io_err("removing the Meshnode Event Log source"))?;
    remove_binary()
}

/// Removes the installed binary and its directory — unless this process runs from it
/// (Windows cannot delete a running executable); it is then left in place.
fn remove_binary() -> Result<(), ServiceError> {
    let dir = program_files_dir()?;
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
    // A stop request is refused (ERROR_SERVICE_CANNOT_ACCEPT_CTRL) while the service is still
    // starting, e.g. when an MSI rollback stops the service its upgrade just started: wait
    // for the start to end first.
    let deadline = Instant::now() + STOP_TIMEOUT;
    while state == ServiceState::StartPending {
        if Instant::now() >= deadline {
            return Err(ServiceError::Failed(format!(
                "the meshnode service did not finish starting within {} s",
                STOP_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(POLL);
        state = query()?;
    }
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
/// shutdown; pending states carry a wait hint and `checkpoint` (at least 1; it must grow
/// while a long start goes on). Other states report checkpoint 0.
pub fn reported_status(
    state: ServiceState,
    exit_code: ServiceExitCode,
    checkpoint: u32,
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
        checkpoint: if wait_hint.is_zero() {
            0
        } else {
            checkpoint.max(1)
        },
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
    let set_pending = |state, checkpoint| {
        let _ = status.set_service_status(reported_status(
            state,
            ServiceExitCode::NO_ERROR,
            checkpoint,
        ));
    };
    let set = |state, exit| {
        let _ = status.set_service_status(reported_status(state, exit, 0));
    };
    set_pending(ServiceState::StartPending, 1);

    // A panic in the daemon's own code ends the service as a failure (the SCM restarts it).
    let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
        ctx.runtime.block_on(async {
            let cfg = MeshdConfig::load_or_default(&ctx.data).map_err(|e| {
                let e = DaemonError::from(e);
                crate::record_start_failure(&ctx.data, &e);
                e
            })?;
            let _log = logging::init(&ctx.data, &cfg.log_level);
            // Opening the node can take a while: show the SCM progress meanwhile.
            let start = Daemon::start_opts(ctx.data.clone(), cfg, crate::host_options());
            tokio::pin!(start);
            let mut ticks = tokio::time::interval(START_CHECKPOINT_EVERY);
            ticks.tick().await;
            let mut checkpoint = 1;
            let started = loop {
                tokio::select! {
                    r = &mut start => break r,
                    _ = ticks.tick() => {
                        checkpoint += 1;
                        set_pending(ServiceState::StartPending, checkpoint);
                    }
                }
            };
            let daemon = match started {
                Ok(d) => d,
                Err(e) => {
                    error!("meshd failed to start: {e}");
                    return Err(e);
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
                    set_pending(ServiceState::StopPending, 1);
                })
                .await;
            Ok::<(), DaemonError>(())
        })
    }));

    match run {
        Err(panic) => {
            let message = format!(
                "Meshnode stopped after an internal error: {}",
                panic_message(panic.as_ref())
            );
            error!("{message}");
            report(Event::Crashed, &message);
            set(ServiceState::Stopped, ServiceExitCode::ServiceSpecific(2));
        }
        Ok(Ok(())) => {
            report(Event::Stopped, "Meshnode stopped.");
            set(ServiceState::Stopped, ServiceExitCode::NO_ERROR);
        }
        Ok(Err(e)) => {
            // Incompatible data stops with NO_ERROR so the failure actions do not restart
            // it (see `service_exit`); the reason is in the Event Log and start-error.txt.
            let (exit, log_event) = super::service_exit(&e);
            if log_event {
                report(
                    Event::StartFailed,
                    &format!("Meshnode failed to start: {e}"),
                );
            }
            let exit = match exit {
                ServiceExit::NoError => ServiceExitCode::NO_ERROR,
                ServiceExit::ServiceSpecific(code) => ServiceExitCode::ServiceSpecific(code),
            };
            set(ServiceState::Stopped, exit);
        }
    }
}

/// The text of a panic's payload.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
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

/// The string SID of the user account `account` (`DOMAIN\\user`, `user` or a UPN), by
/// `LookupAccountNameW`. Only a user account resolves; a group or alias is an error.
pub fn account_sid(account: &str) -> io::Result<String> {
    let name = wide(account);
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut kind: SID_NAME_USE = 0;
    // SAFETY: a size query: null buffers with zero lengths; `name` is NUL-terminated and
    // the out pointers are valid. The local system is searched (null system name).
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    if sid_len == 0 {
        return Err(io::Error::last_os_error());
    }
    // `u64` elements keep the SID buffer aligned.
    let mut sid = vec![0u64; (sid_len as usize).div_ceil(8)];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    // SAFETY: the buffers hold the sizes the first call reported (the lengths are passed
    // in); `name` and the out pointers are valid.
    let ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if kind != SidTypeUser {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{account} is not a user account"),
        ));
    }
    sid_string(sid.as_mut_ptr().cast())
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
    fn folders_come_from_the_known_folders() {
        let install = install_dir().unwrap();
        assert!(
            install.is_absolute() && install.ends_with("Meshnode"),
            "{install:?}"
        );
        let data = default_data_dir().unwrap();
        assert!(data.is_absolute() && data.ends_with("meshnode"), "{data:?}");
    }

    #[test]
    fn data_dir_is_read_back_from_the_command_line() {
        let cases = [
            (
                r#""C:\Program Files\Meshnode\meshd.exe" run --service --data C:\ProgramData\meshnode"#,
                Some(r"C:\ProgramData\meshnode"),
            ),
            (
                r#""C:\Program Files\Meshnode\meshd.exe" run --service --data "D:\mesh data\node""#,
                Some(r"D:\mesh data\node"),
            ),
            // A trailing backslash inside quotes is doubled by the escaping.
            (
                r#"meshd.exe run --service --data "D:\with space\\""#,
                Some(r"D:\with space\"),
            ),
            (r#"meshd.exe run --data "a\"b""#, Some(r#"a"b"#)),
            (r#""C:\x\meshd.exe" run --service"#, None),
            (r#"meshd.exe run --data"#, None),
        ];
        for (line, want) in cases {
            assert_eq!(
                data_dir_from_command_line(line),
                want.map(PathBuf::from),
                "{line}"
            );
        }
    }

    #[test]
    fn panic_messages() {
        let caught = std::panic::catch_unwind(|| panic!("boom {}", 1)).unwrap_err();
        assert_eq!(panic_message(caught.as_ref()), "boom 1");
        let caught = std::panic::catch_unwind(|| std::panic::panic_any(7u8)).unwrap_err();
        assert_eq!(panic_message(caught.as_ref()), "unknown panic");
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
        let running = reported_status(ServiceState::Running, ServiceExitCode::NO_ERROR, 7);
        assert_eq!(
            running.controls_accepted,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        );
        assert_eq!((running.checkpoint, running.wait_hint), (0, Duration::ZERO));
        let stopping = reported_status(ServiceState::StopPending, ServiceExitCode::NO_ERROR, 0);
        assert!(stopping.controls_accepted.is_empty());
        // The daemon's shutdown takes at most 20 s.
        assert!(stopping.wait_hint > Duration::from_secs(20));
        assert_eq!(stopping.checkpoint, 1);
        // A long start advances its checkpoint, each step within the wait hint.
        let starting = reported_status(ServiceState::StartPending, ServiceExitCode::NO_ERROR, 3);
        assert_eq!(starting.checkpoint, 3);
        assert!(starting.wait_hint > START_CHECKPOINT_EVERY);
        let failed = reported_status(
            ServiceState::Stopped,
            ServiceExitCode::ServiceSpecific(1),
            0,
        );
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

    /// [`DATA_DIR_SDDL`]'s entries plus this test's own user (as owner too, which a
    /// non-elevated process may set), so the test can work in and clean up its temp dir.
    fn test_sddl() -> (String, String) {
        let me = current_user_sid().unwrap();
        let dacl = DATA_DIR_SDDL.strip_prefix("O:BA").unwrap();
        (format!("O:{me}{dacl}(A;OICI;FA;;;{me})"), me)
    }

    /// How SDDL writes a local account's SID: the machine's built-in Administrator (RID 500,
    /// the account CI runners use) and Guest (RID 501) print as the aliases `LA` and `LG`.
    fn sddl_token(sid: &str) -> &str {
        if sid.starts_with("S-1-5-21-") && sid.ends_with("-500") {
            "LA"
        } else if sid.starts_with("S-1-5-21-") && sid.ends_with("-501") {
            "LG"
        } else {
            sid
        }
    }

    #[test]
    fn data_dir_sddl_sets_owner_and_protected_dacl() {
        assert!(DATA_DIR_SDDL.starts_with("O:BAD:P"));
    }

    #[test]
    fn protected_dacl_is_applied_to_a_new_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("data");
        let (sddl, me) = test_sddl();
        prepare_data_dir(&sub, &sddl).unwrap();
        let got = security_sddl(&sub);
        assert!(
            got.starts_with(&format!("O:{}D:P", sddl_token(&me))),
            "{got}"
        );
        for ace in ["(A;OICI;FA;;;LS)", "(A;OICI;FA;;;BA)"] {
            assert!(got.contains(ace), "{got}");
        }
        // Nothing inherited from the parent.
        assert!(!got.contains("ID;"), "{got}");
        std::fs::write(sub.join("f"), b"x").unwrap();
        assert_eq!(std::fs::read(sub.join("f")).unwrap(), b"x");
    }

    #[test]
    fn existing_children_are_reset() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("data");
        std::fs::create_dir(&sub).unwrap();
        let child = sub.join("planted");
        std::fs::write(&child, b"x").unwrap();
        let (sddl, me) = test_sddl();
        // An explicit, protected grant to Everyone, as an attacker could have placed it.
        set_file_dacl(&child, &format!("D:P(A;;FA;;;WD)(A;;FA;;;{me})"));
        assert!(security_sddl(&child).contains("(A;;FA;;;WD)"));

        prepare_data_dir(&sub, &sddl).unwrap();
        let got = security_sddl(&child);
        assert!(got.starts_with(&format!("O:{}", sddl_token(&me))), "{got}");
        assert!(!got.contains(";;;WD)"), "{got}");
        assert!(!got.contains("D:P"), "the child inherits again: {got}");
        for ace in ["(A;ID;FA;;;LS)", "(A;ID;FA;;;BA)"] {
            assert!(got.contains(ace), "{got}");
        }
    }

    #[test]
    fn a_junction_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        // `mklink /J` needs no privilege; skip if the shell cannot make one here.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("skipped: could not create a junction");
            return;
        }
        let before = security_sddl(&target);
        let (sddl, _) = test_sddl();
        let err = prepare_data_dir(&link, &sddl).unwrap_err();
        assert!(
            err.to_string().contains("junction or symbolic link"),
            "{err}"
        );
        assert_eq!(security_sddl(&target), before, "the target was not touched");
        // A file is refused too.
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(prepare_data_dir(&file, &sddl).is_err());
    }

    /// Replaces a file's DACL (protected) without touching anything else.
    fn set_file_dacl(path: &Path, sddl: &str) {
        use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
        let w = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: as in `set_tree_security`.
        unsafe {
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    w.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                ),
                0
            );
            let (mut present, mut defaulted) = (0, 0);
            let mut dacl: *mut ACL = std::ptr::null_mut();
            assert_ne!(
                GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted),
                0
            );
            let name = wide(&path.to_string_lossy());
            let code = SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            );
            LocalFree(sd);
            assert_eq!(code, ERROR_SUCCESS);
        }
    }

    /// A file's owner and DACL as SDDL.
    fn security_sddl(path: &Path) -> String {
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
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
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
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
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
