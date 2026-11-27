//! Merges runs of streaming deltas so one batch is one event per run.

use crate::agents::AgentItem;
use hearth_core::store::NewEvent;
use hearth_protocol::AgentEvent;
use serde_json::Value;

/// Adjacent text deltas, adjacent reasoning deltas and adjacent output of one
/// tool call become one event with the texts joined. A merged event keeps
/// every source payload as an array in `raw`.
pub(crate) fn coalesce(items: Vec<AgentItem>) -> Vec<NewEvent> {
    let mut out: Vec<(AgentEvent, Vec<Value>)> = Vec::new();
    for item in items {
        if let Some((last, raws)) = out.last_mut() {
            let merged = match (last, &item.event) {
                (AgentEvent::TextDelta { text }, AgentEvent::TextDelta { text: more })
                | (
                    AgentEvent::ReasoningDelta { text },
                    AgentEvent::ReasoningDelta { text: more },
                ) => {
                    text.push_str(more);
                    true
                }
                (
                    AgentEvent::ToolOutput { tool_call_id, text },
                    AgentEvent::ToolOutput {
                        tool_call_id: other,
                        text: more,
                    },
                ) if tool_call_id == other => {
                    text.push_str(more);
                    true
                }
                _ => false,
            };
            if merged {
                raws.push(item.raw);
                continue;
            }
        }
        out.push((item.event, vec![item.raw]));
    }
    out.into_iter()
        .map(|(event, mut raws)| NewEvent {
            event,
            raw: Some(if raws.len() == 1 {
                raws.remove(0)
            } else {
                Value::Array(raws)
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_protocol::StopReason;
    use serde_json::json;

    fn item(event: AgentEvent) -> AgentItem {
        AgentItem {
            event,
            raw: json!({}),
        }
    }
    fn text(t: &str) -> AgentItem {
        item(AgentEvent::TextDelta { text: t.into() })
    }
    fn thought(t: &str) -> AgentItem {
        item(AgentEvent::ReasoningDelta { text: t.into() })
    }

    // Fails if a run of deltas is not merged, if text is lost or reordered,
    // or if a different kind of event is merged into a delta run.
    #[test]
    fn delta_runs_merge_and_keep_every_character_in_order() {
        let items = vec![
            thought("a"),
            thought("b"),
            text("c"),
            text("d"),
            text("e"),
            thought("f"),
            item(AgentEvent::TurnEnd {
                stop_reason: StopReason::EndTurn,
            }),
        ];
        let events: Vec<AgentEvent> = coalesce(items).into_iter().map(|e| e.event).collect();
        assert_eq!(
            events,
            vec![
                AgentEvent::ReasoningDelta { text: "ab".into() },
                AgentEvent::TextDelta { text: "cde".into() },
                AgentEvent::ReasoningDelta { text: "f".into() },
                AgentEvent::TurnEnd {
                    stop_reason: StopReason::EndTurn
                },
            ]
        );
    }
}
