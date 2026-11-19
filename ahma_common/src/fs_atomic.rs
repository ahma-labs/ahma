//! Replace a file's contents in one step: write a sibling, then rename it over
//! the file, so a reader sees the old contents or the new, never half.
//!
//! The staging sibling is unique to this write (process id and a counter):
//! two writers of one file must not share it. Several copies of this pattern
//! staged at one fixed name (`<file>.json.tmp`), so a second writer's rename
//! found the first had already moved the file away and failed, or moved a file
//! the other was still writing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `bytes` to `path` atomically. The directory must exist. When the
/// rename fails, the staging file is removed and the error returned.
pub fn write(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_staged(path, bytes.as_ref(), None)
}

/// [`write()`] for a file only its owner may read (a token, a launcher): the
/// staging file is created with `mode` (Unix permission bits; ignored
/// elsewhere), so the contents are never readable by anyone else even for a
/// moment, and it is synced to disk before it replaces the file.
pub fn write_with_mode(path: &Path, bytes: impl AsRef<[u8]>, mode: u32) -> std::io::Result<()> {
    write_staged(path, bytes.as_ref(), Some(mode))
}

/// [`write()`] from async code, on the blocking pool.
pub async fn write_async(path: &Path, bytes: Vec<u8>) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || write(&path, bytes))
        .await
        .map_err(std::io::Error::other)?
}

/// [`write_with_mode`] from async code, on the blocking pool.
pub async fn write_with_mode_async(path: &Path, bytes: Vec<u8>, mode: u32) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || write_with_mode(&path, bytes, mode))
        .await
        .map_err(std::io::Error::other)?
}

fn write_staged(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write as _;
    let staged = staging_path(path);
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&staged)?;
        file.write_all(bytes)?;
        if mode.is_some() {
            file.sync_all()?;
        }
        drop(file);
        std::fs::rename(&staged, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

/// A hidden sibling of `path` no other write uses.
fn staging_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_of_one_file_all_succeed_and_leave_one_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let failures: usize = std::thread::scope(|scope| {
            (0..16)
                .map(|n| {
                    let file = &file;
                    scope.spawn(move || {
                        (0..50)
                            .filter(|_| write(file, format!("{{\"writer\":{n}}}")).is_err())
                            .count()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|w| w.join().unwrap())
                .sum()
        });
        assert_eq!(failures, 0);
        let text = std::fs::read_to_string(&file).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("one whole write");
        assert!(parsed["writer"].is_number(), "{text}");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .filter(|n| n != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn an_owner_only_file_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        write_with_mode(&file, "secret", 0o600).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "secret");
    }

    #[test]
    fn a_failed_write_leaves_no_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        // The target is a directory, so the rename over it fails.
        let target = dir.path().join("taken");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside"), "x").unwrap();
        assert!(write(&target, "new").is_err());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["taken"], "{names:?}");
    }
}
