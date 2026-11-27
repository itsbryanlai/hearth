#[tauri::command]
fn app_version() -> &'static str {
    hearth_core::version()
}

/// Test-only (feature `egress-selftest`): `HEARTH_EGRESS_SELFTEST=rust=<host:port>` opens a TCP
/// connection from this process; `=webview=<url>` navigates the webview there, so the WebKit
/// networking helper makes it. The target comes from the environment, never from the source.
#[cfg(feature = "egress-selftest")]
fn plant_egress(app: &tauri::App) {
    use tauri::Manager;
    let Ok(spec) = std::env::var("HEARTH_EGRESS_SELFTEST") else {
        return;
    };
    if let Some(addr) = spec.strip_prefix("rust=") {
        let addr = addr.to_owned();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _held = std::net::TcpStream::connect(addr);
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
    } else if let Some(url) = spec.strip_prefix("webview=") {
        let (url, window) = (url.to_owned(), app.get_webview_window("main"));
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if let (Some(window), Ok(url)) = (window, url.parse()) {
                let _ = window.navigate(url);
            }
        });
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|_app| {
            #[cfg(feature = "egress-selftest")]
            plant_egress(_app);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![app_version])
        .run(tauri::generate_context!())
        .expect("failed to run Hearth");
}
