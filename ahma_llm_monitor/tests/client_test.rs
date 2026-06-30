use std::time::Duration;

use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use ahma_llm_monitor::LlmClient;

fn make_response(content: &str) -> serde_json::Value {
    json!({
        "choices": [{"message": {"content": content, "role": "assistant"}}]
    })
}

#[tokio::test]
async fn test_detect_issues_clean_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("CLEAN")))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let result = client
        .detect_issues(
            "look for crashes",
            "INFO app started",
            Duration::from_secs(5),
        )
        .await;

    assert!(result.is_ok());
    assert!(result.unwrap().is_none(), "expected None (clean)");
}

#[tokio::test]
async fn test_detect_issues_issue_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response(
            "NullPointerException in MainActivity line 42",
        )))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let result = client
        .detect_issues(
            "look for crashes or exceptions",
            "FATAL Exception: NullPointerException",
            Duration::from_secs(5),
        )
        .await;

    assert!(result.is_ok());
    let summary = result.unwrap();
    assert!(summary.is_some(), "expected Some(summary)");
    assert!(summary.unwrap().contains("NullPointerException"));
}

#[tokio::test]
async fn test_detect_issues_clean_case_insensitive() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("clean")))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let result = client
        .detect_issues(
            "look for errors",
            "DEBUG heartbeat ok",
            Duration::from_secs(5),
        )
        .await;

    assert!(result.is_ok());
    assert!(
        result.unwrap().is_none(),
        "expected None for lowercase 'clean'"
    );
}

#[tokio::test]
async fn test_detect_issues_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let result = client
        .detect_issues("look for errors", "some log", Duration::from_secs(5))
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_detect_issues_sends_bearer_auth() {
    use wiremock::matchers::header;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer my-secret-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("CLEAN")))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", Some("my-secret-key".into()));
    let result = client
        .detect_issues("look for errors", "INFO ok", Duration::from_secs(5))
        .await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_list_model_success() {
    let server = MockServer::start().await;
    let mock_response = json!({
        "data": [
            {"id": "llama3.2"},
            {"id": "gemma"}
        ]
    });

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let models = client.list_model().await;

    // Check alphabetical sorting
    assert_eq!(models, vec!["gemma".to_string(), "llama3.2".to_string()]);
}

#[tokio::test]
async fn test_list_model_failure_returns_empty() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let models = client.list_model().await;

    assert!(models.is_empty());
}

#[tokio::test]
async fn test_chat_completion_with_tools_success() {
    let server = MockServer::start().await;
    let mock_response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Using tools",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {
                        "name": "my_tool",
                        "arguments": "{\"arg\":123}"
                    }
                }]
            }
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_response))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let response = client
        .chat_completion_with_tools(vec![json!({"role": "user", "content": "hi"})], &[])
        .await
        .unwrap();

    assert_eq!(response.content, "Using tools");
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "my_tool");
    assert_eq!(response.tool_calls[0].arguments["arg"], 123);
}

#[tokio::test]
async fn test_chat_stream_success() {
    use ahma_llm_monitor::ChatMessage;
    use futures::StreamExt as _;

    let server = MockServer::start().await;
    let sse_body = "data: {\"choices\": [{\"delta\": {\"content\": \"Hello \"}}]}\n\
                    data: {\"choices\": [{\"delta\": {\"content\": \"World!\"}}]}\n\
                    data: [DONE]\n\n";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let messages = vec![ChatMessage::user("hi")];
    let mut stream = Box::pin(client.chat_stream(messages, None));

    let mut tokens = vec![];
    while let Some(res) = stream.next().await {
        let token = res.unwrap();
        if !token.is_empty() {
            tokens.push(token);
        }
    }

    assert_eq!(tokens, vec!["Hello ".to_string(), "World!".to_string()]);
}

#[tokio::test]
async fn test_discover_local_providers() {
    let result = ahma_llm_monitor::discovery::discover_local_providers().await;
    assert!(result.is_ok());
}

/// A transient 5xx is retried with backoff and the call ultimately succeeds.
#[tokio::test]
async fn chat_completion_retries_transient_5xx_then_succeeds() {
    let server = MockServer::start().await;
    // First two attempts get 503 (higher priority, capped at 2 uses)…
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    // …then a normal 200 completion.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response("recovered")))
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let resp = client
        .chat_completion_with_tools(vec![], &[])
        .await
        .expect("retry should recover from transient 503s");
    assert_eq!(resp.content, "recovered");
}

/// A non-retryable 4xx fails immediately (no wasted retries on a caller error).
#[tokio::test]
async fn chat_completion_does_not_retry_4xx() {
    let server = MockServer::start().await;
    // Mounted with a single-use cap: if the client retried, the 2nd call would
    // 404 (no matching mock) — but expect(1) also asserts exactly one call.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1)
        .mount(&server)
        .await;

    let client = LlmClient::new(server.uri(), "test-model", None);
    let err = client
        .chat_completion_with_tools(vec![], &[])
        .await
        .expect_err("4xx must surface as an error, not be retried");
    assert!(err.to_string().contains("400"), "got: {err}");
    // `expect(1)` is verified on server drop: exactly one request was made.
}
