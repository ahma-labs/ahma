//! Heuristic extraction of a denied path from a failed command's stderr.
//!
//! A runtime kernel denial (Landlock `EACCES`, Seatbelt `deny`, a read-only
//! remount) does **not** hand ahma the offending path — the command merely exits
//! non-zero and may print an error mentioning the path. This module recognises a
//! few well-known denial signatures and pulls out a *candidate* path so the
//! grant flow can offer "grant access to X?".
//!
//! ## This only ever *suggests*
//!
//! The extracted path is a hint, never an authorization. Nothing is granted
//! without the human's explicit approval at a surface, and the path is
//! canonicalized (by [`crate::sandbox`]'s grant coordinator) before it is shown or
//! stored. A command that prints a *fake* denial line (e.g.
//! `echo "Permission denied: /etc/shadow"`) can at worst raise a human-gated
//! prompt — it cannot escalate on its own. That is a noise/UX risk, not a
//! privilege risk, and is the explicit reason this is a heuristic side-channel
//! rather than part of the command result.
//!
//! Scanning is plain byte/line iteration (no regex) so it cannot be made to
//! backtrack pathologically on adversarial input, and it returns at most **one**
//! hit (the first) to avoid a prompt storm from a multi-line failure.

use std::path::PathBuf;

use ahma_common::config::ScopeAccess;

/// A denial signature matched in stderr, with the path it referenced and the
/// access level the denied operation implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialHit {
    /// The candidate path the denial referenced (raw, not yet canonicalized).
    pub path: PathBuf,
    /// The access the *denied* operation needed: a blocked write ⇒ `Rw`, a blocked
    /// read ⇒ `Ro`. Ambiguous "permission denied" defaults to the safer `Ro`.
    pub access: ScopeAccess,
    /// Which signature matched — for logs and the prompt's "why".
    pub pattern: &'static str,
}

/// Scan `stderr` for the first kernel-denial signature and extract its path.
/// Returns `None` when nothing matches or no absolute path can be pulled from the
/// matching line.
pub fn scan_denial(stderr: &str) -> Option<DenialHit> {
    for line in stderr.lines() {
        if let Some(hit) = scan_line(line) {
            return Some(hit);
        }
    }
    None
}

/// Match one line. Order matters: the most specific (access-revealing) signatures
/// are tried before the ambiguous `Permission denied`.
fn scan_line(line: &str) -> Option<DenialHit> {
    let is_seatbelt = line.contains("deny(") || line.contains("Sandbox:");

    // macOS Seatbelt audit lines: `... deny(1) file-write-data /path` etc. These
    // reveal the exact operation, so the access level is unambiguous.
    if is_seatbelt
        && (line.contains("file-write") || line.contains("file-create"))
        && let Some(path) = extract_abs_path(line)
    {
        return Some(DenialHit {
            path,
            access: ScopeAccess::Rw,
            pattern: "seatbelt file-write",
        });
    }
    if is_seatbelt
        && line.contains("file-read")
        && let Some(path) = extract_abs_path(line)
    {
        return Some(DenialHit {
            path,
            access: ScopeAccess::Ro,
            pattern: "seatbelt file-read",
        });
    }

    // A write that hit a read-only mount or an explicitly-denied write op ⇒ `Rw`.
    if (line.contains("Read-only file system") || line.contains("Operation not permitted"))
        && let Some(path) = extract_abs_path(line)
    {
        return Some(DenialHit {
            path,
            access: ScopeAccess::Rw,
            pattern: "read-only / not-permitted",
        });
    }

    // Ambiguous: a read or a write may have been denied. Suggest the safer `Ro`;
    // the human can choose read+write at the prompt.
    if line.contains("Permission denied")
        && let Some(path) = extract_abs_path(line)
    {
        return Some(DenialHit {
            path,
            access: ScopeAccess::Ro,
            pattern: "permission denied",
        });
    }

    None
}

/// Pull the first absolute (`/`-rooted) path token out of a line, trimming
/// surrounding quotes/backticks and trailing punctuation. Returns `None` when the
/// line has no plausible absolute path (a bare `/` does not count).
fn extract_abs_path(line: &str) -> Option<PathBuf> {
    for raw in line.split_whitespace() {
        // Trim the message's own grammar from both ends in one pass: surrounding
        // quotes/brackets and trailing punctuation can be interleaved (e.g. the
        // token `` `/path`: `` ends with a backtick *then* a colon).
        let token = raw.trim_matches(|c| {
            matches!(
                c,
                '`' | '\'' | '"' | '(' | ')' | '[' | ']' | '<' | '>' | ':' | ',' | '.' | ';'
            )
        });
        if token.len() > 1 && token.starts_with('/') {
            return Some(PathBuf::from(token));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seatbelt_write_denial_is_rw() {
        let stderr =
            "Sandbox: cargo(7421) deny(1) file-write-data /Users/me/Library/Caches/sccache/0";
        let hit = scan_denial(stderr).expect("seatbelt write line matches");
        assert_eq!(hit.access, ScopeAccess::Rw);
        assert_eq!(
            hit.path,
            PathBuf::from("/Users/me/Library/Caches/sccache/0")
        );
    }

    #[test]
    fn seatbelt_read_denial_is_ro() {
        let stderr = "Sandbox: rustc(99) deny(1) file-read-data /opt/toolchains/x/lib";
        let hit = scan_denial(stderr).expect("seatbelt read line matches");
        assert_eq!(hit.access, ScopeAccess::Ro);
        assert_eq!(hit.path, PathBuf::from("/opt/toolchains/x/lib"));
    }

    #[test]
    fn read_only_file_system_is_rw() {
        let stderr = "error: failed to create directory `/Users/me/.cache/foo`: Read-only file system (os error 30)";
        let hit = scan_denial(stderr).expect("read-only line matches");
        assert_eq!(hit.access, ScopeAccess::Rw);
        assert_eq!(hit.path, PathBuf::from("/Users/me/.cache/foo"));
    }

    #[test]
    fn permission_denied_defaults_to_ro() {
        let stderr = "cat: /etc/private/key: Permission denied";
        let hit = scan_denial(stderr).expect("permission-denied line matches");
        assert_eq!(
            hit.access,
            ScopeAccess::Ro,
            "ambiguous denial suggests the safer Ro"
        );
        assert_eq!(hit.path, PathBuf::from("/etc/private/key"));
    }

    #[test]
    fn fake_bait_line_yields_only_a_suggestion_not_a_grant() {
        // A command can print a fake denial. scan_denial still only *extracts* a
        // candidate — there is no grant here, just a hit a human would have to
        // approve. This documents the noise (not escalation) risk.
        let stderr = r#"echo "Permission denied: /etc/shadow""#;
        let hit = scan_denial(stderr).expect("the bait line is parsed");
        assert_eq!(hit.path, PathBuf::from("/etc/shadow"));
        // Nothing in this module grants anything; the type is just a DenialHit.
    }

    #[test]
    fn first_hit_only_across_multiple_lines() {
        let stderr = "\
warning: something
/first/path: Permission denied
/second/path: Permission denied";
        let hit = scan_denial(stderr).unwrap();
        assert_eq!(
            hit.path,
            PathBuf::from("/first/path"),
            "only the first hit is returned"
        );
    }

    #[test]
    fn non_denial_stderr_yields_none() {
        assert!(
            scan_denial("warning: unused variable `x`\nerror[E0382]: borrow of moved value")
                .is_none()
        );
    }

    #[test]
    fn denial_line_without_absolute_path_yields_none() {
        assert!(scan_denial("Permission denied").is_none());
        assert!(scan_denial("write failed: Operation not permitted").is_none());
    }

    #[test]
    fn bare_slash_is_not_a_path() {
        assert!(scan_denial("/ : Permission denied").is_none());
    }

    #[test]
    fn trailing_punctuation_is_trimmed() {
        let hit = scan_denial("could not open '/var/db/x'. Permission denied").unwrap();
        assert_eq!(hit.path, PathBuf::from("/var/db/x"));
    }
}
