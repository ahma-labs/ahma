//! # Ahma LLM Monitor
//!
//! OpenAI-compatible LLM client for log analysis and issue detection.
//! Used by the live log monitoring pipeline to detect issues described
//! in plain English via a detection prompt.
//!
//! Also provides streaming chat (`LlmClient::chat_stream`) and local-provider
//! auto-discovery (`discover_local_providers`) for the TUI chat interface.

pub mod anthropic;
pub mod client;
pub mod discovery;
pub mod error;
pub mod prompt;

pub use client::{
    ApiFlavor, ChatCompletionResponse, ChatMessage, ChatRole, ChatToolCall, LlmClient,
    LocalProvider,
};
pub use discovery::discover_local_providers;
pub use error::{ApiErrorKind, LlmMonitorError, llm_service_name};
