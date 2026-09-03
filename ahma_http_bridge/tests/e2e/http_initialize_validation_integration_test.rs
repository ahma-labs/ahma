//! Regression test: the HTTP bridge should fail fast on malformed initialize requests.
//!
//! A missing `params.protocolVersion` previously caused the bridge to create a session,
//! forward the request to the stdio subprocess, and then hang until a timeout.
//!
//! This test ensures we return an immediate JSON-RPC error instead.

use crate::common;

use common::spawn_test_server;
use serde_json::Value;

#[tokio::test]
async fn test_initialize_missing_protocol_version_fails_fast() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let malformed_initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            // Intentionally missing: "protocolVersion"
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "test-invalid-init", "version": "1.0"}
        }
    });

    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client
            .post(format!("{}/mcp", server.base_url()))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&malformed_initialize)
            .send(),
    )
    .await
    .expect("Request should fail fast, not time out")
    .expect("HTTP request should complete");

    assert!(
        !resp.headers().contains_key("mcp-session-id")
            && !resp.headers().contains_key("Mcp-Session-Id"),
        "Malformed initialize must not create a session"
    );

    // The bridge currently returns INTERNAL_SERVER_ERROR for JSON-RPC errors.
    assert_eq!(resp.status().as_u16(), 500);

    let body: Value = resp.json().await.expect("Response should be JSON");
    let error = body.get("error").expect("JSON-RPC error object expected");

    assert_eq!(
        error.get("code").and_then(|c| c.as_i64()),
        Some(-32602),
        "Expected invalid params error code -32602. Body: {body:?}"
    );

    let msg = error
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();

    assert!(
        msg.contains("missing") && msg.contains("protocolVersion"),
        "Expected message to mention missing protocolVersion. Got: {msg}"
    );
}

#[tokio::test]
async fn test_initialize_with_stale_session_header_succeeds() {
    let server = spawn_test_server()
        .await
        .expect("Failed to spawn test server");
    let client = common::make_h2_client();

    let initialize_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-stale-session-init", "version": "1.0"}
        }
    });

    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client
            .post(format!("{}/mcp", server.base_url()))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("mcp-session-id", "stale-nonexistent-session-id-12345")
            .json(&initialize_req)
            .send(),
    )
    .await
    .expect("Request should complete within timeout")
    .expect("HTTP request should complete");

    assert_eq!(
        resp.status().as_u16(),
        200,
        "Initialize with stale session header must succeed with HTTP 200"
    );

    let session_header = resp
        .headers()
        .get("mcp-session-id")
        .or_else(|| resp.headers().get("Mcp-Session-Id"))
        .expect("Initialize response must contain mcp-session-id header");

    let new_session_id = session_header
        .to_str()
        .expect("valid session id header string");
    assert_ne!(
        new_session_id, "stale-nonexistent-session-id-12345",
        "A fresh session ID must be generated"
    );
    assert!(!new_session_id.is_empty(), "Session ID must not be empty");
}
