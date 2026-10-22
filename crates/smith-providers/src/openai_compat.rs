//! OpenAI-compatible chat completions.
//!
//! Verified against OpenCode Go (`https://opencode.ai/inference/go/openai/v1`): it streams
//! `chat.completion.chunk` frames, terminates with `data: [DONE]`, reports usage in a final
//! chunk whose `choices` array is empty, and emits a separate `reasoning_content` channel
//! alongside `content`.

use std::collections::VecDeque;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use smith_core::{
    provider::{ChatRequest, ModelInfo, Provider, ProviderEvent, ProviderStream, ToolSpec},
    ContentBlock, ProviderError, Role, StopReason, Usage,
};

use crate::sse::SseParser;

const KIND: &str = "openai_compat";

pub struct OpenAiCompat {
    id: String,
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
    /// Header carrying the conversation id. OpenCode Go requires
    /// `x-opencode-session` so it can optimise routing and prompt caching. Other
    /// OpenAI-compatible endpoints ignore it.
    session_header: Option<String>,
    models: Vec<ModelInfo>,
    client: reqwest::Client,
}

impl OpenAiCompat {
    pub fn new(
        id: impl Into<String>,
        cfg: &smith_core::config::ProviderConfig,
    ) -> Result<Self, ProviderError> {
        let id = id.into();
        let api_key = std::env::var(&cfg.api_key_env)
            .map_err(|_| ProviderError::MissingApiKey(cfg.api_key_env.clone()))?;

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::Transport {
                provider: id.clone(),
                message: e.to_string(),
            })?;

        Ok(Self {
            id,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key,
            user_agent: cfg.user_agent.clone(),
            session_header: cfg.session_header.clone(),
            models: cfg
                .models
                .iter()
                .map(|m| ModelInfo {
                    id: m.clone(),
                    label: m.clone(),
                    context_window: None,
                })
                .collect(),
            client,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

impl Provider for OpenAiCompat {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let body = build_body(&req);
        let url = self.endpoint();
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let user_agent = self.user_agent.clone();
        let session_header = self.session_header.clone();
        let session_id = req.session_id.clone();
        let provider = self.id.clone();

        // `Provider::stream` is not `async`, so a task performs the request and sends
        // normalised events through a channel. The returned stream receives them.
        //
        // The `select!` below cancels an in-flight request immediately rather than at the
        // next chunk boundary.
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let mut request = client.post(&url).json(&body).bearer_auth(&api_key);
            if let Some(ua) = &user_agent {
                request = request.header(reqwest::header::USER_AGENT, ua);
            }
            if let Some(header) = &session_header {
                request = request.header(header.as_str(), &session_id);
            }

            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(Err(ProviderError::Transport {
                            provider: provider.clone(),
                            message: e.to_string(),
                        }))
                        .await;
                    return;
                }
            };

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let _ = tx
                    .send(Err(map_status_with(&provider, status, &body, &url)))
                    .await;
                return;
            }

            let mut chunks = response.bytes_stream();
            let mut parser = SseParser::new();
            let mut pending: VecDeque<ProviderEvent> = VecDeque::new();
            let mut upstream_done = false;

            loop {
                while let Some(event) = pending.pop_front() {
                    // A closed receiver means the consumer stopped reading, so return.
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
                if upstream_done {
                    return;
                }

                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        let _ = tx.send(Err(ProviderError::Cancelled)).await;
                        return;
                    }
                    item = chunks.next() => match item {
                        None => {
                            // Flush a frame that arrived without its trailing newline.
                            for payload in parser.finish() {
                                if let Err(e) = enqueue(&provider, &payload, &mut pending) {
                                    let _ = tx.send(Err(e)).await;
                                    return;
                                }
                            }
                            upstream_done = true;
                        }
                        Some(Err(e)) => {
                            let _ = tx
                                .send(Err(ProviderError::Transport {
                                    provider: provider.clone(),
                                    message: e.to_string(),
                                }))
                                .await;
                            return;
                        }
                        Some(Ok(chunk)) => {
                            for payload in parser.push(&chunk) {
                                if let Err(e) = enqueue(&provider, &payload, &mut pending) {
                                    let _ = tx.send(Err(e)).await;
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        });

        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Classify a non-2xx response.
///
/// The body is inspected first: OpenCode Go returns 403 with a structured reason such as
/// `FreeTierError`, which identifies the failure more precisely than the status code.
fn map_status_with(
    provider: &str,
    status: reqwest::StatusCode,
    body: &str,
    url: &str,
) -> ProviderError {
    let message = extract_error_message(body).unwrap_or_else(|| {
        if body.trim().is_empty() {
            status.to_string()
        } else {
            body.chars().take(400).collect()
        }
    });

    match status.as_u16() {
        401 => ProviderError::Auth {
            provider: provider.to_string(),
            message,
        },
        403 => ProviderError::Auth {
            provider: provider.to_string(),
            message,
        },
        404 => ProviderError::BadRequest {
            provider: provider.to_string(),
            message: format!("{message} (endpoint: {url})"),
        },
        429 => ProviderError::RateLimited {
            provider: provider.to_string(),
            message,
        },
        400 | 422 => ProviderError::BadRequest {
            provider: provider.to_string(),
            message,
        },
        _ => ProviderError::Upstream {
            provider: provider.to_string(),
            message,
        },
    }
}

/// `{"error": {"type": "...", "message": "..."}}`. OpenCode Go also wraps this as
/// `{"type": "error", "error": {...}}`.
fn extract_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let err = value.get("error")?;
    let message = err.get("message")?.as_str()?;
    Some(match err.get("type").and_then(Value::as_str) {
        Some(kind) => format!("{kind}: {message}"),
        None => message.to_string(),
    })
}

/// Decode one SSE payload into zero or more normalised events.
///
/// The function is pure, so the wire format is testable from fixtures without a socket.
pub fn parse_chunk(provider: &str, payload: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
    if payload == "[DONE]" {
        return Ok(Vec::new());
    }

    let chunk: RawChunk = serde_json::from_str(payload).map_err(|e| ProviderError::Malformed {
        provider: provider.to_string(),
        message: format!("{e} in {payload:.200}"),
    })?;

    if let Some(err) = chunk.error {
        let message = err
            .message
            .unwrap_or_else(|| "unspecified error".to_string());
        let kind = err.kind.unwrap_or_default();
        return Err(classify_stream_error(provider, &kind, &message));
    }

    // Some failures are flagged only by the envelope `type`, with no `error` object.
    if chunk.kind.as_deref() == Some("error") {
        return Err(ProviderError::Upstream {
            provider: provider.to_string(),
            message: format!("{payload:.200}"),
        });
    }

    let mut out = Vec::new();

    for choice in chunk.choices {
        if let Some(content) = choice.delta.content {
            if !content.is_empty() {
                out.push(ProviderEvent::TextDelta(content));
            }
        }
        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() {
                out.push(ProviderEvent::ReasoningDelta(reasoning.clone()));
            }
        }
        for call in choice.delta.tool_calls {
            out.push(ProviderEvent::ToolCallDelta {
                index: call.index,
                id: call.id,
                name: call.function.as_ref().and_then(|f| f.name.clone()),
                // Absent `arguments` is legal on continuation fragments.
                args_delta: call.function.and_then(|f| f.arguments).unwrap_or_default(),
            });
        }
        if let Some(reason) = choice.finish_reason {
            out.push(ProviderEvent::Done {
                stop_reason: StopReason::from_finish_reason(&reason),
            });
        }
    }

    if let Some(usage) = chunk.usage {
        out.push(ProviderEvent::Usage(Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        }));
    }

    Ok(out)
}

/// Errors embedded in a 200-status stream. The provider-supplied type string classifies them
/// because no HTTP status code is available.
fn classify_stream_error(provider: &str, kind: &str, message: &str) -> ProviderError {
    let lower = kind.to_ascii_lowercase();
    let message = if kind.is_empty() {
        message.to_string()
    } else {
        format!("{kind}: {message}")
    };

    if lower.contains("auth") || lower.contains("free") || lower.contains("forbidden") {
        ProviderError::Auth {
            provider: provider.to_string(),
            message,
        }
    } else if lower.contains("ratelimit") || lower.contains("rate_limit") {
        ProviderError::RateLimited {
            provider: provider.to_string(),
            message,
        }
    } else {
        ProviderError::Upstream {
            provider: provider.to_string(),
            message,
        }
    }
}

fn enqueue(
    provider: &str,
    payload: &str,
    pending: &mut VecDeque<ProviderEvent>,
) -> Result<(), ProviderError> {
    for event in parse_chunk(provider, payload)? {
        pending.push_back(event);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

/// Translate the normalised transcript into OpenAI's message array.
///
/// Two shape mismatches are handled here rather than in core:
///  * system prompt is a leading `role: "system"` message, not a field;
///  * each tool result must be its own `role: "tool"` message, so one core message can fan
///    out into several wire messages.
pub fn build_body(req: &ChatRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = &req.system {
        messages.push(json!({"role": "system", "content": system}));
    }

    for message in &req.messages {
        let has_results = message
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));

        if has_results {
            for block in &message.content {
                if let ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } = block
                {
                    // OpenAI has no `is_error` field on tool messages, so the failure is
                    // stated in the content the model reads.
                    let content = if *is_error {
                        format!("ERROR: {content}")
                    } else {
                        content.clone()
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content,
                    }));
                }
            }
            continue;
        }

        let text = message.text();
        let calls: Vec<Value> = message
            .tool_calls()
            .map(|(id, name, args)| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": args.to_string(),
                    }
                })
            })
            .collect();

        match message.role {
            Role::User => {
                messages.push(json!({"role": "user", "content": text}));
            }
            Role::Assistant => {
                let mut wire = json!({"role": "assistant"});
                if calls.is_empty() {
                    wire["content"] = json!(text);
                } else {
                    // `content` must be present even when null for strict servers.
                    wire["content"] = if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    };
                    wire["tool_calls"] = json!(calls);
                }
                // Thinking-mode models reject the request unless the reasoning that came
                // with an assistant message is passed back verbatim.
                if let Some(reasoning) = message.reasoning.as_deref().filter(|r| !r.is_empty()) {
                    wire["reasoning_content"] = json!(reasoning);
                }
                messages.push(wire);
            }
        }
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });

    if let Some(max) = req.max_tokens {
        body["max_tokens"] = json!(max);
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }
    if !req.tools.is_empty() {
        body["tools"] = json!(req.tools.iter().map(encode_tool).collect::<Vec<_>>());
    }

    body
}

fn encode_tool(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.schema,
        }
    })
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawChunk {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    choices: Vec<RawChoice>,
    #[serde(default)]
    usage: Option<RawUsage>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Debug, Deserialize)]
struct RawError {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawChoice {
    #[serde(default)]
    delta: RawDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDelta {
    #[serde(default)]
    content: Option<String>,
    /// OpenCode Go emits this for GLM and other reasoning models. `reasoning` is accepted
    /// as an alias because several OpenAI-compatible servers use that spelling instead.
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

#[derive(Debug, Deserialize)]
struct RawToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<RawFunction>,
}

#[derive(Debug, Deserialize)]
struct RawFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

/// Exposed for the registry. The kind string is defined in one place.
pub fn kind() -> &'static str {
    KIND
}
