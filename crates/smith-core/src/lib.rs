//! Core domain and agent logic for smith.
//!
//! This crate contains the shared implementation of smith's behavior. Frontends (terminal,
//! headless, web) render the types defined here. Provider adapters translate wire formats
//! behind [`provider::Provider`]. This crate performs no network, terminal or HTTP I/O, and
//! frontends and adapters must not add such dependencies.

pub mod agent;
pub mod config;
pub mod error;
pub mod message;
pub mod mock;
pub mod prompt;
pub mod provider;
pub mod session;
pub mod tools;
pub mod working_dir;

pub use error::{ProviderError, SmithError};
pub use message::{ContentBlock, Message, Role, StopReason, Usage};
pub use provider::{
    ChatRequest, ModelInfo, Provider, ProviderEvent, ProviderProxy, ProviderStream, ProxyEvent,
    ToolSpec,
};
pub use session::{
    Command, SessionConfig, SessionHandle, SessionId, SessionManager, SessionSnapshot, ToolSet,
    UiEvent, UiEventKind,
};
pub use tools::{Tool, ToolContext, ToolDisplay, ToolError};
