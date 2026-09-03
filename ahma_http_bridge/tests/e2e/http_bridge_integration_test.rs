//! HTTP Bridge Integration Tests
//!
//! These tests verify end-to-end HTTP bridge functionality by:
//! 1. Starting the HTTP bridge with a real ahma_mcp subprocess
//! 2. Sending requests through the HTTP interface
//! 3. Verifying correct responses
//!
//! These tests reproduce the bug where calling a tool from a different project
//! (different working_directory) fails with "expect initialized request" error.
//!
//! NOTE: These tests spawn their own servers with specific sandbox configurations.
//! They use dynamic port allocation to avoid conflicts with other tests.
//! The shared test server singleton (port 5721) is NOT used here.

use crate::common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::server::{ServerGuard, spawn_server_guard_with_config};
use common::uri::paths_equivalent;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::time::sleep;

/// Start the HTTP bridge server and return a ServerGuard
async fn start_http_bridge(
    tools_dir: &std::path::Path,
    sandbox_scope: &std::path::Path,
) -> ServerGuard {
    spawn_server_guard_with_config(tools_dir, sandbox_scope, Some(300))
        .await
        .expect("Failed to start HTTP bridge")
}

/// Strict-roots bridge (no fallback scope) for tests that assert on the scope
/// derived from the client's roots/list answer — an explicit fallback scope is
/// locked without querying roots at all (SPEC R5.2.2).
async fn start_http_bridge_strict_roots(tools_dir: &std::path::Path) -> ServerGuard {
    common::spawn_server_guard_strict_roots(tools_dir)
        .await
        .expect("Failed to start strict-roots HTTP bridge")
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

    // Debug: print all headers
    eprintln!(
        "Response headers for request {}:",
        request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
    );
    for (name, value) in response.headers().iter() {
        if name.as_str().eq_ignore_ascii_case("mcp-session-id") {
            eprintln!("  {}: <redacted>", name);
        } else {
            eprintln!("  {}: {:?}", name, value);
        }
    }

    // Get session ID from response header (case-insensitive)
    let new_session_id = response
        .headers()
        .get("mcp-session-id")
        .or_else(|| response.headers().get("Mcp-Session-Id"))
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, text));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    Ok((body, new_session_id))
}

fn is_sandbox_initializing_error(response: &Value) -> bool {
    let error = response.get("error");
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_i64())
        .unwrap_or_default();
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");

    code == -32001 || message.contains("Sandbox initializing")
}

fn is_transient_transport_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("http 409")
        || lower.contains("http 500")
        || lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
}

fn capped_backoff(base_ms: u64, attempt: usize, max_ms: u64) -> Duration {
    Duration::from_millis((base_ms.saturating_mul(attempt as u64)).min(max_ms))
}

fn roots_handshake_timeout() -> Duration {
    TestTimeouts::get(TimeoutCategory::Handshake)
}

fn post_roots_configured_grace_timeout() -> Duration {
    TestTimeouts::scale_secs(5)
}

async fn open_roots_sse_stream(
    client: &Client,
    base_url: &str,
    session_id: &str,
) -> reqwest::Response {
    let url = format!("{}/mcp", base_url);
    let response = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Mcp-Session-Id", session_id)
        .timeout(TestTimeouts::get(TimeoutCategory::SseStream))
        .send()
        .await
        .expect("Failed to open SSE stream");

    // Avoid a race where initialized is processed before SSE subscription
    // registration is fully active in the bridge.
    sleep(TestTimeouts::short_delay()).await;

    response
}

fn first_sse_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
    match (lf, crlf) {
        (Some((lf_idx, lf_len)), Some((crlf_idx, crlf_len))) => {
            if lf_idx <= crlf_idx {
                Some((lf_idx, lf_len))
            } else {
                Some((crlf_idx, crlf_len))
            }
        }
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

/// Send tools/call with retries for handshake races and transient transport failures.
async fn send_tool_call_with_retry(
    client: &Client,
    base_url: &str,
    session_id: &str,
    tool_call: &Value,
) -> Value {
    let timeout = ahma_common::timeouts::TestTimeouts::get(
        ahma_common::timeouts::TimeoutCategory::SandboxReady,
    );
    let deadline = Instant::now() + timeout;
    let mut attempt = 0usize;

    loop {
        attempt += 1;
        match send_mcp_request(client, base_url, tool_call, Some(session_id)).await {
            Ok((response, _)) => {
                if is_sandbox_initializing_error(&response) {
                    if Instant::now() >= deadline {
                        panic!(
                            "Timed out waiting for sandbox initialization after {} attempts. Last response: {:?}",
                            attempt, response
                        );
                    }
                    sleep(capped_backoff(100, attempt, 1_000)).await;
                    continue;
                }
                return response;
            }
            Err(e) if is_transient_transport_error(&e) => {
                if Instant::now() >= deadline {
                    panic!(
                        "Timed out retrying tools/call after {} attempts. Last error: {}",
                        attempt, e
                    );
                }
                sleep(capped_backoff(200, attempt, 2_000)).await;
            }
            Err(e) => {
                panic!(
                    "Unexpected transport error during tools/call (attempt {}): {}",
                    attempt, e
                );
            }
        }
    }
}

fn parse_sse_event_data(raw_event: &str) -> Option<Value> {
    let data_lines: Vec<&str> = raw_event
        .lines()
        .filter_map(|line| {
            line.trim_end_matches('\r')
                .strip_prefix("data:")
                .map(str::trim)
        })
        .collect();
    if data_lines.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&data_lines.join("\n")).ok()
}

fn drain_sse_events(buffer: &mut String) -> Vec<Value> {
    let mut events = Vec::new();
    while let Some((idx, delimiter_len)) = first_sse_event_boundary(buffer) {
        let raw_event = buffer[..idx].to_string();
        buffer.drain(..idx + delimiter_len);
        if let Some(value) = parse_sse_event_data(&raw_event) {
            events.push(value);
        }
    }
    events
}

async fn reply_roots_list(
    client: &Client,
    base_url: &str,
    session_id: &str,
    request_id: Value,
    roots_json: &[Value],
) {
    let response = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": { "roots": roots_json }
    });
    let _ = send_mcp_request(client, base_url, &response, Some(session_id))
        .await
        .expect("Failed to send roots/list response");
}

/// Process an already-open SSE response to complete the MCP roots handshake.
/// Reads `roots/list`, responds with `roots_json`, and prefers to observe
/// `notifications/sandbox/configured` on the same GET SSE stream.
///
/// Under `cargo llvm-cov nextest`, the bridge can successfully lock the sandbox
/// after the `roots/list` response while the test client never observes the
/// follow-up notification on that specific SSE stream before its timeout.
/// Returning after a bounded grace period keeps the test focused on the real
/// invariant: roots were provided, and later `tools/call` requests must succeed
/// once sandbox activation completes.
///
/// Callers must open the SSE connection *before* sending
/// `notifications/initialized` to avoid the race where the server fires
/// `roots/list` before the client has a listener.
async fn process_sse_roots_handshake(
    sse_resp: reqwest::Response,
    client: &Client,
    base_url: &str,
    session_id: &str,
    roots_json: Vec<Value>,
) {
    assert!(
        sse_resp.status().is_success(),
        "SSE stream must be available, got HTTP {}",
        sse_resp.status()
    );

    let mut stream = sse_resp.bytes_stream();
    let mut buffer = String::new();
    let mut roots_answered = false;
    let mut configured_seen = false;
    let mut post_roots_deadline: Option<tokio::time::Instant> = None;

    let roots_deadline = tokio::time::Instant::now() + roots_handshake_timeout();
    loop {
        if let Some(timeout_at) = post_roots_deadline
            && tokio::time::Instant::now() > timeout_at
        {
            eprintln!(
                "WARNING: did not observe notifications/sandbox/configured after roots/list response; continuing and relying on tools/call retry to verify sandbox activation"
            );
            return;
        }

        if !roots_answered && tokio::time::Instant::now() > roots_deadline {
            panic!(
                "Timed out waiting for roots/list + sandbox/configured over SSE (session isolation likely broken)"
            );
        }

        let chunk = tokio::time::timeout(TestTimeouts::poll_interval(), stream.next())
            .await
            .ok()
            .flatten();

        let Some(next) = chunk else {
            continue;
        };
        let bytes = next.expect("SSE stream read failed");
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        for value in drain_sse_events(&mut buffer) {
            let method = value.get("method").and_then(|m| m.as_str());

            if method == Some("notifications/sandbox/failed") {
                let error = value
                    .get("params")
                    .and_then(|p| p.get("error"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("unknown");
                panic!("Sandbox configuration failed: {}", error);
            }

            if method == Some("notifications/sandbox/configured") {
                configured_seen = true;
                if roots_answered {
                    return;
                }
                continue;
            }

            if method != Some("roots/list") {
                continue;
            }

            let id = value
                .get("id")
                .cloned()
                .expect("roots/list must include id");
            reply_roots_list(client, base_url, session_id, id, &roots_json).await;
            roots_answered = true;
            post_roots_deadline =
                Some(tokio::time::Instant::now() + post_roots_configured_grace_timeout());
            if configured_seen {
                return;
            }
        }
    }
}

fn canonical_test_path(path: &Path) -> std::path::PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn encode_test_file_uri(path: &Path) -> String {
    common::encode_file_uri(&canonical_test_path(path))
}

fn encode_test_file_uri_with_localhost(path: &Path) -> String {
    let uri = encode_test_file_uri(path);
    let suffix = uri
        .strip_prefix("file://")
        .expect("shared file URI helper must include scheme");
    format!("file://localhost{}", suffix)
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "roots": { "listChanged": true }
            },
            "clientInfo": {"name": "test-client", "version": "1.0.0"}
        }
    })
}

async fn initialize_session(client: &Client, base_url: &str) -> String {
    let (_, session_id) = send_mcp_request(client, base_url, &initialize_request(), None)
        .await
        .expect("Initialize should succeed");
    session_id.expect("Session isolation must return mcp-session-id header")
}

fn initialized_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    })
}

fn pwd_tool_call(request_id: u64, working_directory: &Path) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": {
            "name": "pwd",
            "arguments": {
                "subcommand": "default",
                "working_directory": working_directory.to_string_lossy()
            }
        }
    })
}

/// REGRESSION TEST (DO NOT WEAKEN): Cross-repo working_directory must succeed.
///
/// Real-world failure this guards against:
/// - Start the HTTP server from repo A (e.g. `ahma_mcp` checkout).
/// - Connect from VS Code opened on repo B.
/// - VS Code sends `tools/call` with `working_directory` in repo B.
/// - If the server is incorrectly scoped to repo A, it fails with:
///   "Path '...' is outside the sandbox root '...'".
///
/// The correct behavior is **per-session sandbox isolation**:
/// the sandbox scope must be derived from the client's `roots/list` response,
/// so repo B is allowed for that session even if the server was started elsewhere.
///
/// WARNING TO FUTURE AI/MAINTAINERS:
/// - Do NOT change this test to accept either success OR sandbox failure.
/// - Do NOT add test-mode env var bypasses (see SPEC.md R-CFG9.2).
/// - Fix scoping/session isolation if this fails.
#[tokio::test]
#[serial]
async fn test_roots_uri_parsing_percent_encoded_path() {
    let server_scope_dir = TempDir::new().expect("Failed to create temp dir (server_scope)");
    let client_scope_dir = TempDir::new().expect("Failed to create temp dir (client_scope)");

    let tools_dir = server_scope_dir.path().join("tools");
    std::fs::create_dir_all(&tools_dir).expect("Failed to create tools dir");

    common::create_pwd_tool_config(&tools_dir);

    // Make a workspace root with space + unicode in the path.
    let client_root = client_scope_dir.path().join("my proj OK");
    tokio::fs::create_dir_all(&client_root)
        .await
        .expect("Failed to create client root");

    let server = start_http_bridge_strict_roots(&tools_dir).await;
    let base_url = server.base_url();
    let client = common::make_h2_client();

    let session_id = initialize_session(&client, &base_url).await;

    let uri = encode_test_file_uri(&client_root);

    // Open SSE stream BEFORE sending notifications/initialized so the server's
    // roots/list request is not lost if it fires immediately on initialized.
    let sse_resp = client
        .get(format!("{}/mcp", base_url))
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Mcp-Session-Id", &session_id)
        .send()
        .await
        .expect("Failed to open SSE stream");

    let roots_json = vec![json!({"uri": uri, "name": "root"})];
    let sse_client = client.clone();
    let sse_base_url = base_url.clone();
    let sse_session_id = session_id.clone();
    let sse_task = tokio::spawn(async move {
        process_sse_roots_handshake(
            sse_resp,
            &sse_client,
            &sse_base_url,
            &sse_session_id,
            roots_json,
        )
        .await;
    });

    let initialized = initialized_notification();
    let _ = send_mcp_request(&client, &base_url, &initialized, Some(&session_id)).await;
    sse_task.await.expect("roots/list SSE task panicked");

    let tool_call = pwd_tool_call(2, &client_root);

    let resp = send_tool_call_with_retry(&client, &base_url, &session_id, &tool_call).await;

    assert!(
        resp.get("error").is_none(),
        "pwd must succeed, got: {resp:?}"
    );

    let output_text = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    assert!(
        paths_equivalent(output_text, &client_root),
        "pwd output must include decoded client root path; got: {resp:?}"
    );
}

/// Some clients send file URIs in host form: file://localhost/abs/path
#[tokio::test]
#[serial]
async fn test_roots_uri_parsing_file_localhost() {
    let server_scope_dir = TempDir::new().expect("Failed to create temp dir (server_scope)");
    let client_scope_dir = TempDir::new().expect("Failed to create temp dir (client_scope)");

    let tools_dir = server_scope_dir.path().join("tools");
    std::fs::create_dir_all(&tools_dir).expect("Failed to create tools dir");

    common::create_pwd_tool_config(&tools_dir);

    let client_root = client_scope_dir.path().join("my proj OK");
    tokio::fs::create_dir_all(&client_root)
        .await
        .expect("Failed to create client root");

    let server = start_http_bridge_strict_roots(&tools_dir).await;
    let base_url = server.base_url();
    let client = common::make_h2_client();

    let session_id = initialize_session(&client, &base_url).await;

    let uri = encode_test_file_uri_with_localhost(&client_root);

    let sse_resp = open_roots_sse_stream(&client, &base_url, &session_id).await;
    let sse_client = client.clone();
    let sse_base_url = base_url.clone();
    let sse_session_id = session_id.clone();
    let roots_json = vec![json!({"uri": uri.clone(), "name": "root"})];
    let sse_task = tokio::spawn(async move {
        process_sse_roots_handshake(
            sse_resp,
            &sse_client,
            &sse_base_url,
            &sse_session_id,
            roots_json,
        )
        .await;
    });

    let initialized = initialized_notification();
    let _ = send_mcp_request(&client, &base_url, &initialized, Some(&session_id)).await;
    sse_task.await.expect("roots/list SSE task panicked");

    let tool_call = pwd_tool_call(2, &client_root);

    let resp = send_tool_call_with_retry(&client, &base_url, &session_id, &tool_call).await;

    assert!(
        resp.get("error").is_none(),
        "pwd must succeed, got: {resp:?}"
    );
}

/// The expected behavior: The HTTP bridge should reject requests that come
/// before initialize, OR handle initialization automatically.
#[tokio::test]
#[serial]
async fn test_tool_call_without_initialize_returns_proper_error() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let tools_dir = temp_dir.path().join("tools");
    std::fs::create_dir_all(&tools_dir).expect("Failed to create tools dir");

    common::create_pwd_tool_config(&tools_dir);

    let sandbox_scope = temp_dir.path().to_path_buf();
    let server = start_http_bridge(&tools_dir, &sandbox_scope).await;
    let base_url = server.base_url();
    let client = common::make_h2_client();

    // SKIP initialize - send tools/call directly
    // This reproduces the user's bug where the subprocess gets a tools/call first
    let tool_call = pwd_tool_call(1, &sandbox_scope);

    let result = send_mcp_request(&client, &base_url, &tool_call, None).await;

    eprintln!("Tool call without initialize result: {:?}", result);

    // This SHOULD fail - but the question is HOW it fails
    // Good: HTTP 400 or JSON-RPC error saying "not initialized" or similar
    // Bad: "expect initialized request" (means subprocess crashed)

    // Check HTTP response
    let response_error_msg = match &result {
        Ok((response, _)) => response
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => e.clone(),
    };

    // Also check HTTP response doesn't contain this error
    assert!(
        !response_error_msg.contains("expect initialized request"),
        "BUG: HTTP response contains 'expect initialized request': {}",
        response_error_msg
    );
}
