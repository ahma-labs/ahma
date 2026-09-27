//! Safe async by default (SPEC R2.7), asserted over the MCP wire.
//!
//! Async mode lets the model think while a long command runs. What makes it
//! safe is the workspace write queue: commands that may write the workspace run
//! one at a time, in the order they were sent, and everything the model is
//! told about that is in the tool results themselves:
//!
//! * a command still waiting for its turn says `NOT started — queued behind`
//!   the operation ahead of it (R2.7.3), so a queued edit is never mistaken for
//!   an applied one;
//! * it runs after its predecessor, never alongside it (R2.7.1);
//! * a result the model never collected is delivered at the top of its next
//!   tool result (R2.7.5);
//! * ahma's own file edits are refused while a writer runs (R2.7.8).

use ahma_common::timeouts::TestTimeouts;
use ahma_mcp::shell::cli::AppConfig;
use ahma_mcp::test_utils::in_process::{InProcessMcp, create_in_process_mcp_with_workspace_queue};
use ahma_mcp::utils::logging::init_test_logging;
use anyhow::Result;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use std::time::Duration;

fn budget() -> Duration {
    TestTimeouts::scale_secs(60)
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sleep_cmd(secs: u64) -> String {
    if cfg!(windows) {
        format!("Start-Sleep -Seconds {secs}")
    } else {
        format!("sleep {secs}")
    }
}

fn echo_cmd(word: &str) -> String {
    if cfg!(windows) {
        format!("Write-Output {word}")
    } else {
        format!("echo {word}")
    }
}

/// The operation id in an `AHMA ID: <id>` answer.
fn op_id(text: &str) -> String {
    text.split("AHMA ID: ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no operation id in: {text}"))
        .to_string()
}

/// A queue-enabled server in async mode whose inline window is one second
/// (half of a two-second request budget), so a three-second command reliably
/// comes back as an operation id.
async fn server() -> Result<(InProcessMcp, tempfile::TempDir)> {
    let temp = tempfile::tempdir()?;
    let scope = temp.path().join("ws");
    tokio::fs::create_dir_all(&scope).await?;
    let config = AppConfig {
        execution_mode: ahma_common::config::ExecutionPolicy::Async,
        request_budget_override_secs: Some(2),
        ..AppConfig::default()
    };
    let mcp = create_in_process_mcp_with_workspace_queue(&scope, temp.path().join("locks"), config)
        .await?;
    Ok((mcp, temp))
}

async fn call(mcp: &InProcessMcp, tool: &str, args: Value) -> Result<CallToolResult> {
    let params = CallToolRequestParams::new(tool.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    Ok(tokio::time::timeout(budget(), mcp.client.call_tool(params))
        .await
        .unwrap_or_else(|_| panic!("{tool} must answer"))?)
}

async fn run(mcp: &InProcessMcp, command: &str, wd: &std::path::Path) -> Result<String> {
    let r = call(
        mcp,
        "run_terminal_command",
        json!({ "command": command, "working_directory": wd.to_string_lossy() }),
    )
    .await?;
    Ok(result_text(&r))
}

async fn cancel_all(mcp: &InProcessMcp) {
    let _ = call(mcp, "cancel", json!({ "all": true })).await;
}

/// R2.7.1 + R2.7.3: the second writer does not start while the first runs, says
/// so, and then runs after it.
#[tokio::test]
async fn a_second_writer_is_queued_behind_the_first_and_says_so() -> Result<()> {
    init_test_logging();
    let (mcp, temp) = server().await?;
    let wd = temp.path().join("ws");

    let first = run(&mcp, &sleep_cmd(3), &wd).await?;
    let first_id = op_id(&first);

    let second = run(&mcp, &echo_cmd("second-ran"), &wd).await?;
    assert!(
        second.contains("NOT started"),
        "a command waiting for the workspace must say it has not started, got: {second}"
    );
    assert!(
        second.contains(&first_id),
        "it must name the operation it waits for ({first_id}), got: {second}"
    );
    assert!(
        second.contains("Do not send it again"),
        "the model must be told not to resend it, got: {second}"
    );
    let second_id = op_id(&second);

    let awaited = call(
        &mcp,
        "await",
        json!({ "id": second_id, "timeout_seconds": 60 }),
    )
    .await?;
    let awaited = result_text(&awaited);
    assert!(awaited.contains("second-ran"), "got: {awaited}");

    let monitor = &mcp.service.operation_monitor;
    let op1 = monitor
        .check_completion_history_pub(&first_id)
        .await
        .expect("first finished");
    let op2 = monitor
        .check_completion_history_pub(&second_id)
        .await
        .expect("second finished");
    let first_end = op1.end_time.expect("first has an end time");
    assert!(
        op2.start_time >= first_end,
        "the second writer must start only after the first ended \
         (second started {:?}, first ended {first_end:?})",
        op2.start_time
    );
    assert!(
        op2.queue_wait_ms.is_some(),
        "the wait for the workspace is recorded"
    );

    cancel_all(&mcp).await;
    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.7.5: a result the model never `await`ed arrives with its next call — once.
#[tokio::test]
async fn an_uncollected_result_is_delivered_with_the_next_call() -> Result<()> {
    init_test_logging();
    let (mcp, temp) = server().await?;
    let wd = temp.path().join("ws");

    let marker = "piggybacked-result";
    let command = if cfg!(windows) {
        format!("Start-Sleep -Seconds 2; Write-Output {marker}")
    } else {
        format!("sleep 2; echo {marker}")
    };
    let started = run(&mcp, &command, &wd).await?;
    let id = op_id(&started);
    // Let it finish without awaiting it through the tool.
    mcp.service.operation_monitor.wait_for_operation(&id).await;

    let next = result_text(&call(&mcp, "status", json!({})).await?);
    assert!(
        next.contains("Finished since your last call"),
        "the next result must carry the finished operation, got: {next}"
    );
    assert!(next.contains(marker), "with its output, got: {next}");

    let after = result_text(&call(&mcp, "status", json!({})).await?);
    assert!(
        !after.contains("Finished since your last call"),
        "a result is delivered once, got: {after}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.7.5: what `await` returned is delivered; it is not repeated afterwards.
#[tokio::test]
async fn an_awaited_result_is_not_delivered_again() -> Result<()> {
    init_test_logging();
    let (mcp, temp) = server().await?;
    let wd = temp.path().join("ws");

    let id = op_id(&run(&mcp, &sleep_cmd(2), &wd).await?);
    let _ = call(&mcp, "await", json!({ "id": id, "timeout_seconds": 60 })).await?;
    let next = result_text(&call(&mcp, "status", json!({})).await?);
    assert!(
        !next.contains("Finished since your last call"),
        "an awaited result must not be piggybacked, got: {next}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.7.8: ahma's own file edit is refused while a writer holds the workspace,
/// and allowed again once it is done.
#[tokio::test]
async fn an_edit_is_refused_while_a_writer_runs() -> Result<()> {
    init_test_logging();
    let (mcp, temp) = server().await?;
    let wd = temp.path().join("ws");
    let file = wd.join("new_file.txt");

    let id = op_id(&run(&mcp, &sleep_cmd(3), &wd).await?);

    let refused = call(
        &mcp,
        "write_file",
        json!({ "path": file.to_string_lossy(), "content": "x" }),
    )
    .await;
    let message = match refused {
        Ok(r) => result_text(&r),
        Err(e) => e.to_string(),
    };
    assert!(
        message.contains("Not edited"),
        "an edit during a running writer must be refused, got: {message}"
    );
    assert!(
        message.contains(&id),
        "naming the writer ({id}), got: {message}"
    );
    assert!(!file.exists(), "and nothing is written");

    let _ = call(&mcp, "await", json!({ "id": id, "timeout_seconds": 60 })).await?;
    let written = call(
        &mcp,
        "write_file",
        json!({ "path": file.to_string_lossy(), "content": "x" }),
    )
    .await?;
    assert!(
        result_text(&written).contains("File written"),
        "the same edit succeeds once the writer is done"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.7.6: a file another writer changes while a command runs is named in
/// that command's result.
#[tokio::test]
async fn a_file_changed_during_a_run_is_reported() -> Result<()> {
    init_test_logging();
    let (mcp, temp) = server().await?;
    let wd = temp.path().join("ws");

    let id = op_id(&run(&mcp, &sleep_cmd(3), &wd).await?);
    // Another writer — the harness's own editor, say — changes a source file.
    tokio::fs::write(wd.join("edited_meanwhile.rs"), "fn main() {}").await?;

    let awaited =
        result_text(&call(&mcp, "await", json!({ "id": id, "timeout_seconds": 60 })).await?);
    assert!(
        awaited.contains("changed_during_run") && awaited.contains("edited_meanwhile.rs"),
        "the result must name the file changed during the run, got: {awaited}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}

/// R2.7.5: an `await` that ends on its soft timeout delivered nothing, so the
/// result still arrives with a later call.
#[tokio::test]
async fn a_timed_out_await_does_not_count_as_delivery() -> Result<()> {
    init_test_logging();
    let (mcp, temp) = server().await?;
    let wd = temp.path().join("ws");

    let id = op_id(&run(&mcp, &sleep_cmd(3), &wd).await?);
    let early = result_text(&call(&mcp, "await", json!({ "id": id, "timeout_seconds": 1 })).await?);
    assert!(
        early.contains("still running"),
        "a soft timeout, got: {early}"
    );
    mcp.service.operation_monitor.wait_for_operation(&id).await;

    let next = result_text(&call(&mcp, "status", json!({})).await?);
    assert!(
        next.contains("Finished since your last call") && next.contains(&id),
        "the result must still be delivered, got: {next}"
    );

    let _ = mcp.client.cancel().await;
    Ok(())
}
