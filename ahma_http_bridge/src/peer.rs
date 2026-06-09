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
}

impl SubprocessPeerFactory {
    /// Construct a new factory.
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        enable_colored_output: bool,
    ) -> Self {
        Self {
            command: command.into(),
            args,
            enable_colored_output,
        }
    }
}

impl PeerFactory for SubprocessPeerFactory {
    fn create(&self) -> BoxFuture<anyhow::Result<PeerStreams>> {
        let command = self.command.clone();
        let base_args = self.args.clone();
        let enable_colored_output = self.enable_colored_output;

        Box::pin(async move {
            // Append the flags the subprocess needs: defer its own sandbox
            // setup until the bridge sends roots/list_changed, and suppress
            // the interactive CLI output path.
            let mut args = base_args;
            args.push("--defer-sandbox".to_string());
            args.push("--server-child".to_string());

            let stderr_mode = if enable_colored_output {
                Stdio::piped()
            } else {
                Stdio::inherit()
            };

            let mut child = Command::new(&command)
                .args(&args)
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
                .kill_on_drop(true)
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
                    child
                        .stderr
                        .take()
                        .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>)
                } else {
                    None
                };

            info!(command = %command, "Subprocess peer spawned for new bridge session");

            // Explicit shutdown: kill the child on session termination.
            let shutdown_fn: PeerShutdownFn = Box::new(move || {
                Box::pin(async move {
                    let _ = child.kill().await;
                })
            });

            Ok(PeerStreams {
                stdin: Box::new(stdin_handle),
                stdout: Box::new(stdout_handle),
                stderr: stderr_opt,
                shutdown_fn: Some(shutdown_fn),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprocess_factory_stores_fields() {
        let factory =
            SubprocessPeerFactory::new("ahma", vec!["--log-to-stderr".into()], false);
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
