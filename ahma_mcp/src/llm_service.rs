use anyhow::Result;
use std::time::Duration;

#[async_trait::async_trait]
pub trait LlmCompletionService: Send + Sync + std::fmt::Debug {
    async fn detect_issues(
        &self,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        prompt: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<Option<String>>;
}

#[derive(Debug, Clone, Default)]
pub struct DefaultLlmCompletionService;

#[async_trait::async_trait]
impl LlmCompletionService for DefaultLlmCompletionService {
    async fn detect_issues(
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
