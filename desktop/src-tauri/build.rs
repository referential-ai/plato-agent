fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "bootstrap",
            "pick_workspace",
            "read_run",
            "list_sessions",
            "read_session",
            "submit_message",
            "poll_run",
            "recover_run",
            "decide_approval",
            "cancel_run",
        ]),
    ))
    .expect("failed to build Plato desktop shell");
}
