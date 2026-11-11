//! Finding `meshd`, starting a daemon as the user, and installing the system service.
//!
//! [`install_command`] is pure and tested; [`run_elevated`] (UAC, pkexec, osascript) is
//! only ever run by the user's own click in the app, never by tests.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use mesh_ui::{UiError, UiErrorCode};

/// The operating systems whose elevation commands differ.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    /// Linux and the other Unixes: `pkexec`.
    Linux,
    Macos,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        }
    }
}

/// The `meshd` to run: the remembered choice (Locate meshd…) if it still exists, else
/// `meshd` next to this app's executable, else the first one on `PATH`.
pub fn locate_meshd(remembered: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = remembered
        && p.is_file()
    {
        return Some(p);
    }
    let exe = format!("meshd{}", std::env::consts::EXE_SUFFIX);
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
    {
        let p = dir.join(&exe);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(&exe))
        .find(|p| p.is_file())
}

/// The data directory of a daemon run as the user: `%LOCALAPPDATA%\Meshnode`,
/// `$XDG_DATA_HOME/meshnode` (else `~/.local/share/meshnode`) or
/// `~/Library/Application Support/Meshnode`.
pub fn user_data_dir() -> PathBuf {
    let home = || {
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    match Os::current() {
        Os::Windows => std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Local"))
            .join("Meshnode"),
        Os::Macos => home()
            .join("Library")
            .join("Application Support")
            .join("Meshnode"),
        Os::Linux => std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".local").join("share"))
            .join("meshnode"),
    }
}

/// `meshd --data <data> run`: a daemon as the user.
pub fn run_user_command(meshd: &Path, data: &Path) -> (PathBuf, Vec<OsString>) {
    (
        meshd.to_path_buf(),
        vec!["--data".into(), data.into(), "run".into()],
    )
}

/// `meshd service install` (with `--data` when given), through this platform's elevation.
pub fn install_command(meshd: &Path, data: Option<&Path>) -> (PathBuf, Vec<OsString>) {
    install_command_for(Os::current(), meshd, data)
}

/// [`install_command`] for any platform: Windows runs `meshd` itself (elevated by
/// [`run_elevated`]), Linux `pkexec meshd …`, macOS an `osascript` shell script with
/// administrator privileges, its paths shell-quoted.
pub fn install_command_for(os: Os, meshd: &Path, data: Option<&Path>) -> (PathBuf, Vec<OsString>) {
    let mut meshd_args: Vec<OsString> = Vec::new();
    if let Some(d) = data {
        meshd_args.push("--data".into());
        meshd_args.push(d.into());
    }
    meshd_args.push("service".into());
    meshd_args.push("install".into());
    match os {
        Os::Windows => (meshd.to_path_buf(), meshd_args),
        Os::Linux => {
            let mut args: Vec<OsString> = vec![meshd.into()];
            args.extend(meshd_args);
            (PathBuf::from("pkexec"), args)
        }
        Os::Macos => {
            let mut words = vec![sh_quote(&meshd.to_string_lossy())];
            if let Some(d) = data {
                words.push("--data".into());
                words.push(sh_quote(&d.to_string_lossy()));
            }
            words.push("service install".into());
            let script = format!(
                "do shell script \"{}\" with administrator privileges",
                applescript_escape(&words.join(" "))
            );
            (PathBuf::from("osascript"), vec!["-e".into(), script.into()])
        }
    }
}

/// One shell word: single-quoted, each `'` written as `'\''`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The inside of an AppleScript string literal.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

/// Starts `cmd` detached from this app (it outlives it), its output appended to `log`.
/// Windows: no console, its own process group; Unix: its own session. The caller may
/// drop the returned handle; that does not stop the process.
pub fn spawn_detached(cmd: &(PathBuf, Vec<OsString>), log: &Path) -> io::Result<Child> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    let mut command = Command::new(&cmd.0);
    command
        .args(&cmd.1)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no state of this process.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn()
}

/// A Windows command line from separate arguments (the `CommandLineToArgvW` rules):
/// arguments with spaces, tabs or quotes, and empty ones, are quoted; backslashes are
/// doubled only before a quote.
pub fn windows_args(args: &[OsString]) -> String {
    let mut line = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let arg = arg.to_string_lossy();
        if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '"']) {
            line.push_str(&arg);
            continue;
        }
        line.push('"');
        let mut backslashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    line.push('"');
                    backslashes = 0;
                }
                c => {
                    line.extend(std::iter::repeat_n('\\', backslashes));
                    line.push(c);
                    backslashes = 0;
                }
            }
        }
        line.extend(std::iter::repeat_n('\\', backslashes * 2));
        line.push('"');
    }
    line
}

fn cancelled() -> UiError {
    UiError::new(UiErrorCode::Cancelled, "Installation cancelled")
}

/// Runs `program args` with administrator rights and waits for it: UAC (`ShellExecuteExW`,
/// verb `runas`) on Windows; elsewhere `program` is already the elevating command from
/// [`install_command`]. A declined prompt is `Cancelled`.
///
/// Never called by tests: it changes the machine.
#[cfg(windows)]
pub fn run_elevated(program: &Path, args: &[OsString]) -> Result<(), UiError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let verb = wide("runas".as_ref());
    let file = wide(program.as_os_str());
    let line = windows_args(args);
    let params = wide(line.as_ref());
    // SAFETY: SHELLEXECUTEINFOW is plain data; all-zero is its empty state.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: `info` is initialised and its strings outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        if code == ERROR_CANCELLED {
            return Err(cancelled());
        }
        return Err(UiError::new(
            UiErrorCode::Io,
            format!(
                "could not start {}: {}",
                program.display(),
                io::Error::from_raw_os_error(code as i32)
            ),
        ));
    }
    let process = info.hProcess;
    if process.is_null() {
        return Ok(());
    }
    let mut exit = 0u32;
    // SAFETY: `process` is a process handle we own (SEE_MASK_NOCLOSEPROCESS), closed once.
    let got = unsafe {
        WaitForSingleObject(process, INFINITE);
        let got = GetExitCodeProcess(process, &mut exit);
        CloseHandle(process);
        got
    };
    if got == 0 {
        return Err(UiError::internal(
            "could not read the installer's exit code",
        ));
    }
    match exit {
        0 => Ok(()),
        code => Err(UiError::internal(format!(
            "meshd service install failed (exit code {code})"
        ))),
    }
}

/// See the Windows version: here `program` is `pkexec` or `osascript`.
#[cfg(not(windows))]
pub fn run_elevated(program: &Path, args: &[OsString]) -> Result<(), UiError> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            UiError::new(
                UiErrorCode::Io,
                format!("could not start {}: {e}", program.display()),
            )
        })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let declined = match program.file_name().and_then(|n| n.to_str()) {
        // 126: the authentication dialog was dismissed.
        Some("pkexec") => out.status.code() == Some(126),
        // -128: "User canceled."
        Some("osascript") => stderr.contains("-128"),
        _ => false,
    };
    if declined {
        return Err(cancelled());
    }
    Err(UiError::internal(format!(
        "meshd service install failed ({}): {}",
        out.status,
        stderr.trim()
    )))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn install_command_per_platform() {
        let meshd = Path::new("/opt/My App's/meshd");
        assert_eq!(
            install_command_for(Os::Windows, meshd, None),
            (meshd.to_path_buf(), os(&["service", "install"]))
        );
        assert_eq!(
            install_command_for(Os::Linux, meshd, None),
            (
                PathBuf::from("pkexec"),
                os(&["/opt/My App's/meshd", "service", "install"])
            )
        );
        // Shell-quoted ('\'' for the quote), then escaped for the AppleScript string.
        assert_eq!(
            install_command_for(Os::Macos, meshd, None),
            (
                PathBuf::from("osascript"),
                os(&[
                    "-e",
                    r#"do shell script "'/opt/My App'\\''s/meshd' service install" with administrator privileges"#
                ])
            )
        );
        let plain = Path::new("/usr/local/bin/meshd");
        assert_eq!(
            install_command_for(Os::Macos, plain, None).1,
            os(&[
                "-e",
                r#"do shell script "'/usr/local/bin/meshd' service install" with administrator privileges"#
            ])
        );
        // An explicit data directory goes before the subcommand.
        let data = Path::new("/srv/mesh data");
        assert_eq!(
            install_command_for(Os::Windows, meshd, Some(data)).1,
            os(&["--data", "/srv/mesh data", "service", "install"])
        );
        assert_eq!(
            install_command_for(Os::Linux, meshd, Some(data)).1,
            os(&[
                "/opt/My App's/meshd",
                "--data",
                "/srv/mesh data",
                "service",
                "install"
            ])
        );
        assert_eq!(
            install_command_for(Os::Macos, meshd, Some(data)).1,
            os(&[
                "-e",
                r#"do shell script "'/opt/My App'\\''s/meshd' --data '/srv/mesh data' service install" with administrator privileges"#
            ])
        );
        // This platform's command is one of those.
        let here = install_command(meshd, None);
        assert!(here == install_command_for(Os::current(), meshd, None));
    }

    #[test]
    fn run_user_command_runs_the_daemon_on_the_dir() {
        let (program, args) = run_user_command(Path::new("meshd"), Path::new("/d"));
        assert_eq!(program, PathBuf::from("meshd"));
        assert_eq!(args, os(&["--data", "/d", "run"]));
    }

    #[test]
    fn windows_command_line_quoting() {
        assert_eq!(
            windows_args(&os(&["service", "install"])),
            "service install"
        );
        assert_eq!(
            windows_args(&os(&[r"C:\My Data\", "a\"b", ""])),
            r#""C:\My Data\\" "a\"b" """#
        );
    }
}
