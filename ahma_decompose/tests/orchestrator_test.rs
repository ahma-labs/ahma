use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

use ahma_decompose::config::{DecomposeConfig, LlmProviderConfig};
use ahma_decompose::orchestrator::DecomposeOrchestrator;
use ahma_decompose::reducer::ReduceMode;

fn make_response(content: &str) -> serde_json::Value {
    json!({
        "choices": [{"message": {"content": content, "role": "assistant"}}]
    })
}

fn test_cfg(base_url: String) -> DecomposeConfig {
    DecomposeConfig {
        llm_provider: LlmProviderConfig {
            base_url,
            model: "test-model".to_string(),
            api_key: None,
        },
        max_subtasks: Some(3),
        max_concurrent: Some(2),
        reduce_mode: Some(ReduceMode::Summarize),
        answer_prompt: Some("Custom answer prompt".to_string()),
        llm_timeout_seconds: Some(5),
    }
}

#[tokio::test]
async fn test_orchestrator_successful_run() {
    let server = MockServer::start().await;

    // 1. Mock the split question LLM request
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Break the following question"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response(
            "1. What is the capital of France?\n2. What is the population of Paris?",
        )))
        .mount(&server)
        .await;

    // 2. Mock subtask 1 LLM request
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("What is the capital of France?"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("Paris")))
        .mount(&server)
        .await;

    // 3. Mock subtask 2 LLM request
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("What is the population of Paris?"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("2.1 million")))
        .mount(&server)
        .await;

    let orchestrator = DecomposeOrchestrator::new(test_cfg(server.uri())).unwrap();
    let result = orchestrator.run("Tell me about Paris.").await;

    assert!(result.is_ok());
    let output = result.unwrap();
    // The Reducer with Summarize mode formats pairs like:
    // "## Sub-task 1 — What is the capital of France?\n\nParis\n\n## Sub-task 2 — What is the population of Paris?\n\n2.1 million"
    assert!(output.contains("## Sub-task 1 — What is the capital of France?"));
    assert!(output.contains("Paris"));
    assert!(output.contains("## Sub-task 2 — What is the population of Paris?"));
    assert!(output.contains("2.1 million"));
}

#[tokio::test]
async fn test_orchestrator_subtask_failure() {
    let server = MockServer::start().await;

    // 1. Mock the split question LLM request
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Break the following question"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(make_response("1. Sub-question A\n2. Sub-question B")),
        )
        .mount(&server)
        .await;

    // 2. Mock subtask A LLM request (Success)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Sub-question A"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("Answer A")))
        .mount(&server)
        .await;

    // 3. Mock subtask B LLM request (Failure)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Sub-question B"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let orchestrator = DecomposeOrchestrator::new(test_cfg(server.uri())).unwrap();
    let result = orchestrator.run("Parent question").await;

    assert!(result.is_ok());
    let output = result.unwrap();
    // Verify that the success response for Sub-question A is included
    assert!(output.contains("## Sub-task 1 — Sub-question A"));
    assert!(output.contains("Answer A"));
    // Verify that the error message is captured for Sub-question B
    assert!(output.contains("## Sub-task 2 — Sub-question B"));
    assert!(output.contains("Error:"));
}

#[tokio::test]
async fn test_orchestrator_split_failure() {
    let server = MockServer::start().await;

    // Mock split question request returning 500 error
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Break the following question"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
        .mount(&server)
        .await;

    let orchestrator = DecomposeOrchestrator::new(test_cfg(server.uri())).unwrap();
    let result = orchestrator.run("Parent question").await;

    // The whole run should fail since splitting failed
    assert!(result.is_err());
}

#[tokio::test]
async fn test_orchestrator_split_returns_empty() {
    let server = MockServer::start().await;

    // Mock split question request returning nothing or invalid format
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Break the following question"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("")))
        .mount(&server)
        .await;

    // Mock answering the fallback parent question itself
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("Parent question"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("Fallback Answer")))
        .mount(&server)
        .await;

    let orchestrator = DecomposeOrchestrator::new(test_cfg(server.uri())).unwrap();
    let result = orchestrator.run("Parent question").await;

    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.contains("## Sub-task 1 — Parent question"));
    assert!(output.contains("Fallback Answer"));
}
