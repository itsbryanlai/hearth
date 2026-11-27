use crate::{Item, ModelChoice};
use hearth_protocol::{
    AgentEvent, ApprovalId, ApprovalOption, ApprovalOptionKind, ApprovalRequest, FileEditKind,
    StopReason, TokenUsage, ToolKind, ToolStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::{Component, Path},
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Envelope {
    ControlResponse {
        response: ControlResponse,
    },
    ControlRequest {
        request_id: String,
        request: ControlRequest,
    },
    System {
        subtype: String,
        session_id: Option<String>,
        model: Option<String>,
        analytics_disabled: Option<bool>,
    },
    StreamEvent {
        api_message_id: Option<String>,
        event: StreamFrame,
    },
    Assistant {
        message: AssistantMessage,
    },
    User {
        message: UserMessage,
    },
    Result {
        subtype: String,
        stop_reason: Option<String>,
        terminal_reason: Option<String>,
        is_error: Option<bool>,
        api_error_status: Option<u16>,
        usage: Option<Usage>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(crate) struct ControlResponse {
    request_id: String,
    subtype: String,
    response: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelRow {
    value: String,
    display_name: String,
    resolved_model: Option<String>,
}

impl ControlResponse {
    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn succeeded(&self) -> bool {
        self.subtype == "success"
    }

    pub(crate) fn models(self) -> Option<Vec<ModelChoice>> {
        let rows = self.response?.as_object_mut()?.remove("models")?;
        let models: Vec<ModelRow> = serde_json::from_value(rows).ok()?;
        Some(
            models
                .into_iter()
                .filter_map(|row| {
                    Some(ModelChoice {
                        value: identifier(row.value)?,
                        label: redact_protected_paths(&row.display_name),
                        resolved_id: row.resolved_model.and_then(identifier),
                    })
                })
                .collect(),
        )
    }

    pub(crate) fn model(self) -> Option<String> {
        let Value::String(model) = self.response?.as_object_mut()?.remove("model")? else {
            return None;
        };
        identifier(model)
    }
}

#[derive(Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(crate) enum ControlRequest {
    CanUseTool {
        tool_name: String,
        tool_use_id: Option<String>,
        input: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamFrame {
    MessageStart {
        message: MessageId,
    },
    ContentBlockStart {
        index: usize,
        content_block: AssistantBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: BlockChange,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(crate) struct MessageId {
    id: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BlockChange {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(crate) struct AssistantMessage {
    id: String,
    content: Vec<AssistantBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AssistantBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Option<Value>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(crate) struct UserMessage {
    content: UserContent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UserContent {
    Blocks(Vec<UserBlock>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserBlock {
    ToolResult {
        tool_use_id: String,
        content: Option<OutputContent>,
        is_error: Option<bool>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OutputContent {
    Text(String),
    Blocks(Vec<OutputBlock>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(crate) struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    cache_read_input_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
}

#[derive(Default)]
pub(crate) struct TurnDecoder {
    last_message: Option<String>,
    messages: HashMap<String, MessageBlocks>,
    started_tools: HashSet<String>,
    tool_effects: HashMap<String, AgentEvent>,
    protected_tools: HashSet<String>,
}

#[derive(Default)]
struct MessageBlocks {
    emitted: HashSet<usize>,
    next_final_index: usize,
}

impl TurnDecoder {
    pub(crate) fn decode(
        &mut self,
        envelope: Envelope,
        cwd: &Path,
        approvals: &mut HashMap<String, Option<Value>>,
        interrupted: bool,
    ) -> Vec<Item> {
        let mut items = Vec::new();
        match envelope {
            Envelope::System {
                subtype,
                session_id,
                model,
                analytics_disabled,
            } if subtype == "init" => {
                if let (Some(id), Some(model)) =
                    (session_id.and_then(identifier), model.and_then(identifier))
                {
                    items.push(Item::Session {
                        id,
                        model,
                        analytics_disabled,
                    });
                }
            }
            Envelope::ControlRequest {
                request_id,
                request:
                    ControlRequest::CanUseTool {
                        tool_name,
                        tool_use_id,
                        input,
                    },
            } => {
                let (Some(id), Some(tool)) = (identifier(request_id), identifier(tool_name)) else {
                    return items;
                };
                if let Some(tool_use_id) = tool_use_id.and_then(identifier) {
                    self.record_protected_tool(&tool_use_id, &tool, &input);
                }
                let detail = tool_detail(&tool, &input, cwd);
                if let Entry::Vacant(approval) = approvals.entry(id.clone()) {
                    approval.insert(Some(input));
                    event(
                        &mut items,
                        "control_request",
                        AgentEvent::ApprovalRequest {
                            request: ApprovalRequest {
                                id: ApprovalId(id),
                                title: tool.clone(),
                                detail,
                                tool,
                                options: vec![
                                    ApprovalOption {
                                        option_id: "allow".into(),
                                        label: "Allow".into(),
                                        kind: ApprovalOptionKind::AllowOnce,
                                    },
                                    ApprovalOption {
                                        option_id: "deny".into(),
                                        label: "Deny".into(),
                                        kind: ApprovalOptionKind::RejectOnce,
                                    },
                                ],
                            },
                        },
                    );
                }
            }
            Envelope::StreamEvent {
                api_message_id,
                event: frame,
            } => match frame {
                StreamFrame::MessageStart { message } => self.last_message = Some(message.id),
                StreamFrame::ContentBlockStart {
                    index,
                    content_block,
                } => {
                    self.stream_block(api_message_id, index, content_block, &mut items, cwd);
                }
                StreamFrame::ContentBlockDelta { index, delta } => {
                    let block = match delta {
                        BlockChange::TextDelta { text } => AssistantBlock::Text { text },
                        BlockChange::ThinkingDelta { thinking } => {
                            AssistantBlock::Thinking { thinking }
                        }
                        BlockChange::Unknown => return items,
                    };
                    self.stream_block(api_message_id, index, block, &mut items, cwd);
                }
                StreamFrame::Unknown => items.push(observation("stream_event")),
            },
            Envelope::Assistant { message } => {
                let mut blocks = self.messages.remove(&message.id).unwrap_or_default();
                // Claude emits final content as either the complete message or
                // consecutive single-block frames with the same message ID.
                let start = if message.content.len() == 1 {
                    blocks.next_final_index
                } else {
                    0
                };
                blocks.next_final_index =
                    blocks.next_final_index.max(start + message.content.len());
                for (index, block) in (start..).zip(message.content) {
                    if blocks.emitted.insert(index) {
                        self.assistant_block(block, "assistant", &mut items, cwd);
                    }
                }
                self.messages.insert(message.id, blocks);
            }
            Envelope::User {
                message:
                    UserMessage {
                        content: UserContent::Blocks(blocks),
                    },
            } => {
                for block in blocks {
                    if let UserBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = block
                        && let Some(id) = identifier(tool_use_id)
                    {
                        if !self.protected_tools.remove(&id)
                            && let Some(text) = output_text(content)
                            && !text.is_empty()
                        {
                            event(
                                &mut items,
                                "user",
                                AgentEvent::ToolOutput {
                                    tool_call_id: id.clone(),
                                    text,
                                },
                            );
                        }
                        if let Some(effect) = self.tool_effects.remove(&id)
                            && is_error != Some(true)
                        {
                            event(&mut items, "user", effect);
                        }
                        event(
                            &mut items,
                            "user",
                            AgentEvent::ToolEnd {
                                tool_call_id: id,
                                status: if is_error == Some(true) {
                                    ToolStatus::Failed
                                } else {
                                    ToolStatus::Completed
                                },
                            },
                        );
                    }
                }
            }
            Envelope::Result {
                subtype,
                stop_reason,
                terminal_reason,
                is_error,
                api_error_status,
                usage,
            } => {
                if let Some(usage) = usage {
                    event(
                        &mut items,
                        "result",
                        AgentEvent::Usage {
                            usage: TokenUsage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_read_tokens: usage.cache_read_input_tokens,
                                cache_write_tokens: usage.cache_creation_input_tokens,
                            },
                        },
                    );
                }
                event(
                    &mut items,
                    "result",
                    finish(
                        &subtype,
                        stop_reason.as_deref(),
                        terminal_reason.as_deref(),
                        is_error == Some(true),
                        api_error_status,
                        interrupted,
                    ),
                );
            }
            Envelope::ControlRequest { .. } => items.push(observation("control_request")),
            Envelope::System { .. } => items.push(observation("system")),
            Envelope::Unknown => items.push(observation("unknown")),
            Envelope::User { .. } | Envelope::ControlResponse { .. } => {}
        }
        items
    }

    fn stream_block(
        &mut self,
        message_id: Option<String>,
        index: usize,
        block: AssistantBlock,
        output: &mut Vec<Item>,
        cwd: &Path,
    ) {
        let streamed_text = match &block {
            AssistantBlock::Text { text } => !text.is_empty(),
            AssistantBlock::Thinking { thinking } => !thinking.is_empty(),
            _ => false,
        };
        if streamed_text && let Some(id) = message_id.or_else(|| self.last_message.clone()) {
            self.messages.entry(id).or_default().emitted.insert(index);
        }
        self.assistant_block(block, "stream_event", output, cwd);
    }

    fn assistant_block(
        &mut self,
        block: AssistantBlock,
        source: &'static str,
        output: &mut Vec<Item>,
        cwd: &Path,
    ) {
        match block {
            AssistantBlock::Text { text } if !text.is_empty() => {
                event(output, source, AgentEvent::TextDelta { text });
            }
            AssistantBlock::Thinking { thinking } if !thinking.is_empty() => {
                event(
                    output,
                    source,
                    AgentEvent::ReasoningDelta { text: thinking },
                );
            }
            AssistantBlock::ToolUse { id, name, input } => {
                let (Some(id), Some(tool)) = (identifier(id), identifier(name)) else {
                    return;
                };
                if self.started_tools.insert(id.clone()) {
                    event(
                        output,
                        source,
                        AgentEvent::ToolStart {
                            tool_call_id: id.clone(),
                            title: tool.clone(),
                            tool_kind: tool_kind(&tool),
                            tool: tool.clone(),
                        },
                    );
                }
                if let Some(input) = &input {
                    self.record_protected_tool(&id, &tool, input);
                }
                if source == "assistant"
                    && let Some(input) = input
                    && !self.protected_tools.contains(&id)
                {
                    match tool.as_str() {
                        "Edit" | "Write" | "NotebookEdit" => {
                            if let Some(path) =
                                file_target(&tool, &input).and_then(|path| display_path(path, cwd))
                            {
                                self.tool_effects.insert(
                                    id.clone(),
                                    AgentEvent::FileEdit {
                                        tool_call_id: id,
                                        path,
                                        edit_kind: FileEditKind::Modify,
                                    },
                                );
                            }
                        }
                        "Bash" => {
                            if let Some(command) = input["command"].as_str() {
                                self.tool_effects.insert(
                                    id.clone(),
                                    AgentEvent::CommandRun {
                                        tool_call_id: id,
                                        command: redact_protected_paths(command),
                                        exit_code: None,
                                    },
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    fn record_protected_tool(&mut self, id: &str, tool: &str, input: &Value) {
        if let Some(path) = file_target(tool, input)
            && protected_path(Path::new(path))
        {
            self.protected_tools.insert(id.to_owned());
            self.tool_effects.remove(id);
        }
    }
}

fn finish(
    subtype: &str,
    stop_reason: Option<&str>,
    terminal_reason: Option<&str>,
    is_error: bool,
    api_error_status: Option<u16>,
    interrupted: bool,
) -> AgentEvent {
    let stop_reason = match (subtype, stop_reason) {
        ("error_during_execution", _)
            if interrupted && terminal_reason == Some("aborted_streaming") =>
        {
            StopReason::Cancelled
        }
        ("error_max_turns", _) => StopReason::MaxTurnRequests,
        ("success", Some("max_tokens")) if !is_error => StopReason::MaxTokens,
        ("success", Some("refusal")) if !is_error => StopReason::Refusal,
        ("success", _) if !is_error => StopReason::EndTurn,
        _ => {
            return AgentEvent::Error {
                message: if matches!(api_error_status, Some(401 | 403)) {
                    "Claude Code could not authenticate. Run `claude` in your terminal to sign in."
                        .into()
                } else {
                    "Claude Code reported a turn error.".into()
                },
            };
        }
    };
    AgentEvent::TurnEnd { stop_reason }
}

fn output_text(content: Option<OutputContent>) -> Option<String> {
    match content? {
        OutputContent::Text(text) => Some(text),
        OutputContent::Blocks(blocks) => Some(
            blocks
                .into_iter()
                .filter_map(|block| match block {
                    OutputBlock::Text { text } => Some(text),
                    OutputBlock::Other => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        OutputContent::Other(_) => None,
    }
}

fn tool_kind(tool: &str) -> ToolKind {
    match tool {
        "Read" => ToolKind::Read,
        "Edit" | "Write" | "NotebookEdit" => ToolKind::Edit,
        "Bash" => ToolKind::Execute,
        "Glob" | "Grep" | "WebSearch" => ToolKind::Search,
        "WebFetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn tool_detail(tool: &str, input: &Value, cwd: &Path) -> String {
    match tool {
        "Read" | "Write" | "Edit" | "NotebookEdit" => file_target(tool, input)
            .and_then(|path| display_path(path, cwd))
            .unwrap_or_else(|| "[path omitted]".into()),
        "Bash" => input["command"]
            .as_str()
            .map(redact_protected_paths)
            .unwrap_or_else(|| "[command omitted]".into()),
        "Glob" | "Grep" => input["pattern"]
            .as_str()
            .map(redact_protected_paths)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn file_target<'a>(tool: &str, input: &'a Value) -> Option<&'a str> {
    let key = match tool {
        "Read" | "Write" | "Edit" => "file_path",
        "NotebookEdit" => "notebook_path",
        _ => return None,
    };
    input[key].as_str()
}

fn identifier(value: String) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'[' | b']')
        }))
    .then_some(value)
}

fn display_path(path: &str, cwd: &Path) -> Option<String> {
    let path = Path::new(path);
    if protected_path(path) {
        return None;
    }
    Some(
        path.strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned(),
    )
}

fn redact_protected_paths(text: &str) -> String {
    let has_protected_path = text
        .split(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '\'' | '"' | '\x60' | '(' | ')' | '=' | ';' | '&' | '|' | '<' | '>'
                )
        })
        .any(|word| protected_path(Path::new(word)));
    if has_protected_path {
        "[protected path omitted]".into()
    } else {
        text.into()
    }
}

// Tool input may use absolute paths, shell home prefixes, or paths relative
// to the working directory. Match protected names without resolving symlinks
// or asking the filesystem about a tool's target.
fn protected_path(path: &Path) -> bool {
    let mut parts = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .peekable();
    while let Some(part) = parts.next() {
        if part
            .get(0..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(".claude"))
            || [".qwen", ".codex", ".ssh"]
                .iter()
                .any(|name| part.eq_ignore_ascii_case(name))
            || (part.eq_ignore_ascii_case(".config")
                && parts
                    .peek()
                    .is_some_and(|name| name.eq_ignore_ascii_case("gh")))
            || (part.eq_ignore_ascii_case("Library")
                && parts
                    .peek()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Keychains")))
        {
            return true;
        }
    }
    false
}

fn event(output: &mut Vec<Item>, source: &'static str, event: AgentEvent) {
    output.push(Item::Event {
        event,
        raw: json!({"type":source}),
    });
}

fn observation(source: &'static str) -> Item {
    Item::Observation {
        raw: json!({"type":source}),
    }
}
