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

use std::ffi::OsString;
use std::path::Path;

#[cfg(unix)]
use anyhow::Result;

#[cfg(test)]
use super::PublishEnding;
use super::sink::{OpSink, RunEnd};
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

/// What a PTY command gets from the brokers that serve it (SPEC R-CRED.1,
/// R-LINK.1): their variables, and whether a link broker serves it, which
/// its sandbox profile reads as it is built (`links::host::building_for`).
#[derive(Debug, Default)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) struct BrokerEnv {
    pub(super) vars: Vec<(&'static str, OsString)>,
    pub(super) links_served: bool,
}

impl BrokerEnv {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(super) fn push(&mut self, key: &'static str, value: OsString) {
        self.vars.push((key, value));
    }
}

/// Run `command_str` under a PTY through the standard operation lifecycle.
///
/// Mirrors `execute_with_streaming`: writes each output line to `sink`, which
/// also spills the complete output. Returns how it ended, and has no way to
/// publish that: the caller releases the workspace first, then publishes
/// (SPEC R2.7.1).
#[cfg_attr(windows, allow(unused_variables))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_pty_operation(
    sandbox: &Sandbox,
    command_str: &str,
    working_dir: &Path,
    limit: std::time::Duration,
    cancellation_token: &tokio_util::sync::CancellationToken,
    sink: &mut OpSink,
    brokers: &BrokerEnv,
    keep_leftovers: bool,
) -> RunEnd {
    #[cfg(windows)]
    {
        let _ = (brokers, sink);
        RunEnd::Failed(
            "pty: true is not yet supported on Windows (ConPTY integration pending). \
             Run the command without the pty parameter."
                .to_string(),
        )
    }

    #[cfg(unix)]
    unix::run(
        sandbox,
        command_str,
        working_dir,
        limit,
        cancellation_token,
        sink,
        brokers,
        keep_leftovers,
    )
    .await
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::operation_monitor::Stream;
    use crate::shell_pool::platform_shell_program;
    use std::io::{BufRead, BufReader, Read};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    /// Sends SIGKILL to the child's process group on demand.
    ///
    /// The PTY child is a `std::process::Child` (rebuilt as a std `Command` so
    /// its stdio can be the PTY slave fd), so it cannot go through
    /// [`kill_process_tree`](crate::shell_pool::kill_process_tree), which takes a `tokio::process::Child`. It routes
    /// through the same underlying primitive instead — this used to be its own
    /// copy of the syscall pair, which made the "single chokepoint" claim on
    /// `kill_process_tree` untrue.
    ///
    /// Unlike `kill_process_tree` there is no bounded reap here: the child is
    /// owned by the blocking waiter thread spawned in `setup_pty`, which is what
    /// observes the exit and reports it over `exit_tx`.
    struct PtyChildKiller {
        pid: i32,
        /// The run the command is, so that what it leaves running ends with
        /// it (SPEC R-PROC.5).
        run: Option<crate::sandbox::leftovers::Run>,
    }

    impl Drop for PtyChildKiller {
        fn drop(&mut self) {
            // A command that was cancelled, timed out or dropped: nothing to
            // wait for, and no thread to block on a grace.
            if let Some(run) = self.run.take() {
                for leftover in crate::sandbox::leftovers::end_at_once(&run) {
                    tracing::info!(
                        "stopped {} (pid {}), left running by a terminal command that ended \
                         (SPEC R-PROC.5)",
                        leftover.name,
                        leftover.pid
                    );
                }
            }
        }
    }

    impl PtyChildKiller {
        /// End what the command left running, once it has itself ended, and
        /// return it for the command's result. The drop then finds nothing.
        async fn end_leftovers(&mut self) -> Vec<crate::sandbox::leftovers::Leftover> {
            let Some(run) = self.run.take() else {
                return Vec::new();
            };
            // The process table is read through a synchronous API.
            tokio::task::spawn_blocking(move || crate::sandbox::leftovers::end(&run))
                .await
                .unwrap_or_default()
        }

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
    fn setup_pty(
        sandbox: &Sandbox,
        command_str: &str,
        working_dir: &Path,
        brokers: &BrokerEnv,
        keep_leftovers: bool,
    ) -> Result<PtyParts> {
        use anyhow::Context;

        // Built knowing whether a link broker serves this command: its
        // profile, and no other, may then reach the broker directory.
        let mut wrapped = crate::adapter::build_for_this_command(brokers.links_served, || {
            sandbox.create_shell_command(platform_shell_program(), command_str, working_dir)
        })
        .context("failed to build sandboxed PTY command")?;
        // A command a human declared long-running may leave its server
        // running (SPEC R2.7.10, R-PROC.5).
        if keep_leftovers {
            crate::sandbox::leftovers::keep(&mut wrapped);
        }
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
        // Its own brokers: the SSH key broker (SPEC R-CRED.1), whose socket
        // is the only agent it has (the profile refuses it the human's,
        // R-CRED.11), and the link broker's helper and socket (R-LINK.1).
        for (key, value) in &brokers.vars {
            cmd.env(key, value);
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
            // The read-only deny tier, as in `create_shell_command` (SPEC
            // R6.1.7): rebuilding the command dropped that pre_exec hook too.
            let mounts = sandbox.spawn_deny_tier_mounts(working_dir);
            let filter = sandbox.spawn_syscall_filter();
            // SAFETY: the closure only performs async-signal-safe syscalls on
            // buffers built before fork; the OwnedFd moved into it stays open
            // across fork.
            unsafe {
                cmd.pre_exec(move || {
                    if let Some(mounts) = &mounts {
                        mounts.enter_in_child()?;
                    }
                    crate::sandbox::apply_landlock_ruleset_in_child(fd.as_raw_fd())?;
                    if let Some(filter) = &filter {
                        crate::sandbox::syscall_filter::apply_in_child(filter)?;
                    }
                    Ok(())
                });
            }
        }

        let mut child = cmd.spawn().context("failed to spawn PTY command")?;
        let killer = PtyChildKiller {
            pid: child.id() as i32,
            run: crate::sandbox::leftovers::Run::of(&wrapped, Some(child.id())),
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

    /// How long the terminal may stay quiet, after the command has exited,
    /// before its output is taken as complete. A process a human let stay
    /// running (`tools.long_running`) keeps the terminal open for as long as it
    /// lives; its later output is its own, not this command's.
    const QUIET_AFTER_EXIT: Duration = Duration::from_millis(500);

    /// Read what the command wrote before it exited: until the terminal's end
    /// of file, or until it has been quiet for [`QUIET_AFTER_EXIT`].
    async fn drain_after_exit(
        line_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
        sink: &mut OpSink,
    ) {
        while let Ok(Some(line)) = tokio::time::timeout(QUIET_AFTER_EXIT, line_rx.recv()).await {
            sink.line(Stream::Stdout, &line).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run(
        sandbox: &Sandbox,
        command_str: &str,
        working_dir: &Path,
        limit: Duration,
        cancellation_token: &tokio_util::sync::CancellationToken,
        sink: &mut OpSink,
        brokers: &BrokerEnv,
        keep_leftovers: bool,
    ) -> RunEnd {
        let start_time = Instant::now();

        let (mut killer, mut line_rx, mut exit_rx) =
            match setup_pty(sandbox, command_str, working_dir, brokers, keep_leftovers) {
                Ok(parts) => parts,
                Err(e) => return RunEnd::Failed(format!("Failed to start PTY command: {e}")),
            };

        // The command is running: its spill file exists from now on.
        sink.started().await;
        let deadline = tokio::time::Instant::now() + limit;
        // Warned once at 80% of the limit, as a piped command is (SPEC R2.6.6).
        let mut near_limit = sink.near_limit(limit);

        let exit_status: Option<i32> = loop {
            tokio::select! {
                biased;

                _ = cancellation_token.cancelled() => {
                    tracing::info!("PTY operation {} cancelled", sink.op_id());
                    killer.kill();
                    return RunEnd::Cancelled;
                }

                _ = tokio::time::sleep_until(near_limit.at()), if near_limit.pending() => {
                    sink.alert(near_limit.alert()).await;
                }

                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!("PTY operation {} timed out", sink.op_id());
                    killer.kill();
                    // Says how to raise the limit, as the piped path does (SPEC R2.6.6).
                    return RunEnd::TimedOut { ran_for: start_time.elapsed() };
                }

                line = line_rx.recv() => {
                    match line {
                        // A PTY merges stderr into the terminal stream by
                        // construction.
                        Some(line) => sink.line(Stream::Stdout, &line).await,
                        None => {
                            // Reader thread finished (PTY master returned
                            // EOF/EIO after child exit) — all output has been
                            // captured. Now collect the exit code.
                            break exit_rx.recv().await.flatten();
                        }
                    }
                }

                // The command itself has exited. On Linux the terminal reaches
                // end of file only when every process holding it has closed it,
                // and a process the command left running still holds it: waiting
                // for that end of file ran every such command to its time limit.
                // So what it left running ends first (SPEC R-PROC.5), which closes
                // the terminal, and the rest of the output is read after.
                status = exit_rx.recv() => {
                    break status.flatten();
                }
            }
        };

        // The command has ended: what it left running ends too, and its result
        // names it (SPEC R-PROC.5). Before the drain: a process it left running
        // holds the terminal open.
        let stopped = killer.end_leftovers().await;
        drain_after_exit(&mut line_rx, sink).await;
        if let Some(line) = crate::sandbox::leftovers::note(&stopped) {
            sink.alert(line).await;
        }
        RunEnd::Exited(exit_status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::operation_monitor::{MonitorConfig, Operation, OperationMonitor, OperationStatus};
    #[cfg(unix)]
    use crate::sandbox::{Sandbox, SandboxMode};
    #[cfg(unix)]
    use ahma_common::timeouts::TestTimeouts;
    #[cfg(unix)]
    use std::sync::Arc;

    #[cfg(unix)]
    use tempfile::TempDir;
    #[cfg(unix)]
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
            Default::default(),
        )
        .expect("sandbox construction");
        sandbox.mark_roots_received();
        sandbox
    }

    #[cfg(unix)]
    fn test_monitor() -> Arc<OperationMonitor> {
        Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )))
    }

    /// Run a PTY command as the adapter does, through a sink on `monitor`,
    /// and return its ending unpublished.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    async fn run_pty(
        sandbox: &Sandbox,
        command_str: &str,
        working_dir: &std::path::Path,
        limit: std::time::Duration,
        cancellation_token: &CancellationToken,
        op_id: &str,
        monitor: &Arc<OperationMonitor>,
        brokers: &BrokerEnv,
        keep_leftovers: bool,
    ) -> crate::operation_monitor::Terminal {
        let mut sink = OpSink::new(monitor.clone(), op_id, crate::adapter::sink::Shape::Pty);
        let end = run_pty_operation(
            sandbox,
            command_str,
            working_dir,
            limit,
            cancellation_token,
            &mut sink,
            brokers,
            keep_leftovers,
        )
        .await;
        sink.close(end).await
    }

    /// SPEC R-PROC.5: a terminal command that leaves a process running has it
    /// stopped when it ends, and the operation names it, as on the plain
    /// path; a command a human declared long-running keeps what it started.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_terminal_command_ends_and_names_what_it_left_running() {
        if !pty_available() {
            eprintln!("skipping: PTY allocation denied in this environment");
            return;
        }
        let temp = TempDir::new().unwrap();
        let sandbox = test_sandbox(temp.path());
        // A program that waits, started in the background as `nohup` would
        // (a terminal that closes hangs up on its jobs; one that ignores the
        // hangup is the one that stays). The script prints its pid and
        // returns once the program is up.
        let script = format!(
            "trap '' HUP\n\
             AHMA_TEST_LEFTOVER_WAITER=1 '{}' sandbox::leftovers::tests::waits_to_be_stopped \
             --exact --ignored --nocapture >up.ready 2>/dev/null &\necho pid=$!\n\
             until grep -q waiter-ready up.ready; do sleep 0.05; done\n",
            std::env::current_exe().unwrap().display()
        );
        std::fs::write(temp.path().join("leaves.sh"), script).unwrap();
        let limit = TestTimeouts::scale_secs(30);
        let run = async |op_id: &str, keep: bool| {
            let monitor = test_monitor();
            monitor
                .add_operation(Operation::new(
                    op_id.to_string(),
                    "pty_test".to_string(),
                    "sh leaves.sh".to_string(),
                    None,
                ))
                .await;
            run_pty(
                &sandbox,
                "sh leaves.sh",
                temp.path(),
                limit,
                &CancellationToken::new(),
                op_id,
                &monitor,
                &BrokerEnv::default(),
                keep,
            )
            .await
            .publish(&monitor, op_id)
            .await;
            let op = monitor
                .check_completion_history_pub(op_id)
                .await
                .expect("operation should be in completion history");
            assert_eq!(op.state, OperationStatus::Completed, "{:?}", op.result);
            let stdout = op.result.as_ref().and_then(|r| r.get("stdout")).cloned();
            let pid: libc::pid_t = stdout
                .as_ref()
                .and_then(|v| v.as_str())
                .and_then(|out| out.lines().find_map(|l| l.trim().strip_prefix("pid=")))
                .and_then(|p| p.trim().parse().ok())
                .unwrap_or_else(|| panic!("the script prints its pid: {stdout:?}"));
            (pid, op.alerts)
        };
        let alive = |pid: libc::pid_t| {
            // SAFETY: signal 0 only asks whether the pid exists.
            let there = unsafe { libc::kill(pid, 0) == 0 };
            there && !crate::sandbox::leftovers::is_zombie(pid as u32)
        };

        let (left, alerts) = run("pty-leaves", false).await;
        assert!(
            alerts
                .iter()
                .any(|a| a.contains("ahma: stopped") && a.contains(&format!("(pid {left})"))),
            "{alerts:?}"
        );
        assert!(!alive(left), "what the command left running is gone");

        let (kept, alerts) = run("pty-kept", true).await;
        assert!(
            !alerts.iter().any(|a| a.contains("ahma: stopped")),
            "{alerts:?}"
        );
        assert!(alive(kept), "a declared command's process stays");
        // SAFETY: the pid is the waiter this test started.
        unsafe { libc::kill(kept, libc::SIGKILL) };
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

        run_pty(
            &sandbox,
            "echo hello",
            temp.path(),
            std::time::Duration::from_millis(10_000),
            &token,
            op_id,
            &monitor,
            &BrokerEnv::default(),
            false,
        )
        .await
        .publish(&monitor, op_id)
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

    /// SPEC R-CRED.1, R-CRED.11: a PTY command signs through its own broker
    /// like any other. The profile refuses it the human's agent, so without
    /// the broker's socket in `SSH_AUTH_SOCK` it would have no agent at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_pty_points_ssh_at_the_broker_socket_it_is_given() {
        if !pty_available() {
            eprintln!("skipping: PTY allocation denied in this environment");
            return;
        }
        let temp = TempDir::new().unwrap();
        let sandbox = test_sandbox(temp.path());
        let monitor = test_monitor();
        let op_id = "pty-broker-op";
        monitor
            .add_operation(Operation::new(
                op_id.to_string(),
                "pty_test".to_string(),
                "echo".to_string(),
                None,
            ))
            .await;
        let broker = temp.path().join("broker.sock");

        run_pty(
            &sandbox,
            "echo \"sock=$SSH_AUTH_SOCK\"",
            temp.path(),
            std::time::Duration::from_millis(10_000),
            &CancellationToken::new(),
            op_id,
            &monitor,
            &BrokerEnv {
                vars: vec![("SSH_AUTH_SOCK", broker.clone().into_os_string())],
                links_served: false,
            },
            false,
        )
        .await
        .publish(&monitor, op_id)
        .await;

        let op = monitor
            .check_completion_history_pub(op_id)
            .await
            .expect("operation should be in completion history");
        let stdout = op
            .result
            .and_then(|r| r.get("stdout").and_then(|v| v.as_str()).map(str::to_string))
            .unwrap_or_default();
        assert!(
            stdout.contains(&format!("sock={}", broker.display())),
            "the PTY command's SSH_AUTH_SOCK is the broker's: {stdout:?}"
        );
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
        run_pty(
            &sandbox,
            "exit 3",
            temp.path(),
            std::time::Duration::from_millis(10_000),
            &token,
            op_id,
            &monitor,
            &BrokerEnv::default(),
            false,
        )
        .await
        .publish(&monitor, op_id)
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

        run_pty(
            &sandbox,
            "sleep 30",
            temp.path(),
            std::time::Duration::from_millis(60_000),
            &token,
            op_id,
            &monitor,
            &BrokerEnv::default(),
            false,
        )
        .await
        .publish(&monitor, op_id)
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

        run_pty(
            &sandbox,
            "sleep 30",
            temp.path(),
            std::time::Duration::from_millis(50),
            &token,
            op_id,
            &monitor,
            &BrokerEnv::default(),
            false,
        )
        .await
        .publish(&monitor, op_id)
        .await;

        let op = monitor
            .check_completion_history_pub(op_id)
            .await
            .expect("operation should be in completion history");
        assert_eq!(op.state, OperationStatus::TimedOut);
        let result = op.result.expect("timed-out op carries a result payload");
        assert_eq!(result["timed_out"], true, "{result}");
        let msg = crate::operation_monitor::ended_reason(Some(&result)).unwrap_or_default();
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

        run_pty(
            &sandbox,
            "echo nope",
            &missing,
            std::time::Duration::from_millis(10_000),
            &token,
            op_id,
            &monitor,
            &BrokerEnv::default(),
            false,
        )
        .await
        .publish(&monitor, op_id)
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
