//! Integration tests to improve request_handler.rs code coverage.
//!
//! These tests exercise various code paths in the HTTP bridge request handler,
//! including JSON vs SSE transport, session validation, client responses,
//! and error handling.

mod common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::{
    McpTestClient, TransportMode, setup_test_mcp_for_tools,
    spawn_server_guard_with_config_extra_env, spawn_test_server,
};
use futures::StreamExt;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;
use tokio::time::sleep;

const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Test: POST without session ID for non-initialize method returns 400.
/// Covers handle_session_isolated_request path for "Request without session ID".
#[tokio::test]
async fn test_missing_session_id_json() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });

    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&req)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status().as_u16(), 400, "Should return 400 Bad Request");
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    let err = body.get("error").expect("Should have error");
    assert_eq!(err.get("code").and_then(|c| c.as_i64()), Some(-32600));
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        msg.contains("Mcp-Session-Id") || msg.contains("session"),
        "Error should mention session ID: {}",
        msg
    );
}

/// Test: POST with nonexistent session ID returns 403.
/// Covers check_session_exists.
#[tokio::test]
async fn test_nonexistent_session_json() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });

    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(
            MCP_SESSION_ID_HEADER,
            "00000000-0000-0000-0000-000000000000",
        )
        .json(&req)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status().as_u16(), 403, "Should return 403 Forbidden");
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    let err = body.get("error").expect("Should have error");
    assert_eq!(err.get("code").and_then(|c| c.as_i64()), Some(-32600));
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        msg.contains("not found") || msg.contains("terminated"),
        "Error should mention session: {}",
        msg
    );
}

/// Test: Initialize with Accept: text/event-stream returns SSE response.
/// Covers handle_initialize_sse.
#[tokio::test]
async fn test_sse_initialize_returns_sse_stream() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "sse-init-test", "version": "1.0"}
        }
    });

    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .json(&req)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Request failed");

    assert!(resp.status().is_success(), "Initialize should succeed");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        ct.contains("text/event-stream"),
        "Should return SSE content type: {}",
        ct
    );
    assert!(
        resp.headers().contains_key(MCP_SESSION_ID_HEADER)
            || resp.headers().contains_key("mcp-session-id"),
        "Should include session ID header"
    );
}

/// Test: POST (non-initialize) with Accept: text/event-stream but no session ID returns 400.
/// Covers handle_session_isolated_request_sse missing-session path.
#[tokio::test]
async fn test_sse_missing_session_id() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });

    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .json(&req)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    let err = body.get("error").expect("Should have error");
    assert_eq!(err.get("code").and_then(|c| c.as_i64()), Some(-32600));
}

/// Test: POST with Accept: text/event-stream and nonexistent session ID returns 403.
#[tokio::test]
async fn test_sse_nonexistent_session() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });

    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header(
            MCP_SESSION_ID_HEADER,
            "00000000-0000-0000-0000-000000000000",
        )
        .json(&req)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Request failed");

    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    let err = body.get("error").expect("Should have error");
    assert_eq!(err.get("code").and_then(|c| c.as_i64()), Some(-32600));
}

/// Test: Client response with invalid roots array is accepted (202) without crashing.
/// Covers try_lock_sandbox_from_roots with missing/invalid roots.
#[tokio::test]
async fn test_client_response_invalid_roots() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "invalid-roots-test", "version": "1.0"}
        }
    });

    let init_resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&init_req)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Initialize failed");

    let session_id = init_resp
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .or_else(|| init_resp.headers().get("mcp-session-id"))
        .and_then(|h| h.to_str().ok())
        .expect("Session ID")
        .to_string();

    // Open SSE stream
    let sse_resp = client
        .get(format!("{}/mcp", server.base_url()))
        .header("Accept", "text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .send()
        .await
        .expect("SSE GET failed");
    assert!(sse_resp.status().is_success());

    // Send initialized
    let _ = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Initialized failed");

    // Consume SSE until we see roots/list (or timeout)
    let mut stream = sse_resp.bytes_stream();
    let mut buffer = String::new();
    let deadline = Instant::now() + TestTimeouts::get(TimeoutCategory::Handshake);
    let mut roots_req_id: Option<serde_json::Value> = None;

    while Instant::now() < deadline {
        if let Some(chunk) = tokio::time::timeout(TestTimeouts::poll_interval(), stream.next())
            .await
            .ok()
            .flatten()
        {
            let bytes = chunk.unwrap_or_default();
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(idx) = buffer.find("\n\n") {
                let raw = buffer[..idx].to_string();
                buffer = buffer[idx + 2..].to_string();
                if let Some(data) = raw
                    .lines()
                    .find_map(|l| l.strip_prefix("data:"))
                    .map(str::trim)
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(data)
                    && v.get("method").and_then(|m| m.as_str()) == Some("roots/list")
                {
                    roots_req_id = v.get("id").cloned();
                    break;
                }
            }
            if roots_req_id.is_some() {
                break;
            }
        }
        sleep(TestTimeouts::poll_interval()).await;
    }

    let req_id = roots_req_id.unwrap_or(json!(999));

    // Send roots/list response with INVALID roots (no "roots" array)
    let invalid_roots_response = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {}
    });

    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .json(&invalid_roots_response)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Roots response failed");

    // Bridge accepts 202 and forwards; try_lock_sandbox_from_roots no-ops on invalid roots
    assert_eq!(resp.status().as_u16(), 202, "Should accept client response");
}

/// Run tools/list for both JSON and SSE transport (dual-transport coverage per AGENTS.md).
async fn run_tools_list(mode: TransportMode) {
    let Some((_server, mcp)) = common::setup_test_mcp(mode).await else {
        return;
    };
    let result = mcp.list_tools().await;
    assert!(
        result.is_ok(),
        "tools/list should succeed: {:?}",
        result.err()
    );
    let tools = result.unwrap();
    assert!(!tools.is_empty(), "Should have at least one tool");
}

#[tokio::test]
async fn test_tools_list_json() {
    run_tools_list(TransportMode::Json).await;
}

#[tokio::test]
async fn test_tools_list_sse() {
    run_tools_list(TransportMode::Sse).await;
}

/// Run tools/call with timeout_seconds in arguments to cover calculate_tool_timeout.
async fn run_tools_call_timeout(mode: TransportMode) {
    let Some((_server, mcp)) = setup_test_mcp_for_tools(mode, &["run_terminal_command"]).await
    else {
        return;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let result = mcp
        .call_tool(
            "run_terminal_command",
            json!({
                "command": "echo ok",
                "working_directory": cwd.to_string_lossy(),
                "timeout_seconds": 30
            }),
        )
        .await;

    assert!(
        result.success,
        "Tool call should succeed: {:?}",
        result.error
    );
}

#[tokio::test]
async fn test_tools_call_with_timeout_seconds_json() {
    run_tools_call_timeout(TransportMode::Json).await;
}

#[tokio::test]
async fn test_tools_call_with_timeout_seconds_sse() {
    run_tools_call_timeout(TransportMode::Sse).await;
}

async fn run_task_vault_inheritance_and_staged_delete(mode: TransportMode) {
    let vault = TempDir::new().expect("failed to create vault temp dir");
    let vault_root = vault.path().to_path_buf();
    let workdir = vault_root.join("workdir");
    let trash = vault_root.join("trash");
    let audit_log = vault_root.join("audit.jsonl");

    fs::create_dir_all(&workdir).expect("failed to create vault workdir");
    fs::create_dir_all(&trash).expect("failed to create vault trash");
    fs::write(&audit_log, "").expect("failed to init vault audit log");

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let tools_dir = workspace_root.join(".ahma");
    if !tools_dir.exists() {
        eprintln!(
            "WARNING  skipping task-vault inheritance test: tools dir missing at {}",
            tools_dir.display()
        );
        return;
    }

    let extra_env = vec![(
        "AHMA_TASK_VAULT".to_string(),
        vault_root.to_string_lossy().to_string(),
    )];

    let handshake_timeout = TestTimeouts::get(TimeoutCategory::Handshake);
    let mut last_error = String::new();
    let mut server_and_client = None;

    for attempt in 1..=2u32 {
        let server = match spawn_server_guard_with_config_extra_env(
            &tools_dir,
            &workdir,
            Some(
                TestTimeouts::get(TimeoutCategory::Handshake)
                    .as_secs()
                    .max(1),
            ),
            &extra_env,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                last_error = format!("failed to spawn server: {}", e);
                continue;
            }
        };

        let mut mcp = McpTestClient::with_url(&server.base_url()).with_transport(mode);
        match tokio::time::timeout(
            handshake_timeout,
            mcp.initialize_with_roots("task-vault-coverage", std::slice::from_ref(&workdir)),
        )
        .await
        {
            Ok(Ok(_)) => {
                server_and_client = Some((server, mcp));
                break;
            }
            Ok(Err(e)) => last_error = e.to_string(),
            Err(_) => last_error = format!("handshake timed out after {:?}", handshake_timeout),
        }

        if attempt == 1 {
            eprintln!(
                "WARNING  task-vault inheritance test handshake attempt {} failed: {}. Retrying...",
                attempt, last_error
            );
        }
    }

    let Some((_server, mcp)) = server_and_client else {
        let in_ci =
            std::env::var("CI").is_ok() || std::env::var("AHMA_TEST_FAIL_ON_SETUP_ERROR").is_ok();
        if in_ci {
            panic!(
                "task-vault inheritance test: setup failed in CI — aborting: {}",
                last_error
            );
        }
        eprintln!(
            "WARNING  skipping task-vault inheritance test: setup failed: {}",
            last_error
        );
        return;
    };

    let echo_result = mcp
        .call_tool(
            "run_terminal_command",
            json!({
                "command": "echo vault-audit",
                "working_directory": workdir.to_string_lossy().to_string(),
            }),
        )
        .await;
    assert!(
        echo_result.success,
        "echo command should succeed: {:?}",
        echo_result.error
    );

    let delete_target = workdir.join("to-delete.txt");
    fs::write(&delete_target, "delete me").expect("failed to create delete target");

    let rm_result = mcp
        .call_tool(
            "run_terminal_command",
            json!({
                "command": "rm to-delete.txt",
                "working_directory": workdir.to_string_lossy().to_string(),
            }),
        )
        .await;
    assert!(
        rm_result.success,
        "rm command should succeed: {:?}",
        rm_result.error
    );

    assert!(
        !delete_target.exists(),
        "original file should no longer exist in workdir"
    );
    let staged_entries: Vec<_> = fs::read_dir(&trash)
        .expect("failed to list trash entries")
        .filter_map(|entry| entry.ok())
        .collect();
    assert!(
        !staged_entries.is_empty(),
        "trash should contain at least one staged entry"
    );

    sleep(TestTimeouts::short_delay()).await;
    let audit = fs::read_to_string(&audit_log).expect("failed to read audit log");
    assert!(
        audit.contains("\"type\":\"tool_call\""),
        "audit log should contain tool_call events"
    );
    assert!(
        audit.contains("\"type\":\"tool_complete\""),
        "audit log should contain tool_complete events"
    );
    assert!(
        audit.contains("\"type\":\"file_staged\""),
        "audit log should contain file_staged events"
    );
}

#[tokio::test]
async fn test_task_vault_inheritance_and_staged_delete_json() {
    run_task_vault_inheritance_and_staged_delete(TransportMode::Json).await;
}

#[tokio::test]
async fn test_task_vault_inheritance_and_staged_delete_sse() {
    run_task_vault_inheritance_and_staged_delete(TransportMode::Sse).await;
}

/// Test: notifications/initialized with Accept: text/event-stream.
/// Covers forward_notification_sse.
#[tokio::test]
async fn test_sse_notification_initialized() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "sse-notif-test", "version": "1.0"}
        }
    });

    let init_resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&init_req)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Initialize failed");

    let session_id = init_resp
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .or_else(|| init_resp.headers().get("mcp-session-id"))
        .and_then(|h| h.to_str().ok())
        .expect("Session ID")
        .to_string();

    // Open SSE stream first
    let _sse = client
        .get(format!("{}/mcp", server.base_url()))
        .header("Accept", "text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .send()
        .await
        .expect("SSE failed");

    // Send notifications/initialized with Accept: text/event-stream
    let notif = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header(MCP_SESSION_ID_HEADER, &session_id)
        .json(&notif)
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("Notification failed");

    assert!(resp.status().is_success(), "Should accept notification");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        ct.contains("text/event-stream"),
        "Should return SSE: {}",
        ct
    );
}
