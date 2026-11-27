//! `session/update` and `session/request_permission` to `AgentEvent`.

use hearth_protocol::{
    AgentEvent, ApprovalId, ApprovalOption, ApprovalOptionKind, ApprovalRequest, FileEditKind,
    StopReason, TokenUsage, ToolKind, ToolStatus,
};
use serde_json::Value;
use std::collections::HashSet;

/// Per-turn state the mapping needs (which commands were already announced).
#[derive(Default)]
pub struct Mapper {
    commands_seen: HashSet<String>,
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str()
}

fn u32_at(v: &Value, key: &str) -> Option<u32> {
    v.get(key)?.as_u64().map(|n| n.min(u32::MAX as u64) as u32)
}

pub fn tool_kind(kind: Option<&str>) -> ToolKind {
    match kind {
        Some("read") => ToolKind::Read,
        Some("edit") => ToolKind::Edit,
        Some("delete") => ToolKind::Delete,
        Some("move") => ToolKind::Move,
        Some("search") => ToolKind::Search,
        Some("execute") => ToolKind::Execute,
        Some("think") => ToolKind::Think,
        Some("fetch") => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

pub fn stop_reason(s: &str) -> StopReason {
    match s {
        "max_tokens" => StopReason::MaxTokens,
        "max_turn_requests" => StopReason::MaxTurnRequests,
        "refusal" => StopReason::Refusal,
        "cancelled" => StopReason::Cancelled,
        _ => StopReason::EndTurn,
    }
}

fn option_kind(s: &str) -> Option<ApprovalOptionKind> {
    match s {
        "allow_once" => Some(ApprovalOptionKind::AllowOnce),
        "allow_always" => Some(ApprovalOptionKind::AllowAlways),
        "reject_once" => Some(ApprovalOptionKind::RejectOnce),
        "reject_always" => Some(ApprovalOptionKind::RejectAlways),
        _ => None,
    }
}

impl Mapper {
    /// Map the `params` of one `session/update` notification. One update can
    /// yield several events (a finished edit gives output, a file edit and the
    /// end), or none (lifecycle markers, command lists).
    pub fn update(&mut self, params: &Value) -> Vec<AgentEvent> {
        let Some(update) = params.get("update") else {
            return vec![];
        };
        let mut out = Vec::new();
        match str_at(update, &["sessionUpdate"]) {
            Some("agent_message_chunk") => {
                if let Some(text) = non_empty_text(update) {
                    out.push(AgentEvent::TextDelta { text });
                }
                if let Some(u) = update.get("_meta").and_then(|m| m.get("usage"))
                    && let (Some(i), Some(o)) =
                        (u32_at(u, "inputTokens"), u32_at(u, "outputTokens"))
                {
                    out.push(AgentEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: i,
                            output_tokens: o,
                            cache_read_tokens: u32_at(u, "cachedReadTokens"),
                            cache_write_tokens: None,
                        },
                    });
                }
            }
            Some("agent_thought_chunk") => {
                if let Some(text) = non_empty_text(update) {
                    out.push(AgentEvent::ReasoningDelta { text });
                }
            }
            Some("tool_call") => {
                let id = str_at(update, &["toolCallId"])
                    .unwrap_or_default()
                    .to_string();
                let title = str_at(update, &["title"]).unwrap_or_default().to_string();
                let tool = str_at(update, &["_meta", "toolName"])
                    .unwrap_or(&title)
                    .to_string();
                out.push(AgentEvent::ToolStart {
                    tool_call_id: id,
                    tool,
                    tool_kind: tool_kind(str_at(update, &["kind"])),
                    title,
                });
            }
            Some("tool_call_update") => self.tool_update(update, &mut out),
            _ => {}
        }
        out
    }

    fn tool_update(&mut self, update: &Value, out: &mut Vec<AgentEvent>) {
        let id = str_at(update, &["toolCallId"])
            .unwrap_or_default()
            .to_string();
        let kind = str_at(update, &["kind"]);
        if kind == Some("execute")
            && let Some(cmd) = str_at(update, &["rawInput", "command"])
            && self.commands_seen.insert(id.clone())
        {
            out.push(AgentEvent::CommandRun {
                tool_call_id: id.clone(),
                command: cmd.to_string(),
                exit_code: None,
            });
        }
        let status = str_at(update, &["status"]);
        let done = matches!(status, Some("completed" | "failed"));
        if let Some(content) = update.get("content").and_then(Value::as_array) {
            for c in content {
                match str_at(c, &["type"]) {
                    Some("diff") if done => {
                        let old_empty = c
                            .get("oldText")
                            .map(|o| o.is_null() || o.as_str() == Some(""))
                            .unwrap_or(true);
                        let edit_kind = if kind == Some("delete") {
                            FileEditKind::Delete
                        } else if old_empty {
                            FileEditKind::Create
                        } else {
                            FileEditKind::Modify
                        };
                        out.push(AgentEvent::FileEdit {
                            tool_call_id: id.clone(),
                            path: str_at(c, &["path"]).unwrap_or_default().to_string(),
                            edit_kind,
                        });
                    }
                    Some("content") => {
                        if let Some(t) = str_at(c, &["content", "text"])
                            && !t.is_empty()
                        {
                            out.push(AgentEvent::ToolOutput {
                                tool_call_id: id.clone(),
                                text: t.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        if done {
            self.commands_seen.remove(&id);
            out.push(AgentEvent::ToolEnd {
                tool_call_id: id,
                status: if status == Some("failed") {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                },
            });
        }
    }
}

fn non_empty_text(update: &Value) -> Option<String> {
    let t = str_at(update, &["content", "text"])?;
    (!t.is_empty()).then(|| t.to_string())
}

/// What the client needs to answer a `session/request_permission` later.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub rpc_id: Value,
    pub allow_once: Option<String>,
    pub reject_once: Option<String>,
}

/// Map a `session/request_permission` request. Only one-shot options are
/// offered to Hearth; the agent's "always" options stay in the raw event.
pub fn permission(
    params: &Value,
    id: ApprovalId,
    rpc_id: Value,
) -> (ApprovalRequest, PendingPermission) {
    let call = params.get("toolCall").cloned().unwrap_or(Value::Null);
    let title = str_at(&call, &["title"]).unwrap_or_default().to_string();
    let tool = str_at(&call, &["_meta", "toolName"])
        .or_else(|| str_at(&call, &["kind"]))
        .unwrap_or("tool")
        .to_string();
    let detail = str_at(&call, &["rawInput", "command"])
        .or_else(|| {
            call.get("content")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find_map(|c| str_at(c, &["path"])))
        })
        .or_else(|| str_at(&call, &["rawInput", "file_path"]))
        .unwrap_or(&title)
        .to_string();
    let mut options = Vec::new();
    let mut pending = PendingPermission {
        rpc_id,
        allow_once: None,
        reject_once: None,
    };
    for o in params
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(oid), Some(kind)) = (
            str_at(o, &["optionId"]),
            str_at(o, &["kind"]).and_then(option_kind),
        ) else {
            continue;
        };
        match kind {
            ApprovalOptionKind::AllowOnce if pending.allow_once.is_none() => {
                pending.allow_once = Some(oid.to_string())
            }
            ApprovalOptionKind::RejectOnce if pending.reject_once.is_none() => {
                pending.reject_once = Some(oid.to_string())
            }
            _ => continue,
        }
        options.push(ApprovalOption {
            option_id: oid.to_string(),
            label: str_at(o, &["name"]).unwrap_or_default().to_string(),
            kind,
        });
    }
    (
        ApprovalRequest {
            id,
            tool,
            title,
            detail,
            options,
        },
        pending,
    )
}
