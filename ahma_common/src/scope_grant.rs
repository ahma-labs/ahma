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
//! *The sandbox scope cannot be changed during a session* (SPEC R5). Approval here
//! **never widens the live session** — [`persist_grant`] only writes the settings
//! file, which takes effect on the next server start (exactly like the CLI). The
//! grant path must never touch the live `Sandbox` or the scope state machine. That
//! is why this is a *persist*, not a *re-lock*.
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
//!    once** per session. The same path trips the kernel many times; [`begin`]
//!    gates so only the first trip fans out a prompt.
//!    [`begin`]: GrantCoordinator::begin
//!  - **No re-ask loops**: after a Deny *or* a grant the `(path, access)` is added
//!    to a session dismiss list. A grant cannot widen the live session, so the path
//!    keeps tripping — suppressing the re-ask is what stops an ask→deny→ask storm.
//!  - **First-answer-wins**: when the same decision is fanned to several surfaces,
//!    the first answer resolves it; later answers are no-ops ([`resolve`] is
//!    idempotent). Combined with [`crate::config::GrantOutcome::Updated`] this makes
//!    a near-simultaneous double-approve from two surfaces harmless.
//!    [`resolve`]: GrantCoordinator::resolve

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
/// `Serialize`/`Deserialize` so it can travel over the daemon hub (TUI) and inside
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
    /// Grant read-only access.
    GrantRo,
    /// Grant read+write access.
    GrantRw,
}

impl GrantDecision {
    /// The [`ScopeAccess`] to persist, or `None` for [`GrantDecision::Deny`].
    pub fn access(self) -> Option<ScopeAccess> {
        match self {
            GrantDecision::Deny => None,
            GrantDecision::GrantRo => Some(ScopeAccess::Ro),
            GrantDecision::GrantRw => Some(ScopeAccess::Rw),
        }
    }
}

/// What the caller should do after [`GrantCoordinator::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantResolveOutcome {
    /// Write this grant to settings via [`persist_grant`]. The access is the one the
    /// human chose, which may be narrower than the requested access.
    Persist {
        /// Canonical directory to grant.
        path: PathBuf,
        /// Access level the human approved.
        access: ScopeAccess,
        /// Tool that requested it, for `granted_by` provenance.
        tool: Option<String>,
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
        let canonical = canonicalize_best_effort(path);
        let key = (canonical.clone(), access);
        let mut inner = self.inner.lock().unwrap();
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

    /// Resolve a decision with the human's answer. First-answer-wins and idempotent:
    /// a second call for the same `decision_id` returns [`GrantResolveOutcome::AlreadyResolved`].
    ///
    /// On any answer the `(path, access)` is dismissed for the session so the path —
    /// which the live sandbox still blocks until restart — does not re-prompt. A
    /// grant additionally dismisses the *other* access variant for the same path
    /// (granting rw subsumes a pending ro need, and vice-versa).
    pub fn resolve(&self, decision_id: &str, decision: GrantDecision) -> GrantResolveOutcome {
        let mut inner = self.inner.lock().unwrap();
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
                }
            }
        }
    }

    /// Cancel a decision without an answer (e.g. the asking session terminated).
    /// Marks it resolved so a late answer no-ops, and frees its dedup key (the path
    /// is *not* dismissed — a future trip may legitimately re-ask). Returns the
    /// request if it was in flight.
    pub fn cancel(&self, decision_id: &str) -> Option<ScopeGrantRequest> {
        let mut inner = self.inner.lock().unwrap();
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
        self.inner
            .lock()
            .unwrap()
            .in_flight
            .values()
            .cloned()
            .collect()
    }

    /// Number of decisions currently awaiting an answer.
    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().in_flight.len()
    }

    /// Whether `decision_id` is still awaiting an answer.
    pub fn is_in_flight(&self, decision_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .in_flight
            .contains_key(decision_id)
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

/// Persist an approved grant to the settings file — the single chokepoint that
/// satisfies the session-immutability invariant: it writes `~/.ahma/settings.toml`
/// and **never** mutates the live sandbox. Mirrors the CLI `ahma sandbox grant`
/// path so both converge on one persistence code path.
///
/// Uses the strict loader so a corrupt settings file is *not* silently clobbered.
/// `granted_at` is supplied by the caller (stamp with `chrono::Local::now()`),
/// keeping this crate free of a date dependency. Returns the
/// [`GrantOutcome`] so the caller can report "added" vs "updated".
pub fn persist_grant(
    settings_file: &Path,
    path: &Path,
    access: ScopeAccess,
    granted_by: Option<String>,
    granted_at: Option<String>,
    note: Option<String>,
) -> Result<GrantOutcome> {
    let mut settings = AhmaSettings::load_from_result(settings_file)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| {
            format!(
                "refusing to overwrite unparseable {}",
                settings_file.display()
            )
        })?;
    let outcome = settings.sandbox.grant_scope(PersistentScope {
        path: path.to_path_buf(),
        access,
        granted_by,
        granted_at,
        note,
    });
    settings
        .save_to(settings_file)
        .with_context(|| format!("failed to write {}", settings_file.display()))?;
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
            GrantResolveOutcome::Persist { path, access, tool } => {
                assert_eq!(access, ScopeAccess::Rw);
                assert_eq!(tool.as_deref(), Some("sccache"));
                assert_eq!(path, canonicalize_best_effort(p));
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
            &target,
            ScopeAccess::Rw,
            Some("sccache".into()),
            Some("2026-06-22".into()),
            None,
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
            &target,
            ScopeAccess::Ro,
            None,
            Some("2026-06-22".into()),
            None,
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
        let err = persist_grant(&file, home.path(), ScopeAccess::Rw, None, None, None);
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
}
