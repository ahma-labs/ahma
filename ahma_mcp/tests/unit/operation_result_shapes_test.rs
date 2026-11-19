//! The result an operation ends with, on every execution path and for every
//! way it can end, pinned by its shape: the fields of the result object (or
//! that it is a plain string). Agents, the hub and the TUI read these
//! payloads, so a change to how an operation is ended must not change one by
//! accident. The one deliberate change is named where it is made.

use ahma_mcp::adapter::{Adapter, AsyncExecOptions};
use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor, OperationStatus};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

fn adapter(scope: &std::path::Path) -> (Adapter, Arc<OperationMonitor>) {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(60),
    )));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![scope.to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    );
    (
        Adapter::new(monitor.clone(), shell_pool, sandbox).unwrap(),
        monitor,
    )
}

async fn ended(monitor: &OperationMonitor, id: &str) -> Operation {
    tokio::time::timeout(Duration::from_secs(30), monitor.wait_for_operation(id))
        .await
        .expect("the operation ends in time")
        .expect("the operation is in history")
}

/// The shape of a result: `"string"`, `"none"`, or its keys, sorted.
fn shape(result: &Option<Value>) -> String {
    match result {
        None => "none".to_string(),
        Some(Value::String(_)) => "string".to_string(),
        Some(Value::Object(map)) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.join(",")
        }
        Some(other) => format!("other:{other}"),
    }
}

const PIPED: &str = "exit_code,output_file,stderr,stderr_truncated_bytes,stderr_truncated_lines,stdout,stdout_truncated_bytes,stdout_truncated_lines";
const SESSION: &str =
    "exit_code,output_file,session_id,stderr,stdout,stdout_truncated_bytes,stdout_truncated_lines";
#[cfg(unix)]
const PTY: &str =
    "exit_code,output_file,pty,stderr,stdout,stdout_truncated_bytes,stdout_truncated_lines";

/// Start `command` the way `run_terminal_command` does: the platform shell,
/// with the shell tool's own subcommand configuration.
async fn piped(adapter: &Adapter, dir: &str, command: &str, timeout: Option<u64>) -> String {
    let mut args = serde_json::Map::new();
    args.insert("command".into(), Value::String(command.into()));
    args.insert("c_flag".into(), Value::Bool(true));
    let config = ahma_mcp::AhmaMcpService::build_shell_subcommand_config(
        timeout,
        &ahma_mcp::adapter::ExecutionMode::AsyncResultPush,
    );
    adapter
        .execute_async_in_dir_with_options(
            "shape_test",
            ahma_mcp::shell_pool::platform_shell_program(),
            dir,
            AsyncExecOptions {
                id: None,
                args: Some(args),
                timeout,
                subcommand_config: Some(&config),
                log_monitor_config: None,
            },
        )
        .await
        .expect("dispatch")
}

#[tokio::test]
async fn every_ending_on_every_path_keeps_its_result_shape() {
    let temp = tempdir().unwrap();
    let dir = temp.path().to_str().unwrap();
    let (adapter, monitor) = adapter(temp.path());

    // Piped: exit 0 and a failing exit.
    for (command, state) in [
        ("echo hi", OperationStatus::Completed),
        ("exit 3", OperationStatus::Failed),
    ] {
        let id = piped(&adapter, dir, command, None).await;
        let op = ended(&monitor, &id).await;
        assert_eq!(op.state, state, "{command}: {:?}", op.result);
        assert_eq!(shape(&op.result), PIPED, "{command}");
    }

    // Session: exit 0 and a failing exit.
    for (command, state) in [
        ("echo hi", OperationStatus::Completed),
        ("false", OperationStatus::Failed),
    ] {
        let id = adapter
            .execute_session_async("shape_test", "shapes", command, dir, Some(15), None)
            .await
            .expect("session command starts");
        let op = ended(&monitor, &id).await;
        assert_eq!(op.state, state, "session {command}: {:?}", op.result);
        assert_eq!(shape(&op.result), SESSION, "session {command}");
    }

    // PTY.
    #[cfg(unix)]
    if ahma_mcp::adapter::pty_available() {
        let id = adapter
            .execute_pty_async("shape_test", "echo hi", dir, Some(15), None)
            .await
            .expect("pty command starts");
        let op = ended(&monitor, &id).await;
        assert_eq!(op.state, OperationStatus::Completed, "{:?}", op.result);
        assert_eq!(shape(&op.result), PTY);
    }

    // Cancelled by a caller, through the monitor: a reason object.
    let id = piped(&adapter, dir, "sleep 30", None).await;
    assert!(
        monitor
            .cancel_operation_with_reason(&id, Some("asked to".into()))
            .await
    );
    let op = ended(&monitor, &id).await;
    assert_eq!(op.state, OperationStatus::Cancelled);
    assert_eq!(shape(&op.result), "cancelled,reason");

    // Timed out by the command's own limit, on the piped and the session path:
    // the same reason object as the monitor's deadline. (The deliberate
    // change: the executor stored a bare string here, which one reader of the
    // reason did not understand.)
    let id = piped(&adapter, dir, "sleep 30", Some(1)).await;
    let op = ended(&monitor, &id).await;
    assert_eq!(op.state, OperationStatus::TimedOut, "{:?}", op.result);
    assert_eq!(shape(&op.result), "reason,timed_out", "piped timeout");
    let id = adapter
        .execute_session_async("shape_test", "slow", "sleep 30", dir, Some(1), None)
        .await
        .expect("session command starts");
    let op = ended(&monitor, &id).await;
    assert_eq!(op.state, OperationStatus::TimedOut, "{:?}", op.result);
    assert_eq!(shape(&op.result), "reason,timed_out", "session timeout");

    // Timed out by the monitor's own deadline: a reason object.
    let lone = Operation::new_with_timeout(
        "shape_monitor_timeout".into(),
        "t".into(),
        "d".into(),
        None,
        Some(Duration::from_nanos(1)),
    );
    monitor.add_operation(lone).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while monitor
        .get_operation("shape_monitor_timeout")
        .await
        .is_some()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the monitor times it out"
        );
        monitor.check_timeouts().await;
        tokio::task::yield_now().await;
    }
    let op = ended(&monitor, "shape_monitor_timeout").await;
    assert_eq!(op.state, OperationStatus::TimedOut);
    assert_eq!(shape(&op.result), "reason,timed_out");
}
