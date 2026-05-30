use crate::client::LocalProvider;
use crate::error::LlmMonitorError;

/// Discover local LLM providers.
pub async fn discover_local_providers() -> Result<Vec<LocalProvider>, LlmMonitorError> {
    Ok(Vec::new())
}
