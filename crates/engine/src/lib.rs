//! Hearth's engine: owns the store, the file-access guard and one live agent
//! session per thread, and exposes the UI's operations. It has no UI
//! framework dependency; the app supplies an [`EventSink`].

pub mod agents;
mod api;
mod coalesce;
mod engine;

pub use api::{
    AgentStatus, EngineError, EventSink, InstalledAgent, Project, ReviewTurn, ThreadEventPayload,
    ThreadModels, ThreadSnapshot, ThreadSummary, TurnSnapshot, WorktreeInfo,
};
pub use engine::{Engine, EngineConfig};

pub use hearth_core::snapshot::{DiffFile, DiffHunk, DiffRow, DiffRowKind, FileDiff, SplitRow};
