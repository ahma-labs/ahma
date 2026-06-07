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
        let scopes = self.adapter.sandbox().scopes();
        let exceptions = if let Some(primary) = scopes.first() {
            crate::sandbox::load_exceptions(primary)
        } else {
            vec![]
        };
        let sources = collect_log_sources(&log_dir, &scopes, &exceptions)
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

        if !symlink_path.exists() {
            return Err(mcp_invalid_params(format!(
                "Log file '{file_name}' does not exist"
            )));
        }

        let meta = std::fs::symlink_metadata(&symlink_path)
            .map_err(|e| mcp_internal(format!("Failed to read symlink metadata: {e}")))?;

        if !meta.file_type().is_symlink() {
            return Err(mcp_invalid_params(format!(
                "Log file '{file_name}' is not a symbolic link"
            )));
        }

        let target = std::fs::read_link(&symlink_path)
            .map_err(|e| mcp_internal(format!("Failed to read symlink target: {e}")))?;

        let canonical_target = dunce::canonicalize(log_dir.join(&target))
            .map_err(|e| mcp_internal(format!("Failed to canonicalize target path: {e}")))?;

        let primary_root = self
            .adapter
            .sandbox()
            .scopes()
            .first()
            .cloned()
            .ok_or_else(|| mcp_internal("No sandbox scopes configured"))?;

        let exceptions_dir = primary_root.join(".ahma");
        if !exceptions_dir.exists() {
            std::fs::create_dir_all(&exceptions_dir)
                .map_err(|e| mcp_internal(format!("Failed to create .ahma directory: {e}")))?;
        }

        let exceptions_file = exceptions_dir.join("exceptions.json");
        let mut approved = vec![];

        if exceptions_file.exists()
            && let Ok(content) = std::fs::read_to_string(&exceptions_file)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(arr) = val.get("approved_log_symlinks").and_then(|v| v.as_array())
        {
            for item in arr {
                if let Some(t) = item.get("target_path").and_then(|v| v.as_str()) {
                    approved.push(t.to_string());
                }
            }
        }

        let target_str = canonical_target.to_string_lossy().to_string();
        if !approved.contains(&target_str) {
            approved.push(target_str);
        }

        let new_val = serde_json::json!({
            "approved_log_symlinks": approved.into_iter().map(|t| serde_json::json!({ "target_path": t })).collect::<Vec<_>>()
        });

        let new_content = serde_json::to_string_pretty(&new_val)
            .map_err(|e| mcp_internal(format!("Failed to serialize exceptions: {e}")))?;

        std::fs::write(&exceptions_file, new_content)
            .map_err(|e| mcp_internal(format!("Failed to write exceptions.json: {e}")))?;

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
        let file = require_safe_log_path(&args, &log_dir)?;

        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(200)
            .min(2000);
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);

        let content = read_log_window(&file, offset, limit, raw)
            .map_err(|e| mcp_internal(format!("Failed to read {}: {e}", file.display())))?;

        Ok(text_result(content))
    }

    /// `logs_search` — search a log file for lines matching a pattern.
    pub async fn handle_logs_search(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let log_dir = project_log_dir();
        let file = require_safe_log_path(&args, &log_dir)?;

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
            "description": "Name of the log file to read (relative to the project log directory, e.g. 'ahma_mcp.log' or 'ahma_mcp.log.2026-05-24'). Use logs_list to discover available files."
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

/// Returns the canonical project log directory (`<cwd>/logs`).
fn project_log_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("logs")
}

/// Validates and resolves a caller-supplied log file name into a safe absolute path.
///
/// Rejects absolute paths, path separators, and traversals that escape the log directory.
fn require_safe_log_path(args: &Map<String, Value>, log_dir: &Path) -> Result<PathBuf, McpError> {
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
    let canonical_dir = std::fs::canonicalize(log_dir).map_err(|_| {
        mcp_internal(format!(
            "Log directory '{}' does not exist",
            log_dir.display()
        ))
    })?;

    // For the candidate we canonicalize the parent (since the file may or may not exist).
    let canonical_candidate = if candidate.exists() {
        std::fs::canonicalize(&candidate).map_err(|e| mcp_internal(e.to_string()))?
    } else {
        // File doesn't exist — still validate the parent is safe.
        let parent = candidate
            .parent()
            .ok_or_else(|| mcp_internal("Could not determine parent directory"))?;
        let canonical_parent =
            std::fs::canonicalize(parent).map_err(|_| mcp_internal("Invalid path"))?;
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

/// Scans the log directory and returns metadata for each log file.
fn collect_log_sources(
    log_dir: &Path,
    scopes: &[PathBuf],
    exceptions: &[PathBuf],
) -> anyhow::Result<Vec<LogFileInfo>> {
    if !log_dir.exists() {
        return Ok(vec![]);
    }

    let mut sources: Vec<LogFileInfo> = std::fs::read_dir(log_dir)?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;

            let is_symlink = meta.file_type().is_symlink();
            let mut symlink_target = None;
            let mut is_approved = true;

            if is_symlink {
                if let Ok(target) = std::fs::read_link(&path) {
                    symlink_target = Some(target.display().to_string());
                    if let Ok(canonical_target) = dunce::canonicalize(log_dir.join(&target)) {
                        is_approved = crate::sandbox::is_target_allowed(
                            &canonical_target,
                            scopes,
                            exceptions,
                        );
                    } else {
                        is_approved = false;
                    }
                } else {
                    is_approved = false;
                }
            }

            // For size/modified use the real file metadata (follows symlink).
            let real_meta = std::fs::metadata(&path).ok()?;
            if !real_meta.is_file() {
                return None;
            }

            let modified = real_meta.modified().ok().and_then(|t| {
                let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                // Format as ISO-8601 UTC using chrono.
                let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)?;
                Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            });

            Some(LogFileInfo {
                name: path.file_name()?.to_string_lossy().to_string(),
                path: path.display().to_string(),
                size_bytes: real_meta.len(),
                modified,
                is_symlink,
                symlink_target,
                is_approved,
            })
        })
        .collect();

    // Sort: symlinks first, then by modified descending (most recent first).
    sources.sort_by(|a, b| {
        b.is_symlink
            .cmp(&a.is_symlink)
            .then_with(|| b.modified.cmp(&a.modified))
    });

    Ok(sources)
}

/// Reads `limit` lines starting at `offset` from the given file.
fn read_log_window(path: &Path, offset: usize, limit: usize, raw: bool) -> anyhow::Result<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader
        .lines()
        .skip(offset)
        .take(limit)
        .map(|l| l.unwrap_or_default())
        .collect();

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
fn search_log_file(
    path: &Path,
    pattern: &str,
    max_results: usize,
    raw: bool,
    case_sensitive: bool,
) -> anyhow::Result<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let needle = if case_sensitive {
        pattern.to_owned()
    } else {
        pattern.to_lowercase()
    };

    let mut results = Vec::with_capacity(max_results.min(100));
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.unwrap_or_default();
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

    #[test]
    fn test_collect_log_sources() {
        let dir = tempdir().unwrap();
        // empty dir
        let sources = collect_log_sources(dir.path(), &[], &[]).unwrap();
        assert!(sources.is_empty());

        // write some files
        let f1 = dir.path().join("a.log");
        let f2 = dir.path().join("b.log");
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(&f1, "hello").unwrap();
        std::fs::write(&f2, "world").unwrap();

        let sources = collect_log_sources(dir.path(), &[], &[]).unwrap();
        assert_eq!(sources.len(), 2);
        // Assert we have both filenames
        let names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"a.log".to_string()));
        assert!(names.contains(&"b.log".to_string()));
    }

    #[test]
    fn test_read_log_window() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.log");
        std::fs::write(&file, "line 1\npassword=secret123\nline 3").unwrap();

        // test normal read (redacted)
        let content = read_log_window(&file, 0, 10, false).unwrap();
        assert!(content.contains("line 1"));
        assert!(content.contains("password="));
        assert!(!content.contains("secret123")); // should be redacted
        assert!(content.contains("line 3"));

        // test raw read
        let content_raw = read_log_window(&file, 0, 10, true).unwrap();
        assert!(content_raw.contains("secret123")); // should not be redacted

        // test offset/limit
        let window = read_log_window(&file, 1, 1, true).unwrap();
        assert_eq!(window, "password=secret123");
    }

    #[test]
    fn test_search_log_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.log");
        std::fs::write(&file, "Apple\nbanana\npassword=secret").unwrap();

        // Case insensitive search
        let res = search_log_file(&file, "apple", 10, false, false).unwrap();
        assert!(res.contains("Apple"));

        // Case sensitive search
        let res_sens = search_log_file(&file, "apple", 10, false, true).unwrap();
        assert!(res_sens.contains("No lines matching"));

        // Limit results
        std::fs::write(&file, "apple\napple\napple").unwrap();
        let res_limit = search_log_file(&file, "apple", 2, false, false).unwrap();
        assert!(res_limit.contains("2 match(es)"));

        // Redaction
        std::fs::write(&file, "password=secret").unwrap();
        let res_redact = search_log_file(&file, "password", 10, false, false).unwrap();
        assert!(!res_redact.contains("secret"));

        let res_raw = search_log_file(&file, "password", 10, true, false).unwrap();
        assert!(res_raw.contains("secret"));
    }

    #[test]
    fn test_require_safe_log_path() {
        let dir = tempdir().unwrap();
        let log_dir = dir.path();
        let file_path = log_dir.join("test.log");
        std::fs::write(&file_path, "hello").unwrap();

        // Canonicalized directory
        let canonical_dir = std::fs::canonicalize(log_dir).unwrap();

        // 1. Safe path inside
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("test.log".to_string()));
        let res = require_safe_log_path(&args, &canonical_dir);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), std::fs::canonicalize(&file_path).unwrap());

        // 2. Absolute path rejection
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("/etc/passwd".to_string()));
        let res = require_safe_log_path(&args, &canonical_dir);
        assert!(res.is_err());

        // 3. Traversal rejection (starts with dot or contains slashes)
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("../passwd".to_string()));
        let res = require_safe_log_path(&args, &canonical_dir);
        assert!(res.is_err());

        // 4. Missing file returns error
        let mut args = Map::new();
        args.insert(
            "file".to_string(),
            Value::String("nonexistent.log".to_string()),
        );
        let res = require_safe_log_path(&args, &canonical_dir);
        assert!(res.is_err());
        assert!(res.unwrap_err().message.contains("does not exist"));
    }
}
