//! Per-agent detection and spawn policy.

pub mod qwen;

use std::path::Path;

use hearth_protocol::{AgentEvent, ApprovalDecision, ApprovalRequest, UserInput};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type AgentFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct AgentItem {
    pub event: AgentEvent,
    pub raw: Value,
}

pub trait AgentStream: Send {
    fn poll_next(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<AgentItem>>;
}

pub struct OpenSession {
    pub session: Arc<dyn AgentSession>,
    pub external_id: Option<String>,
    pub model: Option<String>,
}

pub trait AgentAdapter: Send + Sync {
    fn open_session<'a>(
        &'a self,
        cwd: &'a Path,
        resume_id: Option<&'a str>,
        passthrough: &'a [String],
    ) -> AgentFuture<'a, Result<OpenSession, String>>;
}

pub trait AgentSession: Send + Sync {
    fn prompt(&self, input: UserInput) -> Box<dyn AgentStream>;
    fn turn_completed(&self);
    fn answer_approval<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        decision: ApprovalDecision,
    ) -> AgentFuture<'a, Result<(), String>>;
    fn cancel(&self) -> AgentFuture<'_, Result<(), String>>;
}
