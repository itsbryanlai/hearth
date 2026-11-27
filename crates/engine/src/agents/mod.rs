//! Per-agent detection and spawn policy.

pub mod qwen;

use hearth_adapter_qwen::SpawnConfig;
use std::path::Path;

/// Finds an agent and decides exactly how it is started. Blocking: the
/// engine calls it on a blocking thread.
pub trait AgentLauncher: Send + Sync {
    /// `passthrough`: the names of the variables the user chose to hand to
    /// the agent (see `agents.qwen-code.envPassthrough`).
    fn spawn_config(&self, cwd: &Path, passthrough: &[String]) -> Result<SpawnConfig, String>;
}
