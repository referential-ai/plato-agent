fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["bootstrap", "pick_workspace", "read_run"]),
    ))
    .expect("failed to build Plato desktop shell");
}
