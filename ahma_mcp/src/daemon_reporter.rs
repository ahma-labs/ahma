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
use std::{sync::Arc, time::Duration};
use tracing::{debug, warn};

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
    let mut backoff_secs: u64 = 1;

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
        backoff_secs = 1; // reset back-off on successful connect

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

        // Subscribe to events BEFORE replaying so we don't miss anything that starts
        // while we are replaying the initial snapshot.
        let mut event_rx = monitor.subscribe_events();

        // ── Replay completed operations ───────────────────────────────────────
        let completed_ops = monitor.get_completed_operations().await;
        let mut replayed_completed = false;
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
                replayed_completed = true;
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
                replayed_completed = true;
                break;
            }
        }
        if replayed_completed {
            continue;
        }

        // ── Replay active operations ──────────────────────────────────────────
        let active_ops = monitor.get_all_active_operations().await;
        let mut replayed_active = false;
        for op in &active_ops {
            let started_ev = ClientMsg::Event {
                payload: DaemonEvent::OpStarted {
                    id: op.id.clone(),
                    tool_name: op.tool_name.clone(),
                    description: op.description.clone(),
                    scope: scope.clone(),
                },
            };
            if send_msg(&mut writer, &started_ev).await.is_err() {
                replayed_active = true;
                break;
            }
        }
        if replayed_active {
            continue;
        }

        // ── Event loop: listen for OperationMonitor events ────────────────────
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let client_msg = match event {
                        crate::operation_monitor::OperationEvent::Started(op) => ClientMsg::Event {
                            payload: DaemonEvent::OpStarted {
                                id: op.id,
                                tool_name: op.tool_name,
                                description: op.description,
                                scope: scope.clone(),
                            },
                        },
                        crate::operation_monitor::OperationEvent::Updated(op) => {
                            if !op.state.is_terminal() {
                                continue;
                            }
                            let duration_ms = op
                                .end_time
                                .and_then(|end| end.duration_since(op.start_time).ok())
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            let result_summary = result_summary_from(&op);
                            ClientMsg::Event {
                                payload: DaemonEvent::OpFinished {
                                    id: op.id.clone(),
                                    status: status_label(op.state),
                                    result_summary,
                                    duration_ms,
                                },
                            }
                        }
                    };

                    if let Err(e) = send_msg(&mut writer, &client_msg).await {
                        debug!("daemon_reporter: send failed ({e}), reconnecting");
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("daemon_reporter event queue lagged by {n} messages; continuing");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
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
