//! Heuristic extraction of a denied path from a failed command's output.
//!
//! A runtime kernel denial (Landlock `EACCES`, Seatbelt `deny`, a read-only
//! remount) does **not** hand ahma the offending path — the command merely exits
//! non-zero and may print an error mentioning the path. This module recognises a
//! few well-known denial signatures and pulls out a *candidate* path so the
//! grant flow can offer "grant access to X?".
//!
//! Two things it must get right, because getting them wrong pushes an agent to
//! give up on the sandbox and re-run the command outside it:
//!
//! * **Both streams.** Use [`scan_denial_streams`], not [`scan_denial`], wherever
//!   stdout is available: `cmd 2>&1 | tail` empties stderr, and a denial that
//!   disappears when the caller merges the streams is a trap.
//! * **The tool's words, not just the kernel's.** Crates that take an advisory
//!   file lock catch the errno themselves and report e.g. "attempted to take an
//!   exclusive lock on a read-only path" — no "Permission denied" anywhere.
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

/// How many lines a keyword-only denial line may look back to find the path it
/// refers to. Cargo/anyhow print `error: failed to open: <path>` then, a couple
/// of lines later under `Caused by:`, the bare `Operation not permitted (os error
/// 1)`. A small window correlates the two without letting an unrelated earlier
/// path bleed into a much later denial.
const MULTILINE_LOOKBACK: usize = 5;

/// Scan a failed command's **stderr, then stdout**, for a denial signature.
///
/// Prefer this over [`scan_denial`] at any call site that has both streams.
///
/// Scanning stderr alone loses the denial whenever the two streams are merged —
/// and `2>&1` is not an exotic case, it is what agents and shell pipelines write
/// by habit (`cargo deny check 2>&1 | tail`). With stderr emptied, the denial is
/// invisible, the agent gets a bare "exit code 1", and the grant flow it should
/// have entered never engages. The observed consequence is the bad one: the agent
/// concludes the sandbox is broken and re-runs the command *outside* it.
///
/// stderr wins when both carry a signature: it is the stream the denial was
/// actually written to when the command did not merge them.
pub fn scan_denial_streams(stderr: &str, stdout: &str) -> Option<DenialHit> {
    scan_denial(stderr).or_else(|| scan_denial(stdout))
}

/// Scan one stream for the first kernel-denial signature and extract its path.
/// Returns `None` when nothing matches or no absolute path can be associated with
/// a denial.
///
/// Two shapes are recognised:
///  1. **Single line** — the denial keyword and the path are on the same line
///     (Seatbelt audit lines, `cat: /p: Permission denied`, …). See [`scan_line`].
///  2. **Multi-line** — the path is on one line and the denial keyword on a later
///     line within [`MULTILINE_LOOKBACK`] (the cargo/anyhow `Caused by:` form).
pub fn scan_denial(stderr: &str) -> Option<DenialHit> {
    // The most recent line that carried an absolute path but no denial keyword,
    // plus how many lines ago it was seen, so a later keyword-only line can be
    // attributed back to it (shape 2).
    let mut recent_path: Option<(PathBuf, usize)> = None;

    for line in stderr.lines() {
        // Shape 1: keyword and path on the same line (most specific).
        if let Some(hit) = scan_line(line) {
            return Some(hit);
        }

        // Shape 2: a keyword-only denial line correlates with a path seen just
        // above it. This is what makes `cargo install` / `cargo binstall`
        // failures (and any other anyhow `Caused by:` denial) detectable.
        if let Some(access) = denial_keyword_access(line)
            && let Some((path, age)) = recent_path.take()
            && age <= MULTILINE_LOOKBACK
        {
            return Some(DenialHit {
                path,
                access,
                pattern: "multi-line denial",
            });
        }

        // Track a path on this line as the candidate for a later keyword line,
        // and age out a previously tracked path so it cannot match too far away.
        if let Some(path) = extract_abs_path(line) {
            recent_path = Some((path, 0));
        } else if let Some((path, age)) = recent_path.take()
            && age < MULTILINE_LOOKBACK
        {
            recent_path = Some((path, age + 1));
        }
    }
    None
}

/// The access level a denial keyword implies, for a line that contains a denial
/// signature but no inline path (the second line of a multi-line denial). Returns
/// `None` when the line carries no recognised denial keyword.
///
/// A bare `os error 5` is deliberately **not** treated as a denial here: it is
/// `EIO` on Unix and only `ACCESS_DENIED` on Windows, so without the explicit
/// `Access is denied` text it is too ambiguous to correlate across lines.
fn denial_keyword_access(line: &str) -> Option<ScopeAccess> {
    let is_seatbelt = line.contains("deny(") || line.contains("Sandbox:");
    if is_seatbelt && (line.contains("file-write") || line.contains("file-create")) {
        return Some(ScopeAccess::Rw);
    }
    if is_seatbelt && line.contains("file-read") {
        return Some(ScopeAccess::Ro);
    }
    // Kept in step with `scan_line`: a tool that reports the errno in its own words
    // ("...on a read-only path") may equally split the path and the reason across
    // lines, so the multi-line shape must recognise the same wording.
    let lower = line.to_ascii_lowercase();
    if lower.contains("read-only file system")
        || lower.contains("read-only path")
        || lower.contains("operation not permitted")
    {
        return Some(ScopeAccess::Rw);
    }
    if line.contains("Access is denied") || line.contains("Permission denied") {
        return Some(ScopeAccess::Ro);
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
    //
    // `read-only path` is not the kernel's wording but a tool's: crates that take
    // an advisory file lock (cargo-deny's advisory DB, cargo's package cache)
    // catch the errno themselves and report e.g. "attempted to take an exclusive
    // lock on a read-only path". That is a *write* denial, and missing it is not
    // academic — it is the exact denial that sent an agent off to re-run the
    // command outside the sandbox instead of asking for a grant.
    let lower = line.to_ascii_lowercase();
    if (lower.contains("read-only file system")
        || lower.contains("read-only path")
        || lower.contains("operation not permitted"))
        && let Some(path) = extract_abs_path(line)
    {
        return Some(DenialHit {
            path,
            access: ScopeAccess::Rw,
            pattern: "read-only / not-permitted",
        });
    }

    // Windows (Job Object / AppContainer) denial: `Access is denied. (os error 5)`.
    // Like `Permission denied` it does not reveal read-vs-write, so suggest the
    // safer `Ro`. Note `os error 5` is `ACCESS_DENIED` on Windows but `EIO` on
    // Unix, so a bare `os error 5` (no "Access is denied" text) is only trusted
    // when the path is a Windows path — otherwise a Unix I/O error would be
    // misread as a grant prompt.
    if (line.contains("Access is denied") || line.contains("os error 5"))
        && let Some(path) = extract_abs_path(line)
    {
        let is_access_denied =
            line.contains("Access is denied") || is_windows_abs(path.to_str().unwrap_or(""));
        if is_access_denied {
            return Some(DenialHit {
                path,
                access: ScopeAccess::Ro,
                pattern: "windows access-denied",
            });
        }
    }

    // Ambiguous: a read or a write may have been denied. Suggest the safer `Ro`;
    // the human can choose read+write at the prompt. (Linux Landlock surfaces as
    // `EACCES` ⇒ "Permission denied", caught here.)
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

/// Pull the first absolute path token out of a line, trimming surrounding
/// quotes/backticks and trailing punctuation. Recognises both Unix (`/`-rooted)
/// and Windows (drive-letter `C:\…` / `C:/…`, or UNC `\\server\share`) absolute
/// paths. Returns `None` when the line has no plausible absolute path (a bare
/// `/` does not count). Paths containing spaces cannot be recovered (the scan is
/// whitespace-tokenised) — an accepted heuristic limitation.
fn extract_abs_path(line: &str) -> Option<PathBuf> {
    for raw in line.split_whitespace() {
        // Trim the message's own grammar from both ends in one pass: surrounding
        // quotes/brackets and trailing punctuation can be interleaved (e.g. the
        // token `` `/path`: `` ends with a backtick *then* a colon). A Windows
        // drive colon (`C:`) is internal, so end-trimming `:` never harms it.
        let token = raw.trim_matches(|c| {
            matches!(
                c,
                '`' | '\'' | '"' | '(' | ')' | '[' | ']' | '<' | '>' | ':' | ',' | '.' | ';'
            )
        });
        if (token.len() > 1 && token.starts_with('/')) || is_windows_abs(token) {
            return Some(PathBuf::from(token));
        }
    }
    None
}

/// True for a Windows absolute path: a drive-letter root (`X:\…` or `X:/…`) or a
/// UNC path (`\\server\share`).
fn is_windows_abs(token: &str) -> bool {
    let b = token.as_bytes();
    let drive_rooted = b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    drive_rooted || token.starts_with("\\\\")
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

    // ── Windows / cross-platform denial signatures (Phase 3) ──────────────────

    #[test]
    fn windows_access_denied_with_drive_path_is_ro() {
        let stderr =
            r"error writing `C:\Users\me\.cache\sccache\0`: Access is denied. (os error 5)";
        let hit = scan_denial(stderr).expect("windows access-denied line matches");
        assert_eq!(hit.access, ScopeAccess::Ro, "ambiguous denial suggests Ro");
        assert_eq!(hit.path, PathBuf::from(r"C:\Users\me\.cache\sccache\0"));
        assert_eq!(hit.pattern, "windows access-denied");
    }

    #[test]
    fn windows_os_error_5_alone_matches() {
        let stderr = r"failed to create C:\build\target\x.rlib (os error 5)";
        let hit = scan_denial(stderr).expect("os error 5 with a path matches");
        assert_eq!(hit.path, PathBuf::from(r"C:\build\target\x.rlib"));
    }

    #[test]
    fn windows_forward_slash_drive_path_matches() {
        let stderr = "open C:/Users/me/cache: Access is denied";
        let hit = scan_denial(stderr).unwrap();
        assert_eq!(hit.path, PathBuf::from("C:/Users/me/cache"));
    }

    #[test]
    fn windows_unc_path_matches() {
        let stderr = r"write \\server\share\cache: Access is denied. (os error 5)";
        let hit = scan_denial(stderr).unwrap();
        assert_eq!(hit.path, PathBuf::from(r"\\server\share\cache"));
    }

    #[test]
    fn windows_access_denied_without_path_yields_none() {
        assert!(scan_denial("Access is denied. (os error 5)").is_none());
    }

    #[test]
    fn bare_drive_root_is_not_a_path() {
        // `C:\` alone is a filesystem root, too broad to suggest.
        assert!(!is_windows_abs("C:"));
        assert!(is_windows_abs(r"C:\x"));
    }

    #[test]
    fn linux_landlock_eacces_is_caught_as_permission_denied() {
        // Landlock surfaces a blocked access as EACCES → "Permission denied".
        let stderr = "/opt/ext/cache/obj: Permission denied (os error 13)";
        let hit = scan_denial(stderr).expect("EACCES is caught");
        assert_eq!(hit.path, PathBuf::from("/opt/ext/cache/obj"));
        assert_eq!(hit.access, ScopeAccess::Ro);
    }

    #[test]
    fn unix_os_error_5_on_unix_path_is_not_an_access_denial() {
        // `os error 5` is EIO on Unix (an I/O error), not access-denied — a bare
        // `os error 5` on a Unix path must NOT raise a grant prompt.
        assert!(scan_denial("read /mnt/disk/file failed (os error 5)").is_none());
    }

    // ── Multi-line denial signatures (cargo / anyhow `Caused by:` form) ────────

    #[test]
    fn cargo_install_crates_toml_multiline_is_rw() {
        // `cargo install` / `cargo binstall` print the path and the EPERM on
        // separate lines. This is the exact failure that broke the grant loop.
        let stderr = "\
    Updating crates.io index
error: failed to open: /Users/me/.cargo/.crates.toml

Caused by:
  Operation not permitted (os error 1)";
        let hit = scan_denial(stderr).expect("multi-line cargo denial matches");
        assert_eq!(hit.path, PathBuf::from("/Users/me/.cargo/.crates.toml"));
        assert_eq!(hit.access, ScopeAccess::Rw);
        assert_eq!(hit.pattern, "multi-line denial");
    }

    #[test]
    fn cargo_install_bin_multiline_is_rw() {
        let stderr = "\
error: failed to write /Users/me/.cargo/bin/cargo-nextest

Caused by:
  Operation not permitted (os error 1)";
        let hit = scan_denial(stderr).expect("multi-line cargo bin denial matches");
        assert_eq!(
            hit.path,
            PathBuf::from("/Users/me/.cargo/bin/cargo-nextest")
        );
        assert_eq!(hit.access, ScopeAccess::Rw);
    }

    #[test]
    fn generic_caused_by_permission_denied_multiline_is_ro() {
        let stderr = "\
error: failed to create /opt/ext/cache/obj

Caused by:
  Permission denied (os error 13)";
        let hit = scan_denial(stderr).expect("multi-line permission-denied matches");
        assert_eq!(hit.path, PathBuf::from("/opt/ext/cache/obj"));
        assert_eq!(hit.access, ScopeAccess::Ro);
    }

    #[test]
    fn multiline_keyword_beyond_window_does_not_match() {
        // A path far above an unrelated denial line must NOT correlate: once the
        // path has aged past the lookback window it is forgotten.
        let stderr = "\
opening /home/user/data.txt
filler 1
filler 2
filler 3
filler 4
filler 5
filler 6
filler 7
some op: Permission denied";
        assert!(
            scan_denial(stderr).is_none(),
            "a denial more than the lookback window away from the path must not match"
        );
    }

    #[test]
    fn multiline_bare_os_error_5_does_not_correlate_on_unix_path() {
        // A Unix path followed by a bare `os error 5` (EIO) must not be a denial.
        let stderr = "\
error: failed to read /mnt/disk/file

Caused by:
  (os error 5)";
        assert!(scan_denial(stderr).is_none());
    }

    /// Regression: a tool that catches the errno itself and reports it in its own
    /// words must still be recognised.
    ///
    /// This is the verbatim message from `cargo deny`, whose advisory DB lives in
    /// `~/.cargo` — outside the workspace scope, so the sandbox correctly made it
    /// read-only. It says neither "Permission denied" nor "Read-only file system",
    /// so the scanner missed it entirely: the agent got a bare failure, no grant
    /// was offered, and it re-ran the command *outside* the sandbox instead.
    #[test]
    fn tool_reported_read_only_lock_is_a_denial() {
        let stderr = "error: failed to acquire advisory database lock: failed to obtain lock \
                      file '/Users/me/.cargo/advisory-dbs/db.lock': attempted to take an \
                      exclusive lock on a read-only path";
        let hit = scan_denial(stderr).expect("a tool-reported read-only lock is a write denial");
        assert_eq!(
            hit.path,
            PathBuf::from("/Users/me/.cargo/advisory-dbs/db.lock")
        );
        assert_eq!(
            hit.access,
            ScopeAccess::Rw,
            "taking an exclusive lock is a write"
        );
    }

    /// Regression: a denial must not vanish because the caller merged the streams.
    ///
    /// `cmd 2>&1 | tail` is what agents and shell pipelines write by habit — it
    /// empties stderr entirely. Scanning stderr alone therefore lost the denial
    /// exactly when a human or agent had been thorough enough to capture output.
    #[test]
    fn denial_on_stdout_is_found_when_stderr_was_merged_away() {
        let stdout = "error: failed to create directory `/opt/ext/cache`: Read-only file system";
        assert!(
            scan_denial("").is_none(),
            "precondition: stderr is empty (2>&1 merged it into stdout)"
        );

        let hit = scan_denial_streams("", stdout)
            .expect("the denial is on stdout because the caller merged the streams");
        assert_eq!(hit.path, PathBuf::from("/opt/ext/cache"));
        assert_eq!(hit.access, ScopeAccess::Rw);
    }

    /// stderr wins when both streams carry a signature: it is where the denial was
    /// actually written when the command did not merge them.
    #[test]
    fn stderr_takes_precedence_over_stdout() {
        let stderr = "error: failed to create directory `/opt/from-stderr`: Read-only file system";
        let stdout = "error: failed to create directory `/opt/from-stdout`: Read-only file system";
        let hit = scan_denial_streams(stderr, stdout).expect("stderr matches");
        assert_eq!(hit.path, PathBuf::from("/opt/from-stderr"));
    }
}
