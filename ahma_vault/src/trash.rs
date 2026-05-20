//! Two-phase deletion for vault files.
//!
//! Ahma never permanently deletes files in a single step.  The [`TrashManager`]
//! enforces the **stage → confirm → purge** pattern:
//!
//! 1. **Stage** — `stage(path)` moves a file or directory into the vault's
//!    `trash/` subdirectory with a timestamp prefix.
//! 2. **List** — `list_staged()` shows what is waiting in trash.
//! 3. **Purge** — `purge()` permanently removes everything in `trash/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

// ─────────────────────────────────────────────────────────────────────────────
// StagedEntry
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata about a file that has been staged for deletion.
#[derive(Debug, Clone)]
pub struct StagedEntry {
    /// The path inside `trash/` where the staged data now lives.
    pub trash_path: PathBuf,
    /// The path before staging (stored as-is; may be relative or absolute).
    pub original_path: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// TrashManager
// ─────────────────────────────────────────────────────────────────────────────

/// Manages two-phase deletion for a single vault's `trash/` directory.
#[derive(Debug, Clone)]
pub struct TrashManager {
    trash_dir: PathBuf,
}

impl TrashManager {
    /// Create a manager for the given trash directory.
    pub fn new(trash_dir: impl Into<PathBuf>) -> Self {
        Self {
            trash_dir: trash_dir.into(),
        }
    }

    /// Stage `path` for deletion by moving it into the trash directory.
    pub fn stage(&self, path: &Path) -> Result<StagedEntry> {
        let original_path = path.display().to_string();

        let filename = path
            .file_name()
            .context("Cannot stage a path with no filename component")?
            .to_string_lossy();

        let ts = Utc::now().format("%Y%m%dT%H%M%S%3fZ");
        let trash_name = format!("{ts}_{filename}");

        std::fs::create_dir_all(&self.trash_dir)
            .with_context(|| format!("Failed to create trash dir: {}", self.trash_dir.display()))?;

        let trash_path = self.trash_dir.join(&trash_name);

        std::fs::rename(path, &trash_path)
            .with_context(|| format!("Failed to move {path:?} to trash as {trash_name}"))?;

        Ok(StagedEntry {
            trash_path,
            original_path,
        })
    }

    /// Return all entries currently staged in the trash directory.
    pub fn list_staged(&self) -> Result<Vec<StagedEntry>> {
        if !self.trash_dir.exists() {
            return Ok(vec![]);
        }

        let mut entries = vec![];
        for entry in std::fs::read_dir(&self.trash_dir)
            .with_context(|| format!("Failed to read trash dir: {}", self.trash_dir.display()))?
        {
            let entry = entry?;
            let trash_path = entry.path();
            let original_path = trash_path.display().to_string();
            entries.push(StagedEntry {
                trash_path,
                original_path,
            });
        }
        Ok(entries)
    }

    /// Return the number of entries currently staged in trash.
    pub fn staged_count(&self) -> Result<u64> {
        Ok(self.list_staged()?.len() as u64)
    }

    /// Permanently delete every entry in the trash directory.
    ///
    /// This is the **only irreversible step**. Only call after explicit confirmation.
    pub fn purge(&self) -> Result<u64> {
        if !self.trash_dir.exists() {
            return Ok(0);
        }

        let mut count = 0u64;
        for entry in std::fs::read_dir(&self.trash_dir).with_context(|| {
            format!(
                "Failed to read trash dir during purge: {}",
                self.trash_dir.display()
            )
        })? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("Failed to remove trash subdir: {path:?}"))?;
            } else {
                std::fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove trash file: {path:?}"))?;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Absolute path to the trash directory.
    pub fn path(&self) -> &Path {
        &self.trash_dir
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_manager(tmp: &TempDir) -> TrashManager {
        TrashManager::new(tmp.path().join("trash"))
    }

    #[test]
    fn stage_moves_file_to_trash() {
        let tmp = TempDir::new().unwrap();
        let manager = make_manager(&tmp);

        let source = tmp.path().join("my-file.txt");
        fs::write(&source, b"hello").unwrap();

        let entry = manager.stage(&source).unwrap();

        assert!(!source.exists());
        assert!(entry.trash_path.exists());
        assert!(entry.trash_path.to_string_lossy().contains("my-file.txt"));
    }

    #[test]
    fn staged_count_reflects_contents() {
        let tmp = TempDir::new().unwrap();
        let manager = make_manager(&tmp);

        for i in 0..3 {
            let f = tmp.path().join(format!("f{i}.txt"));
            fs::write(&f, b"x").unwrap();
            manager.stage(&f).unwrap();
        }

        assert_eq!(manager.staged_count().unwrap(), 3);
    }

    #[test]
    fn purge_removes_all_staged() {
        let tmp = TempDir::new().unwrap();
        let manager = make_manager(&tmp);

        let f = tmp.path().join("to-delete.txt");
        fs::write(&f, b"bye").unwrap();
        manager.stage(&f).unwrap();

        let purged = manager.purge().unwrap();
        assert_eq!(purged, 1);
        assert_eq!(manager.staged_count().unwrap(), 0);
    }

    #[test]
    fn purge_on_empty_trash_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let manager = make_manager(&tmp);
        assert_eq!(manager.purge().unwrap(), 0);
    }

    #[test]
    fn stage_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        let manager = make_manager(&tmp);
        let missing = tmp.path().join("not-here.txt");
        assert!(manager.stage(&missing).is_err());
    }
}
