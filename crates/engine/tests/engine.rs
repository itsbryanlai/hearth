//! The engine against the replay stub (`qwen-replay`) and a real store.

use hearth_adapter_qwen::SpawnConfig;
use hearth_engine::AgentStatus;
use hearth_engine::agents::claude::ClaudeAdapter;
use hearth_engine::agents::qwen::{AgentLauncher, QwenAdapter};
use hearth_engine::agents::{AgentAdapter, AgentFuture, OpenSession, Registration};
use hearth_engine::{Engine, EngineConfig, EventSink, ThreadEventPayload};
use hearth_protocol::{AgentEvent, AgentId, ApprovalDecision, StopReason, ThreadId, UserInput};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

/// `qwen-replay` sits in the target profile directory, some levels above the
/// test binary; build it if it is not there.
fn replay_bin(name: &str, package: &str) -> PathBuf {
    let find = || {
        let exe = std::env::current_exe().unwrap();
        exe.ancestors().map(|d| d.join(name)).find(|p| p.is_file())
    };
    if let Some(bin) = find() {
        return bin;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = std::process::Command::new(cargo)
        .args(["build", "--offline", "-p", package, "--bin", name])
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not build {name}");
    find().unwrap_or_else(|| panic!("{name} is not built"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/qwen")
        .join(name)
}

struct ReplayLauncher(PathBuf);

impl AgentLauncher for ReplayLauncher {
    fn spawn_config(&self, cwd: &Path, _passthrough: &[String]) -> Result<SpawnConfig, String> {
        let mut c =
            SpawnConfig::qwen_defaults(replay_bin("qwen-replay", "hearth-adapter-qwen"), cwd);
        c.args = vec![self.0.to_string_lossy().into_owned()];
        Ok(c)
    }
}

struct RecordingLauncher {
    fixture: PathBuf,
    cwd: Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

impl AgentLauncher for RecordingLauncher {
    fn spawn_config(&self, cwd: &Path, passthrough: &[String]) -> Result<SpawnConfig, String> {
        self.cwd.lock().unwrap().push(cwd.to_path_buf());
        ReplayLauncher(self.fixture.clone()).spawn_config(cwd, passthrough)
    }
}

struct ChannelSink(mpsc::UnboundedSender<ThreadEventPayload>);

impl EventSink for ChannelSink {
    fn emit(&self, payload: ThreadEventPayload) {
        let _ = self.0.send(payload);
    }
}

struct World {
    _tmp: tempfile::TempDir,
    db: PathBuf,
    home: PathBuf,
    project: PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (home, project) = (base.join("home"), base.join("project"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    World {
        db: base.join("hearth.db"),
        home,
        project,
        _tmp: tmp,
    }
}

fn engine(
    w: &World,
    fixture_path: PathBuf,
) -> (Engine, mpsc::UnboundedReceiver<ThreadEventPayload>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let engine = Engine::new(
        EngineConfig {
            db_path: Some(w.db.clone()),
            home: w.home.clone(),
            roots: vec![],
            agents: vec![Registration {
                id: AgentId::QwenCode,
                label: "Qwen Code",
                adapter: Arc::new(QwenAdapter(Arc::new(ReplayLauncher(fixture_path)))),
            }],
        },
        Arc::new(ChannelSink(tx)),
    )
    .unwrap();
    (engine, rx)
}

fn say(t: &str) -> UserInput {
    UserInput::Text { text: t.into() }
}

async fn thread_of(engine: &Engine, w: &World) -> ThreadId {
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    engine
        .create_thread(&project.id, AgentId::QwenCode)
        .await
        .unwrap()
        .id
}

/// Events up to and including the first one `stop` accepts.
async fn until(
    rx: &mut mpsc::UnboundedReceiver<ThreadEventPayload>,
    stop: impl Fn(&AgentEvent) -> bool,
) -> Vec<ThreadEventPayload> {
    let mut seen = Vec::new();
    loop {
        let p = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("sink closed");
        let done = stop(&p.event);
        seen.push(p);
        if done {
            return seen;
        }
    }
}

fn is_end(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::TurnEnd { .. })
}

fn is_approval(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::ApprovalRequest { .. })
}

// Fails if events are emitted out of order or with a repeated/skipped seq, are
// not persisted exactly as emitted, or the user's message is not stored.
#[tokio::test]
async fn send_streams_in_order_and_persists_what_it_emitted() {
    let w = world();
    let (engine, mut rx) = engine(&w, fixture("acp-approval-allowed.jsonl"));
    let thread = thread_of(&engine, &w).await;
    engine.send_message(&thread, say("go")).await.unwrap();
    let approval = until(&mut rx, is_approval).await;
    let AgentEvent::ApprovalRequest { request } = &approval.last().unwrap().event else {
        unreachable!()
    };
    engine
        .answer_approval(&thread, &request.id, ApprovalDecision::AllowOnce)
        .await
        .unwrap();
    let mut emitted = approval;
    emitted.extend(until(&mut rx, is_end).await);

    let seqs: Vec<u64> = emitted.iter().map(|p| p.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    let snap = engine.load_thread(&thread).unwrap();
    assert_eq!(snap.last_seq, seqs.len() as u64);
    assert_eq!(snap.turns.len(), 1);
    assert_eq!(snap.turns[0].input, say("go"));
    let stored: Vec<&AgentEvent> = snap.turns[0].events.iter().collect();
    let live: Vec<&AgentEvent> = emitted.iter().map(|p| &p.event).collect();
    assert_eq!(stored, live);
    assert_eq!(snap.thread.title, "go");
}

// Fails if "allow for this thread" does not answer the next identical request
// by itself (the turn would then wait for an answer forever).
#[tokio::test]
async fn allow_for_thread_answers_the_next_identical_request() {
    let w = world();
    let source = std::fs::read_to_string(fixture("acp-approval-allowed.jsonl")).unwrap();
    let lines: Vec<&str> = source.lines().collect();
    // initialize, session/new, then the prompt segment twice.
    let doubled: Vec<&str> = lines.iter().chain(lines[2..].iter()).copied().collect();
    let path = w.project.parent().unwrap().join("twice.jsonl");
    std::fs::write(&path, doubled.join("\n")).unwrap();
    let (engine, mut rx) = engine(&w, path);
    let thread = thread_of(&engine, &w).await;

    engine.send_message(&thread, say("one")).await.unwrap();
    let seen = until(&mut rx, is_approval).await;
    let AgentEvent::ApprovalRequest { request } = &seen.last().unwrap().event else {
        unreachable!()
    };
    engine
        .answer_approval(&thread, &request.id, ApprovalDecision::AllowForThread)
        .await
        .unwrap();
    until(&mut rx, is_end).await;

    engine.send_message(&thread, say("two")).await.unwrap();
    let second = until(&mut rx, is_end).await;
    let shown = second
        .iter()
        .find_map(|p| match &p.event {
            AgentEvent::ApprovalRequest { request } => Some(request.title.clone()),
            _ => None,
        })
        .expect("the second request is still shown");
    assert!(shown.ends_with("(allowed for this thread)"), "{shown}");
}

// Fails if cancel does not end the turn as cancelled, or the end is not stored.
#[tokio::test]
async fn cancel_ends_the_turn_cancelled_and_persists_it() {
    let w = world();
    let (engine, mut rx) = engine(&w, fixture("acp-cancel.jsonl"));
    let thread = thread_of(&engine, &w).await;
    engine.send_message(&thread, say("count")).await.unwrap();
    engine.cancel_turn(&thread).await.unwrap();
    let seen = until(&mut rx, is_end).await;
    assert!(matches!(
        seen.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        }
    ));
    let snap = engine.load_thread(&thread).unwrap();
    assert!(matches!(
        snap.turns[0].events.last(),
        Some(AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        })
    ));
}

/// A fixture of `chunks` answer chunks, then a turn end of `stop_reason`
/// (the stub waits for `session/cancel` before a `cancelled` one).
fn long_answer_fixture(w: &World, chunks: usize, stop_reason: &str) -> PathBuf {
    let base = std::fs::read_to_string(fixture("acp-cancel.jsonl")).unwrap();
    let mut out: Vec<String> = base.lines().take(2).map(str::to_owned).collect();
    let sid = "fff0a4aa-ad95-475a-98f5-729ea9c8c498";
    for i in 0..chunks {
        out.push(format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{sid}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{i}\n"}}}}}}}}"#
        ));
    }
    out.push(format!(
        r#"{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"{stop_reason}"}}}}"#
    ));
    let path = w.home.join(format!("long-{stop_reason}.jsonl"));
    std::fs::write(&path, out.join("\n")).unwrap();
    path
}

// Fails if Stop, pressed while a long answer is still being streamed and
// saved, does not end the turn as cancelled.
#[tokio::test]
async fn cancel_during_a_long_stream_ends_the_turn_cancelled() {
    let w = world();
    let (engine, mut rx) = engine(&w, long_answer_fixture(&w, 3000, "cancelled"));
    let thread = thread_of(&engine, &w).await;
    engine.send_message(&thread, say("count")).await.unwrap();
    until(&mut rx, |e| matches!(e, AgentEvent::TextDelta { .. })).await;
    engine.cancel_turn(&thread).await.unwrap();
    let seen = until(&mut rx, is_end).await;
    assert!(matches!(
        seen.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        }
    ));
}

// Fails if an agent that ignores `session/cancel` leaves the turn running
// forever instead of the engine ending it as cancelled.
#[tokio::test]
async fn a_cancelled_turn_the_agent_never_ends_is_ended_by_the_engine() {
    let w = world();
    let (engine, mut rx) = engine(&w, long_answer_fixture(&w, 5, "hang"));
    engine.set_cancel_grace(Duration::from_millis(50));
    let thread = thread_of(&engine, &w).await;
    engine.send_message(&thread, say("count")).await.unwrap();
    until(&mut rx, |e| matches!(e, AgentEvent::TextDelta { .. })).await;
    engine.cancel_turn(&thread).await.unwrap();
    let seen = until(&mut rx, is_end).await;
    assert!(matches!(
        seen.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        }
    ));
    let snap = engine.load_thread(&thread).unwrap();
    assert!(matches!(
        snap.turns[0].events.last(),
        Some(AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        })
    ));
}

// Fails if the model the agent reports for the session is not what the thread
// shows once a message was sent (a new thread starts with none).
#[tokio::test]
async fn send_returns_the_thread_with_the_agents_current_model() {
    let w = world();
    let (engine, _rx) = engine(&w, fixture("acp-cancel.jsonl"));
    let thread = thread_of(&engine, &w).await;
    let summary = engine.send_message(&thread, say("count")).await.unwrap();
    assert_eq!(summary.model, "qwen3.8-flash(openai)");
    assert_eq!(
        engine.load_thread(&thread).unwrap().thread.model,
        summary.model
    );
}

// Fails if a reopened thread loses the user's message or events, or its
// numbering restarts (the UI would then drop or duplicate live events).
#[tokio::test]
async fn reopening_the_database_restores_the_thread_and_keeps_numbering() {
    let w = world();
    let (first, mut rx) = engine(&w, fixture("acp-cancel.jsonl"));
    let thread = thread_of(&first, &w).await;
    first.send_message(&thread, say("count")).await.unwrap();
    first.cancel_turn(&thread).await.unwrap();
    let emitted = until(&mut rx, is_end).await;
    let before = first.load_thread(&thread).unwrap();
    drop(first);

    let (second, _rx) = engine(&w, fixture("acp-cancel.jsonl"));
    let after = second.load_thread(&thread).unwrap();
    assert_eq!(after, before);
    assert_eq!(after.turns[0].input, say("count"));
    assert_eq!(after.last_seq, emitted.last().unwrap().seq);
    assert_eq!(second.list_projects().unwrap().len(), 1);
    assert_eq!(
        second.list_threads(&after.thread.project_id).unwrap().len(),
        1
    );
}

// Fails if a folder under a protected home location can become a project.
#[tokio::test]
async fn add_project_refuses_a_protected_folder() {
    let w = world();
    let (engine, _rx) = engine(&w, fixture("acp-cancel.jsonl"));
    let secret = w.home.join(".ssh");
    std::fs::create_dir_all(&secret).unwrap();
    let err = engine.add_project(secret.to_str().unwrap()).unwrap_err();
    assert!(err.0.contains("protected"), "{err}");
    assert!(engine.list_projects().unwrap().is_empty());
}

// Fails if a turn cut off by quitting stays open after reopening (the UI would
// refuse new messages), or if sending again does not work.
#[tokio::test]
async fn reopening_after_a_turn_left_mid_approval_ends_it_and_allows_sending() {
    let w = world();
    let (first, mut rx) = engine(&w, fixture("acp-approval-allowed.jsonl"));
    let thread = thread_of(&first, &w).await;
    first.send_message(&thread, say("go")).await.unwrap();
    let seen = until(&mut rx, is_approval).await;
    let AgentEvent::ApprovalRequest { request } = &seen.last().unwrap().event else {
        unreachable!()
    };
    let stale = request.id.clone();
    drop(first);

    let (second, mut rx) = engine(&w, fixture("acp-approval-allowed.jsonl"));
    let snap = second.load_thread(&thread).unwrap();
    let events = &snap.turns[0].events;
    assert!(matches!(
        &events[events.len() - 2],
        AgentEvent::Error { message } if message == "Hearth closed during this turn"
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        })
    ));
    assert!(
        second
            .answer_approval(&thread, &stale, ApprovalDecision::AllowOnce)
            .await
            .is_err()
    );
    second.send_message(&thread, say("again")).await.unwrap();
    let next = until(&mut rx, is_approval).await;
    assert_eq!(next[0].seq, snap.last_seq + 1);
}

fn claude_adapter(
    w: &World,
    scenario: &str,
    fixture: &Path,
    expected_resume: Option<&str>,
) -> ClaudeAdapter {
    let replay = replay_bin("claude-replay", "hearth-adapter-claude");
    let script = w.home.join("claude");
    let check_resume = expected_resume
        .map(|id| format!("export REPLAY_RESUME='{id}'"))
        .unwrap_or_default();
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
test "$DISABLE_TELEMETRY:$DO_NOT_TRACK:$CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC" = 1:1:1 || exit 1
export REPLAY_VERSION=2.1.328 REPLAY_SCENARIO='{scenario}' REPLAY_FIXTURE='{}'
{check_resume}
exec '{}' "$@"
"#,
            fixture.display(),
            replay.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    ClaudeAdapter::new(w.home.clone(), None, Some(script))
}

fn engine_with_claude(
    w: &World,
    adapter: Arc<dyn AgentAdapter>,
) -> (Engine, mpsc::UnboundedReceiver<ThreadEventPayload>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let engine = Engine::new(
        EngineConfig {
            db_path: Some(w.db.clone()),
            home: w.home.clone(),
            roots: vec![],
            agents: vec![Registration {
                id: AgentId::ClaudeCode,
                label: "Claude Code",
                adapter,
            }],
        },
        Arc::new(ChannelSink(tx)),
    )
    .unwrap();
    (engine, rx)
}

fn claude_engine(
    w: &World,
    scenario: &str,
    fixture: &Path,
    expected_resume: Option<&str>,
) -> (Engine, mpsc::UnboundedReceiver<ThreadEventPayload>) {
    engine_with_claude(
        w,
        Arc::new(claude_adapter(w, scenario, fixture, expected_resume)),
    )
}

struct GatedClaude {
    adapter: ClaudeAdapter,
    entered: Notify,
    release: Notify,
}

impl AgentAdapter for GatedClaude {
    fn availability(&self) -> AgentFuture<'_, AgentStatus> {
        self.adapter.availability()
    }

    fn open_session<'a>(
        &'a self,
        cwd: &'a Path,
        resume_id: Option<&'a str>,
        passthrough: &'a [String],
    ) -> AgentFuture<'a, Result<OpenSession, String>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            self.adapter.open_session(cwd, resume_id, passthrough).await
        })
    }
}

// Fails if Stop during initial session launch submits the cancelled prompt or cancels the next one.
#[tokio::test]
async fn cancelling_during_startup_does_not_submit_or_cancel_the_next_turn() {
    let w = world();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude/simple.jsonl");
    let gated = Arc::new(GatedClaude {
        adapter: claude_adapter(&w, "simple", &fixture, None),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let (engine, mut rx) = engine_with_claude(&w, gated.clone());
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    let thread = engine
        .create_thread(&project.id, AgentId::ClaudeCode)
        .await
        .unwrap()
        .id;
    let sending = tokio::spawn({
        let engine = engine.clone();
        let thread = thread.clone();
        async move {
            engine
                .send_message(&thread, say("Must not reach replay"))
                .await
        }
    });
    gated.entered.notified().await;
    engine.cancel_turn(&thread).await.unwrap();
    gated.release.notify_one();
    sending.await.unwrap().unwrap();
    let first = until(&mut rx, is_end).await;
    assert_eq!(first.len(), 1);
    assert!(matches!(
        first[0].event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        }
    ));

    engine.send_message(&thread, say("Replay")).await.unwrap();
    let next = until(&mut rx, is_end).await;
    assert!(
        next.iter()
            .any(|payload| matches!(payload.event, AgentEvent::TextDelta { .. }))
    );
    assert!(matches!(
        next.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::EndTurn
        }
    ));
    assert_eq!(engine.load_thread(&thread).unwrap().turns.len(), 2);
}

// Fails if Claude cannot stream, replay an approval once, persist its native resume ID, or cancel after reopening.
#[tokio::test]
async fn claude_thread_streams_approvals_and_resumes_for_cancellation() {
    let w = world();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude");
    let approval_path = fixtures.join("approval-allowed.jsonl");
    let (first, mut rx) = claude_engine(&w, "allow", &approval_path, None);
    assert!(matches!(
        first.list_agents().await[0].status,
        AgentStatus::Ready
    ));
    let project = first.add_project(w.project.to_str().unwrap()).unwrap();
    let thread = first
        .create_thread(&project.id, AgentId::ClaudeCode)
        .await
        .unwrap()
        .id;

    first.send_message(&thread, say("Replay")).await.unwrap();
    let before = until(&mut rx, is_approval).await;
    assert!(
        before
            .iter()
            .any(|event| event.model.as_deref() == Some("claude-haiku-5-5"))
    );
    let AgentEvent::ApprovalRequest { request } = &before.last().unwrap().event else {
        unreachable!()
    };
    assert!(
        first
            .answer_approval(&thread, &request.id, ApprovalDecision::AllowForThread)
            .await
            .is_err()
    );
    first
        .answer_approval(&thread, &request.id, ApprovalDecision::AllowOnce)
        .await
        .unwrap();
    let after = until(&mut rx, is_end).await;
    assert!(
        after
            .iter()
            .any(|event| matches!(event.event, AgentEvent::TextDelta { .. }))
    );
    drop(first);

    let approval_init: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(approval_path)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let session_id = approval_init["session_id"].as_str().unwrap();
    let (reopened, mut rx) = claude_engine(
        &w,
        "interrupt",
        &fixtures.join("interrupt.jsonl"),
        Some(session_id),
    );
    let snapshot = reopened.load_thread(&thread).unwrap();
    assert_eq!(snapshot.thread.model, "claude-haiku-5-5");
    assert_eq!(snapshot.turns.len(), 1);
    reopened.send_message(&thread, say("Replay")).await.unwrap();
    until(&mut rx, |event| {
        matches!(event, AgentEvent::TextDelta { .. })
    })
    .await;
    reopened.cancel_turn(&thread).await.unwrap();
    let finished = until(&mut rx, is_end).await;
    assert!(matches!(
        finished.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        }
    ));
    assert_eq!(reopened.load_thread(&thread).unwrap().turns.len(), 2);
}

// Fails if switching changes the native session, rewrites history, or restores the wrong model.
#[tokio::test]
async fn qwen_switch_keeps_native_context_and_historical_labels_across_reopen() {
    let w = world();
    let fixture = model_fixture(&w, &["slow", "fast"], true);
    let (first, mut events) = engine(&w, fixture.clone());
    let thread = thread_of(&first, &w).await;
    first
        .send_message(&thread, say("Earlier request"))
        .await
        .unwrap();
    until(&mut events, is_end).await;

    let options = first.list_thread_models(&thread).await.unwrap();
    assert_eq!(options.selected, None);
    assert_eq!(options.current.as_deref(), Some("slow"));
    assert_eq!(
        options
            .choices
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["slow", "fast"]
    );
    assert_eq!(
        first.set_thread_model(&thread, "fast").await.unwrap().model,
        "fast"
    );
    first
        .send_message(&thread, say("Next request"))
        .await
        .unwrap();
    until(&mut events, is_end).await;
    drop(first);

    let (reopened, mut events) = engine(&w, fixture);
    let snapshot = reopened.load_thread(&thread).unwrap();
    assert_eq!(snapshot.thread.model, "fast");
    assert_eq!(
        snapshot
            .turns
            .iter()
            .map(|turn| turn.model.as_str())
            .collect::<Vec<_>>(),
        ["slow", "fast"]
    );
    assert_eq!(
        reopened
            .list_thread_models(&thread)
            .await
            .unwrap()
            .selected
            .as_deref(),
        Some("fast")
    );
    assert_eq!(
        reopened
            .send_message(&thread, say("After restart"))
            .await
            .unwrap()
            .model,
        "fast"
    );
    until(&mut events, is_end).await;
    let store = hearth_core::store::Store::open(&w.db).unwrap();
    let session = store.latest_agent_session(&thread).unwrap().unwrap();
    assert_eq!(
        session.external_session_id.as_deref(),
        Some("same-native-session")
    );
    assert_eq!(session.model, "fast");
}

// Fails if either an unlisted choice or a native control rejection overwrites the current model.
#[tokio::test]
async fn rejected_model_change_keeps_the_previous_selection() {
    let w = world();
    let (engine, mut events) = engine(&w, model_fixture(&w, &["slow", "fast"], false));
    let thread = thread_of(&engine, &w).await;
    engine
        .send_message(&thread, say("Earlier request"))
        .await
        .unwrap();
    until(&mut events, is_end).await;
    for rejected in ["unlisted", "fast"] {
        assert!(engine.set_thread_model(&thread, rejected).await.is_err());
        assert_eq!(engine.load_thread(&thread).unwrap().thread.model, "slow");
        assert_eq!(
            engine.list_thread_models(&thread).await.unwrap().selected,
            None
        );
    }
}

// Fails if reopening with a removed model silently sends using a different one.
#[tokio::test]
async fn removed_saved_model_blocks_sending_until_an_available_choice_is_made() {
    let w = world();
    let (first, mut events) = engine(&w, model_fixture(&w, &["slow", "fast"], true));
    let thread = thread_of(&first, &w).await;
    first.set_thread_model(&thread, "fast").await.unwrap();
    first.send_message(&thread, say("Used fast")).await.unwrap();
    until(&mut events, is_end).await;
    drop(first);

    let (reopened, mut events) = engine(&w, model_fixture(&w, &["slow"], true));
    let options = reopened.list_thread_models(&thread).await.unwrap();
    assert_eq!(options.selected.as_deref(), Some("fast"));
    assert_eq!(options.choices.len(), 1);
    assert!(
        reopened
            .send_message(&thread, say("Do not send"))
            .await
            .is_err()
    );
    assert_eq!(reopened.load_thread(&thread).unwrap().turns.len(), 1);
    assert_eq!(
        reopened
            .set_thread_model(&thread, "slow")
            .await
            .unwrap()
            .model,
        "slow"
    );
    reopened
        .send_message(&thread, say("Used slow"))
        .await
        .unwrap();
    until(&mut events, is_end).await;
    let turns = reopened.load_thread(&thread).unwrap().turns;
    assert_eq!(
        turns
            .iter()
            .map(|turn| turn.model.as_str())
            .collect::<Vec<_>>(),
        ["fast", "slow"]
    );
}

// Fails if a model choice can overlap submission while either operation starts the native session.
#[tokio::test]
async fn model_change_and_sending_cannot_overlap_session_startup() {
    for switching_first in [false, true] {
        let w = world();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude/simple.jsonl");
        let gated = Arc::new(GatedClaude {
            adapter: claude_adapter(&w, "simple", &fixture, None),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let (engine, mut events) = engine_with_claude(&w, gated.clone());
        let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
        let thread = engine
            .create_thread(&project.id, AgentId::ClaudeCode)
            .await
            .unwrap()
            .id;
        let first = tokio::spawn({
            let engine = engine.clone();
            let thread = thread.clone();
            async move {
                if switching_first {
                    engine.set_thread_model(&thread, "haiku").await
                } else {
                    engine.send_message(&thread, say("Replay")).await
                }
            }
        });

        gated.entered.notified().await;
        if switching_first {
            assert!(
                engine
                    .send_message(&thread, say("Must not reach replay"))
                    .await
                    .is_err()
            );
        } else {
            assert!(engine.set_thread_model(&thread, "haiku").await.is_err());
        }
        gated.release.notify_one();
        let result = first.await.unwrap().unwrap();
        if switching_first {
            assert_eq!(result.model, "claude-haiku-5-5");
            engine.send_message(&thread, say("Replay")).await.unwrap();
        }
        until(&mut events, is_end).await;
        assert_eq!(engine.load_thread(&thread).unwrap().turns.len(), 1);
    }
}

fn model_fixture(w: &World, models: &[&str], switch_ack: bool) -> PathBuf {
    use serde_json::json;
    let choices = models
        .iter()
        .map(|id| json!({"modelId": id, "name": id}))
        .collect::<Vec<_>>();
    let path = w
        .project
        .join(format!("model-{}-{switch_ack}.jsonl", models.join("-")));
    let models = json!({"currentModelId":models[0],"availableModels":choices});
    let mut frames = vec![
        json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}),
        json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"same-native-session","models":models}}),
        json!({"jsonrpc":"2.0","id":3,"result":{"modes":{},"models":models}}),
    ];
    if switch_ack {
        frames.push(json!({"jsonrpc":"2.0","id":4,"result":{"_meta":{"qwenModelSwitch":{}}}}));
    }
    for id in 5..7 {
        frames.push(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
    }
    std::fs::write(
        &path,
        frames
            .into_iter()
            .map(|frame| frame.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    path
}

fn git_in(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn init_git(w: &World) {
    git_in(&w.project, &["init", "-q"]);
    std::fs::write(w.project.join("tracked"), "base").unwrap();
    git_in(&w.project, &["add", "tracked"]);
    git_in(
        &w.project,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "base",
        ],
    );
}

// Fails if the whole-worktree review misses untracked files, changes Git metadata,
// or reads later checkout contents instead of the captured tree.
#[tokio::test]
async fn worktree_review_captures_and_reads_the_original_view() {
    let w = world();
    init_git(&w);
    git_in(&w.project, &["repack", "-ad"]);
    let (app, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = app.add_project(w.project.to_str().unwrap()).unwrap();
    let thread = app
        .create_thread_in(&project.id, AgentId::QwenCode, Some("HEAD".into()))
        .await
        .unwrap();
    let direct = app
        .create_thread(&project.id, AgentId::QwenCode)
        .await
        .unwrap();
    assert!(
        app.worktree_diff_files(&direct.id, &app.start_worktree_review(&direct.id))
            .await
            .unwrap()
            .is_none()
    );
    let saved = hearth_core::store::Store::open(&w.db)
        .unwrap()
        .get_thread(&thread.id)
        .unwrap();
    let checkout = Path::new(&saved.cwd);
    std::fs::write(checkout.join("tracked"), "first\n").unwrap();
    std::fs::write(checkout.join("new"), "new file\n").unwrap();
    let other = app
        .create_thread_in(&project.id, AgentId::QwenCode, Some("HEAD".into()))
        .await
        .unwrap();
    let other_saved = hearth_core::store::Store::open(&w.db)
        .unwrap()
        .get_thread(&other.id)
        .unwrap();
    let index_before = std::fs::read(w.project.join(".git/index")).unwrap();
    let refs_before = git_in(&w.project, &["show-ref"]);
    let objects_before = git_in(&w.project, &["count-objects", "-v"]);
    let durable = w.db.parent().unwrap().join("snapshots");
    std::fs::create_dir(&durable).unwrap();
    std::fs::write(durable.join("retained"), b"existing snapshot").unwrap();
    let view = app
        .worktree_diff_files(&thread.id, &app.start_worktree_review(&thread.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_dir(&durable).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(durable.join("retained")).unwrap(),
        b"existing snapshot"
    );
    assert_eq!(
        view.files
            .iter()
            .map(|f| (f.path.as_str(), f.status.as_str()))
            .collect::<Vec<_>>(),
        vec![("new", "A"), ("tracked", "M")]
    );
    let file = view.files.iter().find(|f| f.path == "tracked").unwrap();
    std::fs::write(checkout.join("tracked"), "later\n").unwrap();
    let diff = app
        .worktree_diff_file(&thread.id, &view.id, &file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        diff.hunks
            .iter()
            .flat_map(|h| &h.rows)
            .any(|r| r.text == "first")
    );
    assert!(
        !diff
            .hunks
            .iter()
            .flat_map(|h| &h.rows)
            .any(|r| r.text == "later")
    );
    assert!(
        app.worktree_diff_file(&direct.id, &view.id, &file.id)
            .await
            .unwrap()
            .is_none()
    );
    let dotgit = checkout.join(".git");
    let original = std::fs::read(&dotgit).unwrap();
    std::fs::write(
        &dotgit,
        std::fs::read(Path::new(&other_saved.cwd).join(".git")).unwrap(),
    )
    .unwrap();
    assert!(
        app.worktree_diff_file(&thread.id, &view.id, &file.id)
            .await
            .unwrap()
            .is_none()
    );
    std::fs::write(&dotgit, original).unwrap();

    let next = app
        .worktree_diff_files(&thread.id, &app.start_worktree_review(&thread.id))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(view.id, next.id);
    assert!(
        app.worktree_diff_file(&thread.id, &view.id, &file.id)
            .await
            .unwrap()
            .is_none()
    );
    app.release_worktree_review(&thread.id, &next.id).await;
    assert_eq!(std::fs::read_dir(&durable).unwrap().count(), 1);
    assert!(
        !std::fs::read_dir(w.db.parent().unwrap())
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("live-review-")
            })
    );
    assert!(
        app.worktree_diff_file(&thread.id, &next.id, &file.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        std::fs::read(w.project.join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(git_in(&w.project, &["show-ref"]), refs_before);
    assert_eq!(git_in(&w.project, &["count-objects", "-v"]), objects_before);
}

// Fails if releasing a pending review allows it to replace a newer request.
#[tokio::test]
async fn released_pending_worktree_review_cannot_replace_a_new_request() {
    let w = world();
    init_git(&w);
    let (app, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = app.add_project(w.project.to_str().unwrap()).unwrap();
    let thread = app
        .create_thread_in(&project.id, AgentId::QwenCode, Some("HEAD".into()))
        .await
        .unwrap();
    let cancelled = app.start_worktree_review(&thread.id);
    app.release_worktree_review(&thread.id, &cancelled).await;
    let current = app.start_worktree_review(&thread.id);
    assert!(
        app.worktree_diff_files(&thread.id, &cancelled)
            .await
            .unwrap()
            .is_none()
    );
    app.release_worktree_review(&thread.id, &cancelled).await;
    assert!(
        app.worktree_diff_files(&thread.id, &current)
            .await
            .unwrap()
            .is_some()
    );
    app.release_worktree_review(&thread.id, &current).await;
}

// Fails if a source alternate is followed to read an unguarded object store.
#[tokio::test]
async fn worktree_review_rejects_source_object_alternates() {
    let w = world();
    init_git(&w);
    let (app, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = app.add_project(w.project.to_str().unwrap()).unwrap();
    let thread = app
        .create_thread_in(&project.id, AgentId::QwenCode, Some("HEAD".into()))
        .await
        .unwrap();
    std::fs::write(
        w.project.join(".git/objects/info/alternates"),
        "../elsewhere\n",
    )
    .unwrap();
    assert!(
        app.worktree_diff_files(&thread.id, &app.start_worktree_review(&thread.id))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !std::fs::read_dir(w.db.parent().unwrap())
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("live-review-")
            })
    );
    assert!(!w.db.parent().unwrap().join("snapshots").exists());
}

// Fails if creation skips home-configured hooks, changes the source, loses its base, or reopens external metadata.
#[tokio::test]
async fn worktree_persists_and_reopens_with_the_source_untouched() {
    let w = world();
    init_git(&w);
    let hooks = w.home.join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, "#!/bin/sh\nprintf ran > checkout-marker\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        w.home.join(".gitconfig"),
        format!("[core]\n hooksPath = {}\n", hooks.display()),
    )
    .unwrap();
    let base = git_in(&w.project, &["rev-parse", "HEAD"]);
    std::fs::write(w.project.join("tracked"), "dirty").unwrap();
    let (engine, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    let info = engine.worktree_info(&project.id).unwrap();
    assert!(info.is_git && info.base.is_some());
    let thread = engine
        .create_thread_in(&project.id, AgentId::QwenCode, info.base)
        .await
        .unwrap();
    assert!(thread.branch.as_deref().unwrap().starts_with("hearth/"));
    let store = hearth_core::store::Store::open(&w.db).unwrap();
    let saved = store.get_thread(&thread.id).unwrap();
    assert_eq!(saved.base_ref.as_deref(), Some(base.as_str()));
    assert_eq!(saved.worktree_path.as_deref(), Some(saved.cwd.as_str()));
    assert_eq!(
        std::fs::read_to_string(Path::new(&saved.cwd).join("checkout-marker")).unwrap(),
        "ran"
    );
    assert_eq!(
        std::fs::read_to_string(w.project.join("tracked")).unwrap(),
        "dirty"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&saved.cwd).join("tracked")).unwrap(),
        "base"
    );
    drop(store);
    drop(engine);
    let opened_cwds = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (tx, _rx) = mpsc::unbounded_channel();
    let reopened = Engine::new(
        EngineConfig {
            db_path: Some(w.db.clone()),
            home: w.home.clone(),
            roots: vec![],
            agents: vec![Registration {
                id: AgentId::QwenCode,
                label: "Qwen Code",
                adapter: Arc::new(QwenAdapter(Arc::new(RecordingLauncher {
                    fixture: fixture("acp-cancel.jsonl"),
                    cwd: opened_cwds.clone(),
                }))),
            }],
        },
        Arc::new(ChannelSink(tx)),
    )
    .unwrap();
    assert_eq!(
        reopened.load_thread(&thread.id).unwrap().thread.branch,
        thread.branch
    );
    reopened.list_thread_models(&thread.id).await.unwrap();
    assert_eq!(
        *opened_cwds.lock().unwrap(),
        vec![PathBuf::from(&saved.cwd)]
    );
    drop(reopened);
    let external = w.project.parent().unwrap().join("other-metadata");
    git_in(
        &w.project,
        &["init", "--bare", "-q", external.to_str().unwrap()],
    );
    std::fs::write(
        Path::new(&saved.cwd).join(".git"),
        format!("gitdir: {}\n", external.display()),
    )
    .unwrap();
    let (reopened, _) = self::engine(&w, fixture("acp-cancel.jsonl"));
    assert!(reopened.list_thread_models(&thread.id).await.is_err());
}

// Fails if a chosen parent is forgotten on reopen or the source checkout changes.
#[tokio::test]
async fn custom_worktree_parent_survives_reopen() {
    let w = world();
    init_git(&w);
    let parent = w.project.parent().unwrap().join("chosen");
    std::fs::create_dir(&parent).unwrap();
    let (engine, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    let thread = engine
        .create_thread_in_parent(
            &project.id,
            AgentId::QwenCode,
            Some("HEAD".into()),
            Some(parent.to_string_lossy().into_owned()),
        )
        .await
        .unwrap();
    let store = hearth_core::store::Store::open(&w.db).unwrap();
    let saved = store.get_thread(&thread.id).unwrap();
    assert_eq!(
        Path::new(&saved.cwd).parent().unwrap(),
        parent.join("project")
    );
    assert_eq!(
        std::fs::read_to_string(w.project.join("tracked")).unwrap(),
        "base"
    );
    assert_eq!(
        git_in(Path::new(&saved.cwd), &["branch", "--show-current"]),
        thread.branch.unwrap()
    );
    drop(store);
    drop(engine);
    let (reopened, _) = self::engine(&w, fixture("acp-cancel.jsonl"));
    reopened.list_thread_models(&thread.id).await.unwrap();
}

// Fails if protected, redirected, or in-project parents create a thread.
#[tokio::test]
async fn invalid_custom_parents_are_refused() {
    let w = world();
    init_git(&w);
    let protected = w.home.join(".ssh");
    std::fs::create_dir(&protected).unwrap();
    let link = w.project.parent().unwrap().join("redirected");
    std::os::unix::fs::symlink(&w.home, &link).unwrap();
    let (engine, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    for parent in [&protected, &link, &w.project, w.project.parent().unwrap()] {
        assert!(
            engine
                .create_thread_in_parent(
                    &project.id,
                    AgentId::QwenCode,
                    Some("HEAD".into()),
                    Some(parent.to_string_lossy().into_owned())
                )
                .await
                .is_err()
        );
    }
    assert!(engine.list_threads(&project.id).unwrap().is_empty());
}

// Fails if an invalid base or a symlinked destination creates a thread or changes the source.
#[tokio::test]
async fn worktree_refuses_invalid_base_and_symlink_destination() {
    let w = world();
    init_git(&w);
    let (engine, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    assert!(
        engine
            .create_thread_in(&project.id, AgentId::QwenCode, Some("missing-base".into()))
            .await
            .is_err()
    );
    let outside = w.project.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(
        &outside,
        w.project.parent().unwrap().join(".hearth-worktrees"),
    )
    .unwrap();
    assert!(
        engine
            .create_thread_in(&project.id, AgentId::QwenCode, Some("HEAD".into()))
            .await
            .is_err()
    );
    assert!(engine.list_threads(&project.id).unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(w.project.join("tracked")).unwrap(),
        "base"
    );
}

// Fails if ordinary folders or explicit repo-mode threads require a worktree.
#[tokio::test]
async fn direct_threads_still_open_in_the_project_folder() {
    let w = world();
    let (engine, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    assert!(!engine.worktree_info(&project.id).unwrap().is_git);
    let thread = engine
        .create_thread(&project.id, AgentId::QwenCode)
        .await
        .unwrap();
    let store = hearth_core::store::Store::open(&w.db).unwrap();
    let saved = store.get_thread(&thread.id).unwrap();
    assert_eq!(saved.cwd, project.path);
    assert!(saved.worktree_path.is_none());
}

// Fails if Git probes external common metadata for a new worktree or blocks direct threads.
#[tokio::test]
async fn external_git_metadata_refuses_worktrees_but_allows_direct_threads() {
    let w = world();
    let metadata = w.project.parent().unwrap().join("separate-metadata");
    git_in(
        &w.project,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
        ],
    );
    std::fs::write(w.project.join("tracked"), "base").unwrap();
    git_in(&w.project, &["add", "tracked"]);
    git_in(
        &w.project,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "base",
        ],
    );
    let (engine, _) = engine(&w, fixture("acp-cancel.jsonl"));
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    let info = engine.worktree_info(&project.id).unwrap();
    assert!(!info.is_git);
    assert!(info.reason.unwrap().contains("outside the project"));
    let error = engine
        .create_thread_in(&project.id, AgentId::QwenCode, Some("HEAD".into()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outside the project"));
    assert!(
        !w.project
            .parent()
            .unwrap()
            .join(".hearth-worktrees")
            .exists()
    );
    engine
        .create_thread(&project.id, AgentId::QwenCode)
        .await
        .unwrap();

    let linked_world = world();
    init_git(&linked_world);
    let linked = linked_world.project.parent().unwrap().join("linked");
    git_in(
        &linked_world.project,
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    let (linked_engine, _) = self::engine(&linked_world, fixture("acp-cancel.jsonl"));
    let linked_project = linked_engine.add_project(linked.to_str().unwrap()).unwrap();
    assert!(
        linked_engine
            .worktree_info(&linked_project.id)
            .unwrap()
            .reason
            .unwrap()
            .contains("outside the project")
    );
    assert!(
        linked_engine
            .create_thread_in(&linked_project.id, AgentId::QwenCode, Some("HEAD".into()))
            .await
            .is_err()
    );
    linked_engine
        .create_thread(&linked_project.id, AgentId::QwenCode)
        .await
        .unwrap();
}

// Fails if turn diffs lose edits, deletions, links or history across reopen.
#[tokio::test]
async fn snapshots_changed_checkout_and_reopens_the_diff() {
    use std::os::unix::fs::symlink;
    let w = world();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&w.project)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    std::fs::write(w.project.join("file"), "before\n").unwrap();
    std::fs::write(w.project.join("deleted"), "gone\n").unwrap();
    std::fs::write(w.project.join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(w.project.join("ignored"), "secret\n").unwrap();
    std::fs::write(w.project.join(".git/info/exclude"), "local-ignore\n").unwrap();
    std::fs::write(w.project.join("local-ignore"), "secret\n").unwrap();
    symlink("old-target", w.project.join("link")).unwrap();
    git(&["add", "file", "deleted", ".gitignore", "link"]);
    let index = std::fs::read(w.project.join(".git/index")).unwrap();
    let (app, mut rx) = engine(&w, fixture("acp-approval-allowed.jsonl"));
    let thread = thread_of(&app, &w).await;
    app.send_message(&thread, say("go")).await.unwrap();
    let approval = until(&mut rx, is_approval).await;
    std::fs::write(w.project.join("file"), "after\n").unwrap();
    std::fs::remove_file(w.project.join("deleted")).unwrap();
    std::fs::remove_file(w.project.join("link")).unwrap();
    symlink("new-target", w.project.join("link")).unwrap();
    std::fs::write(w.project.join("ignored"), "changed\n").unwrap();
    std::fs::write(w.project.join("local-ignore"), "changed\n").unwrap();
    let AgentEvent::ApprovalRequest { request } = &approval.last().unwrap().event else {
        unreachable!()
    };
    app.answer_approval(&thread, &request.id, ApprovalDecision::AllowOnce)
        .await
        .unwrap();
    let events = until(&mut rx, is_end).await;
    let turn = events.last().unwrap().turn_id.clone();
    let reviews = app.review_turns(&thread).unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].turn_id, turn);
    assert_eq!(reviews[0].label, "go");
    assert!(reviews[0].available);
    let files = app.turn_diff_files(&thread, &turn).await.unwrap().unwrap();
    let file = files.iter().find(|file| file.path == "file").unwrap();
    assert_eq!(file.id, "66696c65");
    assert_eq!(file.status, "M");
    let content = app
        .turn_diff_file(&thread, &turn, &file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!content.binary);
    assert_eq!(content.hunks[0].split_rows.len(), 1);
    assert_eq!(
        (
            content.hunks[0].split_rows[0].left,
            content.hunks[0].split_rows[0].right
        ),
        (Some(0), Some(1))
    );
    assert!(
        content.hunks[0]
            .rows
            .iter()
            .any(|row| row.kind == hearth_engine::DiffRowKind::Delete
                && row.text == "before"
                && row.old_line == Some(1))
    );
    assert!(
        content.hunks[0]
            .rows
            .iter()
            .any(|row| row.kind == hearth_engine::DiffRowKind::Add
                && row.text == "after"
                && row.new_line == Some(1))
    );
    assert!(
        files
            .iter()
            .any(|file| file.path == "deleted" && file.status == "D")
    );
    let diff = app.turn_diff(&thread, &turn).await.unwrap().unwrap();
    let text = String::from_utf8(diff.clone()).unwrap();
    assert!(
        text.contains("-before")
            && text.contains("+after")
            && text.contains("-gone")
            && text.contains("+new-target")
    );
    assert!(!text.contains("ignored") && !text.contains("local-ignore"));
    assert_eq!(std::fs::read(w.project.join(".git/index")).unwrap(), index);
    drop(app);
    let (reopened, _) = engine(&w, fixture("acp-approval-allowed.jsonl"));
    assert_eq!(
        reopened.turn_diff(&thread, &turn).await.unwrap(),
        Some(diff)
    );
}

// Fails if an empty diff is confused with unavailable capture, or reply is lost.
#[tokio::test]
async fn snapshots_distinguish_empty_from_unavailable_without_losing_reply() {
    for scenario in ["empty", "non-git", "unsafe-index", "protected-root"] {
        let mut w = world();
        if scenario == "protected-root" {
            w.home = w.project.clone();
        }
        if scenario != "non-git" {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&w.project)
                    .args(["init", "-q"])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        if scenario == "unsafe-index" {
            std::os::unix::fs::symlink("missing", w.project.join(".git/index")).unwrap();
        }
        let (engine, mut rx) = engine(&w, fixture("acp-approval-allowed.jsonl"));
        let thread = thread_of(&engine, &w).await;
        engine.send_message(&thread, say("go")).await.unwrap();
        let approval = until(&mut rx, is_approval).await;
        let AgentEvent::ApprovalRequest { request } = &approval.last().unwrap().event else {
            unreachable!()
        };
        engine
            .answer_approval(&thread, &request.id, ApprovalDecision::AllowOnce)
            .await
            .unwrap();
        let events = until(&mut rx, is_end).await;
        let turn = &events.last().unwrap().turn_id;
        assert_eq!(
            engine.review_turns(&thread).unwrap()[0].available,
            scenario == "empty"
        );
        assert_eq!(
            engine.turn_diff_files(&thread, turn).await.unwrap(),
            (scenario == "empty").then(Vec::new)
        );
        let other = thread_of(&engine, &w).await;
        assert!(engine.turn_diff_files(&other, turn).await.is_err());
        assert_eq!(
            engine.turn_diff(&thread, turn).await.unwrap(),
            (scenario == "empty").then(Vec::new)
        );
        assert!(
            !engine.load_thread(&thread).unwrap().turns[0]
                .events
                .is_empty()
        );
    }
}

// Fails if renamed raw paths, binary changes, or missing newlines are lost.
#[tokio::test]
async fn review_files_preserve_renames_raw_paths_and_binary_notices() {
    use std::os::unix::ffi::OsStringExt;
    let w = world();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&w.project)
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    let (old_bytes, new_bytes): (&[u8], &[u8]) = if cfg!(target_os = "macos") {
        (b"old name", b"new name")
    } else {
        (b"old \xff", b"new \xfe")
    };
    let old = std::ffi::OsString::from_vec(old_bytes.to_vec());
    let new = std::ffi::OsString::from_vec(new_bytes.to_vec());
    std::fs::write(w.project.join(&old), "unchanged\n").unwrap();
    std::fs::write(w.project.join("binary"), b"before\0x").unwrap();
    std::fs::write(w.project.join("lines"), "old").unwrap();
    git(&["add", "."]);
    let (app, mut rx) = engine(&w, fixture("acp-approval-allowed.jsonl"));
    let thread = thread_of(&app, &w).await;
    app.send_message(&thread, say("go")).await.unwrap();
    let approval = until(&mut rx, is_approval).await;
    std::fs::rename(w.project.join(old), w.project.join(new)).unwrap();
    std::fs::write(w.project.join("binary"), b"after\0x").unwrap();
    std::fs::write(w.project.join("lines"), "new").unwrap();
    let AgentEvent::ApprovalRequest { request } = &approval.last().unwrap().event else {
        unreachable!()
    };
    app.answer_approval(&thread, &request.id, ApprovalDecision::AllowOnce)
        .await
        .unwrap();
    let events = until(&mut rx, is_end).await;
    let turn = &events.last().unwrap().turn_id;
    let files = app.turn_diff_files(&thread, turn).await.unwrap().unwrap();
    let renamed = files.iter().find(|f| f.status == "R").unwrap();
    if cfg!(target_os = "macos") {
        assert_eq!(renamed.id, "6e6577206e616d65");
        assert_eq!(renamed.path, "new name");
        assert_eq!(renamed.old_path.as_deref(), Some("old name"));
    } else {
        assert_eq!(renamed.id, "6e657720fe");
        assert_eq!(renamed.path, "new �");
        assert_eq!(renamed.old_path.as_deref(), Some("old �"));
    }
    assert!(
        !app.turn_diff_file(&thread, turn, &renamed.id)
            .await
            .unwrap()
            .unwrap()
            .binary
    );
    let binary = files.iter().find(|f| f.path == "binary").unwrap();
    assert!(
        app.turn_diff_file(&thread, turn, &binary.id)
            .await
            .unwrap()
            .unwrap()
            .binary
    );
    let lines = files.iter().find(|f| f.path == "lines").unwrap();
    let diff = app
        .turn_diff_file(&thread, turn, &lines.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        diff.hunks[0]
            .rows
            .iter()
            .filter(|r| r.kind == hearth_engine::DiffRowKind::Notice)
            .count(),
        2
    );
    assert_eq!(
        diff.hunks[0]
            .split_rows
            .iter()
            .filter(|row| row.notice.is_some())
            .count(),
        2
    );
}

// Fails if a file replaced by a directory includes its descendant's patch.
#[tokio::test]
async fn review_file_replaced_by_directory_reads_each_file_separately() {
    let w = world();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&w.project)
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    std::fs::write(w.project.join("foo"), "old file\n").unwrap();
    git(&["add", "."]);
    let (app, mut rx) = engine(&w, fixture("acp-approval-allowed.jsonl"));
    let thread = thread_of(&app, &w).await;
    app.send_message(&thread, say("go")).await.unwrap();
    let approval = until(&mut rx, is_approval).await;
    std::fs::remove_file(w.project.join("foo")).unwrap();
    std::fs::create_dir(w.project.join("foo")).unwrap();
    std::fs::write(w.project.join("foo/bar"), "new descendant\n").unwrap();
    git(&["add", "-A"]);
    let AgentEvent::ApprovalRequest { request } = &approval.last().unwrap().event else {
        unreachable!()
    };
    app.answer_approval(&thread, &request.id, ApprovalDecision::AllowOnce)
        .await
        .unwrap();
    let events = until(&mut rx, is_end).await;
    let turn = &events.last().unwrap().turn_id;
    let files = app.turn_diff_files(&thread, turn).await.unwrap().unwrap();
    for (path, kind, text) in [
        ("foo", hearth_engine::DiffRowKind::Delete, "old file"),
        ("foo/bar", hearth_engine::DiffRowKind::Add, "new descendant"),
    ] {
        let file = files.iter().find(|file| file.path == path).unwrap();
        let diff = app
            .turn_diff_file(&thread, turn, &file.id)
            .await
            .unwrap()
            .unwrap();
        let rows: Vec<_> = diff.hunks.iter().flat_map(|hunk| &hunk.rows).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, kind);
        assert_eq!(rows[0].text, text);
    }
}
