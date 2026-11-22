//! Test-only (`test-hooks`): builds the guest fixtures in `tests/fixtures` (a separate cargo workspace) for
//! `wasm32-wasip2` and hands out their bytes.
//!
//! The build runs once per test process into its own target dir under the workspace
//! target (`fixtures`), so it never waits on the outer cargo's target-dir lock.
//! An exclusive file lock serializes concurrent test binaries; whoever comes second finds
//! the build up to date.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The bytes of fixture `<name>.wasm`.
pub fn fixture(name: &str) -> Vec<u8> {
    let path = fixtures_dir().join(format!("{name}.wasm"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

fn fixtures_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(build_fixtures)
}

fn build_fixtures() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/Cargo.toml");
    build_wasm(&manifest, "fixtures")
}

/// Builds the workspace at `manifest` for `wasm32-wasip2` (release) into
/// `<workspace target>/<subdir>`; returns the directory holding
/// the `.wasm` files.
fn build_wasm(manifest: &Path, subdir: &str) -> PathBuf {
    let target = workspace_target_dir().join(subdir);
    std::fs::create_dir_all(&target).expect("create wasm target dir");
    let lock = File::create(target.join("build.lock")).expect("create wasm build lock");
    lock.lock().expect("lock wasm build");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "build",
            // The committed lock file, never a re-resolve: a test run must not rewrite it.
            "--locked",
            "--release",
            "--target",
            "wasm32-wasip2",
            "--manifest-path",
        ])
        .arg(manifest)
        .arg("--target-dir")
        .arg(&target)
        // Host-only settings from the outer build must not leak into the wasm build.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .output()
        .expect("run cargo to build wasm");
    assert!(
        out.status.success(),
        "building {} failed (is the wasm32-wasip2 target installed?):\n{}",
        manifest.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    drop(lock);
    target.join("wasm32-wasip2").join("release")
}

/// `<target>` from the test binary's path `<target>/<profile>/deps/<binary>`.
fn workspace_target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current test binary path");
    exe.ancestors()
        .nth(3)
        .expect("test binary lives in <target>/<profile>/deps")
        .to_path_buf()
}
