//! The channel from a detected scope violation to an approval surface.
//!
//! Detection (pre-execution path validation, or a stderr denial heuristic) produces
//! an offending `(path, access)`. A [`ScopeGrantNotifier`] decides whether to raise
//! a "grant access to X?" prompt and delivers it to a surface (TUI modal, MCP
//! `elicitation/create`). Dedup/debounce is the notifier's responsibility, owned by
//! the shared [`GrantCoordinator`] so the same path — which trips the kernel many
//! times — is only asked about once per session.
//!
//! This module also holds the two free wiring helpers the adapter calls so the
//! logic is unit-testable without constructing a full `Adapter`:
//! [`notify_pre_exec`] (a `PathOutsideSandbox` was returned up front) and
//! [`notify_stderr_denial`] (a sandboxed command failed and its stderr matched a
//! denial signature).
//!
//! PR boundary: this PR ships the trait + a [`LoggingGrantNotifier`] stub so
//! detection is observable in logs before any UI exists. The TUI and MCP surfaces
//! plug real notifiers into the same trait in later PRs.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use ahma_common::config::ScopeAccess;
use ahma_common::scope_grant::{GrantCoordinator, GrantReason};

/// Delivers a scope-grant request to a human approval surface. Implementors own
/// (a clone of) the shared [`GrantCoordinator`] and call
/// [`GrantCoordinator::begin`] to dedup before delivering.
#[async_trait]
pub trait ScopeGrantNotifier: Send + Sync + std::fmt::Debug {
    /// Consider raising a grant prompt for `path` at `access`. A no-op if the
    /// `(canonical_path, access)` was already asked or dismissed this session.
    async fn notify_violation(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    );
}

/// The PR-boundary stub: instead of a UI, log an actionable line (once per
/// `(path, access)`, via the coordinator) telling the human exactly how to grant
/// the path. Replaced by the TUI / MCP notifiers in later PRs.
#[derive(Debug)]
pub struct LoggingGrantNotifier {
    coordinator: Arc<GrantCoordinator>,
}

impl LoggingGrantNotifier {
    /// Create a logging notifier sharing `coordinator` (the session's single
    /// [`GrantCoordinator`]).
    pub fn new(coordinator: Arc<GrantCoordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait]
impl ScopeGrantNotifier for LoggingGrantNotifier {
    async fn notify_violation(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    ) {
        if let Some(req) = self.coordinator.begin(path, access, reason, tool) {
            let ro_flag = if req.access == ScopeAccess::Ro {
                " --read-only"
            } else {
                ""
            };
            tracing::warn!(
                path = %req.path.display(),
                access = req.access.label(),
                "Sandbox blocked an out-of-scope path. To allow it, run \
                 `ahma sandbox grant {}{}` — takes effect on the next server start.",
                req.path.display(),
                ro_flag,
            );
        }
    }
}

/// Wiring helper: a `PathOutsideSandbox` was returned by path validation up front,
/// so the offending path is known exactly. Offers it as a read+write grant (a
/// working directory / path argument is used for both). A no-op when there is no
/// notifier or the error is a different `SandboxError`.
pub async fn notify_pre_exec(
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    err: &anyhow::Error,
    tool: &str,
) {
    let Some(n) = notifier else { return };
    if let Some(super::SandboxError::PathOutsideSandbox { path, .. }) =
        err.downcast_ref::<super::SandboxError>()
    {
        n.notify_violation(
            path,
            ScopeAccess::Rw,
            GrantReason::PreExecViolation,
            Some(tool.to_string()),
        )
        .await;
    }
}

/// Wiring helper: a sandboxed command failed; scan its stderr for a denial and, if
/// the referenced path is genuinely out of scope, offer to grant it. A denial for a
/// path already in scope (e.g. a root-owned file inside the workspace) is unrelated
/// to the sandbox and is ignored.
pub async fn notify_stderr_denial(
    sandbox: &super::Sandbox,
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    stderr: &str,
    tool: &str,
) {
    let Some(n) = notifier else { return };
    let Some(hit) = super::denial_scan::scan_denial(stderr) else {
        return;
    };
    if sandbox.is_path_in_scope(&hit.path) {
        return;
    }
    n.notify_violation(
        &hit.path,
        hit.access,
        GrantReason::StderrHeuristic,
        Some(tool.to_string()),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Records every delivered violation so wiring can be asserted.
    #[derive(Debug, Default)]
    struct RecordingNotifier {
        coordinator: Arc<GrantCoordinator>,
        seen: Mutex<Vec<(PathBuf, ScopeAccess, GrantReason)>>,
    }

    #[async_trait]
    impl ScopeGrantNotifier for RecordingNotifier {
        async fn notify_violation(
            &self,
            path: &Path,
            access: ScopeAccess,
            reason: GrantReason,
            tool: Option<String>,
        ) {
            // Exercise the same dedup the real notifiers use.
            if let Some(req) = self.coordinator.begin(path, access, reason, tool) {
                self.seen
                    .lock()
                    .unwrap()
                    .push((req.path, req.access, req.reason));
            }
        }
    }

    fn test_sandbox(scope: &Path) -> super::super::Sandbox {
        super::super::Sandbox::new(
            vec![scope.to_path_buf()],
            super::super::SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pre_exec_notifies_with_offending_path_as_rw() {
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        let err: anyhow::Error = super::super::SandboxError::PathOutsideSandbox {
            path: PathBuf::from("/out/of/scope/dir"),
            scopes: vec![PathBuf::from("/ws")],
        }
        .into();
        notify_pre_exec(Some(&notifier), &err, "run_terminal_command").await;
        let seen = rec.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, PathBuf::from("/out/of/scope/dir"));
        assert_eq!(seen[0].1, ScopeAccess::Rw);
        assert_eq!(seen[0].2, GrantReason::PreExecViolation);
    }

    #[tokio::test]
    async fn pre_exec_ignores_unrelated_errors() {
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        let err = anyhow::anyhow!("some unrelated failure");
        notify_pre_exec(Some(&notifier), &err, "tool").await;
        assert!(rec.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stderr_denial_out_of_scope_notifies_and_preserves_scopes() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let before: Vec<PathBuf> = sandbox.scopes().to_vec();

        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        let stderr =
            "error: failed to create directory `/opt/out/of/scope/cache`: Read-only file system";
        notify_stderr_denial(&sandbox, Some(&notifier), stderr, "sccache").await;

        let seen = rec.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "an out-of-scope denial is offered");
        assert_eq!(seen[0].0, PathBuf::from("/opt/out/of/scope/cache"));
        assert_eq!(seen[0].1, ScopeAccess::Rw);
        assert_eq!(seen[0].2, GrantReason::StderrHeuristic);

        // Hard-constraint guard: detection must NEVER widen the live sandbox.
        assert_eq!(
            sandbox.scopes().to_vec(),
            before,
            "scanning stderr must not mutate live scopes"
        );
    }

    #[tokio::test]
    async fn stderr_denial_for_in_scope_path_is_ignored() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        // A permission error for a path that is already inside the scope is not a
        // scope problem — do not offer to grant it.
        let in_scope = scope.path().join("locked.txt");
        let stderr = format!("cat: {}: Permission denied", in_scope.display());
        notify_stderr_denial(&sandbox, Some(&notifier), &stderr, "cat").await;
        assert!(
            rec.seen.lock().unwrap().is_empty(),
            "in-scope denials must not raise a grant prompt"
        );
    }

    #[tokio::test]
    async fn no_notifier_is_a_noop() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        // Simply must not panic with notifier = None.
        notify_stderr_denial(&sandbox, None, "/x: Permission denied", "t").await;
        let err = anyhow::anyhow!("x");
        notify_pre_exec(None, &err, "t").await;
    }
}
