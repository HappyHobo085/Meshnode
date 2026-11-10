//! systemd (spec §6.4): the binary in `/usr/local/bin/meshd`, a system user and group
//! `meshnode` (the installing `$SUDO_USER` joins the group, which opens the admin socket to
//! them), and the unit `/etc/systemd/system/meshnode.service`.
//!
//! [`unit_file`] is pure and compiled everywhere; the installer is Linux-only.

use std::path::Path;

/// The unit's text for the binary `binary` and the data directory `data`.
pub fn unit_file(binary: &Path, data: &Path) -> String {
    let exec = [
        binary.display().to_string(),
        "run".into(),
        "--data".into(),
        data.display().to_string(),
    ]
    .iter()
    .map(|a| exec_arg(a))
    .collect::<Vec<_>>()
    .join(" ");
    format!(
        "[Unit]\n\
         Description=Meshnode daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         User=meshnode\n\
         Group=meshnode\n\
         StateDirectory=meshnode\n\
         ExecStart={exec}\n\
         Restart=always\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// One `ExecStart=` word: `%` specifiers and `$` variables escaped, double-quoted when it
/// holds anything systemd would split or interpret.
fn exec_arg(arg: &str) -> String {
    let escaped = arg.replace('%', "%%").replace('$', "$$");
    let needs_quotes = escaped.is_empty()
        || escaped
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'));
    if !needs_quotes {
        return escaped;
    }
    let mut quoted = String::from("\"");
    for c in escaped.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

#[cfg(target_os = "linux")]
pub(super) use installer::{install, start, status, stop, uninstall};

#[cfg(target_os = "linux")]
mod installer {
    use std::path::Path;

    use super::super::unix::{
        INSTALLED_BINARY, install_binary, output, remove_file, run, write_file,
    };
    use super::super::{SERVICE_NAME, ServiceError, ServiceStatus, io_err};
    use super::unit_file;

    const UNIT_PATH: &str = "/etc/systemd/system/meshnode.service";
    /// The data directory systemd creates through `StateDirectory=meshnode`.
    const STATE_DIRECTORY: &str = "/var/lib/meshnode";

    fn installed() -> bool {
        Path::new(UNIT_PATH).exists()
    }

    pub fn install(data: &Path) -> Result<(), ServiceError> {
        if installed() {
            return Err(ServiceError::AlreadyInstalled);
        }
        install_binary()?;
        ensure_account()?;
        if let Ok(user) = std::env::var("SUDO_USER")
            && !user.is_empty()
            && user != "root"
        {
            run("usermod", &["-aG", SERVICE_NAME, &user])?;
        }
        if data != Path::new(STATE_DIRECTORY) {
            // systemd only prepares /var/lib/meshnode; another directory is made here.
            std::fs::create_dir_all(data)
                .map_err(io_err(format!("creating {}", data.display())))?;
            let owner = format!("{SERVICE_NAME}:{SERVICE_NAME}");
            run("chown", &[&owner, &data.display().to_string()])?;
        }
        write_file(
            Path::new(UNIT_PATH),
            &unit_file(Path::new(INSTALLED_BINARY), data),
        )?;
        run("systemctl", &["daemon-reload"])?;
        run("systemctl", &["enable", "--now", SERVICE_NAME])
    }

    /// The system user `meshnode` with its own group (reusing a group of that name).
    fn ensure_account() -> Result<(), ServiceError> {
        if output("getent", &["passwd", SERVICE_NAME])?
            .status
            .success()
        {
            return Ok(());
        }
        let mut args = vec![
            "--system",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
        ];
        if output("getent", &["group", SERVICE_NAME])?.status.success() {
            args.extend(["-g", SERVICE_NAME]);
        } else {
            args.push("--user-group");
        }
        args.push(SERVICE_NAME);
        run("useradd", &args)
    }

    pub fn uninstall() -> Result<(), ServiceError> {
        if installed() {
            run("systemctl", &["disable", "--now", SERVICE_NAME])?;
            remove_file(Path::new(UNIT_PATH))?;
            run("systemctl", &["daemon-reload"])?;
        }
        remove_file(Path::new(INSTALLED_BINARY))
    }

    pub fn start() -> Result<(), ServiceError> {
        if !installed() {
            return Err(ServiceError::NotInstalled);
        }
        run("systemctl", &["start", SERVICE_NAME])
    }

    pub fn stop() -> Result<(), ServiceError> {
        if !installed() {
            return Err(ServiceError::NotInstalled);
        }
        run("systemctl", &["stop", SERVICE_NAME])
    }

    pub fn status() -> Result<ServiceStatus, ServiceError> {
        if !installed() {
            return Ok(ServiceStatus::NotInstalled);
        }
        // `is-active` exits non-zero for every state but active; its output names it.
        let out = output("systemctl", &["is-active", SERVICE_NAME])?;
        Ok(match String::from_utf8_lossy(&out.stdout).trim() {
            "active" | "reloading" => ServiceStatus::Running,
            "activating" => ServiceStatus::Starting,
            "deactivating" => ServiceStatus::Stopping,
            _ => ServiceStatus::Stopped,
        })
    }
}
