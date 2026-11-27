//! Hearth's shared protocol types: ids, agent descriptors, the normalized
//! `AgentEvent` union, approvals and user input. Plain serde data: no I/O,
//! no async. The TypeScript mirror is generated with `ts-rs` and committed
//! under `packages/protocol/src/generated/`.
//!
//! Regenerate the TypeScript after changing a type:
//!
//! ```text
//! HEARTH_UPDATE_TS=1 cargo test -p hearth-protocol generated_typescript_is_current
//! ```
//!
//! Without `HEARTH_UPDATE_TS` that test regenerates into a temp dir and fails
//! if the committed files differ.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

macro_rules! uuid_id {
    ($($(#[$m:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
        #[ts(type = "string")]
        pub struct $name(pub String);

        impl $name {
            /// A fresh time-ordered (UUIDv7) id.
            pub fn new() -> Self {
                Self(uuid::Uuid::now_v7().to_string())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    )+};
}

uuid_id! {
    /// A local git repository the user added.
    ProjectId,
    /// One conversation and unit of work in a project.
    ThreadId,
    /// One user message and the agent's response to it.
    TurnId,
    /// A live connection between a thread and one agent process.
    AgentSessionId,
    /// One approval request raised by an agent.
    ApprovalId,
    /// One stored event within a turn.
    EventId,
}

/// Which agent. Unknown ids round-trip through `Other`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, TS)]
#[ts(type = "\"claude-code\" | \"qwen-code\" | (string & {})")]
pub enum AgentId {
    ClaudeCode,
    QwenCode,
    Other(String),
}

impl Serialize for AgentId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        String::from(self.clone()).serialize(s)
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Self::from)
    }
}

impl From<String> for AgentId {
    fn from(s: String) -> Self {
        match s.as_str() {
            "claude-code" => Self::ClaudeCode,
            "qwen-code" => Self::QwenCode,
            _ => Self::Other(s),
        }
    }
}

impl From<AgentId> for String {
    fn from(id: AgentId) -> Self {
        match id {
            AgentId::ClaudeCode => "claude-code".into(),
            AgentId::QwenCode => "qwen-code".into(),
            AgentId::Other(s) => s,
        }
    }
}

/// A model an agent reports it can run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub label: String,
}

/// Hearth's own permission modes; each adapter maps them to its agent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Ask before edits and commands.
    Default,
    /// Auto-approve file edits, ask for commands.
    AcceptEdits,
    /// Read-only planning.
    Plan,
}

/// What a tool call does, for choosing an icon and a renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ToolStatus {
    Completed,
    Failed,
}

/// What a file edit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum FileEditKind {
    Create,
    Modify,
    Delete,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// Token counts for a turn so far. Cache fields are absent when the agent
/// does not report them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cache_read_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cache_write_tokens: Option<u32>,
}

/// Hearth's normalized agent event. Agent-specific raw data is stored beside
/// the event (as a JSON value), never inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    /// A chunk of assistant text.
    #[serde(rename = "text.delta")]
    TextDelta { text: String },
    /// A chunk of the assistant's reasoning.
    #[serde(rename = "reasoning.delta")]
    ReasoningDelta { text: String },
    /// A tool call began. `tool_call_id` ties later events to it.
    #[serde(rename = "tool.start")]
    ToolStart {
        tool_call_id: String,
        tool: String,
        tool_kind: ToolKind,
        title: String,
    },
    /// Streamed output of a running tool call.
    #[serde(rename = "tool.output")]
    ToolOutput { tool_call_id: String, text: String },
    /// A tool call finished.
    #[serde(rename = "tool.end")]
    ToolEnd {
        tool_call_id: String,
        status: ToolStatus,
    },
    /// A file was created, changed or deleted by a tool call.
    #[serde(rename = "file.edit")]
    FileEdit {
        tool_call_id: String,
        path: String,
        edit_kind: FileEditKind,
    },
    /// A shell command ran. `exit_code` is absent until it exits.
    #[serde(rename = "command.run")]
    CommandRun {
        tool_call_id: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        exit_code: Option<i32>,
    },
    /// The agent needs a decision before it can continue.
    #[serde(rename = "approval.request")]
    ApprovalRequest { request: ApprovalRequest },
    /// Token counts so far.
    #[serde(rename = "usage")]
    Usage { usage: TokenUsage },
    /// The turn is over.
    #[serde(rename = "turn.end")]
    TurnEnd { stop_reason: StopReason },
    /// The agent or its process failed.
    #[serde(rename = "error")]
    Error { message: String },
}

/// What kind of answer an approval option gives the agent. Hearth always
/// answers one-shot; "allow for thread" lives in Hearth's own store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// One choice the agent offered for an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalOption {
    /// The agent's id for this option, echoed back when answering.
    pub option_id: String,
    pub label: String,
    pub kind: ApprovalOptionKind,
}

/// An agent asking permission to use a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub tool: String,
    pub title: String,
    /// What exactly would run or change (a command line, a path).
    pub detail: String,
    pub options: Vec<ApprovalOption>,
}

/// The user's answer to an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    AllowOnce,
    /// Hearth remembers it for the thread; the agent still gets a one-shot answer.
    AllowForThread,
    Deny,
}

/// What the user sends to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind")]
pub enum UserInput {
    #[serde(rename = "text")]
    Text { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;
    use ts_rs::Config;

    fn sample_request() -> ApprovalRequest {
        ApprovalRequest {
            id: ApprovalId("0192f0aa-0000-7000-8000-000000000001".into()),
            tool: "run_shell_command".into(),
            title: "Run tests".into(),
            detail: "cargo test".into(),
            options: vec![ApprovalOption {
                option_id: "proceed_once".into(),
                label: "Allow once".into(),
                kind: ApprovalOptionKind::AllowOnce,
            }],
        }
    }

    // Fails if any AgentEvent variant serializes under a different `kind` tag
    // than the spec's, or does not deserialize back to the same value.
    #[test]
    fn agent_event_variants_round_trip_under_spec_tags() {
        let id = || "call-1".to_string();
        let cases: Vec<(&str, AgentEvent)> = vec![
            ("text.delta", AgentEvent::TextDelta { text: "hi".into() }),
            (
                "reasoning.delta",
                AgentEvent::ReasoningDelta { text: "hm".into() },
            ),
            (
                "tool.start",
                AgentEvent::ToolStart {
                    tool_call_id: id(),
                    tool: "read_file".into(),
                    tool_kind: ToolKind::Read,
                    title: "Read a.rs".into(),
                },
            ),
            (
                "tool.output",
                AgentEvent::ToolOutput {
                    tool_call_id: id(),
                    text: "out".into(),
                },
            ),
            (
                "tool.end",
                AgentEvent::ToolEnd {
                    tool_call_id: id(),
                    status: ToolStatus::Completed,
                },
            ),
            (
                "file.edit",
                AgentEvent::FileEdit {
                    tool_call_id: id(),
                    path: "src/a.rs".into(),
                    edit_kind: FileEditKind::Modify,
                },
            ),
            (
                "command.run",
                AgentEvent::CommandRun {
                    tool_call_id: id(),
                    command: "cargo test".into(),
                    exit_code: Some(0),
                },
            ),
            (
                "command.run",
                AgentEvent::CommandRun {
                    tool_call_id: id(),
                    command: "sleep 9".into(),
                    exit_code: None,
                },
            ),
            (
                "approval.request",
                AgentEvent::ApprovalRequest {
                    request: sample_request(),
                },
            ),
            (
                "usage",
                AgentEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: Some(3),
                        cache_write_tokens: None,
                    },
                },
            ),
            (
                "turn.end",
                AgentEvent::TurnEnd {
                    stop_reason: StopReason::EndTurn,
                },
            ),
            (
                "error",
                AgentEvent::Error {
                    message: "boom".into(),
                },
            ),
        ];
        for (tag, event) in cases {
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["kind"], tag, "tag for {event:?}");
            let back: AgentEvent = serde_json::from_value(json).unwrap();
            assert_eq!(back, event, "round trip for {tag}");
        }
    }

    fn read_dir_files(dir: &Path) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "ts") {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let body = std::fs::read_to_string(&path).unwrap();
                let tidy: String = body
                    .lines()
                    .map(|l| format!("{}\n", l.trim_end()))
                    .collect();
                out.insert(name, tidy);
            }
        }
        out
    }

    fn export_all(dir: &Path) {
        let cfg = Config::new().with_out_dir(dir);
        AgentId::export_all(&cfg).unwrap();
        Model::export_all(&cfg).unwrap();
        PermissionMode::export_all(&cfg).unwrap();
        AgentEvent::export_all(&cfg).unwrap();
        ApprovalDecision::export_all(&cfg).unwrap();
        UserInput::export_all(&cfg).unwrap();
        ProjectId::export_all(&cfg).unwrap();
        ThreadId::export_all(&cfg).unwrap();
        TurnId::export_all(&cfg).unwrap();
        AgentSessionId::export_all(&cfg).unwrap();
        EventId::export_all(&cfg).unwrap();
    }

    // Fails if the committed TypeScript differs from what the Rust types
    // generate (a stale file). Set HEARTH_UPDATE_TS=1 to rewrite it.
    #[test]
    fn generated_typescript_is_current() {
        let committed =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/protocol/src/generated");
        let tmp = std::env::temp_dir().join(format!("hearth-protocol-ts-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp).unwrap();
        export_all(&tmp);
        let fresh = read_dir_files(&tmp);
        if std::env::var_os("HEARTH_UPDATE_TS").is_some() {
            std::fs::create_dir_all(&committed).unwrap();
            for old in read_dir_files(&committed).into_keys() {
                std::fs::remove_file(committed.join(old)).unwrap();
            }
            for (name, body) in &fresh {
                std::fs::write(committed.join(name), body).unwrap();
            }
        }
        let on_disk = read_dir_files(&committed);
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(
            on_disk, fresh,
            "generated TypeScript is stale: run `HEARTH_UPDATE_TS=1 cargo test -p hearth-protocol generated_typescript_is_current`"
        );
    }
}
