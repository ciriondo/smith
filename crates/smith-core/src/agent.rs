//! The agent loop.
//!
//! `run_turn` consumes one provider stream and assembles the assistant message.
//! `TurnAccumulator` owns the accumulation logic, including the fragmented tool-call
//! arguments. Tool execution is not implemented here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::message::{ContentBlock, Message, Role, StopReason, Usage};
use crate::provider::{ChatRequest, Provider, ProviderEvent, ProviderStream};

/// A tool call assembled from stream fragments.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    /// Raw JSON string, concatenated from `args_delta` fragments.
    args: String,
}

/// Arguments that failed to parse, kept so the loop can report them to the model as a
/// failed tool result instead of aborting the turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolArgError {
    pub tool: String,
    pub raw: String,
    pub message: String,
}

#[derive(Debug)]
pub struct TurnResult {
    pub message: Message,
    pub usage: Usage,
    /// Reasoning tokens, if the provider emitted any. Not part of the transcript.
    pub reasoning: String,
    pub stop_reason: StopReason,
    pub tool_arg_errors: Vec<ToolArgError>,
}

/// Accumulates provider events into a completed assistant message.
#[derive(Default)]
pub struct TurnAccumulator {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<u32, PartialToolCall>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl TurnAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &ProviderEvent) {
        match event {
            ProviderEvent::TextDelta(s) => self.text.push_str(s),
            ProviderEvent::ReasoningDelta(s) => self.reasoning.push_str(s),
            ProviderEvent::ToolCallDelta {
                index,
                id,
                name,
                args_delta,
            } => {
                let entry = self.tool_calls.entry(*index).or_default();
                if let Some(id) = id {
                    entry.id = id.clone();
                }
                if let Some(name) = name {
                    entry.name = name.clone();
                }
                entry.args.push_str(args_delta);
            }
            ProviderEvent::Usage(u) => self.usage = *u,
            ProviderEvent::Done { stop_reason } => self.stop_reason = Some(stop_reason.clone()),
        }
    }

    /// Finish the turn, parsing accumulated tool-call arguments.
    pub fn finish(self) -> TurnResult {
        let mut errors = Vec::new();
        let mut content = Vec::new();

        if !self.text.is_empty() {
            content.push(ContentBlock::Text { text: self.text });
        }

        for (_, call) in self.tool_calls {
            // An empty argument string is legal and means "no arguments".
            let parsed = if call.args.trim().is_empty() {
                Ok(serde_json::Value::Object(Default::default()))
            } else {
                serde_json::from_str(&call.args)
            };

            match parsed {
                Ok(args) => content.push(ContentBlock::ToolCall {
                    id: call.id,
                    name: call.name,
                    args,
                }),
                Err(e) => {
                    errors.push(ToolArgError {
                        tool: call.name.clone(),
                        raw: call.args.clone(),
                        message: e.to_string(),
                    });
                    // Keep the call in the transcript with null args so the model can see
                    // what it asked for when the loop reports the failure back to it.
                    content.push(ContentBlock::ToolCall {
                        id: call.id,
                        name: call.name,
                        args: serde_json::Value::Null,
                    });
                }
            }
        }

        // A provider that streamed tool calls but never set finish_reason still means
        // "tool use". Infer it rather than defaulting to EndTurn and dropping the tools.
        let stop_reason = self.stop_reason.unwrap_or_else(|| {
            if content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
            {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        });

        TurnResult {
            message: Message {
                role: Role::Assistant,
                content,
            },
            usage: self.usage,
            reasoning: self.reasoning,
            stop_reason,
            tool_arg_errors: errors,
        }
    }
}

/// Run one turn, invoking `on_event` for every event as it arrives.
///
/// `on_event` receives each event as it arrives. Frontends use it to render or forward
/// events incrementally. It must not block.
pub async fn run_turn(
    provider: &dyn Provider,
    req: ChatRequest,
    cancel: CancellationToken,
    on_event: &mut (dyn FnMut(&ProviderEvent) + Send),
) -> Result<TurnResult, ProviderError> {
    use futures_util::StreamExt;

    let mut stream: ProviderStream = provider.stream(req, cancel.clone());
    let mut acc = TurnAccumulator::new();

    while let Some(item) = stream.next().await {
        let event = item?;
        acc.apply(&event);
        on_event(&event);
    }

    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }

    Ok(acc.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(index: u32, id: Option<&str>, name: Option<&str>, args: &str) -> ProviderEvent {
        ProviderEvent::ToolCallDelta {
            index,
            id: id.map(str::to_string),
            name: name.map(str::to_string),
            args_delta: args.to_string(),
        }
    }

    #[test]
    fn text_deltas_concatenate_in_order() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&ProviderEvent::TextDelta("Hello".into()));
        acc.apply(&ProviderEvent::TextDelta(", ".into()));
        acc.apply(&ProviderEvent::TextDelta("world".into()));
        acc.apply(&ProviderEvent::Done {
            stop_reason: StopReason::EndTurn,
        });

        let turn = acc.finish();
        assert_eq!(turn.message.text(), "Hello, world");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn reasoning_is_kept_out_of_the_transcript() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&ProviderEvent::ReasoningDelta("thinking".into()));
        acc.apply(&ProviderEvent::TextDelta("ok".into()));
        acc.apply(&ProviderEvent::Done {
            stop_reason: StopReason::EndTurn,
        });

        let turn = acc.finish();
        assert_eq!(turn.message.text(), "ok");
        assert_eq!(turn.reasoning, "thinking");
    }

    /// Arguments arrive split across several deltas and are only valid JSON after
    /// concatenation.
    #[test]
    fn fragmented_tool_arguments_are_reassembled() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&delta(0, Some("call_1"), Some("read_file"), "{\"pa"));
        acc.apply(&delta(0, None, None, "th\": \"/etc/"));
        acc.apply(&delta(0, None, None, "hostname\"}"));
        acc.apply(&ProviderEvent::Done {
            stop_reason: StopReason::ToolUse,
        });

        let turn = acc.finish();
        let calls: Vec<_> = turn.message.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        let (id, name, args) = calls[0];
        assert_eq!(id, "call_1");
        assert_eq!(name, "read_file");
        assert_eq!(args, &serde_json::json!({"path": "/etc/hostname"}));
    }

    #[test]
    fn parallel_tool_calls_stay_separate() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&delta(0, Some("a"), Some("read_file"), "{\"path\":"));
        acc.apply(&delta(1, Some("b"), Some("list_dir"), "{\"path\":"));
        acc.apply(&delta(0, None, None, "\"/x\"}"));
        acc.apply(&delta(1, None, None, "\"/y\"}"));
        acc.apply(&ProviderEvent::Done {
            stop_reason: StopReason::ToolUse,
        });

        let turn = acc.finish();
        let calls: Vec<_> = turn.message.tool_calls().collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "a");
        assert_eq!(calls[0].2, &serde_json::json!({"path": "/x"}));
        assert_eq!(calls[1].0, "b");
        assert_eq!(calls[1].2, &serde_json::json!({"path": "/y"}));
    }

    #[test]
    fn malformed_arguments_are_reported_not_fatal() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&delta(
            0,
            Some("c1"),
            Some("read_file"),
            "{\"path\": \"/etc",
        ));
        acc.apply(&ProviderEvent::Done {
            stop_reason: StopReason::ToolUse,
        });

        let turn = acc.finish();
        assert_eq!(turn.tool_arg_errors.len(), 1);
        assert_eq!(turn.tool_arg_errors[0].tool, "read_file");
        assert_eq!(turn.tool_arg_errors[0].raw, "{\"path\": \"/etc");
        assert!(turn.message.has_tool_calls());
    }

    #[test]
    fn empty_arguments_mean_empty_object() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&delta(0, Some("c1"), Some("list_dir"), ""));
        acc.apply(&ProviderEvent::Done {
            stop_reason: StopReason::ToolUse,
        });

        let turn = acc.finish();
        assert!(turn.tool_arg_errors.is_empty());
        let (_, _, args) = turn.message.tool_calls().next().unwrap();
        assert_eq!(args, &serde_json::json!({}));
    }

    /// Some providers omit finish_reason on tool-call turns. Inferring ToolUse keeps the
    /// loop from silently dropping the tool calls.
    #[test]
    fn missing_finish_reason_with_tool_calls_infers_tool_use() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&delta(0, Some("c1"), Some("read_file"), "{}"));
        let turn = acc.finish();
        assert_eq!(turn.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn missing_finish_reason_without_tool_calls_infers_end_turn() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&ProviderEvent::TextDelta("hi".into()));
        let turn = acc.finish();
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
    }
}
