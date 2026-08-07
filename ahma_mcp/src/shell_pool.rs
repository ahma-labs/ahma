//! # Platform Shell Selection and Command Timeout Configuration
//!
//! This module provides two small pieces of execution infrastructure:
//!
//! * [`platform_shell_program`] — the shell binary used for command execution
//!   (`powershell` on Windows, `bash` elsewhere).
//! * [`ShellPoolConfig`] / [`ShellPoolManager`] — the shared default command
//!   timeout consumed by the [`Adapter`](crate::adapter::Adapter).
//!
//! ## Historical note
//!
//! This module once contained a prewarmed shell pool (`PrewarmedShell`,
//! `ShellPool`, and the pooling half of `ShellPoolManager`). That machinery had
//! zero production callers — command execution runs through
//! [`ShellSessionManager`](crate::shell_session::ShellSessionManager) PTY
//! sessions and direct sandboxed spawns — so it was removed as dead code. The
//! `ShellPoolManager` name is retained to keep the adapter construction API
//! stable; it is now purely a timeout-configuration holder.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Platform-specific shell helpers
// ---------------------------------------------------------------------------

/// The shell binary used for command execution.
/// On Windows we use the built-in PowerShell (`powershell`); on all other
/// platforms we use `bash`.
///
/// Also used by `mcp_service` handlers to build progress descriptions.
///
/// This is the cross-crate chokepoint for shell selection (see AGENTS.md):
/// every surface that spawns a platform shell — including `ahma_tui` — must
/// take the program name from here rather than hardcoding it.
pub fn platform_shell_program() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "powershell"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "bash"
    }
}

/// Windows process-creation flag that suppresses the console window a spawned
/// child would otherwise flash open.
///
/// This is the cross-crate chokepoint for the flag (see AGENTS.md): every
/// surface that spawns a detached/background process on Windows — including
/// `ahma_tui` — takes it from here rather than hardcoding the raw value.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Execution timeout configuration shared through the [`Adapter`](crate::adapter::Adapter).
///
/// Only `command_timeout` is consumed in production: it is the default budget
/// applied to a tool invocation when the tool config does not specify its own
/// timeout.
#[derive(Debug, Clone)]
pub struct ShellPoolConfig {
    /// Default per-command execution timeout.
    pub command_timeout: Duration,
}

impl Default for ShellPoolConfig {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(300),
        }
    }
}

/// Holder for the shared [`ShellPoolConfig`].
///
/// The prewarmed shell pool this manager once oversaw was removed as dead
/// code; production execution runs through
/// [`ShellSessionManager`](crate::shell_session::ShellSessionManager) PTY
/// sessions. The adapter keeps an `Arc<ShellPoolManager>` purely to read the
/// default command timeout via [`config`](Self::config).
#[derive(Debug)]
pub struct ShellPoolManager {
    config: ShellPoolConfig,
}

impl ShellPoolManager {
    /// Create a new manager holding `config`.
    pub fn new(config: ShellPoolConfig) -> Self {
        Self { config }
    }

    /// Get the shared configuration.
    pub fn config(&self) -> &ShellPoolConfig {
        &self.config
    }
}

/// Grace period for [`kill_process_tree`] to confirm the direct child was reaped
/// after SIGKILL. A child stuck in an uninterruptible kernel wait (D-state: a
/// denied write being retried, a held file lock) will not die promptly even on
/// SIGKILL; bounding the reap keeps the caller from blocking forever on it.
pub const KILL_REAP_GRACE: Duration = Duration::from_secs(5);

/// Kill a spawned command and its entire process group, then **verify** the
/// direct child was reaped within [`KILL_REAP_GRACE`].
///
/// Commands must be spawned as process-group leaders (`.process_group(0)`) for
/// this to reach descendants: on Unix `kill(-pgid)` then takes down the whole
/// tree — e.g. `sandbox-exec → sh → cargo → rustc` — instead of orphaning the
/// grandchildren when only the direct child is signalled. On non-Unix it falls
/// back to killing the direct child.
///
/// This is the cross-crate chokepoint for process-group teardown (see
/// AGENTS.md): every surface that owns a spawned child it must fully reap —
/// including `ahma_tui`'s window commands — calls this rather than
/// reimplementing the group-kill.
///
/// Returns `true` if the child was confirmed dead, `false` if it did not reap
/// within the grace window (it is likely suspended or wedged in the kernel, and
/// may linger as an orphan). The boolean lets callers log the difference instead
/// of silently assuming the kill worked.
pub async fn kill_process_tree(child: &mut tokio::process::Child) -> bool {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Negative pid targets the process group led by the child.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    // `start_kill` sends SIGKILL to the direct child (idempotent on Unix after the
    // group kill); the bounded `wait` confirms the reap rather than blocking
    // unboundedly on an unresponsive child.
    let _ = child.start_kill();
    match tokio::time::timeout(KILL_REAP_GRACE, child.wait()).await {
        Ok(Ok(_status)) => true,
        Ok(Err(e)) => {
            tracing::warn!("kill_process_tree: error reaping child: {}", e);
            false
        }
        Err(_) => {
            tracing::error!(
                "kill_process_tree: child did not exit within {:.0}s of SIGKILL — \
                 it is likely suspended or wedged in the kernel and may orphan",
                KILL_REAP_GRACE.as_secs_f64()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::logging::init_test_logging;

    #[test]
    fn test_shell_pool_config_default_timeout() {
        init_test_logging();
        let config = ShellPoolConfig::default();
        assert_eq!(config.command_timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_manager_returns_configured_timeout() {
        init_test_logging();
        let manager = ShellPoolManager::new(ShellPoolConfig {
            command_timeout: Duration::from_secs(42),
        });
        assert_eq!(manager.config().command_timeout, Duration::from_secs(42));
    }

    #[test]
    fn test_platform_shell_program_matches_platform() {
        init_test_logging();
        #[cfg(target_os = "windows")]
        assert_eq!(platform_shell_program(), "powershell");
        #[cfg(not(target_os = "windows"))]
        assert_eq!(platform_shell_program(), "bash");
    }

    /// Regression: a timed-out/cancelled command must take down its whole process
    /// group, not just the direct child. Spawn `sh` (direct child) as a
    /// process-group leader; it backgrounds `sleep 60` (grandchild) and records
    /// the grandchild pid. `kill_process_tree` must kill the grandchild too —
    /// otherwise interrupted builds orphan `cargo`/`rustc` and leak processes.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_takes_down_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 60 & echo $! > {}; wait", pidfile.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            // Same flag base_command sets in production — makes the child a
            // process-group leader so the group kill reaches the grandchild.
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn sh");

        // Wait for the grandchild pid to be recorded.
        let mut gpid = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(p) = s.trim().parse::<i32>()
            {
                gpid = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let gpid = gpid.expect("grandchild pid file should be written");

        // Grandchild is alive (signal 0 only probes existence).
        assert_eq!(
            unsafe { libc::kill(gpid, 0) },
            0,
            "grandchild should be alive before kill"
        );

        let reaped = kill_process_tree(&mut child).await;
        assert!(
            reaped,
            "kill_process_tree should confirm the direct child was reaped"
        );

        // The grandchild should die (and be reaped) shortly after the group kill.
        let mut dead = false;
        for _ in 0..100 {
            if unsafe { libc::kill(gpid, 0) } != 0 {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            dead,
            "grandchild (pid {gpid}) must be killed via process-group kill, not orphaned"
        );
    }

    /// The verified-kill contract: `kill_process_tree` returns `true` once the
    /// direct child is confirmed reaped, and does so well within the grace window.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_confirms_reap_of_direct_child() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");

        let start = std::time::Instant::now();
        let reaped = kill_process_tree(&mut child).await;
        let elapsed = start.elapsed();

        assert!(reaped, "a plain killable child must be confirmed reaped");
        assert!(
            elapsed < Duration::from_secs(KILL_REAP_GRACE.as_secs()),
            "reap should be near-instant for a killable child, took {elapsed:?}"
        );
    }
}
