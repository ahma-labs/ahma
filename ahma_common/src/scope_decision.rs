//! Scope-change classification and conflict resolution (SPEC R5.3).
//!
//! Two security-critical, pure decisions live here:
//!
//!  1. **Downgrade detection** — does a *proposed* scope grant any write access
//!     not already granted by the *established* scope? Only a downgrade
//!     (widening) may trigger a user prompt (R5.3); establishment and narrowing
//!     are applied silently-but-visibly.
//!
//!  2. **Most-restrictive-wins** — when two surfaces answer the same decision,
//!     the narrower answer wins regardless of arrival order (R5.3.4). A widening
//!     answer can never beat a narrowing one by timing.
//!
//! "Access" is modelled as a set of canonical write-root subtrees. A path `p`
//! is *covered* by a root set `R` when some `r ∈ R` is an ancestor-or-equal of
//! `p`. Root `r` is covered by `R` when `R` covers `r` itself.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// How a proposed scope relates to an established one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeDelta {
    /// Proposed grants exactly the same access as established.
    Same,
    /// Proposed grants strictly less access (every proposed root is covered by
    /// established, and established covers something proposed does not).
    Narrows,
    /// Proposed grants access established did not (at least one proposed root is
    /// not covered by established). This is a security downgrade.
    Widens,
}

/// True when `root` is covered by some root in `set` (an ancestor-or-equal).
fn is_covered_by(root: &Path, set: &[PathBuf]) -> bool {
    set.iter().any(|r| root.starts_with(r))
}

/// Does `proposed` grant any access not already granted by `established`?
/// This is the downgrade predicate: `true` ⇒ a prompt is required (R5.3).
pub fn grants_new_access(established: &[PathBuf], proposed: &[PathBuf]) -> bool {
    proposed.iter().any(|p| !is_covered_by(p, established))
}

/// Classify how `proposed` changes access relative to `established`.
///
/// Expressed in terms of [`compare_restrictiveness`] rather than re-deriving
/// the same two `grants_new_access` checks: `established` is more restrictive
/// (`Greater`, i.e. `proposed` narrower) narrows; anything else where
/// `proposed` grants new access (`Less`, or the incomparable `None`) widens.
pub fn classify_scope_change(established: &[PathBuf], proposed: &[PathBuf]) -> ScopeDelta {
    match compare_restrictiveness(established, proposed) {
        Some(Ordering::Equal) => ScopeDelta::Same,
        Some(Ordering::Greater) => ScopeDelta::Narrows,
        Some(Ordering::Less) | None => ScopeDelta::Widens,
    }
}

/// Order two answers by restrictiveness for most-restrictive-wins (R5.3.4).
///
/// Returns:
///  - `Less`    when `a` is narrower than `b` (a grants a subset of b's access)
///  - `Greater` when `b` is narrower than `a`
///  - `Equal`   when they grant the same access
///  - `None`    when neither covers the other (incomparable) — the caller must
///    fall back to the intersection and re-confirm (R5.3.4).
pub fn compare_restrictiveness(a: &[PathBuf], b: &[PathBuf]) -> Option<Ordering> {
    let a_widens = grants_new_access(b, a); // a grants something b doesn't
    let b_widens = grants_new_access(a, b); // b grants something a doesn't
    match (a_widens, b_widens) {
        (false, false) => Some(Ordering::Equal),
        (false, true) => Some(Ordering::Less), // a ⊆ b ⇒ a narrower
        (true, false) => Some(Ordering::Greater),
        (true, true) => None, // incomparable
    }
}

/// Pick the most-restrictive of two answers (R5.3.4). When incomparable, returns
/// the **intersection** of covered access (each root of one clipped to the
/// other) as the conservative safe choice, and the caller should re-confirm.
pub fn most_restrictive(a: &[PathBuf], b: &[PathBuf]) -> Vec<PathBuf> {
    match compare_restrictiveness(a, b) {
        Some(Ordering::Less) | Some(Ordering::Equal) => a.to_vec(),
        Some(Ordering::Greater) => b.to_vec(),
        None => intersection(a, b),
    }
}

/// The intersection of two root sets: roots present in one that are covered by
/// the other (from both directions), deduplicated. This grants only access both
/// answers agreed on — the safe floor when answers are incomparable.
pub fn intersection(a: &[PathBuf], b: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in a.iter().filter(|p| is_covered_by(p, b)) {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    for p in b.iter().filter(|p| is_covered_by(p, a)) {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn identical_scopes_are_same() {
        let a = paths(&["/ws/proj"]);
        assert_eq!(classify_scope_change(&a, &a), ScopeDelta::Same);
        assert!(!grants_new_access(&a, &a));
    }

    #[test]
    fn subdir_narrows() {
        let est = paths(&["/ws/proj"]);
        let prop = paths(&["/ws/proj/src"]);
        assert_eq!(classify_scope_change(&est, &prop), ScopeDelta::Narrows);
        assert!(!grants_new_access(&est, &prop));
    }

    #[test]
    fn parent_widens_and_is_a_downgrade() {
        let est = paths(&["/ws/proj"]);
        let prop = paths(&["/ws"]);
        assert_eq!(classify_scope_change(&est, &prop), ScopeDelta::Widens);
        assert!(grants_new_access(&est, &prop), "parent grants new access");
    }

    #[test]
    fn sibling_widens() {
        let est = paths(&["/ws/proj"]);
        let prop = paths(&["/ws/other"]);
        assert_eq!(classify_scope_change(&est, &prop), ScopeDelta::Widens);
    }

    #[test]
    fn adding_a_root_widens() {
        let est = paths(&["/ws/proj"]);
        let prop = paths(&["/ws/proj", "/tmp/extra"]);
        assert_eq!(classify_scope_change(&est, &prop), ScopeDelta::Widens);
    }

    #[test]
    fn dropping_a_root_narrows() {
        let est = paths(&["/ws/proj", "/ws/extra"]);
        let prop = paths(&["/ws/proj"]);
        assert_eq!(classify_scope_change(&est, &prop), ScopeDelta::Narrows);
    }

    #[test]
    fn restrictiveness_subset_is_less() {
        let a = paths(&["/ws/proj/src"]);
        let b = paths(&["/ws/proj"]);
        assert_eq!(compare_restrictiveness(&a, &b), Some(Ordering::Less));
        assert_eq!(most_restrictive(&a, &b), a);
    }

    #[test]
    fn restrictiveness_incomparable_returns_none_and_intersects() {
        let a = paths(&["/ws/a"]);
        let b = paths(&["/ws/b"]);
        assert_eq!(compare_restrictiveness(&a, &b), None);
        // Disjoint siblings intersect to nothing — the safest floor.
        assert!(most_restrictive(&a, &b).is_empty());
    }

    #[test]
    fn most_restrictive_picks_narrower_regardless_of_order() {
        let broad = paths(&["/ws"]);
        let narrow = paths(&["/ws/proj"]);
        // narrower wins whether it is the first or second argument (R5.3.4)
        assert_eq!(most_restrictive(&broad, &narrow), narrow);
        assert_eq!(most_restrictive(&narrow, &broad), narrow);
    }

    #[test]
    fn intersection_keeps_only_shared_access() {
        let a = paths(&["/ws/proj", "/ws/shared"]);
        let b = paths(&["/ws/shared/sub", "/ws/other"]);
        let inter = intersection(&a, &b);
        // /ws/shared/sub is covered by /ws/shared; nothing else is shared.
        assert_eq!(inter, paths(&["/ws/shared/sub"]));
    }
}
