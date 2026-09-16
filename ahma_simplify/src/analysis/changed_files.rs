//! Resolves the set of files changed in git, for `ahma simplify --diff`.
//!
//! Parsing git's output is a pure function (`parse_name_list`) kept separate
//! from the subprocess invocation (`changed_files`) so it can be unit-tested
//! without a real git repository.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Returns the absolute paths of files changed in the git repository
/// containing `dir`: staged/unstaged modifications versus `HEAD`, plus
/// untracked (and not ignored) files.
///
/// Fails loudly, rather than falling back to a full scan, when `dir` is not
/// inside a git repository or `git` is not installed — `--diff` is the
/// reason git was needed, and a silent full scan would misrepresent what was
/// analyzed.
pub(crate) fn changed_files(dir: &Path) -> Result<HashSet<PathBuf>> {
    let repo_root = repo_root(dir)?;

    // -z: NUL-separated and never quoted. Without it git escapes non-ASCII paths
    // ("src/caf\303\251.rs"), which would not match anything on disk and would
    // silently drop those files from the scan.
    let modified = run_git(&repo_root, &["diff", "-z", "--name-only", "HEAD"])?;
    let untracked = run_git(
        &repo_root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?;

    let mut files = parse_name_list(&modified, &repo_root);
    files.extend(parse_name_list(&untracked, &repo_root));

    // Canonicalize so these compare equal to the walker's paths, which descend
    // from an already-canonicalized root. Entries that no longer exist are
    // deleted files, which drop out here because there is nothing to analyze.
    Ok(files
        .iter()
        .filter_map(|path| dunce::canonicalize(path).ok())
        .collect())
}

fn repo_root(dir: &Path) -> Result<PathBuf> {
    let output = spawn_git(dir, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        bail!(
            "'{}' is not inside a git repository; --diff requires git to determine which files changed",
            dir.display()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let top_level = stdout.trim();
    if top_level.is_empty() {
        bail!(
            "git reported no repository root for '{}'; --diff requires git to determine which files changed",
            dir.display()
        );
    }
    Ok(PathBuf::from(top_level))
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = spawn_git(repo_root, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed in '{}': {}",
            args.join(" "),
            repo_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn spawn_git(cwd: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("git is required for --diff but could not be run; is it installed and on PATH?")
}

/// Parses NUL-separated git output into absolute paths, joining each
/// repository-relative entry against `repo_root`. Pure and git-independent so
/// it can be tested without a repository.
fn parse_name_list(stdout: &str, repo_root: &Path) -> HashSet<PathBuf> {
    stdout
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| repo_root.join(entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expectations are built with the same `join` the parser uses, so the
    // assertions hold on Windows too, where the separator differs.
    fn root() -> PathBuf {
        PathBuf::from("repo")
    }

    #[test]
    fn parses_multiple_paths() {
        let repo_root = root();
        let result = parse_name_list("src/main.rs\0src/lib.rs\0", &repo_root);
        assert_eq!(
            result,
            HashSet::from([repo_root.join("src/main.rs"), repo_root.join("src/lib.rs")])
        );
    }

    #[test]
    fn skips_empty_entries() {
        let repo_root = root();
        let result = parse_name_list("src/main.rs\0\0\0src/lib.rs\0", &repo_root);
        assert_eq!(
            result,
            HashSet::from([repo_root.join("src/main.rs"), repo_root.join("src/lib.rs")])
        );
    }

    #[test]
    fn handles_nested_paths() {
        let repo_root = root();
        let result = parse_name_list("crate_a/src/deep/nested/mod.rs\0", &repo_root);
        assert_eq!(
            result,
            HashSet::from([repo_root.join("crate_a/src/deep/nested/mod.rs")])
        );
    }

    #[test]
    fn non_ascii_paths_survive_unescaped() {
        let repo_root = root();
        let result = parse_name_list("src/café.rs\0src/日本.rs\0", &repo_root);
        assert_eq!(
            result,
            HashSet::from([repo_root.join("src/café.rs"), repo_root.join("src/日本.rs")])
        );
    }

    #[test]
    fn a_path_containing_a_newline_is_one_entry() {
        let repo_root = root();
        let result = parse_name_list("src/we\nird.rs\0", &repo_root);
        assert_eq!(result, HashSet::from([repo_root.join("src/we\nird.rs")]));
    }

    #[test]
    fn empty_input_yields_empty_set() {
        assert_eq!(parse_name_list("", &root()), HashSet::new());
    }

    #[test]
    fn final_entry_without_trailing_nul_is_still_parsed() {
        let repo_root = root();
        let result = parse_name_list("src/main.rs", &repo_root);
        assert_eq!(result, HashSet::from([repo_root.join("src/main.rs")]));
    }
}
