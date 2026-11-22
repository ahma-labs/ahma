//! A command near its time limit is warned about, and a command stopped at it
//! says how to raise the limit (bug report from an agent whose healthy 25–30
//! minute gate sat right at the 30-minute limit with no warning).

use ahma_mcp::adapter::Adapter;
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

fn adapter_in(dir: &std::path::Path, monitor: Arc<OperationMonitor>) -> Arc<Adapter> {
    let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![dir.to_path_buf()],
            SandboxMode::Test,
            Default::default(),
        )
        .unwrap(),
    );
    Arc::new(Adapter::new(monitor, shell_pool, sandbox).unwrap())
}

#[tokio::test]
async fn a_long_operation_is_warned_at_eighty_percent_and_told_how_to_raise_the_limit() {
    let temp = tempdir().unwrap();
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));
    let adapter = adapter_in(temp.path(), monitor.clone());
    let op_id = adapter
        .execute_async_in_dir(
            "sleep_tool",
            "sleep 60",
            None,
            temp.path().to_str().unwrap(),
            Some(2),
        )
        .await
        .unwrap();
    let wait =
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick);
    let op = tokio::time::timeout(wait, monitor.wait_for_operation(&op_id))
        .await
        .expect("the operation ends at its limit")
        .expect("the operation is known");
    assert!(
        op.alerts
            .iter()
            .any(|a| a.contains("80%") && a.contains("timeout_secs")),
        "warned before the limit: {:?}",
        op.alerts
    );
    let result = op.result.map(|r| r.to_string()).unwrap_or_default();
    assert!(
        result.contains("timeout_secs") && result.contains(".ahma/settings.toml"),
        "the stop says how to raise the limit: {result}"
    );
}

#[tokio::test]
async fn a_command_stopped_at_its_limit_says_how_to_raise_it() {
    let temp = tempdir().unwrap();
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));
    let adapter = adapter_in(temp.path(), monitor);
    let err = adapter
        .run_to_completion(
            None,
            "sleep 10",
            None,
            temp.path().to_str().unwrap(),
            Some(1),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("timed out"), "{err}");
    assert!(
        err.contains("timeout_secs") && err.contains(".ahma/settings.toml"),
        "{err}"
    );
}

/// What a command stopped at a 2-second limit was told: its alerts and its
/// result text.
async fn stopped_at_two_seconds(monitor: &OperationMonitor, op_id: &str) -> (Vec<String>, String) {
    use ahma_mcp::operation_monitor::OperationStatus;
    let wait =
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick);
    let op = tokio::time::timeout(wait, monitor.wait_for_operation(op_id))
        .await
        .expect("the operation ends at its limit")
        .expect("the operation is known");
    assert_eq!(op.state, OperationStatus::TimedOut, "{:?}", op.result);
    let result = op.result.map(|r| r.to_string()).unwrap_or_default();
    (op.alerts, result)
}

fn warned_near_the_limit(alerts: &[String]) -> bool {
    alerts
        .iter()
        .any(|a| a.contains("80%") && a.contains("timeout_secs"))
}

/// A PTY command is warned at 80% of its limit like a piped one (SPEC
/// R2.6.6), and its stop says how to raise the limit. It got neither.
#[cfg(unix)]
#[tokio::test]
async fn a_pty_command_is_warned_at_eighty_percent_and_told_how_to_raise_the_limit() {
    if !ahma_mcp::adapter::pty_available() {
        eprintln!("SKIP: PTY allocation unavailable in this environment (outer sandbox)");
        return;
    }
    let temp = tempdir().unwrap();
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));
    let adapter = adapter_in(temp.path(), monitor.clone());
    let op_id = adapter
        .execute_pty_async(
            "pty_test",
            "sleep 60",
            temp.path().to_str().unwrap(),
            Some(2),
            None,
        )
        .await
        .unwrap();
    let (alerts, result) = stopped_at_two_seconds(&monitor, &op_id).await;
    assert!(
        warned_near_the_limit(&alerts),
        "warned before the limit: {alerts:?}"
    );
    assert!(
        result.contains("timeout_secs") && result.contains(".ahma/settings.toml"),
        "the stop says how to raise the limit: {result}"
    );
}

/// The same for a command in a persistent shell session.
#[tokio::test]
async fn a_session_command_is_warned_at_eighty_percent_and_told_how_to_raise_the_limit() {
    let temp = tempdir().unwrap();
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));
    let adapter = adapter_in(temp.path(), monitor.clone());
    let op_id = adapter
        .execute_session_async(
            "sess",
            "near-limit",
            "sleep 60",
            temp.path().to_str().unwrap(),
            Some(2),
            None,
        )
        .await
        .unwrap();
    let (alerts, result) = stopped_at_two_seconds(&monitor, &op_id).await;
    assert!(
        warned_near_the_limit(&alerts),
        "warned before the limit: {alerts:?}"
    );
    assert!(
        result.contains("timeout_secs") && result.contains(".ahma/settings.toml"),
        "the stop says how to raise the limit: {result}"
    );
}
