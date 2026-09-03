use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

/// Intercepts and redirects file deletion commands to the vault's trash directory.
pub struct RmInterceptor;

impl RmInterceptor {
    /// Checks if a command name looks like an `rm` deletion command.
    pub fn looks_like_rm_command(base_command: &str) -> bool {
        let token = base_command.split_whitespace().next().unwrap_or_default();
        token == "rm" || token.ends_with("/rm")
    }

    /// Extracts target paths for deletion from the arguments map.
    pub fn extract_rm_targets_from_arguments(
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> Vec<String> {
        let mut targets = Vec::new();
        for (key, value) in arguments {
            if Self::is_rm_meta_argument(key) {
                continue;
            }
            Self::extend_rm_targets(&mut targets, value);
        }
        targets
    }

    fn is_rm_meta_argument(key: &str) -> bool {
        matches!(
            key,
            "timeout_seconds" | "execution_mode" | "working_directory"
        )
    }

    fn extend_rm_targets(targets: &mut Vec<String>, value: &serde_json::Value) {
        match value {
            serde_json::Value::String(candidate) => {
                Self::maybe_push_rm_target(targets, candidate);
            }
            serde_json::Value::Array(values) => {
                for candidate in values.iter().filter_map(serde_json::Value::as_str) {
                    Self::maybe_push_rm_target(targets, candidate);
                }
            }
            _ => {}
        }
    }

    fn maybe_push_rm_target(targets: &mut Vec<String>, candidate: &str) {
        if !candidate.starts_with('-') {
            targets.push(candidate.to_string());
        }
    }

    /// Resolves target path relative to the working directory.
    pub fn resolve_delete_source_path(target: &str, working_directory: &str) -> PathBuf {
        let mut source = PathBuf::from(target);
        if source.is_relative() {
            source = PathBuf::from(working_directory).join(source);
        }
        source
    }

    /// Stages a single path into the trash directory, returning the original and staged paths.
    pub fn stage_single_path_into_trash(
        source: PathBuf,
        trash_dir: &Path,
    ) -> Result<Option<(String, String)>> {
        if !source.exists() {
            return Ok(None);
        }

        let filename = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("staged-item");
        let ts = Utc::now().format("%Y%m%dT%H%M%S%3fZ");
        let dest = trash_dir.join(format!("{ts}_{filename}"));

        if let Err(rename_err) = std::fs::rename(&source, &dest) {
            tracing::debug!(
                "Rename failed: {}. Falling back to copy-and-delete for '{}' to '{}'",
                rename_err,
                source.display(),
                dest.display()
            );
            if source.is_dir() {
                copy_dir_all(&source, &dest).with_context(|| {
                    format!(
                        "Failed to copy directory '{}' to '{}' during staging fallback",
                        source.display(),
                        dest.display()
                    )
                })?;
                std::fs::remove_dir_all(&source).with_context(|| {
                    format!(
                        "Failed to remove source directory '{}' after staging fallback copy",
                        source.display()
                    )
                })?;
            } else {
                std::fs::copy(&source, &dest).with_context(|| {
                    format!(
                        "Failed to copy file '{}' to '{}' during staging fallback",
                        source.display(),
                        dest.display()
                    )
                })?;
                std::fs::remove_file(&source).with_context(|| {
                    format!(
                        "Failed to remove source file '{}' after staging fallback copy",
                        source.display()
                    )
                })?;
            }
        }

        Ok(Some((
            source.to_string_lossy().to_string(),
            dest.to_string_lossy().to_string(),
        )))
    }

    /// Stages multiple paths into the vault's trash directory.
    pub fn stage_paths_into_vault_trash(
        trash_dir: &Path,
        working_directory: &str,
        targets: &[String],
    ) -> Result<Vec<(String, String)>> {
        std::fs::create_dir_all(trash_dir).with_context(|| {
            format!("Failed to create vault trash dir: {}", trash_dir.display())
        })?;

        let mut staged = Vec::new();
        for target in targets {
            let source = Self::resolve_delete_source_path(target, working_directory);
            if let Some(pair) = Self::stage_single_path_into_trash(source, trash_dir)? {
                staged.push(pair);
            }
        }

        Ok(staged)
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value, json};
    use tempfile::TempDir;

    // ── looks_like_rm_command ────────────────────────────────────────────────

    #[test]
    fn rm_command_plain() {
        assert!(RmInterceptor::looks_like_rm_command("rm"));
    }

    #[test]
    fn rm_command_with_path_prefix() {
        assert!(RmInterceptor::looks_like_rm_command("/bin/rm"));
    }

    #[test]
    fn rm_command_with_args_in_string() {
        // The function checks the first token only
        assert!(RmInterceptor::looks_like_rm_command("rm -rf /tmp"));
    }

    #[test]
    fn rm_command_not_matching() {
        assert!(!RmInterceptor::looks_like_rm_command("remove"));
        assert!(!RmInterceptor::looks_like_rm_command("mv"));
        assert!(!RmInterceptor::looks_like_rm_command("rmdir"));
        assert!(!RmInterceptor::looks_like_rm_command(""));
    }

    #[test]
    fn rm_command_path_ending_with_rm() {
        assert!(RmInterceptor::looks_like_rm_command("/usr/bin/rm"));
    }

    // ── extract_rm_targets_from_arguments ───────────────────────────────────

    fn args_from_json(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn extract_targets_string_values() {
        let args = args_from_json(json!({
            "file1": "/tmp/foo.txt",
            "file2": "/tmp/bar.txt"
        }));
        let mut targets = RmInterceptor::extract_rm_targets_from_arguments(&args);
        targets.sort();
        assert_eq!(targets, vec!["/tmp/bar.txt", "/tmp/foo.txt"]);
    }

    #[test]
    fn extract_targets_array_value() {
        let args = args_from_json(json!({
            "paths": ["/tmp/a.txt", "/tmp/b.txt"]
        }));
        let mut targets = RmInterceptor::extract_rm_targets_from_arguments(&args);
        targets.sort();
        assert_eq!(targets, vec!["/tmp/a.txt", "/tmp/b.txt"]);
    }

    #[test]
    fn extract_targets_skips_flag_arguments() {
        let args = args_from_json(json!({
            "flag": "-rf",
            "path": "/tmp/real.txt"
        }));
        let targets = RmInterceptor::extract_rm_targets_from_arguments(&args);
        assert_eq!(targets, vec!["/tmp/real.txt"]);
    }

    #[test]
    fn extract_targets_skips_meta_arguments() {
        let args = args_from_json(json!({
            "timeout_seconds": "30",
            "execution_mode": "async",
            "working_directory": "/work",
            "path": "/tmp/real.txt"
        }));
        let targets = RmInterceptor::extract_rm_targets_from_arguments(&args);
        assert_eq!(targets, vec!["/tmp/real.txt"]);
    }

    #[test]
    fn extract_targets_empty_args() {
        let args = Map::new();
        assert!(RmInterceptor::extract_rm_targets_from_arguments(&args).is_empty());
    }

    #[test]
    fn extract_targets_array_with_flags_filtered() {
        let args = args_from_json(json!({
            "paths": ["-f", "/tmp/real.txt", "--verbose"]
        }));
        let targets = RmInterceptor::extract_rm_targets_from_arguments(&args);
        assert_eq!(targets, vec!["/tmp/real.txt"]);
    }

    #[test]
    fn extract_targets_ignores_non_string_non_array_values() {
        let args = args_from_json(json!({
            "count": 42,
            "active": true,
            "path": "/tmp/real.txt"
        }));
        let targets = RmInterceptor::extract_rm_targets_from_arguments(&args);
        assert_eq!(targets, vec!["/tmp/real.txt"]);
    }

    // ── resolve_delete_source_path ───────────────────────────────────────────

    #[test]
    fn resolve_absolute_path_unchanged() {
        let result = RmInterceptor::resolve_delete_source_path("/abs/path/file.txt", "/work");
        assert_eq!(result, PathBuf::from("/abs/path/file.txt"));
    }

    #[test]
    fn resolve_relative_path_joined_with_working_dir() {
        let result = RmInterceptor::resolve_delete_source_path("file.txt", "/work");
        assert_eq!(result, PathBuf::from("/work/file.txt"));
    }

    #[test]
    fn resolve_relative_path_with_subdir() {
        let result = RmInterceptor::resolve_delete_source_path("subdir/file.txt", "/project");
        assert_eq!(result, PathBuf::from("/project/subdir/file.txt"));
    }

    // ── stage_single_path_into_trash ────────────────────────────────────────

    #[test]
    fn stage_single_nonexistent_path_returns_none() {
        let tmp = TempDir::new().unwrap();
        let trash_dir = tmp.path().join("trash");
        std::fs::create_dir_all(&trash_dir).unwrap();
        let missing = tmp.path().join("ghost.txt");

        let result = RmInterceptor::stage_single_path_into_trash(missing, &trash_dir).unwrap();
        assert!(result.is_none(), "missing file should return None");
    }

    #[test]
    fn stage_single_file_moves_to_trash() {
        let tmp = TempDir::new().unwrap();
        let trash_dir = tmp.path().join("trash");
        std::fs::create_dir_all(&trash_dir).unwrap();

        let source = tmp.path().join("myfile.txt");
        std::fs::write(&source, b"data").unwrap();

        let result = RmInterceptor::stage_single_path_into_trash(source.clone(), &trash_dir)
            .unwrap()
            .expect("existing file should stage successfully");

        assert_eq!(result.0, source.to_string_lossy());
        assert!(!source.exists(), "source must be gone after staging");
        assert!(
            PathBuf::from(&result.1).exists(),
            "staged destination must exist"
        );
        assert!(
            result.1.contains("myfile.txt"),
            "staged name preserves original filename"
        );
    }

    #[test]
    fn stage_single_directory_moves_to_trash() {
        let tmp = TempDir::new().unwrap();
        let trash_dir = tmp.path().join("trash");
        std::fs::create_dir_all(&trash_dir).unwrap();

        let source_dir = tmp.path().join("mydir");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("inner.txt"), b"inner").unwrap();

        let result = RmInterceptor::stage_single_path_into_trash(source_dir.clone(), &trash_dir)
            .unwrap()
            .expect("directory should stage successfully");

        assert!(!source_dir.exists(), "source dir must be gone");
        let staged = PathBuf::from(&result.1);
        assert!(staged.exists(), "staged directory must exist");
        assert!(staged.join("inner.txt").exists(), "inner file preserved");
    }

    // ── stage_paths_into_vault_trash ─────────────────────────────────────────

    #[test]
    fn stage_multiple_files() {
        let tmp = TempDir::new().unwrap();
        let trash_dir = tmp.path().join("trash");
        let work = tmp.path().to_string_lossy().to_string();

        // Create two files
        let f1 = tmp.path().join("alpha.txt");
        let f2 = tmp.path().join("beta.txt");
        std::fs::write(&f1, b"a").unwrap();
        std::fs::write(&f2, b"b").unwrap();

        let targets = vec![
            f1.to_string_lossy().to_string(),
            f2.to_string_lossy().to_string(),
        ];

        let staged =
            RmInterceptor::stage_paths_into_vault_trash(&trash_dir, &work, &targets).unwrap();

        assert_eq!(staged.len(), 2, "both files should be staged");
        assert!(!f1.exists());
        assert!(!f2.exists());
    }

    #[test]
    fn stage_paths_creates_trash_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let trash_dir = tmp.path().join("deep/nested/trash");
        let work = tmp.path().to_string_lossy().to_string();

        let f = tmp.path().join("file.txt");
        std::fs::write(&f, b"x").unwrap();

        let targets = vec![f.to_string_lossy().to_string()];
        let staged =
            RmInterceptor::stage_paths_into_vault_trash(&trash_dir, &work, &targets).unwrap();

        assert!(trash_dir.exists(), "trash dir must have been created");
        assert_eq!(staged.len(), 1);
    }

    #[test]
    fn stage_paths_skips_nonexistent_targets() {
        let tmp = TempDir::new().unwrap();
        let trash_dir = tmp.path().join("trash");
        let work = tmp.path().to_string_lossy().to_string();

        let targets = vec!["/this/does/not/exist.txt".to_string()];
        let staged =
            RmInterceptor::stage_paths_into_vault_trash(&trash_dir, &work, &targets).unwrap();

        assert!(
            staged.is_empty(),
            "nonexistent targets should be skipped silently"
        );
    }

    #[test]
    fn stage_paths_relative_target_resolved_from_working_dir() {
        let tmp = TempDir::new().unwrap();
        let trash_dir = tmp.path().join("trash");
        let work = tmp.path().to_string_lossy().to_string();

        let f = tmp.path().join("relative.txt");
        std::fs::write(&f, b"hello").unwrap();

        // Pass relative target name; should be joined with work dir
        let targets = vec!["relative.txt".to_string()];
        let staged =
            RmInterceptor::stage_paths_into_vault_trash(&trash_dir, &work, &targets).unwrap();

        assert_eq!(staged.len(), 1);
        assert!(!f.exists());
    }

    // ── copy_dir_all (indirect via cross-device stage simulation) ────────────

    #[test]
    fn copy_dir_all_copies_recursive_structure() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        std::fs::create_dir_all(src.join("subdir")).unwrap();
        std::fs::write(src.join("root.txt"), b"root").unwrap();
        std::fs::write(src.join("subdir/child.txt"), b"child").unwrap();

        copy_dir_all(&src, &dst).unwrap();

        assert!(dst.join("root.txt").exists());
        assert!(dst.join("subdir/child.txt").exists());
        // Source still exists (copy, not move)
        assert!(src.join("root.txt").exists());
    }
}
