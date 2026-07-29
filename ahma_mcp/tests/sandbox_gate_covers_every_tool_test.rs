//! Every tool that touches a workspace path waits for the sandbox scope
//! (SPEC R5.1.2, R5.2) — asserted per tool, not per handler.
//!
//! This is the regression test for a gap that existed for as long as the gate
//! did. The check was written *inside* `run_terminal_command` and inside the
//! configured-tool path, so those two were covered and the built-in file tools
//! — `write_file`, `replace_in_file`, `read_file`, `list_dir`, `file_search`,
//! `grep_search` — were not gated at all. They validate paths against the scope,
//! so a call arriving before the scope settled was validated against the
//! provisional pre-`roots/list` scope. The gate now runs once at dispatch;
//! this test is what keeps it there, and what will catch the next built-in
//! added without one.
//!
//! It also pins the *exemptions*. `status`/`await`/`cancel`/`sandbox_grant` must
//! stay answerable while the scope settles — gating `sandbox_grant` in
//! particular would be circular, since granting is how a scope gets widened.

use std::collections::HashMap;
use std::time::Duration;

use ahma_common::timeouts::TestTimeouts;
use ahma_mcp::test_utils::in_process::{
    InProcessMcp, create_in_process_mcp_with_client, create_in_process_mcp_with_scope,
};
use ahma_mcp::test_utils::recording_client::RecordingClient;
use ahma_mcp::utils::logging::init_test_logging;
use anyhow::Result;
use rmcp::model::CallToolRequestParams;
use serde_json::{Map, json};

/// The complete set of tools allowed to answer before the scope is settled.
///
/// Hard-coded on purpose, and deliberately *not* imported from the server:
/// importing the same constant the code uses would make this assertion
/// circular. Adding an exemption has to be a deliberate act that edits this
/// list — everything else in `tools/list` is required to be gated, so a new
/// built-in is covered the moment it is advertised.
const EXEMPT: &[&str] = &[
    "status",
    "await",
    "cancel",
    "sandbox_grant",
    "restart",
    "todo_write",
];

/// Exempt tools with side effects too large to invoke in a test. Their
/// membership in [`EXEMPT`] is still asserted; only the call is skipped.
const DO_NOT_INVOKE: &[&str] = &["restart"];

/// Tools that must be gated no matter what else changes. The wire-driven loop
/// below covers every advertised tool, but these are named explicitly because
/// they are the ones that were silently ungated: the check lived inside
/// `run_terminal_command`, so the file tools never had it.
const MUST_BE_GATED: &[&str] = &[
    "run_terminal_command",
    "read_file",
    "write_file",
    "replace_in_file",
    "list_dir",
    "file_search",
    "grep_search",
];

const GATE_MESSAGE: &str = "Sandbox initializing";

fn budget() -> Duration {
    TestTimeouts::scale_secs(30)
}

/// An in-process server whose sandbox scope never settles: the client answers
/// `roots/list` with an empty list, which is exactly what Cursor does in a
/// window with no workspace folder open.
async fn server_without_a_settled_scope() -> Result<InProcessMcp<RecordingClient>> {
    create_in_process_mcp_with_client(
        RecordingClient::new("cursor"),
        HashMap::new(),
        Vec::new(), // no scope — and the client offers none
    )
    .await
}

/// Drive every advertised tool and require the gate to apply to all of them
/// except the documented control surface.
///
/// Enumerating from `tools/list` rather than a fixed list is the point: a
/// built-in added tomorrow is covered the day it is advertised, without anyone
/// remembering to extend a test. That is what failed the first time — the gate
/// was correct in the two handlers it lived in, and absent from the six that
/// were added later.
#[tokio::test]
async fn every_advertised_tool_is_gated_unless_it_is_the_control_surface() -> Result<()> {
    init_test_logging();
    let mcp = server_without_a_settled_scope().await?;

    let tools = tokio::time::timeout(budget(), mcp.client.list_all_tools())
        .await
        .expect("tools/list must answer while the scope is unsettled")?;
    assert!(
        !tools.is_empty(),
        "no tools advertised — nothing was tested"
    );

    let advertised: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    for required in MUST_BE_GATED {
        assert!(
            advertised.iter().any(|n| n == required),
            "`{required}` is no longer advertised; this test would silently stop \
             covering it. Advertised: {advertised:?}"
        );
        assert!(
            !EXEMPT.contains(required),
            "`{required}` resolves workspace paths and must never be exempt"
        );
    }

    for name in &advertised {
        if DO_NOT_INVOKE.contains(&name.as_str()) {
            assert!(
                EXEMPT.contains(&name.as_str()),
                "`{name}` is skipped only because invoking it is unsafe in a \
                 test; that is justified only for exempt tools"
            );
            continue;
        }

        // Deliberately no arguments: the gate has to run *before* argument
        // validation. If a tool answers with a parameter error instead, the
        // handler was reached — which is the ungated state.
        let params = CallToolRequestParams::new(name.clone()).with_arguments(Map::new());
        let outcome = tokio::time::timeout(budget(), mcp.client.call_tool(params))
            .await
            .unwrap_or_else(|_| panic!("`{name}` must answer, not hang"));

        let message = match &outcome {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        };
        let was_gated = message.contains(GATE_MESSAGE);

        if EXEMPT.contains(&name.as_str()) {
            assert!(
                !was_gated,
                "`{name}` is the session's control surface and must answer while \
                 the scope is still settling; it was gated instead: {message}"
            );
        } else {
            assert!(
                was_gated,
                "`{name}` reached its handler with an unsettled sandbox scope. \
                 Every tool that is not on the documented exempt list must wait \
                 for the scope (SPEC R5.1.2). It responded with: {}",
                if message.is_empty() {
                    "a successful result".to_string()
                } else {
                    message
                }
            );
        }
    }

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// Discovery is never gated: a client has to be able to see the tool list while
/// its scope is being decided. Blocking `tools/list` on the scope is how a
/// captured Antigravity session ended up with no ahma tools for 44 minutes.
#[tokio::test]
async fn tool_discovery_is_never_gated() -> Result<()> {
    init_test_logging();
    let mcp = server_without_a_settled_scope().await?;

    let tools = tokio::time::timeout(budget(), mcp.client.list_all_tools())
        .await
        .expect("tools/list must answer while the scope is unsettled")?;
    assert!(
        !tools.is_empty(),
        "tools/list must return the built-ins regardless of sandbox state"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// The complement: once the scope *is* settled, the same tools run. Without
/// this, a gate that rejected everything unconditionally would pass the tests
/// above.
#[tokio::test]
async fn the_same_tools_run_once_the_scope_is_settled() -> Result<()> {
    init_test_logging();
    let temp = tempfile::tempdir()?;
    let ahma_dir = temp.path().join(".ahma");
    tokio::fs::create_dir_all(&ahma_dir).await?;
    tokio::fs::write(temp.path().join("hello.txt"), "hello from the sandbox\n").await?;

    let mcp = create_in_process_mcp_with_scope(&ahma_dir, vec![temp.path().to_path_buf()]).await?;

    let mut args = Map::new();
    args.insert(
        "path".to_string(),
        json!(temp.path().join("hello.txt").to_string_lossy()),
    );
    let result = tokio::time::timeout(
        budget(),
        mcp.client
            .call_tool(CallToolRequestParams::new("read_file").with_arguments(args)),
    )
    .await
    .expect("read_file must answer")?;

    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    assert!(
        text.contains("hello from the sandbox"),
        "a settled scope must let the gated tools through; got: {text:?}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}
