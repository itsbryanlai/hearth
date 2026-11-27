use hearth_adapter_claude::{ClaudeProcess, Error, Item, SpawnConfig, TurnStream};
use hearth_protocol::{AgentEvent, ApprovalDecision, StopReason, ToolStatus};
use std::path::Path;
use tempfile::TempDir;

fn config(root: &TempDir, fixture: &str, scenario: &str) -> SpawnConfig {
    SpawnConfig {
        executable: env!("CARGO_BIN_EXE_claude-replay").into(),
        cwd: root.path().into(),
        env: vec![
            (
                "REPLAY_FIXTURE".into(),
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/claude")
                    .join(format!("{fixture}.jsonl"))
                    .into_os_string(),
            ),
            ("REPLAY_SCENARIO".into(), scenario.into()),
            ("REPLAY_VERSION".into(), "2.1.328".into()),
        ],
        resume: None,
    }
}

async fn collect(turn: &mut TurnStream) -> Vec<Item> {
    let mut items = Vec::new();
    while let Some(item) = turn.next_item().await {
        items.push(item.expect("turn stream failed"));
    }
    items
}

fn events(items: &[Item]) -> impl Iterator<Item = &AgentEvent> {
    items.iter().filter_map(|item| match item {
        Item::Event { event, .. } => Some(event),
        _ => None,
    })
}

fn text(items: &[Item]) -> String {
    events(items)
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn streams_text_once_when_the_final_assistant_repeats_it() {
    let root = TempDir::new().unwrap();
    let client = ClaudeProcess::spawn(&config(&root, "simple", "verbatim"))
        .await
        .unwrap();
    let items = collect(&mut client.prompt("Replay").await.unwrap()).await;
    assert_eq!(
        text(&items),
        "Hello there friendInspect src/.gitignore. Done."
    );
    let reasoning: Vec<_> = events(&items)
        .filter_map(|event| match event {
            AgentEvent::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, ["I should check src/.gitignore."]);
    assert!(
        matches!(items.first(), Some(Item::Session { model, analytics_disabled: Some(false), .. })
        if model == "claude-haiku-5-5")
    );
    assert!(
        events(&items)
            .any(|event| matches!(event, AgentEvent::Usage { usage } if usage.output_tokens > 0))
    );
    assert!(matches!(
        events(&items).last(),
        Some(AgentEvent::TurnEnd {
            stop_reason: StopReason::EndTurn
        })
    ));
}

#[tokio::test]
async fn answers_a_waiting_approval_once_and_only_reports_successful_edits() {
    for (fixture, scenario, decision, expected_status) in [
        (
            "approval-allowed",
            "allow",
            ApprovalDecision::AllowForThread,
            ToolStatus::Completed,
        ),
        (
            "approval-denied",
            "deny",
            ApprovalDecision::Deny,
            ToolStatus::Failed,
        ),
    ] {
        let root = TempDir::new().unwrap();
        let client = ClaudeProcess::spawn(&config(&root, fixture, scenario))
            .await
            .unwrap();
        let mut turn = client.prompt("Replay").await.unwrap();
        let approval = loop {
            match turn.next_item().await.expect("approval missing").unwrap() {
                Item::Event {
                    event: AgentEvent::ApprovalRequest { request },
                    ..
                } => break request,
                _ => continue,
            }
        };
        assert_eq!(approval.tool, "Edit");
        assert!(approval.detail.ends_with("/notes.txt"));
        client.answer(&approval.id, decision).await.unwrap();
        assert_eq!(
            client.answer(&approval.id, decision).await,
            Err(Error::UnknownApproval)
        );
        let items = collect(&mut turn).await;
        let command = events(&items).find_map(|event| match event {
            AgentEvent::CommandRun { command, .. } => Some(command.as_str()),
            _ => None,
        });
        if scenario == "deny" {
            assert_eq!(command, None);
            assert_eq!(
                text(&items),
                "I didn't append \"beta\" to notes.txt. The Edit call was denied by the user, and I won't retry it or try another write path around that denial.\n\nThe file currently reads:\n\n```\nhello\nsecond\nalpha\n```\n\nIf you want the line added, approve the change or tell me to try again, and I'll append \"beta\" after \"alpha\"."
            );
        } else {
            assert_eq!(command, Some("cat src/.gitignore"));
        }

        assert!(events(&items).any(|event| matches!(event, AgentEvent::ToolEnd { status, .. } if *status == expected_status)));
        assert_eq!(events(&items).any(|event| matches!(event, AgentEvent::FileEdit { path, .. } if path.ends_with("/notes.txt"))),
            expected_status == ToolStatus::Completed);
        assert!(events(&items).any(|event| matches!(event, AgentEvent::ToolOutput { .. })));
    }
}

#[tokio::test]
async fn interrupts_while_stdout_waits_and_does_not_call_unrequested_errors_cancelled() {
    for scenario in ["interrupt", "error"] {
        let root = TempDir::new().unwrap();
        let client = ClaudeProcess::spawn(&config(&root, "interrupt", scenario))
            .await
            .unwrap();
        let mut turn = client.prompt("Replay").await.unwrap();
        loop {
            if matches!(
                turn.next_item()
                    .await
                    .expect("missing streamed reply")
                    .unwrap(),
                Item::Event {
                    event: AgentEvent::TextDelta { .. },
                    ..
                }
            ) {
                break;
            }
        }
        if scenario == "interrupt" {
            client.interrupt().await.unwrap();
        }
        let items = collect(&mut turn).await;
        if scenario == "interrupt" {
            assert!(matches!(
                events(&items).last(),
                Some(AgentEvent::TurnEnd {
                    stop_reason: StopReason::Cancelled
                })
            ));
        } else {
            assert!(matches!(
                events(&items).last(),
                Some(AgentEvent::Error { .. })
            ));
        }
    }
}

#[tokio::test]
async fn resumes_the_requested_native_session_and_reports_its_identity() {
    let root = TempDir::new().unwrap();
    let mut config = config(&root, "resume", "resume");
    let id = "d32dce7e-0684-4813-89a2-0dba37e8824d";
    config.resume = Some(id.into());
    config.env.push(("REPLAY_RESUME".into(), id.into()));
    let client = ClaudeProcess::spawn(&config).await.unwrap();
    let items = collect(&mut client.prompt("Replay").await.unwrap()).await;
    assert!(matches!(items.first(), Some(Item::Session { id: resumed, .. }) if resumed == id));
    assert!(text(&items).contains("append the line \"alpha\" to notes.txt"));
}

#[tokio::test]
async fn uses_installed_model_choices_and_waits_for_the_correlated_switch_response() {
    let root = TempDir::new().unwrap();
    let client = ClaudeProcess::spawn(&config(&root, "simple", "simple"))
        .await
        .unwrap();
    let model = client
        .models()
        .first()
        .expect("initialization reported no models");
    assert_eq!(
        (&*model.value, &*model.label, model.resolved_id.as_deref()),
        ("haiku", "Haiku", Some("claude-haiku-5-5"))
    );
    assert_eq!(
        client.set_model("sonnet").await.unwrap().as_deref(),
        Some("claude-sonnet-5-5")
    );
    assert_eq!(
        text(&collect(&mut client.prompt("Replay").await.unwrap()).await),
        "Hello there friend"
    );
}

#[tokio::test]
async fn rejects_an_untested_cli_version_before_starting_a_session() {
    let root = TempDir::new().unwrap();
    let mut config = config(&root, "simple", "simple");
    config.env[2].1 = "2.1.329".into();
    assert!(
        matches!(ClaudeProcess::spawn(&config).await, Err(Error::UnsupportedVersion(version)) if version == "2.1.329")
    );
}

#[tokio::test]
async fn never_emits_private_control_fields_and_redacts_protected_approval_paths() {
    let root = TempDir::new().unwrap();
    let client = ClaudeProcess::spawn(&config(&root, "approval-allowed", "privacy"))
        .await
        .unwrap();
    let mut turn = client.prompt("Replay").await.unwrap();
    assert!(!format!("{:?}", client.models()).contains("synthetic-private"));
    let mut saw_redacted_approval = false;
    let mut saw_unknown = false;
    let mut saw_protected_read_end = false;
    while let Some(item) = turn.next_item().await {
        let item = item.unwrap();
        let rendered = format!("{item:?}");
        assert!(!rendered.contains("synthetic-private") && !rendered.contains("synthetic-secret"));
        match item {
            Item::Event {
                event: AgentEvent::ApprovalRequest { request },
                ..
            } => {
                assert_eq!(request.detail, "[path omitted]");
                saw_redacted_approval = true;
                client
                    .answer(&request.id, ApprovalDecision::AllowOnce)
                    .await
                    .unwrap();
            }
            Item::Observation { raw } if raw["type"] == "unknown" => saw_unknown = true,
            Item::Event {
                event:
                    AgentEvent::ToolEnd {
                        tool_call_id,
                        status: ToolStatus::Completed,
                    },
                ..
            } if tool_call_id == "synthetic-protected-read" => saw_protected_read_end = true,
            _ => {}
        }
    }
    assert!(saw_redacted_approval && saw_unknown && saw_protected_read_end);
}
