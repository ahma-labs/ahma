mod common;

use common::{make_h2_client, spawn_test_server};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::oneshot;

#[tokio::test]
async fn test_routed_sampling_flow() {
    let server = spawn_test_server().await.expect("server should start");
    let http = make_h2_client();

    // 1. Initialize Target Client (Session A) which has sampling capability
    let init_a_resp = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "sampling": {}
                },
                "clientInfo": {"name": "Cursor", "version": "1.0"}
            }
        }))
        .send()
        .await
        .expect("init target request should succeed");

    assert!(init_a_resp.status().is_success());
    let session_a_id = init_a_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("missing session id header for target")
        .to_string();

    // Open target's SSE stream
    let sse_a_resp = http
        .get(format!("{}/mcp", server.base_url()))
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", &session_a_id)
        .send()
        .await
        .expect("open sse target should succeed");

    assert!(sse_a_resp.status().is_success());
    let mut sse_a_stream = sse_a_resp.bytes_stream();

    // Send notifications/initialized to target to complete handshake
    let init_notif_a = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_a_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .expect("notification/initialized A should succeed");
    assert!(init_notif_a.status().is_success());

    // 2. Initialize Agent Client (Session B)
    let init_b_resp = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "TUI", "version": "1.0"}
            }
        }))
        .send()
        .await
        .expect("init agent request should succeed");

    assert!(init_b_resp.status().is_success());
    let session_b_id = init_b_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("missing session id header for agent")
        .to_string();

    // Send notifications/initialized to agent to complete handshake
    let init_notif_b = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_b_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .expect("notification/initialized B should succeed");
    assert!(init_notif_b.status().is_success());

    // Start a background task to monitor Session A's SSE stream and respond
    let (tx_ready, rx_ready) = oneshot::channel();
    let server_url = server.base_url().to_string();
    let session_a_id_clone = session_a_id.clone();
    let http_clone = http.clone();

    let sse_task = tokio::spawn(async move {
        use futures::StreamExt;
        let mut buffer = String::new();
        let _ = tx_ready.send(());

        while let Some(Ok(chunk)) = sse_a_stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            if buffer.contains("\n\n") {
                let parts: Vec<String> = buffer.split("\n\n").map(|s| s.to_string()).collect();
                if let Some((last, elements)) = parts.split_last() {
                    buffer = last.clone();
                    for event in elements {
                        let mut data_str = String::new();
                        for line in event.lines() {
                            if let Some(data) = line.strip_prefix("data:") {
                                data_str.push_str(data.trim());
                            }
                        }
                        if data_str.is_empty() {
                            continue;
                        }
                        if let Ok(req_val) = serde_json::from_str::<Value>(&data_str) {
                            let method = req_val.get("method").and_then(|m| m.as_str());
                            let req_id = req_val.get("id").cloned();

                            if method == Some("roots/list") {
                                let response = json!({
                                    "jsonrpc": "2.0",
                                    "id": req_id,
                                    "result": {
                                        "roots": []
                                    }
                                });
                                let _ = http_clone
                                    .post(format!("{}/mcp", server_url))
                                    .header("Content-Type", "application/json")
                                    .header("Mcp-Session-Id", &session_a_id_clone)
                                    .json(&response)
                                    .send()
                                    .await;
                            } else if method == Some("sampling/createMessage")
                                || (req_id.is_some()
                                    && req_val.to_string().contains("sampling/createMessage"))
                            {
                                let route_id = req_id.unwrap();
                                let response = json!({
                                    "jsonrpc": "2.0",
                                    "id": route_id,
                                    "result": {
                                        "content": [
                                            {
                                                "type": "text",
                                                "text": "Hello from Cursor sampling!"
                                            }
                                        ],
                                        "model": "claude-3-5-sonnet",
                                        "stopReason": "stop"
                                    }
                                });

                                let res = http_clone
                                    .post(format!("{}/mcp", server_url))
                                    .header("Content-Type", "application/json")
                                    .header("Mcp-Session-Id", &session_a_id_clone)
                                    .json(&response)
                                    .send()
                                    .await
                                    .expect("Send client response failed");
                                assert!(res.status().is_success());
                            }
                        }
                    }
                }
            }
        }
    });

    // Wait for the background listener to be ready
    rx_ready.await.unwrap();

    // 3. Send sampling request from Agent (Session B) targeting "Cursor"
    let sampling_req = json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "sampling/createMessage",
        "params": {
            "messages": [
                {
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": "Hello"
                    }
                }
            ],
            "maxTokens": 100,
            "__route_target_label": "Cursor"
        }
    });

    let sampling_res = http
        .post(format!("{}/mcp", server.base_url()))
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

    // Initialize Target A (with sampling)
    let init_a_resp = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "sampling": {}
                },
                "clientInfo": {"name": "Cursor", "version": "1.0"}
            }
        }))
        .send()
        .await
        .unwrap();

    let session_a_id = init_a_resp
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Open target's SSE stream
    let sse_a_resp = http
        .get(format!("{}/mcp", server.base_url()))
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", &session_a_id)
        .send()
        .await
        .unwrap();
    let mut sse_a_stream = sse_a_resp.bytes_stream();

    // Send initialized
    let _ = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_a_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .unwrap();

    // Initialize Agent B
    let init_b_resp = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "TUI", "version": "1.0"}
            }
        }))
        .send()
        .await
        .unwrap();
    let session_b_id = init_b_resp
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let _ = http
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_b_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .unwrap();

    // Start background task to process events
    let server_url = server.base_url().to_string();
    let session_a_id_clone = session_a_id.clone();
    let http_clone = http.clone();

    let sse_task = tokio::spawn(async move {
        use futures::StreamExt;
        let mut buffer = String::new();
        while let Some(Ok(chunk)) = sse_a_stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            if buffer.contains("\n\n") {
                let parts: Vec<String> = buffer.split("\n\n").map(|s| s.to_string()).collect();
                if let Some((last, elements)) = parts.split_last() {
                    buffer = last.clone();
                    for event in elements {
                        let mut data_str = String::new();
                        for line in event.lines() {
                            if let Some(data) = line.strip_prefix("data:") {
                                data_str.push_str(data.trim());
                            }
                        }
                        if data_str.is_empty() {
                            continue;
                        }
                        if let Ok(req_val) = serde_json::from_str::<Value>(&data_str) {
                            let method = req_val.get("method").and_then(|m| m.as_str());
                            let req_id = req_val.get("id").cloned();

                            if method == Some("roots/list") {
                                let response = json!({
                                    "jsonrpc": "2.0",
                                    "id": req_id,
                                    "result": {
                                        "roots": []
                                    }
                                });
                                let _ = http_clone
                                    .post(format!("{}/mcp", server_url))
                                    .header("Content-Type", "application/json")
                                    .header("Mcp-Session-Id", &session_a_id_clone)
                                    .json(&response)
                                    .send()
                                    .await;
                            } else if method == Some("sampling/createMessage")
                                || (req_id.is_some()
                                    && req_val.to_string().contains("sampling/createMessage"))
                            {
                                let route_id = req_id.unwrap();
                                // Sleep to simulate processing time (and keep the lock held)
                                tokio::time::sleep(Duration::from_millis(500)).await;

                                let response = json!({
                                    "jsonrpc": "2.0",
                                    "id": route_id,
                                    "result": {
                                        "content": [
                                            {
                                                "type": "text",
                                                "text": "Slow response"
                                            }
                                        ]
                                    }
                                });

                                let _ = http_clone
                                    .post(format!("{}/mcp", server_url))
                                    .header("Content-Type", "application/json")
                                    .header("Mcp-Session-Id", &session_a_id_clone)
                                    .json(&response)
                                    .send()
                                    .await;
                            }
                        }
                    }
                }
            }
        }
    });

    // Send two concurrent requests from Agent B targeting "Cursor" (since Target A is registered as Cursor)
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
    let server_url = server.base_url().to_string();

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
        // Delay slightly before sending the second request to ensure task1 hits the server/acquires the lock first
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

    // Because requests are serialized and each takes 500ms, the total time must be at least 1000ms (500ms + 500ms)
    // plus network overhead. Since task2 starts 100ms later, if they were concurrent, total time would be ~600ms.
    // If serialized, it must be >= 1000ms.
    assert!(
        elapsed >= Duration::from_millis(1000),
        "Expected sequential execution to take >= 1000ms, got {:?}",
        elapsed
    );

    sse_task.abort();
}
