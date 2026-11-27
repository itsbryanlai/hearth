//! Qwen Code over the Agent Client Protocol (`qwen --acp`, JSON-RPC 2.0 on
//! stdio). See the Qwen integration decision record for the rules this
//! client keeps:
//!
//! - it never calls `authenticate` (there is no such method in this crate);
//! - `session/set_model` always carries `qwen.session.modelPersistDefault =
//!   false`;
//! - it declares no `fs` and no `terminal` capability;
//! - it answers every approval with a one-shot option, never an "always" one;
//! - the masked key in a `set_model` response is dropped, never returned.

mod client;
mod mapping;

pub use client::{
    AdapterError, AdapterItem, AdapterStream, AgentInfo, LoadedSession, QwenProcess, SessionModels,
    SpawnConfig,
};
