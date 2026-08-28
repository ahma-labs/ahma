//! Classification of **auto-executing configuration files** — the "write a file
//! the sandbox permits, let a trusted component outside the sandbox run it"
//! escape class.
//!
//! Every published sandbox escape in this family shares one shape: the agent
//! never broke out. It wrote a file the sandbox legitimately allowed, and a
//! *component outside the sandbox* executed that file later — a git hook on the
//! user's next `git checkout`, an editor's interpreter-discovery binary running
//! a fake `bin/python` next to a planted `pyvenv.cfg`, a `folderOpen` task on
//! the next window reopen, a harness `Stop` hook.
//!
//! ## Two classes, deliberately
//!
//! [`ExecConfigClass::DenyWrite`] is for files no legitimate agent task needs to
//! author. [`ExecConfigClass::Disclose`] is for files that *are* legitimate to
//! write — "add a launch config", "set my git identity" — but that also
//! auto-execute later. Disclose is deliberately **not** a block. Blocking it
//! would break ordinary editor/git-configuration requests, and the resulting
//! prompt-every-time behaviour is exactly the permission fatigue ahma exists to
//! avoid: a user who clicks "allow" reflexively is not protected by the prompt.
//! A loud, unmissable line in the tool result keeps the human in the loop
//! without training them to ignore it.
//!
//! ## Never defend by path spelling
//!
//! Cursor guarded `.git/config` with the regex `^.*/\.git/config$` and was
//! defeated by `git init --separate-git-dir=.git-alt`, which moves the whole git
//! directory somewhere the regex does not describe. This module therefore
//! resolves the *abstraction* — [`resolve_git_dirs`] follows `.git` files
//! (`gitdir: …`, used by both worktrees and `--separate-git-dir`) to wherever
//! the git directory actually lives, and every git rule is expressed against the
//! resolved directories rather than against the literal string `.git`.
//!
//! ## What is and is not kernel-enforced
//!
//! [`deny_write_globs`] returns concrete subpaths, which macOS Seatbelt denies at
//! the kernel. The venv-shaped rule is **not** in that set: it is not a fixed
//! subpath, and a kernel-level deny on `*/bin/python*` would break a legitimate
//! `python -m venv` run through `run_terminal_command`. That rule is enforced at
//! the write-tool layer only, where the agent is authoring the interpreter
//! directly, which is the actual attack. See the enforcement-asymmetry note on
//! [`crate::file_ops::exec_config_write_guard`].
//!
//! ## Escape hatches, because a default with no way out is not a default
//!
//! Two of the [`ExecConfigClass::DenyWrite`] paths have genuinely legitimate
//! uses. This repository's own `AGENTS.md` tells developers to install a
//! pre-push guard with `cp scripts/check-guardrails.sh .git/hooks/pre-push`, and
//! any project that ships its own `.ahma/` tool definitions needs to edit them
//! from inside a session. Denying those with no documented way out does not make
//! anyone safer — it makes them turn the sandbox off wholesale, which is
//! strictly worse. [`HandoffAllowances`] is the narrow, deliberate opt-in:
//! `[sandbox] allow_git_hooks` / `--allow-git-hooks` and `[sandbox]
//! allow_project_tool_config` / `--allow-project-tool-config`. Both default to
//! **denied**, both are session-wide, and enabling either is disclosed loudly at
//! startup (SPEC R7 — ahma never silently weakens enforcement). The denial error
//! itself names the flag, so nobody has to go looking for it.
//!
//! ## Re-resolution timing: a repo cloned mid-command is covered from the *next*
//! command
//!
//! [`resolve_git_dirs`] runs when the Seatbelt profile is generated, and the
//! profile is regenerated per spawn (`sandbox::command`'s macOS arm calls
//! `build_macos_sandbox_command` for every command). So git dirs are re-resolved
//! constantly rather than cached for the session (R-HANDOFF.2 requires exactly
//! that), but the resolution is still a *snapshot taken at spawn time*:
//!
//! * a repository created by command N — `git clone`, `git init`, `git worktree
//!   add` — is **not** in command N's own profile, because that profile was
//!   built before the repository existed. For the remainder of command N its
//!   hooks directory is writable at the kernel level;
//! * from command N+1 onward it **is** denied, because the next profile
//!   resolves it.
//!
//! This is inherent to a kernel policy fixed at process start: Seatbelt profiles
//! are immutable for the life of the sandboxed process, so no amount of
//! re-resolution can cover a directory that did not exist when the process was
//! spawned. It is an accepted limitation, not a bug — narrowing it would require
//! either a filesystem watcher (rejected by R1.4/R-HANDOFF.7) or per-syscall
//! interposition. Note also that a repository cloned more than one directory
//! below the workspace root is never resolved at all, at any command, because
//! [`resolve_git_dirs`] scans only the root and its immediate children under a
//! fixed entry budget. The write-tool guard ([`crate::file_ops`]) is unaffected:
//! it resolves at the moment of the write, so hooks in a freshly-cloned repo are
//! refused immediately there. `deny_write_globs_reflects_a_repository_created_after_the_previous_resolution`
//! pins the behaviour.

use parking_lot::RwLock;
use std::path::{Component, Path, PathBuf};

/// Operator opt-ins that remove a path from the [`ExecConfigClass::DenyWrite`]
/// set — both the write-tool guard and the macOS Seatbelt deny rules.
///
/// Process-global, a single operator policy, installed once at startup from the
/// resolved CLI-flag/settings values (the same startup path that installs
/// keychain access and the credential deny set). Everything is **denied** until
/// then, so in-process embedders and Test-mode sandboxes get the safe default
/// without any wiring — the inverse of
/// [`super::credential_reads::keychain_access_allowed`], whose real default is
/// *on*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HandoffAllowances {
    /// `<git_dir>/hooks/**` is writable (`[sandbox] allow_git_hooks`).
    pub git_hooks: bool,
    /// `<workspace>/.ahma/**` is writable (`[sandbox] allow_project_tool_config`).
    pub project_tool_config: bool,
}

static HANDOFF_ALLOWANCES: RwLock<HandoffAllowances> = RwLock::new(HandoffAllowances {
    git_hooks: false,
    project_tool_config: false,
});

impl HandoffAllowances {
    /// The installed process-global policy (everything denied until startup
    /// installs it).
    pub fn current() -> Self {
        *HANDOFF_ALLOWANCES.read()
    }
}

/// Install the operator escape-hatch policy (called once at startup with the
/// resolved flag/settings values). See [`HandoffAllowances`].
pub fn set_handoff_allowances(allowances: HandoffAllowances) {
    let mut guard = HANDOFF_ALLOWANCES.write();
    *guard = allowances;
}

/// How a write to an auto-executing configuration path should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecConfigClass {
    /// Never written by legitimate agent work; deny at the kernel where possible.
    DenyWrite,
    /// Legitimate to write, but auto-executes later. Allow + disclose loudly.
    Disclose,
}

/// Reason strings. Kept as `&'static str` so callers can embed them in both
/// error text and disclosure text without allocating.
mod reasons {
    pub const GIT_HOOKS: &str = "files under a git hooks directory execute on the next git operation, outside ahma's sandbox";
    pub const AHMA_CONFIG: &str = ".ahma/ holds ahma's own MTDF tool definitions; an agent authoring its own tool surface would define the commands it is later allowed to run";
    pub const PYVENV_CFG: &str = "a pyvenv.cfg marks its directory as a virtualenv, and editor interpreter-discovery then executes the sibling bin/python from an unsandboxed extension host";
    pub const FAKE_INTERPRETER: &str = "an executable named python under bin/ or Scripts/ is what editor interpreter-discovery runs, from an unsandboxed extension host";
    pub const VSCODE_TASKS: &str = "a .vscode task can declare \"runOn\": \"folderOpen\" and run unprompted when the folder is reopened, outside ahma's sandbox";
    pub const VSCODE_SETTINGS: &str = "editor settings can enable automatic task execution (task.allowAutomaticTasks) and select the interpreter that later runs, outside ahma's sandbox";
    pub const HARNESS_HOOKS: &str =
        "agent-harness hooks are executed by the harness itself, outside ahma's sandbox";
    pub const MCP_CONFIG: &str = "an MCP server entry names a command the client launches on startup, outside ahma's sandbox";
    pub const CURSOR_RULES: &str =
        "rules files are injected into future agent turns and can steer them, outside any review";
    pub const GIT_CONFIG: &str = "git config keys core.hooksPath, core.fsmonitor, core.pager, core.sshCommand, alias.*, diff.*.command and filter.*.clean|smudge are all execution primitives that fire on the next git operation";
}

/// The operator opt-in that permits an otherwise-denied write, in the exact
/// spelling the person reading the error has to type.
///
/// AGENTS.md: errors give actionable context. A denial that names no way out is
/// how you get someone to disable the sandbox entirely, so the flag *and* the
/// settings key *and* the consequence are all in the message.
mod hatches {
    pub const GIT_HOOKS: &str = "If installing a hook is genuinely what you want (e.g. a repo's own pre-push guard), restart ahma with `--allow-git-hooks`, or set `[sandbox] allow_git_hooks = true` in ~/.ahma/settings.toml. Either one makes every resolved git hooks directory writable for the whole session, and ahma says so at startup";
    pub const AHMA_CONFIG: &str = "If you are developing this project's tool definitions, restart ahma with `--allow-project-tool-config`, or set `[sandbox] allow_project_tool_config = true` in ~/.ahma/settings.toml. That makes <workspace>/.ahma writable for the whole session (edits still take effect only via the `restart` tool, never automatically), and ahma says so at startup";
}

/// Reason → escape hatch, for the denies that have one. Denies absent from this
/// table (fake interpreters, `pyvenv.cfg`) have no opt-in **by design**: nothing
/// legitimate writes them, so there is nothing to permit.
const DENY_ESCAPE_HATCHES: [(&str, &str); 2] = [
    (reasons::GIT_HOOKS, hatches::GIT_HOOKS),
    (reasons::AHMA_CONFIG, hatches::AHMA_CONFIG),
];

/// The operator escape hatch for a [`ExecConfigClass::DenyWrite`] reason, if the
/// rule has one. Callers embed it in the denial message.
pub fn escape_hatch(reason: &str) -> Option<&'static str> {
    DENY_ESCAPE_HATCHES
        .iter()
        .find(|(r, _)| *r == reason)
        .map(|(_, hatch)| *hatch)
}

/// Directory names that are cheap to skip when scanning for nested git dirs and
/// never contain a repository we care about defending.
const SCAN_SKIP_DIRS: [&str; 6] = ["node_modules", "target", ".git", "vendor", "dist", "build"];

/// Upper bound on directory entries examined by [`resolve_git_dirs`]. A shallow
/// scan of a large monorepo must not turn a single `write_file` into a
/// filesystem walk.
const SCAN_ENTRY_BUDGET: usize = 512;

/// Classify a write target under the **installed** operator policy
/// ([`HandoffAllowances::current`]).
///
/// Performs **no I/O**, so it is unit-testable on every platform; `git_dirs`
/// comes from [`resolve_git_dirs`]. The only non-argument input is the
/// process-global escape-hatch policy — use [`classify_with`] where a test needs
/// to pin that explicitly.
///
/// `path` may be relative; it is resolved against `workspace_root` and
/// normalized lexically (so `.vscode/../.vscode/tasks.json` and
/// `a/../.git/hooks/pre-commit` classify correctly). Fixed names are compared
/// ASCII-case-insensitively: on a case-insensitive filesystem `.VSCode` *is*
/// `.vscode`, and over-matching here costs at most one extra warning line.
pub fn classify(
    path: &Path,
    workspace_root: &Path,
    git_dirs: &[PathBuf],
) -> Option<(ExecConfigClass, &'static str)> {
    classify_with(path, workspace_root, git_dirs, HandoffAllowances::current())
}

/// [`classify`] with the escape-hatch policy passed in rather than read from the
/// process global. Fully pure.
pub fn classify_with(
    path: &Path,
    workspace_root: &Path,
    git_dirs: &[PathBuf],
    allowances: HandoffAllowances,
) -> Option<(ExecConfigClass, &'static str)> {
    let abs = absolutize(path, workspace_root);
    let root = normalize(workspace_root);

    if let Some(hit) = classify_git(&abs, git_dirs, allowances) {
        return Some(hit);
    }
    if let Some(hit) = classify_venv(&abs) {
        return Some(hit);
    }

    // The remaining rules are workspace-relative. A path outside the workspace
    // is not our business here — the sandbox scope check already rejects it.
    let rel = abs.strip_prefix(&root).ok()?;
    let comps = components(rel);
    if comps.is_empty() {
        return None;
    }

    if comps[0].eq_ignore_ascii_case(".ahma") {
        // Opted out of via `[sandbox] allow_project_tool_config`; falling through
        // rather than returning means a `.ahma/.vscode/tasks.json` still gets its
        // ordinary disclose treatment.
        if !allowances.project_tool_config {
            return Some((ExecConfigClass::DenyWrite, reasons::AHMA_CONFIG));
        }
    }

    classify_disclose(&comps)
}

/// Git-directory rules, expressed against the *resolved* git dirs rather than
/// the literal spelling `.git` (see the module doc on `--separate-git-dir`).
fn classify_git(
    abs: &Path,
    git_dirs: &[PathBuf],
    allowances: HandoffAllowances,
) -> Option<(ExecConfigClass, &'static str)> {
    for git_dir in git_dirs {
        if !allowances.git_hooks && abs.starts_with(git_dir.join("hooks")) {
            return Some((ExecConfigClass::DenyWrite, reasons::GIT_HOOKS));
        }
        if abs == git_dir.join("config") {
            // Deliberately not a deny: `git config user.email` is a routine,
            // legitimate request.
            return Some((ExecConfigClass::Disclose, reasons::GIT_CONFIG));
        }
    }
    None
}

/// The virtualenv-shaped fake-interpreter vector, kept deliberately simple:
/// a `pyvenv.cfg` anywhere, or a file named `python*` directly inside a `bin`
/// or `Scripts` directory. Both halves of the real exploit (the marker file and
/// the executable it points discovery at) are covered without trying to model
/// virtualenv layout.
fn classify_venv(abs: &Path) -> Option<(ExecConfigClass, &'static str)> {
    let name = abs.file_name()?.to_string_lossy().to_string();
    if name.eq_ignore_ascii_case("pyvenv.cfg") {
        return Some((ExecConfigClass::DenyWrite, reasons::PYVENV_CFG));
    }
    if !name.to_ascii_lowercase().starts_with("python") {
        return None;
    }
    let parent = abs.parent()?.file_name()?.to_string_lossy().to_string();
    if parent.eq_ignore_ascii_case("bin") || parent.eq_ignore_ascii_case("scripts") {
        return Some((ExecConfigClass::DenyWrite, reasons::FAKE_INTERPRETER));
    }
    None
}

/// Editor / harness configuration files that auto-execute. Matched on the
/// *trailing* `<dir>/<file>` pair so a nested `packages/app/.vscode/tasks.json`
/// is caught too — VS Code reads multi-root workspaces, and depth is not a
/// security property.
fn classify_disclose(comps: &[String]) -> Option<(ExecConfigClass, &'static str)> {
    let n = comps.len();
    let last = comps[n - 1].as_str();

    // `.cursor/rules/**` — any depth beneath the rules directory.
    if comps
        .windows(2)
        .any(|w| w[0].eq_ignore_ascii_case(".cursor") && w[1].eq_ignore_ascii_case("rules"))
        && n >= 3
    {
        return Some((ExecConfigClass::Disclose, reasons::CURSOR_RULES));
    }

    // `.gemini/**/hooks.json` — Gemini nests hooks under variable subdirs.
    if last.eq_ignore_ascii_case("hooks.json")
        && comps.iter().any(|c| c.eq_ignore_ascii_case(".gemini"))
    {
        return Some((ExecConfigClass::Disclose, reasons::HARNESS_HOOKS));
    }

    if n < 2 {
        return None;
    }
    let dir = comps[n - 2].as_str();

    let matches = |d: &str, f: &str| dir.eq_ignore_ascii_case(d) && last.eq_ignore_ascii_case(f);

    if matches(".vscode", "tasks.json") || matches(".vscode", "launch.json") {
        return Some((ExecConfigClass::Disclose, reasons::VSCODE_TASKS));
    }
    if matches(".vscode", "settings.json") {
        return Some((ExecConfigClass::Disclose, reasons::VSCODE_SETTINGS));
    }
    if matches(".vscode", "mcp.json") || matches(".cursor", "mcp.json") {
        return Some((ExecConfigClass::Disclose, reasons::MCP_CONFIG));
    }
    if matches(".claude", "settings.json")
        || matches(".claude", "settings.local.json")
        || matches(".cursor", "hooks.json")
        || matches(".agents", "hooks.json")
        || matches(".codex", "hooks.json")
    {
        return Some((ExecConfigClass::Disclose, reasons::HARNESS_HOOKS));
    }
    None
}

/// Concrete subpaths whose writes should be denied by the kernel where the
/// platform can express it (macOS Seatbelt `(deny file-write* (subpath …))`),
/// under the **installed** operator policy ([`HandoffAllowances::current`]).
///
/// Only the rules that *are* fixed subpaths appear here. The venv-shaped rule is
/// intentionally absent — see the module doc.
///
/// An enabled escape hatch drops its subpath from this set, which is what makes
/// the opt-in real: the kernel rule disappears with the application-layer one,
/// so the write genuinely succeeds instead of failing later with a confusing
/// `Operation not permitted`.
pub fn deny_write_globs(workspace_root: &Path, git_dirs: &[PathBuf]) -> Vec<PathBuf> {
    deny_write_globs_with(workspace_root, git_dirs, HandoffAllowances::current())
}

/// [`deny_write_globs`] with the escape-hatch policy passed in rather than read
/// from the process global. Fully pure.
pub fn deny_write_globs_with(
    workspace_root: &Path,
    git_dirs: &[PathBuf],
    allowances: HandoffAllowances,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = if allowances.git_hooks {
        Vec::new()
    } else {
        git_dirs.iter().map(|g| g.join("hooks")).collect()
    };
    if !allowances.project_tool_config {
        out.push(normalize(workspace_root).join(".ahma"));
    }
    out.dedup();
    out
}

/// Resolve every git directory that governs `workspace_root`. **The only I/O in
/// this module.**
///
/// Handles all three spellings a git directory can take:
/// * a plain `.git` directory;
/// * a `.git` *file* containing `gitdir: <path>` — produced by
///   `git worktree add` and by `git init --separate-git-dir`, with either a
///   relative or an absolute target;
/// * nested repositories one level below the root (monorepos, vendored checkouts).
///
/// For a worktree the pointed-at directory is `<main>/.git/worktrees/<name>`,
/// whose `commondir` file names the shared git dir where the hooks actually
/// live; that is resolved too. Returned paths are canonicalized and absolute.
pub fn resolve_git_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let root = dunce::canonicalize(workspace_root).unwrap_or_else(|_| normalize(workspace_root));
    let mut out = Vec::new();
    let mut budget = SCAN_ENTRY_BUDGET;

    collect_git_dir(&root, &mut out);

    // One level of children: nested repos in a monorepo / vendored checkout.
    let Ok(entries) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.flatten() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let child = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if SCAN_SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_git_dir(&child, &mut out);
        }
    }
    out
}

/// `tokio::fs` twin of [`resolve_git_dirs`], for the async write-tool path
/// (AGENTS.md: no blocking I/O in an async fn, and `spawn_blocking` is reserved
/// for sync-only APIs and CPU-bound work — this is neither).
///
/// Same resolution rules, same return contract. Call it only when
/// [`needs_git_dir_resolution`] says the path could possibly be a git hit;
/// every other write must not pay for a directory scan.
pub async fn resolve_git_dirs_async(workspace_root: &Path) -> Vec<PathBuf> {
    let root = tokio::fs::canonicalize(workspace_root)
        .await
        .map(|p| dunce::simplified(&p).to_path_buf())
        .unwrap_or_else(|_| normalize(workspace_root));
    let mut out = Vec::new();
    let mut budget = SCAN_ENTRY_BUDGET;

    collect_git_dir_async(&root, &mut out).await;

    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let name = entry.file_name().to_string_lossy().to_string();
        if SCAN_SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
            continue;
        }
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            collect_git_dir_async(&entry.path(), &mut out).await;
        }
    }
    out
}

/// Cheap, allocation-light lexical pre-check: could this path possibly land on a
/// git-directory rule? Only a `hooks` component or a file literally named
/// `config` can, so everything else skips [`resolve_git_dirs_async`] entirely and
/// the common write pays no filesystem cost at all.
pub fn needs_git_dir_resolution(path: &Path) -> bool {
    let mut saw_hooks = false;
    for c in path.components() {
        if let Component::Normal(s) = c {
            let s = s.to_string_lossy();
            if s.eq_ignore_ascii_case("hooks") {
                saw_hooks = true;
            }
        }
    }
    saw_hooks
        || path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("config"))
}

async fn collect_git_dir_async(dir: &Path, out: &mut Vec<PathBuf>) {
    let dot_git = dir.join(".git");
    let Ok(meta) = tokio::fs::metadata(&dot_git).await else {
        return;
    };
    if meta.is_dir() {
        push_unique(out, canonical_or_async(&dot_git).await);
        return;
    }
    let Ok(contents) = tokio::fs::read_to_string(&dot_git).await else {
        return;
    };
    let Some(target) = parse_gitdir_pointer(&contents, dir) else {
        return;
    };
    let target = canonical_or_async(&target).await;
    if let Ok(common) = tokio::fs::read_to_string(target.join("commondir")).await
        && let Some(resolved) = resolve_relative(common.trim(), &target)
    {
        push_unique(out, canonical_or_async(&resolved).await);
    }
    push_unique(out, target);
}

async fn canonical_or_async(p: &Path) -> PathBuf {
    match tokio::fs::canonicalize(p).await {
        Ok(c) => dunce::simplified(&c).to_path_buf(),
        Err(_) => normalize(p),
    }
}

/// Resolve `<dir>/.git` (directory or `gitdir:` pointer file) into `out`.
fn collect_git_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let dot_git = dir.join(".git");
    let Ok(meta) = std::fs::metadata(&dot_git) else {
        return;
    };
    if meta.is_dir() {
        push_unique(out, canonical_or(&dot_git));
        return;
    }
    let Ok(contents) = std::fs::read_to_string(&dot_git) else {
        return;
    };
    let Some(target) = parse_gitdir_pointer(&contents, dir) else {
        return;
    };
    let target = canonical_or(&target);
    // A worktree's gitdir names `<main>/.git/worktrees/<name>`; the hooks live in
    // the common dir it points at, so defend both.
    if let Ok(common) = std::fs::read_to_string(target.join("commondir"))
        && let Some(resolved) = resolve_relative(common.trim(), &target)
    {
        push_unique(out, canonical_or(&resolved));
    }
    push_unique(out, target);
}

/// Parse the body of a `.git` pointer file. Pure — split out so the pointer
/// grammar is testable without touching the filesystem.
fn parse_gitdir_pointer(contents: &str, git_file_parent: &Path) -> Option<PathBuf> {
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("gitdir:") {
            return resolve_relative(rest.trim(), git_file_parent);
        }
    }
    None
}

/// Resolve a possibly-relative git pointer target against `base`.
fn resolve_relative(target: &str, base: &Path) -> Option<PathBuf> {
    if target.is_empty() {
        return None;
    }
    let p = Path::new(target);
    Some(if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&base.join(p))
    })
}

fn canonical_or(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| normalize(p))
}

fn push_unique(out: &mut Vec<PathBuf>, p: PathBuf) {
    if !out.contains(&p) {
        out.push(p);
    }
}

/// Lexical `.`/`..` collapse. Mirrors [`super::normalize_path_lexically`]; kept
/// as a local alias so this module has a single spelling for it.
fn normalize(path: &Path) -> PathBuf {
    super::scopes::normalize_path_lexically(path)
}

fn absolutize(path: &Path, workspace_root: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&workspace_root.join(path))
    }
}

fn components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::path_helpers::test_abs;
    use tempfile::tempdir;

    fn ws() -> PathBuf {
        test_abs(&["work", "repo"])
    }

    #[test]
    fn plain_file_is_unclassified() {
        assert_eq!(classify(&ws().join("src/main.rs"), &ws(), &[]), None);
        assert_eq!(classify(&ws().join("README.md"), &ws(), &[]), None);
    }

    #[test]
    fn git_hooks_are_denied_through_the_resolved_git_dir() {
        let git_dir = ws().join(".git");
        let (class, reason) = classify(
            &git_dir.join("hooks/post-checkout"),
            &ws(),
            std::slice::from_ref(&git_dir),
        )
        .expect("git hook must classify");
        assert_eq!(class, ExecConfigClass::DenyWrite);
        assert!(reason.contains("git operation"), "reason: {reason}");
    }

    /// The escape-4 bypass: `git init --separate-git-dir=.git-alt` moves the git
    /// directory somewhere `^.*/\.git/hooks/` never matches. Classification must
    /// follow the resolved dir, not the spelling.
    #[test]
    fn separate_git_dir_hooks_are_denied_though_path_does_not_say_dot_git() {
        let alt = ws().join(".git-alt");
        let hit = classify(
            &alt.join("hooks/pre-commit"),
            &ws(),
            std::slice::from_ref(&alt),
        );
        assert_eq!(
            hit.map(|h| h.0),
            Some(ExecConfigClass::DenyWrite),
            "a relocated git dir's hooks must still be denied"
        );
        // …and with no git dirs resolved, the same path is *not* special-cased
        // by its spelling, proving the rule is abstraction-based.
        assert_eq!(classify(&alt.join("hooks/pre-commit"), &ws(), &[]), None);
    }

    #[test]
    fn dot_dot_traversal_into_hooks_is_still_denied() {
        let git_dir = ws().join(".git");
        let sneaky = ws().join("src/../.git/hooks/pre-push");
        assert_eq!(
            classify(&sneaky, &ws(), std::slice::from_ref(&git_dir)).map(|h| h.0),
            Some(ExecConfigClass::DenyWrite)
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_workspace_root() {
        let git_dir = ws().join(".git");
        assert_eq!(
            classify(
                Path::new(".git/hooks/pre-commit"),
                &ws(),
                std::slice::from_ref(&git_dir)
            )
            .map(|h| h.0),
            Some(ExecConfigClass::DenyWrite)
        );
    }

    #[test]
    fn ahma_tool_config_dir_is_denied() {
        let (class, reason) =
            classify(&ws().join(".ahma/tools/evil.json"), &ws(), &[]).expect("classified");
        assert_eq!(class, ExecConfigClass::DenyWrite);
        assert!(reason.contains("MTDF"), "reason: {reason}");
        // A nested `.ahma` deeper in the tree is not ahma's config surface.
        assert_eq!(classify(&ws().join("sub/.ahma/x.json"), &ws(), &[]), None);
    }

    #[test]
    fn pyvenv_cfg_is_denied_anywhere() {
        for p in [
            ws().join("pyvenv.cfg"),
            ws().join("a/b/pyvenv.cfg"),
            ws().join("a/PyVenv.CFG"),
        ] {
            assert_eq!(
                classify(&p, &ws(), &[]).map(|h| h.0),
                Some(ExecConfigClass::DenyWrite),
                "{}",
                p.display()
            );
        }
    }

    #[test]
    fn fake_interpreter_under_bin_or_scripts_is_denied() {
        for p in [
            ws().join("venv/bin/python"),
            ws().join("venv/bin/python3"),
            ws().join("a/b/Scripts/python.exe"),
            ws().join("x/Scripts/pythonw.exe"),
        ] {
            assert_eq!(
                classify(&p, &ws(), &[]).map(|h| h.0),
                Some(ExecConfigClass::DenyWrite),
                "{}",
                p.display()
            );
        }
        // Not under bin/Scripts, and not named python — untouched.
        assert_eq!(classify(&ws().join("tools/python"), &ws(), &[]), None);
        assert_eq!(classify(&ws().join("venv/bin/pip"), &ws(), &[]), None);
    }

    #[test]
    fn git_config_discloses_rather_than_denies() {
        let git_dir = ws().join(".git");
        let (class, reason) = classify(
            &git_dir.join("config"),
            &ws(),
            std::slice::from_ref(&git_dir),
        )
        .expect("classified");
        assert_eq!(
            class,
            ExecConfigClass::Disclose,
            "`git config user.email` must keep working"
        );
        assert!(reason.contains("core.hooksPath"), "reason: {reason}");
    }

    #[test]
    fn every_disclose_path_is_classified() {
        let cases = [
            ".vscode/tasks.json",
            ".vscode/launch.json",
            ".vscode/settings.json",
            ".vscode/mcp.json",
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".cursor/hooks.json",
            ".cursor/mcp.json",
            ".cursor/rules/anything.mdc",
            ".cursor/rules/nested/deep.mdc",
            ".agents/hooks.json",
            ".codex/hooks.json",
            ".gemini/hooks.json",
            ".gemini/some/nested/hooks.json",
        ];
        for case in cases {
            let hit = classify(&ws().join(case), &ws(), &[]);
            assert_eq!(
                hit.map(|h| h.0),
                Some(ExecConfigClass::Disclose),
                "{case} must be disclosed"
            );
            assert!(
                !hit.unwrap().1.is_empty(),
                "{case} must carry a human reason"
            );
        }
    }

    #[test]
    fn disclose_matches_at_nested_depth() {
        assert_eq!(
            classify(&ws().join("packages/app/.vscode/tasks.json"), &ws(), &[]).map(|h| h.0),
            Some(ExecConfigClass::Disclose)
        );
    }

    #[test]
    fn similar_but_harmless_paths_are_not_classified() {
        for case in [
            "vscode/tasks.json",
            ".vscode/keybindings.json",
            "docs/.claude/README.md",
            "tasks.json",
            ".cursor/rules",
        ] {
            assert_eq!(
                classify(&ws().join(case), &ws(), &[]),
                None,
                "{case} must not be flagged"
            );
        }
    }

    /// Both escape hatches are **off** unless installed, and each flips only its
    /// own rule. The polarity is the inverse of `allow_keychain`, so "default"
    /// here means denied.
    #[test]
    fn escape_hatches_are_off_by_default_and_flip_independently() {
        let git_dir = ws().join(".git");
        let hook = git_dir.join("hooks/pre-push");
        let tool_config = ws().join(".ahma/tools/x.json");
        let git_dirs = std::slice::from_ref(&git_dir);
        let denied = HandoffAllowances::default();

        assert!(!denied.git_hooks && !denied.project_tool_config);
        assert_eq!(
            classify_with(&hook, &ws(), git_dirs, denied).map(|h| h.0),
            Some(ExecConfigClass::DenyWrite)
        );
        assert_eq!(
            classify_with(&tool_config, &ws(), git_dirs, denied).map(|h| h.0),
            Some(ExecConfigClass::DenyWrite)
        );

        // `allow_git_hooks` alone releases the hook and nothing else.
        let hooks_only = HandoffAllowances {
            git_hooks: true,
            project_tool_config: false,
        };
        assert_eq!(classify_with(&hook, &ws(), git_dirs, hooks_only), None);
        assert_eq!(
            classify_with(&tool_config, &ws(), git_dirs, hooks_only).map(|h| h.0),
            Some(ExecConfigClass::DenyWrite),
            "allowing git hooks must not open .ahma/"
        );

        // …and `allow_project_tool_config` alone releases `.ahma/` and nothing else.
        let config_only = HandoffAllowances {
            git_hooks: false,
            project_tool_config: true,
        };
        assert_eq!(
            classify_with(&tool_config, &ws(), git_dirs, config_only),
            None
        );
        assert_eq!(
            classify_with(&hook, &ws(), git_dirs, config_only).map(|h| h.0),
            Some(ExecConfigClass::DenyWrite),
            "allowing .ahma/ must not open git hooks"
        );
    }

    /// An escape hatch relaxes only the *deny* tier. `.git/config` is a
    /// `Disclose` rule and stays disclosed, and a `.vscode/tasks.json` written
    /// inside a now-writable `.ahma/` is still disclosed — the hatch is not a
    /// blanket "stop classifying this subtree".
    #[test]
    fn escape_hatches_do_not_silence_the_disclose_tier() {
        let git_dir = ws().join(".git");
        let all_allowed = HandoffAllowances {
            git_hooks: true,
            project_tool_config: true,
        };
        assert_eq!(
            classify_with(
                &git_dir.join("config"),
                &ws(),
                std::slice::from_ref(&git_dir),
                all_allowed
            )
            .map(|h| h.0),
            Some(ExecConfigClass::Disclose)
        );
        assert_eq!(
            classify_with(
                &ws().join(".ahma/.vscode/tasks.json"),
                &ws(),
                &[],
                all_allowed
            )
            .map(|h| h.0),
            Some(ExecConfigClass::Disclose)
        );
    }

    /// The denial message is useless without the way out, so every DenyWrite
    /// reason that *has* an operator opt-in must be able to name it — and the
    /// ones that deliberately have none must not invent one.
    #[test]
    fn deny_reasons_with_an_opt_in_name_their_flag_and_settings_key() {
        let git_dir = ws().join(".git");
        let (_, hook_reason) = classify(
            &git_dir.join("hooks/pre-push"),
            &ws(),
            std::slice::from_ref(&git_dir),
        )
        .expect("hook classifies");
        let hatch = escape_hatch(hook_reason).expect("git hooks must offer an escape hatch");
        assert!(hatch.contains("--allow-git-hooks"), "{hatch}");
        assert!(hatch.contains("allow_git_hooks = true"), "{hatch}");
        assert!(hatch.contains("settings.toml"), "{hatch}");

        let (_, cfg_reason) =
            classify(&ws().join(".ahma/tools/x.json"), &ws(), &[]).expect("config classifies");
        let hatch = escape_hatch(cfg_reason).expect(".ahma must offer an escape hatch");
        assert!(hatch.contains("--allow-project-tool-config"), "{hatch}");
        assert!(
            hatch.contains("allow_project_tool_config = true"),
            "{hatch}"
        );

        // No legitimate task authors a fake interpreter, so no opt-in is offered.
        let (_, venv_reason) =
            classify(&ws().join("venv/bin/python"), &ws(), &[]).expect("venv classifies");
        assert_eq!(escape_hatch(venv_reason), None);
        assert_eq!(escape_hatch("some unrelated reason"), None);
    }

    #[test]
    fn handoff_allowances_install_and_read_back() {
        let allowances = HandoffAllowances {
            git_hooks: true,
            project_tool_config: false,
        };
        set_handoff_allowances(allowances);
        assert_eq!(HandoffAllowances::current(), allowances);
        set_handoff_allowances(HandoffAllowances::default());
        assert_eq!(
            HandoffAllowances::current(),
            HandoffAllowances::default(),
            "the safe default must be restorable"
        );
    }

    #[test]
    fn deny_write_globs_drop_the_subpath_an_escape_hatch_releases() {
        let git_dir = ws().join(".git");
        let git_dirs = std::slice::from_ref(&git_dir);

        let hooks_only = deny_write_globs_with(
            &ws(),
            git_dirs,
            HandoffAllowances {
                git_hooks: true,
                project_tool_config: false,
            },
        );
        assert!(
            !hooks_only.contains(&git_dir.join("hooks")),
            "allow_git_hooks must remove the kernel deny too: {hooks_only:?}"
        );
        assert!(hooks_only.contains(&ws().join(".ahma")), "{hooks_only:?}");

        let config_only = deny_write_globs_with(
            &ws(),
            git_dirs,
            HandoffAllowances {
                git_hooks: false,
                project_tool_config: true,
            },
        );
        assert!(
            config_only.contains(&git_dir.join("hooks")),
            "{config_only:?}"
        );
        assert!(
            !config_only.contains(&ws().join(".ahma")),
            "allow_project_tool_config must remove the kernel deny too: {config_only:?}"
        );

        assert!(
            deny_write_globs_with(
                &ws(),
                git_dirs,
                HandoffAllowances {
                    git_hooks: true,
                    project_tool_config: true,
                }
            )
            .is_empty(),
            "with both hatches on there is nothing left to deny"
        );
    }

    /// The timing contract documented in the module header, without a
    /// subprocess: resolution is a snapshot, so a repository that appears after
    /// one resolution is absent from that result and present in the next. The
    /// Seatbelt profile is regenerated per spawn, which is why "the next
    /// resolution" means "the next command".
    #[test]
    fn deny_write_globs_reflects_a_repository_created_after_the_previous_resolution() {
        let tmp = tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let policy = HandoffAllowances::default();

        // Command N's profile is built here — the clone has not happened yet.
        let before = deny_write_globs_with(&root, &resolve_git_dirs(&root), policy);
        assert!(
            before.iter().all(|p| !p.ends_with("hooks")),
            "no repository exists yet, so no hooks deny: {before:?}"
        );

        // Command N clones a repo. Its own profile is already fixed: Seatbelt
        // profiles are immutable for the life of the sandboxed process, so the
        // hooks dir stays writable for the rest of command N.
        std::fs::create_dir_all(root.join("cloned/.git/hooks")).unwrap();
        assert_eq!(
            before,
            deny_write_globs_with(&root, &resolve_git_dirs_snapshot(&before), policy),
            "command N's already-generated profile cannot grow a rule"
        );

        // Command N+1 re-resolves and picks it up.
        let after = deny_write_globs_with(&root, &resolve_git_dirs(&root), policy);
        let expected = dunce::canonicalize(root.join("cloned/.git"))
            .unwrap()
            .join("hooks");
        assert!(
            after.contains(&expected),
            "the next command's profile must deny the cloned repo's hooks: {after:?}"
        );

        // Depth is the documented limit: two levels down is never resolved.
        std::fs::create_dir_all(root.join("a/b/deep/.git/hooks")).unwrap();
        let deep = deny_write_globs_with(&root, &resolve_git_dirs(&root), policy);
        assert!(
            !deep.iter().any(|p| p.to_string_lossy().contains("deep")),
            "resolve_git_dirs scans root + one level only: {deep:?}"
        );
    }

    /// Helper for the timing test: reconstruct the git dirs implied by an
    /// already-computed deny set, i.e. what command N's frozen profile knew.
    fn resolve_git_dirs_snapshot(deny_set: &[PathBuf]) -> Vec<PathBuf> {
        deny_set
            .iter()
            .filter(|p| p.ends_with("hooks"))
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect()
    }

    #[test]
    fn deny_write_globs_cover_hooks_and_ahma() {
        let git_dir = ws().join(".git");
        let alt = ws().join(".git-alt");
        let globs = deny_write_globs(&ws(), &[git_dir.clone(), alt.clone()]);
        assert!(globs.contains(&git_dir.join("hooks")), "{globs:?}");
        assert!(globs.contains(&alt.join("hooks")), "{globs:?}");
        assert!(globs.contains(&ws().join(".ahma")), "{globs:?}");
    }

    #[test]
    fn gitdir_pointer_parses_absolute_and_relative_targets() {
        let base = test_abs(&["work", "repo"]);
        let abs_target = test_abs(&["elsewhere", "git-alt"]);
        let parsed = parse_gitdir_pointer(&format!("gitdir: {}\n", abs_target.display()), &base)
            .expect("absolute pointer parses");
        assert_eq!(parsed, abs_target);

        let rel = parse_gitdir_pointer("gitdir: ../shared/.git\n", &base)
            .expect("relative pointer parses");
        assert_eq!(rel, test_abs(&["work", "shared", ".git"]));

        assert_eq!(parse_gitdir_pointer("not a pointer", &base), None);
        assert_eq!(parse_gitdir_pointer("gitdir:", &base), None);
    }

    #[test]
    fn resolve_git_dirs_finds_a_plain_git_directory() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();

        let dirs = resolve_git_dirs(root);
        let expected = dunce::canonicalize(root.join(".git")).unwrap();
        assert!(dirs.contains(&expected), "{dirs:?}");
    }

    /// The real `--separate-git-dir` shape: `.git` is a *file* pointing
    /// elsewhere. Resolution must follow it, and the hooks under the resolved
    /// directory must then classify as DenyWrite.
    #[test]
    fn resolve_git_dirs_follows_a_separate_git_dir_pointer_file() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let alt = root.join("git-alt");
        std::fs::create_dir_all(alt.join("hooks")).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", alt.display())).unwrap();

        let dirs = resolve_git_dirs(root);
        let expected = dunce::canonicalize(&alt).unwrap();
        assert!(dirs.contains(&expected), "resolved dirs: {dirs:?}");

        let hook = expected.join("hooks/post-checkout");
        let canonical_root = dunce::canonicalize(root).unwrap();
        assert_eq!(
            classify(&hook, &canonical_root, &dirs).map(|h| h.0),
            Some(ExecConfigClass::DenyWrite),
            "hooks under the relocated git dir must be denied"
        );
    }

    #[test]
    fn resolve_git_dirs_follows_a_relative_pointer_file() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("git-alt/hooks")).unwrap();
        std::fs::write(root.join(".git"), "gitdir: git-alt\n").unwrap();

        let dirs = resolve_git_dirs(root);
        let expected = dunce::canonicalize(root.join("git-alt")).unwrap();
        assert!(dirs.contains(&expected), "resolved dirs: {dirs:?}");
    }

    #[test]
    fn resolve_git_dirs_follows_worktree_commondir() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        // Simulate `git worktree add`: `.git` file -> main/.git/worktrees/wt,
        // whose `commondir` points back at the shared git dir holding hooks.
        let shared = root.join("main/.git");
        std::fs::create_dir_all(shared.join("hooks")).unwrap();
        let wt_meta = shared.join("worktrees/wt");
        std::fs::create_dir_all(&wt_meta).unwrap();
        std::fs::write(wt_meta.join("commondir"), "../..\n").unwrap();

        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_meta.display())).unwrap();

        let dirs = resolve_git_dirs(&wt);
        let expected = dunce::canonicalize(&shared).unwrap();
        assert!(
            dirs.contains(&expected),
            "worktree commondir must resolve to the shared git dir: {dirs:?}"
        );
    }

    #[test]
    fn resolve_git_dirs_finds_nested_repositories() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("packages/inner/.git/hooks")).unwrap();

        let dirs = resolve_git_dirs(&root.join("packages"));
        let expected = dunce::canonicalize(root.join("packages/inner/.git")).unwrap();
        assert!(dirs.contains(&expected), "{dirs:?}");
    }

    #[test]
    fn resolve_git_dirs_on_a_repo_less_directory_is_empty() {
        let tmp = tempdir().unwrap();
        assert!(resolve_git_dirs(tmp.path()).is_empty());
    }

    #[test]
    fn git_dir_resolution_prefilter_only_fires_where_it_can_matter() {
        assert!(needs_git_dir_resolution(Path::new(".git/hooks/pre-commit")));
        assert!(needs_git_dir_resolution(Path::new(".git-alt/hooks/x")));
        assert!(needs_git_dir_resolution(Path::new(".git/config")));
        assert!(!needs_git_dir_resolution(Path::new("src/main.rs")));
        assert!(!needs_git_dir_resolution(Path::new(".vscode/tasks.json")));
    }

    /// The async resolver must agree with the sync one — they are the same rule
    /// expressed twice (blocking vs `tokio::fs`), and a divergence would mean the
    /// write tools and the Seatbelt profile defend different sets.
    #[tokio::test]
    async fn async_resolver_matches_the_sync_resolver() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let alt = root.join("git-alt");
        std::fs::create_dir_all(alt.join("hooks")).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", alt.display())).unwrap();
        std::fs::create_dir_all(root.join("packages/.git/hooks")).unwrap();

        let mut sync_dirs = resolve_git_dirs(root);
        let mut async_dirs = resolve_git_dirs_async(root).await;
        sync_dirs.sort();
        async_dirs.sort();
        assert_eq!(
            sync_dirs, async_dirs,
            "sync and async resolution must agree"
        );
        assert!(
            sync_dirs.contains(&dunce::canonicalize(&alt).unwrap()),
            "{sync_dirs:?}"
        );
    }
}
