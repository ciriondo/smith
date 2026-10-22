//! The agent loop.
//!
//! `run_turn` consumes one provider stream and assembles the assistant message.
//! `run_agent` is the loop around it: it executes tool calls and calls the provider again
//! until the assistant answers without tool calls. `TurnAccumulator` owns the accumulation
//! logic, including the fragmented tool-call arguments.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::message::{ContentBlock, Message, Role, StopReason, Usage};
use crate::provider::{ChatRequest, Provider, ProviderEvent, ProviderStream};
use crate::tools::{
    failure_display, summarize_call, summarize_result, Tool, ToolContext, ToolDisplay, ToolError,
};

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
                reasoning: if self.reasoning.is_empty() {
                    None
                } else {
                    Some(self.reasoning)
                },
            },
            usage: self.usage,
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

/// One event the loop reports while a turn runs.
#[derive(Debug)]
pub enum AgentEvent {
    /// A provider event, forwarded for rendering.
    Provider(ProviderEvent),
    ToolStarted {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolFinished {
        call_id: String,
        name: String,
        ok: bool,
        duration_ms: u64,
        truncated: bool,
        summary: String,
        display: ToolDisplay,
    },
}

/// Why a turn ended without a result.
#[derive(Debug)]
pub enum AgentError {
    /// The user cancelled. Nothing from the turn is recorded.
    Cancelled,
    /// The turn cannot continue. The message names the cause.
    Failed(String),
}

/// A request to stop a user turn at its next provider-request boundary.
///
/// The flag is owned by the session actor for one turn and shared with the loop. A pause does
/// not cancel the provider stream or a running tool; the loop observes it between steps.
#[derive(Clone, Default)]
pub struct PauseToken(Arc<AtomicBool>);

impl PauseToken {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn clear(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What a turn adds to the transcript and to the frontends.
#[derive(Debug)]
pub struct AgentOutcome {
    /// Assistant messages and tool results, in order. On a completed turn the last entry is
    /// the final assistant message; on a paused turn it may be a tool result.
    pub added: Vec<Message>,
    /// The final assistant message. `None` only when a turn paused before its first provider
    /// request produced anything.
    pub final_message: Option<Message>,
    pub usage: Usage,
    /// Tool calls whose arguments did not parse. Reported with the final event.
    pub tool_arg_errors: Vec<ToolArgError>,
    /// True when the loop stopped at a boundary instead of completing the user turn.
    pub paused: bool,
}

/// The session-owned inputs a turn composes its request from.
pub struct TurnContext<'a> {
    /// Canonical workspace root handed to every tool call.
    pub workspace: &'a Path,
    /// The static system prompt parts. The sandbox fragment is added during the turn.
    pub prompt: &'a crate::prompt::PromptParts,
}

/// Run a whole turn: call the provider, execute the tool calls it asks for, and repeat until
/// it answers without any.
///
/// `emit` receives every event as it arrives. It must not block.
pub async fn run_agent(
    provider: &dyn Provider,
    tools: &[Arc<dyn Tool>],
    context: TurnContext<'_>,
    mut request: ChatRequest,
    pause: PauseToken,
    cancel: CancellationToken,
    emit: &mut (dyn FnMut(AgentEvent) + Send),
) -> Result<AgentOutcome, AgentError> {
    use futures_util::StreamExt;

    let mut messages = request.messages.clone();
    let mut added: Vec<Message> = Vec::new();
    let mut usage = Usage::default();
    let mut tool_arg_errors = Vec::new();

    let mut fragments: Vec<String> = Vec::new();
    for tool in tools {
        match tool.prepare().await {
            Ok(Some(fragment)) => {
                // The sandbox tools share one handle and therefore return the same inventory.
                // Place each distinct fragment once, inside the sandbox section.
                if !fragments.iter().any(|existing| existing == &fragment) {
                    fragments.push(fragment);
                }
            }
            Ok(None) => {}
            Err(ToolError::Message(message)) => return Err(AgentError::Failed(message)),
            Err(ToolError::SandboxGone) => {
                return Err(AgentError::Failed("the sandbox is unavailable".to_string()))
            }
        }
    }
    request.system = context
        .prompt
        .compose(request.system.as_deref(), &request.model, &fragments);
    request.tools = tools.iter().map(|tool| tool.spec()).collect();

    loop {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if pause.is_requested() {
            return Ok(AgentOutcome {
                added,
                final_message: None,
                usage,
                tool_arg_errors,
                paused: true,
            });
        }

        let mut req = request.clone();
        req.messages = messages.clone();
        let mut accumulator = TurnAccumulator::new();
        let mut stream = provider.stream(req, cancel.clone());
        while let Some(item) = stream.next().await {
            let event = match item {
                Ok(event) => event,
                Err(ProviderError::Cancelled) => return Err(AgentError::Cancelled),
                Err(e) => return Err(AgentError::Failed(e.to_string())),
            };
            if let ProviderEvent::Usage(tokens) = &event {
                usage.accumulate(*tokens);
            }
            accumulator.apply(&event);
            emit(AgentEvent::Provider(event));
        }

        let result = accumulator.finish();
        tool_arg_errors.extend(result.tool_arg_errors.clone());
        let assistant = result.message;
        messages.push(assistant.clone());
        added.push(assistant.clone());

        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if !assistant.has_tool_calls() {
            break;
        }

        let calls: Vec<(String, String, serde_json::Value)> = assistant
            .tool_calls()
            .map(|(id, name, args)| (id.to_string(), name.to_string(), args.clone()))
            .collect();

        for (call_id, name, args) in calls {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            emit(AgentEvent::ToolStarted {
                call_id: call_id.clone(),
                name: name.clone(),
                summary: summarize_call(&name, &args),
            });
            let started = Instant::now();

            let tool = tools.iter().find(|tool| tool.spec().name == name);
            let display_args = tool.and_then(|tool| tool.display_args(&args));
            let outcome = if args.is_null() {
                // The accumulator recorded the parse error; answer it so the model can try
                // again rather than leaving the call unanswered.
                Err(ToolError::Message(
                    "tool arguments did not parse".to_string(),
                ))
            } else if let Some(tool) = tool {
                let tool_context = ToolContext {
                    workspace: context.workspace.to_path_buf(),
                    cancel: cancel.clone(),
                };
                tool.call(args, &tool_context).await
            } else {
                let available: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
                Err(ToolError::Message(format!(
                    "unknown tool '{name}'; available tools: {}",
                    available.join(", ")
                )))
            };

            let (ok, content, truncated) = match outcome {
                Ok(content) => {
                    let truncated = result_is_truncated(&content);
                    (true, content, truncated)
                }
                Err(ToolError::Message(message)) => (false, message, false),
                Err(ToolError::SandboxGone) => {
                    return Err(AgentError::Failed("the sandbox is gone".to_string()))
                }
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            let display = if !ok {
                failure_display(&content)
            } else {
                display_args
                    .or_else(|| tool.map(|tool| tool.display_result(&content)))
                    .unwrap_or(ToolDisplay::Collapsed)
            };
            emit(AgentEvent::ToolFinished {
                call_id: call_id.clone(),
                name: name.clone(),
                ok,
                duration_ms,
                truncated,
                summary: summarize_result(&name, ok, truncated, &content),
                display,
            });

            let message = Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error: !ok,
                }],
                reasoning: None,
            };
            messages.push(message.clone());
            added.push(message);
        }
    }

    let final_message = added
        .last()
        .cloned()
        .expect("the loop runs at least one provider call");
    Ok(AgentOutcome {
        final_message: Some(final_message),
        added,
        usage,
        tool_arg_errors,
        paused: false,
    })
}

fn result_is_truncated(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|value| value.get("truncated").and_then(|flag| flag.as_bool()))
        .unwrap_or(false)
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
    fn reasoning_travels_with_the_message_but_not_the_text() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&ProviderEvent::ReasoningDelta("thinking".into()));
        acc.apply(&ProviderEvent::TextDelta("ok".into()));
        acc.apply(&ProviderEvent::Done {
            stop_reason: StopReason::EndTurn,
        });

        let turn = acc.finish();
        assert_eq!(turn.message.text(), "ok");
        assert_eq!(turn.message.reasoning.as_deref(), Some("thinking"));
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

    #[tokio::test]
    async fn the_follow_up_request_replays_the_reasoning() {
        let provider = crate::mock::MockProvider::new("mock");
        provider.push_events(vec![
            ProviderEvent::ReasoningDelta("need the file".into()),
            delta(
                0,
                Some("c1"),
                Some("read_file"),
                r#"{"path":"/etc/hostname"}"#,
            ),
            ProviderEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        provider.push_text("done");

        let tool: Arc<dyn Tool> = Arc::new(crate::mock::MockTool::new("read_file", "host"));
        let request = ChatRequest::new("mock-1", vec![Message::user("go")], "session");
        let outcome = run_agent(
            &provider,
            &[tool],
            TurnContext {
                workspace: Path::new("."),
                prompt: &crate::prompt::PromptParts::none(),
            },
            request,
            PauseToken::default(),
            CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .expect("the turn completes");

        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "one call per model step");
        let assistant = requests[1]
            .messages
            .iter()
            .find(|message| message.has_tool_calls())
            .expect("the follow-up carries the tool call");
        assert_eq!(assistant.reasoning.as_deref(), Some("need the file"));
        assert_eq!(outcome.final_message.unwrap().text(), "done");
    }

    #[tokio::test]
    async fn a_pre_set_pause_stops_before_the_first_request() {
        let provider = crate::mock::MockProvider::new("mock");
        let pause = PauseToken::default();
        pause.request();
        let request = ChatRequest::new("mock-1", vec![Message::user("go")], "session");
        let mut events = Vec::new();

        let outcome = run_agent(
            &provider,
            &[],
            TurnContext {
                workspace: Path::new("."),
                prompt: &crate::prompt::PromptParts::none(),
            },
            request,
            pause,
            CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .await
        .expect("a pause is not an error");

        assert!(outcome.paused);
        assert!(outcome.added.is_empty());
        assert!(outcome.final_message.is_none());
        assert!(events.is_empty());
        assert!(provider.requests().is_empty());
    }
}
