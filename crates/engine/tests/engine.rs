//! The engine against the replay stub (`qwen-replay`) and a real store.

use hearth_adapter_qwen::SpawnConfig;
use hearth_engine::agents::AgentLauncher;
use hearth_engine::{Engine, EngineConfig, EventSink, ThreadEventPayload};
use hearth_protocol::{AgentEvent, AgentId, ApprovalDecision, StopReason, ThreadId, UserInput};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// `qwen-replay` sits in the target profile directory, some levels above the
/// test binary; build it if it is not there.
fn replay_bin() -> PathBuf {
    let find = || {
        let exe = std::env::current_exe().unwrap();
        exe.ancestors()
            .map(|d| d.join("qwen-replay"))
            .find(|p| p.is_file())
    };
    if let Some(bin) = find() {
        return bin;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = std::process::Command::new(cargo)
        .args(["build", "-p", "hearth-adapter-qwen", "--bin", "qwen-replay"])
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not build qwen-replay");
    find().expect("qwen-replay is not built")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/qwen")
        .join(name)
}

struct ReplayLauncher(PathBuf);

impl AgentLauncher for ReplayLauncher {
    fn spawn_config(&self, cwd: &Path, _passthrough: &[String]) -> Result<SpawnConfig, String> {
        let mut c = SpawnConfig::qwen_defaults(replay_bin(), cwd);
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
            launcher: Arc::new(ReplayLauncher(fixture_path)),
        },
        Arc::new(ChannelSink(tx)),
    )
    .unwrap();
    (engine, rx)
}

fn say(t: &str) -> UserInput {
    UserInput::Text { text: t.into() }
}

fn thread_of(engine: &Engine, w: &World) -> ThreadId {
    let project = engine.add_project(w.project.to_str().unwrap()).unwrap();
    engine
        .create_thread(&project.id, AgentId::QwenCode)
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
    let thread = thread_of(&engine, &w);
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
    let thread = thread_of(&engine, &w);

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
    let thread = thread_of(&engine, &w);
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

// Fails if a reopened thread loses the user's message or events, or its
// numbering restarts (the UI would then drop or duplicate live events).
#[tokio::test]
async fn reopening_the_database_restores_the_thread_and_keeps_numbering() {
    let w = world();
    let (first, mut rx) = engine(&w, fixture("acp-cancel.jsonl"));
    let thread = thread_of(&first, &w);
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
    let thread = thread_of(&first, &w);
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
