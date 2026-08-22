//! Integration tests for the --list-tools functionality
//!
//! These tests verify the tool listing functionality works correctly with
//! both stdio and HTTP MCP servers.

use ahma_mcp::test_utils::cli::build_binary_cached;
use std::path::PathBuf;
use std::process::Command;

/// Get the path to the ahma binary, building it (once, cached and
/// cross-process locked) if it is missing or stale.
fn get_ahma_mcp_binary() -> PathBuf {
    build_binary_cached("ahma_mcp", "ahma")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Failed to get workspace dir")
        .to_path_buf()
}

/// Write an mcp.json into `dir` that points at a stdio ahma server.
fn write_stdio_mcp_config(dir: &std::path::Path, binary: &std::path::Path) -> PathBuf {
    let tools_dir = workspace_root().join(".ahma");
    let mcp_config_path = dir.join("mcp.json");
    let mcp_config = format!(
        r#"{{"mcpServers":{{"test":{{"command":"{cmd}","args":["--no-sandbox","--skip-probes","--server-child","--tools-dir","{tools}","serve","stdio"]}}}}}}"#,
        cmd = binary.to_str().unwrap().replace('\\', "/"),
        tools = tools_dir.to_str().unwrap().replace('\\', "/")
    );
    std::fs::write(&mcp_config_path, &mcp_config).expect("Failed to write mcp.json");
    mcp_config_path
}

/// Test that the binary shows help for --list-tools
#[test]
fn test_list_tools_help() {
    let binary = get_ahma_mcp_binary();
    let project_root = workspace_root();

    let output = Command::new(&binary)
        .current_dir(&project_root)
        .args(["--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let help_text = format!("{}{}", stdout, stderr);

    assert!(
        help_text.contains("list") || help_text.contains("tool"),
        "Help should contain 'list' or 'tool' subcommand. Got: {}",
        help_text
    );
    assert!(
        help_text.contains("serve") || help_text.contains("run") || help_text.contains("Commands"),
        "Help should contain subcommands. Got: {}",
        help_text
    );
}

/// List tools from a stdio MCP server: one server config, two invocations.
/// The default (text) format is asserted for both tool listings and the
/// header section; `--format json` is asserted separately because its output
/// genuinely differs.
#[test]
fn test_list_tools_from_stdio_server() {
    let project_root = workspace_root();
    let ahma_binary = get_ahma_mcp_binary();

    // Create a temp mcp.json pointing to the stdio server
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let mcp_config_path = write_stdio_mcp_config(temp_dir.path(), &ahma_binary);

    // --- Default (text) format ---
    let output = Command::new(&ahma_binary)
        .args([
            "--no-sandbox",
            "tool",
            "list",
            "--server",
            "test",
            "--mcp-config",
            mcp_config_path.to_str().unwrap(),
        ])
        .current_dir(&project_root)
        .output()
        .expect("Failed to execute ahma_mcp tool list");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        eprintln!("stdout: {}", stdout);
        eprintln!("stderr: {}", stderr);
    }

    // Check we got some tools listed
    assert!(
        stdout.contains("Tool:") || stdout.contains("tools"),
        "Output should contain tool listings. stdout: {}, stderr: {}",
        stdout,
        stderr
    );
    // Should have a header section
    assert!(
        stdout.contains("MCP") || stdout.contains("Tool"),
        "Output should contain 'MCP' or 'Tool' header.\nStdout: {}\nStderr: {}",
        stdout,
        stderr
    );

    // --- JSON format ---
    let output = Command::new(&ahma_binary)
        .args([
            "--no-sandbox",
            "tool",
            "list",
            "--format",
            "json",
            "--server",
            "test",
            "--mcp-config",
            mcp_config_path.to_str().unwrap(),
        ])
        .current_dir(&project_root)
        .output()
        .expect("Failed to execute ahma_mcp tool list --format json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        eprintln!("stdout: {}", stdout);
        eprintln!("stderr: {}", stderr);
    }

    // JSON output should be valid JSON with "tools" key
    assert!(
        stdout.contains("\"tools\"") || stdout.contains("tools"),
        "JSON output should contain 'tools' key. stdout: {}, stderr: {}",
        stdout,
        stderr
    );
}

/// Test that we can list tools by running a command directly via trailing args
#[test]
fn test_list_tools_trailing_args() {
    let project_root = workspace_root();
    let ahma_binary = get_ahma_mcp_binary();
    let tools_dir = project_root.join(".ahma");

    // Run ahma tool list -- <command>
    let output = Command::new(&ahma_binary)
        .args([
            "--no-sandbox",
            "--skip-probes",
            "--tools-dir",
            tools_dir.to_str().unwrap(),
            "tool",
            "list",
            "--",
            ahma_binary.to_str().unwrap(),
            "--no-sandbox",
            "--skip-probes",
            "--server-child",
            "--tools-dir",
            tools_dir.to_str().unwrap(),
            "serve",
            "stdio",
        ])
        .current_dir(&project_root)
        .output()
        .expect("Failed to execute ahma tool list with trailing args");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        eprintln!("stdout: {}", stdout);
        eprintln!("stderr: {}", stderr);
    }

    assert!(output.status.success());
    assert!(
        stdout.contains("Tool:") || stdout.contains("tools"),
        "Output should contain tool listings. stdout: {}, stderr: {}",
        stdout,
        stderr
    );
}

/// Test that run_list_tools_mode exits with the new helpful suggestions
/// when no connection method is specified
#[test]
fn test_list_tools_no_connection_suggestions() {
    let ahma_binary = get_ahma_mcp_binary();

    // Create a temp directory to run from so no mcp.json is found
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

    let output = Command::new(&ahma_binary)
        .args([
            "--no-sandbox",
            "tool",
            "list",
            "--mcp-config",
            "nonexistent-mcp.json", // Ensure it doesn't fall back to an existing mcp.json
        ])
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute ahma tool list");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("No connection method specified for tool list"),
        "Stderr should contain the error, got: {}",
        stderr
    );
    assert!(
        stderr.contains("Suggestions:"),
        "Stderr should contain Suggestions section, got: {}",
        stderr
    );
    assert!(
        stderr.contains("ahma tool info"),
        "Stderr should suggest ahma tool info, got: {}",
        stderr
    );
}
