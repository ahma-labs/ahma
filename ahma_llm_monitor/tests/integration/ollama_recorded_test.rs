// ─── Conformance against recorded Ollama traffic ─────────────────────────────
//
// Intended home: ahma_llm_monitor/tests/integration/ollama_recorded_test.rs
// plus `mod ollama_recorded_test;` in tests/integration/main.rs (its imports
// overlap ollama_test.rs, so appending there means merging the `use` lines).
// Fixtures live in ahma_llm_monitor/tests/integration/fixtures/ollama/ and
// were captured with curl from Ollama 0.35.0 serving granite4.2:3b on
// 2026-10-03.
//
// Each `*.request.json` is the exact body that was POSTed to the real server,
// and each response file is the server's reply byte for byte. The tests below
// replay the replies through wiremock and assert two things:
//   1. ahma sends *exactly* the request the real server accepted (JSON value
//      equality, so key order is irrelevant), and
//   2. ahma's translation of the real reply is right.
// Expected values are cross-checked against an independent parse of the raw
// NDJSON, so a re-recorded fixture with different wording still checks the
// translation rather than a stale literal.

use futures::StreamExt as _;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

use ahma_llm_monitor::client::StreamDelta;
use ahma_llm_monitor::{
    ApiErrorKind, ApiFlavor, ChatMessage, LlmClient, LlmMonitorError, ModelServer, ServerContext,
};

/// A captured file, relative to this source file.
macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("fixtures/ollama/", $name))
    };
}

const RECORDED_MODEL: &str = "granite4.2:3b";
/// The `num_ctx` every recorded chat request carried.
const RECORDED_NUM_CTX: u32 = 4096;

fn recorded_client(server: &MockServer, model: &str) -> LlmClient {
    LlmClient::new(format!("{}/v1", server.uri()), model, None)
        .with_flavor(ApiFlavor::Ollama)
        .with_num_ctx(Some(RECORDED_NUM_CTX))
}

/// Replay a captured response: the status and Content-Type from the captured
/// headers (`curl -D`), the body byte for byte.
fn replay(headers: &str, body: &'static str) -> ResponseTemplate {
    let mut lines = headers.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("captured status line");
    let content_type = lines
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-type")
                .then(|| v.trim().to_string())
        })
        .expect("captured Content-Type");
    ResponseTemplate::new(status).set_body_raw(body.as_bytes(), &content_type)
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).expect("fixture is JSON")
}

/// Independent oracle: the NDJSON lines of a captured stream.
fn ndjson(body: &str) -> Vec<Value> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(parse)
        .collect()
}

/// Concatenation of `message.<field>` over every line of a captured stream.
fn streamed(body: &str, field: &str) -> String {
    ndjson(body)
        .iter()
        .filter_map(|o| {
            o.pointer(&format!("/message/{field}"))?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// The last line of a captured stream (`done: true`).
fn final_line(body: &str) -> Value {
    let lines = ndjson(body);
    let last = lines.last().cloned().expect("a captured stream has lines");
    assert_eq!(last["done"], json!(true), "fixture ends with the done line");
    last
}

/// Bodies of every request the mock received on `/api/chat`, in order.
async fn sent_chat_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .iter()
        .filter(|r| r.url.path() == "/api/chat")
        .map(|r| serde_json::from_slice(&r.body).expect("ahma sends JSON"))
        .collect()
}

/// Messages and tools of a captured request, as ahma's callers pass them.
fn inputs_of(request: &Value) -> (Vec<Value>, Vec<Value>) {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let tools = request["tools"].as_array().cloned().unwrap_or_default();
    (messages, tools)
}

/// Drain everything already queued on a delta channel.
fn drain(rx: &mut tokio::sync::mpsc::Receiver<StreamDelta>) -> (String, String) {
    let (mut content, mut thinking) = (String::new(), String::new());
    while let Ok(d) = rx.try_recv() {
        match d {
            StreamDelta::Content(c) => content.push_str(&c),
            StreamDelta::Thinking(t) => thinking.push_str(&t),
        }
    }
    (content, thinking)
}

/// Channel large enough to hold every delta of the largest fixture: the
/// streaming call awaits `send`, and these tests read the receiver only after
/// it returns. (A real caller must drain concurrently; see notes.)
fn delta_channel() -> (
    tokio::sync::mpsc::Sender<StreamDelta>,
    tokio::sync::mpsc::Receiver<StreamDelta>,
) {
    tokio::sync::mpsc::channel(4096)
}

// ─── Plain streamed chat ─────────────────────────────────────────────────────

#[tokio::test]
async fn recorded_plain_stream_yields_only_the_visible_text() {
    let request = parse(fixture!("chat_stream_text.request.json"));
    let body = fixture!("chat_stream_text.response.ndjson");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(replay(fixture!("chat_stream_text.response.headers"), body))
        .expect(1)
        .mount(&server)
        .await;

    let system = request["messages"][0]["content"].as_str().unwrap();
    let user = request["messages"][1]["content"].as_str().unwrap();
    let client = recorded_client(&server, RECORDED_MODEL);
    let tokens: Vec<String> = client
        .chat_stream(vec![ChatMessage::user(user)], Some(system))
        .map(|t| t.expect("a token"))
        .collect()
        .await;

    // granite thinks first (44 `thinking` lines), then answers in 5 tokens:
    // only the answer is chat text.
    assert_eq!(tokens.concat(), "Hello there, friend.");
    assert_eq!(tokens.concat(), streamed(body, "content"));
    assert!(
        !streamed(body, "thinking").is_empty(),
        "fixture has reasoning"
    );
    assert_eq!(tokens.len(), 5, "{tokens:?}");

    assert_eq!(sent_chat_bodies(&server).await, vec![request]);
}

// ─── Tool call → tool result → answer ────────────────────────────────────────

#[tokio::test]
async fn recorded_tool_round_trip_sends_what_ollama_accepted() {
    let first_request = parse(fixture!("chat_stream_tool_call.request.json"));
    let second_request = parse(fixture!("chat_stream_tool_result.request.json"));
    let call_body = fixture!("chat_stream_tool_call.response.ndjson");
    let answer_body = fixture!("chat_stream_tool_result.response.ndjson");

    let server = MockServer::start().await;
    let call_reply = replay(
        fixture!("chat_stream_tool_call.response.headers"),
        call_body,
    );
    let answer_reply = replay(
        fixture!("chat_stream_tool_result.response.headers"),
        answer_body,
    );
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            match body["messages"].as_array().map(Vec::len) {
                Some(2) => call_reply.clone(),
                Some(4) => answer_reply.clone(),
                // 418: not retried, and impossible to mistake for a replay.
                _ => ResponseTemplate::new(418),
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let client = recorded_client(&server, RECORDED_MODEL);
    let (messages, tools) = inputs_of(&first_request);

    // Turn 1: the model calls get_weather. Ollama 0.35 sends the call whole,
    // on the `done: true` line, with its own `id` and a `function.index`.
    let (tx, mut rx) = delta_channel();
    let turn1 = client
        .chat_completion_with_tools_streaming(&messages, &tools, tx)
        .await
        .expect("turn 1");
    let recorded = final_line(call_body)["message"]["tool_calls"][0].clone();
    assert_eq!(turn1.content, "");
    assert_eq!(turn1.tool_calls.len(), 1);
    let call = &turn1.tool_calls[0];
    assert_eq!(call.name, "get_weather");
    assert_eq!(
        call.arguments,
        json!({"city": "Helsinki", "unit": "celsius"})
    );
    assert_eq!(call.arguments, recorded["function"]["arguments"]);
    assert_eq!(
        call.id,
        recorded["id"].as_str().unwrap(),
        "Ollama's own call id is kept"
    );
    assert_eq!(turn1.finish_reason.as_deref(), Some("tool_calls"));
    let usage = turn1.usage.as_ref().expect("usage from the done line");
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (326, 109));
    let (content, thinking) = drain(&mut rx);
    assert_eq!(content, "");
    assert_eq!(thinking, streamed(call_body, "thinking"));

    // Turn 2: the agent loop appends the assistant turn as ahma returned it
    // and a tool result keyed by the call id.
    let tool_result = second_request["messages"][3]["content"].clone();
    let mut history = messages.clone();
    history.push(turn1.assistant_message.clone());
    history.push(json!({"role": "tool", "tool_call_id": call.id, "content": tool_result}));
    let (tx, mut rx) = delta_channel();
    let turn2 = client
        .chat_completion_with_tools_streaming(&history, &tools, tx)
        .await
        .expect("turn 2");
    assert_eq!(
        turn2.content,
        "The current weather in Helsinki is 12\u{202f}°C with cloudy conditions."
    );
    assert_eq!(turn2.content, streamed(answer_body, "content"));
    assert!(turn2.tool_calls.is_empty());
    assert_eq!(turn2.finish_reason.as_deref(), Some("stop"));
    let (content, thinking) = drain(&mut rx);
    assert_eq!(content, turn2.content);
    assert_eq!(thinking, streamed(answer_body, "thinking"));

    // Both requests are exactly what the real server answered 200 to: tool
    // definitions passed through, the assistant's arguments as an object (a
    // JSON *string* there is a 400, see
    // `probe_tool_result_string_arguments.*`), and the result by `tool_name`.
    let sent = sent_chat_bodies(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], first_request);
    assert_eq!(sent[1], second_request);
}

#[tokio::test]
async fn recorded_parallel_tool_calls_stay_separate_calls() {
    let request = parse(fixture!("chat_stream_parallel_tool_calls.request.json"));
    let body = fixture!("chat_stream_parallel_tool_calls.response.ndjson");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(replay(
            fixture!("chat_stream_parallel_tool_calls.response.headers"),
            body,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let (messages, tools) = inputs_of(&request);
    let (tx, mut rx) = delta_channel();
    let resp = recorded_client(&server, RECORDED_MODEL)
        .chat_completion_with_tools_streaming(&messages, &tools, tx)
        .await
        .expect("a completion");

    let recorded = final_line(body)["message"]["tool_calls"].clone();
    assert_eq!(resp.tool_calls.len(), 2, "{:?}", resp.tool_calls);
    for (got, want) in resp.tool_calls.iter().zip(recorded.as_array().unwrap()) {
        assert_eq!(got.id, want["id"].as_str().unwrap());
        assert_eq!(got.name, "get_weather");
        assert_eq!(got.arguments, want["function"]["arguments"]);
    }
    assert_eq!(resp.tool_calls[0].arguments["city"], json!("Helsinki"));
    assert_eq!(resp.tool_calls[1].arguments["city"], json!("Oslo"));
    assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
    // ~200 lines of real reasoning must not trip the repetition guard.
    assert_eq!(drain(&mut rx).1, streamed(body, "thinking"));

    assert_eq!(sent_chat_bodies(&server).await, vec![request]);
}

#[tokio::test]
async fn recorded_reply_cut_off_while_thinking_reads_as_length_truncated() {
    // Response-only fixture: `num_predict: 24` spent entirely on reasoning,
    // so Ollama ends with empty content and `done_reason: "length"`.
    let body = fixture!("chat_stream_thinking_cut.response.ndjson");
    assert_eq!(final_line(body)["done_reason"], json!("length"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(replay(
            fixture!("chat_stream_thinking_cut.response.headers"),
            body,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let (tx, mut rx) = delta_channel();
    let resp = recorded_client(&server, RECORDED_MODEL)
        .chat_completion_with_tools_streaming(&[json!({"role": "user", "content": "hi"})], &[], tx)
        .await
        .expect("a completion");
    assert_eq!(resp.content, "");
    assert!(resp.tool_calls.is_empty());
    assert!(resp.is_length_truncated(), "{:?}", resp.finish_reason);
    assert_eq!(drain(&mut rx).1, streamed(body, "thinking"));
}

// ─── Whole (non-streamed) replies ────────────────────────────────────────────

#[tokio::test]
async fn recorded_whole_replies_parse_text_and_tool_calls() {
    let text_request = parse(fixture!("chat_whole_text.request.json"));
    let tool_request = parse(fixture!("chat_whole_tool_call.request.json"));
    let text_body = fixture!("chat_whole_text.response.json");
    let tool_body = fixture!("chat_whole_tool_call.response.json");

    let server = MockServer::start().await;
    let text_reply = replay(fixture!("chat_whole_text.response.headers"), text_body);
    let tool_reply = replay(fixture!("chat_whole_tool_call.response.headers"), tool_body);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            if body.get("tools").is_some() {
                tool_reply.clone()
            } else {
                text_reply.clone()
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let client = recorded_client(&server, RECORDED_MODEL);

    let (messages, _) = inputs_of(&text_request);
    let text = client
        .chat_completion_with_tools(&messages, &[])
        .await
        .expect("text reply");
    assert_eq!(text.content, "pong");
    assert!(text.tool_calls.is_empty());
    assert_eq!(text.finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        text.assistant_message["reasoning_content"],
        parse(text_body)["message"]["thinking"]
    );
    let usage = text.usage.as_ref().unwrap();
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens
        ),
        (22, 36, 58)
    );

    let (messages, tools) = inputs_of(&tool_request);
    let tool = client
        .chat_completion_with_tools(&messages, &tools)
        .await
        .expect("tool reply");
    let recorded = parse(tool_body)["message"]["tool_calls"][0].clone();
    assert_eq!(tool.content, "");
    assert_eq!(tool.tool_calls.len(), 1);
    assert_eq!(tool.tool_calls[0].id, recorded["id"].as_str().unwrap());
    assert_eq!(tool.tool_calls[0].name, "get_weather");
    assert_eq!(
        tool.tool_calls[0].arguments,
        json!({"city": "Helsinki", "unit": "celsius"})
    );
    assert_eq!(tool.finish_reason.as_deref(), Some("tool_calls"));

    assert_eq!(
        sent_chat_bodies(&server).await,
        vec![text_request, tool_request]
    );
}

// ─── Log classification (detect_issues) ──────────────────────────────────────

const DETECTION_PROMPT: &str = "crashes or panics";
const CLEAN_CHUNK: &str = "INFO server started on port 8080\nINFO request GET /health 200";
const PANIC_CHUNK: &str = "INFO server started on port 8080\nERROR thread 'main' panicked at src/main.rs:42: index out of bounds";

#[tokio::test]
async fn recorded_clean_classification_is_clean() {
    let request = parse(fixture!("detect_issues_clean.request.json"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(replay(
            fixture!("detect_issues_clean.response.headers"),
            fixture!("detect_issues_clean.response.json"),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let result = recorded_client(&server, RECORDED_MODEL)
        .detect_issues(
            DETECTION_PROMPT,
            CLEAN_CHUNK,
            std::time::Duration::from_secs(30),
        )
        .await;
    assert!(matches!(result, Ok(None)), "{result:?}");
    assert_eq!(sent_chat_bodies(&server).await, vec![request]);
}

/// Recorded: granite spends the whole 256-token classification
/// budget reasoning about a real panic, so Ollama returns `content: ""` with
/// `done_reason: "length"`. `detect_issues` currently turns that into
/// `Ok(Some(""))` — an issue with an empty summary (and the same would happen
/// for a clean log the model happened to think about at length). Land with the
/// fix: send `think: false` on this call (recorded in
/// `detect_issues_issue_think_false.*`: a real summary in 28 tokens) and/or
/// treat empty-and-truncated as an error.
#[tokio::test]
async fn recorded_classification_cut_off_while_thinking_is_not_an_empty_issue() {
    let request = parse(fixture!("detect_issues_issue.request.json"));
    let body = fixture!("detect_issues_issue.response.json");
    assert_eq!(parse(body)["message"]["content"], json!(""));
    assert_eq!(parse(body)["done_reason"], json!("length"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(replay(
            fixture!("detect_issues_issue.response.headers"),
            body,
        ))
        .mount(&server)
        .await;

    let result = recorded_client(&server, RECORDED_MODEL)
        .detect_issues(
            DETECTION_PROMPT,
            PANIC_CHUNK,
            std::time::Duration::from_secs(30),
        )
        .await;
    assert!(
        !matches!(&result, Ok(Some(s)) if s.trim().is_empty()),
        "a reply cut off before it said anything was reported as an empty issue: {result:?}"
    );
    // The request itself is the one ahma sends today (no `think` field).
    assert_eq!(sent_chat_bodies(&server).await[0], request);
}

// ─── Context window from /api/ps ─────────────────────────────────────────────

#[tokio::test]
async fn recorded_api_ps_gives_the_loaded_context_length() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(replay(
            fixture!("api_ps.response.headers"),
            fixture!("api_ps.response.json"),
        ))
        .mount(&server)
        .await;
    // /api/show states the model's *maximum* (131072), not what is loaded.
    let show = fixture!("api_show.response.trimmed.json");
    assert_eq!(
        parse(show)["model_info"]["granite.context_length"],
        json!(131072)
    );
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(replay(fixture!("api_show.response.headers"), show))
        .mount(&server)
        .await;

    let client = recorded_client(&server, RECORDED_MODEL);
    assert_eq!(
        client.server_context().await,
        Some(ServerContext {
            server: ModelServer::Ollama,
            context_length: Some(4096),
        })
    );
    let loaded = client.loaded_local_models().await.expect("Ollama answered");
    assert_eq!(loaded, vec![RECORDED_MODEL.to_string()]);
    assert!(client.is_model_resident(&loaded));
}

#[tokio::test]
async fn recorded_api_ps_with_nothing_loaded_names_ollama_without_a_size() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(replay(
            fixture!("api_ps_empty.response.headers"),
            fixture!("api_ps_empty.response.json"),
        ))
        .mount(&server)
        .await;

    let client = recorded_client(&server, RECORDED_MODEL);
    assert_eq!(
        client.server_context().await,
        Some(ServerContext {
            server: ModelServer::Ollama,
            context_length: None,
        })
    );
    assert_eq!(client.loaded_local_models().await, Some(vec![]));
}

// ─── Errors ──────────────────────────────────────────────────────────────────

const UNKNOWN_MODEL: &str = "no-such-model:1b";

fn assert_model_not_found(err: &LlmMonitorError) {
    match err {
        LlmMonitorError::Api {
            status,
            kind,
            message,
            ..
        } => {
            assert_eq!(*status, 404);
            assert_eq!(*kind, ApiErrorKind::InvalidRequest);
            assert_eq!(message, "model 'no-such-model:1b' not found");
        }
        other => panic!("expected a typed 404, got {other:?}"),
    }
    assert!(
        !err.is_tools_rejected(),
        "a missing model is not a reason to drop tools"
    );
}

#[tokio::test]
async fn recorded_unknown_model_is_a_typed_404_on_every_path() {
    let stream_request = parse(fixture!("chat_unknown_model_stream_true.request.json"));
    let whole_request = parse(fixture!("chat_unknown_model_stream_false.request.json"));
    let server = MockServer::start().await;
    let streamed_404 = replay(
        fixture!("chat_unknown_model_stream_true.response.headers"),
        fixture!("chat_unknown_model_stream_true.response.json"),
    );
    let whole_404 = replay(
        fixture!("chat_unknown_model_stream_false.response.headers"),
        fixture!("chat_unknown_model_stream_false.response.json"),
    );
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            if body["stream"] == json!(true) {
                streamed_404.clone()
            } else {
                whole_404.clone()
            }
        })
        .mount(&server)
        .await;
    let client = recorded_client(&server, UNKNOWN_MODEL);
    let hi = [json!({"role": "user", "content": "hi"})];

    let err = client
        .chat_completion_with_tools(&hi, &[])
        .await
        .expect_err("404");
    assert_model_not_found(&err);

    let (tx, _rx) = delta_channel();
    let err = client
        .chat_completion_with_tools_streaming(&hi, &[], tx)
        .await
        .expect_err("404");
    assert_model_not_found(&err);

    // `chat_stream`'s stream is not `Unpin`; pin it to call `next`.
    let first = Box::pin(client.chat_stream(vec![ChatMessage::user("hi")], None))
        .next()
        .await
        .expect("an item");
    assert_model_not_found(&first.expect_err("404"));

    let err = client
        .detect_issues("x", "y", std::time::Duration::from_secs(30))
        .await
        .expect_err("404");
    assert_model_not_found(&err);

    // One request per call — no retry, no fallback to /v1, no second try
    // without tools — and the two the real server was sent are byte-equal.
    let all = server.received_requests().await.unwrap();
    assert_eq!(
        all.len(),
        4,
        "{:?}",
        all.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
    assert!(all.iter().all(|r| r.url.path() == "/api/chat"));
    let sent = sent_chat_bodies(&server).await;
    assert_eq!(sent[0], whole_request);
    assert_eq!(sent[1], stream_request);
}

#[tokio::test]
async fn recorded_model_without_tools_is_the_one_tools_rejection() {
    let request = parse(fixture!("chat_tools_unsupported.request.json"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(replay(
            fixture!("chat_tools_unsupported.response.headers"),
            fixture!("chat_tools_unsupported.response.json"),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let (messages, tools) = inputs_of(&request);
    let (tx, _rx) = delta_channel();
    let err = recorded_client(&server, request["model"].as_str().unwrap())
        .chat_completion_with_tools_streaming(&messages, &tools, tx)
        .await
        .expect_err("400");
    assert_eq!(err.api_kind(), Some(ApiErrorKind::InvalidRequest));
    assert!(err.is_tools_rejected(), "{err}");
    assert_eq!(sent_chat_bodies(&server).await, vec![request]);
}

#[test]
fn recorded_rejection_of_string_arguments_is_not_a_tools_rejection() {
    // What Ollama 0.35 says when an assistant tool call's arguments are a JSON
    // string (the OpenAI shape) instead of an object — the reason
    // `to_ollama_messages` converts them.
    let err = LlmMonitorError::from_api_response(
        400,
        None,
        fixture!("probe_tool_result_string_arguments.response.json"),
    );
    assert_eq!(err.api_kind(), Some(ApiErrorKind::InvalidRequest));
    assert_eq!(
        err.provider_message(),
        Some("Value looks like object, but can't find closing '}' symbol")
    );
    assert!(!err.is_tools_rejected());
}
