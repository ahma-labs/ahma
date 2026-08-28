//! Scope-downgrade elicitation coordinator (SPEC R5.3.1–R5.3.5).
//!
//! When a downgrade needs user consent, the server fans one decision (a single
//! `decision_id`) out to every attached session whose client can be asked. This
//! type owns the *pure* coordination logic — fan-out bookkeeping, freshness
//! checks, most-restrictive resolution, and the dismiss list — while the bridge
//! owns the wire I/O and the debounce timer.
//!
//! Guarantees encoded here:
//!  - **Solicited only**: an answer from a peer that was not asked is rejected.
//!  - **Freshness (R5.3.5)**: an answer carrying a stale generation is rejected,
//!    never applied to a recycled session.
//!  - **Most-restrictive-wins (R5.3.4)**: resolution folds all collected answers
//!    to the narrowest; incomparable answers fall to their intersection and set
//!    a `conflicted` flag so the bridge can re-confirm.
//!  - **Dismiss (R5.3.3)**: once resolved, every asked-but-unanswered peer is
//!    listed for `notifications/cancelled`.
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

use std::path::PathBuf;

use crate::scope_decision::most_restrictive;

/// Why an answer was not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The decision is already resolved; this late answer is ignored.
    AlreadyResolved,
    /// The answer's generation does not match the decision's (R5.3.5).
    StaleGeneration,
    /// The answering peer was never asked this decision.
    Unsolicited,
}

/// Result of recording an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// Recorded; `first` is true if it was the first answer (start the debounce).
    Recorded { first: bool },
    /// Not recorded.
    Rejected(RejectReason),
}

/// The committed result of an elicitation decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The narrowest scope across all answers (R5.3.4).
    pub scopes: Vec<PathBuf>,
    /// True when answers were incomparable and the intersection was taken; the
    /// bridge should re-confirm the committed scope (R5.3.4).
    pub conflicted: bool,
    /// Peers that were asked but did not answer — dismiss their modal (R5.3.3).
    pub dismiss: Vec<String>,
}

/// One in-flight downgrade decision.
#[derive(Debug, Clone)]
pub struct ElicitationDecision {
    id: String,
    generation: u64,
    asked: Vec<String>,
    answers: Vec<(String, Vec<PathBuf>)>,
    resolved: Option<Resolution>,
}

impl ElicitationDecision {
    /// Begin a decision identified by `id`, bound to `generation`, asked of the
    /// given peers.
    pub fn new(id: impl Into<String>, generation: u64, asked: Vec<String>) -> Self {
        Self {
            id: id.into(),
            generation,
            asked,
            answers: Vec::new(),
            resolved: None,
        }
    }

    /// The decision id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// True once resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved.is_some()
    }

    /// Record a peer's answer (the scope they chose), validating solicitation and
    /// freshness. Recording does not itself resolve — the bridge calls
    /// [`Self::resolve`] after its debounce window or when all peers have
    /// answered.
    pub fn record_answer(
        &mut self,
        peer: &str,
        generation: u64,
        scopes: Vec<PathBuf>,
    ) -> AnswerOutcome {
        if self.resolved.is_some() {
            return AnswerOutcome::Rejected(RejectReason::AlreadyResolved);
        }
        if generation != self.generation {
            return AnswerOutcome::Rejected(RejectReason::StaleGeneration);
        }
        if !self.asked.iter().any(|p| p == peer) {
            return AnswerOutcome::Rejected(RejectReason::Unsolicited);
        }
        let first = self.answers.is_empty();
        // De-dup: a peer answering twice updates its answer.
        if let Some(slot) = self.answers.iter_mut().find(|(p, _)| p == peer) {
            slot.1 = scopes;
        } else {
            self.answers.push((peer.to_string(), scopes));
        }
        AnswerOutcome::Recorded { first }
    }

    /// True when every asked peer has answered (the bridge may resolve early).
    pub fn all_answered(&self) -> bool {
        !self.asked.is_empty()
            && self
                .asked
                .iter()
                .all(|p| self.answers.iter().any(|(q, _)| q == p))
    }

    /// Resolve the decision: fold all answers to the narrowest scope, flag
    /// conflicts, and list peers to dismiss. Idempotent — repeated calls return
    /// the same resolution. Returns `None` if no answer has been recorded yet.
    pub fn resolve(&mut self) -> Option<Resolution> {
        if let Some(r) = &self.resolved {
            return Some(r.clone());
        }
        if self.answers.is_empty() {
            return None;
        }
        let mut acc = self.answers[0].1.clone();
        let mut conflicted = false;
        for (_, scopes) in &self.answers[1..] {
            // Detect incomparable answers (intersection taken) to flag re-confirm.
            use crate::scope_decision::compare_restrictiveness;
            if compare_restrictiveness(&acc, scopes).is_none() {
                conflicted = true;
            }
            acc = most_restrictive(&acc, scopes);
        }
        let answered: Vec<&String> = self.answers.iter().map(|(p, _)| p).collect();
        let dismiss: Vec<String> = self
            .asked
            .iter()
            .filter(|p| !answered.contains(p))
            .cloned()
            .collect();
        let resolution = Resolution {
            scopes: acc,
            conflicted,
            dismiss,
        };
        self.resolved = Some(resolution.clone());
        Some(resolution)
    }

    /// Cancel the decision (e.g. the owning session terminated, R5.3.3). Returns
    /// the peers to dismiss. Marks the decision resolved-empty so late answers
    /// are ignored.
    pub fn cancel(&mut self) -> Vec<String> {
        if self.resolved.is_none() {
            self.resolved = Some(Resolution {
                scopes: Vec::new(),
                conflicted: false,
                dismiss: self.asked.clone(),
            });
        }
        self.asked.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    fn decision() -> ElicitationDecision {
        ElicitationDecision::new("dec-1", 7, vec!["ide".into(), "tui".into()])
    }

    #[test]
    fn single_answer_resolves_to_that_scope() {
        let mut d = decision();
        assert_eq!(
            d.record_answer("ide", 7, paths(&["/ws/proj"])),
            AnswerOutcome::Recorded { first: true }
        );
        let r = d.resolve().unwrap();
        assert_eq!(r.scopes, paths(&["/ws/proj"]));
        assert!(!r.conflicted);
        // tui was asked but did not answer → dismiss it.
        assert_eq!(r.dismiss, vec!["tui".to_string()]);
    }

    #[test]
    fn unsolicited_answer_rejected() {
        let mut d = decision();
        assert_eq!(
            d.record_answer("stranger", 7, paths(&["/etc"])),
            AnswerOutcome::Rejected(RejectReason::Unsolicited)
        );
    }

    #[test]
    fn stale_generation_rejected() {
        let mut d = decision();
        assert_eq!(
            d.record_answer("ide", 6, paths(&["/ws"])),
            AnswerOutcome::Rejected(RejectReason::StaleGeneration)
        );
    }

    #[test]
    fn most_restrictive_wins_across_two_answers() {
        let mut d = decision();
        d.record_answer("ide", 7, paths(&["/ws"])); // broad
        d.record_answer("tui", 7, paths(&["/ws/proj"])); // narrow
        let r = d.resolve().unwrap();
        assert_eq!(r.scopes, paths(&["/ws/proj"]), "narrowest must win");
        assert!(!r.conflicted);
        assert!(r.dismiss.is_empty(), "both answered");
    }

    #[test]
    fn incomparable_answers_intersect_and_flag_conflict() {
        let mut d = decision();
        d.record_answer("ide", 7, paths(&["/ws/a"]));
        d.record_answer("tui", 7, paths(&["/ws/b"]));
        let r = d.resolve().unwrap();
        assert!(r.conflicted, "disjoint answers must flag re-confirm");
        assert!(
            r.scopes.is_empty(),
            "intersection of disjoint is the safe floor"
        );
    }

    #[test]
    fn resolution_is_idempotent_and_blocks_late_answers() {
        let mut d = decision();
        d.record_answer("ide", 7, paths(&["/ws/proj"]));
        let first = d.resolve().unwrap();
        // late answer after resolution is ignored
        assert_eq!(
            d.record_answer("tui", 7, paths(&["/ws"])),
            AnswerOutcome::Rejected(RejectReason::AlreadyResolved)
        );
        let again = d.resolve().unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn all_answered_detects_completion() {
        let mut d = decision();
        assert!(!d.all_answered());
        d.record_answer("ide", 7, paths(&["/ws/proj"]));
        assert!(!d.all_answered());
        d.record_answer("tui", 7, paths(&["/ws/proj"]));
        assert!(d.all_answered());
    }

    #[test]
    fn cancel_lists_all_peers_to_dismiss_and_blocks_answers() {
        let mut d = decision();
        let dismiss = d.cancel();
        assert_eq!(dismiss, vec!["ide".to_string(), "tui".to_string()]);
        assert!(d.is_resolved());
        assert_eq!(
            d.record_answer("ide", 7, paths(&["/ws"])),
            AnswerOutcome::Rejected(RejectReason::AlreadyResolved)
        );
    }

    #[test]
    fn peer_answering_twice_updates_not_duplicates() {
        let mut d = decision();
        d.record_answer("ide", 7, paths(&["/ws"]));
        let out = d.record_answer("ide", 7, paths(&["/ws/proj"]));
        assert_eq!(out, AnswerOutcome::Recorded { first: false });
        let r = d.resolve().unwrap();
        assert_eq!(r.scopes, paths(&["/ws/proj"]), "updated answer used");
    }
}
