//! Peer connection abstraction for bridge sessions (P1 / P6).
//!

//! Re-exports [`PeerFactory`], [`PeerStreams`], and related types from
//! [`ahma_common::peer_factory`] (P6 split) so call sites in the bridge and
//! in `ahma_mcp::test_utils` can import them from a single, lightweight
//! location without a back-edge dependency.
//!
//! The production adapter [`SubprocessPeerFactory`] lives here (bridge-only:
//! it uses `tokio::process`).  The in-process test doubles live in
//! `ahma_mcp::test_utils::bridge_peer`.
//!
//! [`ahma_common::peer_factory`]: ahma_common::peer_factory
//! [`SessionManagerConfig`]: crate::session::SessionManagerConfig

use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

// ─── Re-exports from ahma_common (P6) ────────────────────────────────────────

pub use ahma_common::peer_factory::{BoxFuture, PeerFactory, PeerShutdownFn, PeerStreams};

// ─── SubprocessPeerFactory ───────────────────────────────────────────────────

/// [`PeerFactory`] that spawns an external MCP server subprocess per session.
///
/// This is the production adapter — each session gets a fully isolated process
/// with its own kernel-enforced sandbox.
#[derive(Debug, Clone)]
pub struct SubprocessPeerFactory {
    /// Path or command name of the MCP server (e.g. `"ahma"`).
    pub command: String,
    /// Additional arguments passed *before* the injected `--defer-sandbox
    /// --server-child` flags.
    pub args: Vec<String>,
    /// When `true`, subprocess stderr is captured so the bridge can display
    /// colored debug output.  When `false` stderr is inherited (goes to the
    /// bridge's own stderr / journal).
    pub enable_colored_output: bool,
    /// Explicit fallback sandbox scope forwarded to the subprocess as
    /// `--sandbox-scope <path>`.  Without this, a subprocess spawned with
    /// `--defer-sandbox` has no pre-configured scopes and will defer its
    /// sandbox indefinitely when the client returns 0 roots.
    pub default_sandbox_scope: Option<PathBuf>,
}

impl SubprocessPeerFactory {
    /// Construct a new factory.
    pub fn new(command: impl Into<String>, args: Vec<String>, enable_colored_output: bool) -> Self {
        Self {
            command: command.into(),
            args,
            enable_colored_output,
            default_sandbox_scope: None,
        }
    }

    /// Set the fallback sandbox scope for the subprocess.
    #[must_use]
    pub fn with_default_sandbox_scope(mut self, scope: Option<PathBuf>) -> Self {
        self.default_sandbox_scope = scope;
        self
    }
}

impl PeerFactory for SubprocessPeerFactory {
    fn create(&self) -> BoxFuture<anyhow::Result<PeerStreams>> {
        let command = self.command.clone();
        let base_args = self.args.clone();
        let enable_colored_output = self.enable_colored_output;
        let default_sandbox_scope = self.default_sandbox_scope.clone();

        Box::pin(async move {
            // Append the flags the subprocess needs: defer its own sandbox
            // setup until the bridge sends roots/list_changed, and suppress
            // the interactive CLI output path.
            let mut args = base_args;
            if let Some(ref scope) = default_sandbox_scope {
                args.push("--sandbox-scope".to_string());
                args.push(scope.to_string_lossy().to_string());
            }
            args.push("--defer-sandbox".to_string());
            args.push("--server-child".to_string());

            let stderr_mode = if enable_colored_output {
                Stdio::piped()
            } else {
                Stdio::inherit()
            };

            let mut cmd = Command::new(&command);
            cmd.args(&args)
                // A subprocess peer is ALWAYS a server-child: it serves exactly one
                // bridge session and must never run the IDE-facing frontend path
                // (which spawns its own background bridge). We pass `--server-child`
                // in `args`, but that flag sits after the `stdio` subcommand and is
                // therefore fragile to parse. Set the internal env marker too — it is
                // the same mechanism `spawn_background_bridge` uses, and
                // `is_test_or_server_child()` honors it unconditionally. Without this,
                // a peer that fails to parse the flag mistakes itself for a frontend
                // and spawns a bridge, which spawns a peer, … — an unbounded
                // self-respawning chain of `ahma serve` processes (process-table
                // exhaustion). env vars inherit reliably across spawn; flags do not.
                .env("AHMA_SERVER_CHILD", "1")
                // Stamp the spawn-depth backstop so a runaway spawn chain through
                // peers self-limits (see ahma_common::process_guard).
                .env(
                    ahma_common::process_guard::SPAWN_DEPTH_ENV,
                    ahma_common::process_guard::child_spawn_depth(),
                )
                // Propagate W3C trace context so subprocess spans are linked
                // to the current session span (W3C Trace Context 1.0 §3.2).
                .env(
                    "TRACEPARENT",
                    ahma_common::observability::current_traceparent().unwrap_or_default(),
                )
                // SECURITY: prevent test environment leakage that would
                // auto-enable permissive sandbox modes in ahma_mcp, masking
                // real sandbox behaviour in integration tests.
                .env_remove("NEXTEST")
                .env_remove("NEXTEST_EXECUTION_MODE")
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("RUST_TEST_THREADS")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(stderr_mode)
                // Ensures the subprocess does not outlive this process when
                // a test exits early or a session is dropped unexpectedly.
                .kill_on_drop(true);

            // Stripping NEXTEST above must not strip endpoint isolation
            // (SPEC R-ISO.1): if this bridge is itself test-owned, the peer
            // must inherit that fact explicitly or it would resolve the
            // machine-global daemon endpoints from inside a test run.
            if ahma_common::test_isolation::spawned_under_test_harness() {
                cmd.env("AHMA_TEST_ISOLATION", "1");
            }

            let mut child = cmd
                .spawn()
                .map_err(|e| anyhow::anyhow!("Failed to spawn subprocess: {}", e))?;

            let stdin_handle = child
                .stdin
                .take()
                .expect("subprocess stdin was not piped — this is a bug");
            let stdout_handle = child
                .stdout
                .take()
                .expect("subprocess stdout was not piped — this is a bug");
            let stderr_opt: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>> =
                if enable_colored_output {
                    child.stderr.take().map(|s| {
                        Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>
                    })
                } else {
                    None
                };

            info!(command = %command, "Subprocess peer spawned for new bridge session");

            // A monitor task owns the child so its ExitStatus can be observed
            // and an abnormal (signal) death reported loudly (SPEC R-SIGN.5) —
            // previously death was visible only as pipe EOF, indistinguishable
            // from a clean exit. Explicit shutdown is requested over a oneshot;
            // dropping the shutdown closure without calling it also kills the
            // child (the receiver errors), preserving the old kill-on-drop
            // semantics for abandoned sessions.
            type KillAck = tokio::sync::oneshot::Sender<()>;
            let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<KillAck>();
            // The classified cause travels to the session so the client's
            // JSON-RPC error names it instead of a bare "session terminated"
            // (SPEC R-SIGN.5). Receiver dropped ⇒ send fails harmlessly.
            let (cause_tx, cause_rx) = tokio::sync::oneshot::channel::<String>();
            let command_for_log = command.clone();
            tokio::spawn(async move {
                tokio::select! {
                    status = child.wait() => match status {
                        Ok(status) => {
                            if let Some(cause) = report_peer_exit(&command_for_log, status) {
                                let _ = cause_tx.send(cause);
                            }
                        }
                        Err(e) => tracing::warn!(
                            command = %command_for_log,
                            "could not observe subprocess peer exit status: {e}"
                        ),
                    },
                    ack = kill_rx => {
                        // Ok: explicit session shutdown. Err: the shutdown
                        // closure was dropped un-called (abandoned session).
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        if let Ok(ack) = ack {
                            let _ = ack.send(());
                        }
                    }
                }
            });

            // Explicit shutdown: ask the monitor task to kill the child and
            // wait for its acknowledgement so termination stays synchronous.
            let shutdown_fn: PeerShutdownFn = Box::new(move || {
                Box::pin(async move {
                    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                    if kill_tx.send(ack_tx).is_ok() {
                        let _ = ack_rx.await;
                    }
                })
            });

            Ok(PeerStreams {
                stdin: Box::new(stdin_handle),
                stdout: Box::new(stdout_handle),
                stderr: stderr_opt,
                shutdown_fn: Some(shutdown_fn),
                exit_cause: Some(cause_rx),
            })
        })
    }
}

/// Log a peer subprocess's exit, loudly when it died by signal (SPEC R-SIGN.5),
/// and return the classified cause so it can also be surfaced to the client.
///
/// A signal death was previously indistinguishable from a clean exit (both
/// surface as pipe EOF), leaving nothing to explain a dead session. SIGKILL on
/// macOS gets the known likely cause spelled out: the kernel's code-signing
/// enforcement killing an ad-hoc-signed binary whose mapped pages were
/// invalidated by an in-place rebuild or evicted under memory pressure.
fn report_peer_exit(command: &str, status: std::process::ExitStatus) -> Option<String> {
    match describe_abnormal_exit(status) {
        Some(cause) => {
            tracing::error!(command = %command, "server subprocess died: {cause}");
            Some(cause)
        }
        None if status.success() => {
            tracing::debug!(command = %command, "server subprocess exited cleanly");
            None
        }
        None => {
            tracing::warn!(
                command = %command,
                "server subprocess exited with {status}"
            );
            // A non-zero exit code is abnormal enough to tell the client about.
            Some(format!("exited with {status}"))
        }
    }
}

/// Describe an abnormal (signal) exit, or `None` for a normal exit.
fn describe_abnormal_exit(status: std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let sig = status.signal()?;
        // SIGKILL is 9 on every Unix.
        if sig == 9 {
            let mac_hint = if cfg!(target_os = "macos") {
                " On macOS this is commonly the kernel's code-signing enforcement \
                 (SPEC R-SIGN): an ad-hoc-signed binary was rebuilt in place or its \
                 code pages were evicted under memory pressure and failed \
                 re-validation. Remediation: never overwrite a running binary \
                 (install via atomic rename, e.g. `ahma update`), and re-sign local \
                 builds with `codesign --force --sign - --options runtime`."
            } else {
                ""
            };
            return Some(format!(
                "killed by SIGKILL (possible OOM-kill or code-signing kill).{mac_hint}"
            ));
        }
        Some(format!("killed by signal {sig}"))
    }
    #[cfg(not(unix))]
    {
        // Windows has no signals; abnormal deaths appear as exit codes and are
        // reported by the non-success arm of `report_peer_exit`.
        let _ = status;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-SIGN.5: a SIGKILLed child must be classified as an abnormal death
    /// with the cause named, not mistaken for a normal exit.
    #[cfg(unix)]
    #[tokio::test]
    async fn describe_abnormal_exit_names_sigkill() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        child.start_kill().expect("send SIGKILL");
        let status = child.wait().await.expect("reap child");

        let desc = describe_abnormal_exit(status).expect("SIGKILL must classify as abnormal");
        assert!(desc.contains("SIGKILL"), "must name the signal: {desc}");
        #[cfg(target_os = "macos")]
        assert!(
            desc.contains("code-signing"),
            "macOS must include the R-SIGN cause and remediation: {desc}"
        );
    }

    /// A clean exit is not classified as abnormal.
    #[cfg(unix)]
    #[tokio::test]
    async fn describe_abnormal_exit_ignores_clean_exit() {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let status = child.wait().await.expect("reap child");
        assert!(status.success());
        assert!(describe_abnormal_exit(status).is_none());
    }

    #[test]
    fn subprocess_factory_stores_fields() {
        let factory = SubprocessPeerFactory::new("ahma", vec!["--log-to-stderr".into()], false);
        assert_eq!(factory.command, "ahma");
        assert_eq!(factory.args, &["--log-to-stderr"]);
        assert!(!factory.enable_colored_output);
    }

    #[test]
    fn subprocess_factory_clone() {
        let factory = SubprocessPeerFactory::new("ahma", vec!["--foo".into()], true);
        let cloned = factory.clone();
        assert_eq!(cloned.command, "ahma");
        assert_eq!(cloned.args, vec!["--foo"]);
        assert!(cloned.enable_colored_output);
    }

    /// Smoke-test that `create()` returns a future (does not panic at call time).
    /// We cannot actually spawn `ahma` in a unit test, but confirming the future
    /// is constructed without error is a useful sanity check.
    #[test]
    fn create_returns_a_future() {
        let factory = SubprocessPeerFactory::new("__nonexistent_cmd__", vec![], false);
        let _fut = factory.create(); // just constructing the future is enough
    }
}
