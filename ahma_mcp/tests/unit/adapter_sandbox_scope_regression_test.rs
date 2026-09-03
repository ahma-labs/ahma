//! Regression tests for sandbox scope validation.
//!
//! This suite catches the historical bug where `Adapter` validated the
//! working directory against its own `root_path` (captured from process cwd)
//! instead of the globally initialized sandbox scopes.

use ahma_mcp::adapter::Adapter;
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::Sandbox;
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn adapter_uses_global_sandbox_scope_not_adapter_root_path() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let sandbox = Arc::new(
        Sandbox::new(
            vec![temp_dir.path().to_path_buf()],
            ahma_mcp::sandbox::SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap(),
    );

    let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(5));
    let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));

    let shell_pool_config = ShellPoolConfig {
        command_timeout: Duration::from_secs(5),
    };
    let shell_pool = Arc::new(ShellPoolManager::new(shell_pool_config));

    // We create an adapter with our test sandbox which has permissive scopes (test mode)
    let adapter = Adapter::new(operation_monitor, shell_pool, sandbox).expect("adapter");

    // Use a real temp directory so the working directory exists on every platform.
    // The prior version passed "/tmp" which is invalid on Windows and required
    // #[cfg(unix)].  A tempdir works on all platforms without the \\?\ UNC-prefix
    // problem that std::fs::canonicalize introduces on Windows (OS error 267).
    let work_dir = temp_dir.path().to_string_lossy().to_string();
    let dir_name = temp_dir
        .path()
        .file_name()
        .expect("dir_name")
        .to_string_lossy()
        .to_string();

    // Prior to the fix, this would fail with:
    //   "Path ... is outside the sandbox root <adapter_root>"
    let out = adapter
        .execute_sync_in_dir("pwd", None, &work_dir, Some(5), None)
        .await
        .expect("pwd should succeed under global sandbox scope");

    let trimmed = out.trim();
    // On Windows with Git Bash, pwd prints a POSIX path like /c/Users/.../dir_name.
    // On Unix it prints the native path.  Either way the final component is the
    // same as the temp dir name.
    assert!(
        trimmed.ends_with(&dir_name) || trimmed.ends_with(&format!("/{dir_name}")),
        "expected pwd output to end with {dir_name:?}, got: {trimmed:?}"
    );
}
