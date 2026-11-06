//! SPEC R1.5.6: the MCP tool hints (`annotations`) a client receives in
//! `tools/list`, asserted on the wire.

use ahma_mcp::test_utils::in_process::create_in_process_mcp_from_dir;
use anyhow::Result;
use rmcp::model::Tool;
use serde_json::json;

fn find<'a>(tools: &'a [Tool], name: &str) -> &'a Tool {
    tools
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("{name} not listed"))
}

fn read_only(tool: &Tool) -> Option<bool> {
    tool.annotations.as_ref().and_then(|a| a.read_only_hint)
}

/// Built-ins carry their hints: a client may run `status` without asking,
/// and is told `run_terminal_command` is neither read-only nor harmless.
#[tokio::test]
async fn builtins_carry_their_hints_on_the_wire() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mcp = create_in_process_mcp_from_dir(dir.path()).await?;
    let tools = mcp.client.list_all_tools().await?;

    let status = find(&tools, "status");
    assert_eq!(read_only(status), Some(true), "{status:?}");
    assert_eq!(
        status.annotations.as_ref().and_then(|a| a.open_world_hint),
        Some(false)
    );

    let run = find(&tools, "run_terminal_command");
    assert_eq!(read_only(run), Some(false));
    assert_eq!(
        run.annotations.as_ref().and_then(|a| a.destructive_hint),
        Some(true)
    );

    let grant = find(&tools, "sandbox_grant");
    assert_eq!(
        read_only(grant),
        Some(false),
        "a grant tool is never read-only"
    );
    assert!(grant.meta.is_some(), "and still requires the human");

    // The JSON a client parses uses the MCP spelling.
    let wire = serde_json::to_value(status)?;
    assert_eq!(wire["annotations"]["readOnlyHint"], json!(true), "{wire}");
    Ok(())
}

/// A configured tool is read-only when its author said `mutates: false`, or
/// when its declared read-only lane is enforced by the kernel, which this
/// sandbox reports. A declared lane alone must not claim read-only where it is
/// not enforced. A tool that says nothing carries no hint, leaving the spec's
/// cautious defaults.
#[tokio::test]
async fn configured_tools_claim_read_only_only_when_it_holds() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let tools_dir = dir.path().join(".ahma");
    std::fs::create_dir_all(&tools_dir)?;
    let config = json!({
        "name": "probe",
        "description": "Probe tool",
        "command": "printf",
        "enabled": true,
        "subcommand": [
            {"name": "said", "description": "x", "enabled": true, "mutates": false},
            {"name": "lane", "description": "x", "enabled": true, "concurrency": "read_only"},
            {"name": "silent", "description": "x", "enabled": true}
        ]
    });
    std::fs::write(
        tools_dir.join("probe.json"),
        serde_json::to_string(&config)?,
    )?;
    let mcp = create_in_process_mcp_from_dir(&tools_dir).await?;
    let tools = mcp.client.list_all_tools().await?;

    assert_eq!(read_only(find(&tools, "probe_said")), Some(true));
    // The lane earns the hint exactly where this sandbox can enforce it:
    // Linux with Landlock can; macOS nested in a sandbox, Windows and Test
    // mode cannot.
    let enforced = mcp.service.adapter.sandbox().can_enforce_read_only();
    assert_eq!(
        read_only(find(&tools, "probe_lane")) == Some(true),
        enforced,
        "the lane claims read-only only where the kernel enforces it (enforced: {enforced})"
    );
    assert!(find(&tools, "probe_silent").annotations.is_none());
    Ok(())
}
