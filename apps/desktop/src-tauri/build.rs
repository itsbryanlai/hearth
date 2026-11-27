fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "app_version",
            "list_agents",
            "list_projects",
            "add_project",
            "list_threads",
            "worktree_info",
            "create_thread",
            "load_thread",
            "list_thread_models",
            "set_thread_model",
            "send_message",
            "answer_approval",
            "cancel_turn",
        ]),
    ))
    .expect("failed to run tauri-build");
}
