//! Per-workspace scope ownership and the single commit point (SPEC R5.1 / R5.1.1).
//!
//! A [`WorkspaceScope`] is owned by a per-workspace server instance and shared
//! (via `Arc`) by every session — IDE, TUI, CLI — attached to that workspace.
//! Every scope commit, regardless of source (explicit flag, `roots/list`, an
//! elicitation answer, or the default), goes through [`WorkspaceScope::commit`].
//! There is exactly one door to "scope locked":
//!
//!  - the first commit **establishes** the scope;
//!  - a later commit that grants no new access is a no-op or a **narrowing**
//!    (applied, since narrowing is always safe — R5.3);
//!  - a later commit that **widens** is refused and reported as requiring user
//!    consent (R5.3) — it is never applied silently;
//!  - once terminated, nothing commits.
//!
//! The `generation` counter binds in-flight decisions to the session lifetime so
//! a stale answer can be rejected (R5.3.5).
//!
//! ## Status: not wired to a live server
//!
//! Nothing outside this file's own tests calls into this module. The commit path
//! a running ahma actually uses is `ahma_mcp::sandbox::Sandbox::commit_scopes`
//! over the two-atomic `ScopeLock`, which has no pending state, no generation
//! counter, and no sharing across sessions — this type is its intended
//! replacement, not a component of it.
//!
//! That is stated here because "complete and unit-tested" reads as "works", and
//! the distance between the two is the whole subject of SPEC R5.3.6's status
//! note. Replacing `ScopeLock` means changing the one mechanism R5.1.1 requires
//! to have exactly one door to "scope locked", so it is not something to wire in
//! halfway: a partial integration is a second door.

use parking_lot::Mutex;
use std::path::PathBuf;

use crate::scope_decision::{ScopeDelta, classify_scope_change};

/// Provenance of a committed scope (mirrors `ScopeSource` in the MCP crate; kept
/// as a plain string token here to avoid a cross-crate dependency).
pub type ScopeSourceToken = &'static str;

/// Outcome of a call to [`WorkspaceScope::commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// First commit — scope is now established with these write roots.
    Established(Vec<PathBuf>),
    /// Proposed grants the same access as the established scope; nothing changed.
    AlreadyActive(Vec<PathBuf>),
    /// Proposed was narrower and has been applied (narrowing is always safe).
    Narrowed(Vec<PathBuf>),
    /// Proposed would widen the established scope. NOT applied — the caller must
    /// obtain explicit user consent (R5.3) before any widening can take effect.
    RequiresConsent {
        established: Vec<PathBuf>,
        proposed: Vec<PathBuf>,
    },
    /// A TUI answer given while no IDE session was live (R5.3.6). The scope is
    /// parked — shown as pending, not enforced as active — and is applied when
    /// the next IDE session attaches to the workspace instance.
    Pending(Vec<PathBuf>),
    /// The workspace scope is terminated; no further commits are possible.
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Awaiting,
    /// A TUI-only answer awaiting the next IDE session (R5.3.6). Not active:
    /// nothing is enforced against it and `scopes()`/`source()` report `None`.
    Pending {
        scopes: Vec<PathBuf>,
        source: ScopeSourceToken,
    },
    Active {
        scopes: Vec<PathBuf>,
        source: ScopeSourceToken,
    },
    Terminated,
}

struct Inner {
    state: State,
    generation: u64,
}

/// A shareable, lock-once workspace scope cell.
pub struct WorkspaceScope {
    inner: Mutex<Inner>,
}

impl Default for WorkspaceScope {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceScope {
    /// A fresh workspace scope awaiting its first commit.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: State::Awaiting,
                generation: 0,
            }),
        }
    }

    /// The single commit point (R5.1.1). See [`CommitOutcome`].
    ///
    /// A commit made while a TUI-only answer is parked ([`CommitOutcome::Pending`],
    /// R5.3.6) is the moment "the next IDE session attaches": the pending scope
    /// is applied (promoted to active) first, and the proposed scope is then
    /// evaluated against it through the normal downgrade gate — same is a
    /// no-op, narrower applies, wider requires consent.
    pub fn commit(&self, proposed: Vec<PathBuf>, source: ScopeSourceToken) -> CommitOutcome {
        let mut inner = self.inner.lock();
        if let State::Pending {
            scopes,
            source: pending_source,
        } = &inner.state
        {
            inner.state = State::Active {
                scopes: scopes.clone(),
                source: pending_source,
            };
        }
        match &inner.state {
            State::Terminated => CommitOutcome::Terminated,
            State::Pending { .. } => unreachable!("promoted above"),
            State::Awaiting => {
                inner.state = State::Active {
                    scopes: proposed.clone(),
                    source,
                };
                CommitOutcome::Established(proposed)
            }
            State::Active { scopes, .. } => match classify_scope_change(scopes, &proposed) {
                ScopeDelta::Same => CommitOutcome::AlreadyActive(scopes.clone()),
                ScopeDelta::Narrows => {
                    let applied = proposed.clone();
                    inner.state = State::Active {
                        scopes: proposed,
                        source,
                    };
                    CommitOutcome::Narrowed(applied)
                }
                ScopeDelta::Widens => CommitOutcome::RequiresConsent {
                    established: scopes.clone(),
                    proposed,
                },
            },
        }
    }

    /// Park a scope answered in the TUI while **no IDE session is live**
    /// (R5.3.6). The scope is recorded and shown as *pending* — it is **not**
    /// locked as active, because no live session is using it. It is applied
    /// when the next IDE session attaches ([`Self::promote_pending`], or
    /// implicitly by that session's first [`Self::commit`]).
    ///
    /// While already pending, a same-or-narrower answer replaces the parked
    /// scope; a widening answer requires consent, exactly like the active
    /// gate (Enter alone must never widen — R5.3.1). If the scope is already
    /// active a live session is using it, so the answer flows through the
    /// normal [`Self::commit`] gate instead.
    pub fn commit_pending(
        &self,
        proposed: Vec<PathBuf>,
        source: ScopeSourceToken,
    ) -> CommitOutcome {
        {
            let mut inner = self.inner.lock();
            match &inner.state {
                State::Terminated => return CommitOutcome::Terminated,
                State::Awaiting => {
                    inner.state = State::Pending {
                        scopes: proposed.clone(),
                        source,
                    };
                    return CommitOutcome::Pending(proposed);
                }
                State::Pending { scopes, .. } => match classify_scope_change(scopes, &proposed) {
                    ScopeDelta::Same => return CommitOutcome::Pending(scopes.clone()),
                    ScopeDelta::Narrows => {
                        inner.state = State::Pending {
                            scopes: proposed.clone(),
                            source,
                        };
                        return CommitOutcome::Pending(proposed);
                    }
                    ScopeDelta::Widens => {
                        return CommitOutcome::RequiresConsent {
                            established: scopes.clone(),
                            proposed,
                        };
                    }
                },
                State::Active { .. } => {} // fall through to the live gate
            }
        }
        self.commit(proposed, source)
    }

    /// Apply a parked TUI-only scope now that an IDE session has attached
    /// (R5.3.6). Returns the established scope, or `None` when nothing was
    /// pending (already active, still awaiting, or terminated).
    pub fn promote_pending(&self) -> Option<Vec<PathBuf>> {
        let mut inner = self.inner.lock();
        if let State::Pending { scopes, source } = &inner.state {
            let scopes = scopes.clone();
            inner.state = State::Active {
                scopes: scopes.clone(),
                source,
            };
            Some(scopes)
        } else {
            None
        }
    }

    /// True while a TUI-only answer is parked awaiting the next IDE session.
    pub fn is_pending(&self) -> bool {
        matches!(self.inner.lock().state, State::Pending { .. })
    }

    /// The parked pending write roots, if any (shown as *pending* — R5.3.6
    /// requires this state to be visible, never silently treated as active).
    pub fn pending_scopes(&self) -> Option<Vec<PathBuf>> {
        match &self.inner.lock().state {
            State::Pending { scopes, .. } => Some(scopes.clone()),
            _ => None,
        }
    }

    /// Force-commit an explicitly user-consented scope, bypassing the downgrade
    /// gate (the user has already approved this widening via elicitation, R5.3).
    /// Still refused once terminated.
    pub fn commit_consented(
        &self,
        scopes: Vec<PathBuf>,
        source: ScopeSourceToken,
    ) -> CommitOutcome {
        let mut inner = self.inner.lock();
        if matches!(inner.state, State::Terminated) {
            return CommitOutcome::Terminated;
        }
        inner.state = State::Active {
            scopes: scopes.clone(),
            source,
        };
        CommitOutcome::Established(scopes)
    }

    /// The currently committed write roots, if active.
    pub fn scopes(&self) -> Option<Vec<PathBuf>> {
        match &self.inner.lock().state {
            State::Active { scopes, .. } => Some(scopes.clone()),
            _ => None,
        }
    }

    /// The provenance token of the committed scope, if active.
    pub fn source(&self) -> Option<ScopeSourceToken> {
        match &self.inner.lock().state {
            State::Active { source, .. } => Some(source),
            _ => None,
        }
    }

    /// True once a scope has been established (and not terminated).
    pub fn is_active(&self) -> bool {
        matches!(self.inner.lock().state, State::Active { .. })
    }

    /// The current generation. Bumped on [`Self::terminate`]; used to reject
    /// stale elicitation answers (R5.3.5).
    pub fn generation(&self) -> u64 {
        self.inner.lock().generation
    }

    /// Terminate the workspace scope and bump the generation. Subsequent commits
    /// return [`CommitOutcome::Terminated`].
    pub fn terminate(&self) {
        let mut inner = self.inner.lock();
        inner.state = State::Terminated;
        inner.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn first_commit_establishes() {
        let ws = WorkspaceScope::new();
        let out = ws.commit(paths(&["/ws/proj"]), "roots/list");
        assert_eq!(out, CommitOutcome::Established(paths(&["/ws/proj"])));
        assert!(ws.is_active());
        assert_eq!(ws.scopes(), Some(paths(&["/ws/proj"])));
        assert_eq!(ws.source(), Some("roots/list"));
    }

    #[test]
    fn same_scope_is_noop() {
        let ws = WorkspaceScope::new();
        ws.commit(paths(&["/ws/proj"]), "explicit");
        let out = ws.commit(paths(&["/ws/proj"]), "roots/list");
        assert_eq!(out, CommitOutcome::AlreadyActive(paths(&["/ws/proj"])));
        // source unchanged — the second (same) commit did not replace it
        assert_eq!(ws.source(), Some("explicit"));
    }

    #[test]
    fn narrowing_is_applied() {
        let ws = WorkspaceScope::new();
        ws.commit(paths(&["/ws/proj"]), "roots/list");
        let out = ws.commit(paths(&["/ws/proj/src"]), "elicited");
        assert_eq!(out, CommitOutcome::Narrowed(paths(&["/ws/proj/src"])));
        assert_eq!(ws.scopes(), Some(paths(&["/ws/proj/src"])));
    }

    #[test]
    fn widening_requires_consent_and_is_not_applied() {
        let ws = WorkspaceScope::new();
        ws.commit(paths(&["/ws/proj"]), "explicit");
        let out = ws.commit(paths(&["/ws"]), "roots/list");
        assert_eq!(
            out,
            CommitOutcome::RequiresConsent {
                established: paths(&["/ws/proj"]),
                proposed: paths(&["/ws"]),
            }
        );
        // CRITICAL: the widening was NOT applied (R5.3 — no silent downgrade).
        assert_eq!(ws.scopes(), Some(paths(&["/ws/proj"])));
    }

    #[test]
    fn consented_widening_is_applied() {
        let ws = WorkspaceScope::new();
        ws.commit(paths(&["/ws/proj"]), "explicit");
        let out = ws.commit_consented(paths(&["/ws"]), "elicited");
        assert_eq!(out, CommitOutcome::Established(paths(&["/ws"])));
        assert_eq!(ws.scopes(), Some(paths(&["/ws"])));
    }

    #[test]
    fn terminate_blocks_further_commits_and_bumps_generation() {
        let ws = WorkspaceScope::new();
        ws.commit(paths(&["/ws/proj"]), "roots/list");
        let g0 = ws.generation();
        ws.terminate();
        assert_eq!(ws.generation(), g0 + 1);
        assert_eq!(
            ws.commit(paths(&["/ws/proj"]), "roots/list"),
            CommitOutcome::Terminated
        );
        assert!(!ws.is_active());
    }

    // ── R5.3.6: TUI-only establishment is pending ────────────────────────────

    #[test]
    fn tui_only_answer_parks_as_pending_not_active() {
        let ws = WorkspaceScope::new();
        let out = ws.commit_pending(paths(&["/ws/proj"]), "elicited");
        assert_eq!(out, CommitOutcome::Pending(paths(&["/ws/proj"])));
        // Shown as pending — never silently locked as if a live session used it.
        assert!(ws.is_pending());
        assert!(!ws.is_active());
        assert_eq!(ws.scopes(), None, "a pending scope is not enforced");
        assert_eq!(ws.pending_scopes(), Some(paths(&["/ws/proj"])));
    }

    #[test]
    fn pending_is_applied_when_the_next_ide_session_attaches() {
        let ws = WorkspaceScope::new();
        ws.commit_pending(paths(&["/ws/proj"]), "elicited");
        let promoted = ws.promote_pending();
        assert_eq!(promoted, Some(paths(&["/ws/proj"])));
        assert!(ws.is_active());
        assert!(!ws.is_pending());
        assert_eq!(
            ws.source(),
            Some("elicited"),
            "provenance survives promotion"
        );
    }

    #[test]
    fn attaching_session_commit_promotes_pending_then_gates_its_roots() {
        // Same roots as the pending answer → promoted, then a no-op.
        let ws = WorkspaceScope::new();
        ws.commit_pending(paths(&["/ws/proj"]), "elicited");
        let out = ws.commit(paths(&["/ws/proj"]), "roots/list");
        assert_eq!(out, CommitOutcome::AlreadyActive(paths(&["/ws/proj"])));
        assert!(ws.is_active());

        // Wider roots than the pending answer → pending is applied, widening
        // still requires consent (the TUI answer can never be silently widened).
        let ws = WorkspaceScope::new();
        ws.commit_pending(paths(&["/ws/proj"]), "elicited");
        let out = ws.commit(paths(&["/ws"]), "roots/list");
        assert_eq!(
            out,
            CommitOutcome::RequiresConsent {
                established: paths(&["/ws/proj"]),
                proposed: paths(&["/ws"]),
            }
        );
        assert_eq!(ws.scopes(), Some(paths(&["/ws/proj"])));
    }

    #[test]
    fn pending_replacement_narrows_but_never_widens() {
        let ws = WorkspaceScope::new();
        ws.commit_pending(paths(&["/ws/proj"]), "elicited");
        // Narrower re-answer replaces the parked scope.
        let out = ws.commit_pending(paths(&["/ws/proj/src"]), "elicited");
        assert_eq!(out, CommitOutcome::Pending(paths(&["/ws/proj/src"])));
        // Wider re-answer is refused, parked scope unchanged.
        let out = ws.commit_pending(paths(&["/ws"]), "elicited");
        assert!(matches!(out, CommitOutcome::RequiresConsent { .. }));
        assert_eq!(ws.pending_scopes(), Some(paths(&["/ws/proj/src"])));
    }

    #[test]
    fn pending_answer_on_an_active_scope_routes_through_the_live_gate() {
        let ws = WorkspaceScope::new();
        ws.commit(paths(&["/ws/proj"]), "roots/list");
        // A live session is using this scope — the answer is not parked.
        let out = ws.commit_pending(paths(&["/ws"]), "elicited");
        assert!(matches!(out, CommitOutcome::RequiresConsent { .. }));
        assert!(!ws.is_pending());
    }

    #[test]
    fn terminate_discards_pending_and_blocks_promotion() {
        let ws = WorkspaceScope::new();
        ws.commit_pending(paths(&["/ws/proj"]), "elicited");
        ws.terminate();
        assert_eq!(ws.promote_pending(), None);
        assert_eq!(
            ws.commit_pending(paths(&["/ws/proj"]), "elicited"),
            CommitOutcome::Terminated
        );
    }

    #[test]
    fn shared_across_sessions_via_arc() {
        use std::sync::Arc;
        let ws = Arc::new(WorkspaceScope::new());
        let session_a = Arc::clone(&ws);
        let session_b = Arc::clone(&ws);
        // session A establishes; session B sees the same shared scope.
        session_a.commit(paths(&["/ws/proj"]), "roots/list");
        assert_eq!(session_b.scopes(), Some(paths(&["/ws/proj"])));
        // session B's widening attempt is gated by the shared single commit point.
        let out = session_b.commit(paths(&["/ws"]), "roots/list");
        assert!(matches!(out, CommitOutcome::RequiresConsent { .. }));
    }
}
