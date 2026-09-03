//! End-to-end HTTP/SSE roots handshake integration test.
//!
//! This test exercises the real Streamable HTTP transport on a running bridge:
//! - POST /mcp initialize (creates session)
//! - GET /mcp SSE (server→client requests)
//! - POST notifications/initialized
//! - Receive roots/list over SSE
//! - Respond with a temp workspace root
//! - Call a tool without providing working_directory and verify it runs inside the root
//!
//! ## Dual-transport exemption
//! This file is **exempt** from the `_json`/`_sse` pair requirement (AGENTS.md §Dual-Transport
//! Coverage).  The protocol under test is the `roots/list` control path, which is inherently
//! SSE-only.  JSON vs SSE tool-call transport coverage is exercised in
//! `request_handler_coverage_test.rs`.
//!
//! Running (spawns its own server with dynamic port):
//!   cargo test -p ahma_http_bridge --test http_roots_handshake_integration_test
//!
//! Or with a custom server URL:
//!   AHMA_TEST_SSE_URL=http://localhost:3000 cargo test -p ahma_http_bridge --test http_roots_handshake_integration_test

use crate::common;

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use common::uri::paths_equivalent;
use common::{McpTestClient, TestServerInstance, spawn_test_server_strict_roots};
use serde_json::json;
use std::env;
use tempfile::TempDir;

/// Spawn a test server or use environment variable URL.
/// Returns (base_url, Option<server_instance>).
/// The server instance must be kept alive for the duration of the test.
async fn get_server_url() -> (String, Option<TestServerInstance>) {
    if let Ok(url) = env::var("AHMA_TEST_SSE_URL") {
        // User specified a custom URL, verify it's available
        let client = common::make_h2_client();
        let health_url = format!("{}/health", url);
        match client
            .get(&health_url)
            .timeout(TestTimeouts::get(TimeoutCategory::HealthCheck))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return (url, None),
            _ => panic!("Custom server URL {} is not available", url),
        }
    }

    // Spawn our own server with dynamic port, in strict-roots mode: this test
    // asserts the tool defaults to the client's roots/list scope, which only
    // exists when no explicit fallback scope is configured (SPEC R5.2.2).
    let server = spawn_test_server_strict_roots()
        .await
        .expect("Failed to spawn test server");
    let url = server.base_url();
    (url, Some(server))
}

#[tokio::test]
async fn http_roots_handshake_then_tool_call_defaults_to_root() {
    let (base_url, _server) = get_server_url().await;
    eprintln!("Using server at {}", base_url);

    let temp_root = TempDir::new().expect("Failed to create temp root");
    let root_path = temp_root.path().to_path_buf();
    let mut mcp = McpTestClient::with_url(&base_url);
    mcp.initialize_with_roots("http_roots_test", std::slice::from_ref(&root_path))
        .await
        .expect("roots handshake should complete");

    // Intentionally omit working_directory: after the roots handshake the MCP service
    // should default tool execution to the first sandbox scope supplied by roots/list.
    let result = mcp
        .call_tool("run_terminal_command", json!({ "command": "pwd" }))
        .await;
    assert!(
        result.success,
        "tools/call should succeed after roots lock: {:?}",
        result.error
    );
    let out = result.output.unwrap_or_default();
    assert!(
        paths_equivalent(&out, &root_path),
        "pwd output should contain root path. got: {out}"
    );
}
