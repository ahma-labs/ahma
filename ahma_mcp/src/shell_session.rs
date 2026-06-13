//! Persistent stateful shell sessions keyed by `session_id`.
//!
//! A session is a long-lived sandboxed shell process (bash on Unix,
//! PowerShell on Windows).  Commands executed with the same `session_id`
//! run sequentially in the SAME shell, so working-directory changes (`cd`),
//! environment variables (`export`), and shell functions persist between
//! tool calls — the missing piece for agent workflows like
//! `source .venv/bin/activate` followed by `pytest`.
//!
//! ## Protocol
//!
//! Each command is written to the session shell followed by a sentinel that
//! prints a unique end marker and the command's exit code.  Output lines are
//! streamed to the caller as they arrive until the marker is seen.  Stderr is
//! merged into stdout per command (`2>&1`), matching ahma's unified shell
//! output contract (SPEC 9.4).
//!
//! ## Failure policy
//!
//! Sessions are best-effort state: on timeout, I/O error, or a command that
//! kills the shell (e.g. `exit`), the session process is killed and removed —
//! the next call with that `session_id` transparently gets a fresh shell.
//! Durable results still flow through the `OperationMonitor` as usual.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::sandbox::Sandbox;
use crate::shell_pool::platform_shell_program;

/// One persistent shell process.
#[derive(Debug)]
struct ShellSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
}

impl ShellSession {
    /// Spawn a new sandboxed session shell rooted at `working_dir`.
    async fn spawn(sandbox: &Sandbox, working_dir: &Path) -> Result<Self> {
        #[cfg(windows)]
        let args: Vec<String> = ["-NoProfile", "-NonInteractive", "-Command", "-"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        #[cfg(not(windows))]
        let args: Vec<String> = Vec::new();

        let mut cmd = sandbox
            .create_command(platform_shell_program(), &args, working_dir)
            .context("failed to build sandboxed session shell command")?;
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().context("failed to spawn session shell")?;
        let stdin = child
            .stdin
            .take()
            .context("session shell stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("session shell stdout unavailable")?;

        Ok(Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
        })
    }

    /// Render the protocol block for one command.
    fn protocol_block(command: &str, marker: &str) -> String {
        #[cfg(windows)]
        {
            // PowerShell `-Command -` evaluates input line by line; the
            // command must therefore be single-line on Windows.
            format!(
                "& {{ {command} }} 2>&1 | Out-String -Stream\nWrite-Output \"{marker}_$(if ($LASTEXITCODE -ne $null) {{ $LASTEXITCODE }} elseif ($?) {{ 0 }} else {{ 1 }})__\"\n"
            )
        }
        #[cfg(not(windows))]
        {
            // Brace group so `a; b && c` style compounds are redirected as a
            // whole; the marker line carries the group's exit code.
            format!("{{ {command}\n}} 2>&1\nprintf '{marker}_%d__\\n' \"$?\"\n")
        }
    }

    /// Execute one command, streaming each output line through `on_line`.
    /// Returns the command's exit code.
    async fn execute_streaming(
        &mut self,
        command: &str,
        timeout: Duration,
        on_line: &mut (dyn FnMut(String) + Send),
    ) -> Result<i32> {
        let nonce: u64 = rand::random();
        let marker = format!("__AHMA_SESSION_DONE_{nonce:016x}");

        let block = Self::protocol_block(command, &marker);
        self.stdin
            .write_all(block.as_bytes())
            .await
            .context("failed to write command to session shell")?;
        self.stdin
            .flush()
            .await
            .context("failed to flush session shell stdin")?;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout_at(deadline, self.reader.read_line(&mut line))
                .await
                .map_err(|_| anyhow::anyhow!("session command timed out after {timeout:?}"))?
                .context("failed to read from session shell")?;
            if read == 0 {
                bail!("session shell exited unexpectedly (EOF)");
            }
            let line = line.trim_end_matches(['\n', '\r']);

            if let Some(idx) = line.find(&marker) {
                // Output without a trailing newline can glue onto the marker.
                let glued = &line[..idx];
                if !glued.is_empty() {
                    on_line(glued.to_string());
                }
                let rest = &line[idx + marker.len()..];
                let code = rest
                    .strip_prefix('_')
                    .and_then(|r| r.strip_suffix("__"))
                    .and_then(|c| c.parse::<i32>().ok())
                    .unwrap_or(-1);
                return Ok(code);
            }
            on_line(line.to_string());
        }
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Manages all persistent shell sessions for one server instance.
#[derive(Debug, Default)]
pub struct ShellSessionManager {
    /// Brief map lock; each session has its own mutex so commands to
    /// different sessions run concurrently while commands to the same
    /// session serialize.
    sessions: Mutex<HashMap<String, Arc<Mutex<ShellSession>>>>,
}

impl ShellSessionManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Execute `command` in the session named `session_id`, creating the
    /// session (rooted at `working_dir`) on first use.  Output lines are
    /// streamed through `on_line`; the command's exit code is returned.
    ///
    /// On timeout or shell death the session is destroyed so the next call
    /// gets a fresh shell.
    pub async fn execute_streaming(
        &self,
        sandbox: &Sandbox,
        session_id: &str,
        working_dir: &Path,
        command: &str,
        timeout: Duration,
        on_line: &mut (dyn FnMut(String) + Send),
    ) -> Result<i32> {
        let session = {
            let mut map = self.sessions.lock().await;
            match map.get(session_id) {
                Some(s) => Arc::clone(s),
                None => {
                    let created = ShellSession::spawn(sandbox, working_dir).await?;
                    let arc = Arc::new(Mutex::new(created));
                    map.insert(session_id.to_string(), Arc::clone(&arc));
                    tracing::info!(
                        "shell session '{}' created (cwd: {})",
                        session_id,
                        working_dir.display()
                    );
                    arc
                }
            }
        };

        let mut guard = session.lock().await;
        match guard.execute_streaming(command, timeout, on_line).await {
            Ok(code) => Ok(code),
            Err(e) => {
                // The shell is in an unknown state — destroy the session.
                guard.kill().await;
                drop(guard);
                self.sessions.lock().await.remove(session_id);
                tracing::warn!("shell session '{}' destroyed: {}", session_id, e);
                Err(e)
            }
        }
    }

    /// Number of live sessions.
    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Terminate one session, if present.
    pub async fn close_session(&self, session_id: &str) {
        let removed = self.sessions.lock().await.remove(session_id);
        if let Some(session) = removed {
            session.lock().await.kill().await;
            tracing::info!("shell session '{}' closed", session_id);
        }
    }

    /// Remove a session from the map WITHOUT acquiring the per-session lock.
    ///
    /// Safe to call from a cancel branch where the per-session mutex may be held
    /// by a suspended `execute_streaming` future.  When all Arc references drop
    /// (including the exec future's internal reference), `kill_on_drop(true)` on
    /// the child process ensures the shell and its children are killed.
    ///
    /// Returns `true` if the session existed.
    pub async fn remove_session(&self, session_id: &str) -> bool {
        let removed = self.sessions.lock().await.remove(session_id);
        if removed.is_some() {
            tracing::info!("shell session '{}' removed (cancel path)", session_id);
        }
        removed.is_some()
    }

    /// Kill all session shells (graceful shutdown).
    pub async fn shutdown_all(&self) {
        let sessions: Vec<_> = self.sessions.lock().await.drain().collect();
        for (id, session) in sessions {
            session.lock().await.kill().await;
            tracing::debug!("shell session '{}' shut down", id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxMode;

    fn test_sandbox(scope: &Path) -> Sandbox {
        Sandbox::new(
            vec![scope.to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap()
    }

    async fn run(
        mgr: &ShellSessionManager,
        sandbox: &Sandbox,
        session: &str,
        wd: &Path,
        cmd: &str,
    ) -> (i32, Vec<String>) {
        let mut lines = Vec::new();
        let code = mgr
            .execute_streaming(
                sandbox,
                session,
                wd,
                cmd,
                Duration::from_secs(10),
                &mut |line| lines.push(line),
            )
            .await
            .expect("session command should run");
        (code, lines)
    }

    #[tokio::test]
    async fn echo_round_trip_and_exit_codes() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(temp.path());
        let mgr = ShellSessionManager::new();

        let (code, lines) = run(&mgr, &sandbox, "s1", temp.path(), "echo hello-session").await;
        assert_eq!(code, 0);
        assert_eq!(lines, vec!["hello-session".to_string()]);

        #[cfg(unix)]
        {
            let (code, _) = run(&mgr, &sandbox, "s1", temp.path(), "false").await;
            assert_eq!(code, 1, "exit code of `false` must propagate");
            let (code, _) = run(&mgr, &sandbox, "s1", temp.path(), "true").await;
            assert_eq!(code, 0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn working_directory_persists_across_commands() {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        let sandbox = test_sandbox(temp.path());
        let mgr = ShellSessionManager::new();

        let (code, _) = run(&mgr, &sandbox, "cwd", temp.path(), "cd subdir").await;
        assert_eq!(code, 0);
        let (_, lines) = run(&mgr, &sandbox, "cwd", temp.path(), "pwd").await;
        assert!(
            lines.iter().any(|l| l.ends_with("subdir")),
            "cd must persist within the session, got: {lines:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn environment_persists_and_sessions_are_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(temp.path());
        let mgr = ShellSessionManager::new();

        let (code, _) = run(&mgr, &sandbox, "a", temp.path(), "export AHMA_T=alpha").await;
        assert_eq!(code, 0);
        let (_, lines) = run(&mgr, &sandbox, "a", temp.path(), "echo \"v=$AHMA_T\"").await;
        assert_eq!(lines, vec!["v=alpha".to_string()]);

        // A different session must not see session a's environment.
        let (_, lines) = run(&mgr, &sandbox, "b", temp.path(), "echo \"v=$AHMA_T\"").await;
        assert_eq!(lines, vec!["v=".to_string()]);
        assert_eq!(mgr.session_count().await, 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_is_merged_and_missing_command_reports_127() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(temp.path());
        let mgr = ShellSessionManager::new();

        let (code, lines) = run(
            &mgr,
            &sandbox,
            "err",
            temp.path(),
            "definitely_not_a_command_xyz",
        )
        .await;
        assert_eq!(code, 127);
        assert!(
            lines.iter().any(|l| l.contains("not found")),
            "stderr must be merged into the stream: {lines:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_without_trailing_newline_is_not_lost() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(temp.path());
        let mgr = ShellSessionManager::new();

        let (code, lines) = run(&mgr, &sandbox, "nl", temp.path(), "printf no-newline").await;
        assert_eq!(code, 0);
        assert_eq!(lines, vec!["no-newline".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dead_session_is_replaced_transparently() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(temp.path());
        let mgr = ShellSessionManager::new();

        // `exit` kills the session shell itself — the command errors...
        let result = mgr
            .execute_streaming(
                &sandbox,
                "dies",
                temp.path(),
                "exit 7",
                Duration::from_secs(5),
                &mut |_| {},
            )
            .await;
        assert!(result.is_err(), "killing the shell should surface an error");
        assert_eq!(mgr.session_count().await, 0, "dead session must be removed");

        // ...and the next call gets a fresh working shell.
        let (code, lines) = run(&mgr, &sandbox, "dies", temp.path(), "echo back").await;
        assert_eq!(code, 0);
        assert_eq!(lines, vec!["back".to_string()]);
    }

    #[tokio::test]
    async fn close_and_shutdown_remove_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(temp.path());
        let mgr = ShellSessionManager::new();

        let _ = run(&mgr, &sandbox, "x", temp.path(), "echo 1").await;
        let _ = run(&mgr, &sandbox, "y", temp.path(), "echo 2").await;
        assert_eq!(mgr.session_count().await, 2);

        mgr.close_session("x").await;
        assert_eq!(mgr.session_count().await, 1);

        mgr.shutdown_all().await;
        assert_eq!(mgr.session_count().await, 0);
    }
}
