//! Configuration types for `tool_type: decompose` tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::reducer::ReduceMode;

/// Connection details for an OpenAI-compatible LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmProviderConfig {
    /// Base URL of the API endpoint, e.g. `http://localhost:11434/v1` (Ollama).
    pub base_url: String,
    /// Model identifier, e.g. `llama3.2`, `gemma:4b`.
    pub model: String,
    /// Optional bearer token. Omit for local models that don't require authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Configuration for a `tool_type: decompose` tool.
///
/// A decompose tool splits a complex business question into smaller sub-questions,
/// dispatches each to a local LLM (e.g. `gemma:4b` via Ollama), and aggregates
/// the results with a deterministic Rust reducer — no cloud egress required.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DecomposeConfig {
    /// LLM provider to use for both splitting and answering sub-questions.
    pub llm_provider: LlmProviderConfig,
    /// Maximum number of sub-questions to generate from the parent question.
    /// Defaults to 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_subtasks: Option<usize>,
    /// Maximum number of sub-questions to run concurrently.
    /// Keep this low (2–4) for single-machine Ollama deployments.  Defaults to 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,
    /// How to combine sub-task results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_mode: Option<ReduceMode>,
    /// System prompt injected when asking the LLM to answer each sub-question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_prompt: Option<String>,
    /// Timeout in seconds for each individual LLM call.  Defaults to 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_timeout_seconds: Option<u64>,
}
