//! Integration tests for the live log monitoring feature.
//!
//! Alerts and final results are observed through the `OperationMonitor`
//! store of record: log-monitor alerts are appended to `Operation::alerts`
//! (and emitted as `Alert` events on the unified stream), and the final
//! result is stored on the completed operation.

use ahma_mcp::adapter::{Adapter, AsyncExecOptions};
use ahma_mcp::log_monitor::{LogLevel, LogMonitorConfig, MonitorStream};
use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor};
use ahma_mcp::sandbox::Sandbox;
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use serde_json::Map;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

async fn create_test_adapter() -> (Adapter, Arc<OperationMonitor>) {
    let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(30));
    let monitor = Arc::new(OperationMonitor::new(monitor_config));
    let shell_pool_config = ShellPoolConfig::default();
    let shell_pool = Arc::new(ShellPoolManager::new(shell_pool_config));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![std::env::temp_dir()],
            ahma_mcp::sandbox::SandboxMode::Test,
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

fn monitor_config(level: LogLevel, stream: MonitorStream) -> Option<LogMonitorConfig> {
    Some(LogMonitorConfig {
        monitor_level: level,
        monitor_stream: stream,
        rate_limit_seconds: 0,
    })
}

/// Wait for the operation to reach a terminal state and return it (with its
/// alerts and final result).
async fn wait_for_completion(monitor: &OperationMonitor, op_id: &str) -> Operation {
    tokio::time::timeout(Duration::from_secs(15), monitor.wait_for_operation(op_id))
        .await
        .expect("Timed out waiting for operation to complete")
        .expect("Operation should have completed")
}

fn result_stdout_stderr(op: &Operation) -> String {
    let result = op.result.as_ref().expect("missing final result");
    format!(
        "{}\n{}",
        result.get("stdout").and_then(|v| v.as_str()).unwrap_or(""),
        result.get("stderr").and_then(|v| v.as_str()).unwrap_or(""),
    )
}

#[allow(unused_variables)]
fn write_cross_platform_script(
    temp_dir: &std::path::Path,
    name_base: &str,
    bash_content: &str,
    ps1_content: &str,
) -> String {
    #[cfg(windows)]
    {
        let name = format!("{}.ps1", name_base);
        let script_path = temp_dir.join(&name);
        std::fs::write(&script_path, ps1_content).unwrap();
        let path_str = script_path.to_string_lossy();
        format!(
            "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
            path_str
        )
    }
    #[cfg(not(windows))]
    {
        let name = format!("{}.sh", name_base);
        let script_path = temp_dir.join(&name);
        std::fs::write(&script_path, bash_content).unwrap();
        format!("bash {}", script_path.display())
    }
}

#[tokio::test]
async fn streaming_stderr_error_triggers_log_alert() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "emit_error",
        "#!/bin/bash\necho 'error[E0308]: mismatched types' >&2\n",
        "[Console]::Error.WriteLine('error[E0308]: mismatched types')\n",
    );
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_stderr_error",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_1".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Stderr),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_1").await;
    assert!(!op.alerts.is_empty(), "Expected alert, got: {:?}", op);
    assert!(
        op.alerts[0].contains("LOG ALERT (error"),
        "alert: {}",
        op.alerts[0]
    );
    assert!(
        op.alerts[0].contains("mismatched types"),
        "alert: {}",
        op.alerts[0]
    );
}

#[tokio::test]
async fn streaming_stdout_error_triggers_when_monitoring_both() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "emit_stdout_error",
        "#!/bin/bash\necho 'ERROR: something failed'\n",
        "Write-Output 'ERROR: something failed'\n",
    );
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_stdout_error",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_2".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Both),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_2").await;
    assert!(!op.alerts.is_empty(), "Expected alert: {:?}", op.alerts);
}

#[tokio::test]
async fn streaming_no_alert_when_output_is_clean() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "clean",
        "#!/bin/bash\necho 'hello world'\n",
        "Write-Output 'hello world'\n",
    );
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_clean",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_3".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Both),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_3").await;
    assert!(
        op.alerts.is_empty(),
        "Clean output should not trigger: {:?}",
        op.alerts
    );
}

#[tokio::test]
async fn streaming_warn_level_triggers_on_warning() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "emit_warn",
        "#!/bin/bash\necho 'warning: unused variable' >&2\n",
        "[Console]::Error.WriteLine('warning: unused variable')\n",
    );
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_warn",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_4".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Warn, MonitorStream::Stderr),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_4").await;
    assert!(
        !op.alerts.is_empty(),
        "Expected alert for warning: {:?}",
        op.alerts
    );
    assert!(
        op.alerts[0].contains("LOG ALERT (warn"),
        "alert: {}",
        op.alerts[0]
    );
}

#[tokio::test]
async fn streaming_error_level_ignores_warnings() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "warn_only",
        "#!/bin/bash\necho 'warning: unused variable' >&2\n",
        "[Console]::Error.WriteLine('warning: unused variable')\n",
    );
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_warn_at_error",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_5".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Stderr),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_5").await;
    assert!(
        op.alerts.is_empty(),
        "Warning should not trigger at Error level: {:?}",
        op.alerts
    );
}

#[tokio::test]
async fn streaming_without_monitor_produces_no_alerts() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "batch",
        "#!/bin/bash\necho 'error: something' >&2\necho done\n",
        "[Console]::Error.WriteLine('error: something')\nWrite-Output 'done'\n",
    );
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_batch",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_6".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: None,
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_6").await;
    assert!(
        op.alerts.is_empty(),
        "No-monitor run should not produce alerts: {:?}",
        op.alerts
    );
    assert!(op.result.is_some(), "Final result must be stored");
}

#[tokio::test]
async fn streaming_alert_includes_context_lines() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let bash_script = "#!/bin/bash\nfor i in $(seq 1 5); do echo \"info: compiling module $i\" >&2; done\necho 'error[E0277]: the trait bound is not satisfied' >&2\n";
    let ps1_script = "1..5 | ForEach-Object { [Console]::Error.WriteLine(\"info: compiling module $_\") }\n[Console]::Error.WriteLine('error[E0277]: the trait bound is not satisfied')\n";
    let cmd = write_cross_platform_script(temp_dir.path(), "context", bash_script, ps1_script);
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_context",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_7".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Stderr),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_7").await;
    assert!(!op.alerts.is_empty(), "Expected alert: {:?}", op.alerts);
    let alert = &op.alerts[0];
    assert!(
        alert.contains("compiling module"),
        "Missing context: {}",
        alert
    );
    assert!(alert.contains("E0277"), "Missing trigger: {}", alert);
}

#[tokio::test]
async fn streaming_multiline_errors_with_rate_limit() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let bash_script = "#!/bin/bash\necho 'error[E0308]: mismatched types' >&2\necho 'error[E0277]: trait bound' >&2\necho 'error[E0599]: no method' >&2\n";
    let ps1_script = "[Console]::Error.WriteLine('error[E0308]: mismatched types')\n[Console]::Error.WriteLine('error[E0277]: trait bound')\n[Console]::Error.WriteLine('error[E0599]: no method')\n";
    let cmd = write_cross_platform_script(temp_dir.path(), "multi_error", bash_script, ps1_script);
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_rate_limit",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_8".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: Some(LogMonitorConfig {
                    monitor_level: LogLevel::Error,
                    monitor_stream: MonitorStream::Stderr,
                    rate_limit_seconds: 60,
                }),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_8").await;
    assert_eq!(
        op.alerts.len(),
        1,
        "Rate limit should suppress: {:?}",
        op.alerts
    );
}

#[tokio::test]
async fn streaming_stderr_only_ignores_stdout_patterns() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "stdout_error",
        "#!/bin/bash\necho 'error[E0308]: type mismatch'\n",
        "Write-Output 'error[E0308]: type mismatch'\n",
    );
    let result = adapter
        .execute_async_in_dir_with_options(
            "test_stderr_filter",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_9".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Stderr),
            },
        )
        .await;
    assert!(result.is_ok());
    let op = wait_for_completion(&monitor, "test_op_9").await;
    assert!(
        op.alerts.is_empty(),
        "Stderr-only should ignore stdout: {:?}",
        op.alerts
    );
}

#[tokio::test]
async fn streaming_final_result_redacts_sensitive_output() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "secret_output",
        "#!/bin/bash\necho 'token=supersecret123'\necho 'Authorization: Bearer abcdefghijklmnop' >&2\n",
        "Write-Output 'token=supersecret123'\n[Console]::Error.WriteLine('Authorization: Bearer abcdefghijklmnop')\n",
    );

    let result = adapter
        .execute_async_in_dir_with_options(
            "test_redaction",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_10".to_string()),
                args: Some(Map::new()),
                timeout: Some(10),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Both),
            },
        )
        .await;
    assert!(result.is_ok());

    let op = wait_for_completion(&monitor, "test_op_10").await;
    let output = result_stdout_stderr(&op);
    assert!(!output.contains("supersecret123"), "output: {}", output);
    assert!(!output.contains("abcdefghijklmnop"), "output: {}", output);
    assert!(output.contains("[REDACTED]"), "output: {}", output);
}

#[tokio::test]
async fn streaming_final_result_is_bounded_and_marks_truncation() {
    let (adapter, monitor) = create_test_adapter().await;
    let temp_dir = tempdir().unwrap();
    let working_dir = temp_dir.path().to_str().unwrap();
    let cmd = write_cross_platform_script(
        temp_dir.path(),
        "many_lines",
        "#!/bin/bash\nfor i in $(seq 1 7000); do echo \"line-$i\"; done\n",
        "1..7000 | ForEach-Object { Write-Output \"line-$_\" }\n",
    );

    let result = adapter
        .execute_async_in_dir_with_options(
            "test_truncation",
            &cmd,
            working_dir,
            AsyncExecOptions {
                id: Some("test_op_11".to_string()),
                args: Some(Map::new()),
                timeout: Some(20),
                subcommand_config: None,
                log_monitor_config: monitor_config(LogLevel::Error, MonitorStream::Both),
            },
        )
        .await;
    assert!(result.is_ok());

    let op = wait_for_completion(&monitor, "test_op_11").await;
    let output = result_stdout_stderr(&op);
    assert!(
        output.contains("[output truncated: dropped"),
        "output: {}",
        output
    );
    assert!(
        !output.contains("line-1\n"),
        "oldest lines should be dropped"
    );
    assert!(
        output.contains("line-7000"),
        "latest line should be retained"
    );
}
