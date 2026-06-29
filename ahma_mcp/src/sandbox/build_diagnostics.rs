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
    /// A dependency's build script failed copying a file with `EPERM` (e.g.
    /// `aws-lc-sys` "Failed to copy include file during build setup"). This is
    /// the `com.apple.provenance` xattr being denied during a metadata-preserving
    /// copy — most visible when a host sandbox (Cursor/VS Code) redirects
    /// `CARGO_TARGET_DIR` to its own cache and denies the build script's write.
    /// The error message carries no `target/` path, so it slips past the
    /// [`ProvenanceResidue`](Self::ProvenanceResidue) heuristic.
    BuildScriptCopy,
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

/// True for a line that looks like a dependency build script failing to copy a
/// file (the `aws-lc-sys` / `ring` family of build-script copy failures). Paired
/// with an `EPERM` line by the caller, this is the signature of a host sandbox
/// denying the metadata-preserving copy of a provenance-stamped source file.
fn looks_like_build_script_copy(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.contains("failed to copy")
        || line.contains("copy include file")
        || line.contains("during build setup")
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

        if looks_like_build_script_copy(line) {
            return Some(ContaminationHint {
                kind: ContaminationKind::BuildScriptCopy,
                remediation: BUILD_SCRIPT_COPY_REMEDIATION.to_string(),
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

const BUILD_SCRIPT_COPY_REMEDIATION: &str = "Build failed with `Operation not permitted` while a \
dependency's build script copied a file (e.g. `aws-lc-sys` copying its include headers). On macOS \
this is a `com.apple.provenance` denial: a sandbox refuses the metadata-preserving copy of a \
provenance-stamped source file. It is most common when a host sandbox (Cursor/VS Code) redirects \
`CARGO_TARGET_DIR` to its own cache outside the workspace and denies the build script's write. \
Fix: re-run the build with the host's full-permission/unsandboxed approval, set \
`AHMA_PREFER_OWN_SANDBOX=1` so ahma applies its own sandbox (which keeps the build inside the \
workspace), or clear the redirected build cache and rebuild.";

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

    #[test]
    fn aws_lc_sys_copy_failure_is_detected() {
        // The exact signature captured under Cursor's host sandbox: a build
        // script copy denied with EPERM, carrying NO `target/` path — so it
        // slips past the provenance-artifact heuristic and needs its own arm.
        let stderr = "\
  --- stderr
  thread 'main' (1693440) panicked at /Users/me/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/aws-lc-sys-0.41.0/builder/main.rs:1116:18:
  Failed to copy include file during build setup: Os { code: 1, kind: PermissionDenied, message: \"Operation not permitted\" }";
        let hit = diagnose(stderr).expect("aws-lc-sys copy failure should be diagnosed");
        assert_eq!(hit.kind, ContaminationKind::BuildScriptCopy);
        assert!(hit.remediation.contains("AHMA_PREFER_OWN_SANDBOX"));
        assert!(hit.remediation.contains("aws-lc-sys"));
    }

    #[test]
    fn build_script_copy_requires_eperm() {
        // A "Failed to copy" line WITHOUT an EPERM signature is an ordinary
        // build error, not a sandbox denial — must not trip the detector.
        let stderr = "error: Failed to copy include file: No such file or directory (os error 2)";
        assert!(diagnose(stderr).is_none());
    }

    #[test]
    fn build_script_copy_during_build_setup_phrase_matches() {
        let stderr = "panicked: something during build setup: Operation not permitted (os error 1)";
        let hit = diagnose(stderr).expect("'during build setup' + EPERM should match");
        assert_eq!(hit.kind, ContaminationKind::BuildScriptCopy);
    }

    #[test]
    fn provenance_artifact_still_wins_over_generic_copy() {
        // A target/ artifact EPERM is classified as provenance residue (its
        // remediation is the more specific `rm -rf target`), not the generic
        // build-script-copy arm, because the artifact check runs first.
        let stderr =
            "error writing `/p/target/debug/deps/x.rlib`: Operation not permitted (os error 1)";
        let hit = diagnose(stderr).unwrap();
        assert_eq!(hit.kind, ContaminationKind::ProvenanceResidue);
    }
}
