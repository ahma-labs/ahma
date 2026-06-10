mod common;

use common::{make_h2_client, spawn_test_server};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// POST `initialize` and return the assigned `session_id`.
///
/// `with_sampling` adds `"sampling": {}` to the capabilities block and
/// requests `Accept: text/event-stream`; callers that do not need an SSE
/// body can ignore the returned `Response`.
async fn initialize_session(
    http: &Client,
    base_url: &str,
    client_name: &str,
    with_sampling: bool,
) -> (String, reqwest::Response) {
    let capabilities = if with_sampling {
        json!({ "sampling": {} })
    } else {
        json!({})
    };
    let accept = if with_sampling {
        "text/event-stream"
    } else {
        "application/json"
    };

    let resp = http
        .post(format!("{}/mcp", base_url))
        .header("Content-Type", "application/json")
        .header("Accept", accept)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": capabilities,
                "clientInfo": {"name": client_name, "version": "1.0"}
            }
        }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("initialize request for {client_name} failed: {e}"));

    assert!(
        resp.status().is_success(),
        "initialize response for {client_name} was not 2xx"
    );

    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| panic!("missing mcp-session-id header for {client_name}"))
        .to_string();

    (session_id, resp)
}

/// POST `notifications/initialized` for `session_id`.
async fn send_initialized(http: &Client, base_url: &str, session_id: &str) {
    let resp = http
        .post(format!("{}/mcp", base_url))
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("notifications/initialized for {session_id} failed: {e}"));
    assert!(resp.status().is_success());
}

/// Open the GET /mcp SSE stream for `session_id`.
async fn open_sse_stream(
    http: Client,
    base_url: String,
    session_id: String,
) -> impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> {
    let resp = http
        .get(format!("{}/mcp", base_url))
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", session_id.clone())
        .send()
        .await
        .unwrap_or_else(|e| panic!("open SSE for {session_id} failed: {e}"));
    assert!(resp.status().is_success());
    resp.bytes_stream()
}

/// Extract the concatenated `data:` payload from one SSE event block.
fn extract_sse_data(event: &str) -> String {
    let mut out = String::new();
    for line in event.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            out.push_str(data.trim());
        }
    }
    out
}

/// Drive an SSE byte-stream, calling `on_event` for each decoded JSON event.
///
/// The loop exits when the stream ends.  `on_event` receives the parsed
/// `Value` and returns a future whose output is `()`.
async fn run_sse_event_loop<F, Fut>(
    mut stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    mut on_event: F,
) where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut buffer = String::new();

    while let Some(Ok(chunk)) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        if !buffer.contains("\n\n") {
            continue;
        }

        let parts: Vec<String> = buffer.split("\n\n").map(|s| s.to_string()).collect();
        if let Some((remainder, events)) = parts.split_last() {
            buffer = remainder.clone();
            for event in events {
                let data_str = extract_sse_data(event);
                if data_str.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<Value>(&data_str) {
                    on_event(val).await;
                }
            }
        }
    }
}

/// Reply to a `roots/list` server request on `session_id`.
async fn reply_roots_list(http: &Client, base_url: &str, session_id: &str, req_id: Value) {
    let _ = http
        .post(format!("{}/mcp", base_url))
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": { "roots": [] }
        }))
        .send()
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_routed_sampling_flow() {
    let server = spawn_test_server().await.expect("server should start");
    let http = make_h2_client();
    let base = server.base_url();

    // 1. Initialize Target Client (Session A) which has sampling capability
    let (session_a_id, _) = initialize_session(&http, &*base, "Cursor", true).await;
    let sse_a_stream = open_sse_stream(http.clone(), base.clone(), session_a_id.clone()).await;
    send_initialized(&http, &*base, &session_a_id).await;

    // 2. Initialize Agent Client (Session B)
    let (session_b_id, _) = initialize_session(&http, &*base, "TUI", false).await;
    send_initialized(&http, &*base, &session_b_id).await;

    // Monitor Session A's SSE stream in the background and respond to requests
    let (tx_ready, rx_ready) = oneshot::channel();
    let server_url = base.to_string();
    let session_a_id_bg = session_a_id.clone();
    let http_bg = http.clone();

    let sse_task = tokio::spawn(async move {
        let _ = tx_ready.send(());

        run_sse_event_loop(Box::pin(sse_a_stream), |req_val: Value| {
            let http = http_bg.clone();
            let server_url = server_url.clone();
            let session_a_id = session_a_id_bg.clone();
            async move {
                let method = req_val.get("method").and_then(|m| m.as_str());
                let req_id = req_val.get("id").cloned();

                if method == Some("roots/list") {
                    reply_roots_list(
                        &http,
                        &server_url,
                        &session_a_id,
                        req_id.unwrap_or(Value::Null),
                    )
                    .await;
                } else if method == Some("sampling/createMessage")
                    || (req_id.is_some() && req_val.to_string().contains("sampling/createMessage"))
                {
                    let route_id = req_id.unwrap();
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": route_id,
                        "result": {
                            "content": [{"type": "text", "text": "Hello from Cursor sampling!"}],
                            "model": "claude-3-5-sonnet",
                            "stopReason": "stop"
                        }
                    });
                    let res = http
                        .post(format!("{}/mcp", server_url))
                        .header("Content-Type", "application/json")
                        .header("Mcp-Session-Id", &session_a_id)
                        .json(&response)
                        .send()
                        .await
                        .expect("Send client response failed");
                    assert!(res.status().is_success());
                }
            }
        })
        .await;
    });

    // Wait for the background listener to be ready
    rx_ready.await.unwrap();

    // 3. Send sampling request from Agent (Session B) targeting "Cursor"
    let sampling_req = json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "sampling/createMessage",
        "params": {
            "messages": [{"role": "user", "content": {"type": "text", "text": "Hello"}}],
            "maxTokens": 100,
            "__route_target_label": "Cursor"
        }
    });

    let sampling_res = http
        .post(format!("{}/mcp", base))
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_b_id)
        .json(&sampling_req)
        .send()
        .await
        .expect("Sampling request should succeed");

    assert!(sampling_res.status().is_success());
    let res_body: Value = sampling_res.json().await.expect("Valid JSON response");

    // Assert routed response payload
    let text = res_body
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str());

    assert_eq!(text, Some("Hello from Cursor sampling!"));

    sse_task.abort();
}

#[tokio::test]
async fn test_sampling_concurrency_lock() {
    let server = spawn_test_server().await.expect("server should start");
    let http = make_h2_client();
    let base = server.base_url();

    // Initialize Target A (with sampling) and Agent B
    let (session_a_id, _) = initialize_session(&http, &*base, "Cursor", true).await;
    let sse_a_stream = open_sse_stream(http.clone(), base.clone(), session_a_id.clone()).await;
    send_initialized(&http, &*base, &session_a_id).await;

    let (session_b_id, _) = initialize_session(&http, &*base, "TUI", false).await;
    send_initialized(&http, &*base, &session_b_id).await;

    // Start background task to process events; sleep before each sampling response
    // to keep the per-session lock held long enough to prove serialization.
    let server_url = base.to_string();
    let session_a_id_bg = session_a_id.clone();
    let http_bg = http.clone();

    let sse_task = tokio::spawn(async move {
        run_sse_event_loop(Box::pin(sse_a_stream), |req_val: Value| {
            let http = http_bg.clone();
            let server_url = server_url.clone();
            let session_a_id = session_a_id_bg.clone();
            async move {
                let method = req_val.get("method").and_then(|m| m.as_str());
                let req_id = req_val.get("id").cloned();

                if method == Some("roots/list") {
                    reply_roots_list(
                        &http,
                        &server_url,
                        &session_a_id,
                        req_id.unwrap_or(Value::Null),
                    )
                    .await;
                } else if method == Some("sampling/createMessage")
                    || (req_id.is_some() && req_val.to_string().contains("sampling/createMessage"))
                {
                    let route_id = req_id.unwrap();
                    // Sleep to simulate processing time (holds the per-session lock)
                    tokio::time::sleep(Duration::from_millis(500)).await;

                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": route_id,
                        "result": {
                            "content": [{"type": "text", "text": "Slow response"}]
                        }
                    });
                    let _ = http
                        .post(format!("{}/mcp", server_url))
                        .header("Content-Type", "application/json")
                        .header("Mcp-Session-Id", &session_a_id)
                        .json(&response)
                        .send()
                        .await;
                }
            }
        })
        .await;
    });

    // Send two concurrent requests from Agent B targeting "Cursor"
    let sampling_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sampling/createMessage",
        "params": {
            "messages": [{"role": "user", "content": {"type": "text", "text": "A"}}],
            "maxTokens": 100,
            "__route_target_label": "Cursor"
        }
    });

    let start_time = tokio::time::Instant::now();
    let server_url = base.to_string();

    let http_1 = http.clone();
    let session_b_1 = session_b_id.clone();
    let req_1 = sampling_req.clone();
    let server_url_1 = server_url.clone();
    let task1 = tokio::spawn(async move {
        http_1
            .post(format!("{}/mcp", server_url_1))
            .header("Content-Type", "application/json")
            .header("Mcp-Session-Id", &session_b_1)
            .json(&req_1)
            .send()
            .await
            .unwrap()
    });

    let http_2 = http.clone();
    let session_b_2 = session_b_id.clone();
    let req_2 = sampling_req.clone();
    let server_url_2 = server_url.clone();
    let task2 = tokio::spawn(async move {
        // Delay slightly to ensure task1 acquires the lock first
        tokio::time::sleep(Duration::from_millis(100)).await;
        http_2
            .post(format!("{}/mcp", server_url_2))
            .header("Content-Type", "application/json")
            .header("Mcp-Session-Id", &session_b_2)
            .json(&req_2)
            .send()
            .await
            .unwrap()
    });

    let res1 = task1.await.unwrap();
    let res2 = task2.await.unwrap();

    let elapsed = start_time.elapsed();

    assert!(res1.status().is_success());
    assert!(res2.status().is_success());

    // Because requests are serialized and each takes 500ms, the total time must be at least
    // 1000ms (500ms + 500ms). If they ran concurrently the total would be ~600ms.
    assert!(
        elapsed >= Duration::from_millis(1000),
        "Expected sequential execution to take >= 1000ms, got {:?}",
        elapsed
    );

    sse_task.abort();
}
