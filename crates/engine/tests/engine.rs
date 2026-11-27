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
