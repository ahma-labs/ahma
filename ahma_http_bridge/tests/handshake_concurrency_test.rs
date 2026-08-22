//! Handshake Concurrency and Race Condition Tests
//!
//! This module tests the robustness of the initial connection handshake,
//! specifically focusing on race conditions, timing issues, and ensuring
//! deterministic behavior when receiving roots from various clients.
//!
//! Key scenarios:
//! 1. Tool calls attempted *before* roots are received.
//! 2. Concurrent handshake and tool execution.
//! 3. Slow client responses to roots/list requests.
//! 4. Rapid connect/disconnect cycles.

mod common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::{
    McpTestClient, ToolCallResult, encode_file_uri, spawn_server_guard_with_deferred_sandbox,
    write_pwd_tool_config,
};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;

/// Assert that a tool call was rejected by the sandbox handshake gate:
/// HTTP 409 with JSON-RPC error code -32001.
fn assert_sandbox_gated(result: &ToolCallResult, context: &str) {
    assert!(
        !result.success,
        "{}: tool call must be rejected while the handshake is pending",
        context
    );
    let error = result.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("409"),
        "{}: expected HTTP 409 during handshake; got error {:?}",
        context,
        result.error
    );
    assert!(
        error.contains("-32001"),
        "{}: expected JSON-RPC code -32001 during handshake; got error {:?}",
        context,
        result.error
    );
}

// =============================================================================
// Test: Tool Call Before Roots Handshake
// =============================================================================

/// Test that tool calls are rejected if attempted before the roots handshake completes.
/// This ensures that no operations can bypass the sandbox check by racing the handshake.
#[tokio::test]
async fn test_tool_call_before_roots_handshake() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let tools_dir = temp_dir.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let server = spawn_server_guard_with_deferred_sandbox(&tools_dir)
        .await
        .expect("Failed to start deferred-sandbox server");
    let mut mcp_client = McpTestClient::with_url(&server.base_url());

    // 1. Send only the initialize request (not initialized notification yet).
    //    This establishes the session without triggering roots/list.
    mcp_client
        .initialize_only("test-client")
        .await
        .expect("Initialize failed");

    // 2. Try to call a tool before the handshake is complete (no initialized sent yet).
    let result = mcp_client
        .call_tool("pwd", json!({"subcommand": "default"}))
        .await;

    // 3. Verify strict gating behavior.
    assert_sandbox_gated(&result, "pre-handshake tools/call");

    // 4. Complete the handshake: open SSE first, then send initialized.
    //    This is the correct protocol order — the SSE listener must be open
    //    before the server fires roots/list on receipt of initialized.
    let roots = vec![temp_dir.path().to_path_buf()];
    mcp_client
        .complete_handshake_with_roots(&roots)
        .await
        .expect("Roots handshake failed");

    // 5. Verify tool call now works
    let result = mcp_client
        .call_tool(
            "pwd",
            json!({
                "subcommand": "default",
                "working_directory": temp_dir.path().to_string_lossy()
            }),
        )
        .await;

    assert!(
        result.success,
        "Tool call should succeed after handshake: {:?}",
        result.error
    );
}

// =============================================================================
// Test: Slow Client Handshake
// =============================================================================

/// Extract the JSON-RPC request ID from an SSE event text containing a `roots/list` request.
/// Returns `None` if the text does not contain a `roots/list` message or the ID cannot be parsed.
fn parse_roots_list_id(text: &str) -> Option<Value> {
    if !text.contains("roots/list") {
        return None;
    }
    let start = text.find("\"id\":")?;
    let rest = &text[start + 5..];
    let end = rest.find(',')?;
    let id_str = rest[..end].trim();
    if let Ok(id_num) = id_str.parse::<i64>() {
        Some(json!(id_num))
    } else {
        Some(json!(id_str.trim_matches('"')))
    }
}

/// SSE task for `test_slow_client_handshake`: opens the SSE stream, waits for the server's
/// `roots/list` request, simulates a slow client with a short delay, sends the roots
/// response, then waits for `notifications/sandbox/configured` before returning.
async fn run_slow_roots_sse_task(
    client: Client,
    base_url: String,
    session_id: String,
    root_uri: String,
) {
    let url = format!("{}/mcp", base_url);
    let resp = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Mcp-Session-Id", session_id.clone())
        .send()
        .await
        .expect("SSE connection failed");

    let mut stream = resp.bytes_stream();

    // Wait for the server's roots/list request.
    let mut request_id = None;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.expect("SSE read error");
        let text = String::from_utf8_lossy(&bytes);
        if let Some(id) = parse_roots_list_id(&text) {
            request_id = Some(id);
            break;
        }
    }

    let id = request_id.expect("Did not receive roots/list request");

    // SIMULATE a slow client (e.g. a user prompt). The delay only needs to be
    // long enough for the main task to observe the pending-handshake gate; the
    // property under test is that the server *waits* rather than timing out.
    sleep(TestTimeouts::scale_millis(250)).await;

    // Send the roots response.
    let roots_json = vec![json!({"uri": root_uri, "name": "root"})];
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"roots": roots_json}
    });
    let _ = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Mcp-Session-Id", session_id.clone())
        .json(&response)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await;

    // Wait for notifications/sandbox/configured so the sandbox is truly Active
    // before the task returns.  Without this, the retry tool call races with
    // the subprocess confirming sandbox activation and incorrectly gets 409.
    let sandbox_ready_timeout = TestTimeouts::get(TimeoutCategory::SandboxReady);
    let deadline = tokio::time::Instant::now() + sandbox_ready_timeout;
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await {
        if let Ok(bytes) = chunk {
            let text = String::from_utf8_lossy(&bytes);
            if text.contains("notifications/sandbox/configured") {
                break;
            }
        }
    }
}

/// Test that the server handles a slow client gracefully.
/// The server should wait for the roots response before allowing tool calls.
#[tokio::test]
async fn test_slow_client_handshake() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let tools_dir = temp_dir.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let server = spawn_server_guard_with_deferred_sandbox(&tools_dir)
        .await
        .expect("Failed to start deferred-sandbox server");
    let base_url = server.base_url();
    let client = common::make_h2_client();
    let mut mcp_client = McpTestClient::with_url(&base_url);

    mcp_client
        .initialize_with_name("slow-client")
        .await
        .expect("Initialize failed");
    let session_id = mcp_client.session_id().expect("No session ID").to_string();

    // Start SSE connection but DELAY sending the roots response.
    let root_uri = encode_file_uri(temp_dir.path());
    let sse_task = tokio::spawn(run_slow_roots_sse_task(
        client.clone(),
        base_url.clone(),
        session_id.clone(),
        root_uri,
    ));

    // Try to call tool during the delay - should fail with strict gating.
    sleep(TestTimeouts::short_delay()).await;
    let result = mcp_client
        .call_tool("pwd", json!({"subcommand": "default"}))
        .await;
    assert_sandbox_gated(&result, "tools/call during slow handshake");

    // Wait for handshake to complete (includes sandbox/configured).
    sse_task.await.expect("SSE task failed");

    // Now it should work.
    let result = mcp_client
        .call_tool(
            "pwd",
            json!({
                "subcommand": "default",
                "working_directory": temp_dir.path().to_string_lossy()
            }),
        )
        .await;

    assert!(
        result.success,
        "Tool call should succeed after slow handshake: {:?}",
        result.error
    );
}

// =============================================================================
// Test: Rapid Connect/Disconnect
// =============================================================================

/// Test that rapid connect/disconnect cycles don't leave the server in a bad state.
/// A failed handshake should not prevent subsequent connections from working.
///
/// Uses a multi-threaded Tokio runtime so that the HTTP/2 connection driver,
/// the SSE handshake task, and the main test code can all run concurrently
/// without scheduler starvation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rapid_connect_disconnect() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let tools_dir = temp_dir.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let server = spawn_server_guard_with_deferred_sandbox(&tools_dir)
        .await
        .expect("Failed to start deferred-sandbox server");
    let base_url = server.base_url();

    // Attempt 1: Connect, Initialize, then Abandon.
    // The client is scoped to this block so its HTTP/2 connection is dropped
    // (and the abandoned session's load is released) before Attempt 2 starts.
    {
        let mut abandoning_client = McpTestClient::with_url(&base_url);
        let _ = abandoning_client.initialize_only("abandoning-client").await;
        // client dropped here — HTTP/2 connection released, abandoned session unloaded
    }

    // Let the bridge settle after the abandoned session before starting Attempt 2.
    sleep(TestTimeouts::short_delay()).await;

    // Attempt 2: Connect immediately
    {
        // Complete handshake for second client using the correct protocol order:
        // open SSE before sending initialized so roots/list is not lost.
        let mut second_client = McpTestClient::with_url(&base_url);
        second_client
            .initialize_with_roots("second-client", &[temp_dir.path().to_path_buf()])
            .await
            .expect("Second initialize + roots handshake failed");

        // Verify tool call works
        let result = second_client
            .call_tool(
                "pwd",
                json!({
                    "subcommand": "default",
                    "working_directory": temp_dir.path().to_string_lossy()
                }),
            )
            .await;

        assert!(
            result.success,
            "Tool call should succeed for second client: {:?}",
            result.error
        );
    }
}
