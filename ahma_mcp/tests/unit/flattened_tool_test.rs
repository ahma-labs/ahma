//! Integration test for tool name flattening
//!
//! This test verifies that subcommands are correctly exposed as flattened tools
//! (e.g., "file-tools_pwd") and can be called directly.

use ahma_mcp::test_utils::in_process::create_in_process_mcp_from_dir;
use ahma_mcp::utils::logging::init_test_logging;
use anyhow::Result;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use std::borrow::Cow;
use tokio::fs;

#[tokio::test]
async fn test_flattened_tool_calling() -> Result<()> {
    init_test_logging();

    // 1. Setup test tools directory with a tool that has subcommands
    let temp_dir = tempfile::tempdir()?;
    let tools_dir = temp_dir.path().join(".ahma");
    fs::create_dir_all(&tools_dir).await?;

    let tool_config = json!({
        "name": "file-tools",
        "description": "File manipulation tools",
        "command": "printf",
        "enabled": true,
        "subcommand": [
            {
                "name": "hello",
                "description": "Print hello",
                "enabled": true,
                "synchronous": true
            },
            {
                "name": "world",
                "description": "Print world",
                "enabled": true,
                "synchronous": true
            }
        ]
    });
    fs::write(
        tools_dir.join("file-tools.json"),
        serde_json::to_string(&tool_config)?,
    )
    .await?;

    // 2. Start the MCP server using in-process helper
    let mcp = create_in_process_mcp_from_dir(&tools_dir).await?;

    // 3. Verify that the flattened tools appear in list_tools
    let tools = mcp.client.list_all_tools().await.map_err(|e| {
        eprintln!("test_flattened_tool_calling: list_tools failed: {e}");
        e
    })?;
    let tool_names: Vec<_> = tools.iter().map(|t| t.name.as_ref() as &str).collect();

    assert!(
        tool_names.contains(&"file-tools_hello"),
        "Flattened tool 'file-tools_hello' should be listed. Got: {:?}",
        tool_names
    );
    assert!(
        tool_names.contains(&"file-tools_world"),
        "Flattened tool 'file-tools_world' should be listed. Got: {:?}",
        tool_names
    );

    // 4. Call the flattened tool directly
    let params = CallToolRequestParams::new(Cow::Borrowed("file-tools_hello"))
        .with_arguments(json!({}).as_object().unwrap().clone());

    let result = mcp.client.call_tool(params).await.map_err(|e| {
        eprintln!("test_flattened_tool_calling: call_tool failed: {e}");
        e
    })?;

    // The call should succeed
    assert!(
        !result.is_error.unwrap_or(false),
        "Call to flattened tool 'file-tools_hello' should succeed. Error: {:?}",
        result
    );

    Ok(())
}

/// Real-world reproduction of a dispatch bug found while dogfooding: `.ahma/gh.json`
/// authors its subcommands as a *flat* list whose `name` already contains an
/// underscore (`"pr_create"`, `"run_watch"`, `"workflow_view"`, …) rather than genuine
/// nested levels. This mirrors that shape (a `gh`-like tool with one flat subcommand
/// named `pr_create`) and drives the call through the real `tools/call` JSON-RPC path
/// — `resolve_configured_tool` → `find_tool_config` → `resolve_flattened_tool`
/// (splitting `gh_pr_create` into parent `gh` + path `pr_create`) → `resolve_subcommand`
/// → `find_subcommand_config_from_args` — not just the unit-level function in
/// `mcp_service::subcommand`, which `test_find_subcommand_flat_underscore_name` there
/// already covers in isolation.
#[tokio::test]
async fn test_flattened_tool_calling_with_flat_underscore_subcommand_name() -> Result<()> {
    init_test_logging();

    let temp_dir = tempfile::tempdir()?;
    let tools_dir = temp_dir.path().join(".ahma");
    fs::create_dir_all(&tools_dir).await?;

    let tool_config = json!({
        "name": "gh",
        "description": "GitHub CLI wrapper (test double)",
        "command": "printf",
        "enabled": true,
        "subcommand": [
            {
                "name": "pr_create",
                "description": "Create a pull request",
                "enabled": true,
                "synchronous": true
            }
        ]
    });
    fs::write(
        tools_dir.join("gh.json"),
        serde_json::to_string(&tool_config)?,
    )
    .await?;

    let mcp = create_in_process_mcp_from_dir(&tools_dir).await?;

    let tools = mcp.client.list_all_tools().await.map_err(|e| {
        eprintln!("test_flattened_tool_calling_with_flat_underscore_subcommand_name: list_tools failed: {e}");
        e
    })?;
    let tool_names: Vec<_> = tools.iter().map(|t| t.name.as_ref() as &str).collect();
    assert!(
        tool_names.contains(&"gh_pr_create"),
        "Flattened tool 'gh_pr_create' should be listed. Got: {:?}",
        tool_names
    );

    let params = CallToolRequestParams::new(Cow::Borrowed("gh_pr_create"))
        .with_arguments(json!({}).as_object().unwrap().clone());

    let result = mcp.client.call_tool(params).await.map_err(|e| {
        eprintln!("test_flattened_tool_calling_with_flat_underscore_subcommand_name: call_tool failed: {e}");
        e
    })?;

    assert!(
        !result.is_error.unwrap_or(false),
        "Call to flattened tool 'gh_pr_create' (a flat, underscore-named subcommand, \
         as in .ahma/gh.json) should succeed, not fail with 'Subcommand not found'. \
         Error: {:?}",
        result
    );

    Ok(())
}
