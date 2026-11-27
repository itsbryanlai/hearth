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
