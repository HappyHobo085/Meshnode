// The shared command names (`COMMANDS`), read from mesh-ui-tauri's source so the build
// script needs no Tauri dependency of its own.
include!("../../../crates/mesh-ui-tauri/src/commands_list.rs");

/// This app's own commands, besides the shared ones.
const DESKTOP_COMMANDS: &[&str] = &[
    "deploy_static_dir",
    "connection",
    "locate_meshd",
    "set_meshd_path",
    "daemon_run_user",
    "daemon_install",
    "pick_folder",
    "pick_file",
    "open_url",
];

fn main() {
    println!("cargo:rerun-if-changed=../../../crates/mesh-ui-tauri/src/commands_list.rs");
    // Every command gets `allow-*` / `deny-*` permissions; capabilities/default.json
    // grants exactly these.
    let commands: Vec<&'static str> = COMMANDS.iter().chain(DESKTOP_COMMANDS).copied().collect();
    let commands: &'static [&'static str] = Box::leak(commands.into_boxed_slice());
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(commands)),
    )
    .expect("failed to run tauri-build");
}
