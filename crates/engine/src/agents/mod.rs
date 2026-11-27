//! Per-agent detection and spawn policy.

pub mod claude;
pub mod qwen;

use crate::api::AgentStatus;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use hearth_protocol::{AgentEvent, AgentId, ApprovalDecision, ApprovalRequest, UserInput};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type AgentFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct AgentItem {
    pub event: AgentEvent,
    pub raw: Value,
}

pub enum StreamItem {
    Event(AgentItem),
    Session { external_id: String, model: String },
    Failed(String),
}

pub trait AgentStream: Send {
    fn poll_next(&mut self, cx: &mut std::task::Context<'_>)
    -> std::task::Poll<Option<StreamItem>>;
}

pub struct Registration {
    pub id: AgentId,
    pub label: &'static str,
    pub adapter: Arc<dyn AgentAdapter>,
}

pub struct EnvPassthrough {
    pub key: &'static str,
    pub defaults: &'static [&'static str],
}

pub struct OpenSession {
    pub session: Arc<dyn AgentSession>,
    pub external_id: Option<String>,
    pub model: Option<String>,
}

pub trait AgentAdapter: Send + Sync {
    fn availability(&self) -> AgentFuture<'_, AgentStatus>;

    fn passthrough(&self) -> Option<EnvPassthrough> {
        None
    }

    fn remembers_approvals(&self) -> bool {
        false
    }

    fn open_session<'a>(
        &'a self,
        cwd: &'a Path,
        resume_id: Option<&'a str>,
        passthrough: &'a [String],
    ) -> AgentFuture<'a, Result<OpenSession, String>>;
}

pub trait AgentSession: Send + Sync {
    fn prompt(&self, input: UserInput) -> AgentFuture<'_, Result<Box<dyn AgentStream>, String>>;

    fn reopen_after_forced_end(&self) -> bool {
        false
    }

    fn turn_completed(&self);
    fn answer_approval<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        decision: ApprovalDecision,
    ) -> AgentFuture<'a, Result<(), String>>;
    fn cancel(&self) -> AgentFuture<'_, Result<(), String>>;
}

fn login_shell_output(shell: &Path, home: &Path, script: &str) -> Option<String> {
    let mut cmd = Command::new(shell);
    cmd.args(["-lc", script])
        .env_clear()
        .env("HOME", home)
        .env("SHELL", shell);
    for name in ["USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(name) {
            cmd.env(name, v);
        }
    }
    run_limited(cmd, Duration::from_secs(10))
}

/// Runs a command with a time limit; stdout on success, `None` otherwise.
fn run_limited(cmd: Command, limit: Duration) -> Option<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async move {
        let mut command = tokio::process::Command::from(cmd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(limit, command.output())
            .await
            .ok()?
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()
    })
}
