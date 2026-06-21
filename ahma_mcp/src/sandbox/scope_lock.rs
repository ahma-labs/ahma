//! The in-process sandbox scope lock — a one-shot commit latch.
//!
//! This is a **security primitive** (SPEC R5). The scope a sandbox enforces is
//! provisional until it is *committed*; once committed it is immutable and must
//! never be re-derived or widened by a later `roots/list` / `roots/list_changed`
//! (SPEC R5.1 / R5.1.1 / R5.2.2).
//!
//! [`ScopeLock`] encapsulates that lifecycle as an explicit state machine per
//! SPEC R23, replacing the two free-standing `AtomicBool`s that used to live on
//! [`crate::sandbox::core::Sandbox`]. Unlike most machines in the workspace it
//! is **not** built on `Observable`: `is_committed()` is read on the hot
//! command-gating path, so the state is held in atomics for lock-free reads
//! rather than behind a mutex. The lifecycle, the one-shot commit semantics, and
//! the memory ordering that the commit decision must not be reordered past the
//! scopes write all live here, in one audited place, instead of being scattered
//! across call sites.

use ahma_common::state_machine::FsmState;
use std::sync::atomic::{AtomicBool, Ordering};

/// Observable lifecycle of the sandbox scope lock.
///
/// Derived from the latches inside [`ScopeLock`]; `Committed` takes precedence
/// over the roots-received flag, so once locked the state stays `Committed`
/// regardless of any later negotiation bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeLockState {
    /// `roots/list` not yet received; the scope is provisional.
    AwaitingRoots,
    /// `roots/list` received; the scope may still be re-derived until committed.
    RootsReceived,
    /// Scope committed and locked — immutable (SPEC R5.1 / R5.1.1 / R5.2.2).
    /// Terminal: no transition leaves this state.
    Committed,
}

impl FsmState for ScopeLockState {
    fn name(&self) -> &'static str {
        match self {
            ScopeLockState::AwaitingRoots => "AwaitingRoots",
            ScopeLockState::RootsReceived => "RootsReceived",
            ScopeLockState::Committed => "Committed",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, ScopeLockState::Committed)
    }
}

/// The sandbox scope lock: a lock-free, one-shot commit latch plus a
/// roots-received negotiation flag.
pub(crate) struct ScopeLock {
    /// True once `roots/list` has been received from the client. May toggle
    /// during negotiation; it has no effect on the commit latch.
    roots_received: AtomicBool,
    /// One-shot latch: set the first time the scope is committed (locked). Once
    /// set, the scope is immutable.
    committed: AtomicBool,
}

impl ScopeLock {
    /// Create a lock in the pre-commit phase. `roots_received` seeds whether the
    /// client roots are already considered in hand (e.g. explicit scopes start
    /// as if roots were received).
    pub(crate) fn new(roots_received: bool) -> Self {
        Self {
            roots_received: AtomicBool::new(roots_received),
            committed: AtomicBool::new(false),
        }
    }

    /// The current observable state, derived from the latches.
    pub(crate) fn state(&self) -> ScopeLockState {
        if self.committed.load(Ordering::Acquire) {
            ScopeLockState::Committed
        } else if self.roots_received.load(Ordering::Relaxed) {
            ScopeLockState::RootsReceived
        } else {
            ScopeLockState::AwaitingRoots
        }
    }

    /// Returns true once the scope has been committed (locked). After this the
    /// scope is immutable and must not be re-derived from a later `roots/list`
    /// (SPEC R5.1 / R5.2.2).
    pub(crate) fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Acquire)
    }

    /// Record whether client workspace roots have been received. This is
    /// negotiation bookkeeping only; it never changes the commit latch, and once
    /// committed the observable [`state`](Self::state) stays `Committed`.
    pub(crate) fn set_roots_received(&self, received: bool) {
        self.roots_received.store(received, Ordering::Relaxed);
    }

    /// Whether client workspace roots have been received.
    pub(crate) fn roots_received(&self) -> bool {
        self.roots_received.load(Ordering::Relaxed)
    }

    /// Atomically claim the one-shot scope commit (`* -> Committed`).
    ///
    /// Returns `true` for the single caller that wins the latch (and may proceed
    /// to apply/enforce scopes) and `false` for every subsequent call, which
    /// must treat the configuration as a tolerated no-op rather than widening the
    /// locked sandbox (SPEC R5.1.1). `AcqRel` ordering ensures the commit
    /// decision is not reordered past the scopes write the winner performs next.
    #[must_use]
    pub(crate) fn try_commit(&self) -> bool {
        self.committed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Clone for ScopeLock {
    fn clone(&self) -> Self {
        Self {
            roots_received: AtomicBool::new(self.roots_received.load(Ordering::Relaxed)),
            committed: AtomicBool::new(self.committed.load(Ordering::Relaxed)),
        }
    }
}

impl std::fmt::Debug for ScopeLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeLock")
            .field("state", &self.state())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_derivation_tracks_latches() {
        let lock = ScopeLock::new(false);
        assert_eq!(lock.state(), ScopeLockState::AwaitingRoots);

        lock.set_roots_received(true);
        assert_eq!(lock.state(), ScopeLockState::RootsReceived);

        assert!(lock.try_commit());
        assert_eq!(lock.state(), ScopeLockState::Committed);
        assert!(lock.state().is_terminal());
    }

    #[test]
    fn commit_is_one_shot() {
        let lock = ScopeLock::new(true);
        assert!(!lock.is_committed());
        assert!(lock.try_commit(), "first commit must win the latch");
        assert!(lock.is_committed());
        assert!(!lock.try_commit(), "second commit must lose (one-shot)");
        assert!(
            !lock.try_commit(),
            "every subsequent commit must keep losing"
        );
    }

    #[test]
    fn committed_state_is_sticky_against_roots_toggle() {
        let lock = ScopeLock::new(true);
        assert!(lock.try_commit());
        // Negotiation bookkeeping after commit must not unlock the scope.
        lock.set_roots_received(false);
        assert_eq!(lock.state(), ScopeLockState::Committed);
        assert!(lock.is_committed());
    }

    #[test]
    fn clone_preserves_latch_values() {
        let lock = ScopeLock::new(true);
        assert!(lock.try_commit());
        let cloned = lock.clone();
        assert!(
            cloned.is_committed(),
            "clone must carry the committed latch"
        );
        assert!(
            !cloned.try_commit(),
            "cloned lock is already committed, so try_commit must lose"
        );
    }
}
