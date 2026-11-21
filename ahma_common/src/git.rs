//! Git's on-disk pointer files, read the way git reads them.
//!
//! A linked worktree, a submodule and a `--separate-git-dir` checkout have a
//! `.git` *file*, not a directory: one line, `gitdir: <path>`, naming the git
//! directory git actually uses. ahma follows that pointer to decide which
//! repository a folder belongs to (`scope_grant`) and which hook and config
//! directories to defend (`ahma_mcp`'s `sandbox::exec_config`). A grant reads it
//! with git's grammar ([`parse_gitdir_pointer`]); a deny follows that reading and
//! the looser one ([`gitdir_pointer_deny_candidates`]), so it always covers at
//! least what a grant could name.

use std::path::{Path, PathBuf};

/// The git directory a `.git` pointer file names, given the file's contents
/// and the directory the file is in. `None` when git itself would not follow
/// the file.
///
/// The grammar is git's own (`read_gitfile_gently` in git's `setup.c`,
/// confirmed against git 2.54):
///
/// * the file must start with exactly `gitdir: ` — no leading whitespace, the
///   space after the colon required, lowercase; anything else is git's
///   "invalid gitfile format";
/// * trailing line endings (`\n`, `\r\n`) are dropped, and nothing else: any
///   other whitespace is part of the path, as it is to git;
/// * an empty path is git's "no path in gitfile";
/// * the pointer is one line. Git takes everything up to the trailing line
///   ending as the path, so a second line makes a path no repository has, and
///   git refuses it ("not a git repository"); `None` says so directly;
/// * a relative path is joined onto the pointer file's directory. It is **not**
///   normalized: git hands `<dir>/../x` to the filesystem, which resolves `..`
///   through any symlink in `<dir>`, so callers canonicalize the result rather
///   than collapse it lexically.
///
/// The file lies in the workspace, where anything can write it: what it names
/// is a claim, not evidence. Callers that *grant* on it must verify it (a
/// worktree-shaped path, a back-reference). Callers that *deny* use
/// [`gitdir_pointer_deny_candidates`], which follows more.
#[must_use]
pub fn parse_gitdir_pointer(contents: &str, git_file_dir: &Path) -> Option<PathBuf> {
    let target = contents
        .strip_prefix("gitdir: ")?
        .trim_end_matches(['\r', '\n']);
    if target.is_empty() || target.contains(['\r', '\n']) {
        return None;
    }
    Some(git_file_dir.join(target))
}

/// Every directory a `.git` pointer file might be read as, for a rule that
/// *denies* (SPEC R-HANDOFF.2.1): git's exact reading ([`parse_gitdir_pointer`])
/// first, then the looser one (any line, `gitdir:` with surrounding whitespace
/// trimmed) that other tools and older ahma accepted. Neither reading alone
/// covers the other: git keeps trailing spaces the loose one trims, and the
/// loose one follows spellings git refuses. A wrong extra deny is harmless; a
/// missed one would leave a hook directory writable.
#[must_use]
pub fn gitdir_pointer_deny_candidates(contents: &str, git_file_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = parse_gitdir_pointer(contents, git_file_dir)
        .into_iter()
        .collect();
    let loose = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(|target| git_file_dir.join(target));
    if let Some(loose) = loose
        && !out.contains(&loose)
    {
        out.push(loose);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shape a `.git` pointer file takes, and the directory git follows
    /// for it (checked by running `git rev-parse --absolute-git-dir` against
    /// each). `ahma_common::scope_grant` and `ahma_mcp::sandbox::exec_config`
    /// each had a parser of their own, and they disagreed on several of these
    /// rows; both now call this one.
    #[test]
    fn a_pointer_names_what_git_would_follow() {
        let base = std::env::temp_dir().join("work").join("repo");
        let abs = std::env::temp_dir().join("elsewhere").join("git-alt");
        let abs_s = abs.display().to_string();
        let with = |suffix: &str| PathBuf::from(format!("{abs_s}{suffix}"));
        let rows: Vec<(&str, String, Option<PathBuf>)> = vec![
            ("absolute", format!("gitdir: {abs_s}\n"), Some(abs.clone())),
            (
                "relative, joined onto the file's directory unnormalized",
                "gitdir: ../shared/.git\n".to_string(),
                Some(base.join("../shared/.git")),
            ),
            (
                "CRLF line ending",
                format!("gitdir: {abs_s}\r\n"),
                Some(abs.clone()),
            ),
            (
                "no line ending",
                format!("gitdir: {abs_s}"),
                Some(abs.clone()),
            ),
            ("leading spaces", format!("  gitdir: {abs_s}\n"), None),
            (
                "trailing spaces are part of the path",
                format!("gitdir: {abs_s}  \n"),
                Some(with("  ")),
            ),
            (
                "a second space after the colon is part of the path",
                format!("gitdir:  {abs_s}\n"),
                Some(base.join(format!(" {abs_s}"))),
            ),
            (
                "no space after the colon",
                format!("gitdir:{abs_s}\n"),
                None,
            ),
            ("empty pointer", "gitdir: \n".to_string(), None),
            ("bare key", "gitdir:".to_string(), None),
            ("missing gitdir: prefix", format!("{abs_s}\n"), None),
            ("uppercase key", format!("GITDIR: {abs_s}\n"), None),
            (
                "pointer on the second line",
                format!("# moved\ngitdir: {abs_s}\n"),
                None,
            ),
            (
                "a line after the pointer",
                format!("gitdir: {abs_s}\nextra\n"),
                None,
            ),
        ];
        let wrong: Vec<String> = rows
            .iter()
            .filter_map(|(case, contents, expected)| {
                let got = parse_gitdir_pointer(contents, &base);
                (&got != expected).then(|| format!("{case}: got {got:?}, want {expected:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// A rule that *denies* follows every directory a pointer could be read as
    /// (SPEC R-HANDOFF.2.1): git's exact reading, which can keep spaces the
    /// loose reading trims, and the loose one other tools accept. A wrong extra
    /// deny is harmless; a missed one leaves hooks writable.
    #[test]
    fn a_deny_follows_every_reading_of_a_pointer() {
        let base = std::env::temp_dir().join("work").join("repo");
        let named = |s: &str| base.join(s);
        for (contents, want) in [
            ("gitdir: git-alt\n", vec![named("git-alt")]),
            (
                "gitdir: git-alt  \n",
                vec![named("git-alt  "), named("git-alt")],
            ),
            ("  gitdir: git-alt\n", vec![named("git-alt")]),
            ("gitdir:git-alt\n", vec![named("git-alt")]),
            ("# moved\ngitdir: git-alt\n", vec![named("git-alt")]),
            ("gitdir: \n", vec![]),
            ("not a pointer\n", vec![]),
        ] {
            assert_eq!(
                gitdir_pointer_deny_candidates(contents, &base),
                want,
                "{contents:?}"
            );
        }
    }
}
