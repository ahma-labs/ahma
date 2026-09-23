use crate::client::{LlmClient, LocalProvider};
use crate::error::LlmMonitorError;

/// Discover local LLM providers by probing common ports.
pub async fn discover_local_providers() -> Result<Vec<LocalProvider>, LlmMonitorError> {
    let endpoints = [
        ("LM Studio", "http://localhost:1234/v1"),
        ("Ollama", "http://localhost:11434/v1"),
        ("llama-server", "http://localhost:8080/v1"),
        ("LM Studio", "http://127.0.0.1:1234/v1"),
        ("Ollama", "http://127.0.0.1:11434/v1"),
        ("llama-server", "http://127.0.0.1:8080/v1"),
    ];

    // Probe every endpoint concurrently: each probe can take up to its full
    // connect timeout, so probing sequentially made worst-case discovery time
    // the *sum* of all timeouts (~18s at TUI startup). Results are folded in
    // the original endpoint order, so the first-listed URL for a provider name
    // still wins deduplication exactly as it did sequentially.
    let probes = endpoints.iter().map(|(name, url)| async move {
        let client = LlmClient::new(*url, "", None);
        (*name, *url, client.list_model().await)
    });
    let results = futures::future::join_all(probes).await;

    let mut discovered = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for (name, url, models) in results {
        if !models.is_empty() {
            if seen_names.contains(name) {
                continue;
            }
            discovered.push(LocalProvider {
                name: name.to_string(),
                base_url: url.to_string(),
                models,
            });
            seen_names.insert(name.to_string());
        }
    }

    Ok(discovered)
}
