//! # ahma_renewal — Renewal contract for long-running tasks
//!
//! Closes the "scheduled task drift" vulnerability: a task running > N minutes
//! unattended is automatically halted, a checkpoint is written to the vault,
//! and the user must explicitly re-approve before the operation continues.
//!
//! ## License
//!
//! This crate is licensed under **GPL-3.0-or-later**.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use ahma_vault::audit::AuditWriter;

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

struct WatchedOp {
    tool_name: String,
    started_at: Instant,
    last_checkpoint: Instant,
    token: CancellationToken,
}

/// Notification emitted when an operation is halted for renewal.
#[derive(Debug, Clone)]
pub struct RenewalHaltEvent {
    pub operation_id: String,
    pub tool_name: String,
    pub elapsed_secs: u64,
    pub checkpoint_path: PathBuf,
}

/// Background watcher that enforces the renewal contract.
pub struct RenewalWatcher {
    cfg: RenewalConfig,
    ops: Arc<RwLock<HashMap<String, WatchedOp>>>,
    audit: Option<AuditWriter>,
}

impl RenewalWatcher {
    /// Create a watcher with the given config.
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
        assert!(events.is_empty());
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn halt_after_zero_second_deadline() {
        let tmp = TempDir::new().unwrap();
        let watcher = make_watcher(&tmp, 0);
        let token = CancellationToken::new();
        watcher.register("op_1", "cargo_build", token.clone());

        tokio::time::sleep(Duration::from_millis(10)).await;

        let events = watcher.tick().await;
        assert_eq!(events.len(), 1);
        assert!(token.is_cancelled());
        assert!(tmp.path().join("op_1.checkpoint.json").exists());
    }

    #[tokio::test]
    async fn checkpoint_resets_timer() {
        let tmp = TempDir::new().unwrap();
        let watcher = make_watcher(&tmp, 300);
        let token = CancellationToken::new();
        watcher.register("op_1", "cargo_build", token.clone());

        watcher.checkpoint("op_1");

        let events = watcher.tick().await;
        assert!(events.is_empty());
    }
}
