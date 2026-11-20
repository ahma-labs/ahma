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
        Arc::new(Sandbox::new(vec![scope], SandboxMode::Test, Default::default()).unwrap());
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

#[cfg_attr(not(unix), allow(dead_code))]
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
    let tail = op.stdout_tail.snapshot();
    assert!(
        tail.iter().any(|l| l.contains("pty-spill-marker")),
        "tail: {tail:?}"
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

// ─── The workspace queue (SPEC R2.7.1) ───────────────────────────────────────

/// An adapter whose workspace queue is on, with the trust-handoff watch on
/// too: the watch's closing inventory is real I/O between the command's end
/// and its report, the gap in which a report published before the release
/// shows the workspace still held.
fn queued_adapter(
    scope: &std::path::Path,
) -> (Arc<Adapter>, Arc<OperationMonitor>, std::path::PathBuf) {
    queued_adapter_with(scope, MonitorConfig::with_timeout(Duration::from_secs(60)))
}

fn queued_adapter_with(
    scope: &std::path::Path,
    config: MonitorConfig,
) -> (Arc<Adapter>, Arc<OperationMonitor>, std::path::PathBuf) {
    use ahma_mcp::adapter::workspace_queue::WorkspaceQueue;
    use ahma_mcp::sandbox::handoff_watch::HandoffWatchMode;
    let monitor = Arc::new(OperationMonitor::new(config));
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![scope.to_path_buf()],
            SandboxMode::Test,
            Default::default(),
        )
        .unwrap(),
    );
    let adapter = Adapter::new(monitor.clone(), shell_pool, sandbox)
        .unwrap()
        .with_workspace_queue(WorkspaceQueue::with_lock_dir(
            true,
            Some(scope.join(".locks")),
        ))
        .with_handoff_watch(HandoffWatchMode::Always);
    let key = adapter.workspace_key_for(scope);
    (Arc::new(adapter), monitor, key)
}

/// A PTY or session command hands the workspace on before anyone can observe
/// it finished, as a piped command does: a caller woken by the terminal state
/// must never find the workspace still held by the command it waited for.
/// Both paths published their result first and released the lease after.
#[tokio::test]
async fn pty_and_session_commands_hand_the_workspace_on_before_they_report() {
    use ahma_mcp::adapter::workspace_queue::LeaseProbe;
    let temp = tempdir().unwrap();
    let (adapter, monitor, key) = queued_adapter(temp.path());
    let wd = temp.path().to_str().unwrap();
    for round in 0..5 {
        let session = adapter
            .execute_session_async("sess", "queue", "echo hi", wd, Some(15), None)
            .await
            .expect("session command starts");
        wait_done(&monitor, &session).await;
        assert!(
            matches!(adapter.workspace_queue().probe(&key), LeaseProbe::Free),
            "round {round}: a finished session command still holds the workspace"
        );
        #[cfg(unix)]
        if ahma_mcp::adapter::pty_available() {
            let pty = adapter
                .execute_pty_async("pty_test", "echo hi", wd, Some(15), None)
                .await
                .expect("pty command starts");
            wait_done(&monitor, &pty).await;
            assert!(
                matches!(adapter.workspace_queue().probe(&key), LeaseProbe::Free),
                "round {round}: a finished PTY command still holds the workspace"
            );
        }
    }
}

/// A command waiting its turn in the workspace queue is `Pending`, not
/// `InProgress` (SPEC R2.7.1): the PTY and session paths marked it running
/// before they waited.
#[tokio::test]
async fn a_queued_pty_or_session_command_is_pending_until_its_turn() {
    use ahma_mcp::adapter::workspace_queue::HolderInfo;
    use ahma_mcp::operation_monitor::OperationStatus;
    let temp = tempdir().unwrap();
    let (adapter, monitor, key) = queued_adapter(temp.path());
    let wd = temp.path().to_str().unwrap();
    let held = adapter
        .workspace_queue()
        .enqueue(&key, HolderInfo::new("op_holder", "cargo build"))
        .expect("queue on")
        .acquire(&tokio_util::sync::CancellationToken::new(), &|_| {})
        .await
        .expect("the holder gets the workspace");

    let mut queued = vec![
        adapter
            .execute_session_async("sess", "waits", "echo hi", wd, Some(15), None)
            .await
            .expect("session command starts"),
    ];
    #[cfg(unix)]
    if ahma_mcp::adapter::pty_available() {
        queued.push(
            adapter
                .execute_pty_async("pty_test", "echo hi", wd, Some(15), None)
                .await
                .expect("pty command starts"),
        );
    }
    for id in &queued {
        let state = monitor
            .get_operation(id)
            .await
            .map(|op| op.state)
            .expect("registered");
        assert_eq!(state, OperationStatus::Pending, "{id} waits its turn");
    }
    drop(held);
    for id in &queued {
        let op = wait_done(&monitor, id).await;
        assert_eq!(
            op.state,
            OperationStatus::Completed,
            "{id}: {:?}",
            op.result
        );
    }
}

/// A piped command hands the workspace on before anyone can observe it ended,
/// whether it exited or was stopped at its time limit. Each ending released
/// the lease by hand, one `drop(lease.take())` per branch.
#[tokio::test]
async fn a_piped_command_hands_the_workspace_on_however_it_ends() {
    use ahma_mcp::adapter::workspace_queue::LeaseProbe;
    use ahma_mcp::operation_monitor::OperationStatus;
    let temp = tempdir().unwrap();
    let (adapter, monitor, key) = queued_adapter(temp.path());
    let wd = temp.path().to_str().unwrap();
    let start = |command: &'static str, limit: u64| {
        let adapter = adapter.clone();
        async move {
            adapter
                .execute_async_in_dir("run_terminal_command", command, None, wd, Some(limit))
                .await
                .expect("piped command starts")
        }
    };

    let exited = start("echo hi", 15).await;
    assert_eq!(
        wait_done(&monitor, &exited).await.state,
        OperationStatus::Completed
    );
    assert!(
        matches!(adapter.workspace_queue().probe(&key), LeaseProbe::Free),
        "an exited command still holds the workspace"
    );

    let timed_out = start("sleep 60", 1).await;
    assert_eq!(
        wait_done(&monitor, &timed_out).await.state,
        OperationStatus::TimedOut
    );
    assert!(
        matches!(adapter.workspace_queue().probe(&key), LeaseProbe::Free),
        "a command stopped at its limit still holds the workspace"
    );

    let cancelled = start("sleep 60", 30).await;
    ahma_mcp::test_utils::assertions::assert_eventually(
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
        ahma_common::timeouts::TestTimeouts::poll_interval(),
        "the command is running",
        || async {
            monitor
                .get_operation(&cancelled)
                .await
                .is_some_and(|op| op.state == OperationStatus::InProgress)
        },
    )
    .await;
    assert!(
        monitor
            .cancel_operation_with_reason(&cancelled, Some("stopped by the test".into()))
            .await
    );
    let op = wait_done(&monitor, &cancelled).await;
    assert_eq!(op.state, OperationStatus::Cancelled);
    assert!(
        op.result
            .as_ref()
            .is_some_and(|r| r.to_string().contains("stopped by the test")),
        "the cancel's own reason is the result: {:?}",
        op.result
    );
    assert!(
        matches!(adapter.workspace_queue().probe(&key), LeaseProbe::Free),
        "a cancelled command still holds the workspace"
    );
}

/// The monitor's watchdog stops a silent command the same way: the workspace
/// is free once the command is observed timed out, and the result names the
/// watchdog's reason. The watchdog published the ending first and signalled
/// the task second, like `cancel_operation`.
#[tokio::test]
async fn a_command_the_watchdog_stops_hands_the_workspace_on_first() {
    use ahma_mcp::adapter::workspace_queue::LeaseProbe;
    use ahma_mcp::operation_monitor::OperationStatus;
    let temp = tempdir().unwrap();
    let config = MonitorConfig::with_timeout(Duration::from_secs(60))
        .with_idle_timeout(Some(Duration::from_secs(1)));
    let (adapter, monitor, key) = queued_adapter_with(temp.path(), config);
    let id = adapter
        .execute_async_in_dir(
            "run_terminal_command",
            "sleep 60",
            None,
            temp.path().to_str().unwrap(),
            Some(30),
        )
        .await
        .expect("piped command starts");
    ahma_mcp::test_utils::assertions::assert_eventually(
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick),
        ahma_common::timeouts::TestTimeouts::poll_interval(),
        "the watchdog stops the silent command",
        || async {
            monitor.check_timeouts().await;
            monitor.get_operation(&id).await.is_none()
        },
    )
    .await;
    let op = wait_done(&monitor, &id).await;
    assert_eq!(op.state, OperationStatus::TimedOut);
    assert!(
        matches!(adapter.workspace_queue().probe(&key), LeaseProbe::Free),
        "a command the watchdog stopped still holds the workspace"
    );
    assert!(
        op.result
            .as_ref()
            .is_some_and(|r| r.to_string().contains("no output")),
        "the watchdog's reason is the result: {:?}",
        op.result
    );
}
