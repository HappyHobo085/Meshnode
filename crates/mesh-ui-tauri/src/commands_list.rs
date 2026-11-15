// The shared commands every app registers. Also `include!`d by the apps' build.rs files
// (which list each command in the app manifest), so it holds nothing but this constant.

/// The shared Tauri commands, by name. `deploy_static_dir` is not among them: only the
/// desktop app registers it (Android deploys from a document tree instead).
pub const COMMANDS: &[&str] = &[
    "status",
    "members",
    "services",
    "init",
    "join",
    "invite",
    "remove",
    "set_info",
    "set_tags_of",
    "deploy_wasm_file",
    "undeploy",
    "purge",
    "set_placement",
    "set_outbound",
    "set_limits",
    "config_get",
    "config_set",
    "reset_identity",
    "declare_fresh",
    "platform",
    "logs_follow",
    "probe",
    "cancel_stream",
];
