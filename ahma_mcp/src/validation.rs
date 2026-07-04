//! Tool configuration validation module.
//!
//! Validates MTDF tool configuration files against the JSON schema.
//! Used by the `--validate` CLI flag to check tool configs before startup.

use crate::schema_validation::MtdfValidator;
use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::{error, info};

/// A single per-file validation failure, with the full human-readable report
/// so the CLI can surface it on stdout instead of only logging it.
pub struct FileFailure {
    /// The file (or target string) that failed.
    pub path: String,
    /// The detailed, human-readable reason (schema errors, read error, or
    /// "target not found").
    pub detail: String,
}

/// Result of validating one or more tool configuration files.
pub struct ValidationResult {
    /// Total number of files checked.
    pub files_checked: usize,
    /// Number of files that passed validation.
    pub files_passed: usize,
    /// Number of files that failed validation.
    pub files_failed: usize,
    /// Targets that could not be found (neither a file nor a directory).
    pub missing_targets: Vec<String>,
    /// Per-file failure details, ready to print.
    pub failures: Vec<FileFailure>,
    /// Whether all files passed validation.
    pub all_valid: bool,
}

/// Validates tool configuration files at the given target path.
///
/// The target can be:
/// - A directory (scans for `.json` files)
/// - A single file
/// - A comma-separated list of files and/or directories
///
/// Returns a [`ValidationResult`] summarizing the outcome, including the full
/// per-file failure reports so callers can display them.
pub fn run_validation(validation_target: &str) -> Result<ValidationResult> {
    let validator = MtdfValidator::new();
    let targets: Vec<String> = validation_target
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let (files, missing_targets) = collect_validation_files(targets)?;

    let mut passed = 0usize;
    let mut failures = Vec::new();

    for f in &files {
        match validate_file(&validator, f) {
            Ok(()) => passed += 1,
            Err(detail) => failures.push(FileFailure {
                path: f.display().to_string(),
                detail,
            }),
        }
    }

    for target in &missing_targets {
        failures.push(FileFailure {
            path: target.clone(),
            detail: format!(
                "target not found: no such file or directory '{target}'. \
                 Point the validate target at a directory containing *.json tool \
                 definitions, or at a single tool JSON file."
            ),
        });
    }

    let files_checked = files.len();
    let files_failed = failures.len();
    Ok(ValidationResult {
        files_checked,
        files_passed: passed,
        files_failed,
        all_valid: missing_targets.is_empty() && failures.is_empty(),
        missing_targets,
        failures,
    })
}

/// Returns true if `path` matches the legacy `.ahma/tools` directory pattern.
fn is_legacy_ahma_tools_path(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()) == Some("tools")
        && path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            == Some(".ahma")
}

/// Normalizes the validation target path for legacy compatibility.
///
/// If the path matches `.ahma/tools` but doesn't exist, falls back to the
/// parent `.ahma` directory when it exists.
fn normalize_validation_target(path: PathBuf) -> PathBuf {
    if !is_legacy_ahma_tools_path(&path) || path.exists() {
        return path;
    }
    match path.parent() {
        Some(parent) if parent.exists() => parent.to_path_buf(),
        _ => path,
    }
}

/// Resolves target strings into concrete file paths to validate.
///
/// Returns the collected files and the list of targets that could not be found.
fn collect_validation_files(targets: Vec<String>) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let mut files = Vec::new();
    let mut missing = Vec::new();

    for target in targets {
        let path = normalize_validation_target(PathBuf::from(target));
        if path.is_dir() {
            files.extend(get_json_files(&path)?);
        } else if path.is_file() {
            files.push(path);
        } else {
            error!("Validation target not found: {}", path.display());
            missing.push(path.display().to_string());
        }
    }

    Ok((files, missing))
}

/// Reads and validates a single tool configuration file.
///
/// Returns `Ok(())` on success, or `Err(detail)` with the full human-readable
/// failure report (schema errors or a read error) so the caller can print it.
fn validate_file(validator: &MtdfValidator, file_path: &Path) -> Result<(), String> {
    let content = match fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(e) => {
            error!("Failed to read file {}: {}", file_path.display(), e);
            return Err(format!("could not read '{}': {e}", file_path.display()));
        }
    };

    match validator.validate_tool_config(file_path, &content) {
        Ok(_) => {
            info!("{} is valid.", file_path.display());
            Ok(())
        }
        Err(errors) => {
            let report = validator.format_errors(&errors, file_path);
            error!(
                "Validation failed for {}: {:?}",
                file_path.display(),
                errors
            );
            Err(report)
        }
    }
}

/// Scans a directory for top-level `.json` files (non-recursive).
fn get_json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_temp_dir_with_files(files: &[(&str, &str)]) -> TempDir {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        for (name, content) in files {
            let file_path = temp_dir.path().join(name);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).expect("Failed to create parent dirs");
            }
            fs::write(&file_path, content).expect("Failed to write file");
        }
        temp_dir
    }

    // ==================== get_json_files tests ====================

    #[test]
    fn test_get_json_files_returns_only_json_files() {
        let temp_dir = setup_temp_dir_with_files(&[
            ("tool1.json", "{}"),
            ("tool2.json", "{}"),
            ("readme.txt", "text"),
            ("config.yaml", "yaml: true"),
        ]);

        let files = get_json_files(temp_dir.path()).expect("Should succeed");

        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|p| p.extension().unwrap() == "json"));
    }

    #[test]
    fn test_get_json_files_empty_directory() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");

        let files = get_json_files(temp_dir.path()).expect("Should succeed");

        assert!(files.is_empty());
    }

    #[test]
    fn test_get_json_files_no_json_files() {
        let temp_dir =
            setup_temp_dir_with_files(&[("readme.md", "# Readme"), ("config.toml", "[config]")]);

        let files = get_json_files(temp_dir.path()).expect("Should succeed");

        assert!(files.is_empty());
    }

    #[test]
    fn test_get_json_files_nonexistent_directory() {
        let result = get_json_files(Path::new("/nonexistent/path/12345"));

        assert!(result.is_err());
    }

    #[test]
    fn test_get_json_files_ignores_subdirectories() {
        let temp_dir =
            setup_temp_dir_with_files(&[("tool.json", "{}"), ("subdir/nested.json", "{}")]);

        let files = get_json_files(temp_dir.path()).expect("Should succeed");

        // Should only find top-level json files, not nested ones
        assert_eq!(files.len(), 1);
        assert!(files[0].file_name().unwrap() == "tool.json");
    }

    // ==================== run_validation tests ====================

    /// Creates a minimal valid MTDF tool configuration
    /// Required fields: name, description, command
    fn valid_tool_config() -> &'static str {
        r#"{
            "name": "test_tool",
            "description": "A test tool for validation",
            "command": "echo"
        }"#
    }

    #[test]
    fn test_run_validation_valid_single_file() {
        let temp_dir = setup_temp_dir_with_files(&[("tool.json", valid_tool_config())]);

        let target = temp_dir
            .path()
            .join("tool.json")
            .to_string_lossy()
            .to_string();

        let result = run_validation(&target).expect("Should succeed");

        assert!(result.all_valid);
        assert_eq!(result.files_checked, 1);
        assert_eq!(result.files_passed, 1);
        assert_eq!(result.files_failed, 0);
    }

    #[test]
    fn test_run_validation_valid_directory() {
        let temp_dir = setup_temp_dir_with_files(&[
            ("tools/tool1.json", valid_tool_config()),
            ("tools/tool2.json", valid_tool_config()),
        ]);

        let target = temp_dir.path().join("tools").to_string_lossy().to_string();

        let result = run_validation(&target).expect("Should succeed");

        assert!(result.all_valid);
        assert_eq!(result.files_checked, 2);
        assert_eq!(result.files_passed, 2);
    }

    #[test]
    fn test_run_validation_comma_separated_files() {
        let temp_dir = setup_temp_dir_with_files(&[
            ("tool1.json", valid_tool_config()),
            ("tool2.json", valid_tool_config()),
        ]);

        let file1 = temp_dir
            .path()
            .join("tool1.json")
            .to_string_lossy()
            .to_string();
        let file2 = temp_dir
            .path()
            .join("tool2.json")
            .to_string_lossy()
            .to_string();

        let target = format!("{},{}", file1, file2);

        let result = run_validation(&target).expect("Should succeed");

        assert!(result.all_valid);
        assert_eq!(result.files_checked, 2);
    }

    #[test]
    fn test_run_validation_nonexistent_target() {
        let result = run_validation("/nonexistent/path/12345").expect("Should succeed");

        assert!(!result.all_valid);
        // A missing target is reported as such, NOT as "1/0 files invalid":
        // files_checked stays 0 and the target lands in missing_targets.
        assert_eq!(result.files_checked, 0);
        assert_eq!(result.missing_targets.len(), 1);
        assert!(result.missing_targets[0].contains("12345"));
        // and it surfaces a readable, actionable failure detail.
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].detail.contains("target not found"));
    }

    #[test]
    fn test_run_validation_invalid_json_content() {
        let temp_dir = setup_temp_dir_with_files(&[("tool.json", "{ invalid json }")]);

        let target = temp_dir
            .path()
            .join("tool.json")
            .to_string_lossy()
            .to_string();

        let result = run_validation(&target).expect("Should succeed");

        assert!(!result.all_valid);
        assert_eq!(result.files_failed, 1);
        // The failure carries a detailed, human-readable report — not just a count.
        assert_eq!(result.failures.len(), 1);
        assert!(
            result.failures[0].detail.contains("Invalid JSON")
                || result.failures[0].detail.contains("Validation errors"),
            "detail should explain the parse failure: {}",
            result.failures[0].detail
        );
    }

    #[test]
    fn test_run_validation_empty_directory() {
        let temp_dir = setup_temp_dir_with_files(&[]);

        // Create an empty tools directory
        fs::create_dir(temp_dir.path().join("tools")).expect("Failed to create tools dir");

        let target = temp_dir.path().join("tools").to_string_lossy().to_string();

        let result = run_validation(&target).expect("Should succeed");

        assert!(result.all_valid); // Empty directory is valid (no files to fail)
        assert_eq!(result.files_checked, 0);
    }

    #[test]
    fn test_run_validation_mixed_valid_invalid() {
        let temp_dir = setup_temp_dir_with_files(&[
            ("tools/valid.json", valid_tool_config()),
            ("tools/invalid.json", "{ not json }"),
        ]);

        let target = temp_dir.path().join("tools").to_string_lossy().to_string();

        let result = run_validation(&target).expect("Should succeed");

        assert!(!result.all_valid);
        assert_eq!(result.files_checked, 2);
        assert_eq!(result.files_passed, 1);
        assert_eq!(result.files_failed, 1);
    }

    #[test]
    fn test_run_validation_missing_required_fields() {
        // Tool config missing required 'name' field
        let invalid_tool = r#"{
            "description": "Missing name field",
            "inputSchema": {
                "type": "object"
            }
        }"#;

        let temp_dir = setup_temp_dir_with_files(&[("tool.json", invalid_tool)]);

        let target = temp_dir
            .path()
            .join("tool.json")
            .to_string_lossy()
            .to_string();

        let result = run_validation(&target).expect("Should succeed");

        assert!(!result.all_valid);
        assert_eq!(result.files_failed, 1);
    }
}
