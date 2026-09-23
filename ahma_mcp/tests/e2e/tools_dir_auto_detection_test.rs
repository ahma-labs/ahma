//! Tests for automatic .ahma directory detection
//!
//! This test suite ensures that the ahma_mcp server correctly auto-detects
//! a .ahma directory in the current working directory when --tools-dir is
//! not explicitly provided.
//!
//! Requirements tested:
//! - R1.2.1: Auto-detection of .ahma in CWD when --tools-dir not provided
//! - R1.2.2: Explicit --tools-dir takes precedence over auto-detection
//! - Built-in tools (await, status, run_terminal_command) always available

use ahma_common::timeouts::TestTimeouts;
use ahma_mcp::test_utils::client::ClientBuilder;
use tempfile::TempDir;

/// Helper to create a tool config JSON
fn create_tool_config(name: &str, description: &str) -> String {
    format!(
        r#"{{
    "name": "{}",
    "description": "{}",
    "command": "echo",
    "enabled": true,
    "timeout_seconds": 10,
    "subcommand": [
        {{
            "name": "default",
            "description": "Default subcommand",
            "positional_args": [
                {{
                    "name": "message",
                    "type": "string",
                    "description": "Message to echo",
                    "required": false
                }}
            ]
        }}
    ]
}}"#,
        name, description
    )
}

/// Test that when no --tools-dir is provided and .ahma exists in CWD,
/// the server auto-detects and loads tools from .ahma
#[tokio::test]
async fn test_auto_detect_ahma_in_cwd() -> anyhow::Result<()> {
    // Create a temporary directory to act as CWD
    let temp_dir = TempDir::new()?;
    let cwd = temp_dir.path();

    // Create .ahma directory in CWD
    let ahma_dir = cwd.join(".ahma");
    std::fs::create_dir(&ahma_dir)?;

    // Create a test tool config in .ahma
    let test_tool_json = create_tool_config("test_tool", "A test tool for auto-detection");
    std::fs::write(ahma_dir.join("test_tool.json"), test_tool_json)?;

    // Start server with CWD set to temp_dir and NO --tools-dir argument
    // This should trigger auto-detection
    let service = ClientBuilder::new().working_dir(cwd).build().await?;

    // Give server a moment to initialize
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    // List tools
    let tools_result = service.list_tools(None).await?;
    let tools = tools_result.tools;

    // Verify built-in tools are present
    assert!(
        tools.iter().any(|t| t.name == "await"),
        "Built-in 'await' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "status"),
        "Built-in 'status' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "run_terminal_command"),
        "Built-in 'run_terminal_command' tool should be present"
    );

    // Verify auto-detected tool is present
    assert!(
        tools.iter().any(|t| t.name == "test_tool"),
        "Auto-detected 'test_tool' should be present from .ahma. Available tools: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    Ok(())
}

/// Test that explicit --tools-dir takes precedence over auto-detection
#[tokio::test]
async fn test_explicit_tools_dir_takes_precedence() -> anyhow::Result<()> {
    // Create two directories:
    // 1. CWD with .ahma containing tool_a
    // 2. Explicit tools dir containing tool_b
    let temp_cwd = TempDir::new()?;
    let cwd = temp_cwd.path();

    let temp_explicit = TempDir::new()?;
    let explicit_tools_dir = temp_explicit.path();

    // Create .ahma in CWD with tool_a
    let ahma_dir = cwd.join(".ahma");
    std::fs::create_dir(&ahma_dir)?;
    std::fs::write(
        ahma_dir.join("tool_a.json"),
        create_tool_config("tool_a", "Tool A in .ahma"),
    )?;

    // Create tool_b in explicit tools dir
    std::fs::write(
        explicit_tools_dir.join("tool_b.json"),
        create_tool_config("tool_b", "Tool B in explicit dir"),
    )?;

    // Start server with explicit --tools-dir pointing to explicit_tools_dir
    let service = ClientBuilder::new()
        .tools_dir(explicit_tools_dir)
        .working_dir(cwd)
        .build()
        .await?;

    tokio::time::sleep(TestTimeouts::short_delay()).await;

    // List tools
    let tools_result = service.list_tools(None).await?;
    let tools = tools_result.tools;

    // Verify built-in tools are present
    assert!(
        tools.iter().any(|t| t.name == "await"),
        "Built-in 'await' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "status"),
        "Built-in 'status' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "run_terminal_command"),
        "Built-in 'run_terminal_command' tool should be present"
    );

    // Verify tool_b is present (from explicit dir)
    assert!(
        tools.iter().any(|t| t.name == "tool_b"),
        "Explicit 'tool_b' should be present. Available tools: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // Verify tool_a is NOT present (should be ignored because explicit dir specified)
    assert!(
        !tools.iter().any(|t| t.name == "tool_a"),
        "Auto-detected 'tool_a' should NOT be present when explicit --tools-dir is provided"
    );

    Ok(())
}

/// Test that when no .ahma exists and no --tools-dir provided,
/// only built-in tools are available
#[tokio::test]
async fn test_no_ahma_fallback_to_builtin_tools() -> anyhow::Result<()> {
    // Create a temporary directory with NO .ahma subdirectory
    let temp_dir = TempDir::new()?;
    let cwd = temp_dir.path();

    // Start server with CWD set and no --tools-dir
    let service = ClientBuilder::new().working_dir(cwd).build().await?;

    tokio::time::sleep(TestTimeouts::short_delay()).await;

    // List tools
    let tools_result = service.list_tools(None).await?;
    let tools = tools_result.tools;

    // Verify only built-in tools are present
    let builtins = ahma_mcp::builtin_tool::BuiltinTool::ALL.len();
    assert_eq!(
        tools.len(),
        builtins,
        "Should have exactly the {builtins} built-in tools when no .ahma exists. Got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    assert!(
        tools.iter().any(|t| t.name == "agent"),
        "Built-in 'agent' sub-agent tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "todo_write"),
        "Built-in 'todo_write' plan tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "sandbox_grant"),
        "Built-in 'sandbox_grant' tool should be present"
    );

    assert!(
        tools.iter().any(|t| t.name == "await"),
        "Built-in 'await' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "status"),
        "Built-in 'status' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "run_terminal_command"),
        "Built-in 'run_terminal_command' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "logs_list"),
        "Built-in 'logs_list' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "logs_approve"),
        "Built-in 'logs_approve' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "logs_read"),
        "Built-in 'logs_read' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "logs_search"),
        "Built-in 'logs_search' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "restart"),
        "Built-in 'restart' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "cancel"),
        "Built-in 'cancel' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "read_file"),
        "Built-in 'read_file' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "list_dir"),
        "Built-in 'list_dir' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "file_search"),
        "Built-in 'file_search' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "grep_search"),
        "Built-in 'grep_search' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "fetch_webpage"),
        "Built-in 'fetch_webpage' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "write_file"),
        "Built-in 'write_file' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "replace_in_file"),
        "Built-in 'replace_in_file' tool should be present"
    );
    assert!(
        tools.iter().any(|t| t.name == "log_monitor"),
        "Built-in 'log_monitor' tool should be present"
    );

    Ok(())
}

/// Test that run_terminal_command is available as a core built-in even without any .ahma/ directory
#[tokio::test]
async fn test_run_terminal_command_builtin_without_json_file() -> anyhow::Result<()> {
    // Create a temp directory with NO .ahma
    let temp_dir = TempDir::new()?;
    let cwd = temp_dir.path();

    // Start server
    let service = ClientBuilder::new().working_dir(cwd).build().await?;
    tokio::time::sleep(TestTimeouts::short_delay()).await;

    // List tools
    let tools_result = service.list_tools(None).await?;
    let tools = tools_result.tools;

    // Verify run_terminal_command is present even without JSON file
    let run_terminal_command = tools
        .iter()
        .find(|t| t.name == "run_terminal_command")
        .expect("run_terminal_command should be present as built-in tool");

    // Verify it has the expected description
    assert!(
        run_terminal_command
            .description
            .as_ref()
            .map(|d| d.contains("run_terminal_command")
                || d.contains("sandbox")
                || d.contains("operation_id"))
            .unwrap_or(false),
        "run_terminal_command should have proper description. Got: {:?}",
        run_terminal_command.description
    );

    Ok(())
}
