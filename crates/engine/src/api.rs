//! The engine's public types. Field names match `apps/desktop/src/ipc/contract.ts`.

use hearth_core::store::StoreError;
use hearth_protocol::{AgentEvent, AgentId, ProjectId, ThreadId, TurnId, UserInput};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub agent: AgentId,
    pub model: String,
    pub branch: Option<String>,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub is_git: bool,
    pub base: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AgentStatus {
    Ready,
    Missing,
    Unsupported { version: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledAgent {
    pub id: AgentId,
    pub label: &'static str,
    pub status: AgentStatus,
    pub remembers_approvals: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
    pub resolved_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadModels {
    pub choices: Vec<ModelChoice>,
    pub selected: Option<String>,
    pub current: Option<String>,
}

/// An assistant turn available for review, including turns without retained snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewTurn {
    pub turn_id: TurnId,
    pub label: String,
    pub available: bool,
}

/// An editable hand-off preamble for switching a thread to another agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffDraft {
    pub thread_id: ThreadId,
    pub source_agent: AgentId,
    pub target_agent: AgentId,
    pub text: String,
}

/// A captured review of a thread's current working copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeReview {
    pub id: String,
    pub files: Vec<hearth_core::snapshot::DiffFile>,
}

/// One user message and the agent events it produced, in order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSnapshot {
    pub turn_id: TurnId,
    pub agent: AgentId,
    pub model: String,
    pub input: UserInput,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSnapshot {
    pub thread: ThreadSummary,
    pub turns: Vec<TurnSnapshot>,
    /// The `seq` of the newest event in `turns`; the UI drops live events at or below it.
    pub last_seq: u64,
}

/// Payload of the `thread-event` channel. `seq` increases per thread.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEventPayload {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub event: AgentEvent,
}

/// Where live events go (the app turns them into `thread-event` emits).
pub trait EventSink: Send + Sync {
    fn emit(&self, payload: ThreadEventPayload);
}

/// A message safe to show the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError(pub String);

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

impl From<StoreError> for EngineError {
    fn from(e: StoreError) -> Self {
        Self(e.to_string())
    }
}

impl From<&str> for EngineError {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for EngineError {
    fn from(s: String) -> Self {
        Self(s)
    }
}
