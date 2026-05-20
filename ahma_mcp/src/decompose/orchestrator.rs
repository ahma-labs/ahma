//! Sub-task dispatch and orchestration for the `decompose` tool type.
//!
//! The orchestrator takes a parent question, asks a local LLM to split it into
//! sub-questions, dispatches each in bounded concurrency, and hands the results
//! to a [`Reducer`].

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use ahma_llm_monitor::LlmClient;

use crate::config::DecomposeConfig;

use super::reducer::Reducer;

// ─────────────────────────────────────────────────────────────────────────────
// SubTaskResult
// ─────────────────────────────────────────────────────────────────────────────

/// The outcome of a single decomposed sub-task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTaskResult {
    /// The sub-task label / sub-question.
    pub label: String,
    /// The model's answer, or an error description.
    pub text: String,
    /// Whether the sub-task succeeded.
    pub success: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// DecomposeOrchestrator
// ─────────────────────────────────────────────────────────────────────────────

/// Orchestrates decomposition of a complex question into local-LLM sub-tasks.
pub struct DecomposeOrchestrator {
    cfg: DecomposeConfig,
    client: Arc<LlmClient>,
}

impl DecomposeOrchestrator {
    /// Build an orchestrator from a [`DecomposeConfig`].
    pub fn new(cfg: DecomposeConfig) -> Self {
        let client = Arc::new(LlmClient::new(
            &cfg.llm_provider.base_url,
            &cfg.llm_provider.model,
            cfg.llm_provider.api_key.clone(),
        ));
        Self { cfg, client }
    }

    /// Run the full decompose pipeline and return the aggregated answer.
    ///
    /// 1. Ask the LLM to break `question` into `max_subtasks` sub-questions.
    /// 2. Dispatch each sub-question to the LLM in bounded concurrency.
    /// 3. Aggregate with the configured reducer.
    pub async fn run(&self, question: &str) -> Result<String> {
        let sub_questions = self.split_question(question).await?;
        debug!("Decomposed into {} sub-questions", sub_questions.len());

        let results = self.dispatch_subtasks(&sub_questions).await;

        let reduce_mode = self.cfg.reduce_mode.clone().unwrap_or_default();
        let reducer = Reducer::new(reduce_mode);

        let pairs: Vec<(&str, &str)> = results
            .iter()
            .map(|r| (r.label.as_str(), r.text.as_str()))
            .collect();

        Ok(reducer.reduce(&pairs))
    }

    /// Ask the LLM to split `question` into sub-questions.
    async fn split_question(&self, question: &str) -> Result<Vec<String>> {
        let max = self.cfg.max_subtasks.unwrap_or(5);
        let timeout = Duration::from_secs(self.cfg.llm_timeout_seconds.unwrap_or(30));

        let split_prompt = format!(
            "Break the following question into at most {max} distinct, self-contained sub-questions \
             that together cover the full answer. Return ONLY the sub-questions, one per line, \
             numbered like '1. ...' No other text.\n\nQuestion: {question}"
        );

        // Re-use detect_issues with a custom prompt to get the sub-questions.
        // The LLM is instructed not to return "CLEAN", so we'll always get Some().
        let raw = self
            .client
            .detect_issues(&split_prompt, "", timeout)
            .await
            .context("LLM call to split question failed")?;

        let text = raw.unwrap_or_else(|| question.to_string());

        let sub_questions: Vec<String> = text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| {
                // Strip leading numbering like "1. " or "- "
                if let Some(rest) = l.strip_prefix(|c: char| c.is_ascii_digit()) {
                    rest.trim_start_matches(". ")
                        .trim_start_matches(") ")
                        .to_string()
                } else {
                    l.trim_start_matches("- ").to_string()
                }
            })
            .take(max)
            .collect();

        if sub_questions.is_empty() {
            Ok(vec![question.to_string()])
        } else {
            Ok(sub_questions)
        }
    }

    /// Dispatch sub-questions to the LLM with bounded concurrency.
    async fn dispatch_subtasks(&self, sub_questions: &[String]) -> Vec<SubTaskResult> {
        let max_concurrent = self.cfg.max_concurrent.unwrap_or(3);
        let timeout = Duration::from_secs(self.cfg.llm_timeout_seconds.unwrap_or(30));

        // Split into chunks of max_concurrent and process sequentially per chunk.
        let mut all_results = vec![];

        for chunk in sub_questions.chunks(max_concurrent) {
            let futures: Vec<_> = chunk
                .iter()
                .map(|q| {
                    let client = Arc::clone(&self.client);
                    let question = q.clone();
                    let detection_prompt = self.cfg.answer_prompt.clone().unwrap_or_else(|| {
                        "Answer the question concisely and accurately.".to_string()
                    });
                    async move {
                        let result = client
                            .detect_issues(&detection_prompt, &question, timeout)
                            .await;
                        match result {
                            Ok(Some(text)) => SubTaskResult {
                                label: question,
                                text,
                                success: true,
                            },
                            Ok(None) => SubTaskResult {
                                label: question.clone(),
                                text: String::from("(No issues detected — result was CLEAN)"),
                                success: true,
                            },
                            Err(e) => {
                                warn!("Sub-task failed: {e}");
                                SubTaskResult {
                                    label: question,
                                    text: format!("Error: {e}"),
                                    success: false,
                                }
                            }
                        }
                    }
                })
                .collect();

            let chunk_results = join_all(futures).await;
            all_results.extend(chunk_results);
        }

        all_results
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DecomposeConfig, LlmProviderConfig};

    fn test_cfg() -> DecomposeConfig {
        use crate::decompose::ReduceMode;
        DecomposeConfig {
            llm_provider: LlmProviderConfig {
                base_url: "http://localhost:11434/v1".to_string(),
                model: "llama3.2".to_string(),
                api_key: None,
            },
            max_subtasks: Some(3),
            max_concurrent: Some(2),
            reduce_mode: Some(ReduceMode::Summarize),
            answer_prompt: None,
            llm_timeout_seconds: Some(5),
        }
    }

    #[test]
    fn orchestrator_constructs_without_panic() {
        let _orch = DecomposeOrchestrator::new(test_cfg());
    }
}
