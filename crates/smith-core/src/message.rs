use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a message.
///
/// Tool calls and tool results are blocks rather than roles. This keeps the transcript
/// independent of any provider's JSON, so adding a second provider does not change the
/// transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        /// Parsed arguments. Adapters deal with the string fragments. Core never sees partial
        /// JSON.
        args: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Reasoning the provider streamed with this assistant message.
    ///
    /// It is continuation state, not conversation: [`Message::text`] ignores it and no
    /// frontend renders it. Providers that require the previous reasoning for the next request
    /// (thinking mode with tool calls) get it back from the adapter. The field is omitted from
    /// serialised snapshots, so the HTTP shape does not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            reasoning: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            reasoning: None,
        }
    }

    /// Concatenation of all text blocks, ignoring tool calls and results.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolCall { id, name, args } => Some((id.as_str(), name.as_str(), args)),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls().next().is_some()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl Usage {
    pub fn accumulate(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

impl StopReason {
    /// Map a provider's `finish_reason` string onto the normalised set.
    ///
    /// The catch-all handles values not listed here. An unknown reason must not be an error.
    pub fn from_finish_reason(raw: &str) -> Self {
        match raw {
            "stop" => StopReason::EndTurn,
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" | "max_tokens" => StopReason::MaxTokens,
            other => StopReason::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_ignores_tool_blocks() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "reading".into(),
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "/etc/hostname"}),
                },
            ],
            reasoning: Some("the path is in the request".into()),
        };
        assert_eq!(msg.text(), "reading");
        assert!(msg.has_tool_calls());
    }

    #[test]
    fn reasoning_is_serialised_only_when_present() {
        let plain = Message::assistant("ok");
        let json = serde_json::to_value(&plain).unwrap();
        assert_eq!(json.get("reasoning"), None, "absent field stays absent");

        let mut reasoned = Message::assistant("ok");
        reasoned.reasoning = Some("thinking".into());
        let json = serde_json::to_value(&reasoned).unwrap();
        assert_eq!(json["reasoning"], "thinking");

        let restored: Message = serde_json::from_value(json).unwrap();
        assert_eq!(restored.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn finish_reasons_normalise() {
        assert_eq!(StopReason::from_finish_reason("stop"), StopReason::EndTurn);
        assert_eq!(
            StopReason::from_finish_reason("tool_calls"),
            StopReason::ToolUse
        );
        assert_eq!(
            StopReason::from_finish_reason("length"),
            StopReason::MaxTokens
        );
        // Unknown values must survive rather than panic.
        assert_eq!(
            StopReason::from_finish_reason("content_filter"),
            StopReason::Other("content_filter".into())
        );
    }
}
