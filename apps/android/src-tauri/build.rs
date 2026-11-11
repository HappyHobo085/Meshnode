// The shared command names (`COMMANDS`), read from mesh-ui-tauri's source so the build
// script needs no Tauri dependency of its own.
include!("../../../crates/mesh-ui-tauri/src/commands_list.rs");

/// This app's own commands, besides the shared ones.
const ANDROID_COMMANDS: &[&str] = &[
    "node_running",
    "service_start",
    "service_stop",
    "power_policy_get",
    "power_policy_set",
    "pick_tree",
    "pick_wasm",
    "deploy_static_tree",
];

fn main() {
    println!("cargo:rerun-if-changed=../../../crates/mesh-ui-tauri/src/commands_list.rs");
    // Every command gets `allow-*` / `deny-*` permissions; capabilities/default.json
    // grants exactly these (and the barcode scanner's scan permissions).
    let commands: Vec<&'static str> = COMMANDS.iter().chain(ANDROID_COMMANDS).copied().collect();
    let commands: &'static [&'static str] = Box::leak(commands.into_boxed_slice());
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(commands)),
    )
    .expect("failed to run tauri-build");
}
