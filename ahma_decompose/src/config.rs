//! Configuration types for `tool_type: decompose` tools.

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use ahma_common::config::{interpolate_env_vars, warn_if_looks_like_literal_secret};

use crate::reducer::ReduceMode;

/// Connection details for an OpenAI-compatible LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmProviderConfig {
    /// Base URL of the API endpoint, e.g. `http://localhost:11434/v1` (Ollama).
    pub base_url: String,
    /// Model identifier, e.g. `llama3.2`, `gemma:4b`.
    pub model: String,
    /// Optional bearer token. Supports `${ENV_VAR}` interpolation — **never
    /// store literal API keys in tool definition files**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl LlmProviderConfig {
    /// Resolve env-var placeholders and return concrete connection values.
    ///
    /// - `${ENV_VAR}` in `api_key` is expanded from the process environment.
    /// - Returns `Err` if a referenced variable is not set.
    /// - Warns if `api_key` looks like a literal secret (e.g. starts with `sk-`).
    pub fn resolve(&self) -> Result<ResolvedLlmProvider> {
        // Warn on literal key even before interpolation attempt.
        if let Some(key) = &self.api_key {
            warn_if_looks_like_literal_secret(key);
        }
        let api_key = self
            .api_key
            .as_deref()
            .map(interpolate_env_vars)
            .transpose()?;
        Ok(ResolvedLlmProvider {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key,
        })
    }
}

/// A [`LlmProviderConfig`] with secrets resolved — ready to pass to an HTTP client.
#[derive(Debug, Clone)]
pub struct ResolvedLlmProvider {
    pub base_url: String,
    pub model: String,
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
