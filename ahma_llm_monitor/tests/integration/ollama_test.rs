//! Ollama's own chat API, chosen automatically for Ollama endpoints so a
//! configured context size takes effect (ahma_llm_monitor SPEC, "Ollama's own
//! API"). The mock listens on a random port, so the flavor is set explicitly
//! here; selection by URL is tested without a server.

use serde_json::json;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

use ahma_llm_monitor::{ApiFlavor, ChatMessage, LlmClient};

fn ollama_client(server: &MockServer) -> LlmClient {
    LlmClient::new(format!("{}/v1", server.uri()), "qwen3:8b", None).with_flavor(ApiFlavor::Ollama)
}

#[test]
fn an_ollama_url_speaks_ollamas_own_api() {
    assert_eq!(
        LlmClient::new("http://localhost:11434/v1", "m", None).flavor(),
        ApiFlavor::Ollama
    );
    assert_eq!(
        LlmClient::new("http://localhost:1234/v1", "m", None).flavor(),
        ApiFlavor::OpenAi
    );
    // An OpenAI-compatible provider entry pointing at Ollama still gets it.
    assert_eq!(
        LlmClient::new("http://localhost:11434/v1", "m", None)
            .with_provider_kind(ahma_common::config::ProviderKind::OpenAi)
            .flavor(),
        ApiFlavor::Ollama
    );
    assert!(
        LlmClient::new("http://localhost:11434/v1", "m", None)
            .with_num_ctx(Some(8192))
            .sends_num_ctx()
    );
}

#[tokio::test]
async fn a_streamed_tool_turn_goes_to_api_chat_with_the_context_size() {
    let server = MockServer::start().await;
    let ndjson = concat!(
        "{\"message\":{\"role\":\"assistant\",\"content\":\"Let me look\"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}}]},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":20,\"eval_count\":7}\n"
    );
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(ndjson),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = ollama_client(&server).with_num_ctx(Some(12288));
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let tools = vec![
        json!({"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}),
    ];
    let resp = client
        .chat_completion_with_tools_streaming(
            &[json!({"role": "user", "content": "hi"})],
            &tools,
            tx,
        )
        .await
        .expect("a completion");
    assert_eq!(resp.content, "Let me look");
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].name, "read_file");
    assert_eq!(resp.tool_calls[0].arguments, json!({"path": "a.rs"}));
    assert!(
        matches!(rx.try_recv(), Ok(ahma_llm_monitor::client::StreamDelta::Content(c)) if c == "Let me look")
    );

    let sent: Vec<Request> = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["options"]["num_ctx"], json!(12288));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));
}

#[tokio::test]
async fn a_whole_tool_turn_and_a_plain_chat_stream_use_api_chat_too() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if body["stream"] == json!(true) {
                ResponseTemplate::new(200).set_body_string(
                    "{\"message\":{\"content\":\"He\"},\"done\":false}\n{\"message\":{\"content\":\"y\"},\"done\":false}\n{\"message\":{\"content\":\"\"},\"done\":true}\n",
                )
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "message": {"role": "assistant", "content": "whole"},
                    "done": true, "done_reason": "stop", "prompt_eval_count": 4, "eval_count": 1
                }))
            }
        })
        .mount(&server)
        .await;

    let client = ollama_client(&server);
    let resp = client
        .chat_completion_with_tools(&[json!({"role": "user", "content": "hi"})], &[])
        .await
        .expect("a completion");
    assert_eq!(resp.content, "whole");

    use futures::StreamExt as _;
    let tokens: Vec<String> = client
        .chat_stream(vec![ChatMessage::user("hi")], Some("sys"))
        .map(|t| t.unwrap())
        .collect()
        .await;
    assert_eq!(tokens.concat(), "Hey");

    let sent = server.received_requests().await.unwrap();
    let plain: serde_json::Value = serde_json::from_slice(&sent[1].body).unwrap();
    assert_eq!(
        plain["messages"][0],
        json!({"role": "system", "content": "sys"})
    );
    assert!(plain.get("options").is_some());
}

#[tokio::test]
async fn log_analysis_uses_api_chat() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": {"role": "assistant", "content": "CLEAN"}, "done": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    let result = ollama_client(&server)
        .detect_issues("crashes", "INFO ok", std::time::Duration::from_secs(5))
        .await;
    assert!(matches!(result, Ok(None)), "{result:?}");
}
