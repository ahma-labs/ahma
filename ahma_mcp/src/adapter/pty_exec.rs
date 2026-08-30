//! PTY-backed command execution (opt-in via `pty: true`).
//!
//! Some tools change behaviour when stdout is not a terminal: they disable
//! progress bars, colours, or interactive prompts, or refuse to run at all.
//! This module runs a command attached to a real pseudo-terminal (via
//! `libc::openpty` on Unix), while keeping the rest of the pipeline identical
//! to the piped path: lines are redacted, appended to the operation tail
//! (emitting `OutputLine` events), spilled to the full-output file, and the
//! terminal status transition emits the usual event.
//!
//! A PTY merges stdout and stderr into one stream by construction, so all
//! lines are recorded as stdout.
//!
//! Windows support (ConPTY) is pending; `pty: true` fails fast there with an
//! actionable error.

use std::path::Path;
use std::sync::Arc;

#[cfg(unix)]
use anyhow::Result;
use serde_json::Value;

use crate::operation_monitor::{OperationMonitor, OperationStatus};
use crate::sandbox::Sandbox;

/// Returns `true` when a pseudo-terminal can be allocated in this process.
///
/// PTY allocation can be denied by an outer sandbox (e.g. running the test
/// suite inside ahma's own Seatbelt wrapper denies `/dev/ptmx`).  Callers and
/// tests use this to skip gracefully instead of failing.
pub fn pty_available() -> bool {
    #[cfg(unix)]
    {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            true
        } else {
            false
        }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Run `command_str` under a PTY through the standard operation lifecycle.
///
/// Mirrors `execute_with_streaming`: emits output lines via the monitor,
/// spills the complete output, and ends with exactly one terminal status
/// transition (Completed/Failed/Cancelled/TimedOut).
///
/// Returns how it ended so the caller can close the audit log's `tool_call` with
/// exactly one matching `tool_complete`.
#[cfg_attr(windows, allow(unused_variables))]
pub(super) async fn run_pty_operation(
    sandbox: &Sandbox,
    command_str: &str,
    working_dir: &Path,
    timeout_ms: u64,
    cancellation_token: &tokio_util::sync::CancellationToken,
    op_id: &str,
    monitor: &Arc<OperationMonitor>,
) -> (crate::adapter::audit::Outcome, Option<i32>) {
    #[cfg(windows)]
    {
        monitor
            .update_status(
                op_id,
                OperationStatus::Failed,
                Some(Value::String(
                    "pty: true is not yet supported on Windows (ConPTY integration pending). \
                     Run the command without the pty parameter."
                        .to_string(),
                )),
            )
            .await;
        (crate::adapter::audit::Outcome::Failed, None)
    }

    #[cfg(unix)]
    unix::run(
        sandbox,
        command_str,
        working_dir,
        timeout_ms,
        cancellation_token,
        op_id,
        monitor,
    )
    .await
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::adapter::spill;
    use crate::shell_pool::platform_shell_program;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    /// Sends SIGKILL to the child's process group on demand.
    ///
    /// The PTY child is a `std::process::Child` (rebuilt as a std `Command` so
    /// its stdio can be the PTY slave fd), so it cannot go through
    /// [`kill_process_tree`], which takes a `tokio::process::Child`. It routes
    /// through the same underlying primitive instead — this used to be its own
    /// copy of the syscall pair, which made the "single chokepoint" claim on
    /// `kill_process_tree` untrue.
    ///
    /// Unlike `kill_process_tree` there is no bounded reap here: the child is
    /// owned by the blocking waiter thread spawned in `setup_pty`, which is what
    /// observes the exit and reports it over `exit_tx`.
    struct PtyChildKiller {
        pid: i32,
    }

    impl PtyChildKiller {
        fn kill(&self) {
            // The child called setsid(), so its pid is also its process-group
            // id — kill the whole group to take down shell descendants.
            crate::shell_pool::signal_process_group_kill(self.pid);
        }
    }

    type PtyParts = (
        PtyChildKiller,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tokio::sync::mpsc::UnboundedReceiver<Option<i32>>,
    );

    /// Open a PTY, spawn the sandbox-wrapped shell command attached to it,
    /// and start blocking reader/waiter threads.
    fn setup_pty(sandbox: &Sandbox, command_str: &str, working_dir: &Path) -> Result<PtyParts> {
        use anyhow::Context;

        let wrapped = sandbox
            .create_shell_command(platform_shell_program(), command_str, working_dir)
            .context("failed to build sandboxed PTY command")?;
        let std_wrapped = wrapped.as_std();

        // Open the pseudo-terminal pair.
        let (master_fd, slave_fd) = {
            let mut master: libc::c_int = -1;
            let mut slave: libc::c_int = -1;
            let rc = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                anyhow::bail!("openpty failed: {}", std::io::Error::last_os_error());
            }
            // SAFETY: openpty returned valid, owned file descriptors.
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
        };

        // Rebuild as a std Command so we can attach the PTY slave as
        // stdin/stdout/stderr and start a new session in pre_exec.
        let mut cmd = std::process::Command::new(std_wrapped.get_program());
        cmd.args(std_wrapped.get_args());
        cmd.current_dir(std_wrapped.get_current_dir().unwrap_or(working_dir));
        for (key, value) in std_wrapped.get_envs() {
            match value {
                Some(v) => {
                    cmd.env(key, v);
                }
                None => {
                    cmd.env_remove(key);
                }
            }
        }
        cmd.stdin(std::process::Stdio::from(slave_fd.try_clone()?));
        cmd.stdout(std::process::Stdio::from(slave_fd.try_clone()?));
        cmd.stderr(std::process::Stdio::from(slave_fd));
        // New session so the PTY can become the controlling terminal and the
        // whole process group is killable as one unit.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        // Rebuilding as a std Command above drops the pre_exec hooks that
        // `create_shell_command` attached, so re-apply spawn-time Landlock
        // here. Landlock restricts only the calling thread; restricting the
        // single-threaded forked child is the only placement that reliably
        // covers commands spawned from tokio worker threads.
        #[cfg(target_os = "linux")]
        if let Some(fd) = sandbox
            .spawn_landlock_ruleset_fd()
            .context("failed to build Landlock ruleset for PTY command")?
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the closure only performs async-signal-safe syscalls; the
            // OwnedFd moved into it stays open across fork.
            unsafe {
                cmd.pre_exec(move || {
                    crate::sandbox::apply_landlock_ruleset_in_child(fd.as_raw_fd())
                });
            }
        }

        let mut child = cmd.spawn().context("failed to spawn PTY command")?;
        let killer = PtyChildKiller {
            pid: child.id() as i32,
        };

        let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (exit_tx, exit_rx) = tokio::sync::mpsc::unbounded_channel::<Option<i32>>();

        // Blocking reader thread — PTY I/O has no portable async API.  When
        // the child exits the read fails with EIO (Linux) or returns 0
        // (macOS); both end the loop.
        let master_file = std::fs::File::from(master_fd);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(LimitedRead(master_file));
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']);
                        if line_tx.send(trimmed.to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Blocking waiter thread for the exit status.
        std::thread::spawn(move || {
            let status = child.wait().ok().and_then(|s| s.code());
            let _ = exit_tx.send(status);
        });

        Ok((killer, line_rx, exit_rx))
    }

    /// Newtype so a `File` over the PTY master can sit inside `BufReader`
    /// with read errors treated as end-of-stream by the caller.
    struct LimitedRead(std::fs::File);

    impl Read for LimitedRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    pub(super) async fn run(
        sandbox: &Sandbox,
        command_str: &str,
        working_dir: &Path,
        timeout_ms: u64,
        cancellation_token: &tokio_util::sync::CancellationToken,
        op_id: &str,
        monitor: &Arc<OperationMonitor>,
    ) -> (crate::adapter::audit::Outcome, Option<i32>) {
        use crate::adapter::audit::Outcome;

        let start_time = Instant::now();

        let (killer, mut line_rx, mut exit_rx) = match setup_pty(sandbox, command_str, working_dir)
        {
            Ok(parts) => parts,
            Err(e) => {
                monitor
                    .update_status(
                        op_id,
                        OperationStatus::Failed,
                        Some(Value::String(format!("Failed to start PTY command: {e}"))),
                    )
                    .await;
                return (Outcome::Failed, None);
            }
        };

        let mut spill_writer = spill::SpillWriter::create(op_id).await;
        let mut collected = crate::adapter::BoundedLineCollector::default();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        let exit_status: Option<i32> = loop {
            tokio::select! {
                biased;

                _ = cancellation_token.cancelled() => {
                    tracing::info!("PTY operation {} cancelled", op_id);
                    killer.kill();
                    spill_writer.finish().await;
                    monitor
                        .update_status(
                            op_id,
                            OperationStatus::Cancelled,
                            Some(Value::String("Operation was cancelled".to_string())),
                        )
                        .await;
                    return (Outcome::Cancelled, None);
                }

                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!("PTY operation {} timed out", op_id);
                    killer.kill();
                    spill_writer.finish().await;
                    let duration_ms = start_time.elapsed().as_millis() as u64;
                    monitor
                        .update_status(
                            op_id,
                            OperationStatus::TimedOut,
                            Some(Value::String(format!(
                                "Operation timed out after {duration_ms}ms (exceeded timeout limit)"
                            ))),
                        )
                        .await;
                    return (Outcome::TimedOut, None);
                }

                line = line_rx.recv() => {
                    match line {
                        Some(line) => {
                            let safe = crate::log_monitor::redact_sensitive_line(&line);
                            spill_writer.write_line(&safe, false).await;
                            collected.push(safe.clone());
                            monitor.append_output_line(op_id, safe, false).await;
                        }
                        None => {
                            // Reader thread finished (PTY master returned
                            // EOF/EIO after child exit) — all output has been
                            // captured. Now collect the exit code.
                            break exit_rx.recv().await.flatten();
                        }
                    }
                }
            }
        };

        spill_writer.finish().await;

        let exit_code = exit_status.unwrap_or(-1);
        let success = exit_code == 0;
        let final_output = json!({
            "stdout": collected.rendered_output(),
            // A PTY merges stderr into the terminal stream by construction.
            "stderr": "",
            "exit_code": exit_code,
            "pty": true,
            "stdout_truncated_lines": collected.dropped_lines(),
            "stdout_truncated_bytes": collected.dropped_bytes(),
            "output_file": spill::operation_spill_path(op_id).to_string_lossy(),
        });
        let status = if success {
            OperationStatus::Completed
        } else {
            OperationStatus::Failed
        };
        monitor
            .update_status(op_id, status, Some(final_output))
            .await;

        let outcome = if success {
            Outcome::Completed
        } else {
            Outcome::Failed
        };
        (outcome, exit_status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_monitor::{MonitorConfig, Operation, OperationMonitor, OperationStatus};
    use crate::sandbox::{Sandbox, SandboxMode};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    /// `pty_available()` must run without panicking and return a `bool`.
    /// On Unix it should report `true` on a normal dev/CI box that can
    /// allocate a PTY; if PTY allocation is denied (nested sandbox) it returns
    /// `false`. Either way the call itself must not panic.
    #[test]
    fn pty_available_is_callable_and_returns_bool() {
        // The call itself must not panic; on Unix a normal dev/CI box that can
        // allocate a PTY reports `true`, otherwise `false`. On non-Unix it is
        // always `false`.
        let available = pty_available();
        #[cfg(unix)]
        {
            // Calling twice must be stable (no side effects flip the result).
            assert_eq!(available, pty_available());
        }
        #[cfg(not(unix))]
        assert!(!available, "non-Unix always reports PTY unavailable");
    }

    /// Build a `Sandbox` scoped to `dir` in test mode, mirroring
    /// `test_utils::in_process` so command execution works without an
    /// OS-level kernel sandbox.
    #[cfg(unix)]
    fn test_sandbox(dir: &std::path::Path) -> Sandbox {
        let sandbox = Sandbox::new(
            vec![dir.to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .expect("sandbox construction");
        sandbox.set_roots_received(true);
        sandbox
    }

    #[cfg(unix)]
    fn test_monitor() -> Arc<OperationMonitor> {
        Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            Duration::from_secs(30),
        )))
    }

    /// Happy path: run `echo hello` through the PTY pipeline and assert the
    /// operation completes successfully with the expected captured output.
    ///
    /// Covers `run_pty_operation` Unix dispatch (lines 92-102), `unix::run`
    /// setup + select-loop `line_rx`/`exit_rx` arms and terminal Completed
    /// status (lines 263-368), plus `setup_pty` Ok path (lines 140-251).
    #[cfg(unix)]
    #[tokio::test]
    async fn run_pty_echo_completes_with_output() {
        if !pty_available() {
            eprintln!("skipping: PTY allocation denied in this environment");
            return;
        }
        let temp = TempDir::new().unwrap();
        let sandbox = test_sandbox(temp.path());
        let monitor = test_monitor();
        let op_id = "pty-echo-op";
        monitor
            .add_operation(Operation::new(
                op_id.to_string(),
                "pty_test".to_string(),
                "echo".to_string(),
                None,
            ))
            .await;
        let token = CancellationToken::new();

        run_pty_operation(
            &sandbox,
            "echo hello",
            temp.path(),
            10_000,
            &token,
            op_id,
            &monitor,
        )
        .await;

        let op = monitor
            .check_completion_history_pub(op_id)
            .await
            .expect("operation should be in completion history");
        assert_eq!(op.state, OperationStatus::Completed);
        let result = op.result.expect("completed op carries a result payload");
        let stdout = result
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            stdout.contains("hello"),
            "expected captured stdout to contain 'hello', got: {stdout:?}"
        );
        assert_eq!(result.get("exit_code").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(result.get("pty").and_then(|v| v.as_bool()), Some(true));
    }

    /// A command that exits non-zero must end in `Failed` with the non-zero
    /// exit code recorded. Covers the `success == false` branch (lines
    /// 349-365) of `unix::run`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_pty_nonzero_exit_marks_failed() {
        if !pty_available() {
            eprintln!("skipping: PTY allocation denied in this environment");
            return;
        }
        let temp = TempDir::new().unwrap();
        let sandbox = test_sandbox(temp.path());
        let monitor = test_monitor();
        let op_id = "pty-exit-op";
        monitor
            .add_operation(Operation::new(
                op_id.to_string(),
                "pty_test".to_string(),
                "exit".to_string(),
                None,
            ))
            .await;
        let token = CancellationToken::new();

        // `exit 3` is portable across the POSIX shells used on Unix.
        run_pty_operation(
            &sandbox,
            "exit 3",
            temp.path(),
            10_000,
            &token,
            op_id,
            &monitor,
        )
        .await;

        let op = monitor
            .check_completion_history_pub(op_id)
            .await
            .expect("operation should be in completion history");
        assert_eq!(op.state, OperationStatus::Failed);
        let result = op.result.expect("failed op carries a result payload");
        assert_eq!(result.get("exit_code").and_then(|v| v.as_i64()), Some(3));
    }

    /// An already-cancelled token makes the biased `select!` take the
    /// cancellation arm immediately, ending in `Cancelled`. Covers lines
    /// 297-309 of `unix::run`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_pty_cancelled_token_yields_cancelled() {
        if !pty_available() {
            eprintln!("skipping: PTY allocation denied in this environment");
            return;
        }
        let temp = TempDir::new().unwrap();
        let sandbox = test_sandbox(temp.path());
        let monitor = test_monitor();
        let op_id = "pty-cancel-op";
        monitor
            .add_operation(Operation::new(
                op_id.to_string(),
                "pty_test".to_string(),
                "sleep".to_string(),
                None,
            ))
            .await;
        let token = CancellationToken::new();
        token.cancel();

        run_pty_operation(
            &sandbox,
            "sleep 30",
            temp.path(),
            60_000,
            &token,
            op_id,
            &monitor,
        )
        .await;

        let op = monitor
            .check_completion_history_pub(op_id)
            .await
            .expect("operation should be in completion history");
        assert_eq!(op.state, OperationStatus::Cancelled);
    }

    /// A tiny timeout against a long-running command makes the
    /// `sleep_until(deadline)` arm fire, ending in `TimedOut`. Covers lines
    /// 311-326 of `unix::run`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_pty_timeout_yields_timed_out() {
        if !pty_available() {
            eprintln!("skipping: PTY allocation denied in this environment");
            return;
        }
        let temp = TempDir::new().unwrap();
        let sandbox = test_sandbox(temp.path());
        let monitor = test_monitor();
        let op_id = "pty-timeout-op";
        monitor
            .add_operation(Operation::new(
                op_id.to_string(),
                "pty_test".to_string(),
                "sleep".to_string(),
                None,
            ))
            .await;
        let token = CancellationToken::new();

        run_pty_operation(
            &sandbox,
            "sleep 30",
            temp.path(),
            50,
            &token,
            op_id,
            &monitor,
        )
        .await;

        let op = monitor
            .check_completion_history_pub(op_id)
            .await
            .expect("operation should be in completion history");
        assert_eq!(op.state, OperationStatus::TimedOut);
        let result = op.result.expect("timed-out op carries a result payload");
        let msg = result.as_str().unwrap_or_default();
        assert!(
            msg.contains("timed out"),
            "expected a timeout message, got: {msg:?}"
        );
    }

    /// A non-existent working directory makes the child spawn fail inside
    /// `setup_pty`, so `unix::run` reports `Failed` with the start-error
    /// message. Covers the `setup_pty` `Err` arm (lines 277-286) of `run`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_pty_bad_working_dir_marks_failed() {
        let temp = TempDir::new().unwrap();
        let sandbox = test_sandbox(temp.path());
        let monitor = test_monitor();
        let op_id = "pty-baddir-op";
        monitor
            .add_operation(Operation::new(
                op_id.to_string(),
                "pty_test".to_string(),
                "baddir".to_string(),
                None,
            ))
            .await;
        let token = CancellationToken::new();
        let missing = temp.path().join("does-not-exist-subdir");

        run_pty_operation(
            &sandbox,
            "echo nope",
            &missing,
            10_000,
            &token,
            op_id,
            &monitor,
        )
        .await;

        let op = monitor
            .check_completion_history_pub(op_id)
            .await
            .expect("operation should be in completion history");
        assert_eq!(op.state, OperationStatus::Failed);
        let result = op.result.expect("failed op carries a result payload");
        let msg = result.as_str().unwrap_or_default();
        assert!(
            msg.contains("Failed to start PTY command"),
            "expected start-failure message, got: {msg:?}"
        );
    }
}
