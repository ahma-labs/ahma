//! `ahma hooks edit-guard` — the harness side of SPEC R2.7.8.
//!
//! A harness's native file tools (Claude Code's `Edit`, Codex's `apply_patch`,
//! Copilot's `edit`/`create`, Cursor's `Write`, Antigravity's
//! `write_to_file`, VS Code's edit tools) never pass through ahma, so the
//! workspace write queue cannot order them. Every one of those harnesses runs a
//! pre-tool hook, though, and this is that hook: it reads the hook payload on
//! stdin, finds the file(s) the edit would touch, asks whether an ahma command
//! that may read or write the same workspace is running right now, and if so
//! denies the edit with a reason naming that command — the same refusal ahma's
//! own file tools give.
//!
//! It also enforces the sandbox *scope* on those same edits (SPEC R5.5.6): a
//! native `Write`/`Edit` never passes through the shell sandbox, so without this
//! check an agent whose shell is confined to one repository can still create
//! files anywhere on the machine — which is exactly how one session cloned a
//! second checkout next to its own. An edit whose target lies outside the hook's
//! sandbox scope (the enclosing repository, plus the persistent `rw` grants in
//! `~/.ahma/settings.toml` and the temp directory) is refused with the same
//! "ask the human to grant it" remediation the shell hook gives.
//!
//! Installed by default with every shell hook (`ahma hooks install`; decline
//! with `--no-edit-guard`), under its own managed id, so `uninstall` and
//! `status` treat it separately. It never waits and never takes the lease: an
//! edit is refused, not queued, because a hook that blocks is killed by its
//! timeout and the model learns nothing.
//!
//! **Refuse, or say nothing** (R5.5.5): when the workspace is free, or the
//! payload is not an edit this hook understands, it takes no position — empty
//! output for the harnesses whose contract defines that as "no opinion"
//! (Claude Code, Codex, Copilot, VS Code), and the same plain `allow` ahma's
//! shell hook already emits for Cursor and Antigravity, whose undecided
//! outcome is not verified (see `build_cursor_hook_output`). It always exits 0:
//! Copilot treats any non-zero exit of a `preToolUse` hook as a deny.

use super::{
    BinaryReference, HOOK_TIMEOUT_SECS, HookEnvironment, HookPlatform, HookScope,
    PATH_LOOKUP_BINARY, build_windows_absolute_command, ensure_child_array, ensure_child_object,
    ensure_root_object, remove_managed_hook_entries, shell_quote_posix,
};
use crate::adapter::workspace_queue::WorkspaceQueue;
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Marks every hook entry this module writes, and nothing else.
pub(super) const MANAGED_ID_EDIT_GUARD_V1: &str = "ahma-edit-guard-v1";

/// Arguments of `ahma hooks edit-guard`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct HooksEditGuardArgs {
    /// Which harness is calling, so the decision is written in its format. Omitted: inferred from the payload's shape (Claude Code / VS Code format unless it carries Copilot's `toolName`).
    #[arg(long, value_enum)]
    pub platform: Option<HookPlatform>,
    /// Marks the hook entry as managed by ahma, so uninstall and status can find it.
    #[arg(long, hide = true, default_value = MANAGED_ID_EDIT_GUARD_V1)]
    pub managed_id: String,
}

// ---------------------------------------------------------------------------
// What an edit touches
// ---------------------------------------------------------------------------

/// Tool names that edit files, across harnesses (lower-cased).
const EDIT_TOOLS: &[&str] = &[
    // Claude Code
    "edit",
    "write",
    "multiedit",
    "notebookedit",
    // Codex
    "apply_patch",
    // Copilot CLI
    "create",
    "str_replace_editor",
    "str_replace",
    // Cursor
    "delete",
    // Antigravity
    "write_to_file",
    "replace_file_content",
    "multi_replace_file_content",
    // VS Code (Local agent)
    "editfiles",
    "create_file",
    "replace_string_in_file",
    "multi_replace_string_in_file",
    "insert_edit_into_file",
    "edit_notebook_file",
    "edit_file",
];

/// Whether `name` is a file-editing tool. A harness that ignores matchers (VS
/// Code's Local agent runs every `PreToolUse` hook for every tool) sends this
/// hook reads and shell calls too; those must pass untouched.
pub fn is_edit_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if EDIT_TOOLS.contains(&n.as_str()) {
        return true;
    }
    const READS: &[&str] = &[
        "read", "view", "list", "search", "grep", "find", "glob", "fetch", "terminal", "bash",
        "shell", "command", "todo",
    ];
    const WRITES: &[&str] = &["edit", "write", "replace", "patch", "insert", "create_file"];
    WRITES.iter().any(|w| n.contains(w)) && !READS.iter().any(|r| n.contains(r))
}

/// The tool name in a payload: Claude/Codex/Cursor/Antigravity/VS Code
/// `tool_name`, Copilot `toolName`.
fn tool_name(payload: &Value) -> Option<&str> {
    payload
        .get("tool_name")
        .or_else(|| payload.get("toolName"))
        .and_then(Value::as_str)
}

/// The tool arguments: `tool_input` or Copilot's `toolArgs` (an object, or a
/// JSON string holding one).
fn tool_input(payload: &Value) -> Option<Value> {
    let raw = payload
        .get("tool_input")
        .or_else(|| payload.get("toolArgs"))
        .or_else(|| payload.get("toolInput"))?;
    match raw {
        Value::String(s) => serde_json::from_str(s).ok(),
        other => Some(other.clone()),
    }
}

/// Keys under which harnesses name the file an edit targets.
const PATH_KEYS: &[&str] = &[
    "file_path",
    "filePath",
    "path",
    "notebook_path",
    "TargetFile",
    "targetFile",
    "target_file",
    "AbsolutePath",
    "file",
];

/// Every file the edit would touch, as written in the payload (possibly relative).
pub fn edited_paths(input: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_paths(input, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for key in PATH_KEYS {
                if let Some(p) = map.get(*key).and_then(Value::as_str) {
                    out.push(p.to_string());
                }
            }
            // Lists of edits (`files`, `replacements`, `edits`) and patch
            // bodies (Codex `apply_patch`, under whatever key carries them).
            for (key, v) in map {
                match v {
                    Value::Array(items) => {
                        for item in items {
                            if let Some(s) = item.as_str() {
                                if key == "files" {
                                    out.push(s.to_string());
                                } else {
                                    // `["apply_patch", "*** Begin Patch…"]`
                                    out.extend(patch_paths(s));
                                }
                            } else {
                                collect_paths(item, out);
                            }
                        }
                    }
                    Value::String(s) => out.extend(patch_paths(s)),
                    Value::Object(_) => collect_paths(v, out),
                    _ => {}
                }
            }
        }
        Value::String(s) => out.extend(patch_paths(s)),
        _ => {}
    }
}

/// The files named in a Codex-format patch (`*** Update File: …`). Only a
/// string that *is* a patch counts: a file's content that merely quotes a
/// patch header (documentation about Codex, say) names no edited path.
fn patch_paths(text: &str) -> Vec<String> {
    if !text.trim_start().starts_with("*** Begin Patch") {
        return Vec::new();
    }
    const HEADERS: &[&str] = &[
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            HEADERS
                .iter()
                .find_map(|h| line.strip_prefix(h))
                .map(|p| p.trim().to_string())
        })
        .filter(|p| !p.is_empty())
        .collect()
}

fn absolute(path: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return Some(p);
    }
    cwd.map(|c| Path::new(c).join(p))
}

// ---------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------

/// Why the edit is refused, if it is: the first touched path outside the
/// sandbox scope (when `allowed` is given), else the first touched workspace
/// that is busy.
///
/// `allowed = None` disables the scope check (the caller has no scope to
/// enforce — only tests do that); `Some(&[])` refuses every edit.
pub fn refusal(
    payload: &Value,
    queue: &WorkspaceQueue,
    allowed: Option<&[PathBuf]>,
) -> Option<String> {
    if let Some(name) = tool_name(payload)
        && !is_edit_tool(name)
    {
        return None;
    }
    let input = tool_input(payload)?;
    let cwd = payload.get("cwd").and_then(Value::as_str);
    let paths: Vec<PathBuf> = edited_paths(&input)
        .iter()
        .filter_map(|raw| absolute(raw, cwd))
        .collect();
    if let Some(allowed) = allowed {
        for path in &paths {
            if !within_scope(path, allowed) {
                return Some(scope_refusal(path, allowed));
            }
        }
    }
    paths.iter().find_map(|path| queue.edit_conflict(path, &[]))
}

/// Canonicalize a path that may not exist yet: resolve the deepest existing
/// ancestor through the filesystem (so a symlink out of the scope is seen for
/// what it is), then append the remaining components lexically.
pub fn canonical_for_scope(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = dunce::canonicalize(&existing) {
            let mut out = canon;
            for c in tail.iter().rev() {
                if c == ".." {
                    out.pop();
                } else if c != "." {
                    out.push(c);
                }
            }
            return out;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Whether `path` (canonicalized as far as it exists) lies inside one of the
/// `allowed` scopes.
fn within_scope(path: &Path, allowed: &[PathBuf]) -> bool {
    let canon = canonical_for_scope(path);
    allowed
        .iter()
        .any(|scope| canon.starts_with(canonical_for_scope(scope)))
}

/// The refusal for an edit outside the sandbox scope (SPEC R5.5.6): names the
/// path, the scope, and the only way to widen it — a human.
fn scope_refusal(path: &Path, allowed: &[PathBuf]) -> String {
    let dir = path.parent().unwrap_or(path);
    let scopes = allowed
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Edit refused: {} is outside the sandbox scope ({}). Native file edits are confined to \
         the same directories as sandboxed shell commands. To allow it, ask the human — they \
         approve it in the ahma TUI or run `ahma sandbox grant {}`. You cannot widen the \
         scope yourself.",
        path.display(),
        if scopes.is_empty() {
            "none".to_string()
        } else {
            scopes
        },
        dir.display()
    )
}

/// The directories a native edit may touch: the hook sandbox scope for `cwd`
/// (the enclosing repository, SPEC R5.2.1), every persistent `rw` grant, and
/// the temp directory — the same set a hooked shell command may write.
pub fn allowed_edit_scopes(
    cwd: &Path,
    persistent: &[ahma_common::config::PersistentScope],
) -> Vec<PathBuf> {
    let mut allowed = super::resolve_hook_sandbox_scopes(cwd);
    // SPEC R5.5.7: the harness's own working set needs no grant.
    allowed.extend(harness_owned_dirs_for_this_user());
    for scope in persistent {
        if scope.access.is_write() {
            let raw = ahma_common::config::expand_home(&scope.path);
            allowed.push(dunce::canonicalize(&raw).unwrap_or(raw));
        }
    }
    let tmp = std::env::temp_dir();
    allowed.push(dunce::canonicalize(&tmp).unwrap_or(tmp));
    allowed
}

/// Directories a harness owns for its *own* bookkeeping, writable by its
/// native edit tools without a grant (SPEC R5.5.7): Claude Code keeps plan-mode
/// documents in `~/.claude/plans` and a per-session scratchpad under
/// `<temp_root>/claude-<uid>/`. They hold no project data, and refusing them
/// broke plan mode outright — the only thing the human could do was grant a
/// directory that is not theirs to worry about. Only directories that exist
/// are listed; nothing is created, and nothing absent is granted.
pub fn harness_owned_dirs(home: Option<&Path>, temp_root: &Path, uid: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push_existing = |p: PathBuf| {
        if p.is_dir()
            && let Ok(canon) = dunce::canonicalize(&p)
            && !out.contains(&canon)
        {
            out.push(canon);
        }
    };
    if let Some(home) = home {
        push_existing(home.join(".claude").join("plans"));
    }
    push_existing(temp_root.join(format!("claude-{uid}")));
    out
}

/// [`harness_owned_dirs`] for the real home, temp root and user.
pub fn harness_owned_dirs_for_this_user() -> Vec<PathBuf> {
    let home = ahma_common::config::ahma_home_dir();
    #[cfg(unix)]
    let (temp_root, uid) = (PathBuf::from("/tmp"), unsafe { libc::getuid() } as u32);
    #[cfg(not(unix))]
    let (temp_root, uid) = (std::env::temp_dir(), 0u32);
    harness_owned_dirs(home.as_deref(), &temp_root, uid)
}

/// The output format a decision must be written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// Claude Code, Codex, VS Code Local: `hookSpecificOutput.permissionDecision`.
    ClaudeLike,
    /// Copilot CLI (camelCase payload): top-level `permissionDecision`.
    Copilot,
    /// Cursor: `permission`.
    Cursor,
    /// Antigravity: `decision`.
    Antigravity,
}

fn dialect(platform: Option<HookPlatform>, payload: &Value) -> Dialect {
    match platform {
        Some(HookPlatform::Cursor) => Dialect::Cursor,
        Some(HookPlatform::Antigravity) => Dialect::Antigravity,
        // One Copilot hook file serves Copilot CLI (camelCase payload) and VS
        // Code's Local agent (snake_case payload, Claude-style output).
        Some(HookPlatform::Copilot) | None if payload.get("toolName").is_some() => Dialect::Copilot,
        _ => Dialect::ClaudeLike,
    }
}

/// The hook's stdout: `Some(json)` to print, `None` to print nothing.
pub fn decide(
    platform: Option<HookPlatform>,
    payload: &Value,
    queue: &WorkspaceQueue,
    allowed: Option<&[PathBuf]>,
) -> Option<Value> {
    let reason = refusal(payload, queue, allowed);
    match (dialect(platform, payload), reason) {
        (Dialect::ClaudeLike, Some(r)) => Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": r,
            }
        })),
        (Dialect::Copilot, Some(r)) => Some(json!({
            "permissionDecision": "deny",
            "permissionDecisionReason": r,
        })),
        (Dialect::Cursor, Some(r)) => Some(json!({
            "permission": "deny",
            "user_message": r,
            "agent_message": r,
        })),
        (Dialect::Antigravity, Some(r)) => Some(json!({ "decision": "deny", "reason": r })),
        (Dialect::ClaudeLike | Dialect::Copilot, None) => None,
        (Dialect::Cursor, None) => Some(json!({ "permission": "allow" })),
        (Dialect::Antigravity, None) => Some(json!({ "decision": "allow" })),
    }
}

/// Entry point: read the payload from stdin, print a decision if there is one.
/// Fails open — any error is "no opinion" — and always exits 0.
pub fn run(args: &HooksEditGuardArgs, cfg: &crate::shell::cli::AppConfig) -> Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload = serde_json::from_str::<Value>(&input).unwrap_or(Value::Null);
    let queue = if cfg.edit_guard {
        WorkspaceQueue::new(true)
    } else {
        WorkspaceQueue::disabled()
    };
    // The scope check runs whenever the hook is active at all (same switch as
    // the shell hook, SPEC R5.5.6); a payload without a usable cwd falls back to
    // this process's, which the harness sets to the session's directory.
    let allowed = super::is_ahma_hooks_active().then(|| {
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        // Session-tier answers given at the TUI or a harness prompt apply to
        // native edits too (SPEC R-PERM.4.4).
        let mut persistent = cfg.persistent_scopes.clone();
        persistent.extend(crate::sandbox::session_scopes_for(
            &super::resolve_hook_sandbox_scopes(&cwd),
        ));
        allowed_edit_scopes(&cwd, &persistent)
    });
    if let Some(decision) = decide(args.platform, &payload, &queue, allowed.as_deref()) {
        use std::io::Write;
        let _ = writeln!(std::io::stdout().lock(), "{decision}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// The pre-tool matcher selecting each harness's file-edit tools.
fn matcher(platform: HookPlatform) -> &'static str {
    match platform {
        HookPlatform::Claude => "Edit|Write|MultiEdit|NotebookEdit",
        HookPlatform::Codex => "^(apply_patch|Edit|Write)$",
        HookPlatform::Copilot => "create|edit|str_replace_editor|apply_patch",
        HookPlatform::Cursor => "Write|Delete",
        HookPlatform::Antigravity => {
            "write_to_file|replace_file_content|multi_replace_file_content"
        }
    }
}

fn guard_args(platform: HookPlatform) -> Vec<String> {
    vec![
        "hooks".to_string(),
        "edit-guard".to_string(),
        "--platform".to_string(),
        platform.cli_name().to_string(),
        "--managed-id".to_string(),
        MANAGED_ID_EDIT_GUARD_V1.to_string(),
    ]
}

fn is_guard_command(command: &str) -> bool {
    command.contains(MANAGED_ID_EDIT_GUARD_V1)
}

/// Whether a hook entry (any harness's shape) is ours.
fn is_guard_entry(entry: &Value) -> bool {
    fn any_command(v: &Value) -> bool {
        ["command", "bash", "powershell", "commandWindows"]
            .iter()
            .any(|k| {
                v.get(*k)
                    .and_then(Value::as_str)
                    .is_some_and(is_guard_command)
            })
    }
    any_command(entry)
        || entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hs| hs.iter().any(any_command))
}

fn entry(platform: HookPlatform, scope: HookScope, env: &HookEnvironment) -> Value {
    let args = guard_args(platform);
    let binary = BinaryReference::for_scope(env, scope);
    let command = binary.build_command(&args);
    let windows_command = match &binary {
        BinaryReference::PathLookup => std::iter::once(PATH_LOOKUP_BINARY.to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" "),
        BinaryReference::Absolute(path) => build_windows_absolute_command(path, &args),
    };
    match platform {
        HookPlatform::Claude | HookPlatform::Codex | HookPlatform::Antigravity => {
            let mut handler = Map::new();
            handler.insert("type".into(), json!("command"));
            handler.insert("command".into(), json!(command));
            if matches!(scope, HookScope::Project) || cfg!(target_os = "windows") {
                handler.insert("commandWindows".into(), json!(windows_command));
            }
            handler.insert("timeout".into(), json!(HOOK_TIMEOUT_SECS));
            handler.insert(
                "statusMessage".into(),
                json!("Checking the workspace write queue"),
            );
            json!({ "matcher": matcher(platform), "hooks": [Value::Object(handler)] })
        }
        HookPlatform::Copilot => {
            let bash = match &binary {
                BinaryReference::PathLookup => command.clone(),
                BinaryReference::Absolute(path) => {
                    std::iter::once(shell_quote_posix(&path.to_string_lossy()))
                        .chain(args.iter().map(|a| shell_quote_posix(a)))
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            };
            json!({
                "type": "command",
                "matcher": matcher(platform),
                "bash": bash,
                "powershell": windows_command,
                "timeoutSec": HOOK_TIMEOUT_SECS,
            })
        }
        HookPlatform::Cursor => json!({
            "matcher": matcher(platform),
            "command": command,
            "timeout": HOOK_TIMEOUT_SECS,
            // Fail open: a missing or crashing ahma must never block edits.
            "failClosed": false,
        }),
    }
}

/// Add (or refresh) the edit guard in a harness's hook document.
pub(super) fn install(
    document: &mut Value,
    platform: HookPlatform,
    scope: HookScope,
    env: &HookEnvironment,
) -> Result<()> {
    let root = ensure_root_object(document)?;
    if matches!(platform, HookPlatform::Cursor | HookPlatform::Copilot) {
        root.entry("version".to_string())
            .or_insert_with(|| Value::Number(1.into()));
    }
    let hooks = ensure_child_object(root, "hooks")?;
    let entries = ensure_child_array(hooks, platform.event_key())?;
    entries.retain(|e| !is_guard_entry(e));
    entries.push(entry(platform, scope, env));
    Ok(())
}

/// Remove the edit guard; `true` when something was removed.
pub(super) fn uninstall(document: &mut Value, platform: HookPlatform) -> Result<bool> {
    remove_managed_hook_entries(
        document,
        platform.label(),
        platform.event_key(),
        is_guard_entry,
    )
}

/// Whether the edit guard is installed in a harness's hook document.
pub(super) fn installed(document: &Value, platform: HookPlatform) -> bool {
    document
        .get("hooks")
        .and_then(|h| h.get(platform.event_key()))
        .and_then(Value::as_array)
        .is_some_and(|entries| entries.iter().any(is_guard_entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::workspace_queue::HolderInfo;
    use tempfile::{TempDir, tempdir};
    use tokio_util::sync::CancellationToken;

    /// A repository with a running exclusive operation, and the queue that sees it.
    async fn busy_repo() -> (
        TempDir,
        PathBuf,
        WorkspaceQueue,
        crate::adapter::workspace_queue::Lease,
    ) {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let repo = dunce::canonicalize(&repo).unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let lease = queue
            .enqueue(&repo, HolderInfo::new("op_3", "cargo fmt --all"))
            .unwrap()
            .acquire(&CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        (td, repo, queue, lease)
    }

    fn file(repo: &Path) -> String {
        repo.join("src/lib.rs").to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn claude_code_edit_is_denied_in_claude_format() {
        let (_td, repo, queue, _lease) = busy_repo().await;
        let payload =
            json!({"tool_name": "Edit", "cwd": "/", "tool_input": {"file_path": file(&repo)}});
        let out = decide(Some(HookPlatform::Claude), &payload, &queue, None).expect("deny");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        let reason = out["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(reason.contains("cargo fmt --all"), "{reason}");
    }

    /// SPEC R2.7.8: a test run reads sources, so a native edit made while one
    /// runs goes through; the run's drift report names the file it may have
    /// seen change. This was the most common refusal, and it bought nothing.
    #[tokio::test]
    async fn an_edit_during_a_test_run_is_left_to_the_client() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let repo = dunce::canonicalize(&repo).unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let _lease = queue
            .enqueue(
                &repo,
                HolderInfo::new("op_9", "cargo nextest run --no-fail-fast"),
            )
            .unwrap()
            .acquire(&CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let payload =
            json!({"tool_name": "Write", "cwd": "/", "tool_input": {"file_path": file(&repo)}});
        assert!(
            decide(Some(HookPlatform::Claude), &payload, &queue, None).is_none(),
            "no opinion: the client decides as usual"
        );
    }

    /// A writer bound to one subtree does not block an edit in another.
    #[tokio::test]
    async fn an_edit_outside_the_writers_subtree_is_left_to_the_client() {
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("rust")).unwrap();
        std::fs::create_dir_all(repo.join("android")).unwrap();
        let repo = dunce::canonicalize(&repo).unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let _lease = queue
            .enqueue(
                &repo,
                HolderInfo::new("op_4", "./gen.sh").with_footprint(repo.join("rust")),
            )
            .unwrap()
            .acquire(&CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let kotlin = repo.join("android/Main.kt").to_string_lossy().into_owned();
        let payload = json!({"tool_name": "Write", "tool_input": {"file_path": kotlin}});
        assert!(decide(Some(HookPlatform::Claude), &payload, &queue, None).is_none());
        let rust = repo.join("rust/lib.rs").to_string_lossy().into_owned();
        let payload = json!({"tool_name": "Write", "tool_input": {"file_path": rust}});
        assert!(decide(Some(HookPlatform::Claude), &payload, &queue, None).is_some());
    }

    /// Outside a git repository the server keys a workspace by its sandbox
    /// scope, which the hook cannot know: a lease on any ancestor of the
    /// edited file must still refuse the edit.
    #[tokio::test]
    async fn an_edit_in_a_non_git_workspace_is_denied() {
        let td = tempdir().unwrap();
        let project = td.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let project = dunce::canonicalize(&project).unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let _lease = queue
            .enqueue(&project, HolderInfo::new("op_5", "make test"))
            .unwrap()
            .acquire(&CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let path = project.join("src/lib.rs").to_string_lossy().into_owned();
        let payload = json!({"tool_name": "Edit", "tool_input": {"file_path": path}});
        let out = decide(Some(HookPlatform::Claude), &payload, &queue, None).expect("deny");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    /// A `Write` whose *content* merely quotes a Codex patch header edits one
    /// file, not the files the quoted patch names.
    #[test]
    fn a_patch_passed_as_an_argv_element_is_read() {
        let input = json!({
            "command": ["apply_patch", "*** Begin Patch\n*** Update File: src/a.rs\n*** End Patch"]
        });
        assert_eq!(edited_paths(&input), vec!["src/a.rs"]);
    }

    #[test]
    fn patch_headers_inside_file_content_are_not_edited_paths() {
        let input = json!({
            "file_path": "/docs/codex.md",
            "content": "Codex patches look like:\n*** Update File: src/other.rs\n"
        });
        assert_eq!(edited_paths(&input), vec!["/docs/codex.md"]);
    }

    #[tokio::test]
    async fn codex_apply_patch_is_denied_by_the_paths_in_its_patch() {
        let (_td, repo, queue, _lease) = busy_repo().await;
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch";
        let payload = json!({
            "tool_name": "apply_patch",
            "cwd": repo.to_string_lossy(),
            "tool_input": {"command": patch}
        });
        let out = decide(Some(HookPlatform::Codex), &payload, &queue, None).expect("deny");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[tokio::test]
    async fn copilot_cli_edit_is_denied_in_copilot_format() {
        let (_td, repo, queue, _lease) = busy_repo().await;
        // Copilot sends toolArgs as a JSON string.
        let args = json!({"path": file(&repo), "old_str": "a", "new_str": "b"}).to_string();
        let payload = json!({"toolName": "edit", "cwd": "/", "toolArgs": args});
        let out = decide(Some(HookPlatform::Copilot), &payload, &queue, None).expect("deny");
        assert_eq!(out["permissionDecision"], "deny");
        assert!(
            out["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("op_3")
        );
    }

    #[tokio::test]
    async fn vs_code_local_reading_the_copilot_file_gets_claude_format() {
        let (_td, repo, queue, _lease) = busy_repo().await;
        let payload = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "replace_string_in_file",
            "cwd": "/",
            "tool_input": {"filePath": file(&repo)}
        });
        let out = decide(Some(HookPlatform::Copilot), &payload, &queue, None).expect("deny");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[tokio::test]
    async fn cursor_and_antigravity_edits_are_denied_in_their_formats() {
        let (_td, repo, queue, _lease) = busy_repo().await;
        let cursor = json!({"tool_name": "Write", "tool_input": {"file_path": file(&repo)}});
        let out = decide(Some(HookPlatform::Cursor), &cursor, &queue, None).expect("deny");
        assert_eq!(out["permission"], "deny");
        let agy = json!({"tool_name": "write_to_file", "tool_input": {"TargetFile": file(&repo)}});
        let out = decide(Some(HookPlatform::Antigravity), &agy, &queue, None).expect("deny");
        assert_eq!(out["decision"], "deny");
    }

    #[tokio::test]
    async fn reads_and_shell_calls_pass_even_when_the_hook_sees_every_tool() {
        let (_td, repo, queue, _lease) = busy_repo().await;
        for name in [
            "read_file",
            "Read",
            "run_in_terminal",
            "Bash",
            "grep_search",
        ] {
            let payload = json!({"tool_name": name, "tool_input": {"filePath": file(&repo)}});
            assert!(
                decide(Some(HookPlatform::Claude), &payload, &queue, None).is_none(),
                "{name} must not be refused"
            );
        }
    }

    #[tokio::test]
    async fn a_free_workspace_gets_no_opinion_or_the_verified_allow() {
        let td = tempdir().unwrap();
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        let path = td.path().join("x.rs").to_string_lossy().into_owned();
        let payload = json!({"tool_name": "Edit", "tool_input": {"file_path": path}});
        assert!(decide(Some(HookPlatform::Claude), &payload, &queue, None).is_none());
        assert!(decide(Some(HookPlatform::Codex), &payload, &queue, None).is_none());
        assert!(decide(Some(HookPlatform::Copilot), &payload, &queue, None).is_none());
        assert_eq!(
            decide(Some(HookPlatform::Cursor), &payload, &queue, None).unwrap()["permission"],
            "allow"
        );
        assert_eq!(
            decide(Some(HookPlatform::Antigravity), &payload, &queue, None).unwrap()["decision"],
            "allow"
        );
    }

    #[test]
    fn garbage_payloads_have_no_opinion() {
        let queue = WorkspaceQueue::disabled();
        assert!(decide(None, &json!("garbage"), &queue, None).is_none());
        assert!(
            decide(
                None,
                &json!({"tool_name": "Edit", "tool_input": {}}),
                &queue,
                None
            )
            .is_none()
        );
    }

    #[test]
    fn paths_are_found_wherever_harnesses_put_them() {
        let input = json!({
            "files": ["/a.rs", {"filePath": "/b.rs"}],
            "replacements": [{"filePath": "/c.rs"}],
            "input": "*** Begin Patch\n*** Add File: d.rs\n*** Move to: e.rs\n*** End Patch"
        });
        assert_eq!(
            edited_paths(&input),
            vec!["/a.rs", "/b.rs", "/c.rs", "d.rs", "e.rs"]
        );
    }

    #[test]
    fn edit_tool_names_are_recognised() {
        for name in [
            "Edit",
            "Write",
            "MultiEdit",
            "apply_patch",
            "edit",
            "create",
            "write_to_file",
            "replace_string_in_file",
            "editFiles",
        ] {
            assert!(is_edit_tool(name), "{name}");
        }
        for name in [
            "Read",
            "view",
            "Bash",
            "run_in_terminal",
            "grep",
            "list_dir",
        ] {
            assert!(!is_edit_tool(name), "{name}");
        }
    }

    fn env(td: &TempDir) -> HookEnvironment {
        HookEnvironment {
            home_dir: td.path().join("home"),
            project_root: td.path().join("proj"),
            current_exe: PathBuf::from("/usr/local/bin/ahma"),
        }
    }

    #[test]
    fn install_is_idempotent_separate_from_the_shell_hook_and_removable() {
        let td = tempdir().unwrap();
        let env = env(&td);
        for platform in HookPlatform::all() {
            let mut doc = json!({});
            super::super::install_platform_hook(&mut doc, platform, HookScope::User, &env).unwrap();
            install(&mut doc, platform, HookScope::User, &env).unwrap();
            install(&mut doc, platform, HookScope::User, &env).unwrap();
            assert!(installed(&doc, platform), "{platform:?}");
            let entries = doc["hooks"][platform.event_key()].as_array().unwrap();
            assert_eq!(
                entries.iter().filter(|e| is_guard_entry(e)).count(),
                1,
                "{platform:?}: re-install must not duplicate"
            );
            assert!(
                super::super::platform_hook_installed(&doc, platform),
                "{platform:?}: the shell hook is untouched"
            );
            let guard = entries.iter().find(|e| is_guard_entry(e)).unwrap();
            assert_eq!(guard["matcher"], matcher(platform), "{platform:?}");
            assert!(uninstall(&mut doc, platform).unwrap());
            assert!(!installed(&doc, platform));
            assert!(
                super::super::platform_hook_installed(&doc, platform),
                "{platform:?}: removing the guard leaves the shell hook"
            );
        }
    }

    /// The arguments an installed hook passes must parse, for every harness.
    #[test]
    fn the_installed_arguments_parse() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            command: super::super::HooksCommand,
        }
        for platform in HookPlatform::all() {
            let mut argv = vec!["ahma".to_string()];
            argv.extend(guard_args(platform).into_iter().skip(1));
            let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            match cli.command {
                super::super::HooksCommand::EditGuard(args) => {
                    assert_eq!(args.platform, Some(platform));
                    assert_eq!(args.managed_id, MANAGED_ID_EDIT_GUARD_V1);
                }
                other => panic!("parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn the_installed_command_runs_the_guard_for_that_harness() {
        let td = tempdir().unwrap();
        let env = env(&td);
        let mut doc = json!({});
        install(&mut doc, HookPlatform::Codex, HookScope::Project, &env).unwrap();
        let cmd = doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.starts_with("ahma hooks edit-guard --platform codex"),
            "{cmd}"
        );
        let mut doc = json!({});
        install(&mut doc, HookPlatform::Copilot, HookScope::User, &env).unwrap();
        let bash = doc["hooks"]["preToolUse"][0]["bash"].as_str().unwrap();
        assert!(
            bash.contains("edit-guard") && bash.contains("copilot"),
            "{bash}"
        );
        assert_eq!(doc["version"], 1);
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use tempfile::tempdir;

    fn idle_queue(td: &Path) -> WorkspaceQueue {
        WorkspaceQueue::with_lock_dir(true, Some(td.join("locks")))
    }

    fn repo(td: &Path) -> PathBuf {
        let repo = td.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        dunce::canonicalize(&repo).unwrap()
    }

    fn write_payload(cwd: &Path, file: &Path) -> Value {
        json!({
            "tool_name": "Write",
            "cwd": cwd.to_string_lossy(),
            "tool_input": {"file_path": file.to_string_lossy(), "content": "x"}
        })
    }

    /// SPEC R5.5.6: a native edit outside the hook's scope is refused, naming
    /// the path, the scope and the human-only remediation.
    #[test]
    fn edit_guard_denies_file_outside_hook_scope() {
        let td = tempdir().unwrap();
        let repo = repo(td.path());
        let elsewhere = td
            .path()
            .join("alt4")
            .join("neubit4")
            .join("src")
            .join("new.rs");
        // `allowed_edit_scopes` admits the temp dir (as the shell sandbox does),
        // which is where this test lives — so the scope is built by hand here.
        let allowed = vec![repo.clone()];
        let out = decide(
            Some(HookPlatform::Claude),
            &write_payload(&repo, &elsewhere),
            &idle_queue(td.path()),
            Some(&allowed),
        )
        .expect("deny");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        let reason = out["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(reason.contains("outside the sandbox scope"), "{reason}");
        assert!(reason.contains("ahma sandbox grant"), "{reason}");
        assert!(reason.contains("cannot widen"), "{reason}");
    }

    #[test]
    fn edit_guard_allows_file_inside_repo_root() {
        let td = tempdir().unwrap();
        let repo = repo(td.path());
        // cwd is a subdirectory; the scope is the enclosing repository, so a
        // file elsewhere in the same repo is fine, even one that does not exist.
        let cwd = repo.join("src");
        let target = repo.join("docs").join("new.md");
        let allowed = super::super::resolve_hook_sandbox_scopes(&cwd);
        assert_eq!(
            allowed,
            vec![repo.clone()],
            "the enclosing repo is the scope"
        );
        assert!(
            decide(
                Some(HookPlatform::Claude),
                &write_payload(&cwd, &target),
                &idle_queue(td.path()),
                Some(&allowed),
            )
            .is_none(),
            "an in-repo edit is no opinion"
        );
    }

    #[test]
    fn edit_guard_allows_persistent_rw_grant() {
        let td = tempdir().unwrap();
        let repo = repo(td.path());
        let cache = td.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cache = dunce::canonicalize(&cache).unwrap();
        let grant = ahma_common::config::PersistentScope {
            path: cache.clone(),
            access: ahma_common::config::ScopeAccess::Rw,
            workspace: None,
            granted_by: Some("user".into()),
            granted_at: None,
            note: None,
        };
        let target = cache.join("heavy.lock");
        let denied = vec![repo.clone()];
        assert!(
            decide(
                Some(HookPlatform::Claude),
                &write_payload(&repo, &target),
                &idle_queue(td.path()),
                Some(&denied),
            )
            .is_some(),
            "without the grant the cache is out of scope"
        );
        let allowed: Vec<PathBuf> = vec![repo.clone(), cache.clone()];
        let computed = allowed_edit_scopes(&repo, &[grant]);
        assert!(
            computed.contains(&cache),
            "an rw grant is in the allowed set: {computed:?}"
        );
        assert!(
            decide(
                Some(HookPlatform::Claude),
                &write_payload(&repo, &target),
                &idle_queue(td.path()),
                Some(&allowed),
            )
            .is_none(),
            "a persistent rw grant admits the edit"
        );
        // A read-only grant does not admit a write.
        let ro = ahma_common::config::PersistentScope {
            path: cache.clone(),
            access: ahma_common::config::ScopeAccess::Ro,
            workspace: None,
            granted_by: None,
            granted_at: None,
            note: None,
        };
        let ro_allowed = allowed_edit_scopes(&repo, &[ro]);
        assert!(
            !ro_allowed.contains(&cache),
            "an ro grant is not writable: {ro_allowed:?}"
        );
        let ro_allowed = vec![repo.clone()];
        assert!(
            decide(
                Some(HookPlatform::Claude),
                &write_payload(&repo, &target),
                &idle_queue(td.path()),
                Some(&ro_allowed),
            )
            .is_some()
        );
    }

    /// A symlink inside the repo that points outside it is resolved before the
    /// check, so it cannot be used to write through the scope.
    #[cfg(unix)]
    #[test]
    fn edit_guard_follows_symlinks_out_of_scope() {
        let td = tempdir().unwrap();
        let repo = repo(td.path());
        let outside = td.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();
        let target = repo.join("link").join("escaped.txt");
        let allowed = vec![repo.clone()];
        assert!(
            decide(
                Some(HookPlatform::Claude),
                &write_payload(&repo, &target),
                &idle_queue(td.path()),
                Some(&allowed),
            )
            .is_some(),
            "a symlinked path is judged by where it really points"
        );
    }

    #[test]
    fn hooks_install_claude_installs_edit_guard_by_default() {
        let args = super::super::HooksInstallArgs {
            platforms: vec![],
            scope: HookScope::User,
            dry_run: true,
            edit_guard: false,
            no_edit_guard: false,
        };
        assert!(args.installs_edit_guard());
        let declined = super::super::HooksInstallArgs {
            no_edit_guard: true,
            ..args
        };
        assert!(!declined.installs_edit_guard());
    }
}

#[cfg(test)]
mod harness_owned_dir_tests {
    use super::*;
    use tempfile::tempdir;

    /// SPEC R5.5.7: a harness's own working set — Claude Code's plan files and
    /// its per-session scratchpad — is writable by its native edit tools without
    /// a grant. They hold no project data, and refusing them broke plan mode.
    #[test]
    fn edit_guard_allows_claude_plan_and_scratchpad_dirs() {
        let home = tempdir().unwrap();
        let plans = home.path().join(".claude").join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let temp_root = tempdir().unwrap();
        let scratch_root = temp_root.path().join("claude-501");
        std::fs::create_dir_all(&scratch_root).unwrap();

        let dirs = harness_owned_dirs(Some(home.path()), temp_root.path(), 501);
        assert!(
            dirs.contains(&dunce::canonicalize(&plans).unwrap()),
            "{dirs:?}"
        );
        assert!(
            dirs.contains(&dunce::canonicalize(&scratch_root).unwrap()),
            "{dirs:?}"
        );

        // Absent directories are not listed: nothing is granted that does not exist.
        let bare_home = tempdir().unwrap();
        let bare_temp = tempdir().unwrap();
        assert!(harness_owned_dirs(Some(bare_home.path()), bare_temp.path(), 501).is_empty());

        // And the guard itself lets the edit through.
        let td = tempdir().unwrap();
        let repo = td.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let repo = dunce::canonicalize(&repo).unwrap();
        let mut allowed = vec![repo.clone()];
        allowed.extend(dirs);
        let plan = plans.join("my-plan.md");
        let payload = serde_json::json!({
            "tool_name": "Write",
            "cwd": repo.to_string_lossy(),
            "tool_input": {"file_path": plan.to_string_lossy()}
        });
        let queue = WorkspaceQueue::with_lock_dir(true, Some(td.path().join("locks")));
        assert!(
            decide(Some(HookPlatform::Claude), &payload, &queue, Some(&allowed)).is_none(),
            "a plan-file write is no opinion"
        );
    }
}
