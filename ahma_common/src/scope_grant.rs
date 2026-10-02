//! Scope-grant coordination (the *automatic* half of the persistent-scope flow).
//!
//! PR #292 shipped the durable foundation: a human runs `ahma sandbox grant <dir>`
//! and the directory is persisted to `~/.ahma/settings.toml`
//! ([`crate::config::SandboxSettings::grant_scope`]) so it survives `roots/list`
//! replacement. This module builds the layer that *raises that question
//! automatically*: when a sandboxed command trips an out-of-scope path, a detector
//! asks the human "grant access to X?" through some surface (TUI modal, MCP
//! `elicitation/create`), and on approval the answer is persisted here.
//!
//! ## The invariant that shapes this module
//!
//! *The committed workspace scope cannot be changed during a session* (SPEC
//! R5.1): nothing here re-locks or replaces it. A **human-approved** grant may be
//! *added* beside it — [`persist_grant`] writes the settings file, and the caller
//! that holds the live `Sandbox` (the broker, the TUI reporter, the
//! `sandbox_grant` handler) applies the same directory to the running session
//! (R5.4.6) and announces it. A `session`-tier answer is applied live only and
//! never written.
//!
//! ## Why a sibling, not an extension of [`crate::elicitation`]
//!
//! [`crate::elicitation::ElicitationDecision`] coordinates scope *downgrades*
//! (most-restrictive-wins, narrow-now). A grant is the opposite direction —
//! *widen-on-next-start* — so it gets its own coordinator rather than overloading
//! the downgrade fold.
//!
//! ## What [`GrantCoordinator`] guarantees
//!
//!  - **Dedup / debounce**: a given `(canonical_path, access)` is asked **at most
//!    once** per session. The same path trips the kernel many times;
//!    [`begin`](GrantCoordinator::begin) gates so only the first trip fans out a prompt.
//!  - **No re-ask loops**: after a Deny *or* a grant the `(path, access)` is added
//!    to a session dismiss list. A grant cannot widen the live session, so the path
//!    keeps tripping — suppressing the re-ask is what stops an ask→deny→ask storm.
//!  - **First-answer-wins**: when the same decision is fanned to several surfaces,
//!    the first answer resolves it; later answers are no-ops
//!    ([`resolve`](GrantCoordinator::resolve) is idempotent). Combined with
//!    [`crate::config::GrantOutcome::Updated`] this makes a near-simultaneous
//!    double-approve from two surfaces harmless.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{AhmaSettings, GrantOutcome, PersistentScope, ScopeAccess, expand_home};

/// Why a scope grant is being requested — carried to the surface for context and
/// recorded for auditability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantReason {
    /// A path argument / working-dir was rejected up front by path validation
    /// (`SandboxError::PathOutsideSandbox`). The offending path is known exactly.
    PreExecViolation,
    /// A sandboxed command failed and its stderr matched a kernel-denial signature
    /// from which a candidate path was extracted. The path is a *suggestion* — only
    /// the human's explicit approval persists anything.
    StderrHeuristic,
}

/// A request to **persist** a new sandbox scope grant (widen-on-next-start, never
/// live). Fanned to every capable surface under one `decision_id`.
///
/// `Serialize`/`Deserialize` so it can travel over the hub (TUI) and inside
/// an MCP `elicitation/create` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeGrantRequest {
    /// Correlates the fan-out and the answer; binds to the asking session.
    pub decision_id: String,
    /// The canonical offending directory (resolved, symlinks followed). Displayed
    /// literally at the surface — never the raw, spoofable string from stderr.
    pub path: PathBuf,
    /// The access the detector inferred is needed (the human may downgrade rw→ro,
    /// never the reverse without an explicit choice).
    pub access: ScopeAccess,
    /// Why this is being asked.
    pub reason: GrantReason,
    /// The tool/command that tripped the scope (e.g. `"run_terminal_command"`),
    /// for the prompt text and the grant's `granted_by` provenance.
    pub tool: Option<String>,
}

/// The human's answer at any surface. Three-valued — never a bool — because
/// "yes" must distinguish read-only from read+write, and the default/Enter choice
/// must be the safe `Deny` (SPEC R5.3.1: Enter must never widen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantDecision {
    /// Do not grant. Suppresses re-asking this `(path, access)` for the session.
    Deny,
    /// Grant read-only access, persisted (`always` tier).
    GrantRo,
    /// Grant read+write access, persisted (`always` tier).
    GrantRw,
    /// Grant read-only access for this session only — applied live, never written.
    GrantRoSession,
    /// Grant read+write access for this session only — applied live, never written.
    GrantRwSession,
}

impl GrantDecision {
    /// The [`ScopeAccess`] to apply, or `None` for [`GrantDecision::Deny`].
    pub fn access(self) -> Option<ScopeAccess> {
        match self {
            GrantDecision::Deny => None,
            GrantDecision::GrantRo | GrantDecision::GrantRoSession => Some(ScopeAccess::Ro),
            GrantDecision::GrantRw | GrantDecision::GrantRwSession => Some(ScopeAccess::Rw),
        }
    }

    /// How long the answer lasts (SPEC R-PERM.2): `always` is written to the
    /// settings file, `session` lives only in the running instance.
    pub fn tier(self) -> crate::permissions::GrantTier {
        match self {
            GrantDecision::GrantRoSession | GrantDecision::GrantRwSession => {
                crate::permissions::GrantTier::Session
            }
            _ => crate::permissions::GrantTier::Always,
        }
    }
}

/// What the caller should do after [`GrantCoordinator::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantResolveOutcome {
    /// Apply this grant: at the `always` tier write it via [`persist_grant`] and
    /// apply it live; at the `session` tier apply it live only. The access is the
    /// one the human chose, which may be narrower than the requested access.
    Persist {
        /// Canonical directory to grant.
        path: PathBuf,
        /// Access level the human approved.
        access: ScopeAccess,
        /// Tool that requested it, for `granted_by` provenance.
        tool: Option<String>,
        /// `always` (write it) or `session` (live only).
        tier: crate::permissions::GrantTier,
    },
    /// The human denied; nothing is persisted. The `(path, access)` is now dismissed
    /// for the session.
    Denied {
        /// The directory that was denied.
        path: PathBuf,
    },
    /// This `decision_id` was already resolved (a twin surface answered first).
    AlreadyResolved,
    /// This `decision_id` is not in flight (stale or never issued). Ignored.
    Unknown,
}

/// Coordinates in-flight scope-grant decisions across surfaces. Cheap to share
/// behind an `Arc`; all state is behind one mutex.
#[derive(Debug, Default)]
pub struct GrantCoordinator {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// In-flight decisions awaiting an answer, by `decision_id`.
    in_flight: HashMap<String, ScopeGrantRequest>,
    /// `(canonical_path, access)` currently awaiting an answer — the dedup gate.
    active_keys: HashSet<(PathBuf, ScopeAccess)>,
    /// `(canonical_path, access)` we will not ask about again this session
    /// (denied, or already granted-for-next-start).
    dismissed: HashSet<(PathBuf, ScopeAccess)>,
    /// `decision_id`s already resolved — makes [`GrantCoordinator::resolve`]
    /// idempotent and lets late twin answers no-op.
    resolved: HashSet<String>,
}

impl GrantCoordinator {
    /// Create an empty coordinator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a decision for `path` at `access`, or return `None` if it should not be
    /// asked: the `(canonical_path, access)` is already in flight, or was dismissed
    /// (denied / already granted) this session.
    ///
    /// On `Some`, a fresh `decision_id` is minted and the request is recorded as
    /// in-flight; the caller fans it out to every capable surface.
    pub fn begin(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    ) -> Option<ScopeGrantRequest> {
        self.begin_canonical(canonicalize_best_effort(path), access, reason, tool)
    }

    /// [`Self::begin`], given an already-canonicalized path — lets
    /// [`Self::reopen`] canonicalize once and reuse the result instead of
    /// canonicalizing again inside `begin`.
    fn begin_canonical(
        &self,
        canonical: PathBuf,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    ) -> Option<ScopeGrantRequest> {
        // SPEC R-PERM.4.3: a path the hard denylist refuses never becomes a
        // question. The prompt would be one whose right answer is always "no",
        // and a `session` answer to it would apply live, past `persist_grant`.
        if let Some(why) = refusal_reason(&canonical) {
            tracing::warn!(
                path = %canonical.display(),
                access = access.label(),
                "scope grant not raised: {why}"
            );
            return None;
        }
        let key = (canonical.clone(), access);
        let mut inner = self.inner.lock();
        if inner.dismissed.contains(&key) || inner.active_keys.contains(&key) {
            return None;
        }
        let decision_id = uuid::Uuid::new_v4().to_string();
        let req = ScopeGrantRequest {
            decision_id: decision_id.clone(),
            path: canonical,
            access,
            reason,
            tool,
        };
        inner.active_keys.insert(key);
        inner.in_flight.insert(decision_id, req.clone());
        Some(req)
    }

    /// Re-open a question the session already answered, at the user's explicit
    /// request (SPEC R-PERM.7.1: selecting a denied operation and confirming
    /// re-raises the grant question).
    ///
    /// This is the one sanctioned way past the ask-once memo, and it is safe
    /// precisely because it is not automatic: the memo exists so ahma does not
    /// *nag*, and a person deliberately choosing a denied row and confirming is
    /// not ahma nagging. Still returns `None` when the same `(path, access)` is
    /// already in flight — re-raising a live question would just duplicate the
    /// modal, not add information.
    pub fn reopen(
        &self,
        path: &Path,
        access: ScopeAccess,
        reason: GrantReason,
        tool: Option<String>,
    ) -> Option<ScopeGrantRequest> {
        let canonical = canonicalize_best_effort(path);
        {
            let mut inner = self.inner.lock();
            inner.dismissed.remove(&(canonical.clone(), access));
        }
        self.begin_canonical(canonical, access, reason, tool)
    }

    /// Resolve a decision with the human's answer. First-answer-wins and idempotent:
    /// a second call for the same `decision_id` returns [`GrantResolveOutcome::AlreadyResolved`].
    ///
    /// On any answer the `(path, access)` is dismissed for the session so the path —
    /// which the live sandbox still blocks until restart — does not re-prompt. A
    /// grant additionally dismisses the *other* access variant for the same path
    /// (granting rw subsumes a pending ro need, and vice-versa).
    pub fn resolve(&self, decision_id: &str, decision: GrantDecision) -> GrantResolveOutcome {
        let mut inner = self.inner.lock();
        if inner.resolved.contains(decision_id) {
            return GrantResolveOutcome::AlreadyResolved;
        }
        let Some(req) = inner.in_flight.remove(decision_id) else {
            return GrantResolveOutcome::Unknown;
        };
        inner.resolved.insert(decision_id.to_string());
        inner.active_keys.remove(&(req.path.clone(), req.access));

        match decision.access() {
            None => {
                // Deny: suppress re-asking exactly this (path, access).
                inner.dismissed.insert((req.path.clone(), req.access));
                GrantResolveOutcome::Denied { path: req.path }
            }
            Some(access) => {
                // Grant: suppress both access variants for this path — the live
                // session can't widen, so the path keeps tripping until restart.
                inner.dismissed.insert((req.path.clone(), ScopeAccess::Ro));
                inner.dismissed.insert((req.path.clone(), ScopeAccess::Rw));
                GrantResolveOutcome::Persist {
                    path: req.path,
                    access,
                    tool: req.tool,
                    tier: decision.tier(),
                }
            }
        }
    }

    /// Cancel a decision without an answer (e.g. the asking session terminated).
    /// Marks it resolved so a late answer no-ops, and frees its dedup key (the path
    /// is *not* dismissed — a future trip may legitimately re-ask). Returns the
    /// request if it was in flight.
    pub fn cancel(&self, decision_id: &str) -> Option<ScopeGrantRequest> {
        let mut inner = self.inner.lock();
        let req = inner.in_flight.remove(decision_id);
        if let Some(r) = &req {
            inner.active_keys.remove(&(r.path.clone(), r.access));
        }
        inner.resolved.insert(decision_id.to_string());
        req
    }

    /// Snapshot of the decisions currently awaiting an answer, for
    /// session-health disclosure (#485): the `grant_pending` event and the
    /// heartbeat `pending_grants` count. Order is unspecified.
    pub fn pending(&self) -> Vec<ScopeGrantRequest> {
        self.inner.lock().in_flight.values().cloned().collect()
    }

    /// Number of decisions currently awaiting an answer.
    pub fn pending_count(&self) -> usize {
        self.inner.lock().in_flight.len()
    }

    /// Whether `decision_id` is still awaiting an answer.
    pub fn is_in_flight(&self, decision_id: &str) -> bool {
        self.inner.lock().in_flight.contains_key(decision_id)
    }
}

/// Best-effort canonicalization for the dedup key and the displayed path: resolve
/// symlinks and `..` so two spellings of the same directory share one key and a
/// spoofed stderr path cannot masquerade. Mirrors the parent-canonicalize fallback
/// used by `path_security::validate_path` for paths that do not exist yet.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    let expanded = expand_home(path);
    if let Ok(c) = dunce::canonicalize(&expanded) {
        return c;
    }
    // Path may not exist (or a component is missing): canonicalize the deepest
    // existing ancestor and re-attach the remainder.
    match (expanded.parent(), expanded.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            match dunce::canonicalize(parent) {
                Ok(c) => c.join(name),
                Err(_) => expanded,
            }
        }
        _ => expanded,
    }
}

/// The risk tier of a proposed grant, decided purely from the path, the user's
/// home directory, and the live sandbox scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantRisk {
    /// Catastrophic and never legitimate — refused even with `confirm: true`.
    Refused(String),
    /// Allowed with confirmation, but each reason is surfaced loudly first.
    High(Vec<String>),
    /// An ordinary grant (a build cache, a dependency source dir, a sibling
    /// project, …).
    Normal,
}

/// Check if `candidate_parent` is the enclosing git repository root (or main repo of a worktree)
/// of `scope`. When true, granting `candidate_parent` is not widening above the workspace;
/// it is granting the workspace root itself.
pub fn is_enclosing_git_repo(scope: &Path, candidate_parent: &Path) -> bool {
    let canon_scope = dunce::canonicalize(scope).unwrap_or_else(|_| scope.to_path_buf());
    let canon_parent =
        dunce::canonicalize(candidate_parent).unwrap_or_else(|_| candidate_parent.to_path_buf());

    let mut dir = canon_scope.clone();
    while dir.starts_with(&canon_parent) {
        let git = dir.join(".git");
        if git.exists() {
            if dir == canon_parent {
                return true;
            }
            if git.is_file()
                && let Ok(content) = std::fs::read_to_string(&git)
                && let Some(line) = content
                    .lines()
                    .find(|l| l.trim_start().starts_with("gitdir:"))
            {
                let raw_gitdir = line.trim_start()["gitdir:".len()..].trim();
                let gitdir_path = Path::new(raw_gitdir);
                let resolved = if gitdir_path.is_relative() {
                    dir.join(gitdir_path)
                } else {
                    gitdir_path.to_path_buf()
                };
                let canon_gitdir = dunce::canonicalize(&resolved).unwrap_or(resolved);
                if let Some(main) = canon_gitdir
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|gp| gp.parent())
                {
                    let canon_main =
                        dunce::canonicalize(main).unwrap_or_else(|_| main.to_path_buf());
                    if canon_main == canon_parent {
                        return true;
                    }
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    false
}

/// Classify the risk of granting `path` — the hard denylist every write path
/// into the ledger is gated by (SPEC R5.4.5, R-PERM.2).
///
/// `path` is expected to arrive canonicalized (symlinks and `..` resolved as far as it exists).
/// `home` is canonicalized **here**, on purpose: the caller passes
/// `ahma_home_dir()`, which is whatever the OS reports and may contain a symlink
/// component — `/home` → `/mnt/home` on many Linux setups, an automounted
/// corporate home, or a macOS home relocated to another volume. Comparing a
/// resolved path against an unresolved `$HOME` makes every equality rule below
/// silently miss, and these rules are the *hard* denylist: `$HOME` itself,
/// `~/.ssh`, `~/.aws`, `~/.ahma`. A denylist that quietly stops matching is worse
/// than no denylist, because everything downstream assumes it held.
pub fn classify_grant_risk(path: &Path, home: Option<&Path>, scopes: &[PathBuf]) -> GrantRisk {
    let home = home.map(|h| dunce::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));
    let home = home.as_deref();

    // 1. A filesystem root has no parent — granting it exposes the whole drive.
    if path.parent().is_none() {
        return GrantRisk::Refused(
            "it is a filesystem root — granting it would expose the entire drive".to_string(),
        );
    }

    // 2. The exact home directory exposes every dotfile, key, and credential.
    if let Some(home) = home
        && path == home
    {
        return GrantRisk::Refused(
            "it is your home directory — granting it would expose every dotfile, key, and \
             credential under $HOME"
                .to_string(),
        );
    }

    // 3. A strict ancestor of a live scope would widen the sandbox above the
    //    workspace. (Equality is merely redundant — handled as a High warning.)
    for scope in scopes {
        if scope != path && scope.starts_with(path) {
            if is_enclosing_git_repo(scope, path) {
                // Not widening above the workspace; it is the enclosing workspace/repo root itself!
                continue;
            }
            return GrantRisk::Refused(format!(
                "it is a parent of the active sandbox scope {} — granting it would widen the \
                 sandbox above your workspace",
                scope.display()
            ));
        }
    }

    // 4. Credential directories and ahma's own settings directory.
    if let Some(home) = home {
        const SENSITIVE: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker", ".ahma"];
        if SENSITIVE.iter().any(|name| path == home.join(name))
            || path == home.join(".config").join("gh")
            || path == home.join(".config").join("gcloud")
        {
            return GrantRisk::Refused(format!(
                "'{}' holds credentials/secrets (or ahma's own settings) and must never be \
                 exposed to a sandboxed tool",
                path.display()
            ));
        }
    }

    // 5. OS system directories.
    if is_system_dir(path) {
        return GrantRisk::Refused(format!(
            "'{}' is a system directory — granting it is never required for a build and risks \
             the OS",
            path.display()
        ));
    }

    // ── Not refused: collect elevated-risk warnings. ──
    let mut warnings = Vec::new();

    if scopes.iter().any(|s| s == path) {
        warnings.push(
            "this path is already inside the active sandbox scope; the grant is redundant"
                .to_string(),
        );
    }

    // A direct child of the filesystem root (e.g. `/data`, `/opt`).
    if path.parent().is_some_and(|p| p.parent().is_none()) {
        warnings.push(format!(
            "'{}' sits directly under the filesystem root; double-check it is the specific \
             directory you mean",
            path.display()
        ));
    }

    if !path.exists() {
        warnings.push(format!(
            "'{}' does not exist on disk — confirm the path is correct and not a typo",
            path.display()
        ));
    }

    // A hidden directory directly under home that is not a known build cache.
    if let Some(home) = home
        && path.parent() == Some(home)
        && let Some(name) = path.file_name().and_then(|n| n.to_str())
        && name.starts_with('.')
        && !is_known_cache_dir(name)
    {
        warnings.push(format!(
            "'{}' is a hidden directory in your home folder; make sure it does not hold private \
             data",
            path.display()
        ));
    }

    if warnings.is_empty() {
        GrantRisk::Normal
    } else {
        GrantRisk::High(warnings)
    }
}

/// Whether `path` is exactly an OS system directory that must never be granted.
pub fn is_system_dir(path: &Path) -> bool {
    // Exact matches only: `/usr/local/foo` is a legitimate grant, `/usr` is not.
    const UNIX_SYSTEM_DIRS: &[&str] = &[
        "/etc", "/usr", "/bin", "/sbin", "/var", "/boot", "/dev", "/proc", "/sys", "/root",
        "/System", "/Library", "/opt", "/private",
    ];
    const WINDOWS_SYSTEM_DIRS: &[&str] = &[
        "C:\\Windows",
        "C:\\Program Files",
        "C:\\Program Files (x86)",
        "C:\\ProgramData",
    ];
    // macOS canonicalizes `/etc`, `/var` and `/tmp` to `/private/<dir>`, and
    // every caller that canonicalizes first would otherwise slip past an
    // exact-match list. Compare the path as given *and* with that prefix
    // removed, so `/private/etc` is `/etc`.
    let lexical = path
        .strip_prefix("/private")
        .ok()
        .map(|rest| Path::new("/").join(rest));
    UNIX_SYSTEM_DIRS
        .iter()
        .chain(WINDOWS_SYSTEM_DIRS)
        .any(|d| path == Path::new(d) || lexical.as_deref() == Some(Path::new(d)))
}

/// Whether `name` (a `~/<name>` hidden directory) is a well-known build cache,
/// in which case a hidden-home-dir grant is unremarkable rather than elevated.
pub fn is_known_cache_dir(name: &str) -> bool {
    const CACHES: &[&str] = &[
        ".cargo",
        ".rustup",
        ".cache",
        ".npm",
        ".gradle",
        ".m2",
        ".pub-cache",
        ".cocoapods",
        ".sccache",
        ".ccache",
        ".gem",
        ".yarn",
        ".pnpm-store",
        ".nuget",
        ".gradle-cache",
        ".deno",
        ".bun",
    ];
    CACHES.contains(&name)
}

/// Everything a write into the persistent-scope ledger needs to say about
/// itself: what is granted, and who decided it where.
#[derive(Debug, Clone)]
pub struct NewGrant<'a> {
    /// The directory to grant. `~` is expanded; the path is canonicalized as far
    /// as it exists before the denylist sees it.
    pub path: &'a Path,
    pub access: ScopeAccess,
    /// What asked for it (a tool or command name), for `granted_by` provenance.
    pub granted_by: Option<String>,
    /// `YYYY-MM-DD`, stamped by the caller (this crate carries no date dependency).
    pub granted_at: Option<String>,
    pub note: Option<String>,
    /// Which human surface answered: `cli`, `tui`, `harness` (an MCP
    /// elicitation). Recorded in the audit log; a grant with no human surface
    /// has no business here.
    pub surface: &'a str,
    /// The session's live sandbox scopes, so a grant that would widen the
    /// sandbox *above* the workspace is refused (`[]` when unknown — the
    /// remaining denylist rules still hold).
    pub live_scopes: &'a [PathBuf],
    /// The workspace this grant is for (SPEC R5.4.11); `None` writes a global
    /// grant, which only the CLI's explicit `--global` should ever ask for.
    pub workspace: Option<&'a Path>,
}

/// Whether a persistent grant made for `workspace` applies to a session whose
/// committed scopes are `scopes` (SPEC R5.4.11). A grant with no workspace is a
/// legacy global grant and applies everywhere. Otherwise it applies when a
/// committed scope lies inside the workspace, the workspace lies inside a
/// committed scope, or the workspace is the enclosing git repository of one —
/// the same project seen from a subdirectory or a worktree.
pub fn grant_applies(workspace: Option<&Path>, scopes: &[PathBuf]) -> bool {
    let Some(ws) = workspace else {
        return true;
    };
    let ws = canonicalize_best_effort(ws);
    scopes.iter().any(|s| {
        let s = canonicalize_best_effort(s);
        s.starts_with(&ws) || ws.starts_with(&s) || is_enclosing_git_repo(&s, &ws)
    })
}

/// Persist a **human-approved** grant to the settings file — the single
/// chokepoint every surface writes through: the CLI, an elicitation answer
/// relayed by the permission broker, the TUI modal. It applies the hard
/// denylist (R5.4.5 / R-PERM.2) and appends the audit record (R-PERM.2.1)
/// here, so no surface can skip either; it writes `~/.ahma/settings.toml`
/// and never touches the live sandbox (the caller that holds one decides
/// whether to apply the grant live, R5.4.6).
///
/// Uses the strict loader so a corrupt settings file is *not* silently clobbered.
/// Returns the [`GrantOutcome`] so the caller can report "added" vs "updated".
pub fn persist_grant(settings_file: &Path, grant: NewGrant<'_>) -> Result<GrantOutcome> {
    let canonical = canonicalize_best_effort(grant.path);
    let home = crate::config::ahma_home_dir();
    if let GrantRisk::Refused(reason) =
        classify_grant_risk(&canonical, home.as_deref(), grant.live_scopes)
    {
        anyhow::bail!(
            "refusing to grant {}: {reason}. This is a hard limit with no override; if a tool \
             genuinely needs something under that path, grant the specific subdirectory it \
             needs instead.",
            canonical.display()
        );
    }
    let mut settings = AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| {
            format!(
                "refusing to overwrite unparseable {}",
                settings_file.display()
            )
        })?;
    let outcome = settings.sandbox.grant_scope(PersistentScope {
        path: grant.path.to_path_buf(),
        access: grant.access,
        workspace: grant.workspace.map(canonicalize_best_effort),
        granted_by: grant.granted_by,
        granted_at: grant.granted_at.clone(),
        note: grant.note,
    });
    settings
        .save_to(settings_file)
        .with_context(|| format!("failed to write {}", settings_file.display()))?;
    crate::permissions::append_audit(&crate::permissions::audit_entry(
        grant.granted_at.unwrap_or_else(|| "unknown".to_string()),
        crate::permissions::AuditAction::Grant,
        crate::permissions::GrantKind::FsScope,
        canonical.display().to_string(),
        Some(if grant.access.is_write() { "rw" } else { "ro" }.to_string()),
        crate::permissions::GrantTier::Always,
        Some(grant.surface.to_string()),
    ));
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn coord() -> GrantCoordinator {
        GrantCoordinator::new()
    }

    #[test]
    fn begin_dedups_same_path_and_access() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        let first = c.begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None);
        assert!(first.is_some(), "first ask issues a decision");
        let second = c.begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None);
        assert!(
            second.is_none(),
            "second ask for an in-flight key is suppressed"
        );
    }

    #[test]
    fn begin_distinct_access_asks_again() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        assert!(
            c.begin(p, ScopeAccess::Ro, GrantReason::PreExecViolation, None)
                .is_some()
        );
        // A different access level is a different key → a fresh ask.
        assert!(
            c.begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
                .is_some()
        );
    }

    #[test]
    fn resolve_deny_dismisses_and_suppresses_reask() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        let req = c
            .begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
            .unwrap();
        let out = c.resolve(&req.decision_id, GrantDecision::Deny);
        assert!(matches!(out, GrantResolveOutcome::Denied { .. }));
        // Same (path, access) is now dismissed → no re-ask.
        assert!(
            c.begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
                .is_none()
        );
    }

    #[test]
    fn resolve_grant_returns_persist_and_suppresses_both_access_variants() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        let req = c
            .begin(
                p,
                ScopeAccess::Rw,
                GrantReason::PreExecViolation,
                Some("sccache".into()),
            )
            .unwrap();
        let out = c.resolve(&req.decision_id, GrantDecision::GrantRw);
        match out {
            GrantResolveOutcome::Persist {
                path,
                access,
                tool,
                tier,
            } => {
                assert_eq!(access, ScopeAccess::Rw);
                assert_eq!(tool.as_deref(), Some("sccache"));
                assert_eq!(path, canonicalize_best_effort(p));
                assert_eq!(tier, crate::permissions::GrantTier::Always);
            }
            other => panic!("expected Persist, got {other:?}"),
        }
        // A grant suppresses re-asking *either* access for this path.
        assert!(
            c.begin(p, ScopeAccess::Ro, GrantReason::PreExecViolation, None)
                .is_none()
        );
        assert!(
            c.begin(p, ScopeAccess::Rw, GrantReason::StderrHeuristic, None)
                .is_none()
        );
    }

    #[test]
    fn resolve_is_idempotent_first_answer_wins() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        let req = c
            .begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
            .unwrap();
        let first = c.resolve(&req.decision_id, GrantDecision::GrantRo);
        assert!(matches!(
            first,
            GrantResolveOutcome::Persist {
                access: ScopeAccess::Ro,
                ..
            }
        ));
        // A twin surface answering later is a no-op — the rw answer cannot override.
        let second = c.resolve(&req.decision_id, GrantDecision::GrantRw);
        assert_eq!(second, GrantResolveOutcome::AlreadyResolved);
    }

    #[test]
    fn resolve_unknown_decision_id_is_ignored() {
        let c = coord();
        assert_eq!(
            c.resolve("never-issued", GrantDecision::GrantRw),
            GrantResolveOutcome::Unknown
        );
    }

    #[test]
    fn cancel_frees_key_and_blocks_late_answer_without_dismissing() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        let req = c
            .begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
            .unwrap();
        let cancelled = c.cancel(&req.decision_id);
        assert_eq!(
            cancelled.as_ref().map(|r| &r.decision_id),
            Some(&req.decision_id)
        );
        // Late answer after cancel no-ops.
        assert_eq!(
            c.resolve(&req.decision_id, GrantDecision::GrantRw),
            GrantResolveOutcome::AlreadyResolved
        );
        // Not dismissed: a fresh trip may legitimately re-ask.
        assert!(
            c.begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
                .is_some()
        );
    }

    /// `reopen` is the user-initiated escape hatch from the ask-once memo
    /// (SPEC R-PERM.7.1): after a denial the path is dismissed for the session
    /// and `begin` correctly refuses to nag, but a person selecting the denied
    /// row and confirming must still get the question back.
    #[test]
    fn reopen_asks_again_after_the_session_dismissed_the_path() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();

        let req = c
            .begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
            .expect("first ask");
        c.resolve(&req.decision_id, GrantDecision::Deny);

        // The memo holds against an automatic re-ask …
        assert!(
            c.begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
                .is_none(),
            "a denied path must not re-prompt on its own"
        );

        // … and yields to an explicit human request.
        let again = c
            .reopen(p, ScopeAccess::Rw, GrantReason::StderrHeuristic, None)
            .expect("reopen must re-raise");
        assert_ne!(again.decision_id, req.decision_id, "a fresh decision");
        assert_eq!(again.access, ScopeAccess::Rw);
    }

    /// Re-raising a question that is already on screen would duplicate the
    /// modal without adding information.
    #[test]
    fn reopen_is_a_noop_while_the_question_is_in_flight() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        let _live = c
            .begin(p, ScopeAccess::Ro, GrantReason::PreExecViolation, None)
            .expect("first ask");
        assert!(
            c.reopen(p, ScopeAccess::Ro, GrantReason::PreExecViolation, None)
                .is_none()
        );
    }

    #[test]
    fn pending_tracks_in_flight_decisions() {
        let c = coord();
        let dir = tempdir().unwrap();
        let p = dir.path();
        assert_eq!(c.pending_count(), 0);
        let req = c
            .begin(p, ScopeAccess::Rw, GrantReason::PreExecViolation, None)
            .unwrap();
        assert_eq!(c.pending_count(), 1);
        assert_eq!(c.pending()[0].decision_id, req.decision_id);
        // Any resolution (here: deny) empties the pending set.
        c.resolve(&req.decision_id, GrantDecision::Deny);
        assert_eq!(c.pending_count(), 0);
        assert!(c.pending().is_empty());
    }

    #[test]
    fn canonicalize_is_symlink_and_dotdot_insensitive() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("cache");
        std::fs::create_dir(&real).unwrap();
        let dotted = dir.path().join("cache").join("..").join("cache");
        assert_eq!(
            canonicalize_best_effort(&real),
            canonicalize_best_effort(&dotted),
            "`..`-laden and direct spellings must share one dedup key"
        );
    }

    #[test]
    fn persist_grant_deny_path_never_writes() {
        // There is no "persist a deny" — Deny yields Denied, not Persist — so a
        // denied decision simply never calls persist_grant. Assert the settings
        // file stays absent when only a deny occurred.
        let home = tempdir().unwrap();
        let file = home.path().join(".ahma").join("settings.toml");
        let c = coord();
        let target = home.path().join("ext");
        let req = c
            .begin(
                &target,
                ScopeAccess::Rw,
                GrantReason::PreExecViolation,
                None,
            )
            .unwrap();
        let out = c.resolve(&req.decision_id, GrantDecision::Deny);
        assert!(matches!(out, GrantResolveOutcome::Denied { .. }));
        assert!(
            !file.exists(),
            "a deny must not create or write the settings file"
        );
    }

    #[test]
    fn persist_grant_writes_then_updates_in_place() {
        let home = tempdir().unwrap();
        let file = home.path().join(".ahma").join("settings.toml");
        let target = home.path().join("Library").join("Caches").join("sccache");

        // First grant: rw, recorded as Added.
        let added = persist_grant(
            &file,
            NewGrant {
                path: &target,
                access: ScopeAccess::Rw,
                granted_by: Some("sccache".into()),
                granted_at: Some("2026-06-22".into()),
                note: None,
                surface: "test",
                live_scopes: &[],
                workspace: None,
            },
        )
        .unwrap();
        assert_eq!(added, GrantOutcome::Added);

        let reloaded = AhmaSettings::load_from_result(&file).unwrap();
        let scope = reloaded
            .sandbox
            .find_scope(&target)
            .expect("scope persisted");
        assert_eq!(scope.access, ScopeAccess::Rw);
        assert_eq!(scope.granted_by.as_deref(), Some("sccache"));

        // Second grant on the same path with narrower access: Updated in place,
        // proving a double-approve from two surfaces is safe and idempotent.
        let updated = persist_grant(
            &file,
            NewGrant {
                path: &target,
                access: ScopeAccess::Ro,
                granted_by: None,
                granted_at: Some("2026-06-22".into()),
                note: None,
                surface: "test",
                live_scopes: &[],
                workspace: None,
            },
        )
        .unwrap();
        assert!(matches!(updated, GrantOutcome::Updated(old) if old.access == ScopeAccess::Rw));

        let reloaded = AhmaSettings::load_from_result(&file).unwrap();
        assert_eq!(
            reloaded.sandbox.persistent_scopes.len(),
            1,
            "updated in place, not duplicated"
        );
        assert_eq!(
            reloaded.sandbox.find_scope(&target).unwrap().access,
            ScopeAccess::Ro
        );
    }

    #[test]
    fn persist_grant_refuses_to_clobber_corrupt_settings() {
        let home = tempdir().unwrap();
        let dir = home.path().join(".ahma");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.toml");
        std::fs::write(&file, "this is : not valid toml [[[").unwrap();
        let target = home.path().join("cache");
        let err = persist_grant(
            &file,
            NewGrant {
                path: &target,
                access: ScopeAccess::Rw,
                granted_by: None,
                granted_at: None,
                note: None,
                surface: "test",
                live_scopes: &[],
                workspace: None,
            },
        );
        assert!(
            err.is_err(),
            "a corrupt settings file must not be silently overwritten"
        );
        // The bad contents are left intact for the human to fix.
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("not valid toml")
        );
    }

    fn grant<'a>(path: &'a Path, scopes: &'a [PathBuf]) -> NewGrant<'a> {
        NewGrant {
            path,
            access: ScopeAccess::Rw,
            granted_by: Some("test".into()),
            granted_at: Some("2026-10-02".into()),
            note: None,
            surface: "test",
            live_scopes: scopes,
            workspace: None,
        }
    }

    /// SPEC R5.4.11: a grant is bound to its workspace; a legacy record with
    /// no workspace applies everywhere.
    #[test]
    fn persistent_scope_with_workspace_not_applied_to_other_workspace() {
        let td = tempdir().unwrap();
        let a = td.path().join("projects").join("a");
        let b = td.path().join("projects").join("b");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::create_dir_all(a.join("rust")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let a = dunce::canonicalize(&a).unwrap();
        let b = dunce::canonicalize(&b).unwrap();

        assert!(
            grant_applies(Some(&a), std::slice::from_ref(&a)),
            "same workspace"
        );
        assert!(
            grant_applies(Some(&a), &[a.join("rust")]),
            "a subdirectory session"
        );
        assert!(
            grant_applies(Some(&a.join("rust")), std::slice::from_ref(&a)),
            "granted from a subdirectory"
        );
        assert!(
            !grant_applies(Some(&a), std::slice::from_ref(&b)),
            "another project"
        );
        assert!(!grant_applies(Some(&a), &[]), "no scope yet");
        assert!(
            grant_applies(None, std::slice::from_ref(&b)),
            "legacy global applies everywhere"
        );
    }

    #[test]
    fn persist_grant_records_the_workspace() {
        let home = tempdir().unwrap();
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let file = home.path().join(".ahma").join("settings.toml");
        let target = home.path().join("cache");
        let ws = home.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let mut g = grant(&target, &[]);
        g.workspace = Some(&ws);
        persist_grant(&file, g).unwrap();
        let reloaded = AhmaSettings::load_from_result(&file).unwrap();
        let rec = reloaded.sandbox.find_scope(&target).unwrap();
        assert_eq!(
            rec.workspace.as_deref(),
            Some(dunce::canonicalize(&ws).unwrap().as_path())
        );
        unsafe { std::env::remove_var("AHMA_TEST_HOME") };
    }

    /// SPEC R-PERM.2: the hard denylist gates the one write path, so no surface
    /// — CLI, TUI modal, elicitation answer, MCP tool — can write `~/.ahma`,
    /// `$HOME`, a system directory or a parent of the live scope.
    #[test]
    fn persist_grant_refuses_denylisted_path_from_any_surface() {
        let home = tempdir().unwrap();
        // nextest runs each test in its own process: the override is local.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let file = home.path().join(".ahma").join("settings.toml");
        let ahma_dir = home.path().join(".ahma");
        std::fs::create_dir_all(&ahma_dir).unwrap();

        // A filesystem root rather than `/bin`: on Ubuntu `/bin` canonicalizes
        // to `/usr/bin` (not an exact system dir), and on Windows it is nothing.
        let root = std::path::Path::new(if cfg!(windows) { "C:\\" } else { "/" }).to_path_buf();
        for (what, path) in [
            ("~/.ahma", ahma_dir.clone()),
            ("$HOME", home.path().to_path_buf()),
            ("a filesystem root", root),
        ] {
            let err = persist_grant(&file, grant(&path, &[]))
                .expect_err(&format!("{what} must be refused"));
            assert!(
                err.to_string().contains("refusing to grant"),
                "{what}: {err}"
            );
        }
        // A parent of the live scope widens the sandbox above the workspace.
        let ws = home.path().join("projects").join("app");
        std::fs::create_dir_all(&ws).unwrap();
        let ws = dunce::canonicalize(&ws).unwrap();
        let parent = ws.parent().unwrap().to_path_buf();
        let scopes = vec![ws.clone()];
        assert!(persist_grant(&file, grant(&parent, &scopes)).is_err());
        assert!(
            !file.exists(),
            "a refused grant writes nothing: {}",
            file.display()
        );
        unsafe { std::env::remove_var("AHMA_TEST_HOME") };
    }

    /// SPEC R-PERM.2.1: every persist appends one audit record naming the
    /// surface, from inside the chokepoint — the TUI and tool paths used to
    /// skip it.
    #[test]
    fn persist_grant_appends_an_audit_record_with_the_surface() {
        let home = tempdir().unwrap();
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let file = home.path().join(".ahma").join("settings.toml");
        let target = home.path().join("cache");
        std::fs::create_dir_all(&target).unwrap();
        let mut g = grant(&target, &[]);
        g.surface = "tui";
        persist_grant(&file, g).unwrap();
        let audit =
            std::fs::read_to_string(home.path().join(".ahma").join("permissions-audit.jsonl"))
                .expect("audit log written");
        let entry: crate::permissions::AuditEntry =
            serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        assert_eq!(entry.surface.as_deref(), Some("tui"));
        assert_eq!(entry.access.as_deref(), Some("rw"));
        assert!(entry.subject.ends_with("cache"), "{}", entry.subject);
        unsafe { std::env::remove_var("AHMA_TEST_HOME") };
    }
}

#[cfg(test)]
mod refused_path_gate_tests {
    use super::*;

    /// SPEC R-PERM.4.3: a path the hard denylist refuses is never turned into a
    /// question. Asking "grant ~/.ssh?" trains the reflex click the denylist
    /// exists to make unnecessary, and a `session` answer would apply it.
    #[test]
    fn coordinator_never_raises_request_for_refused_path() {
        let c = GrantCoordinator::new();
        let home = crate::config::ahma_home_dir().expect("home dir");
        for refused in [home.join(".ssh"), home.join(".aws"), home.clone()] {
            assert!(
                c.begin(
                    &refused,
                    ScopeAccess::Rw,
                    GrantReason::StderrHeuristic,
                    Some("git".into())
                )
                .is_none(),
                "{} must never become a prompt",
                refused.display()
            );
        }
        assert!(
            c.begin(
                Path::new("/"),
                ScopeAccess::Ro,
                GrantReason::PreExecViolation,
                None
            )
            .is_none()
        );
        // The refusal is reportable to the agent as text, not a silent `None`.
        let why = refusal_reason(&home.join(".ssh")).expect("a reason is given");
        assert!(why.contains("credentials"), "{why}");
        let system_dir = if cfg!(windows) { "C:\\Windows" } else { "/etc" };
        assert!(refusal_reason(Path::new(system_dir)).is_some());
    }
}

/// Why `path` can never be granted, if the hard denylist refuses it
/// (SPEC R-PERM.4.3): a filesystem root, `$HOME`, a credential directory,
/// ahma's own settings directory, or an OS system directory. `None` for a path
/// a human may be asked about. Evaluated without live scopes, so the
/// "parent of the workspace" rule is left to the apply site, which has them.
pub fn refusal_reason(path: &Path) -> Option<String> {
    let canonical = canonicalize_best_effort(path);
    match classify_grant_risk(&canonical, crate::config::ahma_home_dir().as_deref(), &[]) {
        GrantRisk::Refused(why) => Some(why),
        _ => None,
    }
}
