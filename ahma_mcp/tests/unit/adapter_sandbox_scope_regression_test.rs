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

#[tokio::test]
async fn adapter_sync_denial_on_symlinked_target_returns_runtime_denial() {
    let ws = tempfile::tempdir().expect("ws");
    let ext = tempfile::tempdir().expect("ext");

    let ws_canon = dunce::canonicalize(ws.path()).unwrap();
    let ext_canon = dunce::canonicalize(ext.path()).unwrap();

    let target_symlink = ws_canon.join("target");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&ext_canon, &target_symlink).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&ext_canon, &target_symlink).unwrap();

    let sandbox = Arc::new(
        Sandbox::new(
            vec![ws_canon.clone()],
            ahma_mcp::sandbox::SandboxMode::Strict,
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

    let adapter = Adapter::new(operation_monitor, shell_pool, sandbox).expect("adapter");

    #[cfg(unix)]
    {
        let script = ws_canon.join("fail.sh");
        std::fs::write(&script, "printf \"error: failed to create directory 'target/debug'\\nCaused by:\\n  Operation not permitted (os error 1)\\n\" >&2\nexit 1\n").unwrap();
    }
    #[cfg(windows)]
    {
        let script = ws_canon.join("fail.bat");
        std::fs::write(
            &script,
            "@echo error: failed to create directory 'target\\debug' 1>&2\r\n@echo Caused by: 1>&2\r\n@echo   Operation not permitted (os error 1) 1>&2\r\n@exit /b 1\r\n",
        )
        .unwrap();
    }

    #[cfg(unix)]
    let cmd = "sh fail.sh";
    #[cfg(windows)]
    let cmd = "cmd /c fail.bat";

    let res = adapter
        .execute_sync_in_dir(cmd, None, &ws_canon.to_string_lossy(), Some(5), None)
        .await;

    assert!(res.is_err(), "command must fail");
    let err = res.unwrap_err();
    let sandbox_err = err
        .downcast_ref::<ahma_mcp::sandbox::SandboxError>()
        .expect("must return typed SandboxError::RuntimeDenial");

    match sandbox_err {
        ahma_mcp::sandbox::SandboxError::RuntimeDenial { path, access, .. } => {
            assert_eq!(
                path, &ext_canon,
                "denial path must be the external symlink target"
            );
            assert_eq!(*access, ahma_common::config::ScopeAccess::Rw);
        }
        other => panic!("expected RuntimeDenial, got: {:?}", other),
    }
}
