fn main() {
    // The app's commands get `allow-*` / `deny-*` permissions; capabilities/default.json
    // grants exactly these four.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "status",
            "join",
            "service_start",
            "service_stop",
        ]),
    ))
    .expect("failed to run tauri-build");
}
