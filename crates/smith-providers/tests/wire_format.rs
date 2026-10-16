//! Offline tests over streams recorded from the live OpenCode Go endpoint.
//!
//! Every fixture in `tests/fixtures/` is a verbatim capture of a real response
//! (`POST https://opencode.ai/inference/go/openai/v1/chat/completions`), so the wire format is
//! tested against recorded traffic rather than documentation.

use serde_json::json;

use smith_core::agent::TurnAccumulator;
use smith_core::provider::{ChatRequest, ProviderEvent, ToolSpec};
use smith_core::{ContentBlock, Message, Role, StopReason};
use smith_providers::openai_compat::{build_body, parse_chunk};
use smith_providers::sse::SseParser;

const PROVIDER: &str = "opencode-go";

/// Run a recorded fixture through the same parser the live stream uses.
fn replay(fixture: &str) -> TurnAccumulator {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), fixture);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));

    let mut parser = SseParser::new();
    let mut payloads = parser.push(&bytes);
    payloads.extend(parser.finish());

    let mut acc = TurnAccumulator::new();
    let mut saw_done = false;
    for payload in payloads {
        if payload == "[DONE]" {
            saw_done = true;
            continue;
        }
        for event in parse_chunk(PROVIDER, &payload).unwrap_or_else(|e| panic!("{payload}: {e}")) {
            acc.apply(&event);
        }
    }
    assert!(
        saw_done,
        "{fixture} should terminate with a [DONE] sentinel"
    );
    acc
}

#[test]
fn plain_stream_assembles_text_and_usage() {
    let turn = replay("glm-plain.sse").finish();

    assert_eq!(turn.message.role, Role::Assistant);
    assert_eq!(turn.message.text(), "hello world");
    assert_eq!(turn.stop_reason, StopReason::EndTurn);
    assert!(turn.tool_arg_errors.is_empty());

    // Usage arrives in a final chunk whose `choices` array is empty. A parser that indexes
    // `choices[0]` unconditionally would drop it.
    assert!(
        turn.usage.input_tokens > 0 && turn.usage.output_tokens > 0,
        "usage should be captured, got {:?}",
        turn.usage
    );
}

#[test]
fn reasoning_arrives_on_its_own_channel() {
    let turn = replay("glm-plain.sse").finish();

    // GLM on OpenCode Go emits `reasoning_content` deltas alongside `content`. They must be
    // kept out of the transcript but must not be dropped either.
    assert!(
        !turn.reasoning.is_empty(),
        "expected reasoning_content deltas in the fixture"
    );
    assert!(
        !turn.message.text().contains(&turn.reasoning),
        "reasoning must not leak into the reply text"
    );
}

#[test]
fn tool_call_stream_assembles_arguments() {
    let turn = replay("glm-toolcall.sse").finish();

    assert_eq!(turn.stop_reason, StopReason::ToolUse);
    let calls: Vec<_> = turn.message.tool_calls().collect();
    assert_eq!(calls.len(), 1);

    let (id, name, args) = calls[0];
    assert!(id.starts_with("chatcmpl-tool-"), "unexpected id {id}");
    assert_eq!(name, "read_file");
    assert_eq!(args, &json!({"path": "/etc/hostname"}));
    assert!(turn.tool_arg_errors.is_empty());
}

#[test]
fn done_sentinel_is_not_an_event() {
    assert!(parse_chunk(PROVIDER, "[DONE]").unwrap().is_empty());
}

#[test]
fn usage_only_chunk_with_empty_choices_yields_usage() {
    let payload = r#"{"id":"x","choices":[],"usage":{"prompt_tokens":19,"completion_tokens":41,"total_tokens":60}}"#;
    let events = parse_chunk(PROVIDER, payload).unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], ProviderEvent::Usage(_)));
}

/// The exact shape OpenCode Go returns when a credential is not entitled to a model. It
/// arrives as an HTTP 403 whose body contains the reason.
#[test]
fn freetier_error_body_is_surfaced_not_swallowed() {
    let payload = r#"{"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode."}}"#;
    let err = parse_chunk(PROVIDER, payload).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("FreeTierError"), "{text}");
    assert!(text.contains("free tier"), "{text}");
}

#[test]
fn truncated_json_is_malformed_not_a_panic() {
    let err = parse_chunk(PROVIDER, r#"{"choices":[{"delta":{"content":"#).unwrap_err();
    assert!(
        matches!(err, smith_core::ProviderError::Malformed { .. }),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

#[test]
fn body_puts_system_prompt_first_and_enables_streaming() {
    let mut req = ChatRequest::new("glm-5.3-flash", vec![Message::user("hi")], "session-1");
    req.system = Some("be terse".to_string());
    req.max_tokens = Some(64);

    let body = build_body(&req);
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["model"], json!("glm-5.3-flash"));
    assert_eq!(body["max_tokens"], json!(64));
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][1]["role"], json!("user"));
    assert_eq!(body["messages"][1]["content"], json!("hi"));
}

/// One core `Message` holding two tool results becomes two `role: "tool"` wire messages.
/// This shape mismatch is why the translation exists.
#[test]
fn tool_results_fan_out_to_separate_wire_messages() {
    let mut assistant = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "checking".into(),
        }],
    };
    assistant.content.push(ContentBlock::ToolCall {
        id: "call_a".into(),
        name: "read_file".into(),
        args: json!({"path": "/x"}),
    });
    assistant.content.push(ContentBlock::ToolCall {
        id: "call_b".into(),
        name: "read_file".into(),
        args: json!({"path": "/y"}),
    });

    let results = Message {
        role: Role::User,
        content: vec![
            ContentBlock::ToolResult {
                call_id: "call_a".into(),
                content: "AAA".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                call_id: "call_b".into(),
                content: "boom".into(),
                is_error: true,
            },
        ],
    };

    let req = ChatRequest::new(
        "glm-5.3-flash",
        vec![Message::user("go"), assistant, results],
        "s",
    );
    let body = build_body(&req);
    let messages = body["messages"].as_array().unwrap();

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["role"], json!("assistant"));
    assert_eq!(messages[1]["tool_calls"][0]["id"], json!("call_a"));
    // Arguments are re-serialised as a JSON string, as the wire format requires.
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["arguments"],
        json!("{\"path\":\"/x\"}")
    );
    assert_eq!(messages[2]["role"], json!("tool"));
    assert_eq!(messages[2]["tool_call_id"], json!("call_a"));
    assert_eq!(messages[2]["content"], json!("AAA"));
    // A failed result is marked in the content, since OpenAI has no is_error field.
    assert_eq!(messages[3]["content"], json!("ERROR: boom"));
}

#[test]
fn assistant_text_only_has_no_tool_calls_key() {
    let req = ChatRequest::new("m", vec![Message::assistant("done")], "s");
    let body = build_body(&req);
    let msg = &body["messages"][0];
    assert_eq!(msg["role"], json!("assistant"));
    assert_eq!(msg["content"], json!("done"));
    assert!(msg.get("tool_calls").is_none());
}

#[test]
fn tools_are_encoded_as_function_specs() {
    let mut req = ChatRequest::new("m", vec![Message::user("hi")], "s");
    req.tools = vec![ToolSpec {
        name: "read_file".into(),
        description: "Read a file".into(),
        schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }];

    let body = build_body(&req);
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
        json!("string")
    );
}

#[test]
fn message_without_tools_omits_the_tools_key() {
    let req = ChatRequest::new("m", vec![Message::user("hi")], "s");
    let body = build_body(&req);
    assert!(body.get("tools").is_none());
}
