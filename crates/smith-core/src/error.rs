use thiserror::Error;

/// Errors a [`crate::provider::Provider`] may return.
///
/// Adapters translate transport-specific failures into these variants, so frontends do not
/// depend on the HTTP client. The enum has no `reqwest` variant.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("no API key: environment variable {0} is unset")]
    MissingApiKey(String),

    #[error("authentication rejected by {provider}: {message}")]
    Auth { provider: String, message: String },

    #[error("rate limited by {provider}: {message}")]
    RateLimited { provider: String, message: String },

    #[error("{provider} rejected the request: {message}")]
    BadRequest { provider: String, message: String },

    #[error("{provider} reported an upstream failure: {message}")]
    Upstream { provider: String, message: String },

    #[error("transport failure for {provider}: {message}")]
    Transport { provider: String, message: String },

    /// The stream itself was malformed (bad JSON, truncated SSE frame).
    #[error("{provider} sent a malformed response: {message}")]
    Malformed { provider: String, message: String },

    /// Tool arguments arrived as unparseable JSON.
    ///
    /// This has its own variant because the parse failure is recoverable. The turn still
    /// finishes, and `TurnFinished` reports the error to the frontend.
    #[error("tool call {tool} had unparseable arguments: {message}")]
    MalformedToolArgs {
        tool: String,
        raw: String,
        message: String,
    },

    #[error("request cancelled")]
    Cancelled,
}

/// Errors originating in smith itself rather than a provider.
#[derive(Debug, Error)]
pub enum SmithError {
    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("session {0} is no longer running")]
    SessionClosed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
