//! Append-only audit log for task vaults.
//!
//! Every significant action taken within a vault — tool calls, artifact writes,
//! staged deletions, elevation grants, and sub-task lifecycle events — is
//! recorded here as a newline-delimited JSON event.  The log is append-only
//! so individual events cannot be silently removed after the fact.
//!
//! ## Format
//!
//! Each line is a JSON object: `{"timestamp":"…","kind":{"type":"…",…}}`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt as _;

// ─────────────────────────────────────────────────────────────────────────────
// AuditEvent
// ─────────────────────────────────────────────────────────────────────────────

/// A single timestamped entry in the vault audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// UTC timestamp (RFC 3339).
    pub timestamp: String,
    /// Optional OTel trace ID for correlation (populated when tracing is active).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// The event payload.
    #[serde(flatten)]
    pub kind: AuditEventKind,
}

/// Every kind of auditable event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEventKind {
    /// The vault was created.
    VaultCreated { vault_path: String, slug: String },

    /// An MCP tool was invoked (recorded before execution).
    ToolCall {
        operation_id: String,
        tool_name: String,
        /// Human-readable summary of key arguments.  Secrets are redacted.
        args_summary: String,
    },

    /// An MCP tool completed (recorded after execution).
    ToolComplete {
        operation_id: String,
        success: bool,
        duration_ms: u64,
    },

    /// An artifact was written to the outputs directory.
    ArtifactWritten {
        /// Path relative to vault root.
        path: String,
        size_bytes: u64,
    },

    /// A file was moved to the trash staging area.
    FileStaged {
        /// Original path (relative to vault root if inside the vault).
        original_path: String,
        /// Trash path.
        trash_path: String,
    },

    /// Staged files were permanently purged from trash.
    TrashPurged { count: u64 },

    /// An elevation was requested (e.g. write outside vault scope).
    ElevationRequested {
        /// Why the elevation is needed.
        reason: String,
        /// Whether the user granted it.
        granted: bool,
    },

    /// A sub-task was dispatched (from a `decompose` tool call).
    SubTaskDispatched {
        parent_op: String,
        sub_op: String,
        model: String,
        prompt_summary: String,
    },

    /// A sub-task completed.
    SubTaskCompleted {
        sub_op: String,
        success: bool,
        result_summary: String,
    },

    /// A renewal checkpoint was recorded for a long-running task.
    RenewalCheckpoint {
        operation_id: String,
        elapsed_secs: u64,
        checkpoint_path: String,
    },

    /// A task was halted pending re-approval.
    TaskHalted {
        operation_id: String,
        reason: String,
    },

    /// A worker (synthesized code) was executed.
    WorkerExecuted {
        operation_id: String,
        language: String,
        source_hash: String,
        kept_source: bool,
    },

    /// An egress domain was allowed or blocked.
    EgressDecision { domain: String, allowed: bool },
}

// ─────────────────────────────────────────────────────────────────────────────
// AuditWriter
// ─────────────────────────────────────────────────────────────────────────────

/// Appends [`AuditEvent`]s to the vault's `audit.jsonl` file.
///
/// Cloning the writer is cheap — all clones share the same path and each
/// open–write–close cycle is atomic at the line level.
#[derive(Debug, Clone)]
pub struct AuditWriter {
    path: PathBuf,
}

impl AuditWriter {
    /// Create a writer for the given audit log path.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Append a new event to the audit log.
    pub async fn emit(&self, kind: AuditEventKind) -> Result<()> {
        let event = AuditEvent {
            timestamp: Utc::now().to_rfc3339(),
            trace_id: None,
            kind,
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Emit a vault-created event.
    pub async fn vault_created(&self, vault_path: &str, slug: &str) -> Result<()> {
        self.emit(AuditEventKind::VaultCreated {
            vault_path: vault_path.to_string(),
            slug: slug.to_string(),
        })
        .await
    }

    /// Emit a tool-call event.
    pub async fn tool_call(
        &self,
        operation_id: &str,
        tool_name: &str,
        args_summary: &str,
    ) -> Result<()> {
        self.emit(AuditEventKind::ToolCall {
            operation_id: operation_id.to_string(),
            tool_name: tool_name.to_string(),
            args_summary: args_summary.to_string(),
        })
        .await
    }

    /// Emit a tool-complete event.
    pub async fn tool_complete(
        &self,
        operation_id: &str,
        success: bool,
        duration_ms: u64,
    ) -> Result<()> {
        self.emit(AuditEventKind::ToolComplete {
            operation_id: operation_id.to_string(),
            success,
            duration_ms,
        })
        .await
    }

    /// Emit a renewal-checkpoint event.
    pub async fn renewal_checkpoint(
        &self,
        operation_id: &str,
        elapsed_secs: u64,
        checkpoint_path: &str,
    ) -> Result<()> {
        self.emit(AuditEventKind::RenewalCheckpoint {
            operation_id: operation_id.to_string(),
            elapsed_secs,
            checkpoint_path: checkpoint_path.to_string(),
        })
        .await
    }

    /// Emit a task-halted event.
    pub async fn task_halted(&self, operation_id: &str, reason: &str) -> Result<()> {
        self.emit(AuditEventKind::TaskHalted {
            operation_id: operation_id.to_string(),
            reason: reason.to_string(),
        })
        .await
    }

    /// Emit an egress-decision event.
    pub async fn emit_egress_decision(&self, domain: &str, allowed: bool) -> Result<()> {
        self.emit(AuditEventKind::EgressDecision {
            domain: domain.to_string(),
            allowed,
        })
        .await
    }

    /// Return the path to the underlying audit log file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn emit_creates_and_appends_jsonl() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("audit.jsonl");
        let writer = AuditWriter::new(&log_path);

        writer
            .tool_call("op_1", "cargo_build", "--release")
            .await
            .unwrap();
        writer.tool_complete("op_1", true, 4200).await.unwrap();

        let contents = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = contents.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "tool_call");
        assert_eq!(first["tool_name"], "cargo_build");

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["type"], "tool_complete");
        assert_eq!(second["success"], true);
    }

    #[tokio::test]
    async fn emit_is_idempotent_on_missing_parent() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("sub").join("audit.jsonl");
        let writer = AuditWriter::new(&log_path);
        let result = writer.task_halted("op_1", "test").await;
        assert!(result.is_err(), "missing parent dir should produce an error");
    }
}
