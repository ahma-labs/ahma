//! End-to-end tests for the native Anthropic Messages API flavor of
//! [`LlmClient`], driven against a mocked `/messages` endpoint.
//!
//! The mock server URI does not contain `anthropic.com`, so the flavor is
//! forced explicitly with [`ApiFlavor::Anthropic`] (the auto-detection path is
//! unit-tested in `src/anthropic.rs`).

use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header, method, path},
};

use ahma_llm_monitor::{ApiFlavor, ChatMessage, LlmClient};

fn anthropic_client(uri: String) -> LlmClient {
    LlmClient::new(uri, "claude-opus-4-8", Some("sk-ant-test".into()))
        .with_flavor(ApiFlavor::Anthropic)
}

#[tokio::test]
async fn detect_issues_uses_messages_endpoint_and_anthropic_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "NullPointerException at line 42"}],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
        .mount(&server)
        .await;

    let client = anthropic_client(server.uri());
    let result = client
        .detect_issues(
            "look for crashes",
            "FATAL Exception: NullPointerException",
            Duration::from_secs(5),
        )
        .await
        .expect("request should succeed");

    assert_eq!(result.as_deref(), Some("NullPointerException at line 42"));
}

#[tokio::test]
async fn detect_issues_clean_returns_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "CLEAN"}]
        })))
        .mount(&server)
        .await;

    let client = anthropic_client(server.uri());
    let result = client
        .detect_issues("look for crashes", "INFO ok", Duration::from_secs(5))
        .await
        .expect("request should succeed");

    assert!(result.is_none(), "CLEAN should map to None");
}

#[tokio::test]
async fn chat_completion_parses_tool_use_blocks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        // The OpenAI tool definition must have been translated to input_schema.
        .and(body_string_contains("input_schema"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [
                {"type": "text", "text": "Checking status."},
                {"type": "tool_use", "id": "tu_1", "name": "status", "input": {"verbose": true}}
            ],
            "usage": {"input_tokens": 20, "output_tokens": 8}
        })))
        .mount(&server)
        .await;

    let client = anthropic_client(server.uri());
    let tools = vec![json!({
        "type": "function",
        "function": {
            "name": "status",
            "description": "show status",
            "parameters": {"type": "object", "properties": {"verbose": {"type": "boolean"}}}
        }
    })];

    let resp = client
        .chat_completion_with_tools(&[json!({"role": "user", "content": "status?"})], &tools)
        .await
        .expect("request should succeed");

    assert_eq!(resp.content, "Checking status.");
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].name, "status");
    assert_eq!(resp.tool_calls[0].arguments, json!({"verbose": true}));

    // assistant_message is OpenAI-shaped so the agentic loop can append + replay it.
    let oa_call = &resp.assistant_message["tool_calls"][0];
    assert_eq!(oa_call["id"], "tu_1");
    assert_eq!(oa_call["function"]["name"], "status");

    let usage = resp.usage.expect("usage present");
    assert_eq!(usage.prompt_tokens, 20);
    assert_eq!(usage.completion_tokens, 8);
}

#[tokio::test]
async fn chat_completion_adaptive_thinking_in_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(body_string_contains("\"thinking\""))
        .and(body_string_contains("adaptive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "done"}]
        })))
        .mount(&server)
        .await;

    // Default Anthropic flavor enables adaptive thinking.
    let client = anthropic_client(server.uri());
    let resp = client
        .chat_completion_with_tools(&[json!({"role": "user", "content": "hi"})], &[])
        .await
        .expect("request should succeed");
    assert_eq!(resp.content, "done");
}

#[tokio::test]
async fn chat_stream_decodes_anthropic_sse_text_deltas() {
    let server = MockServer::start().await;
    // A minimal Messages API SSE stream: two text deltas, then message_stop.
    // thinking_delta is interleaved and must be skipped by the token stream.
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = anthropic_client(server.uri());
    let stream = client.chat_stream(vec![ChatMessage::user("hi")], Some("be brief"));
    futures::pin_mut!(stream);

    let mut collected = String::new();
    while let Some(item) = stream.next().await {
        collected.push_str(&item.expect("stream item should be Ok"));
    }
    assert_eq!(collected, "Hello, world");
}

#[tokio::test]
async fn http_error_surfaces_as_parse_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "type": "error",
            "error": {"type": "authentication_error", "message": "invalid x-api-key"}
        })))
        .mount(&server)
        .await;

    let client = anthropic_client(server.uri());
    let err = client
        .chat_completion_with_tools(&[json!({"role": "user", "content": "hi"})], &[])
        .await
        .expect_err("401 should be an error");
    assert!(err.to_string().contains("401"), "got: {err}");
}
