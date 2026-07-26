//! Sandbox and Roots Handshake Integration Tests
//!
//! This module provides comprehensive test coverage for the critical sandbox
//! initialization path via the MCP roots/list protocol. These tests verify:
//!
//! 1. **Empty Roots Rejection**: Sessions with empty roots are rejected with clear error
//! 2. **Malformed URI Handling**: Invalid file:// URIs are handled gracefully
//! 3. **Multi-Root Workspace Scoping**: Multiple workspace roots are handled correctly
//! 4. **Post-Lock Roots Rejection**: Attempts to change roots after lock are rejected
//! 5. **Client-Specific Handshake Simulation**: Different MCP clients (VSCode, Cursor)
//! 6. **Race Condition Prevention**: Concurrent roots/list requests are handled atomically
//!
//! ## Security Critical
//!
//! These tests are security-critical. Do NOT:
//! - Weaken assertions to accept sandbox failures as "passing"
//! - Add test-mode env var bypasses (see SPEC.md R21.3)
//! - Remove environment variable clearing (see AGENTS.md guardrails)
//!
//! ## Test Environment
//!
//! All tests use `SandboxTestEnv::configure()` to ensure spawned ahma_mcp
//! processes test real sandbox behavior, not the permissive test mode.

mod common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::{
    SANDBOX_BYPASS_ENV_VARS, SandboxTestEnv, ServerGuard, encode_file_uri, malformed_uris,
    parse_file_uri, spawn_server_guard_strict_roots, spawn_server_guard_with_deferred_sandbox,
    write_pwd_tool_config,
};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

fn roots_handshake_timeout() -> Duration {
    // SandboxReady (60s base ⇒ 240s on Windows ×4), NOT SseStream (120s base ⇒
    // 480s): this bounds an in-test handshake/readiness loop and must fire before
    // the 360s nextest backstop, else a slow Windows subprocess presents as an
    // opaque TIMEOUT[360s] instead of a clean failure.  See
    // ahma_common::timeouts::NEXTEST_CI_HARD_KILL_SECS.
    TestTimeouts::get(TimeoutCategory::SandboxReady)
}

fn server_base_url(server: &ServerGuard) -> String {
    format!("http://127.0.0.1:{}", server.port())
}

async fn start_initialized_session(tools_dir: &Path) -> (ServerGuard, String, Client, String) {
    let server = spawn_server_guard_with_deferred_sandbox(tools_dir)
        .await
        .expect("Failed to start deferred-sandbox server");
    initialize_against(server).await
}

/// Same as [`start_initialized_session`] but with **no** explicit fallback scope,
/// so a client that supplies no usable roots never gets a locked sandbox.
async fn start_initialized_session_strict_roots(
    tools_dir: &Path,
) -> (ServerGuard, String, Client, String) {
    let server = spawn_server_guard_strict_roots(tools_dir)
        .await
        .expect("Failed to start strict-roots server");
    initialize_against(server).await
}

async fn initialize_against(server: ServerGuard) -> (ServerGuard, String, Client, String) {
    let base_url = server_base_url(&server);
    let client = common::make_h2_client();
    let session_id = initialize_session(&client, &base_url)
        .await
        .expect("Initialize failed");
    (server, base_url, client, session_id)
}

fn pwd_tool_call(id: u64, working_directory: Option<&Path>) -> Value {
    match working_directory {
        Some(working_directory) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "pwd",
                "arguments": {
                    "subcommand": "default",
                    "working_directory": working_directory.to_string_lossy()
                }
            }
        }),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "pwd",
                "arguments": {"subcommand": "default"}
            }
        }),
    }
}

fn spawn_complete_roots_handshake(
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: Vec<String>,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let client = client.clone();
    let base_url = base_url.to_string();
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        complete_roots_handshake_with_uris(&client, &base_url, &session_id, &root_uris).await
    })
}

/// Send a JSON-RPC request to the MCP endpoint
async fn send_mcp_request(
    client: &Client,
    base_url: &str,
    request: &Value,
    session_id: Option<&str>,
) -> Result<(Value, Option<String>), String> {
    let url = format!("{}/mcp", base_url);

    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest));

    if let Some(id) = session_id {
        req = req.header("Mcp-Session-Id", id);
    }

    let response = req
        .json(request)
        .send()
        .await
        .map_err(|e| format!("Request failed: {:?}", e))?;

    let status = response.status();
    let new_session_id = response
        .headers()
        .get("mcp-session-id")
        .or_else(|| response.headers().get("Mcp-Session-Id"))
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, text));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    Ok((body, new_session_id))
}

/// Send only initialize and return the session ID.
async fn initialize_session(client: &Client, base_url: &str) -> Result<String, String> {
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "test-client", "version": "1.0.0"}
        }
    });

    let (_, session_id) = send_mcp_request(client, base_url, &init_request, None).await?;
    session_id.ok_or_else(|| "No session ID received".to_string())
}

async fn send_initialized_notification(
    client: &Client,
    base_url: &str,
    session_id: &str,
) -> Result<(), String> {
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = send_mcp_request(client, base_url, &initialized, Some(session_id)).await;
    Ok(())
}

/// Wait for sandbox readiness by polling a known-good tool call.
async fn wait_for_tool_ready(
    client: &Client,
    base_url: &str,
    session_id: &str,
    working_directory: &Path,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + roots_handshake_timeout();
    let mut last_error: Option<String> = None;

    while tokio::time::Instant::now() < deadline {
        let tool_call = pwd_tool_call(9001, Some(working_directory));

        match send_mcp_request(client, base_url, &tool_call, Some(session_id)).await {
            Ok((response, _)) if response.get("error").is_none() => return Ok(()),
            Ok((response, _)) => {
                last_error = Some(
                    response["error"]["message"]
                        .as_str()
                        .unwrap_or("tool call returned error")
                        .to_string(),
                );
            }
            Err(e) => last_error = Some(e),
        }

        sleep(TestTimeouts::poll_interval()).await;
    }

    Err(format!(
        "Timeout waiting for sandbox/tool readiness: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

/// Parse the `data:` lines from a raw SSE event block and deserialize the JSON payload.
fn parse_sse_event_data(raw_event: &str) -> Option<Value> {
    let data: String = raw_event
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter_map(|l| l.strip_prefix("data:").map(str::trim))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&data).ok()
}

/// Dispatch a parsed SSE event. Returns `Some(result)` when the exchange is
/// complete (success or error) and `None` to keep reading the stream.
///
/// Completion requires BOTH:
/// - `roots/list` has been answered, AND
/// - `notifications/sandbox/configured` has been received
///
/// This mirrors `common/client.rs::handle_roots_handshake_event` and prevents
/// `wait_for_tool_ready` from polling while the sandbox is still in `Configuring`
/// state on slow Windows CI runners.
async fn handle_sse_event(
    value: Value,
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: &[String],
    roots_answered: &mut bool,
    configured_seen: &mut bool,
) -> Option<Result<(), String>> {
    let method = value.get("method").and_then(|m| m.as_str());

    if method == Some("notifications/sandbox/failed") {
        let error = value
            .get("params")
            .and_then(|p| p.get("error"))
            .and_then(|e| e.as_str())
            .unwrap_or("unknown");
        return Some(Err(format!("Sandbox configuration failed: {}", error)));
    }

    if method == Some("notifications/sandbox/configured") {
        *configured_seen = true;
        if *roots_answered {
            return Some(Ok(()));
        }
        return None;
    }

    if method == Some("roots/list") {
        let id = match value.get("id").cloned() {
            Some(id) => id,
            None => return Some(Err("roots/list must include id".to_string())),
        };
        let roots_json: Vec<Value> = root_uris
            .iter()
            .map(|uri| json!({"uri": uri, "name": "root"}))
            .collect();
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"roots": roots_json}
        });
        if let Err(e) = send_mcp_request(client, base_url, &response, Some(session_id))
            .await
            .map(|_| ())
        {
            return Some(Err(e));
        }
        *roots_answered = true;
        if *configured_seen {
            return Some(Ok(()));
        }
        return None;
    }

    None
}

async fn open_roots_sse_stream(
    client: &Client,
    base_url: &str,
    session_id: &str,
) -> Result<reqwest::Response, String> {
    let url = format!("{}/mcp", base_url);
    // No reqwest .timeout() here: SSE is an infinite stream, so a reqwest timeout fires on the
    // body read phase and aborts the connection before roots/list arrives. The internal deadline
    // inside process_roots_list_response handles the per-test time budget.
    let resp = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Mcp-Session-Id", session_id)
        .send()
        .await
        .map_err(|e| format!("SSE connection failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("SSE stream failed: HTTP {}", resp.status()));
    }

    Ok(resp)
}

/// Return the position and byte-length of the first SSE event boundary in `buffer`.
///
/// The SSE spec allows either LF-only (`\n\n`) or CRLF (`\r\n\r\n`) as the blank-line
/// separator between events. Both forms must be handled to avoid missing events when
/// the OS or HTTP stack uses CRLF line endings (observed on Windows CI runners).
fn first_sse_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|i| (i, 2usize));
    let crlf = buffer.find("\r\n\r\n").map(|i| (i, 4usize));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Drain complete SSE events from `buffer`, dispatching each via `handle_sse_event`.
/// Returns `Some(result)` when the exchange completes, or `None` to keep reading.
async fn drain_sse_buffer(
    buffer: &mut String,
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: &[String],
    roots_answered: &mut bool,
    configured_seen: &mut bool,
) -> Option<Result<(), String>> {
    loop {
        let (idx, delim_len) = first_sse_boundary(buffer)?;
        let raw_event = buffer[..idx].to_string();
        *buffer = buffer[idx + delim_len..].to_string();

        let Some(value) = parse_sse_event_data(&raw_event) else {
            continue;
        };

        if let Some(result) = handle_sse_event(
            value,
            client,
            base_url,
            session_id,
            root_uris,
            roots_answered,
            configured_seen,
        )
        .await
        {
            return Some(result);
        }
    }
}

async fn process_roots_list_response(
    resp: reqwest::Response,
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: &[String],
) -> Result<(), String> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut roots_answered = false;
    let mut configured_seen = false;
    let deadline = tokio::time::Instant::now() + roots_handshake_timeout();

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err("Timeout waiting for roots/list + sandbox/configured over SSE".to_string());
        }

        let chunk = match tokio::time::timeout(TestTimeouts::poll_interval(), stream.next()).await {
            Err(_elapsed) => continue, // poll window elapsed, no data yet — try again
            Ok(None) => {
                return Err(
                    "SSE handshake stream closed by server before handshake completed".to_string(),
                );
            }
            Ok(Some(chunk)) => chunk,
        };

        let bytes = chunk.map_err(|e| format!("SSE read error: {}", e))?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        if let Some(result) = drain_sse_buffer(
            &mut buffer,
            client,
            base_url,
            session_id,
            root_uris,
            &mut roots_answered,
            &mut configured_seen,
        )
        .await
        {
            return result;
        }
    }
}

/// Process the SSE stream for the roots/list exchange ONLY.
///
/// Returns after the `roots/list` response POST succeeds, without waiting for
/// `notifications/sandbox/configured`. Use this for failure-path tests where the
/// sandbox will never configure (empty roots, all-malformed URIs) so that the
/// test does not wait for a `notifications/sandbox/configured` that will never arrive.
async fn process_roots_exchange_stream(
    resp: reqwest::Response,
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: &[String],
) -> Result<(), String> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut roots_answered = false;
    // configured_seen is intentionally unused: we return as soon as roots are answered.
    let mut configured_seen = false;
    let deadline = tokio::time::Instant::now() + roots_handshake_timeout();

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err("Timeout waiting for roots/list over SSE".to_string());
        }

        let chunk = match tokio::time::timeout(TestTimeouts::poll_interval(), stream.next()).await {
            Err(_elapsed) => continue,
            Ok(None) => {
                return Err("SSE stream closed before roots/list arrived".to_string());
            }
            Ok(Some(chunk)) => chunk,
        };

        let bytes = chunk.map_err(|e| format!("SSE read error: {}", e))?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        // After roots_answered, stop — do not wait for sandbox/configured.
        if let Some(result) = drain_sse_buffer(
            &mut buffer,
            client,
            base_url,
            session_id,
            root_uris,
            &mut roots_answered,
            &mut configured_seen,
        )
        .await
        {
            return result;
        }

        if roots_answered {
            return Ok(());
        }
    }
}

/// Open SSE after the client has already sent notifications/initialized and answer roots/list.
async fn answer_roots_list_with_uris(
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: &[String],
) -> Result<(), String> {
    let resp = open_roots_sse_stream(client, base_url, session_id).await?;
    process_roots_list_response(resp, client, base_url, session_id, root_uris).await
}

/// Complete the normal roots handshake in the safe order: open SSE first, then send initialized.
///
/// Waits for BOTH `roots/list` and `notifications/sandbox/configured` before returning, so
/// callers can be certain the sandbox is `Active` when this resolves.
async fn complete_roots_handshake_with_uris(
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: &[String],
) -> Result<(), String> {
    let resp = open_roots_sse_stream(client, base_url, session_id).await?;
    sleep(TestTimeouts::short_delay()).await;
    send_initialized_notification(client, base_url, session_id).await?;
    process_roots_list_response(resp, client, base_url, session_id, root_uris).await
}

/// Complete only the roots/list exchange (no `notifications/sandbox/configured` wait).
///
/// Use this for failure-path tests (empty roots, all-malformed URIs) where the sandbox
/// will never reach `Active` and `notifications/sandbox/configured` will never arrive.
async fn complete_roots_exchange_with_uris(
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: &[String],
) -> Result<(), String> {
    let resp = open_roots_sse_stream(client, base_url, session_id).await?;
    sleep(TestTimeouts::short_delay()).await;
    send_initialized_notification(client, base_url, session_id).await?;
    process_roots_exchange_stream(resp, client, base_url, session_id, root_uris).await
}

fn spawn_complete_roots_exchange(
    client: &Client,
    base_url: &str,
    session_id: &str,
    root_uris: Vec<String>,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let client = client.clone();
    let base_url = base_url.to_string();
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        complete_roots_exchange_with_uris(&client, &base_url, &session_id, &root_uris).await
    })
}

// =============================================================================
// Test: Empty Roots Rejection
// =============================================================================

/// SECURITY TEST: Empty roots/list response must be rejected with clear error.
///
/// If a client returns an empty roots list, the session should be rejected
/// because there's no valid sandbox scope to use. This prevents accidental
/// over-permissive behavior.
///
/// The server is started in **strict-roots** mode (no `--sandbox-scope`). With an
/// explicit fallback scope configured, empty roots is *not* a rejection: the
/// bridge locks the sandbox from the fallback a few milliseconds later and the
/// `tools/call` below succeeds. This test used to pass only by beating that lock,
/// and failed under load once the lock won — reporting whatever the subprocess
/// answered after per-client tool discovery had replaced the synthetic tool set
/// ("Tool 'pwd' not found"). Removing the fallback makes the rejection real.
#[tokio::test]
async fn test_empty_roots_rejection() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let tools_dir = temp_dir.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let (_server, base_url, client, session_id) =
        start_initialized_session_strict_roots(&tools_dir).await;

    // Use exchange-only variant: sandbox/configured will never arrive for empty roots,
    // so we must not wait for it.
    let sse_task = spawn_complete_roots_exchange(&client, &base_url, &session_id, vec![]);

    // Give time for roots/list exchange
    sleep(TestTimeouts::short_delay()).await;
    let _ = sse_task.await;

    // Try to call a tool - should fail because sandbox wasn't initialized
    let tool_call = pwd_tool_call(2, None);

    let result = send_mcp_request(&client, &base_url, &tool_call, Some(&session_id)).await;

    match result {
        Ok((response, _)) => {
            // Should have an error about sandbox not being initialized
            let error = response.get("error");
            assert!(
                error.is_some(),
                "Expected error for empty roots, got success: {:?}",
                response
            );
            let error_msg = error
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            eprintln!("Got expected error: {}", error_msg);
            // The error should mention sandbox, roots, or initialization
            let mentions_issue = error_msg.contains("sandbox")
                || error_msg.contains("Sandbox")
                || error_msg.contains("roots")
                || error_msg.contains("initializ");
            assert!(
                mentions_issue,
                "Error should mention sandbox/roots issue, got: {}",
                error_msg
            );
        }
        Err(e) => {
            // HTTP-level error is also acceptable (e.g., 403 Forbidden, 409 Conflict)
            eprintln!("Got HTTP error (acceptable): {}", e);
            let e_lower = e.to_lowercase();
            assert!(
                e.contains("403")
                    || e.contains("400")
                    || e.contains("409")
                    || e_lower.contains("sandbox"),
                "Expected 403/400/409 or sandbox-related error, got: {}",
                e
            );
        }
    }
}

// =============================================================================
// Test: Malformed URI Edge Cases
// =============================================================================

/// Test that malformed file:// URIs are handled gracefully.
///
/// Invalid URIs should be filtered out, not cause crashes or unexpected behavior.
#[tokio::test]
async fn test_malformed_uri_parsing() {
    // Test the parsing function directly first
    for invalid_uri in malformed_uris::INVALID {
        let result = parse_file_uri(invalid_uri);
        assert!(
            result.is_none(),
            "Expected None for invalid URI '{}', got {:?}",
            invalid_uri,
            result
        );
    }

    // Test edge cases
    for (uri, expected_path) in malformed_uris::EDGE_CASES {
        let result = parse_file_uri(uri);
        match expected_path {
            Some(expected) => {
                assert!(
                    result.is_some(),
                    "Expected Some for URI '{}', got None",
                    uri
                );
                let path = result.unwrap();
                assert_eq!(
                    path.to_string_lossy(),
                    *expected,
                    "Path mismatch for URI '{}'",
                    uri
                );
            }
            None => {
                assert!(
                    result.is_none(),
                    "Expected None for URI '{}', got {:?}",
                    uri,
                    result
                );
            }
        }
    }
}

/// Test that a session with only malformed URIs is rejected.
#[tokio::test]
async fn test_session_with_only_malformed_uris() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let tools_dir = temp_dir.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let (_server, base_url, client, session_id) = start_initialized_session(&tools_dir).await;

    // Answer roots/list with only malformed URIs
    let malformed_uris = vec![
        "http://not-a-file-uri/path".to_string(),
        "ftp://also-wrong/file".to_string(),
        "".to_string(),
    ];

    // Use exchange-only variant: all URIs are malformed, sandbox/configured will never arrive.
    let sse_task = spawn_complete_roots_exchange(&client, &base_url, &session_id, malformed_uris);

    sleep(TestTimeouts::short_delay()).await;
    let _ = sse_task.await;

    // Tool call should fail - no valid roots
    let tool_call = pwd_tool_call(2, None);

    let result = send_mcp_request(&client, &base_url, &tool_call, Some(&session_id)).await;

    match result {
        Ok((response, _)) => {
            let error = response.get("error");
            assert!(
                error.is_some(),
                "Expected error for malformed-only roots, got success: {:?}",
                response
            );
        }
        Err(e) => {
            eprintln!("Got HTTP error (acceptable for malformed URIs): {}", e);
        }
    }
}

// =============================================================================
// Test: Multi-Root Workspace Scoping
// =============================================================================

/// Test that multiple valid roots are all accepted for sandbox scoping.
#[tokio::test]
async fn test_multi_root_workspace_scoping() {
    let root1 = TempDir::new().expect("Failed to create temp dir 1");
    let root2 = TempDir::new().expect("Failed to create temp dir 2");
    let tools_temp = TempDir::new().expect("Failed to create tools temp dir");
    let tools_dir = tools_temp.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    // Create test file in root2 to prove it's accessible
    std::fs::write(root2.path().join("test.txt"), "hello").expect("Failed to create test file");

    let (_server, base_url, client, session_id) = start_initialized_session(&tools_dir).await;

    // Answer roots/list with both roots
    let root_uris = vec![encode_file_uri(root1.path()), encode_file_uri(root2.path())];

    let sse_task = spawn_complete_roots_handshake(&client, &base_url, &session_id, root_uris);

    // Wait for roots exchange
    let sse_result = sse_task.await.expect("SSE task panicked");
    assert!(
        sse_result.is_ok(),
        "Roots exchange failed: {:?}",
        sse_result
    );

    wait_for_tool_ready(&client, &base_url, &session_id, root1.path())
        .await
        .expect("Sandbox should become ready for tool calls");

    // Tool call in root1 should work
    let tool_call_1 = pwd_tool_call(2, Some(root1.path()));

    let (response1, _) = send_mcp_request(&client, &base_url, &tool_call_1, Some(&session_id))
        .await
        .expect("Tool call 1 request failed");

    assert!(
        response1.get("error").is_none(),
        "Tool call in root1 should succeed: {:?}",
        response1
    );

    // Tool call in root2 should also work
    let tool_call_2 = pwd_tool_call(3, Some(root2.path()));

    let (response2, _) = send_mcp_request(&client, &base_url, &tool_call_2, Some(&session_id))
        .await
        .expect("Tool call 2 request failed");

    assert!(
        response2.get("error").is_none(),
        "Tool call in root2 should succeed: {:?}",
        response2
    );
}

// =============================================================================
// Test: URL-Encoded Paths
// =============================================================================

/// Test that paths with spaces and special characters work correctly.
#[tokio::test]
async fn test_url_encoded_path_in_roots() {
    // Create a temp dir with spaces in the name
    let base_temp = TempDir::new().expect("Failed to create temp dir");
    let special_path = base_temp.path().join("my project");
    std::fs::create_dir_all(&special_path).expect("Failed to create special dir");

    let tools_dir = base_temp.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let (_server, base_url, client, session_id) = start_initialized_session(&tools_dir).await;

    // Create properly encoded URI with space
    let root_uri = encode_file_uri(&special_path);
    assert!(
        root_uri.contains("%20"),
        "URI should contain encoded space: {}",
        root_uri
    );

    let sse_task = spawn_complete_roots_handshake(&client, &base_url, &session_id, vec![root_uri]);

    let sse_result = sse_task.await.expect("SSE task panicked");
    assert!(
        sse_result.is_ok(),
        "Roots exchange failed: {:?}",
        sse_result
    );

    wait_for_tool_ready(&client, &base_url, &session_id, &special_path)
        .await
        .expect("Sandbox should become ready for URL-encoded root");

    // Tool call in the special path should work
    let tool_call = pwd_tool_call(2, Some(&special_path));

    let (response, _) = send_mcp_request(&client, &base_url, &tool_call, Some(&session_id))
        .await
        .expect("Tool call request failed");

    assert!(
        response.get("error").is_none(),
        "Tool call in path with spaces should succeed: {:?}",
        response
    );

    // Verify the output contains the special path
    let output = response
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");

    assert!(
        output.contains("my project"),
        "Output should contain 'my project': {}",
        output
    );
}

// =============================================================================
// Test: SandboxTestEnv Helper
// =============================================================================

/// Verify that SandboxTestEnv correctly identifies bypass vars.
#[test]
fn test_sandbox_test_env_detection() {
    // This test verifies the helper works correctly
    let vars = SandboxTestEnv::current_bypass_vars();
    eprintln!("Current bypass vars: {:?}", vars);

    // In test environment, some of these are likely set
    // The important thing is the detection works
    assert!(
        SANDBOX_BYPASS_ENV_VARS.len() == 3,
        "Should have 3 bypass vars defined"
    );
}

/// Verify Command configuration removes expected env vars.
#[test]
fn test_sandbox_test_env_configure() {
    let mut cmd = Command::new("true");
    SandboxTestEnv::configure(&mut cmd);
    // Can't easily verify env removal, but at least verify it doesn't panic
}

// =============================================================================
// Test: File URI Encoding/Decoding Roundtrip
// =============================================================================

#[test]
fn test_file_uri_roundtrip() {
    let test_paths = [
        "/tmp/simple",
        "/tmp/with spaces",
        "/tmp/unicode/日本語",
        "/tmp/special!@#$%^&()",
        "/home/user/my project/src",
    ];

    for path_str in test_paths {
        let path = PathBuf::from(path_str);
        let uri = encode_file_uri(&path);
        let decoded = parse_file_uri(&uri);

        assert!(
            decoded.is_some(),
            "Failed to decode URI for path: {}",
            path_str
        );
        assert_eq!(
            decoded.unwrap().to_string_lossy(),
            path_str,
            "Roundtrip failed for path: {}",
            path_str
        );
    }
}

// =============================================================================
// Test: Client Handshake Simulation (VSCode vs Cursor ordering)
// =============================================================================

/// Different MCP clients may send SSE-first or MCP-first.
/// This test verifies both orderings work correctly.
#[tokio::test]
async fn test_handshake_ordering_sse_first() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let project_dir = temp_dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("Failed to create project dir");

    let tools_dir = temp_dir.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let _server = spawn_server_guard_with_deferred_sandbox(&tools_dir)
        .await
        .expect("Failed to start deferred-sandbox server");
    let base_url = server_base_url(&_server);
    let client = common::make_h2_client();

    // VSCode Copilot style: Initialize, then SSE connects and answers roots/list

    // Step 1: Initialize
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"roots": {"listChanged": true}},
            "clientInfo": {"name": "vscode-copilot-simulation", "version": "1.0.0"}
        }
    });

    let (_, session_id) = send_mcp_request(&client, &base_url, &init_request, None)
        .await
        .expect("Initialize failed");
    let session_id = session_id.expect("No session ID");

    // Step 2: Send initialized notification
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = send_mcp_request(&client, &base_url, &initialized, Some(&session_id)).await;

    // Step 3: Open SSE and answer roots/list
    let root_uri = encode_file_uri(&project_dir);
    let sse_client = client.clone();
    let sse_base_url = base_url.clone();
    let sse_session_id = session_id.clone();
    let sse_task = tokio::spawn(async move {
        answer_roots_list_with_uris(&sse_client, &sse_base_url, &sse_session_id, &[root_uri]).await
    });

    let sse_result = sse_task.await.expect("SSE task panicked");
    assert!(
        sse_result.is_ok(),
        "Roots exchange failed: {:?}",
        sse_result
    );

    wait_for_tool_ready(&client, &base_url, &session_id, &project_dir)
        .await
        .expect("Sandbox should become ready for VSCode-style ordering");

    // Step 4: Verify tool call works
    let tool_call = pwd_tool_call(2, Some(&project_dir));

    let (response, _) = send_mcp_request(&client, &base_url, &tool_call, Some(&session_id))
        .await
        .expect("Tool call failed");

    assert!(
        response.get("error").is_none(),
        "Tool call should succeed after VSCode-style handshake: {:?}",
        response
    );
}

// =============================================================================
// Test: Mixed Valid and Invalid URIs
// =============================================================================

/// Test that a mix of valid and invalid URIs works (valid ones are used).
#[tokio::test]
async fn test_mixed_valid_invalid_uris() {
    let valid_root = TempDir::new().expect("Failed to create temp dir");
    let tools_temp = TempDir::new().expect("Failed to create tools temp dir");
    let tools_dir = tools_temp.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let (_server, base_url, client, session_id) = start_initialized_session(&tools_dir).await;

    // Mix of valid and invalid URIs
    let root_uris = vec![
        "http://invalid/not-file-scheme".to_string(),
        encode_file_uri(valid_root.path()), // This one is valid
        "ftp://also-invalid/path".to_string(),
        "".to_string(),
    ];

    let sse_task = spawn_complete_roots_handshake(&client, &base_url, &session_id, root_uris);

    let sse_result = sse_task.await.expect("SSE task panicked");
    assert!(
        sse_result.is_ok(),
        "Roots exchange failed: {:?}",
        sse_result
    );

    wait_for_tool_ready(&client, &base_url, &session_id, valid_root.path())
        .await
        .expect("Sandbox should become ready with mixed valid/invalid URIs");

    // Tool call should work because we had one valid root
    let tool_call = pwd_tool_call(2, Some(valid_root.path()));

    let (response, _) = send_mcp_request(&client, &base_url, &tool_call, Some(&session_id))
        .await
        .expect("Tool call request failed");

    assert!(
        response.get("error").is_none(),
        "Tool call should succeed with at least one valid root: {:?}",
        response
    );
}

// =============================================================================
// Test: Post-Lock Roots Rejection (R8.4.6)
// =============================================================================

/// SECURITY TEST: a `roots/list_changed` after sandbox lock must NOT widen the
/// sandbox — but it must NOT tear down the session either.
///
/// Per the instance-ownership model (SPEC R5.1 / R5.1.1 / R5.2.2) the committed
/// scope is immutable and can never be widened by any means. A client
/// `roots/list_changed` after lock is therefore a tolerated no-op: the session
/// stays alive (no HTTP 403, no termination — which previously caused
/// stdio-proxy respawn churn) and, crucially, the sandbox is NOT expanded to any
/// newly-announced root. This test pins both halves of that invariant.
/// True when the platform's OS-level sandbox can actually be enforced.
///
/// In a nested sandbox (Docker, Cursor, an outer `sandbox-exec`, CI running
/// inside a sandbox) `sandbox_apply` is denied, so the spawned bridge runs
/// without kernel enforcement and a "widened" out-of-scope path is reachable
/// regardless of the lock. The widen assertion below only means something when
/// the kernel sandbox engages, so skip otherwise rather than report a false
/// failure — it still runs and must pass on real CI hosts.
fn os_sandbox_enforced() -> bool {
    ahma_mcp::sandbox::check_sandbox_prerequisites().is_ok()
        && ahma_mcp::sandbox::test_sandbox_exec_available().is_ok()
}

#[tokio::test]
async fn test_post_lock_roots_change_does_not_widen_sandbox() {
    if !os_sandbox_enforced() {
        eprintln!(
            "Skipping test: OS-level sandbox cannot be enforced in this environment \
             (nested sandbox); kernel enforcement is required for the widen assertion."
        );
        return;
    }
    let initial_root = TempDir::new().expect("Failed to create initial root");
    let new_root = TempDir::new().expect("Failed to create new root"); // attacker's target
    let tools_temp = TempDir::new().expect("Failed to create tools temp dir");
    let tools_dir = tools_temp.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let (_server, base_url, client, session_id) = start_initialized_session(&tools_dir).await;

    // Answer roots/list with initial root (locks sandbox)
    let initial_uri = encode_file_uri(initial_root.path());
    let sse_task =
        spawn_complete_roots_handshake(&client, &base_url, &session_id, vec![initial_uri]);

    let sse_result = sse_task.await.expect("SSE task panicked");
    assert!(sse_result.is_ok(), "Initial roots exchange failed");

    // Give the stdio I/O time to process - Windows CI can be slow with inter-process communication
    if cfg!(windows) {
        sleep(TestTimeouts::short_delay()).await;
    }

    wait_for_tool_ready(&client, &base_url, &session_id, initial_root.path())
        .await
        .expect("Sandbox should become ready before roots/list_changed test");

    // Verify initial root works
    let tool_call = pwd_tool_call(2, Some(initial_root.path()));

    let (response, _) = send_mcp_request(&client, &base_url, &tool_call, Some(&session_id))
        .await
        .expect("Initial tool call failed");
    assert!(
        response.get("error").is_none(),
        "Tool call in initial root should succeed: {:?}",
        response
    );

    // NOW: Send roots/list_changed notification (attempt to change/widen roots).
    let roots_changed = json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    });

    let result = send_mcp_request(&client, &base_url, &roots_changed, Some(&session_id)).await;

    // It must be TOLERATED (no 403 / no termination): the request succeeds.
    assert!(
        result.is_ok(),
        "roots/list_changed after lock must be a tolerated no-op (no 403/termination), got: {:?}",
        result
    );

    // The session must stay alive: the originally-locked root still works.
    sleep(TestTimeouts::poll_interval()).await;
    let (still_ok, _) = send_mcp_request(
        &client,
        &base_url,
        &pwd_tool_call(3, Some(initial_root.path())),
        Some(&session_id),
    )
    .await
    .expect("Session must survive a benign roots change");
    assert!(
        still_ok.get("error").is_none(),
        "Original locked root must still work after roots change: {:?}",
        still_ok
    );

    // SECURITY: the sandbox must NOT have widened — the new root is still blocked.
    let (forbidden, _) = send_mcp_request(
        &client,
        &base_url,
        &pwd_tool_call(4, Some(new_root.path())),
        Some(&session_id),
    )
    .await
    .expect("Forbidden tool call request should complete");
    assert!(
        forbidden.get("error").is_some(),
        "SECURITY VIOLATION: sandbox widened to a new root after roots/list_changed. Response: {:?}",
        forbidden
    );
}

/// Test that working_directory outside locked sandbox roots is rejected.
#[tokio::test]
async fn test_working_directory_outside_sandbox_rejected() {
    if !os_sandbox_enforced() {
        eprintln!(
            "Skipping test: OS-level sandbox cannot be enforced in this environment \
             (nested sandbox); kernel enforcement is required to reject an out-of-scope cwd."
        );
        return;
    }
    let allowed_root = TempDir::new().expect("Failed to create allowed root");
    let forbidden_root = TempDir::new().expect("Failed to create forbidden root");
    let tools_temp = TempDir::new().expect("Failed to create tools temp dir");
    let tools_dir = tools_temp.path().join("tools");
    write_pwd_tool_config(&tools_dir);

    let (_server, base_url, client, session_id) = start_initialized_session(&tools_dir).await;

    // Lock sandbox to ONLY allowed_root
    let allowed_uri = encode_file_uri(allowed_root.path());
    let sse_task =
        spawn_complete_roots_handshake(&client, &base_url, &session_id, vec![allowed_uri]);

    let sse_result = sse_task.await.expect("SSE task panicked");
    assert!(sse_result.is_ok(), "Roots exchange failed");

    wait_for_tool_ready(&client, &base_url, &session_id, allowed_root.path())
        .await
        .expect("Sandbox should become ready for allowed root");

    // Tool call in allowed root should work
    let allowed_call = pwd_tool_call(2, Some(allowed_root.path()));

    let (allowed_response, _) =
        send_mcp_request(&client, &base_url, &allowed_call, Some(&session_id))
            .await
            .expect("Allowed tool call request failed");
    assert!(
        allowed_response.get("error").is_none(),
        "Tool call in allowed root should succeed: {:?}",
        allowed_response
    );

    // Tool call in FORBIDDEN root should fail
    let forbidden_call = pwd_tool_call(3, Some(forbidden_root.path()));

    let (forbidden_response, _) =
        send_mcp_request(&client, &base_url, &forbidden_call, Some(&session_id))
            .await
            .expect("Forbidden tool call request failed");

    // This MUST fail with a sandbox/path error
    let error = forbidden_response.get("error");
    assert!(
        error.is_some(),
        "Tool call in forbidden directory should fail: {:?}",
        forbidden_response
    );

    let error_msg = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");

    assert!(
        error_msg.contains("sandbox")
            || error_msg.contains("outside")
            || error_msg.contains("path"),
        "Error should mention sandbox violation: {}",
        error_msg
    );
}
