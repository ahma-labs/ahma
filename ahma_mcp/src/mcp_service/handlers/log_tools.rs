//! Handlers for the built-in `logs_list`, `logs_read`, and `logs_search` MCP tools.
//!
//! These tools give LLMs pull-on-demand access to Ahma's log files, with redaction
//! enabled by default so secrets are masked before returning output.
//!
//! ## Security model
//!
//! All requested paths are resolved relative to the project log directory (`./logs/`
//! within the sandbox scope). Absolute paths are rejected.  Traversals (`../`) are
//! normalised away by `canonicalize` and rejected if they escape the log directory.
//!
//! Raw (un-redacted) output requires the caller to pass `"raw": true` explicitly.

use super::common::{mcp_internal, mcp_invalid_params, text_result};
use crate::AhmaMcpService;
use crate::log_monitor::redact_sensitive_line;
use crate::utils::logging::project_log_dir;
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────────────────────
// Public entry points (called from AhmaMcpService)
// ─────────────────────────────────────────────────────────────────────────────

impl AhmaMcpService {
    /// `logs_list` — enumerate all log files in the project log directory.
    pub async fn handle_logs_list(
        &self,
        _args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let log_dir = project_log_dir();
        // Materialize the scopes into an owned Vec before the `.await` below —
        // `ScopesGuard` wraps a `std::sync::RwLockReadGuard`, which is not `Send`,
        // so it must not be held live across an await point.
        let scopes: Vec<PathBuf> = {
            let guard = self.adapter.sandbox().scopes();
            guard.to_vec()
        };
        let exceptions = if let Some(primary) = scopes.first() {
            crate::sandbox::load_exceptions(primary)
        } else {
            vec![]
        };
        let sources = collect_log_sources(&log_dir, &scopes, &exceptions)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;
        let json = serde_json::to_string_pretty(&sources).unwrap_or_else(|_| "[]".to_string());
        Ok(text_result(json))
    }

    /// `logs_approve` — approve a blocked out-of-scope log symlink target.
    pub async fn handle_logs_approve(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let file_name = args
            .get("file")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'file' parameter is required"))?;

        if file_name.contains('/') || file_name.contains('\\') || file_name.starts_with('.') {
            return Err(mcp_invalid_params(format!(
                "Invalid log file name '{file_name}': must be a plain filename, not a path"
            )));
        }

        let log_dir = project_log_dir();
        let symlink_path = log_dir.join(file_name);

        if !tokio::fs::try_exists(&symlink_path).await.unwrap_or(false) {
            return Err(mcp_invalid_params(format!(
                "Log file '{file_name}' does not exist"
            )));
        }

        let meta = tokio::fs::symlink_metadata(&symlink_path)
            .await
            .map_err(|e| mcp_internal(format!("Failed to read symlink metadata: {e}")))?;

        if !meta.file_type().is_symlink() {
            return Err(mcp_invalid_params(format!(
                "Log file '{file_name}' is not a symbolic link"
            )));
        }

        let target = tokio::fs::read_link(&symlink_path)
            .await
            .map_err(|e| mcp_internal(format!("Failed to read symlink target: {e}")))?;

        let canonical_target = canonicalize_dunce(log_dir.join(&target))
            .await
            .map_err(|e| mcp_internal(format!("Failed to canonicalize target path: {e}")))?;

        let primary_root = self
            .adapter
            .sandbox()
            .scopes()
            .first()
            .cloned()
            .ok_or_else(|| mcp_internal("No sandbox scopes configured"))?;

        // Persisted out-of-sandbox (~/.config/ahma/) so a sandboxed agent
        // cannot grant itself access by writing the file inside the workspace.
        crate::sandbox::add_log_exception(&primary_root, &canonical_target)
            .map_err(|e| mcp_internal(format!("Failed to write log exceptions: {e}")))?;

        Ok(text_result(format!(
            "Successfully approved symlink target: {}. Please restart the TUI/session to apply changes.",
            canonical_target.display()
        )))
    }

    /// `logs_read` — return lines from a log file with optional offset and limit.
    pub async fn handle_logs_read(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let log_dir = project_log_dir();
        let file = require_safe_log_path(&args, &log_dir).await?;

        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(200)
            .min(2000);
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);

        let content = read_log_window(&file, offset, limit, raw)
            .await
            .map_err(|e| mcp_internal(format!("Failed to read {}: {e}", file.display())))?;

        Ok(text_result(content))
    }

    /// `logs_search` — search a log file for lines matching a pattern.
    pub async fn handle_logs_search(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let log_dir = project_log_dir();
        let file = require_safe_log_path(&args, &log_dir).await?;

        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'pattern' is required"))?;
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(100)
            .min(1000);
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let results = search_log_file(&file, pattern, max_results, raw, case_sensitive)
            .await
            .map_err(|e| mcp_internal(format!("Search failed on {}: {e}", file.display())))?;

        Ok(text_result(results))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Schema generation (called from mod.rs)
// ─────────────────────────────────────────────────────────────────────────────

use crate::mcp_service::schema;
use serde_json::json;
use std::sync::Arc;

/// Input schema for `logs_list`.
pub fn logs_list_schema() -> Arc<Map<String, Value>> {
    // No required parameters — returns all sources in the log directory.
    schema::object_input_schema(Map::new(), &[])
}

/// Input schema for `logs_read`.
pub fn logs_read_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "file".to_string(),
        json!({
            "type": "string",
            "description": "Name of the log file to read (relative to the project log directory, e.g. 'ahma.log' or 'ahma.log.2026-05-24'). Use logs_list to discover available files."
        }),
    );
    props.insert(
        "offset".to_string(),
        json!({
            "type": "integer",
            "description": "Line offset to start reading from (0 = beginning, negative unsupported). Default: 0.",
            "default": 0
        }),
    );
    props.insert(
        "limit".to_string(),
        json!({
            "type": "integer",
            "description": "Maximum number of lines to return. Default: 200. Maximum: 2000.",
            "default": 200
        }),
    );
    props.insert(
        "raw".to_string(),
        json!({
            "type": "boolean",
            "description": "Return un-redacted output. Default: false (secrets are masked). Set to true only when debugging credential issues.",
            "default": false
        }),
    );
    schema::object_input_schema(props, &["file"])
}

/// Input schema for `logs_search`.
pub fn logs_search_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "file".to_string(),
        json!({
            "type": "string",
            "description": "Name of the log file to search (relative to the project log directory). Use logs_list to discover available files."
        }),
    );
    props.insert(
        "pattern".to_string(),
        json!({
            "type": "string",
            "description": "Substring or literal string to search for. Not a regex."
        }),
    );
    props.insert(
        "max_results".to_string(),
        json!({
            "type": "integer",
            "description": "Maximum number of matching lines to return. Default: 100. Maximum: 1000.",
            "default": 100
        }),
    );
    props.insert(
        "case_sensitive".to_string(),
        json!({
            "type": "boolean",
            "description": "Perform a case-sensitive search. Default: false.",
            "default": false
        }),
    );
    props.insert(
        "raw".to_string(),
        json!({
            "type": "boolean",
            "description": "Return un-redacted output. Default: false (secrets are masked).",
            "default": false
        }),
    );
    schema::object_input_schema(props, &["file", "pattern"])
}

/// Input schema for `logs_approve`.
pub fn logs_approve_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "file".to_string(),
        json!({
            "type": "string",
            "description": "Name of the log file symlink to approve (e.g. 'sys.log')."
        }),
    );
    schema::object_input_schema(props, &["file"])
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Validates and resolves a caller-supplied log file name into a safe absolute path.
///
/// Rejects absolute paths, path separators, and traversals that escape the log directory.
async fn require_safe_log_path(
    args: &Map<String, Value>,
    log_dir: &Path,
) -> Result<PathBuf, McpError> {
    let file_name = args
        .get("file")
        .and_then(Value::as_str)
        .ok_or_else(|| mcp_invalid_params("'file' parameter is required"))?;

    // Reject absolute paths or anything containing a path separator.
    if file_name.contains('/') || file_name.contains('\\') || file_name.starts_with('.') {
        return Err(mcp_invalid_params(format!(
            "Invalid log file name '{file_name}': must be a plain filename, not a path"
        )));
    }

    let candidate = log_dir.join(file_name);

    // Canonicalize both and ensure candidate is inside log_dir.
    // If the log_dir doesn't exist yet, return a descriptive error.
    let canonical_dir = tokio::fs::canonicalize(log_dir).await.map_err(|_| {
        mcp_internal(format!(
            "Log directory '{}' does not exist",
            log_dir.display()
        ))
    })?;

    // For the candidate we canonicalize the parent (since the file may or may not exist).
    let canonical_candidate = if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
        tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?
    } else {
        // File doesn't exist — still validate the parent is safe.
        let parent = candidate
            .parent()
            .ok_or_else(|| mcp_internal("Could not determine parent directory"))?;
        let canonical_parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|_| mcp_internal("Invalid path"))?;
        if !canonical_parent.starts_with(&canonical_dir) {
            return Err(mcp_invalid_params(format!(
                "Log file '{file_name}' is outside the log directory"
            )));
        }
        return Err(mcp_invalid_params(format!(
            "Log file '{file_name}' does not exist. Use logs_list to see available files."
        )));
    };

    if !canonical_candidate.starts_with(&canonical_dir) {
        return Err(mcp_invalid_params(format!(
            "Log file '{file_name}' is outside the log directory"
        )));
    }

    Ok(canonical_candidate)
}

/// A serializable summary of a single log file in the log directory.
#[derive(serde::Serialize, Clone, Debug)]
pub struct LogFileInfo {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified: Option<String>,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub is_approved: bool,
}

/// Canonicalizes `path` via `dunce::canonicalize` on a blocking task, since
/// `dunce` has no async API (mirrors `path_security::canonicalize_simplified`).
async fn canonicalize_dunce(path: PathBuf) -> std::io::Result<PathBuf> {
    tokio::task::spawn_blocking(move || dunce::canonicalize(path))
        .await
        .map_err(std::io::Error::other)?
}

/// Scans the log directory and returns metadata for each log file.
async fn collect_log_sources(
    log_dir: &Path,
    scopes: &[PathBuf],
    exceptions: &[PathBuf],
) -> anyhow::Result<Vec<LogFileInfo>> {
    if !tokio::fs::try_exists(log_dir).await.unwrap_or(false) {
        return Ok(vec![]);
    }

    // Canonical log dir used to recognise ahma's own rolling-log symlinks
    // (e.g. ahma.log → ahma.log.2026-06-15). A symlink whose resolved target
    // stays inside this directory is always safe regardless of sandbox scope —
    // it was created by ahma's own rotation code, not an external actor.
    let canonical_log_dir = canonicalize_dunce(log_dir.to_path_buf())
        .await
        .unwrap_or_else(|_| log_dir.to_path_buf());

    let mut sources: Vec<LogFileInfo> = Vec::new();
    let mut read_dir = tokio::fs::read_dir(log_dir).await?;
    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let path = entry.path();
        let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };

        let is_symlink = meta.file_type().is_symlink();
        let mut symlink_target = None;
        let mut is_approved = true;

        if is_symlink {
            if let Ok(target) = tokio::fs::read_link(&path).await {
                symlink_target = Some(target.display().to_string());
                if let Ok(canonical_target) = canonicalize_dunce(log_dir.join(&target)).await {
                    // Symlink pointing within the managed log directory is always
                    // approved — apply the sandbox check only for targets that escape it.
                    if canonical_target.starts_with(&canonical_log_dir) {
                        is_approved = true;
                    } else {
                        is_approved = crate::sandbox::is_target_allowed(
                            &canonical_target,
                            scopes,
                            exceptions,
                        );
                    }
                } else {
                    is_approved = false;
                }
            } else {
                is_approved = false;
            }
        }

        // For size/modified use the real file metadata (follows symlink).
        let Ok(real_meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        if !real_meta.is_file() {
            continue;
        }

        let modified = real_meta.modified().ok().and_then(|t| {
            let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            // Format as ISO-8601 UTC using chrono.
            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)?;
            Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        });

        let Some(name) = path.file_name() else {
            continue;
        };

        sources.push(LogFileInfo {
            name: name.to_string_lossy().to_string(),
            path: path.display().to_string(),
            size_bytes: real_meta.len(),
            modified,
            is_symlink,
            symlink_target,
            is_approved,
        });
    }

    // Sort: symlinks first, then by modified descending (most recent first).
    sources.sort_by(|a, b| {
        b.is_symlink
            .cmp(&a.is_symlink)
            .then_with(|| b.modified.cmp(&a.modified))
    });

    Ok(sources)
}

/// Reads `limit` lines starting at `offset` from the given file.
async fn read_log_window(
    path: &Path,
    offset: usize,
    limit: usize,
    raw: bool,
) -> anyhow::Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let file = tokio::fs::File::open(path).await?;
    let mut line_stream = BufReader::new(file).lines();

    // Mirrors the previous `.skip(offset).take(limit)` semantics: every yielded
    // line (including ones that failed to decode, which become an empty string,
    // matching the prior `.unwrap_or_default()`) counts toward the offset.
    let mut lines: Vec<String> = Vec::new();
    let mut index = 0usize;
    while lines.len() < limit {
        match line_stream.next_line().await {
            Ok(Some(line)) => {
                if index >= offset {
                    lines.push(line);
                }
                index += 1;
            }
            Ok(None) => break,
            Err(_) => {
                if index >= offset {
                    lines.push(String::new());
                }
                index += 1;
            }
        }
    }

    let output = if raw {
        lines.join("\n")
    } else {
        lines
            .iter()
            .map(|l| redact_sensitive_line(l))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(output)
}

/// Searches `path` for lines containing `pattern`, returning up to `max_results` matches.
async fn search_log_file(
    path: &Path,
    pattern: &str,
    max_results: usize,
    raw: bool,
    case_sensitive: bool,
) -> anyhow::Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let file = tokio::fs::File::open(path).await?;
    let mut line_stream = BufReader::new(file).lines();

    let needle = if case_sensitive {
        pattern.to_owned()
    } else {
        pattern.to_lowercase()
    };

    let mut results = Vec::with_capacity(max_results.min(100));
    let mut line_no = 0usize;
    loop {
        let line = match line_stream.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(_) => String::new(),
        };
        let haystack = if case_sensitive {
            line.clone()
        } else {
            line.to_lowercase()
        };
        if haystack.contains(&needle) {
            let display = if raw {
                line.clone()
            } else {
                redact_sensitive_line(&line)
            };
            results.push(format!("{}: {display}", line_no + 1));
            if results.len() >= max_results {
                break;
            }
        }
        line_no += 1;
    }

    if results.is_empty() {
        return Ok(format!(
            "No lines matching '{pattern}' found in {}",
            path.display()
        ));
    }

    Ok(format!(
        "{} match(es) for '{pattern}' in {}:\n{}",
        results.len(),
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        results.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_collect_log_sources() {
        let dir = tempdir().unwrap();
        // empty dir
        let sources = collect_log_sources(dir.path(), &[], &[]).await.unwrap();
        assert!(sources.is_empty());

        // write some files
        let f1 = dir.path().join("a.log");
        let f2 = dir.path().join("b.log");
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(&f1, "hello").unwrap();
        std::fs::write(&f2, "world").unwrap();

        let sources = collect_log_sources(dir.path(), &[], &[]).await.unwrap();
        assert_eq!(sources.len(), 2);
        // Assert we have both filenames
        let names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"a.log".to_string()));
        assert!(names.contains(&"b.log".to_string()));
    }

    /// Ahma's own rolling-log symlink (e.g. `ahma.log → ahma.log.2026-06-15`) must
    /// be approved even when no sandbox scopes are configured.  Its target stays
    /// inside the same managed log directory, so it cannot be an exfil vector.
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_symlink_within_log_dir_is_always_approved() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let dated = dir.path().join("ahma.log.2026-06-15");
        std::fs::write(&dated, "log content").unwrap();
        // Create ahma.log → ahma.log.2026-06-15 (relative, as the real code does)
        symlink("ahma.log.2026-06-15", dir.path().join("ahma.log")).unwrap();

        // Empty scopes: without fix A this would set is_approved=false.
        let sources = collect_log_sources(dir.path(), &[], &[]).await.unwrap();
        let symlink_entry = sources.iter().find(|s| s.name == "ahma.log").unwrap();
        assert!(
            symlink_entry.is_symlink,
            "ahma.log should be detected as a symlink"
        );
        assert!(
            symlink_entry.is_approved,
            "symlink pointing within the log dir must be approved without scope check"
        );
    }

    /// A symlink in the log directory whose target escapes to an external path must
    /// still require scope/exception approval (the security check is preserved).
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escaping_log_dir_requires_scope_approval() {
        use std::os::unix::fs::symlink;
        let log_dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        let external_file = external.path().join("sensitive.log");
        std::fs::write(&external_file, "sensitive").unwrap();

        // Use an absolute target so canonicalization works on all platforms.
        symlink(&external_file, log_dir.path().join("escape.log")).unwrap();

        // No scopes, no exceptions → must be blocked.
        let sources = collect_log_sources(log_dir.path(), &[], &[]).await.unwrap();
        let entry = sources.iter().find(|s| s.name == "escape.log");
        if let Some(entry) = entry {
            assert!(
                !entry.is_approved,
                "symlink escaping log dir must not be auto-approved"
            );
        }
        // If the symlink target didn't canonicalize the entry may be absent —
        // that is also a safe outcome.
    }

    #[tokio::test]
    async fn test_read_log_window() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.log");
        std::fs::write(&file, "line 1\npassword=secret123\nline 3").unwrap();

        // test normal read (redacted)
        let content = read_log_window(&file, 0, 10, false).await.unwrap();
        assert!(content.contains("line 1"));
        assert!(content.contains("password="));
        assert!(!content.contains("secret123")); // should be redacted
        assert!(content.contains("line 3"));

        // test raw read
        let content_raw = read_log_window(&file, 0, 10, true).await.unwrap();
        assert!(content_raw.contains("secret123")); // should not be redacted

        // test offset/limit
        let window = read_log_window(&file, 1, 1, true).await.unwrap();
        assert_eq!(window, "password=secret123");
    }

    #[tokio::test]
    async fn test_search_log_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.log");
        std::fs::write(&file, "Apple\nbanana\npassword=secret").unwrap();

        // Case insensitive search
        let res = search_log_file(&file, "apple", 10, false, false)
            .await
            .unwrap();
        assert!(res.contains("Apple"));

        // Case sensitive search
        let res_sens = search_log_file(&file, "apple", 10, false, true)
            .await
            .unwrap();
        assert!(res_sens.contains("No lines matching"));

        // Limit results
        std::fs::write(&file, "apple\napple\napple").unwrap();
        let res_limit = search_log_file(&file, "apple", 2, false, false)
            .await
            .unwrap();
        assert!(res_limit.contains("2 match(es)"));

        // Redaction
        std::fs::write(&file, "password=secret").unwrap();
        let res_redact = search_log_file(&file, "password", 10, false, false)
            .await
            .unwrap();
        assert!(!res_redact.contains("secret"));

        let res_raw = search_log_file(&file, "password", 10, true, false)
            .await
            .unwrap();
        assert!(res_raw.contains("secret"));
    }

    #[tokio::test]
    async fn test_require_safe_log_path() {
        let dir = tempdir().unwrap();
        let log_dir = dir.path();
        let file_path = log_dir.join("test.log");
        std::fs::write(&file_path, "hello").unwrap();

        // Canonicalized directory
        let canonical_dir = std::fs::canonicalize(log_dir).unwrap();

        // 1. Safe path inside
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("test.log".to_string()));
        let res = require_safe_log_path(&args, &canonical_dir).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), std::fs::canonicalize(&file_path).unwrap());

        // 2. Absolute path rejection
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("/etc/passwd".to_string()));
        let res = require_safe_log_path(&args, &canonical_dir).await;
        assert!(res.is_err());

        // 3. Traversal rejection (starts with dot or contains slashes)
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("../passwd".to_string()));
        let res = require_safe_log_path(&args, &canonical_dir).await;
        assert!(res.is_err());

        // 4. Missing file returns error
        let mut args = Map::new();
        args.insert(
            "file".to_string(),
            Value::String("nonexistent.log".to_string()),
        );
        let res = require_safe_log_path(&args, &canonical_dir).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().message.contains("does not exist"));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test helpers (added)
    // ─────────────────────────────────────────────────────────────────────

    use crate::test_utils::in_process::build_test_service;

    /// Restores an env var to its previous value on drop (panic-safe). Env
    /// mutation is process-global; under `cargo nextest` each test runs in its
    /// own process so this is isolated. Under plain `cargo test` parallel tests
    /// touching the same key could race — nextest is the project's standard runner.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, val: &Path) -> Self {
            let prev = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, val);
            }
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Schema functions
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn test_logs_list_schema() {
        let schema = logs_list_schema();
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("object"),
            "list schema must be an object schema"
        );
        // No properties and no required keys.
        let props = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties must be present");
        assert!(props.is_empty(), "logs_list takes no parameters");
        assert!(
            !schema.contains_key("required"),
            "logs_list has no required keys; the 'required' key must be omitted"
        );
    }

    #[test]
    fn test_logs_read_schema() {
        let schema = logs_read_schema();
        assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
        let props = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        for key in ["file", "offset", "limit", "raw"] {
            assert!(props.contains_key(key), "missing property '{key}'");
        }
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["file"]);
    }

    #[test]
    fn test_logs_search_schema() {
        let schema = logs_search_schema();
        assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
        let props = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        for key in ["file", "pattern", "max_results", "case_sensitive", "raw"] {
            assert!(props.contains_key(key), "missing property '{key}'");
        }
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["file", "pattern"]);
    }

    #[test]
    fn test_logs_approve_schema() {
        let schema = logs_approve_schema();
        assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
        let props = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(props.contains_key("file"));
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["file"]);
    }

    // ─────────────────────────────────────────────────────────────────────
    // require_safe_log_path — additional branches
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn require_safe_log_path_missing_arg_errors() {
        let dir = tempdir().unwrap();
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let args = Map::new(); // no "file" key
        let err = require_safe_log_path(&args, &canonical_dir)
            .await
            .unwrap_err();
        assert!(err.message.contains("'file' parameter is required"));
    }

    #[tokio::test]
    async fn require_safe_log_path_nonexistent_log_dir_errors() {
        // A valid plain filename but the log directory itself does not exist.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist-subdir");
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("ahma.log".to_string()));
        let err = require_safe_log_path(&args, &missing).await.unwrap_err();
        assert!(
            err.message.contains("does not exist"),
            "expected missing-log-dir error, got: {}",
            err.message
        );
    }

    /// A symlink that exists inside the log dir but resolves to an external path
    /// must be rejected as "outside the log directory" (covers the existing-file
    /// canonicalisation escape branch).
    #[cfg(unix)]
    #[tokio::test]
    async fn require_safe_log_path_existing_symlink_escaping_dir_rejected() {
        use std::os::unix::fs::symlink;
        let log_dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        let external_file = external.path().join("outside.log");
        std::fs::write(&external_file, "secret").unwrap();
        symlink(&external_file, log_dir.path().join("escape.log")).unwrap();

        let canonical_dir = std::fs::canonicalize(log_dir.path()).unwrap();
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("escape.log".to_string()));
        let err = require_safe_log_path(&args, &canonical_dir)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("outside the log directory"),
            "expected outside-dir rejection, got: {}",
            err.message
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // read_log_window / search_log_file — additional edge cases
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_log_window_offset_beyond_eof_and_zero_limit() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("edge.log");
        std::fs::write(&file, "a\nb\nc").unwrap();

        // Offset past the end → empty string.
        let beyond = read_log_window(&file, 100, 10, true).await.unwrap();
        assert_eq!(beyond, "");

        // Zero limit → no lines.
        let none = read_log_window(&file, 0, 0, false).await.unwrap();
        assert_eq!(none, "");
    }

    #[tokio::test]
    async fn search_log_file_no_match_and_special_chars() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("special.log");
        std::fs::write(&file, "value = (a+b)*c\nplain line").unwrap();

        // Absent pattern → "No lines matching".
        let miss = search_log_file(&file, "zzz-not-here", 10, true, false)
            .await
            .unwrap();
        assert!(miss.contains("No lines matching"));

        // Regex-special chars are treated literally (substring match).
        let hit = search_log_file(&file, "(a+b)*c", 10, true, false)
            .await
            .unwrap();
        assert!(hit.contains("1 match(es)"));
        assert!(hit.contains("(a+b)*c"));
    }

    // ─────────────────────────────────────────────────────────────────────
    // collect_log_sources — additional branches
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn collect_log_sources_missing_dir_returns_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        let sources = collect_log_sources(&missing, &[], &[]).await.unwrap();
        assert!(sources.is_empty());
    }

    /// A dangling symlink (target does not resolve) is reported but not approved.
    #[cfg(unix)]
    #[tokio::test]
    async fn collect_log_sources_dangling_symlink_not_approved() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        // Also include a real file so the dir is non-trivial.
        std::fs::write(dir.path().join("real.log"), "x").unwrap();
        symlink(
            dir.path().join("missing-target.log"),
            dir.path().join("dangling.log"),
        )
        .unwrap();

        let sources = collect_log_sources(dir.path(), &[], &[]).await.unwrap();
        // The dangling symlink's real metadata cannot be read, so the entry is
        // dropped entirely (std::fs::metadata follows the broken link and fails).
        // The real file must still be present and approved.
        let real = sources.iter().find(|s| s.name == "real.log").unwrap();
        assert!(real.is_approved);
        assert!(!real.is_symlink);
        assert!(
            sources.iter().all(|s| s.name != "dangling.log"),
            "broken symlink should not surface as a readable source"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Async handlers
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn handle_logs_list_read_search_end_to_end() {
        let (service, _scope) = build_test_service().await.unwrap();
        let log_dir = tempdir().unwrap();
        std::fs::write(log_dir.path().join("alpha.log"), "first line\nsecond line").unwrap();
        std::fs::write(log_dir.path().join("beta.log"), "needle here\nother").unwrap();
        let _guard = EnvVarGuard::set("AHMA_LOG_DIR", log_dir.path());

        // list
        let list = service.handle_logs_list(Map::new()).await.unwrap();
        let list_text = text_of(&list);
        assert!(list_text.contains("alpha.log"), "list: {list_text}");
        assert!(list_text.contains("beta.log"), "list: {list_text}");

        // read
        let mut read_args = Map::new();
        read_args.insert("file".to_string(), Value::String("alpha.log".to_string()));
        read_args.insert("limit".to_string(), Value::from(1u64));
        read_args.insert("raw".to_string(), Value::Bool(true));
        let read = service.handle_logs_read(read_args).await.unwrap();
        assert_eq!(text_of(&read), "first line");

        // search — hit
        let mut search_args = Map::new();
        search_args.insert("file".to_string(), Value::String("beta.log".to_string()));
        search_args.insert("pattern".to_string(), Value::String("needle".to_string()));
        let search = service.handle_logs_search(search_args).await.unwrap();
        let search_text = text_of(&search);
        assert!(search_text.contains("1 match(es)"), "search: {search_text}");
        assert!(search_text.contains("needle here"));

        // search — miss
        let mut miss_args = Map::new();
        miss_args.insert("file".to_string(), Value::String("beta.log".to_string()));
        miss_args.insert(
            "pattern".to_string(),
            Value::String("absent-xyz".to_string()),
        );
        let miss = service.handle_logs_search(miss_args).await.unwrap();
        assert!(text_of(&miss).contains("No lines matching"));
    }

    #[tokio::test]
    async fn handle_logs_read_invalid_path_errors() {
        let (service, _scope) = build_test_service().await.unwrap();
        // A path separator fails validation before any filesystem access, so no
        // valid log dir is required — but set one anyway for determinism.
        let log_dir = tempdir().unwrap();
        let _guard = EnvVarGuard::set("AHMA_LOG_DIR", log_dir.path());

        let mut args = Map::new();
        args.insert(
            "file".to_string(),
            Value::String("../../etc/passwd".to_string()),
        );
        let err = service.handle_logs_read(args).await.unwrap_err();
        assert!(
            err.message.contains("must be a plain filename"),
            "expected path-rejection error, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn handle_logs_search_missing_pattern_errors() {
        let (service, _scope) = build_test_service().await.unwrap();
        let log_dir = tempdir().unwrap();
        std::fs::write(log_dir.path().join("x.log"), "content").unwrap();
        let _guard = EnvVarGuard::set("AHMA_LOG_DIR", log_dir.path());

        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("x.log".to_string()));
        // no "pattern"
        let err = service.handle_logs_search(args).await.unwrap_err();
        assert!(err.message.contains("'pattern' is required"));
    }

    #[tokio::test]
    async fn handle_logs_approve_invalid_inputs() {
        let (service, _scope) = build_test_service().await.unwrap();
        let log_dir = tempdir().unwrap();
        std::fs::write(log_dir.path().join("plain.log"), "not a symlink").unwrap();
        let _guard = EnvVarGuard::set("AHMA_LOG_DIR", log_dir.path());

        // Path-like name rejected before touching the filesystem.
        let mut bad = Map::new();
        bad.insert(
            "file".to_string(),
            Value::String("sub/evil.log".to_string()),
        );
        let err = service.handle_logs_approve(bad).await.unwrap_err();
        assert!(err.message.contains("must be a plain filename"));

        // Missing file.
        let mut missing = Map::new();
        missing.insert("file".to_string(), Value::String("ghost.log".to_string()));
        let err = service.handle_logs_approve(missing).await.unwrap_err();
        assert!(err.message.contains("does not exist"));

        // Regular file is not a symlink.
        let mut regular = Map::new();
        regular.insert("file".to_string(), Value::String("plain.log".to_string()));
        let err = service.handle_logs_approve(regular).await.unwrap_err();
        assert!(err.message.contains("not a symbolic link"));
    }

    /// Approving an out-of-scope symlink persists an exception and returns a
    /// success message. The exceptions file is redirected to a temp dir via
    /// `AHMA_CONFIG_DIR` so the real user config is never touched.
    #[cfg(unix)]
    #[tokio::test]
    async fn handle_logs_approve_success() {
        use std::os::unix::fs::symlink;
        let (service, _scope) = build_test_service().await.unwrap();
        let log_dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        let config = tempdir().unwrap();
        let external_file = external.path().join("sys.log");
        std::fs::write(&external_file, "external content").unwrap();
        symlink(&external_file, log_dir.path().join("sys.log")).unwrap();

        let _log_guard = EnvVarGuard::set("AHMA_LOG_DIR", log_dir.path());
        let _cfg_guard = EnvVarGuard::set("AHMA_CONFIG_DIR", config.path());

        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("sys.log".to_string()));
        let result = service.handle_logs_approve(args).await.unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("Successfully approved symlink target"),
            "approve: {text}"
        );

        // Exception file was written under the redirected config dir.
        let exceptions = config.path().join("ahma").join("log_exceptions.json");
        assert!(exceptions.exists(), "exceptions file must be persisted");
    }
}
