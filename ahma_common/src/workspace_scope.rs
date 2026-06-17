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

use std::path::PathBuf;
use std::sync::Mutex;

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
    /// The workspace scope is terminated; no further commits are possible.
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Awaiting,
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
    pub fn commit(&self, proposed: Vec<PathBuf>, source: ScopeSourceToken) -> CommitOutcome {
        let mut inner = self.inner.lock().unwrap();
        match &inner.state {
            State::Terminated => CommitOutcome::Terminated,
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

    /// Force-commit an explicitly user-consented scope, bypassing the downgrade
    /// gate (the user has already approved this widening via elicitation, R5.3).
    /// Still refused once terminated.
    pub fn commit_consented(
        &self,
        scopes: Vec<PathBuf>,
        source: ScopeSourceToken,
    ) -> CommitOutcome {
        let mut inner = self.inner.lock().unwrap();
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
        match &self.inner.lock().unwrap().state {
            State::Active { scopes, .. } => Some(scopes.clone()),
            _ => None,
        }
    }

    /// The provenance token of the committed scope, if active.
    pub fn source(&self) -> Option<ScopeSourceToken> {
        match &self.inner.lock().unwrap().state {
            State::Active { source, .. } => Some(source),
            _ => None,
        }
    }

    /// True once a scope has been established (and not terminated).
    pub fn is_active(&self) -> bool {
        matches!(self.inner.lock().unwrap().state, State::Active { .. })
    }

    /// The current generation. Bumped on [`Self::terminate`]; used to reject
    /// stale elicitation answers (R5.3.5).
    pub fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    /// Terminate the workspace scope and bump the generation. Subsequent commits
    /// return [`CommitOutcome::Terminated`].
    pub fn terminate(&self) {
        let mut inner = self.inner.lock().unwrap();
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
