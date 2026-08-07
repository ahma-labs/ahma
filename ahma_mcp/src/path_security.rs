//! Path security validation for sandboxed operations.
//!
//! This module ensures that resolved file paths stay within a configured root
//! (sandbox scope). It canonicalizes paths to detect symlink escapes and
//! normalizes relative segments to prevent traversal outside the sandbox.
//!
//! ## Security
//! Callers must validate any user-provided paths before file access. This module
//! is a defense-in-depth layer in addition to kernel sandboxing.

use crate::sandbox::normalize_path_lexically;
use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

/// Canonicalize `path` using `dunce::canonicalize` to prevent symlink escapes.
///
/// Uses `dunce::canonicalize` which resolves symlinks and normalizes paths
/// while avoiding the Windows `\\?\` extended-length path prefix that
/// `std::fs::canonicalize` produces. This is critical for security as it
/// ensures symlinks are fully resolved before path validation.
///
/// Runs in a blocking task since `dunce::canonicalize` is synchronous.
async fn canonicalize_simplified(path: &Path) -> std::io::Result<PathBuf> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || dunce::canonicalize(&path))
        .await
        .map_err(std::io::Error::other)?
}

/// Validates that a path is within the specified root directory.
/// Resolves symlinks and relative paths.
pub async fn validate_path(path: &Path, root: &Path) -> Result<PathBuf> {
    let root_canonical = canonicalize_simplified(root)
        .await
        .map_err(|e| anyhow!("Failed to canonicalize root path {:?}: {}", root, e))?;

    // If path is absolute, check it directly. If relative, join with root.
    let path_to_check = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root_canonical.join(path)
    };

    // Try to canonicalize the full path to handle symlinks correctly.
    // If the file does not exist yet, canonicalize the parent directory (which should exist)
    // so symlink escapes are still detected for create/write operations.
    let resolved_path = match canonicalize_simplified(&path_to_check).await {
        Ok(p) => p,
        Err(_) => {
            if let Some(parent) = path_to_check.parent() {
                if let Ok(parent_canonical) = canonicalize_simplified(parent).await {
                    if let Some(name) = path_to_check.file_name() {
                        parent_canonical.join(name)
                    } else {
                        parent_canonical
                    }
                } else {
                    // If even the parent cannot be canonicalized, fall back to lexical normalization.
                    // This should be rare and is primarily for deeply-nested create flows.
                    normalize_path_lexically(&path_to_check)
                }
            } else {
                normalize_path_lexically(&path_to_check)
            }
        }
    };

    if resolved_path.starts_with(&root_canonical) {
        Ok(resolved_path)
    } else {
        Err(anyhow!(
            "Path {:?} is outside the sandbox root {:?}",
            path,
            root
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs;

    #[tokio::test]
    async fn test_validate_path_inside() -> Result<()> {
        let temp = TempDir::new()?;
        let root = temp.path();
        let file = root.join("foo.txt");
        fs::write(&file, "content").await?;

        let validated = validate_path(&file, root).await?;
        assert_eq!(validated, canonicalize_simplified(&file).await?);
        Ok(())
    }

    #[tokio::test]
    async fn test_validate_path_outside() -> Result<()> {
        let temp = TempDir::new()?;
        let root = temp.path();
        let outside = root.join("../outside.txt");

        assert!(validate_path(&outside, root).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_validate_path_relative_inside() -> Result<()> {
        let temp = TempDir::new()?;
        let root = temp.path();
        let subdir = root.join("subdir");
        fs::create_dir(&subdir).await?;
        let file = subdir.join("file.txt");
        fs::write(&file, "content").await?;

        // Relative path should be joined with root
        let relative = Path::new("subdir/file.txt");
        let validated = validate_path(relative, root).await?;
        assert_eq!(validated, canonicalize_simplified(&file).await?);
        Ok(())
    }

    #[tokio::test]
    async fn test_validate_path_nonexistent_file_in_existing_parent() -> Result<()> {
        let temp = TempDir::new()?;
        let root = temp.path();
        let subdir = root.join("subdir");
        fs::create_dir(&subdir).await?;

        // File doesn't exist but parent does - should still validate
        let new_file = subdir.join("newfile.txt");
        let validated = validate_path(&new_file, root).await?;
        assert!(validated.starts_with(canonicalize_simplified(root).await?));
        Ok(())
    }

    #[tokio::test]
    async fn test_validate_path_symlink_escape_blocked() -> Result<()> {
        let temp = TempDir::new()?;
        let _root = temp.path();
        #[cfg(unix)]
        let outside = temp.path().parent().unwrap();

        // Create a symlink inside root pointing outside
        #[cfg(unix)]
        let link_path = _root.join("escape_link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside, &link_path)?;

        #[cfg(unix)]
        {
            // Trying to access via symlink should fail
            let result = validate_path(&link_path.join("anything"), _root).await;
            assert!(result.is_err(), "Symlink escape should be blocked");
        }
        Ok(())
    }

    // normalize_path's own dot/dotdot behavior is covered by
    // sandbox::scopes's test suite for normalize_path_lexically, which this
    // module now calls directly rather than keeping a duplicate.
}
