//! Which files changed in a workspace while a command ran (root SPEC R2.7.6).
//!
//! The workspace write queue orders the writers ahma runs, but it cannot stop
//! a harness's own editor (Claude Code's `Edit`, say) — those never pass
//! through ahma. What it *can* do is notice. After a command finishes, ahma
//! lists the files whose modification time falls inside the command's run, so
//! a test result that raced an edit says so instead of passing silently on a
//! tree the model no longer has.
//!
//! This is optimistic concurrency control, not locking: it costs one
//! `.gitignore`-aware walk (no watcher, no daemon, nothing to clean up) and it
//! sees every writer, cooperating or not.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Files modified during a window, relative to the walked root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangedFiles {
    /// Up to `limit` paths, sorted.
    pub paths: Vec<PathBuf>,
    /// How many more matched beyond `limit`.
    pub more: usize,
    /// The walk stopped at `max_entries` before covering the whole tree.
    pub incomplete: bool,
}

impl ChangedFiles {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Slack after `until`: a write that lands as the process exits (a final
/// flush) is still the command's, and filesystem timestamps are not taken from
/// the same clock read as `SystemTime::now()`.
const UNTIL_SLACK: Duration = Duration::from_secs(1);

/// List files under `root` whose mtime lies in `[since, until + 1s]`.
///
/// `.gitignore`d paths (`target/`, `node_modules/`) and `.git/` are skipped —
/// they are build output and VCS state, not sources a result depends on — as is
/// anything under an `exclude` path (ahma's own log directory). At most
/// `max_entries` directory entries are visited, so a pathological tree costs a
/// bounded amount; `incomplete` says when that bound cut the walk short.
pub fn files_modified_between(
    root: &Path,
    since: SystemTime,
    until: SystemTime,
    exclude: &[PathBuf],
    limit: usize,
    max_entries: usize,
) -> ChangedFiles {
    let until = until + UNTIL_SLACK;
    let mut found = Vec::new();
    let mut visited = 0usize;
    let mut incomplete = false;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .parents(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker {
        visited += 1;
        if visited > max_entries {
            incomplete = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if exclude.iter().any(|x| path.starts_with(x)) {
            continue;
        }
        let Ok(modified) = entry
            .metadata()
            .and_then(|m| m.modified().map_err(Into::into))
        else {
            continue;
        };
        if modified >= since && modified <= until {
            found.push(path.strip_prefix(root).unwrap_or(path).to_path_buf());
        }
    }
    found.sort();
    let more = found.len().saturating_sub(limit);
    found.truncate(limit);
    ChangedFiles {
        paths: found,
        more,
        incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn touch_at(path: &Path, when: SystemTime) {
        fs::write(path, "x").unwrap();
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    #[test]
    fn lists_only_files_modified_inside_the_window() {
        let td = tempdir().unwrap();
        let root = td.path();
        let now = SystemTime::now();
        let hour = Duration::from_secs(3600);
        touch_at(&root.join("before.rs"), now - 2 * hour);
        touch_at(&root.join("during.rs"), now - hour / 2);
        touch_at(&root.join("after.rs"), now + hour);
        let got = files_modified_between(root, now - hour, now, &[], 10, 10_000);
        assert_eq!(got.paths, vec![PathBuf::from("during.rs")]);
        assert_eq!(got.more, 0);
        assert!(!got.incomplete);
    }

    #[test]
    fn respects_gitignore_git_dir_and_excludes() {
        let td = tempdir().unwrap();
        let root = td.path();
        let now = SystemTime::now();
        let since = now - Duration::from_secs(60);
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::File::options()
            .write(true)
            .open(root.join(".gitignore"))
            .unwrap()
            .set_modified(since - Duration::from_secs(60))
            .unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        touch_at(&root.join("target/out.o"), now);
        touch_at(&root.join(".git/index"), now);
        touch_at(&root.join("logs/op.log"), now);
        touch_at(&root.join("src/lib.rs"), now);
        let got = files_modified_between(root, since, now, &[root.join("logs")], 10, 10_000);
        assert_eq!(got.paths, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn caps_the_list_and_reports_the_rest() {
        let td = tempdir().unwrap();
        let now = SystemTime::now();
        for i in 0..5 {
            touch_at(&td.path().join(format!("f{i}")), now);
        }
        let got =
            files_modified_between(td.path(), now - Duration::from_secs(5), now, &[], 2, 10_000);
        assert_eq!(got.paths.len(), 2);
        assert_eq!(got.more, 3);
    }

    #[test]
    fn a_bounded_walk_says_it_was_cut_short() {
        let td = tempdir().unwrap();
        let now = SystemTime::now();
        for i in 0..20 {
            touch_at(&td.path().join(format!("f{i}")), now);
        }
        let got = files_modified_between(td.path(), now - Duration::from_secs(5), now, &[], 50, 5);
        assert!(got.incomplete);
    }
}
