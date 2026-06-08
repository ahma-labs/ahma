//! Sub-task dispatch and orchestration for the `decompose` tool type.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use ahma_llm_monitor::LlmClient;

use crate::config::DecomposeConfig;
use crate::reducer::Reducer;

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

/// Orchestrates decomposition of a complex question into local-LLM sub-tasks.
pub struct DecomposeOrchestrator {
    cfg: DecomposeConfig,
    client: Arc<LlmClient>,
}

impl DecomposeOrchestrator {
    /// Build an orchestrator from a [`DecomposeConfig`].
    ///
    /// Resolves any `${ENV_VAR}` placeholders in the provider's API key.
    /// Returns `Err` if a referenced environment variable is not set.
    pub fn new(cfg: DecomposeConfig) -> Result<Self> {
        let provider = cfg.llm_provider.resolve()?;
        let client = Arc::new(LlmClient::new(
            &provider.base_url,
            &provider.model,
            provider.api_key,
        ));
        Ok(Self { cfg, client })
    }

    /// Run the full decompose pipeline and return the aggregated answer.
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

    async fn split_question(&self, question: &str) -> Result<Vec<String>> {
        let max = self.cfg.max_subtasks.unwrap_or(5);
        let timeout = Duration::from_secs(self.cfg.llm_timeout_seconds.unwrap_or(30));

        let prompts = ahma_common::prompts::AhmaPrompts::load();
        let split_prompt = prompts
            .split_prompt()
            .replace("{max}", &max.to_string())
            .replace("{question}", question);

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

    async fn dispatch_subtasks(&self, sub_questions: &[String]) -> Vec<SubTaskResult> {
        let max_concurrent = self.cfg.max_concurrent.unwrap_or(3);
        let timeout = Duration::from_secs(self.cfg.llm_timeout_seconds.unwrap_or(30));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DecomposeConfig, LlmProviderConfig};
    use crate::reducer::ReduceMode;

    fn test_cfg() -> DecomposeConfig {
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
        DecomposeOrchestrator::new(test_cfg()).expect("orchestrator construction should succeed");
    }
}
