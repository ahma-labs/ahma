use anyhow::Result;
use std::time::Duration;

/// The livelog pipeline's LLM issue-detection call.
///
/// This was previously a trait (`LlmCompletionService`) with exactly one
/// implementation and no test double — every test exercised this same
/// concrete type and mocked at the HTTP layer via `wiremock` instead of
/// swapping the trait impl. Collapsed to a concrete type to drop the
/// `dyn`-dispatch/`async_trait` boxing overhead the indirection bought
/// nothing for.
#[derive(Debug, Clone, Default)]
pub struct DefaultLlmCompletionService;

impl DefaultLlmCompletionService {
    pub async fn detect_issues(
        &self,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        prompt: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<Option<String>> {
        let llm = ahma_llm_monitor::LlmClient::new(base_url, model, api_key);
        llm.detect_issues(prompt, text, timeout)
            .await
            .map_err(anyhow::Error::from)
    }
}
