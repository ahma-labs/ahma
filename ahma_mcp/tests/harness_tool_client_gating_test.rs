//! Clients with native file-editing tools (Claude Code, Cursor, VS Code) should
//! not see ahma's harness file tools (`read_file`, `write_file`,
//! `replace_in_file`, `list_dir`, `file_search`, `grep_search`, `todo_write`) in
//! `tools/list` — those clients already have their own Read/Write/Edit/Glob/Grep,
//! and duplicate tools compete for the model's attention and can be reached for
//! by a subagent that lacks its harness's native equivalent (observed in the
//! wild: a Claude Code plan-mode subagent with no native `Write` found and
//! called `mcp__Ahma__write_file` via tool search).
//!
//! Clients without native equivalents (ahma's own agent loop / TUI, or an
//! unrecognized bare MCP client) must keep seeing the full set — this is not a
//! deprecation, only a per-client visibility gate.

use std::collections::HashMap;

use ahma_mcp::test_utils::in_process::create_in_process_mcp_with_client;
use ahma_mcp::test_utils::recording_client::RecordingClient;
use anyhow::Result;

const HARNESS_FILE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "replace_in_file",
    "list_dir",
    "file_search",
    "grep_search",
    "todo_write",
];

const NO_NATIVE_EQUIVALENT: &[&str] = &[
    "run_terminal_command",
    "await",
    "status",
    "cancel",
    "restart",
    "agent",
    "fetch_webpage",
    "sandbox_grant",
];

async fn tool_names_for(client_name: &str) -> Result<Vec<String>> {
    let temp = tempfile::tempdir()?;
    let client = RecordingClient::new(client_name);
    let mcp =
        create_in_process_mcp_with_client(client, HashMap::new(), vec![temp.path().to_path_buf()])
            .await?;
    let tools = mcp.client.list_all_tools().await?;
    Ok(tools.into_iter().map(|t| t.name.to_string()).collect())
}

#[tokio::test]
async fn claude_code_does_not_see_harness_file_tools() -> Result<()> {
    let names = tool_names_for("claude-code").await?;
    for hidden in HARNESS_FILE_TOOLS {
        assert!(
            !names.contains(&hidden.to_string()),
            "expected '{hidden}' to be hidden from claude-code, but tools/list had it: {names:?}"
        );
    }
    for kept in NO_NATIVE_EQUIVALENT {
        assert!(
            names.contains(&kept.to_string()),
            "expected '{kept}' to remain visible to claude-code, but tools/list did not have it: {names:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn cursor_does_not_see_harness_file_tools() -> Result<()> {
    let names = tool_names_for("cursor").await?;
    for hidden in HARNESS_FILE_TOOLS {
        assert!(
            !names.contains(&hidden.to_string()),
            "expected '{hidden}' to be hidden from cursor, but tools/list had it: {names:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unrecognized_client_still_sees_full_harness_tool_set() -> Result<()> {
    let names = tool_names_for("some-bare-mcp-client").await?;
    for kept in HARNESS_FILE_TOOLS {
        assert!(
            names.contains(&kept.to_string()),
            "expected '{kept}' to remain visible to an unrecognized client, but tools/list did not have it: {names:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn ahma_own_client_still_sees_full_harness_tool_set() -> Result<()> {
    let names = tool_names_for("ahma-tui").await?;
    for kept in HARNESS_FILE_TOOLS {
        assert!(
            names.contains(&kept.to_string()),
            "expected '{kept}' to remain visible to ahma-tui, but tools/list did not have it: {names:?}"
        );
    }
    Ok(())
}
