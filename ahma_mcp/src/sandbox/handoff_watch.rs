//! Detection of writes to the trust-handoff deny tier where the kernel does not
//! stop them (SPEC R6.1.7, R-HANDOFF.4).
//!
//! The deny tier — every resolved `<git_dir>/hooks` and the project's own
//! `<workspace>/.ahma` ([`super::exec_config::deny_write_globs`]) — is a hole
//! *inside* the writable workspace. macOS Seatbelt can subtract it at the
//! kernel. Landlock cannot: it grants per hierarchy and has no deny rule, so a
//! `run_terminal_command` child inherits the workspace-wide write right and can
//! write `.git/hooks/pre-commit`, which the user's own `git` later runs outside
//! any sandbox. Windows has no filesystem boundary at all (R6.3.9). Kernel
//! prevention on Linux needs user and mount namespaces, which stock Ubuntu 24.04
//! denies to unprivileged processes.
//!
//! So where the kernel does not hold the tier, ahma **detects** instead: it takes
//! a bounded inventory of the deny-tier paths just before a command starts and
//! again once its process tree is gone, and any created, modified or removed
//! entry — or one that newly became executable — is a trust-handoff alert. The
//! alert leads the tool result, goes to the log at `warn`, and lands in the
//! execution audit log as `handoff_write` (R-HANDOFF.10). Nothing is reverted:
//! the change may be the user's own concurrent edit, and silently undoing it
//! would be worse than saying so.
//!
//! ## What it costs, and the bounds
//!
//! * No walk beyond the deny-tier directories, and none at all where the tier is
//!   kernel-enforced ([`deny_tier_kernel_enforced`]).
//! * Each target is inventoried in path order, at most [`MAX_INVENTORY_ENTRIES`]
//!   entries and [`MAX_DEPTH`] levels deep. A target that holds more is
//!   *capped*: changes past the last entry looked at are not compared, and the
//!   alert says so on every command while it stays that way — a planted pile of
//!   files must not quietly push a real hook out of view.
//! * Entries are compared by size, mtime, permission bits and, on Unix, inode
//!   number and inode-change time. `touch -r` can forge an mtime; it cannot forge
//!   a ctime, and a `mv` over the file changes the inode.
//!
//! ## What it does not cover
//!
//! * **The same floor as the kernel rules (R-HANDOFF.2).** Targets are resolved
//!   when the command starts, so a repository created *by* the command is
//!   watched from the next command on.
//! * **Attribution.** Another process (the user's editor, another agent) may make
//!   the change during the window; the alert says so rather than blaming the
//!   command.
//! * **ahma's own writes** are excluded: its log directory (which defaults to
//!   `<workspace>/.ahma/logs` and receives spill files and the audit log while a
//!   command runs), the `.ahma/.gitignore` it maintains for that directory, and
//!   the user-level `~/.ahma` control plane, in case a scope is the home
//!   directory itself. None of these is a tool definition or a hook.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};

use super::Sandbox;
use super::exec_config;

/// At most this many entries are inventoried per deny-tier target. A hooks
/// directory holds about fourteen sample files; a project's `.ahma/` a few tool
/// definitions. Five hundred is generous for both and keeps a snapshot to a few
/// hundred `lstat` calls.
pub const MAX_INVENTORY_ENTRIES: usize = 500;

/// How far below a target the inventory descends. Git runs only top-level hooks
/// and ahma reads tool definitions from the top of `.ahma/`; the bound keeps a
/// planted deep tree from turning a snapshot into a walk.
pub const MAX_DEPTH: usize = 4;

/// A directory listing longer than this is not sorted at all: the target is
/// capped at that directory instead. Sorting is what makes the cap
/// deterministic, and sorting needs the whole listing.
const MAX_LISTING: usize = 10 * MAX_INVENTORY_ENTRIES;

/// At most this many writes are named in a tool result. The audit log records
/// every one.
pub const MAX_NAMED_IN_RESULT: usize = 20;

/// The fixed prefix of each alert line. Fixed so that a human, a test, or a
/// harness hook can find it with a plain substring search.
pub const ALERT_PREFIX: &str = "TRUST-HANDOFF WRITE:";

/// The fixed prefix of the line that says a target was too large to inventory
/// in full.
pub const INCOMPLETE_PREFIX: &str = "TRUST-HANDOFF WATCH INCOMPLETE:";

/// What will execute a changed file, and what to do about it, per deny-tier
/// target. Worded for the person reading the tool result.
mod triggers {
    pub const GIT_HOOKS: &str =
        "git runs files in .git/hooks outside any sandbox; review before your next git command";
    pub const PROJECT_TOOL_CONFIG: &str = "ahma loads tool definitions from .ahma/ on its next start or `restart` and runs the commands they define; review before restarting ahma";
}

/// Whether ahma watches the deny tier across a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HandoffWatchMode {
    /// Watch exactly where the deny tier is not kernel-enforced
    /// ([`deny_tier_kernel_enforced`]). The production default.
    #[default]
    Auto,
    /// Always watch, whatever the platform. For tests, and for embedders that
    /// want the record regardless.
    Always,
    /// Never watch.
    Never,
}

impl HandoffWatchMode {
    /// Whether a command run under `sandbox` is watched.
    pub fn is_active(self, sandbox: &Sandbox) -> bool {
        match self {
            Self::Auto => !deny_tier_kernel_enforced(sandbox),
            Self::Always => true,
            Self::Never => false,
        }
    }
}

/// Whether the kernel itself refuses writes to the deny tier for commands run
/// under `sandbox`. True only for an enforcing macOS Seatbelt sandbox, whose
/// profile ends with `(deny file-write* (subpath …))` for every deny-tier path
/// (R-HANDOFF.4). Linux (Landlock, R6.1.7) and Windows (no boundary, R6.3.9)
/// never enforce it, and neither does a sandbox that is not enforcing at all
/// (`--no-sandbox`, or deferring to a host sandbox, R7.6).
pub fn deny_tier_kernel_enforced(sandbox: &Sandbox) -> bool {
    cfg!(target_os = "macos") && sandbox.is_enforced()
}

/// How a deny-tier entry changed while a command ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffChangeKind {
    /// The entry did not exist before.
    Created,
    /// Its contents, identity or permissions changed.
    Modified,
    /// It existed before and is gone.
    Removed,
    /// It gained an execute bit (Unix). Reported instead of `Modified`,
    /// because it is the change that turns a file into something git runs.
    MadeExecutable,
}

impl HandoffChangeKind {
    /// The word used in the tool result.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Removed => "removed",
            Self::MadeExecutable => "made executable",
        }
    }

    /// The value recorded in the audit log and the result JSON.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Removed => "removed",
            Self::MadeExecutable => "made_executable",
        }
    }
}

/// One changed deny-tier entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffChange {
    /// Absolute path of the entry.
    pub path: PathBuf,
    /// What happened to it.
    pub kind: HandoffChangeKind,
    /// What will execute it, and what to do about it.
    pub trigger: &'static str,
}

/// Everything a watch found. Empty when nothing changed and nothing was capped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoffReport {
    /// Changed entries, in path order within each target.
    pub changes: Vec<HandoffChange>,
    /// Targets too large to inventory in full.
    pub capped: Vec<PathBuf>,
}

impl HandoffReport {
    /// Whether there is nothing to report.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.capped.is_empty()
    }

    /// The text that leads the tool result: one fixed-form line per change (at
    /// most [`MAX_NAMED_IN_RESULT`]), one per capped target, then one sentence
    /// saying what this is and is not. Empty for an empty report.
    pub fn render_alert(&self) -> String {
        let mut lines: Vec<String> = self
            .changes
            .iter()
            .take(MAX_NAMED_IN_RESULT)
            .map(|c| {
                format!(
                    "{ALERT_PREFIX} {} ({}) — {}",
                    c.path.display(),
                    c.kind.as_str(),
                    c.trigger
                )
            })
            .collect();
        if self.changes.len() > MAX_NAMED_IN_RESULT {
            lines.push(format!(
                "{ALERT_PREFIX} (+{} more; every one is in the execution audit log)",
                self.changes.len() - MAX_NAMED_IN_RESULT
            ));
        }
        for target in &self.capped {
            lines.push(format!(
                "{INCOMPLETE_PREFIX} {} holds more than {MAX_INVENTORY_ENTRIES} entries; \
                 changes past the first {MAX_INVENTORY_ENTRIES} (in path order) were not checked",
                target.display()
            ));
        }
        if !self.changes.is_empty() {
            lines.push(
                "Detected after the command ran, not prevented: on this platform the kernel does \
                 not stop writes to these paths (SPEC R6.1.7). Another process, such as your \
                 editor, may have made the change. Nothing was reverted."
                    .to_string(),
            );
        }
        lines.join("\n")
    }

    /// The machine-readable form carried in the operation result as
    /// `handoff_writes`.
    pub fn to_json(&self) -> Value {
        json!({
            "changes": self.changes.iter().map(|c| json!({
                "path": c.path.to_string_lossy(),
                "change": c.kind.wire(),
                "trigger": c.trigger,
            })).collect::<Vec<_>>(),
            "capped": self.capped.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        })
    }
}

/// What is compared about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryMeta {
    len: u64,
    modified: Option<SystemTime>,
    /// Unix permission bits; the read-only flag elsewhere.
    mode: u32,
    /// Unix `(ctime, ctime_nsec, ino)`: what a forged mtime or a rename-over
    /// cannot keep stable.
    stamp: Option<(i64, i64, u64)>,
}

impl EntryMeta {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let (mode, stamp) = {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            (
                meta.permissions().mode(),
                Some((meta.ctime(), meta.ctime_nsec(), meta.ino())),
            )
        };
        #[cfg(not(unix))]
        let (mode, stamp) = (u32::from(meta.permissions().readonly()), None);
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            mode,
            stamp,
        }
    }

    fn executable(&self) -> bool {
        cfg!(unix) && self.mode & 0o111 != 0
    }
}

/// One target's inventory: every non-directory entry looked at, keyed by path.
#[derive(Debug, Clone, Default)]
struct Inventory {
    entries: BTreeMap<PathBuf, EntryMeta>,
    /// Set when the cap was reached: the last path looked at. Entries are
    /// visited in path order, so nothing after it was seen.
    horizon: Option<PathBuf>,
}

impl Inventory {
    /// Inventory `target` in path order (a pre-order walk with sorted children
    /// visits paths in exactly `PathBuf`'s component-wise order), skipping
    /// anything under `exclude`. Directories are descended into, not recorded:
    /// git and ahma execute files, and recording a directory's mtime would turn
    /// every file creation into two alerts. Symlinks are recorded, never
    /// followed.
    async fn take(target: &Path, exclude: &[PathBuf], cap: usize) -> Self {
        let mut inv = Self::default();
        let mut stack: Vec<(PathBuf, usize)> = vec![(target.to_path_buf(), 0)];
        while let Some((path, depth)) = stack.pop() {
            if exclude.iter().any(|e| path.starts_with(e)) {
                continue;
            }
            let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
                continue;
            };
            if meta.is_dir() {
                if depth >= MAX_DEPTH {
                    continue;
                }
                match list_sorted(&path).await {
                    Some(children) => {
                        stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
                    }
                    None => {
                        // Too long to sort: nothing at or below it is compared.
                        inv.horizon = Some(path);
                        break;
                    }
                }
                continue;
            }
            if inv.entries.len() >= cap {
                inv.horizon = inv.entries.keys().next_back().cloned().or(Some(path));
                break;
            }
            inv.entries.insert(path, EntryMeta::of(&meta));
        }
        inv
    }
}

/// The entries of `dir`, sorted; `None` when there are more than
/// [`MAX_LISTING`]. An unreadable directory lists as empty.
async fn list_sorted(dir: &Path) -> Option<Vec<PathBuf>> {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return Some(Vec::new());
    };
    let mut children = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        if children.len() >= MAX_LISTING {
            return None;
        }
        children.push(entry.path());
    }
    children.sort();
    Some(children)
}

/// Compare two inventories of one target, appending what changed to `out`.
/// Pure. When either side was capped, only paths up to the earlier horizon are
/// compared: past it, an entry missing from one side may simply not have been
/// looked at.
fn diff_inventory(
    before: &Inventory,
    after: &Inventory,
    trigger: &'static str,
    out: &mut Vec<HandoffChange>,
) {
    let horizon = match (&before.horizon, &after.horizon) {
        (Some(a), Some(b)) => Some(std::cmp::min(a, b)),
        (Some(h), None) | (None, Some(h)) => Some(h),
        (None, None) => None,
    };
    let within = |p: &PathBuf| horizon.is_none_or(|h| p <= h);
    let mut push = |path: &PathBuf, kind| {
        out.push(HandoffChange {
            path: path.clone(),
            kind,
            trigger,
        })
    };
    for (path, now) in after.entries.iter().filter(|(p, _)| within(p)) {
        match before.entries.get(path) {
            None => push(path, HandoffChangeKind::Created),
            Some(was) if was == now => {}
            Some(was) if !was.executable() && now.executable() => {
                push(path, HandoffChangeKind::MadeExecutable)
            }
            Some(_) => push(path, HandoffChangeKind::Modified),
        }
    }
    for path in before
        .entries
        .keys()
        .filter(|p| within(p) && !after.entries.contains_key(*p))
    {
        push(path, HandoffChangeKind::Removed);
    }
}

/// A canonical spelling of `p` even when it does not exist yet: canonicalize the
/// path, else its parent plus its name, else keep it as given. Comparisons below
/// are against canonical roots, so a raw `/tmp/…` would never match
/// `/private/tmp/…`.
async fn canonical_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = tokio::fs::canonicalize(p).await {
        return dunce::simplified(&c).to_path_buf();
    }
    if let (Some(parent), Some(name)) = (p.parent(), p.file_name())
        && let Ok(c) = tokio::fs::canonicalize(parent).await
    {
        return dunce::simplified(&c).join(name);
    }
    p.to_path_buf()
}

/// Whether a deny-tier target is a project's tool-config directory rather than
/// a hooks directory — the only two shapes [`exec_config::deny_write_globs`]
/// returns.
fn is_project_tool_config(target: &Path) -> bool {
    target
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(".ahma"))
}

fn trigger_for(target: &Path) -> &'static str {
    if is_project_tool_config(target) {
        triggers::PROJECT_TOOL_CONFIG
    } else {
        triggers::GIT_HOOKS
    }
}

/// The deny-tier targets for commands that may write `roots`: every resolved
/// `<git_dir>/hooks` and every `<root>/.ahma`, under the installed escape-hatch
/// policy — the same set, from the same function, as the Seatbelt deny rules
/// (`exec_config::deny_write_globs`). A path an operator opted into writing
/// (`--allow-git-hooks`, `--allow-project-tool-config`) is not watched either.
/// Sorted and de-duplicated.
pub async fn deny_tier_targets(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        let root = canonical_lenient(root).await;
        let git_dirs = exec_config::resolve_git_dirs_async(&root).await;
        for target in exec_config::deny_write_globs(&root, &git_dirs) {
            let target = canonical_lenient(&target).await;
            if !out.contains(&target) {
                out.push(target);
            }
        }
    }
    out.sort();
    out
}

/// Paths ahma itself writes while a command runs, which must never raise an
/// alert. See the module doc.
async fn ahma_owned_paths(targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let log_dir = crate::utils::logging::project_log_dir();
    out.push(canonical_lenient(&log_dir).await);
    out.push(log_dir);
    for target in targets.iter().filter(|t| is_project_tool_config(t)) {
        out.push(target.join("logs"));
        out.push(target.join(".gitignore"));
    }
    if let Some(home) = ahma_common::config::ahma_home_dir() {
        let control_plane = home.join(".ahma");
        out.push(canonical_lenient(&control_plane).await);
        out.push(control_plane);
    }
    out
}

/// A deny-tier inventory taken before a command, to be compared with one taken
/// after it ([`Self::finish`]).
#[derive(Debug)]
pub struct HandoffWatch {
    targets: Vec<PathBuf>,
    exclude: Vec<PathBuf>,
    cap: usize,
    before: Vec<Inventory>,
}

impl HandoffWatch {
    /// Resolve the deny-tier targets for `roots` (the writable scopes plus the
    /// command's working directory, as the Seatbelt profile resolves them) and
    /// take the "before" inventory.
    pub async fn begin(roots: &[PathBuf]) -> Self {
        Self::begin_with_cap(roots, MAX_INVENTORY_ENTRIES).await
    }

    /// [`Self::begin`] with an explicit per-target entry cap, so a test can
    /// exercise the cap without creating hundreds of files.
    pub async fn begin_with_cap(roots: &[PathBuf], cap: usize) -> Self {
        let targets = deny_tier_targets(roots).await;
        let exclude = ahma_owned_paths(&targets).await;
        let before = inventory_all(&targets, &exclude, cap).await;
        Self {
            targets,
            exclude,
            cap,
            before,
        }
    }

    /// The deny-tier targets this watch covers.
    pub fn targets(&self) -> &[PathBuf] {
        &self.targets
    }

    /// Take the "after" inventory of the same targets and report what changed.
    pub async fn finish(self) -> HandoffReport {
        let after = inventory_all(&self.targets, &self.exclude, self.cap).await;
        let mut report = HandoffReport::default();
        for ((target, before), after) in self.targets.iter().zip(&self.before).zip(&after) {
            diff_inventory(before, after, trigger_for(target), &mut report.changes);
            if before.horizon.is_some() || after.horizon.is_some() {
                report.capped.push(target.clone());
            }
        }
        report
    }
}

async fn inventory_all(targets: &[PathBuf], exclude: &[PathBuf], cap: usize) -> Vec<Inventory> {
    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        out.push(Inventory::take(target, exclude, cap).await);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(len: u64, mode: u32) -> EntryMeta {
        EntryMeta {
            len,
            modified: None,
            mode,
            stamp: None,
        }
    }

    fn inv(entries: &[(&str, EntryMeta)], horizon: Option<&str>) -> Inventory {
        Inventory {
            entries: entries
                .iter()
                .map(|(p, m)| (PathBuf::from(p), m.clone()))
                .collect(),
            horizon: horizon.map(PathBuf::from),
        }
    }

    fn diff(before: &Inventory, after: &Inventory) -> Vec<(String, HandoffChangeKind)> {
        let mut out = Vec::new();
        diff_inventory(before, after, triggers::GIT_HOOKS, &mut out);
        out.into_iter()
            .map(|c| (c.path.to_string_lossy().into_owned(), c.kind))
            .collect()
    }

    #[test]
    fn an_unchanged_inventory_reports_nothing() {
        let a = inv(&[("/h/pre-commit.sample", meta(10, 0o755))], None);
        assert!(diff(&a, &a.clone()).is_empty());
    }

    #[test]
    fn created_modified_and_removed_entries_are_each_named() {
        let before = inv(&[("/h/a", meta(1, 0o644)), ("/h/b", meta(1, 0o644))], None);
        let after = inv(&[("/h/a", meta(2, 0o644)), ("/h/c", meta(1, 0o644))], None);
        assert_eq!(
            diff(&before, &after),
            vec![
                ("/h/a".into(), HandoffChangeKind::Modified),
                ("/h/c".into(), HandoffChangeKind::Created),
                ("/h/b".into(), HandoffChangeKind::Removed),
            ]
        );
    }

    #[test]
    fn gaining_an_execute_bit_is_its_own_kind() {
        let before = inv(&[("/h/pre-push", meta(5, 0o644))], None);
        let after = inv(&[("/h/pre-push", meta(5, 0o755))], None);
        let expected = if cfg!(unix) {
            HandoffChangeKind::MadeExecutable
        } else {
            HandoffChangeKind::Modified
        };
        assert_eq!(
            diff(&before, &after),
            vec![("/h/pre-push".into(), expected)]
        );
    }

    /// The reason for the horizon: past the earlier cap, "missing on one side"
    /// means "not looked at", not "created" or "removed".
    #[test]
    fn nothing_past_the_horizon_is_compared() {
        let before = inv(
            &[("/h/a", meta(1, 0o644)), ("/h/b", meta(1, 0o644))],
            Some("/h/b"),
        );
        // A new early entry pushed `b` past the cap afterwards.
        let after = inv(
            &[("/h/0", meta(1, 0o644)), ("/h/a", meta(1, 0o644))],
            Some("/h/a"),
        );
        assert_eq!(
            diff(&before, &after),
            vec![("/h/0".into(), HandoffChangeKind::Created)],
            "`b` lies past the horizon `a` and must not be reported removed"
        );
    }

    #[test]
    fn the_alert_has_a_fixed_form_and_says_what_it_is_not() {
        let report = HandoffReport {
            changes: vec![HandoffChange {
                path: PathBuf::from("/ws/.git/hooks/pre-commit"),
                kind: HandoffChangeKind::Created,
                trigger: triggers::GIT_HOOKS,
            }],
            capped: vec![],
        };
        let alert = report.render_alert();
        assert!(
            alert.starts_with(
                "TRUST-HANDOFF WRITE: /ws/.git/hooks/pre-commit (created) — git runs files in \
                 .git/hooks outside any sandbox; review before your next git command"
            ),
            "{alert}"
        );
        assert!(alert.contains("not prevented") && alert.contains("Nothing was reverted"));
        assert_eq!(report.to_json()["changes"][0]["change"], "created");
    }

    #[test]
    fn a_long_list_is_bounded_in_the_result() {
        let changes = (0..MAX_NAMED_IN_RESULT + 5)
            .map(|i| HandoffChange {
                path: PathBuf::from(format!("/ws/.ahma/t{i}.json")),
                kind: HandoffChangeKind::Created,
                trigger: triggers::PROJECT_TOOL_CONFIG,
            })
            .collect();
        let alert = HandoffReport {
            changes,
            capped: vec![],
        }
        .render_alert();
        assert_eq!(
            alert.matches("(created)").count(),
            MAX_NAMED_IN_RESULT,
            "{alert}"
        );
        assert!(alert.contains("+5 more"), "{alert}");
    }

    #[test]
    fn an_empty_report_renders_nothing() {
        assert!(HandoffReport::default().is_empty());
        assert!(HandoffReport::default().render_alert().is_empty());
    }

    #[test]
    fn targets_are_told_apart_by_shape() {
        assert_eq!(
            trigger_for(Path::new("/ws/.ahma")),
            triggers::PROJECT_TOOL_CONFIG
        );
        assert_eq!(
            trigger_for(Path::new("/ws/.git/hooks")),
            triggers::GIT_HOOKS
        );
    }

    #[tokio::test]
    async fn the_inventory_skips_excluded_paths_and_records_files_not_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("logs/operations")).unwrap();
        std::fs::write(root.join("logs/operations/op.log"), "x").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/tool.json"), "{}").unwrap();

        let inv = Inventory::take(&root, &[root.join("logs")], MAX_INVENTORY_ENTRIES).await;
        let paths: Vec<_> = inv.entries.keys().cloned().collect();
        assert_eq!(paths, vec![root.join("sub/tool.json")]);
        assert!(inv.horizon.is_none());
    }

    #[tokio::test]
    async fn the_inventory_stops_at_the_cap_in_path_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        for name in ["d", "b", "a", "c"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let inv = Inventory::take(&root, &[], 2).await;
        let paths: Vec<_> = inv.entries.keys().cloned().collect();
        assert_eq!(paths, vec![root.join("a"), root.join("b")]);
        assert_eq!(inv.horizon, Some(root.join("b")));
    }
}
