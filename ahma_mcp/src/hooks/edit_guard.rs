//! `ahma hooks edit-guard` — the harness side of SPEC R2.7.8.
//!
//! A harness's native file tools (Claude Code's `Edit`, `Write`, `MultiEdit`,
//! `NotebookEdit`) never pass through ahma, so the workspace write queue cannot
//! order them. Claude Code does run `PreToolUse` hooks for them, though, and
//! this is that hook: it reads the hook payload on stdin, asks whether an ahma
//! command that may read or write the same workspace is running right now, and
//! if so denies the edit with a reason naming that command — the same refusal
//! ahma's own file tools give.
//!
//! Opt-in, installed by hand (docs/workspace-queue.md). It never waits and
//! never takes the lease: an edit is refused, not queued, because a harness
//! hook that blocks is killed by its timeout and the model learns nothing.
//!
//! **Undecided unless refusing** (R5.5.5): when the workspace is free, or the
//! payload is not one this hook understands, it prints nothing and exits 0 —
//! the harness's own permission flow runs as if the hook were not there. It
//! never emits an explicit allow.

use crate::adapter::workspace_queue::{
    LeaseProbe, WorkspaceQueue, edit_refusal, workspace_key_for_path,
};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The file a `PreToolUse` payload is about to edit, absolute.
fn target_path(payload: &Value) -> Option<PathBuf> {
    let input = payload.get("tool_input")?;
    let raw = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)?;
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Some(path);
    }
    let cwd = payload.get("cwd").and_then(Value::as_str)?;
    Some(Path::new(cwd).join(path))
}

/// The hook's stdout for `payload`: a deny decision, or `None` for "no opinion".
pub fn decide(payload: &Value, queue: &WorkspaceQueue) -> Option<Value> {
    let path = target_path(payload)?;
    let key = workspace_key_for_path(&path, &[]);
    let LeaseProbe::Held { holder } = queue.probe(&key) else {
        return None;
    };
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": edit_refusal(&path, &key, holder.as_ref()),
        }
    }))
}

/// Entry point: read the payload from stdin, print a decision if there is one.
/// Fails open — any error is "no opinion" — so ahma can never wedge an editor.
pub fn run(enabled: bool) -> anyhow::Result<()> {
    if !enabled {
        return Ok(());
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        return Ok(());
    };
    if let Some(decision) = decide(&payload, &WorkspaceQueue::new(true)) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{decision}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::workspace_queue::HolderInfo;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn payload(file: &Path) -> Value {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "cwd": "/",
            "tool_input": { "file_path": file.to_string_lossy() }
        })
    }

    #[tokio::test]
    async fn an_edit_in_a_busy_workspace_is_denied_naming_the_command() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let key = dunce::canonicalize(&repo).unwrap();

        assert!(decide(&payload(&repo.join("src/lib.rs")), &queue).is_none());

        let _lease = queue
            .enqueue(&key, HolderInfo::new("op_3", "cargo nextest run"))
            .unwrap()
            .acquire(&CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let decision = decide(&payload(&repo.join("src/lib.rs")), &queue).expect("deny");
        let out = &decision["hookSpecificOutput"];
        assert_eq!(out["permissionDecision"], "deny");
        let reason = out["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.contains("cargo nextest run"), "{reason}");
        assert!(reason.contains("`await`"), "{reason}");
    }

    #[test]
    fn a_payload_without_a_path_has_no_opinion() {
        let queue = WorkspaceQueue::disabled();
        assert!(decide(&json!({"tool_name": "Edit", "tool_input": {}}), &queue).is_none());
        assert!(decide(&json!("garbage"), &queue).is_none());
    }

    #[test]
    fn a_relative_path_is_resolved_against_cwd() {
        let p = target_path(&json!({"cwd": "/work", "tool_input": {"file_path": "a/b.rs"}}));
        assert_eq!(p, Some(PathBuf::from("/work/a/b.rs")));
        let n = target_path(&json!({"tool_input": {"notebook_path": "/n.ipynb"}}));
        assert_eq!(n, Some(PathBuf::from("/n.ipynb")));
    }
}
