//! The real client against the replay stub (`qwen-replay`).

use hearth_adapter_qwen::{AdapterItem, AdapterStream, QwenProcess, SpawnConfig};
use hearth_protocol::{AgentEvent, ApprovalDecision, ApprovalOptionKind, StopReason, UserInput};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/qwen")
        .join(name)
}

fn start(name: &str, env: &[(&str, &str)]) -> QwenProcess {
    QwenProcess::spawn(SpawnConfig {
        program: env!("CARGO_BIN_EXE_qwen-replay").into(),
        args: vec![fixture(name).to_string_lossy().into_owned()],
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: std::env::temp_dir(),
    })
    .unwrap()
}

async fn ready(name: &str) -> QwenProcess {
    let p = start(name, &[]);
    p.initialize().await.unwrap();
    p.new_session(&std::env::temp_dir()).await.unwrap();
    p
}

fn say(text: &str) -> UserInput {
    UserInput::Text { text: text.into() }
}

async fn until_approval(
    s: &mut AdapterStream,
) -> (hearth_protocol::ApprovalRequest, Vec<AdapterItem>) {
    let mut seen = Vec::new();
    while let Some(item) = s.next().await {
        if let AgentEvent::ApprovalRequest { request } = &item.event {
            let r = request.clone();
            seen.push(item);
            return (r, seen);
        }
        seen.push(item);
    }
    panic!("no approval request in the stream");
}

async fn rest(s: &mut AdapterStream) -> Vec<AdapterItem> {
    let mut v = Vec::new();
    while let Some(i) = s.next().await {
        v.push(i);
    }
    v
}

async fn received(p: &QwenProcess) -> Vec<Value> {
    p.debug_request("stub/received", json!({}))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn outcome_of_permission_answer(received: &[Value]) -> Value {
    received
        .iter()
        .find(|m| m["response"].get("outcome").is_some())
        .expect("the agent got no answer to its permission request")["response"]["outcome"]
        .clone()
}

// Fails if text is dropped or reordered, or the turn never ends.
#[tokio::test]
async fn simple_turn_streams_text_then_ends() {
    let p = start("acp-resume-load.jsonl", &[]);
    p.initialize().await.unwrap();
    p.load_session("s", &std::env::temp_dir()).await.unwrap();
    let items = rest(&mut p.prompt(say("hi"))).await;
    let text: String = items
        .iter()
        .filter_map(|i| match &i.event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!text.is_empty());
    assert!(
        items
            .iter()
            .all(|i| !matches!(&i.event, AgentEvent::TextDelta { text } if text.is_empty()))
    );
    assert!(matches!(
        items.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::EndTurn
        }
    ));
    assert!(items.iter().all(|i| i.raw.is_object()));
}

// Fails if either decision answers with an always option, the wrong option,
// or the edit does not surface as a file edit and a finished tool call.
#[tokio::test]
async fn approval_decisions_pick_one_shot_options() {
    let table = [
        (
            "acp-approval-allowed.jsonl",
            ApprovalDecision::AllowOnce,
            "proceed_once",
        ),
        (
            "acp-approval-allowed.jsonl",
            ApprovalDecision::AllowForThread,
            "proceed_once",
        ),
        (
            "acp-approval-denied.jsonl",
            ApprovalDecision::Deny,
            "cancel",
        ),
    ];
    for (file, decision, expected) in table {
        let p = ready(file).await;
        let mut s = p.prompt(say("go"));
        let (request, _) = until_approval(&mut s).await;
        assert!(request.options.iter().all(|o| matches!(
            o.kind,
            ApprovalOptionKind::AllowOnce | ApprovalOptionKind::RejectOnce
        )));
        p.answer_permission(&request, decision).await.unwrap();
        let after = rest(&mut s).await;
        assert!(matches!(
            after.last().unwrap().event,
            AgentEvent::TurnEnd { .. }
        ));
        if file.contains("allowed") {
            assert!(
                after
                    .iter()
                    .any(|i| matches!(i.event, AgentEvent::FileEdit { .. }))
            );
        }
        let outcome = outcome_of_permission_answer(&received(&p).await);
        assert_eq!(
            outcome,
            json!({"outcome": "selected", "optionId": expected}),
            "{file}"
        );
    }
}

// Fails if cancel does not end the turn as cancelled.
#[tokio::test]
async fn cancel_ends_the_turn_as_cancelled() {
    let p = ready("acp-cancel.jsonl").await;
    let mut s = p.prompt(say("count"));
    p.cancel().await.unwrap();
    let items = rest(&mut s).await;
    assert!(matches!(
        items.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        }
    ));
    let seen = received(&p).await;
    assert!(seen.iter().any(|m| m["method"] == "session/cancel"));
}

// Fails if an open approval is left unanswered (or answered with a choice) on cancel.
#[tokio::test]
async fn cancel_answers_an_open_approval_as_cancelled() {
    let p = ready("acp-approval-allowed.jsonl").await;
    let mut s = p.prompt(say("go"));
    until_approval(&mut s).await;
    p.cancel().await.unwrap();
    let items = rest(&mut s).await;
    assert!(matches!(
        items.last().unwrap().event,
        AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled
        }
    ));
    let outcome = outcome_of_permission_answer(&received(&p).await);
    assert_eq!(outcome, json!({"outcome": "cancelled"}));
}

// Fails if history replayed during load is lost or comes after the response.
#[tokio::test]
async fn load_session_returns_the_replayed_history() {
    let p = start("acp-resume-load.jsonl", &[]);
    p.initialize().await.unwrap();
    let loaded = p.load_session("s", &std::env::temp_dir()).await.unwrap();
    assert!(!loaded.models.is_empty());
    assert!(
        loaded
            .history
            .iter()
            .any(|i| matches!(i.event, AgentEvent::TextDelta { .. }))
    );
    assert!(
        loaded
            .history
            .iter()
            .any(|i| matches!(i.event, AgentEvent::ToolStart { .. }))
    );
    assert!(
        loaded
            .history
            .iter()
            .all(|i| !matches!(i.event, AgentEvent::TurnEnd { .. }))
    );
}

// Fails if set_model can write the model into the agent's own settings.
#[tokio::test]
async fn set_model_sends_the_no_persist_flag() {
    let p = start("acp-resume-load.jsonl", &[]);
    p.initialize().await.unwrap();
    p.load_session("s", &std::env::temp_dir()).await.unwrap();
    p.set_model("qwen3.7-plus").await.unwrap();
    let seen = received(&p).await;
    let call = seen
        .iter()
        .find(|m| m["method"] == "session/set_model")
        .unwrap();
    assert_eq!(call["params"]["modelId"], "qwen3.7-plus");
    assert_eq!(
        call["params"]["_meta"]["qwen.session.modelPersistDefault"],
        json!(false)
    );
}

// Fails if the child inherits anything the caller did not pass.
#[tokio::test]
async fn spawned_env_is_exactly_what_the_config_gave() {
    let p = start(
        "acp-initialize-new.jsonl",
        &[("HEARTH_TEST_A", "1"), ("HEARTH_TEST_B", "2")],
    );
    let names = p.debug_request("stub/env", json!({})).await.unwrap();
    assert_eq!(names, json!(["HEARTH_TEST_A", "HEARTH_TEST_B"]));
}

// Fails if the client declares fs or terminal, or calls authenticate.
#[tokio::test]
async fn initialize_declares_no_capabilities_and_never_authenticates() {
    let p = ready("acp-initialize-new.jsonl").await;
    let seen = received(&p).await;
    let init = seen.iter().find(|m| m["method"] == "initialize").unwrap();
    let caps = &init["params"]["clientCapabilities"];
    assert_eq!(caps["terminal"], json!(false));
    assert_eq!(
        caps["fs"],
        json!({"readTextFile": false, "writeTextFile": false})
    );
    assert!(seen.iter().all(|m| m["method"] != "authenticate"));
}
