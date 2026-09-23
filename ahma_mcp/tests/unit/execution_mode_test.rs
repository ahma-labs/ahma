//! `tools.execution_mode` (SPEC R2.1, R2.4), observed over the MCP wire.
//!
//! Both modes run every call as a tracked operation. `sync` waits for the
//! command to finish — as long as the client can hold the request open, the
//! same bound `await` uses — and `async` answers after a short adaptive
//! window. A small request budget makes the two distinguishable quickly: the
//! adaptive window is at most half the budget, the sync wait is all of it.

use ahma_common::config::ExecutionPolicy;
use ahma_common::timeouts::TestTimeouts;
use ahma_mcp::shell::cli::AppConfig;
use ahma_mcp::test_utils::in_process::{InProcessMcp, create_in_process_mcp_with_scope};
use anyhow::Result;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::json;
use std::sync::Arc;

/// The single-request budget the tests pin (seconds, platform-scaled).
fn budget_secs() -> u64 {
    TestTimeouts::scale_secs(4).as_secs()
}

/// Sleep for `secs`, then print `done`, in the platform shell.
fn sleep_then_done(secs: u64) -> String {
    if cfg!(windows) {
        format!("Start-Sleep -Seconds {secs}; Write-Output done")
    } else {
        format!("sleep {secs}; echo done")
    }
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

async fn server(mode: ExecutionPolicy) -> Result<(InProcessMcp, tempfile::TempDir)> {
    let temp = tempfile::tempdir()?;
    let ahma_dir = temp.path().join(".ahma");
    tokio::fs::create_dir_all(&ahma_dir).await?;
    let mcp = create_in_process_mcp_with_scope(&ahma_dir, vec![temp.path().to_path_buf()]).await?;
    mcp.service.set_app_config(Arc::new(AppConfig {
        execution_mode: mode,
        request_budget_override_secs: Some(budget_secs()),
        ..AppConfig::default()
    }));
    Ok((mcp, temp))
}

async fn run(mcp: &InProcessMcp, command: &str) -> Result<CallToolResult> {
    let params = CallToolRequestParams::new("run_terminal_command").with_arguments(
        json!({ "command": command })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let limit = TestTimeouts::scale_secs(60);
    Ok(tokio::time::timeout(limit, mcp.client.call_tool(params))
        .await
        .expect("run_terminal_command must answer")?)
}

/// A command longer than the adaptive window but within the budget: sync
/// returns its output, async returns an id.
#[tokio::test]
async fn sync_waits_for_a_command_async_hands_back_an_id() -> Result<()> {
    // Longer than half the budget (the adaptive window), shorter than it.
    let secs = budget_secs() * 3 / 4;

    let (mcp, _t) = server(ExecutionPolicy::Sync).await?;
    let text = result_text(&run(&mcp, &sleep_then_done(secs)).await?);
    assert!(text.contains("done"), "sync returns the output: {text}");
    assert!(!text.starts_with("AHMA ID"), "sync is not an id: {text}");

    let (mcp, _t) = server(ExecutionPolicy::Async).await?;
    let text = result_text(&run(&mcp, &sleep_then_done(secs)).await?);
    assert!(text.contains("AHMA ID"), "async returns an id: {text}");
    Ok(())
}

/// Sync still runs a tracked operation: a finished sync call is in `status`,
/// like any other operation (visible to the TUI, the audit log, `cancel`).
#[tokio::test]
async fn a_sync_call_is_a_tracked_operation() -> Result<()> {
    let (mcp, _t) = server(ExecutionPolicy::Sync).await?;
    let text = result_text(&run(&mcp, &sleep_then_done(0)).await?);
    assert!(text.contains("done"), "{text}");

    let status = mcp
        .client
        .call_tool(CallToolRequestParams::new("status"))
        .await?;
    let status = result_text(&status);
    assert!(
        status.contains("run_terminal_command"),
        "the sync call is a monitored operation: {status}"
    );
    Ok(())
}

/// A command that outlasts what the client can wait for is not lost: sync
/// returns its id at the budget and says to collect it with `await`.
#[tokio::test]
async fn sync_hands_back_an_id_when_the_client_cannot_wait_longer() -> Result<()> {
    let (mcp, _t) = server(ExecutionPolicy::Sync).await?;
    let started = std::time::Instant::now();
    let text = result_text(&run(&mcp, &sleep_then_done(budget_secs() * 10)).await?);
    assert!(text.contains("AHMA ID"), "{text}");
    assert!(
        text.contains("Still running") && text.contains("call `await`"),
        "tells the caller why it returned and how to collect: {text}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(budget_secs() * 5),
        "bounded by the budget, not the command"
    );

    // Leave nothing running behind the test.
    let _ = mcp
        .client
        .call_tool(CallToolRequestParams::new("cancel"))
        .await;
    Ok(())
}

/// A sequence tool follows the mode too: in sync mode its answer carries each
/// step's output, not just "started" lines.
#[tokio::test]
async fn a_sequence_waits_for_its_steps_in_sync_mode() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let tools = temp.path().join(".ahma");
    tokio::fs::create_dir_all(&tools).await?;
    // Neither tool says `synchronous`, so the server's mode decides.
    tokio::fs::write(
        tools.join("say.json"),
        r#"{
            "name": "say", "description": "echo", "command": "echo",
            "timeout_seconds": 30, "enabled": true,
            "subcommand": [{ "name": "default", "description": "echo",
                "positional_args": [{ "name": "message", "type": "string",
                    "description": "m", "required": false }] }]
        }"#,
    )
    .await?;
    tokio::fs::write(
        tools.join("both.json"),
        r#"{
            "name": "both", "description": "two steps", "command": "sequence",
            "timeout_seconds": 30, "enabled": true, "step_delay_ms": 10,
            "sequence": [
                { "tool": "say", "subcommand": "default", "description": "a",
                  "args": { "message": "first-step-out" } },
                { "tool": "say", "subcommand": "default", "description": "b",
                  "args": { "message": "second-step-out" } }
            ]
        }"#,
    )
    .await?;

    for (mode, expect_output) in [
        (ExecutionPolicy::Sync, true),
        (ExecutionPolicy::Async, false),
    ] {
        let mcp = create_in_process_mcp_with_scope(&tools, vec![temp.path().to_path_buf()]).await?;
        mcp.service.set_app_config(Arc::new(AppConfig {
            execution_mode: mode,
            request_budget_override_secs: Some(TestTimeouts::scale_secs(20).as_secs()),
            ..AppConfig::default()
        }));
        let params = CallToolRequestParams::new("both").with_arguments(
            json!({ "working_directory": temp.path().to_string_lossy() })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let text = result_text(&mcp.client.call_tool(params).await?);
        assert_eq!(
            text.contains("first-step-out") && text.contains("second-step-out"),
            expect_output,
            "{mode:?}: {text}"
        );
    }
    Ok(())
}

/// The model is told how calls behave in the mode it is actually getting: in
/// sync mode an operation id is the exception it must know how to handle, not
/// the workflow it should plan around.
#[tokio::test]
async fn the_server_instructions_describe_the_active_mode() -> Result<()> {
    use rmcp::ServerHandler;
    let (service, _t) = ahma_mcp::test_utils::in_process::build_test_service().await?;

    service.set_app_config(Arc::new(AppConfig {
        execution_mode: ExecutionPolicy::Sync,
        ..AppConfig::default()
    }));
    let sync = service.get_info().instructions.unwrap_or_default();
    assert!(
        sync.contains("Each call waits for the command to finish"),
        "{sync}"
    );
    assert!(
        !sync.contains("returns an operation_id immediately"),
        "{sync}"
    );

    service.set_app_config(Arc::new(AppConfig {
        execution_mode: ExecutionPolicy::Async,
        ..AppConfig::default()
    }));
    let asynchronous = service.get_info().instructions.unwrap_or_default();
    assert!(
        asynchronous.contains("returns an operation_id immediately"),
        "{asynchronous}"
    );
    Ok(())
}
