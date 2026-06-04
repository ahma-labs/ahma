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

        std::fs::rename(&source, &dest).with_context(|| {
            format!(
                "Failed to stage '{}' into '{}'",
                source.display(),
                dest.display()
            )
        })?;

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
