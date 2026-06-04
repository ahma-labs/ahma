//! Configuration types for the task tree orchestrator.

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use ahma_common::config::{interpolate_env_vars, warn_if_looks_like_literal_secret};

/// Connection details for an OpenAI-compatible LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmProviderConfig {
    /// Base URL of the API endpoint, e.g. `http://localhost:11434/v1` (Ollama).
    pub base_url: String,
    /// Model identifier, e.g. `llama3.2`, `gemma:4b`.
    pub model: String,
    /// Optional bearer token. Supports `${ENV_VAR}` interpolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl LlmProviderConfig {
    /// Resolve env-var placeholders and return concrete connection values.
    pub fn resolve(&self) -> Result<ResolvedLlmProvider> {
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

/// A resolved LLM provider.
#[derive(Debug, Clone)]
pub struct ResolvedLlmProvider {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// Configuration for a task tree orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskTreeConfig {
    /// LLM provider to use for planning, execution, and summarisation.
    pub llm_provider: LlmProviderConfig,
    /// Maximum depth of the task tree to prevent infinite recursion loops.
    /// Defaults to 4.
    pub max_depth: Option<usize>,
    /// Maximum number of retries per failed task node.
    /// Defaults to 2.
    pub max_retries: Option<usize>,
    /// Maximum concurrency level.
    /// Defaults to 3.
    pub max_concurrent: Option<usize>,
    /// Output length threshold in characters before invoking output summarisation.
    /// Defaults to 500.
    pub summarisation_threshold: Option<usize>,
    /// Timeout in seconds for LLM calls.
    /// Defaults to 30.
    pub llm_timeout_seconds: Option<u64>,
}

impl Default for TaskTreeConfig {
    fn default() -> Self {
        Self {
            llm_provider: LlmProviderConfig {
                base_url: "http://localhost:11434/v1".to_string(),
                model: "gemma:4b".to_string(),
                api_key: None,
            },
            max_depth: Some(4),
            max_retries: Some(2),
            max_concurrent: Some(3),
            summarisation_threshold: Some(500),
            llm_timeout_seconds: Some(30),
        }
    }
}
