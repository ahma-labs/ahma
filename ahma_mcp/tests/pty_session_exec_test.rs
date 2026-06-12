//! Integration tests for PTY execution (`pty: true`) and persistent shell
//! sessions (`session_id`) through the adapter's standard operation
//! lifecycle (operation ids, streamed tails, spill files, terminal events).

use ahma_mcp::adapter::Adapter;
use ahma_mcp::operation_monitor::{MonitorConfig, Operation, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

fn build_adapter(scope: std::path::PathBuf) -> (Arc<Adapter>, Arc<OperationMonitor>) {
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(60),
    )));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox =
        Arc::new(Sandbox::new(vec![scope], SandboxMode::Test, false, false, false).unwrap());
    (
        Arc::new(Adapter::new(monitor.clone(), shell_pool, sandbox).unwrap()),
        monitor,
    )
}

async fn wait_done(monitor: &OperationMonitor, id: &str) -> Operation {
    tokio::time::timeout(Duration::from_secs(15), monitor.wait_for_operation(id))
        .await
        .expect("operation should finish in time")
        .expect("operation should be in history")
}

fn stdout_of(op: &Operation) -> String {
    op.result
        .as_ref()
        .and_then(|r| r.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn exit_code_of(op: &Operation) -> i64 {
    op.result
        .as_ref()
        .and_then(|r| r.get("exit_code"))
        .and_then(|v| v.as_i64())
        .unwrap_or(i64::MIN)
}

// ─── PTY ─────────────────────────────────────────────────────────────────────

/// The defining property of the PTY path: the child sees a real TTY on stdout.
#[cfg(unix)]
#[tokio::test]
async fn pty_command_sees_a_real_tty() {
    if !ahma_mcp::adapter::pty_available() {
        eprintln!("SKIP: PTY allocation unavailable in this environment (outer sandbox)");
        return;
    }
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    let id = adapter
        .execute_pty_async(
            "pty_test",
            "if [ -t 1 ]; then echo IS_TTY; else echo NOT_TTY; fi",
            temp.path().to_str().unwrap(),
            Some(15),
            None,
        )
        .await
        .expect("pty operation should start");

    let op = wait_done(&monitor, &id).await;
    let stdout = stdout_of(&op);
    assert!(
        stdout.contains("IS_TTY"),
        "command must see a TTY under pty execution, got stdout {stdout:?}, full result: {:?}",
        op.result
    );
    assert_eq!(exit_code_of(&op), 0);
    assert_eq!(
        op.result.as_ref().unwrap().get("pty"),
        Some(&serde_json::json!(true))
    );
}

/// Exit codes propagate and failures map to a Failed terminal state.
#[cfg(unix)]
#[tokio::test]
async fn pty_nonzero_exit_code_fails_operation() {
    use ahma_mcp::operation_monitor::OperationStatus;

    if !ahma_mcp::adapter::pty_available() {
        eprintln!("SKIP: PTY allocation unavailable in this environment (outer sandbox)");
        return;
    }
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    let id = adapter
        .execute_pty_async(
            "pty_fail",
            "echo before-failure; exit 3",
            temp.path().to_str().unwrap(),
            Some(15),
            None,
        )
        .await
        .expect("pty operation should start");

    let op = wait_done(&monitor, &id).await;
    assert_eq!(op.state, OperationStatus::Failed);
    assert_eq!(exit_code_of(&op), 3);
    assert!(stdout_of(&op).contains("before-failure"));
}

/// PTY output is recorded in the spill file like any other operation.
#[cfg(unix)]
#[tokio::test]
async fn pty_output_reaches_tail_and_spill() {
    if !ahma_mcp::adapter::pty_available() {
        eprintln!("SKIP: PTY allocation unavailable in this environment (outer sandbox)");
        return;
    }
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    let id = adapter
        .execute_pty_async(
            "pty_spill",
            "echo pty-spill-marker",
            temp.path().to_str().unwrap(),
            Some(15),
            None,
        )
        .await
        .unwrap();

    let op = wait_done(&monitor, &id).await;
    assert!(
        op.stdout_tail
            .iter()
            .any(|l| l.contains("pty-spill-marker")),
        "tail: {:?}",
        op.stdout_tail
    );
    let output_file = op
        .result
        .as_ref()
        .and_then(|r| r.get("output_file"))
        .and_then(|v| v.as_str())
        .expect("output_file must be advertised")
        .to_string();
    let content = tokio::fs::read_to_string(&output_file)
        .await
        .expect("spill file should exist");
    assert!(content.contains("pty-spill-marker"), "spill: {content}");
}

// ─── Sessions ────────────────────────────────────────────────────────────────

/// `cd` and exported variables persist across commands in one session and do
/// not leak into other sessions.
#[cfg(unix)]
#[tokio::test]
async fn session_state_persists_and_is_isolated() {
    let temp = tempdir().unwrap();
    std::fs::create_dir(temp.path().join("inner")).unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());
    let wd = temp.path().to_str().unwrap();

    let id = adapter
        .execute_session_async(
            "sess",
            "alpha",
            "cd inner && export AHMA_SESSION_PROBE=42",
            wd,
            Some(15),
            None,
        )
        .await
        .unwrap();
    let op = wait_done(&monitor, &id).await;
    assert_eq!(exit_code_of(&op), 0);

    let id = adapter
        .execute_session_async(
            "sess",
            "alpha",
            "echo \"cwd=$(basename \"$PWD\") probe=$AHMA_SESSION_PROBE\"",
            wd,
            Some(15),
            None,
        )
        .await
        .unwrap();
    let op = wait_done(&monitor, &id).await;
    let stdout = stdout_of(&op);
    assert!(
        stdout.contains("cwd=inner probe=42"),
        "session state must persist, got: {stdout:?}"
    );

    // Fresh session: no leaked state.
    let id = adapter
        .execute_session_async(
            "sess",
            "beta",
            "echo \"probe=$AHMA_SESSION_PROBE\"",
            wd,
            Some(15),
            None,
        )
        .await
        .unwrap();
    let op = wait_done(&monitor, &id).await;
    assert!(
        stdout_of(&op).contains("probe="),
        "other sessions must not see alpha's environment"
    );
    assert!(!stdout_of(&op).contains("probe=42"));
}

/// Session command failures map to Failed with the real exit code, and the
/// session remains usable afterwards.
#[cfg(unix)]
#[tokio::test]
async fn session_failure_keeps_session_usable() {
    use ahma_mcp::operation_monitor::OperationStatus;

    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());
    let wd = temp.path().to_str().unwrap();

    let id = adapter
        .execute_session_async("sess", "robust", "false", wd, Some(15), None)
        .await
        .unwrap();
    let op = wait_done(&monitor, &id).await;
    assert_eq!(op.state, OperationStatus::Failed);
    assert_eq!(exit_code_of(&op), 1);

    let id = adapter
        .execute_session_async("sess", "robust", "echo still-alive", wd, Some(15), None)
        .await
        .unwrap();
    let op = wait_done(&monitor, &id).await;
    assert_eq!(op.state, OperationStatus::Completed);
    assert!(stdout_of(&op).contains("still-alive"));
}

/// Works on all platforms: a simple echo through a session completes and the
/// result carries the session id.
#[tokio::test]
async fn session_echo_round_trip() {
    let temp = tempdir().unwrap();
    let (adapter, monitor) = build_adapter(temp.path().to_path_buf());

    let id = adapter
        .execute_session_async(
            "sess",
            "plain",
            "echo session-round-trip",
            temp.path().to_str().unwrap(),
            Some(15),
            None,
        )
        .await
        .unwrap();
    let op = wait_done(&monitor, &id).await;
    assert!(stdout_of(&op).contains("session-round-trip"));
    assert_eq!(
        op.result.as_ref().unwrap().get("session_id"),
        Some(&serde_json::json!("plain"))
    );
}
