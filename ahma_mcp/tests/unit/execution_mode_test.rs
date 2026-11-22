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
    assert!(!sync.contains("NOT started"), "{sync}");

    service.set_app_config(Arc::new(AppConfig {
        execution_mode: ExecutionPolicy::Async,
        ..AppConfig::default()
    }));
    let asynchronous = service.get_info().instructions.unwrap_or_default();
    assert!(
        asynchronous.contains("returns an operation_id"),
        "{asynchronous}"
    );
    // Async is only safe because writers are ordered; the model must know how
    // a queued command reads, and that it must not resend it (SPEC R2.7.3).
    assert!(asynchronous.contains("NOT started"), "{asynchronous}");
    assert!(
        asynchronous.contains("do not send it again"),
        "{asynchronous}"
    );
    Ok(())
}

// ==================== the sync flags are a waiting policy ====================
//
// MTDF `synchronous: true`, the `blocking: true` argument and
// `run_terminal_command`'s unadvertised `execution_mode: "Synchronous"` ask
// for one call to wait as sync mode waits (SPEC R2.1, R2.3). They are not a
// second execution path: the call is a tracked operation like every other,
// bounded by the same client budget.

/// An MTDF tool that runs one line in the platform shell, the way
/// `run_terminal_command` does, so a test can make it slow on every platform.
async fn write_shell_tool(dir: &std::path::Path, name: &str, synchronous: Option<bool>) {
    let mut tool = json!({
        "name": name,
        "description": "Run a shell line",
        "command": ahma_mcp::shell_pool::platform_shell_program(),
        "timeout_seconds": 600,
        "enabled": true,
        "subcommand": [{
            "name": "default",
            "description": "Run a shell line",
            "options": [{
                "name": "c_flag", "type": "boolean", "description": "run a line",
                "alias": "c"
            }],
            "positional_args": [{
                "name": "command", "type": "string", "description": "the line",
                "required": true
            }]
        }]
    });
    if let Some(sync) = synchronous {
        tool["synchronous"] = json!(sync);
    }
    tokio::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(&tool).expect("tool json"),
    )
    .await
    .expect("write tool config");
}

/// An async-mode server with a `synchronous: true` tool (`waits`) and an
/// unflagged one (`plain`).
async fn flagged_server() -> Result<(InProcessMcp, tempfile::TempDir)> {
    let temp = tempfile::tempdir()?;
    let tools = temp.path().join(".ahma");
    tokio::fs::create_dir_all(&tools).await?;
    write_shell_tool(&tools, "waits", Some(true)).await;
    write_shell_tool(&tools, "plain", None).await;
    let mcp = create_in_process_mcp_with_scope(&tools, vec![temp.path().to_path_buf()]).await?;
    mcp.service.set_app_config(Arc::new(AppConfig {
        execution_mode: ExecutionPolicy::Async,
        request_budget_override_secs: Some(budget_secs()),
        ..AppConfig::default()
    }));
    Ok((mcp, temp))
}

/// The three ways a call asks to wait, each as `(tool, arguments)` for `line`.
fn sync_flagged_calls(line: &str) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("waits", json!({ "c_flag": true, "command": line })),
        (
            "plain",
            json!({ "c_flag": true, "command": line, "blocking": true }),
        ),
        (
            "run_terminal_command",
            json!({ "command": line, "execution_mode": "Synchronous" }),
        ),
    ]
}

async fn call(
    mcp: &InProcessMcp,
    tool: &'static str,
    args: &serde_json::Value,
) -> Result<CallToolResult> {
    let params = CallToolRequestParams::new(tool)
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    let limit = TestTimeouts::scale_secs(60);
    Ok(tokio::time::timeout(limit, mcp.client.call_tool(params))
        .await
        .unwrap_or_else(|_| panic!("{tool} must answer within {limit:?}"))?)
}

async fn status_of(mcp: &InProcessMcp, tool: &str) -> Result<String> {
    let params = CallToolRequestParams::new("status").with_arguments(
        json!({ "tools": tool })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    Ok(result_text(&mcp.client.call_tool(params).await?))
}

/// A call that asks to wait is still a tracked operation: it is in `status`
/// (and so in the TUI and the audit log), like any other.
#[tokio::test]
async fn a_sync_flagged_call_is_a_tracked_operation() -> Result<()> {
    for (tool, args) in sync_flagged_calls("echo flagged-and-tracked") {
        let (mcp, _t) = flagged_server().await?;
        let text = result_text(&call(&mcp, tool, &args).await?);
        assert!(text.contains("flagged-and-tracked"), "{tool}: {text}");

        let status = status_of(&mcp, tool).await?;
        assert!(
            !status.contains("(total: 0)") && status.contains("flagged-and-tracked"),
            "{tool}: the call must be a monitored operation: {status}"
        );
    }
    Ok(())
}

/// Asking to wait still waits: past the adaptive window, within the budget,
/// the call answers with the output rather than an id.
#[tokio::test]
async fn a_sync_flagged_call_waits_past_the_adaptive_window() -> Result<()> {
    let secs = budget_secs() * 3 / 4;
    for (tool, args) in sync_flagged_calls(&sleep_then_done(secs)) {
        let (mcp, _t) = flagged_server().await?;
        let text = result_text(&call(&mcp, tool, &args).await?);
        assert!(text.contains("done"), "{tool} waits for the output: {text}");
        assert!(!text.contains("AHMA ID"), "{tool} is not an id: {text}");
    }
    Ok(())
}

/// A call that asks to wait for a command longer than the client can hold a
/// request open gets the operation id at the budget, as sync mode does — it
/// no longer holds the request for the whole command.
#[tokio::test]
async fn a_sync_flagged_call_hands_back_an_id_when_the_client_cannot_wait_longer() -> Result<()> {
    for (tool, args) in sync_flagged_calls(&sleep_then_done(budget_secs() * 10)) {
        let (mcp, _t) = flagged_server().await?;
        let started = std::time::Instant::now();
        let text = result_text(&call(&mcp, tool, &args).await?);
        assert!(text.contains("AHMA ID"), "{tool}: {text}");
        assert!(
            text.contains("Still running") && text.contains("call `await`"),
            "{tool}: says why it returned and how to collect: {text}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(budget_secs() * 5),
            "{tool}: bounded by the budget, not the command"
        );
        let _ = mcp
            .client
            .call_tool(CallToolRequestParams::new("cancel"))
            .await;
    }
    Ok(())
}

/// Cancelling the MCP request that asked to wait still stops the command, as
/// it did on the direct path, but now as the operation's own cancellation:
/// recorded, with the reason, where `status` and `await` can see it.
#[tokio::test]
async fn cancelling_a_sync_flagged_request_cancels_its_operation() -> Result<()> {
    use rmcp::model::{CallToolRequest, ClientRequest};
    use rmcp::service::PeerRequestOptions;

    let secs = TestTimeouts::scale_secs(30).as_secs();
    let (mcp, _t) = flagged_server().await?;
    let params = CallToolRequestParams::new("waits").with_arguments(
        json!({ "c_flag": true, "command": sleep_then_done(secs) })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let handle = mcp
        .client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        )
        .await?;

    // Cancel once the command is running: it must be an operation by then.
    let running = ahma_mcp::test_utils::concurrency::wait_for_condition(
        TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::ToolCall),
        TestTimeouts::poll_interval(),
        || async {
            status_of(&mcp, "waits")
                .await
                .is_ok_and(|s| s.contains("1 active"))
        },
    )
    .await;
    assert!(running, "the waiting call must show as an active operation");
    handle.cancel(Some("test gives up".into())).await?;

    let awaited = call(&mcp, "await", &json!({ "tools": "waits" })).await?;
    let text = result_text(&awaited);
    assert!(
        text.contains("\"state\": \"Cancelled\"") && text.contains("test gives up"),
        "the operation records its cancellation and why: {text}"
    );
    Ok(())
}

/// What a sync-flagged call answers when it finishes in time: the direct
/// path's output, unchanged, under the identity line every inline result
/// leads with (SPEC R2.6.2).
#[tokio::test]
async fn a_sync_flagged_result_is_the_direct_output_under_the_identity_line() -> Result<()> {
    let line = "echo first-line; echo second-line";
    let (mcp, temp) = flagged_server().await?;
    let wd = temp.path().to_string_lossy().into_owned();
    // The direct path, as `run_terminal_command` configures it.
    let shell = ahma_mcp::AhmaMcpService::build_shell_subcommand_config(
        None,
        &ahma_mcp::adapter::ExecutionMode::Synchronous,
    );
    let direct = mcp
        .service
        .adapter
        .execute_sync_in_dir(
            ahma_mcp::shell_pool::platform_shell_program(),
            json!({ "c_flag": true, "command": line })
                .as_object()
                .cloned(),
            &wd,
            None,
            Some(&shell),
        )
        .await?;

    for (tool, mut args) in sync_flagged_calls(line) {
        args["working_directory"] = json!(wd);
        let text = result_text(&call(&mcp, tool, &args).await?);
        let (identity, body) = text
            .split_once('\n')
            .unwrap_or_else(|| panic!("{tool}: an identity line, then the output: {text}"));
        assert!(
            identity.starts_with('\u{2713}') && identity.contains("exit 0"),
            "{tool}: the first line says how it ended: {identity}"
        );
        assert_eq!(body, direct.trim_end(), "{tool}: the output is unchanged");
    }
    Ok(())
}

/// A sequence that asks to wait runs its steps as tracked operations, and a
/// failed step still stops the steps after it.
#[tokio::test]
async fn a_synchronous_sequence_tracks_its_steps_and_stops_at_a_failure() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let tools = temp.path().join(".ahma");
    tokio::fs::create_dir_all(&tools).await?;
    write_shell_tool(&tools, "step", None).await;
    let fail = "exit 3";
    tokio::fs::write(
        tools.join("chain.json"),
        serde_json::to_string_pretty(&json!({
            "name": "chain", "description": "three steps", "command": "sequence",
            "timeout_seconds": 60, "enabled": true, "synchronous": true,
            "step_delay_ms": 10,
            "sequence": [
                { "tool": "step", "subcommand": "default", "description": "a",
                  "args": { "c_flag": true, "command": "echo chain-step-one" } },
                { "tool": "step", "subcommand": "default", "description": "b",
                  "args": { "c_flag": true, "command": fail } },
                { "tool": "step", "subcommand": "default", "description": "c",
                  "args": { "c_flag": true, "command": "echo chain-step-three" } }
            ]
        }))?,
    )
    .await?;
    let mcp = create_in_process_mcp_with_scope(&tools, vec![temp.path().to_path_buf()]).await?;
    mcp.service.set_app_config(Arc::new(AppConfig {
        execution_mode: ExecutionPolicy::Async,
        request_budget_override_secs: Some(TestTimeouts::scale_secs(20).as_secs()),
        ..AppConfig::default()
    }));

    let args = json!({ "working_directory": temp.path().to_string_lossy() });
    let result = call(&mcp, "chain", &args).await?;
    let text = result_text(&result);
    assert!(text.contains("chain-step-one"), "step one ran: {text}");
    assert!(
        !text.contains("chain-step-three"),
        "a failed step stops the ones after it: {text}"
    );
    assert_eq!(result.is_error, Some(true), "the sequence failed: {text}");

    let status = status_of(&mcp, "step").await?;
    assert!(
        status.contains("chain-step-one") && !status.contains("chain-step-three"),
        "the steps that ran are operations, the one after the failure never started: {status}"
    );
    Ok(())
}
