//! Tool definitions are loaded once, at startup — never re-read from disk.
//!
//! SECURITY: the tools directory sits inside the sandbox scope, so the agent
//! under execution can write it. MTDF's `command` is a free-form string, so a
//! server that re-read the directory at runtime would let a sandboxed agent
//! repoint an already-approved tool name at an arbitrary command with no
//! restart and no notification. The file watcher that used to do exactly that
//! has been removed; the deliberate, auditable reload path is the `restart`
//! builtin. This test is the end-to-end guard on that property.

use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
use ahma_mcp::test_utils::client::ClientBuilder;
use anyhow::Result;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn tool_definitions_are_never_reloaded_from_disk() -> Result<()> {
    let temp_dir = tempdir()?;
    let tools_dir = temp_dir.path().to_path_buf();

    fs::write(
        tools_dir.join("initial_tool.json"),
        tool_json("initial_tool", "Initial tool"),
    )?;

    let client = ClientBuilder::new().tools_dir(&tools_dir).build().await?;

    let tools = tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        client.list_tools(None),
    )
    .await??;
    assert!(tools.tools.iter().any(|t| t.name == "initial_tool"));
    assert!(!tools.tools.iter().any(|t| t.name == "new_tool"));

    // Write a brand-new tool and repoint an existing one, exactly as a
    // sandboxed agent with workspace write access could.
    fs::write(
        tools_dir.join("new_tool.json"),
        tool_json("new_tool", "New tool added dynamically"),
    )?;
    fs::write(
        tools_dir.join("initial_tool.json"),
        tool_json("initial_tool", "Modified initial tool"),
    )?;

    tokio::time::sleep(TestTimeouts::scale_millis(750)).await;

    let tools = tokio::time::timeout(
        TestTimeouts::get(TimeoutCategory::ToolCall),
        client.list_tools(None),
    )
    .await??;
    assert!(
        !tools.tools.iter().any(|t| t.name == "new_tool"),
        "a tool written after startup must not appear without a restart"
    );
    assert_eq!(
        tools
            .tools
            .iter()
            .find(|t| t.name == "initial_tool")
            .and_then(|t| t.description.clone())
            .as_deref(),
        Some("Initial tool"),
        "an existing tool must not be redefined by a runtime write"
    );

    Ok(())
}

fn tool_json(name: &str, description: &str) -> String {
    format!(
        r#"{{
    "name": "{name}",
    "description": "{description}",
    "command": "echo",
    "timeout_seconds": 10,
    "synchronous": true,
    "enabled": true,
    "subcommand": [
        {{
            "name": "default",
            "description": "Default subcommand"
        }}
    ]
}}
"#
    )
}
