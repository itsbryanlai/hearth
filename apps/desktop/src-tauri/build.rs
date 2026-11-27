fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "app_version",
            "list_projects",
            "add_project",
            "list_threads",
            "create_thread",
            "load_thread",
            "send_message",
            "answer_approval",
            "cancel_turn",
        ]),
    ))
    .expect("failed to run tauri-build");
}
