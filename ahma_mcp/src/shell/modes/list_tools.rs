//! # List Tools Mode
//!
//! Runs the ahma_mcp server in list-tools mode, which connects to an MCP server
//! and lists all available tools.

use crate::shell::{cli::AppConfig, list_tools};
use anyhow::{Result, anyhow};
use std::collections::HashMap;

/// Run in list-tools mode: connect to an MCP server and list all available tools.
///
/// # Arguments
/// * `config` - Immutable application configuration.
///
/// # Errors
/// Returns an error if the connection or listing fails.
pub async fn run_list_tools_mode(config: &AppConfig) -> Result<()> {
    // Determine connection mode
    let result = if let Some(ref http_url) = config.list_http {
        list_tools::list_tools_http(http_url).await?
    } else if config.run_tool.is_some() || !config.run_tool_args.is_empty() {
        // Build command args from run_tool (first positional) and run_tool_args (after --)
        let mut command_args: Vec<String> = Vec::new();
        if let Some(ref cmd) = config.run_tool {
            command_args.push(cmd.clone());
        }
        command_args.extend(config.run_tool_args.clone());

        if command_args.is_empty() {
            return Err(anyhow!(
                "No command specified for tool list. Provide server command after --"
            ));
        }

        list_tools::list_tools_stdio_with_env(&command_args, HashMap::new()).await?
    } else if config.mcp_config.exists() {
        list_tools::list_tools_from_config(&config.mcp_config, config.list_server.as_deref())
            .await?
    } else {
        return Err(anyhow!(
            "No connection method specified for tool list.\n\n\
             Suggestions:\n\
             1. To list locally configured tools in this project, use:\n\
                ahma tool info\n\n\
             2. To query a running HTTP MCP server:\n\
                ahma tool list --http http://localhost:3000\n\n\
             3. To query a stdio MCP server command directly:\n\
                ahma tool list -- <command> [args...]\n\n\
             4. To query an MCP server defined in a config file:\n\
                ahma tool list --mcp-config mcp.json --server <name>\n\n\
             For more help, run: ahma tool list --help"
        ));
    };

    // Output result
    match config.list_format {
        list_tools::OutputFormat::Text => list_tools::print_text_output(&result),
        list_tools::OutputFormat::Json => list_tools::print_json_output(&result)?,
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::cli::AppConfig;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    /// Short bound so a hung connection attempt fails the test loudly instead
    /// of stalling the suite. All branches under test are expected to error
    /// well within this window.
    const TEST_TIMEOUT: Duration = Duration::from_secs(20);

    /// Build a baseline config that selects none of the connection methods and
    /// points `mcp_config` at a path guaranteed not to exist.
    fn no_connection_config(temp: &TempDir) -> AppConfig {
        AppConfig {
            list_http: None,
            run_tool: None,
            run_tool_args: vec![],
            // Non-existent path → mcp_config.exists() is false.
            mcp_config: temp.path().join("definitely-missing-mcp.json"),
            ..AppConfig::default()
        }
    }

    /// The final `else` arm: no connection method selected → descriptive error.
    #[tokio::test]
    async fn run_list_tools_mode_errors_when_no_connection_method() {
        let temp = TempDir::new().unwrap();
        let config = no_connection_config(&temp);

        let result = timeout(TEST_TIMEOUT, run_list_tools_mode(&config))
            .await
            .expect("run_list_tools_mode should return promptly, not hang");

        let err = result.expect_err("expected an error when no connection method is specified");
        let msg = err.to_string();
        assert!(
            msg.contains("No connection method"),
            "error message should mention the missing connection method, got: {msg}"
        );
    }

    /// The stdio branch (`run_tool` is `Some`): a bogus binary makes the child
    /// process spawn / handshake fail fast, so the mode returns `Err`.
    #[tokio::test]
    async fn run_list_tools_mode_errors_for_nonexistent_stdio_command() {
        let temp = TempDir::new().unwrap();
        let mut config = no_connection_config(&temp);
        config.run_tool = Some("ahma-definitely-not-a-real-binary-xyz".to_string());

        let result = timeout(TEST_TIMEOUT, run_list_tools_mode(&config))
            .await
            .expect("stdio connection to a missing binary should fail fast, not hang");

        assert!(
            result.is_err(),
            "expected an error when the stdio server command does not exist"
        );
    }

    /// The stdio branch driven purely by `run_tool_args` (the
    /// `!config.run_tool_args.is_empty()` half of the `else if`). Also a bogus
    /// command → fast `Err`.
    #[tokio::test]
    async fn run_list_tools_mode_errors_for_stdio_args_only() {
        let temp = TempDir::new().unwrap();
        let mut config = no_connection_config(&temp);
        config.run_tool = None;
        config.run_tool_args = vec!["ahma-definitely-not-a-real-binary-xyz".to_string()];

        let result = timeout(TEST_TIMEOUT, run_list_tools_mode(&config))
            .await
            .expect("stdio connection via args to a missing binary should fail fast, not hang");

        assert!(
            result.is_err(),
            "expected an error when the stdio server command (from args) does not exist"
        );
    }

    /// The HTTP branch: an unreachable URL (port 1, connection refused) yields a
    /// fast `Err`. Bounded by `timeout` in case the transport were to stall.
    #[tokio::test]
    async fn run_list_tools_mode_errors_for_unreachable_http() {
        let temp = TempDir::new().unwrap();
        let mut config = no_connection_config(&temp);
        config.list_http = Some("http://127.0.0.1:1/".to_string());

        let result = timeout(TEST_TIMEOUT, run_list_tools_mode(&config))
            .await
            .expect("connecting to an unreachable HTTP endpoint should fail fast, not hang");

        assert!(
            result.is_err(),
            "expected an error when the HTTP MCP endpoint is unreachable"
        );
    }

    /// The HTTP branch with a syntactically invalid URL: `Url::parse` fails
    /// inside `list_tools_http`, surfacing as `Err` immediately.
    #[tokio::test]
    async fn run_list_tools_mode_errors_for_invalid_http_url() {
        let temp = TempDir::new().unwrap();
        let mut config = no_connection_config(&temp);
        config.list_http = Some("not a valid url".to_string());

        let result = timeout(TEST_TIMEOUT, run_list_tools_mode(&config))
            .await
            .expect("invalid URL parsing should fail fast, not hang");

        assert!(
            result.is_err(),
            "expected an error when the HTTP URL is invalid"
        );
    }
}
