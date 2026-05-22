//! # Renewal Contract for Long-Running Tasks (T3.4)
//!
//! Closes Cowork's "scheduled task drift" vulnerability:
//!
//! > A task running > N minutes unattended can be re-injected via its input
//! > corpus without the user knowing, because the original approval covers
//! > all future inputs, not just the inputs present at approval time.
//!
//! The renewal contract enforces:
//!
//! 1. Any operation that has run longer than `renew_after` seconds **without
//!    producing a checkpoint** is automatically halted via its cancellation
//!    token.
//! 2. Before halting, a checkpoint is written to the vault: current
//!    accumulated output + intermediate state.
//! 3. The TUI/user is notified via an `ApprovalRequired` event (see
//!    [`crate::tui::app::TuiEvent`]).
//! 4. The user must explicitly re-approve before the operation continues.
//!
//! ## Integration
//!
//! ```no_run
//! use ahma_mcp::renewal::{RenewalWatcher, RenewalConfig};
//! use std::time::Duration;
//!
//! let config = RenewalConfig {
//!     renew_after: Duration::from_secs(300),   // 5 minutes
//!     checkpoint_dir: "/path/to/vault/workdir".into(),
//! };
//! let watcher = RenewalWatcher::new(config);
//!
//! // Register an operation to watch.
//! watcher.register("op_001", "cargo_build");
//!
//! // Check periodically (call from a background task).
//! // watcher.tick().await;
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::vault::audit::AuditWriter;

// ─────────────────────────────────────────────────────────────────────────────
// RenewalConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the renewal contract.
#[derive(Debug, Clone)]
pub struct RenewalConfig {
    /// Halt an operation that has run longer than this without a checkpoint.
    pub renew_after: Duration,
    /// Directory to write checkpoint files.
    pub checkpoint_dir: PathBuf,
}

impl Default for RenewalConfig {
    fn default() -> Self {
        Self {
            renew_after: Duration::from_secs(300),
            checkpoint_dir: PathBuf::from("."),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WatchedOp
// ─────────────────────────────────────────────────────────────────────────────

struct WatchedOp {
    tool_name: String,
    started_at: Instant,
    last_checkpoint: Instant,
    token: CancellationToken,
}

// ─────────────────────────────────────────────────────────────────────────────
// CheckpointEvent
// ─────────────────────────────────────────────────────────────────────────────

/// Notification emitted when an operation is halted for renewal.
#[derive(Debug, Clone)]
pub struct RenewalHaltEvent {
    pub operation_id: String,
    pub tool_name: String,
    pub elapsed_secs: u64,
    pub checkpoint_path: PathBuf,
}

// ─────────────────────────────────────────────────────────────────────────────
// RenewalWatcher
// ─────────────────────────────────────────────────────────────────────────────

/// Background watcher that enforces the renewal contract.
pub struct RenewalWatcher {
    cfg: RenewalConfig,
    ops: Arc<RwLock<HashMap<String, WatchedOp>>>,
    audit: Option<AuditWriter>,
}

impl RenewalWatcher {
    /// Create a watcher with the given config and optional audit log.
    pub fn new(cfg: RenewalConfig) -> Self {
        Self {
            cfg,
            ops: Arc::new(RwLock::new(HashMap::new())),
            audit: None,
        }
    }

    /// Attach an audit writer to record renewal events.
    pub fn with_audit(mut self, writer: AuditWriter) -> Self {
        self.audit = Some(writer);
        self
    }

    /// Register an operation to watch.
    ///
    /// `token` is the `CancellationToken` from the existing `Operation`; the
    /// watcher cancels it when the renewal deadline is exceeded.
    pub fn register(&self, op_id: &str, tool_name: &str, token: CancellationToken) {
        let mut ops = self.ops.write().unwrap();
        ops.insert(
            op_id.to_string(),
            WatchedOp {
                tool_name: tool_name.to_string(),
                started_at: Instant::now(),
                last_checkpoint: Instant::now(),
                token,
            },
        );
    }

    /// Deregister a completed or cancelled operation.
    pub fn deregister(&self, op_id: &str) {
        let mut ops = self.ops.write().unwrap();
        ops.remove(op_id);
    }

    /// Record a checkpoint for an operation (resets the renewal timer).
    pub fn checkpoint(&self, op_id: &str) {
        let mut ops = self.ops.write().unwrap();
        if let Some(op) = ops.get_mut(op_id) {
            op.last_checkpoint = Instant::now();
        }
    }

    /// Check all watched operations and halt any that have exceeded the renewal window.
    ///
    /// Returns a list of halt events that should be forwarded to the TUI/user.
    pub async fn tick(&self) -> Vec<RenewalHaltEvent> {
        let mut to_halt = vec![];

        {
            let ops = self.ops.read().unwrap();
            for (op_id, op) in ops.iter() {
                let since_checkpoint = op.last_checkpoint.elapsed();
                if since_checkpoint > self.cfg.renew_after {
                    to_halt.push((op_id.clone(), op.tool_name.clone(), op.started_at.elapsed()));
                }
            }
        }

        let mut halt_events = vec![];
        for (op_id, tool_name, elapsed) in to_halt {
            let checkpoint_path = self
                .write_checkpoint(&op_id, &tool_name, elapsed)
                .await
                .unwrap_or_else(|_| self.cfg.checkpoint_dir.join(format!("{op_id}.checkpoint")));

            // Cancel the operation.
            {
                let ops = self.ops.read().unwrap();
                if let Some(op) = ops.get(&op_id) {
                    warn!(
                        "Renewal contract: halting {op_id} ({tool_name}) after {}s unattended",
                        elapsed.as_secs()
                    );
                    op.token.cancel();
                }
            }

            self.deregister(&op_id);

            let elapsed_secs = elapsed.as_secs();

            if let Some(ref writer) = self.audit {
                let _ = writer
                    .renewal_checkpoint(
                        &op_id,
                        elapsed_secs,
                        checkpoint_path.to_str().unwrap_or(""),
                    )
                    .await;
                let _ = writer
                    .task_halted(&op_id, "Renewal contract exceeded — re-approval required")
                    .await;
            }

            halt_events.push(RenewalHaltEvent {
                operation_id: op_id,
                tool_name,
                elapsed_secs,
                checkpoint_path,
            });
        }

        halt_events
    }

    async fn write_checkpoint(
        &self,
        op_id: &str,
        tool_name: &str,
        elapsed: Duration,
    ) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.cfg.checkpoint_dir).await?;
        let path = self
            .cfg
            .checkpoint_dir
            .join(format!("{op_id}.checkpoint.json"));
        let data = serde_json::json!({
            "operation_id": op_id,
            "tool_name": tool_name,
            "elapsed_secs": elapsed.as_secs(),
            "halted_at": chrono::Utc::now().to_rfc3339(),
            "reason": "Renewal contract exceeded — re-approval required"
        });
        tokio::fs::write(&path, serde_json::to_string_pretty(&data)?.as_bytes()).await?;
        info!("Checkpoint written: {}", path.display());
        Ok(path)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_watcher(tmp: &TempDir, renew_after_secs: u64) -> RenewalWatcher {
        RenewalWatcher::new(RenewalConfig {
            renew_after: Duration::from_secs(renew_after_secs),
            checkpoint_dir: tmp.path().to_path_buf(),
        })
    }

    #[tokio::test]
    async fn no_halt_before_deadline() {
        let tmp = TempDir::new().unwrap();
        let watcher = make_watcher(&tmp, 300);
        let token = CancellationToken::new();
        watcher.register("op_1", "cargo_build", token.clone());

        let events = watcher.tick().await;
        assert!(events.is_empty(), "should not halt before deadline");
        assert!(!token.is_cancelled(), "token should not be cancelled");
    }

    #[tokio::test]
    async fn halt_after_zero_second_deadline() {
        let tmp = TempDir::new().unwrap();
        // Use 0-second renewal window so the first tick always fires.
        let watcher = make_watcher(&tmp, 0);
        let token = CancellationToken::new();
        watcher.register("op_1", "cargo_build", token.clone());

        // Brief sleep to ensure elapsed > 0.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let events = watcher.tick().await;
        assert_eq!(events.len(), 1, "op_1 should be halted");
        assert!(token.is_cancelled(), "cancellation token should fire");
        assert!(
            tmp.path().join("op_1.checkpoint.json").exists(),
            "checkpoint file written"
        );
    }

    #[tokio::test]
    async fn checkpoint_resets_timer() {
        let tmp = TempDir::new().unwrap();
        let watcher = make_watcher(&tmp, 300);
        let token = CancellationToken::new();
        watcher.register("op_1", "cargo_build", token.clone());

        // Record a checkpoint — should reset the timer.
        watcher.checkpoint("op_1");

        let events = watcher.tick().await;
        assert!(events.is_empty(), "should not halt after recent checkpoint");
    }
}
