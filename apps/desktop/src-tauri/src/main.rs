use hearth_engine::agents::qwen::QwenLauncher;
use hearth_engine::{
    Engine, EngineConfig, EventSink, Project, ThreadEventPayload, ThreadSnapshot, ThreadSummary,
};
use hearth_protocol::{AgentId, ApprovalDecision, ApprovalId, ProjectId, ThreadId, UserInput};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};

/// The channel the UI listens on (`THREAD_EVENT_CHANNEL` in `ipc/contract.ts`).
const THREAD_EVENT_CHANNEL: &str = "thread-event";

struct TauriSink(tauri::AppHandle);

impl EventSink for TauriSink {
    fn emit(&self, payload: ThreadEventPayload) {
        let _ = self.0.emit(THREAD_EVENT_CHANNEL, payload);
    }
}

fn show<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
fn list_projects(engine: State<'_, Engine>) -> Result<Vec<Project>, String> {
    engine.list_projects().map_err(show)
}

#[tauri::command]
fn add_project(engine: State<'_, Engine>, path: String) -> Result<Project, String> {
    engine.add_project(&path).map_err(show)
}

#[tauri::command]
fn list_threads(
    engine: State<'_, Engine>,
    project_id: ProjectId,
) -> Result<Vec<ThreadSummary>, String> {
    engine.list_threads(&project_id).map_err(show)
}

#[tauri::command]
fn create_thread(
    engine: State<'_, Engine>,
    project_id: ProjectId,
    agent: AgentId,
) -> Result<ThreadSummary, String> {
    engine.create_thread(&project_id, agent).map_err(show)
}

#[tauri::command]
fn load_thread(engine: State<'_, Engine>, thread_id: ThreadId) -> Result<ThreadSnapshot, String> {
    engine.load_thread(&thread_id).map_err(show)
}

#[tauri::command]
async fn send_message(
    engine: State<'_, Engine>,
    thread_id: ThreadId,
    input: UserInput,
) -> Result<(), String> {
    engine.send_message(&thread_id, input).await.map_err(show)
}

#[tauri::command]
async fn answer_approval(
    engine: State<'_, Engine>,
    thread_id: ThreadId,
    approval_id: ApprovalId,
    decision: ApprovalDecision,
) -> Result<(), String> {
    engine
        .answer_approval(&thread_id, &approval_id, decision)
        .await
        .map_err(show)
}

#[tauri::command]
async fn cancel_turn(engine: State<'_, Engine>, thread_id: ThreadId) -> Result<(), String> {
    engine.cancel_turn(&thread_id).await.map_err(show)
}

/// Builds the engine: the database in the app data dir, the user's home as the
/// guard's home, Qwen found through the login shell.
fn start_engine(app: &tauri::App) -> Result<Engine, Box<dyn std::error::Error>> {
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let home = app.path().home_dir()?;
    // Dev hook, read from Hearth's own environment (not a user setting): when
    // set, this program replaces detecting `qwen`, so a replay stub can stand in.
    let program_override = std::env::var_os("HEARTH_QWEN_PROGRAM").map(PathBuf::from);
    let shell = std::env::var_os("SHELL").map(PathBuf::from);
    let launcher = Arc::new(QwenLauncher::new(home.clone(), shell, program_override));
    let warm = launcher.clone();
    // The login shell can be slow; read its PATH and find `qwen` off the main thread.
    std::thread::spawn(move || warm.prewarm());
    Ok(Engine::new(
        EngineConfig {
            db_path: Some(data_dir.join("hearth.db")),
            home,
            roots: vec![data_dir],
            launcher,
        },
        Arc::new(TauriSink(app.handle().clone())),
    )?)
}

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
        .setup(|app| {
            app.manage(start_engine(app)?);
            #[cfg(feature = "egress-selftest")]
            plant_egress(app);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_version,
            list_projects,
            add_project,
            list_threads,
            create_thread,
            load_thread,
            send_message,
            answer_approval,
            cancel_turn
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Hearth");
}
