//! Post-execution observation for the deferred-host hook path.
//!
//! When ahma's terminal hook detects a host sandbox (Cursor/VS Code/Docker) it
//! [defers to the host](super::defer_to_host_decision) (SPEC R7): the command
//! runs unchanged inside the host's kernel sandbox and ahma does **not** re-wrap
//! it. The cost of that honesty is a blind spot — if the *host* sandbox then
//! denies a write (the classic `aws-lc-sys` build-script copy into the host's
//! injected `CARGO_TARGET_DIR` cache), ahma's pre-execution hook never sees it
//! and the user is left with a bare `Operation not permitted` deep in a build
//! log, with no guidance.
//!
//! This module closes that blind spot on the **post**-execution side. Cursor's
//! `afterShellExecution` hook hands ahma the finished command's output; we scan
//! it for a sandbox-denial signature (reusing the same detectors the
//! authoritative MCP path uses) and, on a hit, return an actionable remediation
//! the editor can surface. ahma cannot *grant* its way out of a host-cache
//! denial (there is nothing of ahma's to grant), so the goal here is strictly to
//! **stop failing silently**: turn an opaque errno into "here is what happened
//! and what to do".

use crate::sandbox::{build_diagnostics, denial_scan};

/// Prefix that frames every surfaced message with the honest disclosure that the
/// command ran under the host sandbox, not ahma's.
const DEFER_NOTE: &str = "ahma deferred this command to the host sandbox (e.g. Cursor/VS Code), so \
it ran under the host's sandbox rather than ahma's.";

/// Inspect a finished shell command's combined output and return an actionable
/// remediation message when a sandbox-denial signature is present.
///
/// Returns `None` when the output carries no recognised denial signature. A
/// matched signature implies the command failed because of a sandbox block, so
/// `failed` is advisory: a clean command with no signature is never surfaced,
/// but an unknown exit status does not suppress a clear denial signature.
///
/// Detection order mirrors the authoritative path:
///  1. [`build_diagnostics::diagnose`] — in-scope `EPERM` contamination
///     (provenance residue, sccache, and the `aws-lc-sys` build-script copy).
///  2. [`denial_scan::scan_denial`] — an out-of-scope path the host kernel blocked.
pub fn surface_sandbox_denial(output: &str, failed: bool) -> Option<String> {
    if output.is_empty() {
        return None;
    }

    if let Some(hint) = build_diagnostics::diagnose(output) {
        return Some(format!("{DEFER_NOTE} {}", hint.remediation));
    }

    if let Some(hit) = denial_scan::scan_denial(output) {
        let path = hit.path.display();
        return Some(format!(
            "{DEFER_NOTE} The host sandbox denied access to `{path}`. To recover: re-run the \
             command with the host's full-permission/unsandboxed approval, or set \
             `AHMA_PREFER_OWN_SANDBOX=1` to apply ahma's own sandbox and then grant the path \
             (`ahma sandbox grant {path}`), or adjust the host sandbox configuration."
        ));
    }

    // No signature: nothing to surface, regardless of exit status.
    let _ = failed;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REGRESSION: the captured `aws-lc-sys` failure that motivated this module
    /// must produce a remediation message through the post-exec scanner. Before
    /// this work it slipped past both detectors (no `target/` path) and the user
    /// saw only a bare `Operation not permitted`.
    #[test]
    fn aws_lc_sys_host_cache_denial_is_surfaced() {
        let output = "\
  --- stderr
  thread 'main' panicked at /Users/me/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/aws-lc-sys-0.41.0/builder/main.rs:1116:18:
  Failed to copy include file during build setup: Os { code: 1, kind: PermissionDenied, message: \"Operation not permitted\" }";
        let msg = surface_sandbox_denial(output, true).expect("aws-lc-sys denial must surface");
        assert!(
            msg.contains("deferred this command to the host sandbox"),
            "message must disclose host deferral: {msg}"
        );
        assert!(
            msg.contains("AHMA_PREFER_OWN_SANDBOX"),
            "message must offer the prefer-own-sandbox recovery: {msg}"
        );
    }

    #[test]
    fn out_of_scope_kernel_denial_is_surfaced_with_grant_path() {
        let output = "cat: /etc/private/key: Permission denied";
        let msg = surface_sandbox_denial(output, true).expect("kernel denial must surface");
        assert!(
            msg.contains("/etc/private/key"),
            "must name the denied path: {msg}"
        );
        assert!(
            msg.contains("ahma sandbox grant /etc/private/key"),
            "must offer the grant recovery: {msg}"
        );
    }

    #[test]
    fn provenance_residue_is_surfaced() {
        let output = "error: error writing dependencies to \
            `/Users/me/proj/target/debug/deps/ring-591f4a0e8e94c602.d`: \
            Operation not permitted (os error 1)";
        let msg = surface_sandbox_denial(output, true).expect("provenance residue must surface");
        assert!(msg.contains("com.apple.provenance"), "{msg}");
    }

    #[test]
    fn clean_output_yields_nothing() {
        assert!(surface_sandbox_denial("Compiling foo v0.1.0\n    Finished", false).is_none());
    }

    #[test]
    fn empty_output_yields_nothing() {
        assert!(surface_sandbox_denial("", true).is_none());
    }

    #[test]
    fn ordinary_compile_error_yields_nothing() {
        // A normal failure (non-zero exit) with no denial signature must NOT be
        // surfaced — this hook is only for sandbox denials.
        let output = "error[E0382]: borrow of moved value: `x`\n  --> src/main.rs:10:5";
        assert!(surface_sandbox_denial(output, true).is_none());
    }
}
