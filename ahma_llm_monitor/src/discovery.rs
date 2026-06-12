use crate::client::{LlmClient, LocalProvider};
use crate::error::LlmMonitorError;

/// Discover local LLM providers by probing common ports.
pub async fn discover_local_providers() -> Result<Vec<LocalProvider>, LlmMonitorError> {
    let endpoints = vec![
        ("oMLX", "http://localhost:8000/v1"),
        ("Ollama", "http://localhost:11434/v1"),
        ("llama-server", "http://localhost:8080/v1"),
        ("oMLX", "http://127.0.0.1:8000/v1"),
        ("Ollama", "http://127.0.0.1:11434/v1"),
        ("llama-server", "http://127.0.0.1:8080/v1"),
    ];

    let mut discovered = Vec::new();
    let mut seen_urls = std::collections::HashSet::new();
    let mut seen_names = std::collections::HashSet::new();

    for (name, url) in endpoints {
        if seen_urls.contains(url) {
            continue;
        }

        let client = LlmClient::new(url, "", None);
        let models = client.list_model().await;
        if !models.is_empty() {
            if seen_names.contains(name) {
                continue;
            }
            discovered.push(LocalProvider {
                name: name.to_string(),
                base_url: url.to_string(),
                models,
            });
            seen_urls.insert(url.to_string());
            seen_names.insert(name.to_string());
        }
    }

    Ok(discovered)
}
