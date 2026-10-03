//! Handlers for the built-in `logs_list`, `logs_read`, and `logs_search` MCP tools.
//!
//! These tools give LLMs pull-on-demand access to Ahma's log files, with redaction
//! enabled by default so secrets are masked before returning output.
//!
//! ## Security model
//!
//! All requested paths are resolved relative to the project log directory (`.ahma/logs/`
//! within the sandbox scope). Absolute paths are rejected.  Traversals (`../`) are
//! normalised away by `canonicalize` and rejected if they escape the log directory.
//!
//! Raw (un-redacted) output requires the caller to pass `"raw": true` explicitly.
//!
//! `logs_approve` never approves anything on the model's word (SPEC R9.2,
//! R5.4.5): the agent can plant a `.ahma/logs` link to any file, so the call
//! only *asks* a human, through the permission ladder (R-PERM.3), and only the
//! human's answer — applied by the broker or the TUI reporter — records it.

use super::common::{mcp_internal, mcp_invalid_params, text_result};
use crate::AhmaMcpService;
use crate::log_monitor::redact_sensitive_line;
use crate::utils::logging::project_log_dir;
use ahma_common::scope_grant::GrantStatus;
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
        // Approved targets come from the permission ledger, read fresh on every
        // call: an approval made earlier in this session shows as approved here
        // even though the sandbox's read scope only picks it up next session.
        let exceptions = match scopes.first() {
            Some(primary) => {
                let key = ahma_common::permissions::workspace_key_async(primary).await;
                ahma_common::config::AhmaSettings::load_async()
                    .await
                    .log_targets
                    .approved_targets(&key)
            }
            None => vec![],
        };
        let sources = collect_log_sources(&log_dir, &scopes, &exceptions)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;
        let json = serde_json::to_string_pretty(&sources).unwrap_or_else(|_| "[]".to_string());
        Ok(text_result(json))
    }

    /// `logs_approve` — ask a human to let the log tools read the file outside
    /// the workspace that a `.ahma/logs/*.log` symlink points at.
    ///
    /// The call is a request, never an approval, for every client: whether
    /// the caller is the TUI's agent, an IDE that gates tool calls or a
    /// headless harness that auto-approves them, the question is raised as a
    /// [`GrantReason::LogTarget`] grant question through the permission ladder
    /// and only a human's answer is applied (the broker or the hub reporter
    /// records an `always` answer as a `log-target` row and applies either
    /// answer to this session's read scope). This handler reports the answer.
    ///
    /// [`GrantReason::LogTarget`]: ahma_common::scope_grant::GrantReason::LogTarget
    pub async fn handle_logs_approve(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let file_name = require_plain_log_filename(&args)?;

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

        // The agent can write `.ahma/logs`, so it can plant a link to anything:
        // the hard denylist every filesystem grant obeys (R-PERM.4.3) applies.
        if let Some(why) = ahma_common::scope_grant::refusal_reason(&canonical_target) {
            return Err(mcp_invalid_params(format!(
                "logs_approve REFUSED for {}: {why}. No approval can make it a log target.",
                canonical_target.display()
            )));
        }

        // SPEC R-PERM.3.4: the human sees the agent's own stated reason,
        // labelled as its claim. A request without one is one nobody can judge.
        let reason = super::common::require_str(
            &args,
            "reason",
            "logs_approve requires a `reason`: one sentence saying why you need to read this log, \
             for the human who decides",
        )?;

        let sandbox = self.adapter.sandbox_arc();
        let primary_root = sandbox
            .scopes()
            .first()
            .cloned()
            .ok_or_else(|| mcp_internal("No sandbox scopes configured"))?;

        // Already approved for this workspace: nothing to ask.
        let key = ahma_common::permissions::workspace_key_async(&primary_root).await;
        if ahma_common::config::AhmaSettings::load_async()
            .await
            .log_targets
            .is_target_approved(&key, &canonical_target)
        {
            return Ok(text_result(already_approved_text(
                &canonical_target,
                &primary_root,
            )));
        }

        let Some(notifier) = self.adapter.scope_grant_notifier().cloned() else {
            return Ok(text_result(not_raised_text(
                &canonical_target,
                &primary_root,
                "Not raised: no surface can ask a human in this session. Nothing is recorded.",
            )));
        };
        // The risk section inspects the target (and the ledger): blocking I/O,
        // so not on this async task.
        let context = {
            let sandbox = sandbox.clone();
            let target = canonical_target.clone();
            tokio::task::spawn_blocking(move || {
                crate::sandbox::grant_channel::build_context(
                    &sandbox,
                    &target,
                    None,
                    None,
                    None,
                    Some(reason.as_str()),
                    false,
                )
            })
            .await
            .unwrap_or_default()
        };
        let raised = notifier
            .notify_violation_with(
                &canonical_target,
                ahma_common::config::ScopeAccess::Ro,
                ahma_common::scope_grant::GrantReason::LogTarget,
                Some("logs_approve".to_string()),
                context,
            )
            .await;

        let text = match &raised {
            None if self.adapter.grant_budget_exhausted() => not_raised_text(
                &canonical_target,
                &primary_root,
                "Not raised: this session has used its prompt budget. Stop requesting approvals; \
                 tell the human in conversation what you need and why, and continue with what \
                 you have.",
            ),
            None => not_raised_text(
                &canonical_target,
                &primary_root,
                "Not raised: this file was already asked about this session, or is refused \
                 outright. Nothing is recorded.",
            ),
            Some(req) => match self.adapter.grant_status(&req.decision_id) {
                GrantStatus::Decided(decision) if decision.access().is_some() => {
                    let tier = decision.tier();
                    // Report what is true, not what was answered: the answer is
                    // applied by the surface that received it, and a failed
                    // save or a refused live grant must not read as approved.
                    let live = sandbox.read_scopes().contains(&canonical_target);
                    let saved = tier != ahma_common::permissions::GrantTier::Always
                        || ahma_common::config::AhmaSettings::load_async()
                            .await
                            .log_targets
                            .is_target_approved(&key, &canonical_target);
                    if live && saved {
                        approved_text(&canonical_target, &primary_root, tier)
                    } else {
                        format!(
                            "A human approved reading\n  {}\n\nbut ahma could not apply it (the \
                             server log says why). Nothing changed. Tell the human, and do not \
                             ask again.",
                            canonical_target.display()
                        )
                    }
                }
                GrantStatus::Decided(_) => declined_text(&canonical_target),
                GrantStatus::Pending => pending_text(&canonical_target, &primary_root, req),
                GrantStatus::Closed => nobody_asked_text(&canonical_target, &primary_root, req),
            },
        };
        Ok(text_result(text))
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
///
/// There is deliberately no argument that approves: whatever a client sends
/// is the model's word (SPEC R5.4.5). Only a human answering the question this
/// raises — or pressing `a` on the log in the ahma TUI — approves a target.
pub fn logs_approve_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "file".to_string(),
        json!({
            "type": "string",
            "description": "Name of the log file symlink whose target you need to read (e.g. 'sys.log')."
        }),
    );
    props.insert(
        "reason".to_string(),
        schema::string_property(
            "One sentence, for the human: why you need to read this log. Shown at the prompt as \
             YOUR claim, next to the file the link points at, so write it for a person deciding \
             in ten seconds. Required.",
        ),
    );
    schema::object_input_schema(props, &["file", "reason"])
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// How a human approves a log target without the agent: the paste-able
/// alternative every reply that leaves it unapproved carries. There is no
/// CLI command that approves one, and nothing a tool call passes can.
fn human_alternatives(workspace: &Path) -> String {
    format!(
        "A human can approve it themselves: open `ahma tui` in {workspace}, pick this log with \
         `l` and press `a`; or use a client that shows permission prompts. Review approvals with \
         `ahma permissions list --kind log-target`.",
        workspace = workspace.display()
    )
}

/// The question never reached a human.
fn not_raised_text(target: &Path, workspace: &Path, status: &str) -> String {
    format!(
        "Requested read access to the log target\n  {target}\n\n{status}\n\n{alt}",
        target = target.display(),
        alt = human_alternatives(workspace),
    )
}

/// The question waits in the ahma TUI. Carries the exact body the human sees
/// (SPEC R-PERM.3.4) so the agent can relay it.
fn pending_text(
    target: &Path,
    workspace: &Path,
    req: &ahma_common::scope_grant::ScopeGrantRequest,
) -> String {
    format!(
        "Requested read access to the log target\n  {target}\n\nBlocked until a human answers. \
         The question is waiting in the ahma TUI. It is NOT approved until a person approves it; \
         Enter/Esc deny. Do not ask again; tell the human it is waiting there.\n\n{body}\n{alt}",
        target = target.display(),
        body = ahma_common::grant_prompt::render(req).to_message(),
        alt = human_alternatives(workspace),
    )
}

/// The question reached no human surface (rung 3, SPEC R-PERM.3): the agent is
/// the only way it reaches a person.
fn nobody_asked_text(
    target: &Path,
    workspace: &Path,
    req: &ahma_common::scope_grant::ScopeGrantRequest,
) -> String {
    format!(
        "Requested read access to the log target\n  {target}\n\nBlocked until a human approves \
         it: no surface could ask them (this client shows no prompts and no ahma TUI is \
         attached). Nothing is recorded. Show the human the text below UNCHANGED, then stop \
         asking.\n\n{body}\n{alt}",
        target = target.display(),
        body = ahma_common::grant_prompt::render(req).to_message(),
        alt = human_alternatives(workspace),
    )
}

/// A human declined. It is an answer: not asked again this session (R-PERM.4).
fn declined_text(target: &Path) -> String {
    format!(
        "A human declined reading the log target\n  {}\n\nNothing is recorded, and it will not be \
         asked about again this session. Continue without it, or explain in conversation why \
         you need it.",
        target.display()
    )
}

/// A human approved: the tier they chose decides whether anything was written.
fn approved_text(
    target: &Path,
    workspace: &Path,
    tier: ahma_common::permissions::GrantTier,
) -> String {
    let how_long = if tier == ahma_common::permissions::GrantTier::Always {
        let file = ahma_common::config::settings_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.ahma/settings.toml".to_string());
        format!(
            "Recorded as a `log-target` grant for this workspace in {file} and applied to this \
             session now. Review with `ahma permissions list --kind log-target`; revoke with \
             `ahma permissions revoke log-target {target} --workspace {workspace}`.",
            target = target.display(),
            workspace = workspace.display()
        )
    } else {
        "Approved for this session only (never written to disk) and applied now.".to_string()
    };
    format!(
        "✓ A human approved reading the log target\n  {}\n\n{how_long}",
        target.display()
    )
}

/// Already approved for this workspace: nothing was asked.
fn already_approved_text(target: &Path, workspace: &Path) -> String {
    format!(
        "The log target\n  {target}\n\nwas already approved for this workspace; nothing was \
         asked. Revoke with `ahma permissions revoke log-target {target} --workspace \
         {workspace}`.",
        target = target.display(),
        workspace = workspace.display()
    )
}

/// Extracts the required `file` argument and enforces that it is a plain
/// filename: no path separators, no leading dot. Shared by
/// `handle_logs_approve` and `require_safe_log_path` — the two validations are
/// security-relevant and must stay identical.
fn require_plain_log_filename(args: &Map<String, Value>) -> Result<&str, McpError> {
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
    Ok(file_name)
}

/// Validates and resolves a caller-supplied log file name into a safe absolute path.
///
/// Rejects absolute paths, path separators, and traversals that escape the log directory.
async fn require_safe_log_path(
    args: &Map<String, Value>,
    log_dir: &Path,
) -> Result<PathBuf, McpError> {
    let file_name = require_plain_log_filename(args)?;

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

/// Resolves the symlink target and approval status for a single log-directory
/// entry already known to be a symlink. Split out of `collect_log_sources` so
/// the caller's loop body reads as a flat sequence of steps rather than a
/// three-level-deep `if let` chain.
///
/// A target that resolves inside `canonical_log_dir` (ahma's own rolling-log
/// symlinks, e.g. `ahma.log` → `ahma.log.2026-06-15`) is always approved,
/// regardless of sandbox scope — it was created by ahma's own rotation code,
/// not an external actor. A target that escapes the log directory falls back
/// to the sandbox scope/exception check. A target that can't be read or
/// canonicalized is reported (when known) but not approved.
async fn resolve_symlink_approval(
    path: &Path,
    log_dir: &Path,
    canonical_log_dir: &Path,
    scopes: &[PathBuf],
    exceptions: &[PathBuf],
) -> (Option<String>, bool) {
    let Ok(target) = tokio::fs::read_link(path).await else {
        return (None, false);
    };
    let symlink_target = Some(target.display().to_string());

    let Ok(canonical_target) = canonicalize_dunce(log_dir.join(&target)).await else {
        return (symlink_target, false);
    };

    let is_approved = canonical_target.starts_with(canonical_log_dir)
        || crate::sandbox::is_target_allowed(&canonical_target, scopes, exceptions);
    (symlink_target, is_approved)
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
        let (symlink_target, is_approved) = if is_symlink {
            resolve_symlink_approval(&path, log_dir, &canonical_log_dir, scopes, exceptions).await
        } else {
            (None, true)
        };

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

    /// `logs_approve` takes the link and the agent's reason — and nothing
    /// that could pass for a human's approval (SPEC R5.4.5).
    #[test]
    fn test_logs_approve_schema() {
        let schema = logs_approve_schema();
        assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
        let props = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["file", "reason"]);
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["file", "reason"]);
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
        let _guard = EnvVarGuard::set("AHMA_TEST_LOG_DIR", log_dir.path());

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
        let _guard = EnvVarGuard::set("AHMA_TEST_LOG_DIR", log_dir.path());

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
        let _guard = EnvVarGuard::set("AHMA_TEST_LOG_DIR", log_dir.path());

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
        let _guard = EnvVarGuard::set("AHMA_TEST_LOG_DIR", log_dir.path());

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

    /// The agent can plant a link in `.ahma/logs`, so the hard denylist that
    /// binds every filesystem grant (R-PERM.4.3) binds a log target too.
    #[cfg(unix)]
    #[tokio::test]
    async fn handle_logs_approve_refuses_a_hard_denylisted_target() {
        use std::os::unix::fs::symlink;
        let (service, _scope) = build_test_service().await.unwrap();
        let log_dir = tempdir().unwrap();
        let home = tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let key = ssh.join("id_ed25519");
        std::fs::write(&key, "secret").unwrap();
        // The agent can write `.ahma/logs`, so it can plant this link itself.
        symlink(&key, log_dir.path().join("key.log")).unwrap();

        let _log_guard = EnvVarGuard::set("AHMA_TEST_LOG_DIR", log_dir.path());
        let _home_guard = EnvVarGuard::set("AHMA_TEST_HOME", home.path());

        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("key.log".to_string()));
        let err = service
            .handle_logs_approve(args)
            .await
            .expect_err("a credential file can never become a log target (R-PERM.4.3)");
        assert!(err.message.contains("REFUSED"), "{}", err.message);

        let settings = home.path().join(".ahma").join("settings.toml");
        let written = std::fs::read_to_string(&settings).unwrap_or_default();
        assert!(
            !written.contains("id_ed25519"),
            "a refused target must not reach the ledger: {written}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // logs_approve asks a human; only the human's answer records (SPEC R9.2)
    // ─────────────────────────────────────────────────────────────────────

    /// A temp home, a log dir holding `sys.log` → a file outside the
    /// workspace, and the env guards that point ahma at them. The guards
    /// restore the environment on drop.
    #[cfg(unix)]
    struct LogLinkFixture {
        tmp: tempfile::TempDir,
        workspace: PathBuf,
        log_dir: PathBuf,
        home: PathBuf,
        /// The canonical target `sys.log` points at.
        target: PathBuf,
        _log_guard: EnvVarGuard,
        _home_guard: EnvVarGuard,
    }

    #[cfg(unix)]
    impl LogLinkFixture {
        fn new() -> Self {
            let tmp = tempdir().unwrap();
            let workspace = tmp.path().join("workspace");
            let log_dir = tmp.path().join("logs");
            let home = tmp.path().join("home");
            let external = tmp.path().join("external");
            for d in [&workspace, &log_dir, &home, &external] {
                std::fs::create_dir_all(d).unwrap();
            }
            let external_file = external.join("sys.log");
            std::fs::write(&external_file, "external content").unwrap();
            std::os::unix::fs::symlink(&external_file, log_dir.join("sys.log")).unwrap();
            let target = dunce::canonicalize(&external_file).unwrap();
            let _log_guard = EnvVarGuard::set("AHMA_TEST_LOG_DIR", &log_dir);
            let _home_guard = EnvVarGuard::set("AHMA_TEST_HOME", &home);
            Self {
                tmp,
                workspace,
                log_dir,
                home,
                target,
                _log_guard,
                _home_guard,
            }
        }

        fn ledger(&self) -> PathBuf {
            self.home.join(".ahma").join("settings.toml")
        }

        /// The `log-target` rows in the ledger for this workspace.
        fn approved(&self) -> Vec<PathBuf> {
            let settings = ahma_common::config::AhmaSettings::load_from(&self.ledger());
            let key = ahma_common::permissions::workspace_key(&self.workspace);
            settings.log_targets.approved_targets(&key)
        }

        fn audit(&self) -> Vec<ahma_common::permissions::AuditEntry> {
            ahma_common::permissions::read_audit_entries(
                &self.home.join(".ahma").join("permissions-audit.jsonl"),
            )
        }

        /// No `fs-scope` grant was written for anything: a log-target answer
        /// never takes the ordinary scope-grant path.
        fn assert_no_fs_scope_grant(&self) {
            let settings = ahma_common::config::AhmaSettings::load_from(&self.ledger());
            assert!(
                settings.sandbox.persistent_scopes.is_empty(),
                "a log target is never an fs-scope grant: {:?}",
                settings.sandbox.persistent_scopes
            );
        }
    }

    #[cfg(unix)]
    fn approve_args() -> Map<String, Value> {
        let mut args = Map::new();
        args.insert("file".to_string(), Value::String("sys.log".to_string()));
        args.insert(
            "reason".to_string(),
            Value::String("the app's errors are in its own log".to_string()),
        );
        args
    }

    /// A server whose client shows elicitation prompts and answers each with
    /// `answer` (`None` declines, as the elicitation `decline` action).
    #[cfg(unix)]
    async fn server_answering(
        f: &LogLinkFixture,
        client: crate::test_utils::recording_client::RecordingClient,
    ) -> crate::test_utils::in_process::InProcessMcp<
        crate::test_utils::recording_client::RecordingClient,
    > {
        let (mcp, _broker) = crate::test_utils::in_process::create_in_process_mcp_with_broker(
            client,
            &f.workspace,
            None,
        )
        .await
        .unwrap();
        mcp
    }

    #[cfg(unix)]
    async fn listed_as_approved(service: &AhmaMcpService) -> bool {
        let listed = service.handle_logs_list(Map::new()).await.unwrap();
        let entries: Vec<Value> = serde_json::from_str(&text_of(&listed)).unwrap();
        entries
            .iter()
            .find(|e| e["name"] == "sys.log")
            .and_then(|e| e["is_approved"].as_bool())
            .expect("sys.log is listed")
    }

    /// `reason` is required, like `sandbox_grant`'s: the human is shown it.
    #[cfg(unix)]
    #[tokio::test]
    async fn handle_logs_approve_requires_a_reason() {
        let f = LogLinkFixture::new();
        let (service, _scope) = build_test_service().await.unwrap();
        let mut args = approve_args();
        args.remove("reason");
        let err = service.handle_logs_approve(args).await.unwrap_err();
        assert!(err.message.contains("`reason`"), "{}", err.message);
        assert!(f.approved().is_empty());
    }

    /// The bug this fixes: an MCP call alone recorded the grant. With nobody
    /// to ask — no notifier wired, or a client that shows no prompts and no
    /// TUI — the call writes NOTHING, and says how a human can approve it.
    #[cfg(unix)]
    #[tokio::test]
    async fn logs_approve_without_a_human_answer_writes_nothing() {
        let f = LogLinkFixture::new();

        // No permission broker at all.
        let (service, _scope) = build_test_service().await.unwrap();
        let text = text_of(&service.handle_logs_approve(approve_args()).await.unwrap());
        assert!(text.contains("Not raised"), "{text}");
        assert!(text.contains("ahma tui"), "the human alternative: {text}");
        assert!(!text.contains('✓'), "{text}");

        // The real ladder, with a client that cannot elicit and no TUI.
        let mcp = server_answering(
            &f,
            crate::test_utils::recording_client::RecordingClient::new("cursor"),
        )
        .await;
        let text = text_of(
            &mcp.service
                .handle_logs_approve(approve_args())
                .await
                .unwrap(),
        );
        assert!(
            text.contains("no surface could ask them"),
            "nobody was asked: {text}"
        );
        assert!(
            text.contains("a log file in .ahma/logs links to this file outside the workspace"),
            "the body the human must be shown: {text}"
        );
        assert!(!text.contains('✓'), "{text}");

        assert!(
            ahma_common::config::AhmaSettings::load_from(&f.ledger())
                .log_targets
                .approvals
                .is_empty(),
            "nothing reached the ledger, for any workspace"
        );
        assert!(
            !f.audit()
                .iter()
                .any(|a| a.action == ahma_common::permissions::AuditAction::Grant),
            "nothing was granted: {:?}",
            f.audit()
        );
        assert!(
            !mcp.service
                .adapter
                .sandbox()
                .read_scopes()
                .contains(&f.target),
            "the live session did not widen either"
        );
        assert!(!listed_as_approved(&mcp.service).await);
    }

    /// A human's `always` answer at the client's prompt records exactly one
    /// audited `log-target` row — with the agent's tool as `granted_by` and
    /// the surface that answered — applies it to this session, and writes no
    /// `fs-scope` grant. The prompt says what the link is and shows the
    /// agent's reason. Asking again is answered from the ledger.
    #[cfg(unix)]
    #[tokio::test]
    async fn logs_approve_always_answer_records_one_audited_row() {
        let f = LogLinkFixture::new();
        let client = crate::test_utils::recording_client::RecordingClient::new("cursor")
            .with_elicitation(Some("read-only"));
        let prompts = client.elicitation().expect("elicitation enabled");
        let mcp = server_answering(&f, client).await;
        let service = &mcp.service;
        assert!(!listed_as_approved(service).await);

        let text = text_of(&service.handle_logs_approve(approve_args()).await.unwrap());
        assert!(text.contains("A human approved"), "{text}");
        assert!(text.contains("log-target"), "{text}");

        let asked = prompts.messages();
        assert_eq!(asked.len(), 1, "one question: {asked:?}");
        assert!(
            asked[0].contains(
                "a log file in .ahma/logs links to this file outside the workspace; approving \
                 lets ahma's log tools read it"
            ),
            "{}",
            asked[0]
        );
        assert!(
            asked[0].contains("the app's errors are in its own log"),
            "the agent's claim is shown: {}",
            asked[0]
        );

        assert_eq!(f.approved(), vec![f.target.clone()]);
        let settings = ahma_common::config::AhmaSettings::load_from(&f.ledger());
        let row = &settings.log_targets.approvals[0];
        assert_eq!(row.granted_by.as_deref(), Some("logs_approve"));
        assert_eq!(row.surface.as_deref(), Some("harness"));
        f.assert_no_fs_scope_grant();

        let grants: Vec<_> = f
            .audit()
            .into_iter()
            .filter(|a| a.action == ahma_common::permissions::AuditAction::Grant)
            .collect();
        assert_eq!(grants.len(), 1, "one audited grant: {grants:?}");
        assert_eq!(
            grants[0].kind,
            ahma_common::permissions::GrantKind::LogTarget
        );
        assert_eq!(grants[0].subject, f.target.display().to_string());
        assert_eq!(grants[0].surface.as_deref(), Some("harness"));

        assert!(
            service.adapter.sandbox().read_scopes().contains(&f.target),
            "an approved target is readable this session (R-PERM.4.1)"
        );
        assert!(listed_as_approved(service).await);

        // Asking again is answered from the ledger: no second question, no
        // second row.
        let again = text_of(&service.handle_logs_approve(approve_args()).await.unwrap());
        assert!(again.contains("already approved"), "{again}");
        assert_eq!(prompts.messages().len(), 1);
        assert_eq!(f.approved(), vec![f.target.clone()]);

        // The retired store is never written.
        assert!(
            !f.home
                .join(".config")
                .join("ahma")
                .join("log_exceptions.json")
                .exists()
        );
    }

    /// `deny` is an answer: nothing recorded, the denial audited as a
    /// `log-target`, and not asked again this session (R-PERM.4).
    #[cfg(unix)]
    #[tokio::test]
    async fn logs_approve_deny_answer_writes_nothing() {
        let f = LogLinkFixture::new();
        let client = crate::test_utils::recording_client::RecordingClient::new("cursor")
            .with_elicitation(Some("deny"));
        let prompts = client.elicitation().expect("elicitation enabled");
        let mcp = server_answering(&f, client).await;

        let text = text_of(
            &mcp.service
                .handle_logs_approve(approve_args())
                .await
                .unwrap(),
        );
        assert!(text.contains("A human declined"), "{text}");
        assert!(f.approved().is_empty());
        f.assert_no_fs_scope_grant();
        assert!(
            f.audit()
                .iter()
                .any(|a| a.action == ahma_common::permissions::AuditAction::Deny
                    && a.kind == ahma_common::permissions::GrantKind::LogTarget),
            "the denial is audited as a log-target: {:?}",
            f.audit()
        );
        assert!(
            !mcp.service
                .adapter
                .sandbox()
                .read_scopes()
                .contains(&f.target)
        );

        let again = text_of(
            &mcp.service
                .handle_logs_approve(approve_args())
                .await
                .unwrap(),
        );
        assert!(again.contains("Not raised"), "{again}");
        assert_eq!(
            prompts.messages().len(),
            1,
            "a denied target is not re-asked"
        );
    }

    /// The client cancelling the prompt is nobody's decision (SPEC R5.3.1):
    /// nothing is recorded, it is not reported as a denial, and the next call
    /// asks again.
    #[cfg(unix)]
    #[tokio::test]
    async fn logs_approve_cancel_is_not_a_denial() {
        let f = LogLinkFixture::new();
        let client = crate::test_utils::recording_client::RecordingClient::new("cursor")
            .with_elicitation_cancel();
        let prompts = client.elicitation().expect("elicitation enabled");
        let mcp = server_answering(&f, client).await;

        let text = text_of(
            &mcp.service
                .handle_logs_approve(approve_args())
                .await
                .unwrap(),
        );
        assert!(!text.contains("declined"), "a cancel is not a no: {text}");
        assert!(text.contains("no surface could ask them"), "{text}");
        assert!(f.approved().is_empty());
        assert!(
            !f.audit()
                .iter()
                .any(|a| a.kind == ahma_common::permissions::GrantKind::LogTarget),
            "no decision was made, so none is audited: {:?}",
            f.audit()
        );

        let _ = mcp
            .service
            .handle_logs_approve(approve_args())
            .await
            .unwrap();
        assert_eq!(
            prompts.messages().len(),
            2,
            "a dismissed question is asked again"
        );
    }

    /// A `session` answer makes the target readable now and writes nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn logs_approve_session_answer_is_live_and_never_written() {
        let f = LogLinkFixture::new();
        let client = crate::test_utils::recording_client::RecordingClient::new("cursor")
            .with_elicitation(Some("read-only-session"));
        let mcp = server_answering(&f, client).await;

        let text = text_of(
            &mcp.service
                .handle_logs_approve(approve_args())
                .await
                .unwrap(),
        );
        assert!(text.contains("this session only"), "{text}");
        assert!(
            mcp.service
                .adapter
                .sandbox()
                .read_scopes()
                .contains(&f.target),
            "readable this session"
        );
        assert!(f.approved().is_empty(), "a session answer is never written");
        f.assert_no_fs_scope_grant();
        assert!(
            f.audit()
                .iter()
                .any(|a| a.kind == ahma_common::permissions::GrantKind::LogTarget
                    && a.tier == ahma_common::permissions::GrantTier::Session),
            "{:?}",
            f.audit()
        );
    }

    /// A target on the hard denylist is refused before any question is put
    /// to a human — even one who would say yes.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_denylisted_target_is_refused_before_any_question() {
        let f = LogLinkFixture::new();
        let ssh = f.home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let key = ssh.join("id_ed25519");
        std::fs::write(&key, "secret").unwrap();
        std::os::unix::fs::symlink(&key, f.log_dir.join("key.log")).unwrap();

        let client = crate::test_utils::recording_client::RecordingClient::new("cursor")
            .with_elicitation(Some("read-only"));
        let prompts = client.elicitation().expect("elicitation enabled");
        let mcp = server_answering(&f, client).await;

        let mut args = approve_args();
        args.insert("file".to_string(), Value::String("key.log".to_string()));
        let err = mcp.service.handle_logs_approve(args).await.unwrap_err();
        assert!(err.message.contains("REFUSED"), "{}", err.message);
        assert!(prompts.messages().is_empty(), "nobody is asked");
        assert!(f.approved().is_empty());
    }

    /// The TUI's `[a]` on a blocked log is a human's decision at an
    /// unsandboxed surface, not an MCP call: it still records the row, with
    /// the TUI as its surface — and records nothing when the link was swapped
    /// after it was shown.
    #[cfg(unix)]
    #[test]
    fn the_tui_approval_of_a_log_link_still_records() {
        let f = LogLinkFixture::new();
        let link = f.log_dir.join("sys.log");
        let shown = std::fs::read_link(&link).unwrap();

        let (target, added) =
            crate::sandbox::approve_log_link(&f.workspace, &link, Some(&shown), "ahma tui", "tui")
                .unwrap();
        assert!(added);
        assert_eq!(target, f.target);
        assert_eq!(f.approved(), vec![f.target.clone()]);
        let settings = ahma_common::config::AhmaSettings::load_from(&f.ledger());
        assert_eq!(
            settings.log_targets.approvals[0].surface.as_deref(),
            Some("tui")
        );
        assert!(
            f.audit()
                .iter()
                .any(|a| a.kind == ahma_common::permissions::GrantKind::LogTarget
                    && a.surface.as_deref() == Some("tui"))
        );

        // The agent swaps the link between the banner and the key press.
        let other = f.tmp.path().join("external").join("other.log");
        std::fs::write(&other, "x").unwrap();
        let swapped = f.log_dir.join("swap.log");
        std::os::unix::fs::symlink(&other, &swapped).unwrap();
        let err = crate::sandbox::approve_log_link(
            &f.workspace,
            &swapped,
            Some(&shown),
            "ahma tui",
            "tui",
        )
        .unwrap_err();
        assert!(err.contains("nothing was recorded"), "{err}");
        assert_eq!(f.approved(), vec![f.target.clone()]);
    }
}
