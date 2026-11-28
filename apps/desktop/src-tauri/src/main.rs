use hearth_engine::agents::Registration;
use hearth_engine::agents::claude::ClaudeAdapter;
use hearth_engine::agents::qwen::{QwenAdapter, QwenLauncher};
use hearth_engine::{
    Engine, EngineConfig, EventSink, InstalledAgent, Project, ThreadEventPayload, ThreadModels,
    ThreadSnapshot, ThreadSummary,
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
async fn list_agents(engine: State<'_, Engine>) -> Result<Vec<InstalledAgent>, String> {
    Ok(engine.list_agents().await)
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
async fn worktree_info(
    engine: State<'_, Engine>,
    project_id: ProjectId,
) -> Result<hearth_engine::WorktreeInfo, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.worktree_info(&project_id))
        .await
        .map_err(|e| e.to_string())?
        .map_err(show)
}

#[tauri::command]
async fn create_thread(
    engine: State<'_, Engine>,
    project_id: ProjectId,
    agent: AgentId,
    base: Option<String>,
    parent: Option<String>,
) -> Result<ThreadSummary, String> {
    engine
        .create_thread_in_parent(&project_id, agent, base, parent)
        .await
        .map_err(show)
}

#[tauri::command]
fn load_thread(engine: State<'_, Engine>, thread_id: ThreadId) -> Result<ThreadSnapshot, String> {
    engine.load_thread(&thread_id).map_err(show)
}

#[tauri::command]
async fn list_thread_models(
    engine: State<'_, Engine>,
    thread_id: ThreadId,
) -> Result<ThreadModels, String> {
    engine.list_thread_models(&thread_id).await.map_err(show)
}

#[tauri::command]
async fn set_thread_model(
    engine: State<'_, Engine>,
    thread_id: ThreadId,
    model_id: String,
) -> Result<ThreadSummary, String> {
    engine
        .set_thread_model(&thread_id, &model_id)
        .await
        .map_err(show)
}

#[tauri::command]
async fn send_message(
    engine: State<'_, Engine>,
    thread_id: ThreadId,
    input: UserInput,
) -> Result<ThreadSummary, String> {
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

/// Registers the agents and opens the local database.
fn start_engine(app: &tauri::App) -> Result<Engine, Box<dyn std::error::Error>> {
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let home = app.path().home_dir()?;
    // Dev hook, read from Hearth's own environment (not a user setting): when
    // set, this program replaces detecting `qwen`, so a replay stub can stand in.
    let program_override = std::env::var_os("HEARTH_QWEN_PROGRAM").map(PathBuf::from);
    let shell = std::env::var_os("SHELL").map(PathBuf::from);
    let launcher = Arc::new(QwenLauncher::new(
        home.clone(),
        shell.clone(),
        program_override,
    ));
    let claude_override = std::env::var_os("HEARTH_CLAUDE_PROGRAM").map(PathBuf::from);
    let agents = vec![
        Registration {
            id: AgentId::QwenCode,
            label: "Qwen Code",
            adapter: Arc::new(QwenAdapter(launcher)),
        },
        Registration {
            id: AgentId::ClaudeCode,
            label: "Claude Code",
            adapter: Arc::new(ClaudeAdapter::new(home.clone(), shell, claude_override)),
        },
    ];
    Ok(Engine::new(
        EngineConfig {
            db_path: Some(data_dir.join("hearth.db")),
            home,
            roots: vec![data_dir],
            agents,
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
            list_agents,
            list_projects,
            add_project,
            list_threads,
            worktree_info,
            create_thread,
            load_thread,
            list_thread_models,
            set_thread_model,
            send_message,
            answer_approval,
            cancel_turn
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Hearth");
}
