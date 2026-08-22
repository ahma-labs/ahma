//! Shared test utilities for HTTP bridge integration tests.
//!
//! This module intentionally re-exports focused helpers from smaller modules to
//! keep call sites stable while reducing complexity in test infrastructure.

#![allow(dead_code)]

use ahma_common::timeouts::TestTimeouts;

pub mod client;
pub mod protocol;
pub mod sandbox_env;
pub mod server;
pub mod uri;

#[allow(unused_imports)]
pub use client::{McpTestClient, ToolCallResult, TransportMode};
#[allow(unused_imports)]
pub use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
#[allow(unused_imports)]
pub use sandbox_env::{SANDBOX_BYPASS_ENV_VARS, SandboxTestEnv};
#[allow(unused_imports)]
pub use server::{
    InProcessServerInstance, ServerGuard, TestServerInstance, spawn_in_process_server,
    spawn_server_guard_strict_roots, spawn_server_guard_with_config,
    spawn_server_guard_with_config_extra_env, spawn_server_guard_with_deferred_sandbox,
    spawn_test_server, spawn_test_server_strict_roots, spawn_test_server_with_timeout,
};
#[allow(unused_imports)]
pub use uri::{
    create_pwd_tool_config, encode_file_uri, malformed_uris, normalize_path_for_comparison,
    parse_file_uri, paths_equivalent, write_pwd_tool_config,
};

/// Create an HTTP/2-only reqwest client for use against the bridge server.
///
/// The server only accepts HTTP/2 (h2c). HTTP/1.1 connections are rejected.
pub fn make_h2_client() -> reqwest::Client {
    reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("Failed to build HTTP/2 test client")
}

/// Spawn and handshake a test MCP server/client, then verify required tools exist.
///
/// Returns `None` (skip) when infrastructure setup fails or required tools are missing.
pub async fn setup_test_mcp_for_tools(
    transport: TransportMode,
    required_tools: &[&str],
) -> Option<(TestServerInstance, McpTestClient)> {
    let (server, mcp) = setup_test_mcp(transport).await?;

    let mut missing = Vec::new();
    for tool in required_tools {
        if !mcp.is_tool_available(tool).await {
            missing.push(*tool);
        }
    }

    if missing.is_empty() {
        return Some((server, mcp));
    }

    eprintln!("WARNING  missing required tools, skipping: {:?}", missing);
    None
}

/// Asserts that a tool call succeeded and returns a borrowed output string.
pub fn assert_tool_success_with_output<'a>(result: &'a ToolCallResult, context: &str) -> &'a str {
    assert!(result.success, "{} failed: {:?}", context, result.error);
    result
        .output
        .as_deref()
        .unwrap_or_else(|| panic!("{} returned no output", context))
}

/// Heuristic helper for tools that may legitimately return async operation hints.
pub fn is_async_operation_output(output: &str) -> bool {
    output.contains("operation")
        || output.contains("async")
        || output.contains("started")
        || output.contains("op_")
}

/// Spawn a test server and complete the full MCP handshake.
///
/// Returns `None` with a warning on any infrastructure failure so that tests
/// can skip gracefully instead of panicking.  The returned
/// [`TestServerInstance`] **must** be kept alive for the duration of the test.
pub async fn setup_test_mcp(
    transport: TransportMode,
) -> Option<(TestServerInstance, McpTestClient)> {
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let handshake_timeout = TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Handshake);
    let mut last_error = String::new();
    for attempt in 1..=2u32 {
        let server = match spawn_test_server().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("WARNING  setup_test_mcp: server spawn failed: {}", e);
                return None;
            }
        };
        let mut mcp = McpTestClient::with_url(&server.base_url()).with_transport(transport);
        match tokio::time::timeout(
            handshake_timeout,
            mcp.initialize_with_roots("tool-test-client", std::slice::from_ref(&root)),
        )
        .await
        {
            Ok(Ok(_)) => return Some((server, mcp)),
            Ok(Err(e)) => last_error = e.to_string(),
            Err(_) => last_error = format!("handshake timed out after {:?}", handshake_timeout),
        }
        if attempt == 1 {
            eprintln!(
                "WARNING  setup_test_mcp: handshake attempt {} failed: {}. Retrying...",
                attempt, last_error
            );
        }
    }
    // In CI (or when AHMA_TEST_FAIL_ON_SETUP_ERROR is set) a persistent handshake
    // failure is a real regression, not a local infrastructure hiccup; panic so the
    // build fails visibly instead of silently skipping the test.
    let in_ci =
        std::env::var("CI").is_ok() || std::env::var("AHMA_TEST_FAIL_ON_SETUP_ERROR").is_ok();
    if in_ci {
        panic!(
            "setup_test_mcp: handshake failed in CI — aborting: {}",
            last_error
        );
    }
    eprintln!(
        "WARNING  setup_test_mcp: skipping due to persistent handshake failure: {}",
        last_error
    );
    None
}

/// Spawn an **in-process** bridge (no subprocess) and complete the MCP handshake.
///
/// This is the P5 fast-path alternative to [`setup_test_mcp`]:
/// - The bridge runs in a background tokio task (no forked process)
/// - Sessions use `NullPeerFactory` by default (EOF-returning in-memory peer)
///   so the handshake completes without spawning `ahma_mcp`
/// - Only the TCP loopback socket is real
///
/// Use this for tests that exercise bridge behavior (session lifecycle,
/// handshake state machine, auth middleware, CORS) without needing real tool
/// execution.  For tests that need actual tool calls, build an
/// `InProcessMcpPeerFactory` and use `spawn_in_process_server` directly.
///
/// Returns `None` on infrastructure failure (skips the test).
pub async fn setup_in_process_mcp(
    transport: TransportMode,
) -> Option<(InProcessServerInstance, McpTestClient)> {
    use ahma_mcp::test_utils::bridge_peer::NullPeerFactory;

    let factory = std::sync::Arc::new(NullPeerFactory);
    let server = match spawn_in_process_server(factory).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("WARNING  setup_in_process_mcp: bridge spawn failed: {e}");
            return None;
        }
    };

    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let handshake_timeout = TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Handshake);
    let mut mcp = McpTestClient::with_url(&server.base_url()).with_transport(transport);

    match tokio::time::timeout(
        handshake_timeout,
        mcp.initialize_with_roots("in-process-test-client", std::slice::from_ref(&root)),
    )
    .await
    {
        Ok(Ok(_)) => Some((server, mcp)),
        Ok(Err(e)) => {
            eprintln!("WARNING  setup_in_process_mcp: handshake failed: {e}");
            None
        }
        Err(_) => {
            eprintln!(
                "WARNING  setup_in_process_mcp: handshake timed out after {:?}",
                handshake_timeout
            );
            None
        }
    }
}
