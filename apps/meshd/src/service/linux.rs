//! systemd (spec §6.4): the binary in `/usr/local/bin/meshd`, a system user and group
//! `meshnode` (the installing `$SUDO_USER` joins the group, which opens the admin socket to
//! them), and the unit `/etc/systemd/system/meshnode.service`.
//!
//! [`unit_file`] is pure and compiled everywhere; the installer is Linux-only.

use std::path::{Path, PathBuf};

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
         StateDirectoryMode=0750\n\
         UMask=0027\n\
         ExecStart={exec}\n\
         Restart=always\n\
         RestartPreventExitStatus=4\n\
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

/// The data directory in a unit's `ExecStart=` (`… --data <dir>`), as [`unit_file`] wrote
/// it; `None` when there is none.
pub fn data_dir_from_unit(unit: &str) -> Option<PathBuf> {
    let exec = unit
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let words = exec_words(exec)?;
    let i = words.iter().position(|w| w == "--data")?;
    words.get(i + 1).map(PathBuf::from)
}

/// Splits an `ExecStart=` value into words, undoing [`exec_arg`]: double or single quotes
/// with backslash escapes, then `%%` and `$$`. `None` on an unterminated quote.
fn exec_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else { break };
        let mut word = String::new();
        if first == '"' || first == '\'' {
            chars.next();
            loop {
                match chars.next()? {
                    '\\' => word.push(chars.next()?),
                    c if c == first => break,
                    c => word.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                word.push(c);
            }
        }
        words.push(word.replace("%%", "%").replace("$$", "$"));
    }
    Some(words)
}

/// A POSIX portable user name (`[a-z_][a-z0-9_-]*[$]?`, at most 32 bytes): what
/// `$SUDO_USER` must be before it is passed to `usermod`.
pub fn valid_user_name(name: &str) -> bool {
    let body = name.strip_suffix('$').unwrap_or(name);
    let mut bytes = body.bytes();
    name.len() <= 32
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(target_os = "linux")]
pub(super) use installer::{install, installed, start, status, stop, uninstall};

#[cfg(target_os = "linux")]
mod installer {
    use std::path::Path;

    use super::super::unix::{
        INSTALLED_BINARY, install_binary, output, read_registration, remove_file, run, write_file,
    };
    use super::super::{Installed, SERVICE_NAME, ServiceError, ServiceStatus, io_err};
    use super::{data_dir_from_unit, unit_file, valid_user_name};

    const UNIT_PATH: &str = "/etc/systemd/system/meshnode.service";
    /// The data directory systemd creates through `StateDirectory=meshnode`.
    const STATE_DIRECTORY: &str = "/var/lib/meshnode";

    fn is_installed() -> bool {
        Path::new(UNIT_PATH).exists()
    }

    pub fn installed() -> Result<Installed, ServiceError> {
        let unit = read_registration(Path::new(UNIT_PATH))?;
        Ok(Installed::from_registration(
            unit.as_deref(),
            data_dir_from_unit,
        ))
    }

    pub fn install(data: &Path) -> Result<(), ServiceError> {
        if is_installed() {
            return Err(ServiceError::AlreadyInstalled);
        }
        // Checked before anything changes; it is passed to usermod.
        let sudo_user = match std::env::var("SUDO_USER") {
            Ok(user) if user.is_empty() || user == "root" => None,
            Ok(user) if valid_user_name(&user) => Some(user),
            Ok(user) => {
                return Err(ServiceError::Failed(format!(
                    "SUDO_USER {user:?} is not a valid user name; nothing was installed"
                )));
            }
            Err(_) => None,
        };
        install_binary()?;
        ensure_account()?;
        if let Some(user) = sudo_user {
            run("usermod", &["-aG", SERVICE_NAME, "--", &user])?;
        }
        if data != Path::new(STATE_DIRECTORY) {
            // systemd only prepares /var/lib/meshnode; another directory is made here.
            std::fs::create_dir_all(data)
                .map_err(io_err(format!("creating {}", data.display())))?;
            let owner = format!("{SERVICE_NAME}:{SERVICE_NAME}");
            run("chown", &["-R", "--", &owner, &data.display().to_string()])?;
            // Private to the service account and its group, like StateDirectoryMode in the unit.
            run("chmod", &["0750", "--", &data.display().to_string()])?;
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
        if !is_installed() {
            return Err(ServiceError::NotInstalled);
        }
        run("systemctl", &["disable", "--now", SERVICE_NAME])?;
        remove_file(Path::new(UNIT_PATH))?;
        run("systemctl", &["daemon-reload"])?;
        remove_file(Path::new(INSTALLED_BINARY))
    }

    pub fn start() -> Result<(), ServiceError> {
        if !is_installed() {
            return Err(ServiceError::NotInstalled);
        }
        run("systemctl", &["start", SERVICE_NAME])
    }

    pub fn stop() -> Result<(), ServiceError> {
        if !is_installed() {
            return Err(ServiceError::NotInstalled);
        }
        run("systemctl", &["stop", SERVICE_NAME])
    }

    pub fn status() -> Result<ServiceStatus, ServiceError> {
        if !is_installed() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_prevents_restart_on_incompatible() {
        let unit = unit_file(
            Path::new("/usr/local/bin/meshd"),
            Path::new("/var/lib/meshnode"),
        );
        let service = unit
            .split("[Service]\n")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("a [Service] section");
        assert!(service.contains("RestartPreventExitStatus=4\n"), "{unit}");
    }

    #[test]
    fn data_dir_is_read_back_from_the_unit() {
        for data in [
            "/var/lib/meshnode",
            "/srv/mesh node/100%",
            "/srv/a\"b\\c$HOME",
            "/srv/it's",
        ] {
            let unit = unit_file(Path::new("/usr/local/bin/meshd"), Path::new(data));
            assert_eq!(
                data_dir_from_unit(&unit),
                Some(PathBuf::from(data)),
                "{unit}"
            );
        }
        assert_eq!(data_dir_from_unit("[Service]\nExecStart=/bin/true\n"), None);
        assert_eq!(
            data_dir_from_unit("ExecStart=/x run --data \"/unterminated"),
            None
        );
        assert_eq!(data_dir_from_unit("[Unit]\n"), None);
    }

    #[test]
    fn sudo_user_names_are_checked() {
        for ok in ["alice", "_svc", "bob-2", "machine$", "a_b-c9"] {
            assert!(valid_user_name(ok), "{ok}");
        }
        let long = "a".repeat(33);
        for bad in [
            "",
            "-rf",
            "Alice",
            "9lives",
            "a b",
            "a;b",
            "--",
            "x$y",
            long.as_str(),
        ] {
            assert!(!valid_user_name(bad), "{bad}");
        }
    }
}
