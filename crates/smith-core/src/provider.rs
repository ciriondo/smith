use std::pin::Pin;

use futures_util::Stream;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::message::{Message, StopReason, Usage};

/// A boxed, cancellable stream of normalised provider events.
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Human-facing label. It falls back to `id` when the provider does not supply one.
    pub label: String,
    pub context_window: Option<u32>,
}

/// A tool advertised to the model. The schema is JSON Schema. Adapters translate it into
/// their own wire format (OpenAI `tools[]`, Anthropic `tools[]`, …).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// One request to a provider.
///
/// Intentionally not `serde`-annotated for any particular wire format: serialisation is the
/// adapter's job.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Stable per-conversation id. OpenCode Go requires this (`x-opencode-session`) and uses
    /// it for routing and prompt caching. Other providers may ignore it.
    pub session_id: String,
}

impl ChatRequest {
    pub fn new(
        model: impl Into<String>,
        messages: Vec<Message>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            session_id: session_id.into(),
        }
    }
}

/// A normalised event from a provider stream.
///
/// Normalising here keeps frontends independent of wire formats and solves the
/// fragmented-JSON tool-argument problem in one place.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    /// Reasoning/thinking tokens, kept separate from the answer. Providers that emit
    /// `reasoning_content` (OpenCode Go does for GLM and others) produce this event.
    ReasoningDelta(String),
    ToolCallDelta {
        /// Position in the tool-call array. Fragments belonging to one call share an index.
        index: u32,
        /// Present only on the first fragment of a call.
        id: Option<String>,
        /// Present only on the first fragment of a call.
        name: Option<String>,
        /// Raw JSON fragment. Concatenate the fragments for one call and parse the result
        /// after the stream ends.
        args_delta: String,
    },
    Usage(Usage),
    Done {
        stop_reason: StopReason,
    },
}

/// A model backend.
///
/// `stream` is not `async` and returns a boxed stream. This keeps the trait object-safe
/// without `async-trait` and passes cancellation into the stream rather than wrapping the
/// call.
pub trait Provider: Send + Sync {
    /// Stable id used in config and in `provider/model` addressing, for example `opencode-go`.
    fn id(&self) -> &str;

    /// Models this provider advertises.
    fn models(&self) -> Vec<ModelInfo>;

    /// Stream one assistant turn.
    ///
    /// Implementations must not retry internally. Retry policy belongs to the agent loop.
    /// They must honour `cancel`, terminating the stream promptly with
    /// [`ProviderError::Cancelled`] when it fires.
    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream;
}
