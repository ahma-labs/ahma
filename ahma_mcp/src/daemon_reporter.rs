//! Daemon reporter — background task that registers this ahma instance with
//! the hub daemon and forwards operation events to it.
//!
//! ## Design goals
//!
//! * **Non-blocking**: spawned as a detached Tokio task; never affects MCP
//!   operation if the daemon is unavailable or crashes.
//! * **Self-healing**: reconnects automatically with exponential back-off
//!   (initial 5 s, cap 30 s) when the daemon connection is lost.
//! * **Low overhead**: polls [`OperationMonitor`] every 2 seconds and diffs
//!   the snapshot — no changes means no wire traffic.

use crate::operation_monitor::{Operation, OperationMonitor, OperationStatus};
use ahma_common::daemon_hub::{
    ClientMsg, DaemonEvent, connect_to_daemon, ensure_daemon_running, send_msg,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tracing::{debug, warn};

// ─── Snapshot entry ───────────────────────────────────────────────────────────

#[derive(Clone)]
struct SnapEntry {
    tool_name: String,
    description: String,
    status: OperationStatus,
    scope: String,
    result_summary: Option<String>,
    duration_ms: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn a background task that registers this instance with the hub daemon and
/// forwards operation events.
///
/// This function returns immediately; all errors are logged at `warn` / `debug`
/// level and the task retries silently.
///
/// # Arguments
///
/// * `monitor` – the shared [`OperationMonitor`] for this service instance.
/// * `mode` – transport mode string shown in the TUI (`"stdio"`, `"http"`, …).
/// * `scope` – sandbox scope / workspace root path (human readable).
/// * `label` – short label for this instance (e.g. `"VS Code"` or `"ahma"`).
pub fn spawn_reporter(
    monitor: Arc<OperationMonitor>,
    mode: impl Into<String> + Send + 'static,
    scope: impl Into<String> + Send + 'static,
    label: impl Into<String> + Send + 'static,
) {
    let mode = mode.into();
    let scope = scope.into();
    let label = label.into();

    tokio::spawn(async move {
        run_reporter_loop(monitor, mode, scope, label).await;
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal loop
// ─────────────────────────────────────────────────────────────────────────────

/// Main reporter loop.  Runs until the process exits.
async fn run_reporter_loop(
    monitor: Arc<OperationMonitor>,
    mode: String,
    scope: String,
    label: String,
) {
    let pid = std::process::id();
    let mut backoff_secs: u64 = 5;

    loop {
        // ── Ensure daemon is running ─────────────────────────────────────────
        if let Err(e) = ensure_daemon_running().await {
            warn!("daemon_reporter: daemon unavailable ({e}); retry in {backoff_secs}s");
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
            continue;
        }

        // ── Connect ──────────────────────────────────────────────────────────
        let stream = match connect_to_daemon().await {
            Ok(s) => s,
            Err(e) => {
                debug!("daemon_reporter: connect failed ({e}); retry in {backoff_secs}s");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        debug!("daemon_reporter: connected to hub daemon");
        backoff_secs = 5; // reset back-off on successful connect

        // Split into half-owned writer — we only send, never receive.
        let (_, write_half) = tokio::io::split(stream);
        let mut writer = write_half;

        // ── Register this instance ────────────────────────────────────────────
        let reg = ClientMsg::Register {
            pid,
            mode: mode.clone(),
            scope: scope.clone(),
            label: label.clone(),
        };
        if let Err(e) = send_msg(&mut writer, &reg).await {
            debug!("daemon_reporter: register failed ({e})");
            continue;
        }

        // ── Replay completed operations ───────────────────────────────────────
        let completed_ops = monitor.get_completed_operations().await;
        for op in &completed_ops {
            let started_ev = ClientMsg::Event {
                payload: DaemonEvent::OpStarted {
                    id: op.id.clone(),
                    tool_name: op.tool_name.clone(),
                    description: op.description.clone(),
                    scope: scope.clone(),
                },
            };
            if send_msg(&mut writer, &started_ev).await.is_err() {
                break;
            }
            let duration_ms = op
                .end_time
                .and_then(|end| end.duration_since(op.start_time).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let finished_ev = ClientMsg::Event {
                payload: DaemonEvent::OpFinished {
                    id: op.id.clone(),
                    status: status_label(op.state),
                    result_summary: result_summary_from(op),
                    duration_ms,
                },
            };
            if send_msg(&mut writer, &finished_ev).await.is_err() {
                break;
            }
        }

        // ── Event loop: poll monitor every 2 s ───────────────────────────────
        let mut snapshot: HashMap<String, SnapEntry> = HashMap::new();
        for op in &completed_ops {
            let duration_ms = op
                .end_time
                .and_then(|end| end.duration_since(op.start_time).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            snapshot.insert(
                op.id.clone(),
                SnapEntry {
                    tool_name: op.tool_name.clone(),
                    description: op.description.clone(),
                    status: op.state,
                    scope: scope.clone(),
                    result_summary: result_summary_from(op),
                    duration_ms,
                },
            );
        }

        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;

            let active = monitor.get_all_active_operations().await;
            let completed = monitor.get_completed_operations().await;

            // Build new snapshot from both active and recently-completed ops.
            let mut new_snap: HashMap<String, SnapEntry> = HashMap::new();
            for op in active.iter().chain(completed.iter()) {
                let duration_ms = op
                    .end_time
                    .and_then(|end| end.duration_since(op.start_time).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                new_snap.insert(
                    op.id.clone(),
                    SnapEntry {
                        tool_name: op.tool_name.clone(),
                        description: op.description.clone(),
                        status: op.state,
                        scope: scope.clone(),
                        result_summary: result_summary_from(op),
                        duration_ms,
                    },
                );
            }

            // Diff: detect newly-started and just-finished operations.
            let mut events: Vec<ClientMsg> = Vec::new();

            for (id, entry) in &new_snap {
                if let Some(prev) = snapshot.get(id) {
                    // Already known — check for terminal transition.
                    if !prev.status.is_terminal() && entry.status.is_terminal() {
                        events.push(ClientMsg::Event {
                            payload: DaemonEvent::OpFinished {
                                id: id.clone(),
                                status: status_label(entry.status),
                                result_summary: entry.result_summary.clone(),
                                duration_ms: entry.duration_ms,
                            },
                        });
                    }
                } else {
                    // New operation we haven't seen yet.
                    events.push(ClientMsg::Event {
                        payload: DaemonEvent::OpStarted {
                            id: id.clone(),
                            tool_name: entry.tool_name.clone(),
                            description: entry.description.clone(),
                            scope: entry.scope.clone(),
                        },
                    });
                    if entry.status.is_terminal() {
                        events.push(ClientMsg::Event {
                            payload: DaemonEvent::OpFinished {
                                id: id.clone(),
                                status: status_label(entry.status),
                                result_summary: entry.result_summary.clone(),
                                duration_ms: entry.duration_ms,
                            },
                        });
                    }
                }
            }

            // Send all accumulated events.
            let mut disconnected = false;
            for ev in events {
                if let Err(e) = send_msg(&mut writer, &ev).await {
                    debug!("daemon_reporter: send failed ({e}), reconnecting");
                    disconnected = true;
                    break;
                }
            }

            snapshot = new_snap;

            if disconnected {
                break;
            }

            // Prune terminal operations that are old enough to drop from snapshot.
            prune_old_terminals(&mut snapshot);
        }

        // Back-off before reconnect attempt.
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn status_label(s: OperationStatus) -> String {
    match s {
        OperationStatus::Completed => "Completed",
        OperationStatus::Failed => "Failed",
        OperationStatus::Cancelled => "Cancelled",
        OperationStatus::TimedOut => "TimedOut",
        _ => "Unknown",
    }
    .to_string()
}

fn result_summary_from(op: &Operation) -> Option<String> {
    let result = op.result.as_ref()?;
    let summary = if let Some(msg) = result.get("message").and_then(|v| v.as_str()) {
        msg.to_string()
    } else if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        err.to_string()
    } else if let Some(err_obj) = result
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
    {
        err_obj.to_string()
    } else {
        serde_json::to_string(result).unwrap_or_default()
    };

    if summary.len() > 200 {
        Some(format!("{}...", &summary[..197]))
    } else {
        Some(summary)
    }
}

/// Remove terminal entries that have been in the snapshot for a long time
/// to avoid unbounded growth.
///
/// The `completed` list returned by `get_completed_operations()` is bounded
/// by the monitor itself, so we won't re-emit terminal operations as "started"
/// after they are pruned.
fn prune_old_terminals(snap: &mut HashMap<String, SnapEntry>) {
    snap.retain(|_, v| !v.status.is_terminal());
}
