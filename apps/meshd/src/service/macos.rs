//! launchd (spec §6.4, best-effort): the binary in `/usr/local/bin/meshd` and the daemon
//! `/Library/LaunchDaemons/net.meshnode.meshd.plist`, run as root in this version.
//!
//! [`plist`] is pure and compiled everywhere; the installer is macOS-only.

use std::path::Path;

/// The launchd label.
pub const LABEL: &str = "net.meshnode.meshd";

/// The property list's text for the binary `binary` and the data directory `data`.
pub fn plist(binary: &Path, data: &Path) -> String {
    let args = [
        binary.display().to_string(),
        "run".into(),
        "--data".into(),
        data.display().to_string(),
    ]
    .iter()
    .map(|a| format!("\t\t<string>{}</string>\n", xml_escape(a)))
    .collect::<String>();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {args}\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n"
    )
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(target_os = "macos")]
pub(super) use installer::{install, start, status, stop, uninstall};

#[cfg(target_os = "macos")]
mod installer {
    use std::path::Path;

    use super::super::unix::{
        INSTALLED_BINARY, install_binary, output, remove_file, run, write_file,
    };
    use super::super::{ServiceError, ServiceStatus, io_err};
    use super::{LABEL, plist};

    const PLIST_PATH: &str = "/Library/LaunchDaemons/net.meshnode.meshd.plist";

    fn installed() -> bool {
        Path::new(PLIST_PATH).exists()
    }

    fn target() -> String {
        format!("system/{LABEL}")
    }

    /// Whether launchd has the daemon loaded.
    fn loaded() -> Result<bool, ServiceError> {
        Ok(output("launchctl", &["print", &target()])?.status.success())
    }

    pub fn install(data: &Path) -> Result<(), ServiceError> {
        if installed() {
            return Err(ServiceError::AlreadyInstalled);
        }
        install_binary()?;
        std::fs::create_dir_all(data).map_err(io_err(format!("creating {}", data.display())))?;
        write_file(
            Path::new(PLIST_PATH),
            &plist(Path::new(INSTALLED_BINARY), data),
        )?;
        run("launchctl", &["bootstrap", "system", PLIST_PATH])
    }

    pub fn uninstall() -> Result<(), ServiceError> {
        if loaded()? {
            run("launchctl", &["bootout", &target()])?;
        }
        remove_file(Path::new(PLIST_PATH))?;
        remove_file(Path::new(INSTALLED_BINARY))
    }

    /// Loading the daemon starts it (`RunAtLoad`).
    pub fn start() -> Result<(), ServiceError> {
        if !installed() {
            return Err(ServiceError::NotInstalled);
        }
        if loaded()? {
            run("launchctl", &["kickstart", &target()])
        } else {
            run("launchctl", &["bootstrap", "system", PLIST_PATH])
        }
    }

    /// `KeepAlive` restarts a killed daemon, so stopping unloads it.
    pub fn stop() -> Result<(), ServiceError> {
        if !installed() {
            return Err(ServiceError::NotInstalled);
        }
        if loaded()? {
            run("launchctl", &["bootout", &target()])?;
        }
        Ok(())
    }

    pub fn status() -> Result<ServiceStatus, ServiceError> {
        if !installed() {
            return Ok(ServiceStatus::NotInstalled);
        }
        let out = output("launchctl", &["print", &target()])?;
        if !out.status.success() {
            return Ok(ServiceStatus::Stopped);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(if text.contains("state = running") {
            ServiceStatus::Running
        } else {
            ServiceStatus::Stopped
        })
    }
}
