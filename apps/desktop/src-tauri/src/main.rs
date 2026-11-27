#[tauri::command]
fn app_version() -> &'static str {
    hearth_core::version()
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_version])
        .run(tauri::generate_context!())
        .expect("failed to run Hearth");
}
