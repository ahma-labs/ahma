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
    /// SSH publickey authentication failed because `~/.ssh/id_*` private keys are
    /// blocked from reading by the sandbox and the key was not loaded into ssh-agent.
    SshPublicKeyAuth,
    /// HTTPS authentication failed: no credential helper answered inside the
    /// sandbox (none configured, or the keychain/`gh` helper is blocked by a
    /// sandbox setting). The more common transport, so it gets the same
    /// first-class, exact remediation as SSH.
    HttpsAuth,
    /// A child tool applied its *own* `sandbox-exec` (SwiftPM's manifest
    /// loader, `xcodebuild` package resolution) and macOS refused to nest it
    /// inside ahma's profile: `sandbox_apply: Operation not permitted`. A
    /// capability refusal, not a path; no grant can fix it (SPEC R7.7).
    NestedSandbox,
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

fn looks_like_nested_sandbox_refusal(line: &str) -> bool {
    line.contains("sandbox_apply: Operation not permitted")
        || line.contains("sandbox-exec: sandbox_apply")
        || line.contains("sandbox_apply failed")
        // Swift's macro plugin server sandboxes itself and dies inside ours.
        || (line.contains("swift-plugin-server") && line.contains("malformed response"))
}

fn looks_like_https_auth_failure(line: &str) -> bool {
    line.contains("could not read Username for 'https://")
        || line.contains("could not read Password for 'https://")
        || line.contains("Authentication failed for 'https://")
        || line.contains("Invalid username or token")
        || line.contains("Password authentication is not supported")
        || line.contains("Support for password authentication was removed")
}

fn looks_like_ssh_publickey_failure(line: &str) -> bool {
    line.contains("Permission denied (publickey)")
        || (line.contains("fatal: Could not read from remote repository")
            && line.contains("publickey"))
}

/// Scan a failed command's `stderr` for a sandbox-contamination signature.
pub fn diagnose(stderr: &str) -> Option<ContaminationHint> {
    diagnose_streams(stderr, "")
}

/// Scan a failed command's `stderr` then `stdout` for a sandbox-contamination or credential signature.
///
/// Returns `Some` with a remediation hint when the failure matches a signature,
/// `None` otherwise. Pure line iteration (no regex), so it cannot backtrack pathologically;
/// returns on the first match to avoid duplicate hints from a multi-line failure.
pub fn diagnose_streams(stderr: &str, stdout: &str) -> Option<ContaminationHint> {
    // Check both streams for SSH publickey failure first
    for line in stderr.lines().chain(stdout.lines()) {
        if looks_like_nested_sandbox_refusal(line) {
            return Some(ContaminationHint {
                kind: ContaminationKind::NestedSandbox,
                remediation: NESTED_SANDBOX_REMEDIATION.to_string(),
            });
        }
        if looks_like_ssh_publickey_failure(line) {
            return Some(ContaminationHint {
                kind: ContaminationKind::SshPublicKeyAuth,
                remediation: SSH_PUBLICKEY_REMEDIATION.clone(),
            });
        }
        if looks_like_https_auth_failure(line) {
            return Some(ContaminationHint {
                kind: ContaminationKind::HttpsAuth,
                remediation: HTTPS_AUTH_REMEDIATION.to_string(),
            });
        }
    }

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

const SCCACHE_REMEDIATION: &str = "Build failed and `sccache` (via RUSTC_WRAPPER) was active. The \
usual cause is the shared sccache *server*: if it was started by a command inside some session's \
ahma sandbox it inherited that sandbox, serves every session on this machine, and can write only \
that one checkout — every other checkout then fails under its own `target/` with a bare \
`Operation not permitted`. One thing for the human to do: `sccache --stop-server && sccache \
--start-server` in a terminal that is not inside any sandbox; `ahma doctor` reports a confined \
server by pid. If the server is fine, the cache directory itself may be outside the sandbox: ask \
the human to grant it, or clear the wrapper for this build (`RUSTC_WRAPPER=\"\" cargo …`).";

const NESTED_SANDBOX_REMEDIATION: &str = "A tool in this command applied its own sandbox \
(`sandbox-exec`) and macOS refused to nest it inside ahma's: `sandbox_apply: Operation not \
permitted`. SwiftPM's manifest loader and `xcodebuild` package resolution do this. It is a \
capability, not a path: no directory grant can help, so do not request one. Options: `swift \
build --disable-sandbox` / `swift package resolve --disable-sandbox` for SwiftPM; for \
`xcodebuild`, add the flag yourself: `xcodebuild -IDEPackageSupportDisableManifestSandbox=YES …` \
(it turns off only SwiftPM's own manifest sandbox for that command; ahma's sandbox still \
confines the build). To make it permanent, the human can run `defaults write \
com.apple.dt.Xcode IDEPackageSupportDisableManifestSandbox -bool YES` once. Swift macros (`@Observable`: \
\"swift-plugin-server produced malformed response\") are the same limit: add \
`'OTHER_SWIFT_FLAGS=$(inherited) -disable-sandbox'` to the xcodebuild command, or `-disable-sandbox` \
to swiftc.";

const BUILD_SCRIPT_COPY_REMEDIATION: &str = "Build failed with `Operation not permitted` while a \
dependency's build script copied a file (e.g. `aws-lc-sys` copying its include headers). On macOS \
this is a `com.apple.provenance` denial: a sandbox refuses the metadata-preserving copy of a \
provenance-stamped source file. It is most common when a host sandbox (Cursor/VS Code) redirects \
`CARGO_TARGET_DIR` to its own cache outside the workspace and denies the build script's write. \
Fix: re-run the build with the host's full-permission/unsandboxed approval, run the build \
through ahma's own sandbox (its terminal hook or `run_terminal_command`, which keeps the build \
inside the workspace), or clear the redirected build cache and rebuild.";

const HTTPS_AUTH_REMEDIATION: &str = "Git over HTTPS could not authenticate inside the sandbox. \
ahma does not block HTTPS credential helpers: the login keychain is allowed (unless `[sandbox] \
allow_keychain = false`) and `~/.config/gh` is readable (unless listed in `deny_credential_reads`). \
One thing for the human to do, on the host: `gh auth login` then `gh auth setup-git` (or `git config \
--global credential.helper osxkeychain` on macOS) so a helper the sandbox can reach holds the token; \
`ahma doctor` reports which helper git uses (`git config --get-all credential.helper`) and whether a \
sandbox setting blocks it.";

/// The refusal and this diagnostic share [`ahma_common::scope_grant::SSH_KEY_USE`],
/// so one failure never gets two stories.
static SSH_PUBLICKEY_REMEDIATION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "SSH authentication failed (`Permission denied (publickey)`): ahma's sandbox keeps \
         private keys under `~/.ssh` unreadable, by design, and no directory grant changes \
         that. {}",
        ahma_common::scope_grant::SSH_KEY_USE
    )
});

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
        assert!(hit.remediation.contains("ahma's own sandbox"));
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

    #[test]
    fn ssh_publickey_denial_is_diagnosed() {
        let stdout = "git@github.com: Permission denied (publickey).\n\
            fatal: Could not read from remote repository.";
        let hit = diagnose_streams("", stdout).expect("ssh publickey failure should match");
        assert_eq!(hit.kind, ContaminationKind::SshPublicKeyAuth);
        // The same statement the grant refusal makes for a key file.
        assert!(
            hit.remediation
                .contains(ahma_common::scope_grant::SSH_KEY_USE),
            "{}",
            hit.remediation
        );
        // The broker is the way through (SPEC R-CRED.1); the agent is only
        // for keys it cannot sign with itself (R-CRED.8).
        if cfg!(unix) {
            assert!(
                hit.remediation.contains("ahma permissions grant ssh-sign"),
                "{}",
                hit.remediation
            );
        }
        assert!(hit.remediation.contains("ssh-add"));
    }
}

#[cfg(test)]
mod https_auth_tests {
    use super::*;

    /// HTTPS is the more common git transport; its failure inside the sandbox
    /// has to come with the exact fix, not a generic "credentials".
    #[test]
    fn https_auth_failure_is_diagnosed_with_exact_fix() {
        let stderr =
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled";
        let hit = diagnose(stderr).expect("https auth failure should match");
        assert_eq!(hit.kind, ContaminationKind::HttpsAuth);
        assert!(
            hit.remediation.contains("gh auth login"),
            "{}",
            hit.remediation
        );
        assert!(
            hit.remediation.contains("credential.helper"),
            "{}",
            hit.remediation
        );
        assert!(
            hit.remediation.contains("allow_keychain"),
            "names the one sandbox setting that can block a keychain helper: {}",
            hit.remediation
        );
        for line in [
            "remote: Invalid username or token. Password authentication is not supported for Git operations.",
            "fatal: Authentication failed for 'https://github.com/x/y.git/'",
        ] {
            assert_eq!(
                diagnose(line).map(|h| h.kind),
                Some(ContaminationKind::HttpsAuth),
                "{line}"
            );
        }
        assert!(diagnose("fatal: repository 'https://x/y' not found").is_none());
    }
}

#[cfg(test)]
mod nested_and_sccache_tests {
    use super::*;

    /// SwiftPM and `xcodebuild` apply their own `sandbox-exec` to the manifest
    /// loader; macOS refuses to nest it inside ahma's profile. No grant fixes
    /// that, so the diagnosis must name the real options.
    #[test]
    fn nested_sandbox_refusal_is_diagnosed_with_the_real_options() {
        let stderr = "error: 'neubit': sandbox_apply: Operation not permitted\n\
            xcodebuild: error: Could not resolve package dependencies";
        let hit = diagnose(stderr).expect("nested sandbox refusal should match");
        assert_eq!(hit.kind, ContaminationKind::NestedSandbox);
        assert!(
            hit.remediation.contains("--disable-sandbox"),
            "{}",
            hit.remediation
        );
        // The agent can fix xcodebuild itself, per command, with no human:
        // verified inside ahma's sandbox on macOS 27 / Xcode 27.
        assert!(
            hit.remediation
                .contains("xcodebuild -IDEPackageSupportDisableManifestSandbox=YES"),
            "{}",
            hit.remediation
        );
        assert!(
            !hit.remediation.contains("sandbox_grant"),
            "a capability refusal must not send the agent hunting for a path: {}",
            hit.remediation
        );
    }

    /// Swift's macro plugin server applies its own sandbox too; inside ahma's
    /// every macro (`@Observable`) fails with "produced malformed response".
    /// `-disable-sandbox` (an `OTHER_SWIFT_FLAGS` setting for xcodebuild)
    /// fixes it, verified inside ahma's sandbox.
    #[test]
    fn a_swift_macro_plugin_refusal_names_the_compiler_flag() {
        let stderr = "AppViewModel.swift:22:2: error: external macro implementation type \
            'ObservationMacros.ObservableMacro' could not be found for macro 'Observable()'; \
            '/Applications/Xcode.app/Contents/Developer/Platforms/iPhoneOS.platform/Developer/usr/bin/swift-plugin-server' produced malformed response";
        let hit = diagnose(stderr).expect("macro plugin refusal should match");
        assert_eq!(hit.kind, ContaminationKind::NestedSandbox);
        assert!(
            hit.remediation
                .contains("OTHER_SWIFT_FLAGS=$(inherited) -disable-sandbox"),
            "{}",
            hit.remediation
        );
    }

    /// A sccache *server* started inside one session's sandbox can only write
    /// that checkout; every other checkout's build then fails with a bare EPERM
    /// under its own `target/`. The hint names the server and the exact restart.
    #[test]
    fn sccache_remediation_names_the_confined_server() {
        let stderr = "sccache: error: failed to execute compile\n\
            error writing `/p/target/debug/deps/x.rlib`: Operation not permitted (os error 1)";
        let hit = diagnose(stderr).unwrap();
        assert_eq!(hit.kind, ContaminationKind::SccacheWrapper);
        assert!(
            hit.remediation
                .contains("sccache --stop-server && sccache --start-server"),
            "{}",
            hit.remediation
        );
        assert!(
            hit.remediation.contains("ahma doctor"),
            "{}",
            hit.remediation
        );
    }
}
