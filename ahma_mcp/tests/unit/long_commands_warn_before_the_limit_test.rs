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
            false,
            false,
            false,
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
        .execute_sync_in_dir(
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
