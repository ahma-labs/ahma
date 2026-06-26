//! Turn the *opaque* failures the sandbox provokes in `cargo` builds into a
//! single, actionable line for the user.
//!
//! [`denial_scan`](super::denial_scan) handles the case the kernel actually
//! denies: a write to a path **outside** the workspace scope (sccache's cache,
//! `~/.cargo`, …), which the grant flow can offer to allow. This module handles
//! the nastier sibling it deliberately skips: a write that fails with `EPERM`
//! (`Operation not permitted`) on a path that **is** in scope — the
//! `com.apple.provenance` contamination.
//!
//! ## Why this needs its own detector
//!
//! On macOS, Seatbelt stamps every file a sandboxed process writes with the
//! `com.apple.provenance` extended attribute. A *different* process (a parallel
//! worktree build, an IDE background `cargo check`, or the same build after a
//! wrapper change) can then no longer overwrite those files — the write returns
//! `Operation not permitted (os error 1)` even though the path sits squarely
//! inside the workspace. There is **no kernel denial event** here: nothing is
//! out of scope, so [`denial_scan::scan_denial`](super::denial_scan::scan_denial)
//! finds an in-scope path and the grant flow correctly declines to prompt — and
//! the user is left with a bare, un-actionable `os error 1` deep in a build log.
//!
//! Compounding it, an `RUSTC_WRAPPER=sccache` inherited from the environment runs
//! sccache *inside* the sandbox, where its out-of-scope cache writes both fail and
//! re-seed the contamination. The sccache cache lives outside the workspace scope,
//! so a sandboxed build that uses it is denied unless that cache directory is
//! granted to the sandbox (or the wrapper is cleared for the build).
//!
//! This detector recognises that exact signature and returns a remediation hint,
//! so the failure reads as "here is what happened and what to do" instead of a
//! kernel errno.

/// A diagnosis of a sandbox-provoked build failure, with a ready-to-surface
/// remediation message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContaminationHint {
    /// Which signature matched — for logs and the "why".
    pub kind: ContaminationKind,
    /// A user-facing, actionable remediation line.
    pub remediation: String,
}

/// The class of contamination detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContaminationKind {
    /// `EPERM` writing a build artifact inside the workspace — the
    /// `com.apple.provenance` residue from a prior sandboxed write.
    ProvenanceResidue,
    /// An `sccache`/`RUSTC_WRAPPER` was active and tripped the sandbox.
    SccacheWrapper,
}

/// True for a token that looks like a Rust build artifact path (the things a
/// provenance-contaminated `target/` makes un-overwritable).
fn looks_like_build_artifact(line: &str) -> bool {
    // The directory that always appears, plus the artifact extensions cargo/rustc
    // emit. `target` alone is too loose (it matches prose); pairing it with an
    // EPERM line — checked by the caller — is what makes this specific.
    line.contains("/target/")
        || line.contains(".rlib")
        || line.contains(".rmeta")
        || line.ends_with(".d")
        || line.contains(".d`")
        || line.contains(".d'")
        || line.contains(".d:")
}

/// Scan a failed build's `stderr` for a sandbox-contamination signature.
///
/// Returns `Some` with a remediation hint when the failure matches the
/// provenance-residue or sccache-wrapper pattern, `None` otherwise. Pure line
/// iteration (no regex), so it cannot backtrack pathologically; returns on the
/// first match to avoid duplicate hints from a multi-line failure.
pub fn diagnose(stderr: &str) -> Option<ContaminationHint> {
    let mentions_sccache = stderr.contains("sccache");

    for line in stderr.lines() {
        let eperm = line.contains("Operation not permitted")
            || line.contains("os error 1")
            // `EPERM` itself, as some toolchains print it.
            || line.contains("(EPERM)");

        if !eperm {
            continue;
        }

        if mentions_sccache || line.contains("sccache") {
            return Some(ContaminationHint {
                kind: ContaminationKind::SccacheWrapper,
                remediation: SCCACHE_REMEDIATION.to_string(),
            });
        }

        if looks_like_build_artifact(line) {
            return Some(ContaminationHint {
                kind: ContaminationKind::ProvenanceResidue,
                remediation: PROVENANCE_REMEDIATION.to_string(),
            });
        }
    }

    // A bare sccache mention without an EPERM line is not, by itself, a failure
    // signature — sccache prints stats and cache hits on success too.
    None
}

const PROVENANCE_REMEDIATION: &str = "Build failed with `Operation not permitted` writing a file \
inside the workspace `target/` directory. This is macOS `com.apple.provenance` contamination: a \
file written by one sandboxed build cannot be overwritten by another process (a parallel build, an \
IDE background `cargo check`, or a build whose compiler wrapper changed). Fix: remove the \
contaminated build directory and rebuild — `rm -rf target`. Avoid running a second sandboxed build \
against the same target dir concurrently.";

const SCCACHE_REMEDIATION: &str = "Build failed and `sccache` (via RUSTC_WRAPPER) was active inside \
the sandbox — its cache lives outside the workspace, so its reads/writes are denied and can \
contaminate the target dir. Fix: grant the sccache cache directory to the sandbox scope (e.g. \
`ahma sandbox grant <cache-dir>`) so the build can read/write it, or clear the wrapper for this \
build (`RUSTC_WRAPPER=\"\" cargo …`).";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_eperm_on_dep_file_is_detected() {
        let stderr = "error: error writing dependencies to \
            `/Users/me/proj/target/debug/deps/ring-591f4a0e8e94c602.d`: \
            Operation not permitted (os error 1)";
        let hit = diagnose(stderr).expect("provenance signature should match");
        assert_eq!(hit.kind, ContaminationKind::ProvenanceResidue);
        assert!(hit.remediation.contains("com.apple.provenance"));
        assert!(hit.remediation.contains("rm -rf target"));
    }

    #[test]
    fn eperm_on_rmeta_artifact_is_detected() {
        let stderr =
            "failed to write `/w/target/debug/foo.rmeta`: Operation not permitted (os error 1)";
        let hit = diagnose(stderr).expect("rmeta artifact EPERM should match");
        assert_eq!(hit.kind, ContaminationKind::ProvenanceResidue);
    }

    #[test]
    fn sccache_with_eperm_is_classified_as_wrapper() {
        let stderr = "sccache: error: Permission denied\n\
            error writing /home/u/.cache/sccache/x: Operation not permitted (os error 1)";
        let hit = diagnose(stderr).expect("sccache + EPERM should match");
        assert_eq!(hit.kind, ContaminationKind::SccacheWrapper);
        assert!(hit.remediation.contains("RUSTC_WRAPPER"));
    }

    #[test]
    fn sccache_wrapper_wins_even_when_artifact_path_present() {
        // When sccache is in play, its remediation is the more useful one even
        // though the EPERM line also names a target artifact.
        let stderr = "sccache active\n\
            error writing `/proj/target/debug/deps/x.d`: Operation not permitted (os error 1)";
        let hit = diagnose(stderr).unwrap();
        assert_eq!(hit.kind, ContaminationKind::SccacheWrapper);
    }

    #[test]
    fn eperm_without_build_artifact_or_sccache_is_ignored() {
        // EPERM on some unrelated path is not our signature — denial_scan / the
        // grant flow owns out-of-scope denials.
        let stderr = "cannot write /etc/hosts: Operation not permitted (os error 1)";
        assert!(diagnose(stderr).is_none());
    }

    #[test]
    fn ordinary_compile_error_is_ignored() {
        let stderr = "error[E0382]: borrow of moved value: `x`\n  --> src/main.rs:10:5";
        assert!(diagnose(stderr).is_none());
    }

    #[test]
    fn successful_sccache_stats_without_eperm_is_ignored() {
        // sccache prints cache stats on success; a bare mention must not trip.
        let stderr = "Compiling foo v0.1.0\nsccache: cache hits 42, misses 3";
        assert!(diagnose(stderr).is_none());
    }

    #[test]
    fn target_in_prose_without_eperm_is_ignored() {
        let stderr = "note: the target/ directory is large";
        assert!(diagnose(stderr).is_none());
    }

    #[test]
    fn first_signature_only_across_many_lines() {
        let stderr = "writing `/p/target/a.d`: Operation not permitted (os error 1)\n\
            writing `/p/target/b.d`: Operation not permitted (os error 1)";
        // Should not panic or duplicate; returns a single hint.
        let hit = diagnose(stderr).unwrap();
        assert_eq!(hit.kind, ContaminationKind::ProvenanceResidue);
    }

    #[test]
    fn empty_stderr_is_none() {
        assert!(diagnose("").is_none());
    }
}
