//! OS service installation (spec §6.4): the generated systemd unit and launchd plist
//! (golden files), the refusal without privileges, and — needing Administrator, so ignored —
//! a real Windows install round trip.

use std::path::Path;

use meshd::service::{self, ServiceError, linux, macos};

/// The golden file's text; git may have checked it out with CRLF line ends.
fn golden(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

#[test]
fn systemd_unit_golden() {
    let unit = linux::unit_file(
        Path::new("/usr/local/bin/meshd"),
        Path::new("/var/lib/meshnode"),
    );
    assert_eq!(unit, golden("meshnode.service"));
}

#[test]
fn systemd_unit_quotes_paths_with_spaces_and_percent() {
    let unit = linux::unit_file(
        Path::new("/usr/local/bin/meshd"),
        Path::new("/srv/mesh node/100%"),
    );
    assert!(
        unit.contains("ExecStart=/usr/local/bin/meshd run --data \"/srv/mesh node/100%%\"\n"),
        "{unit}"
    );
}

#[test]
fn launchd_plist_golden() {
    let plist = macos::plist(
        Path::new("/usr/local/bin/meshd"),
        Path::new("/Library/Application Support/meshnode"),
    );
    assert_eq!(plist, golden("net.meshnode.meshd.plist"));
}

#[test]
fn launchd_plist_escapes_xml() {
    let plist = macos::plist(Path::new("/usr/local/bin/meshd"), Path::new("/data/a&b<c>"));
    assert!(
        plist.contains("<string>/data/a&amp;b&lt;c&gt;</string>"),
        "{plist}"
    );
}

#[test]
fn install_without_privileges_refused() {
    if service::is_elevated() {
        // Never install for real from the automated suite.
        eprintln!("skipped: the test process is elevated");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let who = if cfg!(windows) {
        "Administrator"
    } else {
        "root"
    };
    let checks: [(&str, Result<(), ServiceError>); 4] = [
        ("install", service::install(data.path())),
        (
            "uninstall",
            service::uninstall(Some(data.path()), false).map(drop),
        ),
        ("start", service::start()),
        ("stop", service::stop()),
    ];
    for (action, result) in checks {
        let err = result.unwrap_err();
        if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            assert!(
                matches!(err, ServiceError::NeedsPrivileges { .. }),
                "{action}: {err:?}"
            );
            assert_eq!(err.to_string(), format!("service {action} requires {who}"));
        } else {
            assert!(
                matches!(err, ServiceError::Unsupported),
                "{action}: {err:?}"
            );
        }
    }
    // Nothing was created in the data directory.
    assert_eq!(std::fs::read_dir(data.path()).unwrap().count(), 0);
}

/// Installs the real service, checks it runs, stops and removes it. Needs an elevated
/// prompt and changes the machine (Program Files, the registry, the SCM), so it only runs
/// when asked for: `cargo test -p meshd --test service -- --ignored`.
#[cfg(windows)]
#[test]
#[ignore = "needs Administrator; installs a real Windows service"]
fn windows_install_roundtrip() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    assert!(
        service::is_elevated(),
        "run this test from an elevated prompt"
    );
    assert_eq!(
        service::status().unwrap(),
        service::ServiceStatus::NotInstalled,
        "a meshnode service is already installed; refusing to touch it"
    );
    let data = tempfile::tempdir().unwrap();
    let meshd = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_meshd"))
            .arg("--data")
            .arg(data.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "meshd {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    meshd(&["service", "install"]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while service::status().unwrap() != service::ServiceStatus::Running {
        assert!(
            Instant::now() < deadline,
            "the service did not reach Running"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let cfg = meshd::MeshdConfig::load_or_default(data.path()).unwrap();
    assert!(cfg.admin.windows_user_sid.is_some());
    assert!(meshd(&["service", "status"]).contains("running"));
    meshd(&["service", "stop"]);
    assert_eq!(service::status().unwrap(), service::ServiceStatus::Stopped);
    meshd(&["service", "uninstall", "--purge-data"]);
    assert_eq!(
        service::status().unwrap(),
        service::ServiceStatus::NotInstalled
    );
}
