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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use ahma_common::config::ScopeAccess;
use ahma_common::scope_grant::{GrantCoordinator, GrantReason};

/// The directory to actually offer for a denied `path` (P1c).
///
/// Granting a single *file* is nearly useless — the next sibling file in the
/// same cache trips the kernel again and re-prompts. So when a denial names a
/// file, offer its **parent directory** instead, which covers the whole cache
/// in one grant. A directory (or a path that already looks like one) is offered
/// as-is. This only ever walks up to the *immediate* parent and never suggests a
/// filesystem root, and the result is still only a suggestion the human approves
/// at the prompt — so it cannot widen scope on its own.
pub(crate) fn grant_dir_for(path: &Path) -> PathBuf {
    let looks_like_file = if path.is_dir() {
        false
    } else if path.is_file() {
        true
    } else {
        // Nonexistent (e.g. a cache file about to be created): treat a final
        // component bearing an extension as a file.
        path.extension().is_some()
    };

    if looks_like_file
        && let Some(parent) = path.parent()
        // `parent.parent().is_some()` is false only for a filesystem root, so
        // this refuses to ever suggest `/` (or a bare drive root) as a grant.
        && parent.parent().is_some()
    {
        return parent.to_path_buf();
    }
    path.to_path_buf()
}

/// Agent-facing remediation for a *runtime* sandbox denial (a sandboxed command
/// exited non-zero because the kernel blocked an out-of-scope access). Describes
/// the supported grant -> restart -> retry loop using the MCP tools, so the sync
/// error payload and the async operation alert phrase the recovery identically.
///
/// References `grant_dir_for` so the suggested grant path matches what the
/// approval prompt offers (a file's parent directory, so one grant covers the
/// whole cache rather than re-prompting per file).
pub fn runtime_denial_remediation(path: &Path, access: ScopeAccess) -> String {
    let target = grant_dir_for(path);
    let access_str = if access.is_write() { "rw" } else { "ro" };
    let verb = if access.is_write() { "write" } else { "read" };
    format!(
        "ahma's kernel sandbox blocked an out-of-scope {verb} to '{denied}'. This is expected: \
         writing outside the workspace (for example installing a global binary under ~/.cargo) is \
         denied by default. To allow it, call the `sandbox_grant` tool with path \"{target}\" and \
         access \"{access_str}\" (it previews and asks the human to approve), then run the \
         `restart` tool to apply the grant, then re-run the command. No flags are required.",
        verb = verb,
        denied = path.display(),
        target = target.display(),
        access_str = access_str,
    )
}

/// CLI-oriented variant of [`runtime_denial_remediation`] for contexts where the
/// MCP `sandbox_grant`/`restart` tools are not in play — notably the shell hook
/// running in the editor's *native* terminal. Points at `ahma sandbox grant`.
pub fn runtime_denial_remediation_cli(path: &Path, access: ScopeAccess) -> String {
    let target = grant_dir_for(path);
    let ro_flag = if access.is_write() {
        ""
    } else {
        " --read-only"
    };
    let verb = if access.is_write() { "write" } else { "read" };
    format!(
        "ahma's kernel sandbox blocked an out-of-scope {verb} to '{denied}'. This is expected: \
         writing outside the workspace (for example installing a global binary under ~/.cargo) is \
         denied by default. To allow it, run `ahma sandbox grant {target}{ro_flag}` — the grant \
         is validated against a denylist, persisted to ~/.ahma/settings.toml, and takes effect \
         on the next server start — then re-run the command.",
        verb = verb,
        denied = path.display(),
        target = target.display(),
        ro_flag = ro_flag,
    )
}

/// Shared actionable tail for the "blocked out-of-scope path" log lines: how to
/// allow it and how to make the grant take effect.
fn grant_hint(path: &Path, access: ScopeAccess) -> String {
    let ro_flag = if access == ScopeAccess::Ro {
        " --read-only"
    } else {
        ""
    };
    format!(
        "run `ahma sandbox grant {}{}`. The grant is saved to settings — restart the \
         bridge (the `restart` tool) to apply it now, otherwise it takes effect on the \
         next server start.",
        path.display(),
        ro_flag,
    )
}

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
            tracing::warn!(
                path = %req.path.display(),
                access = req.access.label(),
                "Sandbox blocked an out-of-scope path. To allow it, {}",
                grant_hint(&req.path, req.access),
            );
        }
    }
}

/// A notifier that forwards each de-duplicated grant request to the daemon hub
/// (for a connected TUI to show as a modal) **and** logs it (so it stays
/// observable even when no TUI is attached). The shared [`GrantCoordinator`] is
/// the same instance the daemon reporter uses to resolve the answer, so dedup,
/// first-answer-wins, and dismiss all coordinate across the request and the reply.
#[derive(Debug)]
pub struct HubGrantNotifier {
    coordinator: Arc<GrantCoordinator>,
    req_tx: tokio::sync::mpsc::UnboundedSender<ahma_common::scope_grant::ScopeGrantRequest>,
}

impl HubGrantNotifier {
    /// Create a hub-delivering notifier sharing `coordinator`, sending fresh
    /// requests on `req_tx` (drained by the daemon reporter and forwarded to the
    /// hub as `ClientMsg::Relay(HubRelay::ScopeGrantRequested)`).
    pub fn new(
        coordinator: Arc<GrantCoordinator>,
        req_tx: tokio::sync::mpsc::UnboundedSender<ahma_common::scope_grant::ScopeGrantRequest>,
    ) -> Self {
        Self {
            coordinator,
            req_tx,
        }
    }
}

#[async_trait]
impl ScopeGrantNotifier for HubGrantNotifier {
    async fn notify_violation(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    ) {
        let Some(req) = self.coordinator.begin(path, access, reason, tool) else {
            return;
        };
        // Log unconditionally so the violation is visible even with no TUI attached.
        tracing::warn!(
            path = %req.path.display(),
            access = req.access.label(),
            "Sandbox blocked an out-of-scope path. Approve the prompt, or {}",
            grant_hint(&req.path, req.access),
        );
        // Deliver to the hub; if the channel is closed (no reporter yet) the log
        // above is the fallback.
        let _ = self.req_tx.send(req);
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
        // Offer the enclosing directory when the argument is a file (P1c), so one
        // grant covers it and its siblings.
        let target = grant_dir_for(path);
        n.notify_violation(
            &target,
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
    stdout: &str,
    tool: &str,
) {
    notify_stderr_denial_in_dir(sandbox, notifier, stderr, stdout, tool, None).await;
}

/// Variant of [`notify_stderr_denial`] that resolves relative candidate paths
/// against the command's actual working directory.
pub async fn notify_stderr_denial_in_dir(
    sandbox: &super::Sandbox,
    notifier: Option<&Arc<dyn ScopeGrantNotifier>>,
    stderr: &str,
    stdout: &str,
    tool: &str,
    working_dir: Option<&Path>,
) {
    let Some(n) = notifier else { return };
    // stdout too: a merged pipeline (`… 2>&1 | tail`) leaves stderr empty, and a
    // denial that disappears when a caller adds `2>&1` is a trap, not a feature.
    let Some(hit) = super::denial_scan::scan_denial_streams(stderr, stdout) else {
        return;
    };
    let (in_scope, target) = {
        let scopes_guard = sandbox.scopes();
        let base_wd = working_dir.or_else(|| scopes_guard.first().map(|p| p.as_path()));
        let in_scope = if let Some(wd) = base_wd {
            sandbox.is_path_in_scope_in_dir(&hit.path, wd)
        } else {
            sandbox.is_path_in_scope(&hit.path)
        };
        let target = if !in_scope {
            resolve_grant_target(&hit.path, base_wd, sandbox)
        } else {
            PathBuf::new()
        };
        (in_scope, target)
    };
    if in_scope {
        return;
    }
    // The denial usually names a single cache file; offer its parent directory so
    // one grant covers the whole cache rather than re-prompting per file (P1c).
    // When the path was reached through a symlink to an out-of-scope tree (e.g. a
    // symlinked `target/` directory), offer the canonical external target root.
    n.notify_violation(
        &target,
        hit.access,
        GrantReason::StderrHeuristic,
        Some(tool.to_string()),
    )
    .await;
}

/// Resolve the candidate path from a denial into the directory that should actually
/// be offered for a grant.
///
/// Follows symlinks within the workspace (e.g. `target -> /shared/target`) to
/// recommend granting the external target root directly rather than an individual
/// non-existent leaf.
pub fn resolve_grant_target(
    path: &Path,
    working_dir: Option<&Path>,
    sandbox: &super::Sandbox,
) -> PathBuf {
    let scopes_guard = sandbox.scopes();
    let is_rooted = path.is_absolute() || path.has_root();
    let full_path = if is_rooted {
        path.to_path_buf()
    } else if let Some(wd) = working_dir {
        wd.join(path)
    } else if let Some(first_scope) = scopes_guard.first() {
        first_scope.join(path)
    } else {
        path.to_path_buf()
    };

    // Check if any ancestor (from full_path up to root) is a symlink pointing outside scope
    let mut current = full_path.as_path();
    while let Some(parent) = current.parent() {
        if parent.parent().is_none() {
            break;
        }
        if let Ok(meta) = std::fs::symlink_metadata(current)
            && meta.file_type().is_symlink()
            && let Ok(canon) = dunce::canonicalize(current)
            && !sandbox.is_path_allowed(&canon, &scopes_guard)
        {
            return canon;
        }
        current = parent;
    }

    if is_rooted {
        grant_dir_for(path)
    } else {
        grant_dir_for(&full_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::path::PathBuf;

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
                self.seen.lock().push((req.path, req.access, req.reason));
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
        let seen = rec.seen.lock();
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
        assert!(rec.seen.lock().is_empty());
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
        notify_stderr_denial(&sandbox, Some(&notifier), stderr, "", "sccache").await;

        let seen = rec.seen.lock();
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
        notify_stderr_denial(&sandbox, Some(&notifier), &stderr, "", "cat").await;
        assert!(
            rec.seen.lock().is_empty(),
            "in-scope denials must not raise a grant prompt"
        );
    }

    #[tokio::test]
    async fn hub_notifier_emits_once_per_key_and_carries_fields() {
        let coordinator = Arc::new(GrantCoordinator::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let notifier = HubGrantNotifier::new(coordinator, tx);
        let p = Path::new("/opt/ext/cache");

        notifier
            .notify_violation(
                p,
                ScopeAccess::Rw,
                GrantReason::StderrHeuristic,
                Some("sccache".into()),
            )
            .await;
        // Same (path, access) again — dedup must suppress a second emission.
        notifier
            .notify_violation(
                p,
                ScopeAccess::Rw,
                GrantReason::StderrHeuristic,
                Some("sccache".into()),
            )
            .await;

        let req = rx
            .try_recv()
            .expect("first violation is delivered to the hub channel");
        assert_eq!(req.access, ScopeAccess::Rw);
        assert_eq!(req.reason, GrantReason::StderrHeuristic);
        assert_eq!(req.tool.as_deref(), Some("sccache"));
        assert!(!req.decision_id.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "the duplicate violation is deduped, not re-emitted"
        );
    }

    #[tokio::test]
    async fn no_notifier_is_a_noop() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        // Simply must not panic with notifier = None.
        notify_stderr_denial(&sandbox, None, "/x: Permission denied", "", "t").await;
        let err = anyhow::anyhow!("x");
        notify_pre_exec(None, &err, "t").await;
    }

    // ── P1c: parent-directory suggestion ──────────────────────────────────────

    #[test]
    fn grant_dir_for_existing_file_returns_parent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.bin");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(grant_dir_for(&file), dir.path());
    }

    #[test]
    fn grant_dir_for_existing_dir_returns_self() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(grant_dir_for(dir.path()), dir.path());
    }

    #[test]
    fn grant_dir_for_nonexistent_file_like_path_returns_parent() {
        // A cache file about to be created: extension ⇒ treat as file.
        let p = Path::new("/home/u/.cache/sccache/0/abc.o");
        assert_eq!(grant_dir_for(p), Path::new("/home/u/.cache/sccache/0"));
    }

    #[test]
    fn grant_dir_for_nonexistent_dir_like_path_returns_self() {
        // No extension ⇒ treat as a directory to create; offer it directly.
        let p = Path::new("/home/u/.cache/sccache/shard0");
        assert_eq!(grant_dir_for(p), p);
    }

    #[test]
    fn grant_dir_for_never_suggests_filesystem_root() {
        // A file directly under root must not collapse the suggestion to `/`.
        let p = Path::new("/lonely.bin");
        assert_eq!(grant_dir_for(p), p);
    }

    #[tokio::test]
    async fn stderr_denial_offers_parent_dir_for_a_file() {
        let scope = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox(scope.path());
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();
        // A write denial naming a specific out-of-scope cache *file*.
        let stderr =
            "error writing `/opt/ext/sccache/0/object.o`: Operation not permitted (os error 1)";
        notify_stderr_denial(&sandbox, Some(&notifier), stderr, "", "sccache").await;

        let seen = rec.seen.lock();
        assert_eq!(
            seen[0].0,
            PathBuf::from("/opt/ext/sccache/0"),
            "the cache file's parent dir is offered, not the leaf file"
        );
    }

    #[tokio::test]
    async fn stderr_denial_symlinked_target_notifies_external_target_dir() {
        let ws = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();

        let ws_canon = dunce::canonicalize(ws.path()).unwrap();
        let ext_canon = dunce::canonicalize(ext.path()).unwrap();

        let target_symlink = ws_canon.join("target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ext_canon, &target_symlink).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&ext_canon, &target_symlink).unwrap();

        let sandbox = test_sandbox(&ws_canon);
        let rec = Arc::new(RecordingNotifier::default());
        let notifier: Arc<dyn ScopeGrantNotifier> = rec.clone();

        let stderr = "\
error: failed to create directory 'target/debug'

Caused by:
  Operation not permitted (os error 1)";

        notify_stderr_denial(&sandbox, Some(&notifier), stderr, "", "cargo").await;

        let seen = rec.seen.lock();
        assert_eq!(seen.len(), 1, "out-of-scope symlink denial must notify");
        assert_eq!(seen[0].0, ext_canon);
        assert_eq!(seen[0].1, ScopeAccess::Rw);
        assert_eq!(seen[0].2, GrantReason::StderrHeuristic);
    }
}
