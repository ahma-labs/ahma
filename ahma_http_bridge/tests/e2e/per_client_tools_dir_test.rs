//! Per-Client Tools Directory Discovery
//!
//! Regression test for the multi-window scenario where each connected VS Code
//! window points at a different repository and expects its own
//! `<workspace_root>/.ahma/` directory to drive tool registration in addition
//! to the server's built-ins.
//!
//! When the client returns its workspace root via `roots/list`, the server
//! should:
//!
//! 1. Lock the sandbox to that root (existing behavior).
//! 2. Look for `<root>/.ahma/` and, if present, reload tool configs from it.
//! 3. Emit `tools/list_changed` so the client refreshes its tool list.
//!
//! Per AGENTS.md §R15.5 the test runs over both JSON and SSE transports.

use crate::common;

use ahma_common::timeouts::TestTimeouts;
use common::{McpTestClient, TransportMode, spawn_server_guard_strict_roots};
use serde_json::{Value, json};
use std::path::PathBuf;
use tempfile::TempDir;

/// Stage a tempdir that looks like a VS Code workspace with its own
/// `.ahma/per_client_demo.json` tool definition.  The tool name is unique so it
/// cannot accidentally collide with built-ins or the workspace `.ahma/` configs
/// loaded by the bridge at startup.
fn stage_client_workspace(tool_name: &str) -> TempDir {
    let workspace = TempDir::new().expect("create client workspace tempdir");
    let ahma_dir = workspace.path().join(".ahma");
    std::fs::create_dir_all(&ahma_dir).expect("create .ahma dir");

    let tool_config = json!({
        "name": tool_name,
        "description": "Synthetic per-client tool used by per_client_tools_dir_test.",
        "command": "echo",
        "enabled": true,
        "subcommand": [{
            "name": "default",
            "description": "Print a marker string.",
            "synchronous": true,
        }],
    });
    std::fs::write(
        ahma_dir.join(format!("{tool_name}.json")),
        serde_json::to_string_pretty(&tool_config).unwrap(),
    )
    .expect("write per-client tool config");

    workspace
}

fn tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

async fn run_per_client_tools_dir_discovery(transport: TransportMode) {
    // Pick a tool name unique per transport so parallel test execution can't
    // confuse the two cases.
    let tool_name = match transport {
        TransportMode::Json => "per_client_demo_json",
        TransportMode::Sse => "per_client_demo_sse",
    };

    let client_workspace = stage_client_workspace(tool_name);
    let workspace_path = client_workspace.path().to_path_buf();

    // Bridge tools_dir: the bridge's own workspace `.ahma/`.  We point it at a
    // separate empty tempdir so that any per-client tool we observe must have
    // come from the *client* workspace discovery path, not from startup load.
    let bridge_tools_dir = TempDir::new().expect("bridge tools tempdir");

    // Strict-roots mode: the per-client `.ahma/` discovery root must come from
    // the client's roots/list answer. (A bridge-level fallback scope is an
    // explicit scope and is locked without querying roots at all, per R5.2.2.)
    let server = match spawn_server_guard_strict_roots(bridge_tools_dir.path()).await {
        Ok(s) => s,
        Err(e) => {
            common::skip_or_fail(&format!(
                "per_client_tools_dir_test: server spawn failed: {e}"
            ));
            return;
        }
    };

    let mut mcp = McpTestClient::with_url(&server.base_url()).with_transport(transport);
    let roots: [PathBuf; 1] = [workspace_path.clone()];
    if let Err(e) = mcp
        .initialize_with_roots("per-client-tools-test", &roots)
        .await
    {
        panic!("handshake failed: {e}");
    }

    // Give the per-client reload (kicked off inside configure_sandbox_from_roots)
    // a brief moment to publish the new tools/list before we query.  The
    // server fires `tools/list_changed` after `update_tools`, but our test
    // client doesn't subscribe — a short poll loop is the simplest robust path.
    let mut found = false;
    let mut last_names: Vec<String> = Vec::new();
    let poll_interval = TestTimeouts::poll_interval();
    let max_attempts =
        (TestTimeouts::scale_secs(4).as_millis() / poll_interval.as_millis()).max(1) as usize;
    for _ in 0..max_attempts {
        match mcp.list_tools().await {
            Ok(tools) => {
                last_names = tool_names(&tools);
                if last_names.iter().any(|n| n == tool_name) {
                    found = true;
                    break;
                }
            }
            Err(e) => {
                eprintln!("list_tools error (will retry): {e}");
            }
        }
        tokio::time::sleep(poll_interval).await;
    }

    assert!(
        found,
        "expected per-client tool `{tool_name}` to appear in tools/list after roots handshake; \
         got: {last_names:?}"
    );
}

#[tokio::test]
async fn per_client_tools_dir_discovery_json() {
    run_per_client_tools_dir_discovery(TransportMode::Json).await;
}

#[tokio::test]
async fn per_client_tools_dir_discovery_sse() {
    run_per_client_tools_dir_discovery(TransportMode::Sse).await;
}
