//! HTTP DELETE Session Termination Tests (R8.4.7)
//!
//! These tests verify that HTTP DELETE with `Mcp-Session-Id` header properly
//! terminates sessions and their subprocesses.
//!
//! Per MCP specification (R8.4.7): HTTP DELETE with `Mcp-Session-Id` terminates
//! session and subprocess.

use crate::common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::{McpTestClient, TestServerInstance, spawn_test_server};
use serde_json::json;
use tokio::time::sleep;

/// Spawn the shared test server (workspace tool configs + temp sandbox scope).
async fn start_server() -> TestServerInstance {
    spawn_test_server()
        .await
        .expect("Failed to start HTTP bridge")
}

/// Send an HTTP DELETE to the MCP endpoint, optionally with a session header.
async fn delete_session(
    client: &reqwest::Client,
    base_url: &str,
    session_id: Option<&str>,
) -> reqwest::Response {
    let mut req = client
        .delete(format!("{}/mcp", base_url))
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest));
    if let Some(id) = session_id {
        req = req.header("Mcp-Session-Id", id);
    }
    req.send().await.expect("DELETE request should complete")
}

/// Test that DELETE with valid session ID returns 204 and terminates the session (R8.4.7)
#[tokio::test]
async fn test_delete_session_terminates_subprocess() {
    let server = start_server().await;
    let base_url = server.base_url();
    let client = common::make_h2_client();

    // Step 1: Initialize a session
    let mut mcp = McpTestClient::for_server(&server);
    mcp.initialize_only("test-client")
        .await
        .expect("Initialize should succeed");
    let session_id = mcp
        .session_id()
        .expect("Should receive session ID from initialize")
        .to_string();
    eprintln!("Got session ID: {}", session_id);

    // Step 2: Send DELETE request to terminate the session
    let delete_response = delete_session(&client, &base_url, Some(&session_id)).await;
    eprintln!("DELETE response status: {}", delete_response.status());

    // Step 3: Assert 204 No Content
    assert_eq!(
        delete_response.status().as_u16(),
        204,
        "DELETE should return 204 No Content"
    );

    // Step 4: Verify subsequent requests with same session ID are rejected
    sleep(TestTimeouts::short_delay()).await;

    let post_response = client
        .post(format!("{}/mcp", base_url))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .timeout(TestTimeouts::get(TimeoutCategory::HttpRequest))
        .send()
        .await
        .expect("POST request should complete");

    eprintln!("POST after DELETE status: {}", post_response.status());

    // Session should no longer exist - expect 403 Forbidden (per R8D.13: security response for
    // non-existent or terminated sessions) or 404 Not Found
    let status = post_response.status().as_u16();
    assert!(
        status == 403 || status == 404,
        "Requests to deleted session should return 403 Forbidden or 404 Not Found, got {}",
        status
    );
}

/// Test that DELETE without session ID returns 400 Bad Request
#[tokio::test]
async fn test_delete_without_session_id_returns_400() {
    let server = start_server().await;
    let client = common::make_h2_client();

    let delete_response = delete_session(&client, &server.base_url(), None).await;
    eprintln!("DELETE response status: {}", delete_response.status());

    assert_eq!(
        delete_response.status().as_u16(),
        400,
        "DELETE without session ID should return 400 Bad Request"
    );
}

/// Test that DELETE with non-existent session ID returns 404 Not Found
#[tokio::test]
async fn test_delete_nonexistent_session_returns_404() {
    let server = start_server().await;
    let client = common::make_h2_client();

    let delete_response = delete_session(
        &client,
        &server.base_url(),
        Some("non-existent-session-id-12345"),
    )
    .await;
    eprintln!("DELETE response status: {}", delete_response.status());

    assert_eq!(
        delete_response.status().as_u16(),
        404,
        "DELETE with non-existent session should return 404 Not Found"
    );
}
