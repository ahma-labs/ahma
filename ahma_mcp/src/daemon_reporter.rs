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

use crate::mcp_service::{ActiveAgentSession, get_global_prompt_runner};
use crate::operation_monitor::{Operation, OperationMonitor, OperationStatus};
use ahma_common::daemon_hub::{
    ClientMsg, DaemonEvent, DaemonMsg, connect_to_daemon, ensure_daemon_running, recv_msg, send_msg,
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

        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = tokio::io::BufReader::new(read_half);
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

        // Channels for outbound messages and agent active session
        let (hub_tx, mut hub_rx) = tokio::sync::mpsc::channel::<ClientMsg>(100);
        let session = Arc::new(tokio::sync::Mutex::new(ActiveAgentSession::default()));

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

        // ── Event loop: forward the unified operation event stream ───────────
        let mut closed = false;
        while !closed {
            tokio::select! {
                biased;

                // 1. Unified operation events to forward
                event_res = event_rx.recv() => {
                    match event_res {
                        Ok(event) => {
                            let Some(payload) = daemon_event_for(&event, &scope) else {
                                continue;
                            };
                            let client_msg = ClientMsg::Event { payload };

                            if let Err(e) = send_msg(&mut writer, &client_msg).await {
                                debug!("daemon_reporter: send failed ({e}), reconnecting");
                                closed = true;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("daemon_reporter event queue lagged by {n} messages; continuing");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            closed = true;
                        }
                    }
                }

                // 2. Outbound client messages from Agent Runner
                hub_msg = hub_rx.recv() => {
                    if let Some(msg) = hub_msg {
                        if let Err(e) = send_msg(&mut writer, &msg).await {
                            debug!("daemon_reporter: send hub_msg failed ({e}), reconnecting");
                            closed = true;
                        }
                    } else {
                        closed = true;
                    }
                }

                // 3. Incoming messages from daemon hub
                daemon_msg = recv_msg::<_, DaemonMsg>(&mut reader) => {
                    match daemon_msg {
                        Ok(DaemonMsg::Ping { seq }) => {
                            debug!("daemon_reporter: received ping seq={seq}");
                            if let Err(e) = send_msg(&mut writer, &ClientMsg::Pong { seq }).await {
                                debug!("daemon_reporter: pong send failed ({e}), reconnecting");
                                closed = true;
                            }
                        }
                        Ok(DaemonMsg::RunPrompt { messages, system_prompt, provider, model }) => {
                            debug!("daemon_reporter: received RunPrompt");
                            if let Some(runner) = get_global_prompt_runner() {
                                let runner = runner.clone();
                                let hub_tx = hub_tx.clone();
                                let session = session.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = runner.run_prompt(messages, system_prompt, provider, model, hub_tx.clone(), session).await {
                                        let _ = hub_tx.send(ClientMsg::AgentError { error: e }).await;
                                    } else {
                                        let _ = hub_tx.send(ClientMsg::AgentDone).await;
                                    }
                                });
                            } else {
                                warn!("daemon_reporter: RunPrompt received but no prompt runner is registered");
                                let _ = hub_tx.send(ClientMsg::AgentError {
                                    error: "No prompt runner registered on this instance".to_string()
                                }).await;
                            }
                        }
                        Ok(DaemonMsg::SubmitApproval { approved }) => {
                            debug!("daemon_reporter: received SubmitApproval approved={approved}");
                            let mut session_guard = session.lock().await;
                            if let Some(tx) = session_guard.approval_tx.take() {
                                let _ = tx.send(approved);
                            } else {
                                debug!("daemon_reporter: received SubmitApproval but no approval sender pending");
                            }
                        }
                        Ok(msg) => {
                            debug!("daemon_reporter: ignored unexpected DaemonMsg: {:?}", msg);
                        }
                        Err(e) => {
                            debug!("daemon_reporter: read error or EOF ({e}), reconnecting");
                            closed = true;
                        }
                    }
                }
            }
        }

        // Back-off before reconnect attempt.
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Map a unified [`OperationEvent`] to the hub wire event, or `None` for
/// events the hub does not carry (Progress, McpNotification).
fn daemon_event_for(
    event: &ahma_common::event_dispatcher::OperationEvent,
    scope: &str,
) -> Option<DaemonEvent> {
    use ahma_common::event_dispatcher::OperationEvent as Ev;
    Some(match event {
        Ev::Started {
            operation_id,
            tool_name,
            description,
        } => DaemonEvent::OpStarted {
            id: operation_id.clone(),
            tool_name: tool_name.clone(),
            description: description.clone(),
            scope: scope.to_string(),
        },
        Ev::OutputLine {
            operation_id,
            line,
            is_stderr,
        } => DaemonEvent::OpOutput {
            id: operation_id.clone(),
            line: line.clone(),
            is_stderr: *is_stderr,
        },
        Ev::Alert {
            operation_id,
            message,
        } => DaemonEvent::LogLine {
            level: "alert".to_string(),
            message: format!("{operation_id}: {message}"),
        },
        Ev::Completed {
            operation_id,
            result,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: "Completed".to_string(),
            result_summary: summary_from_value(result),
            duration_ms: *duration_ms,
        },
        Ev::Failed {
            operation_id,
            error,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: "Failed".to_string(),
            result_summary: Some(clip_summary(error.clone())),
            duration_ms: *duration_ms,
        },
        Ev::Cancelled {
            operation_id,
            reason,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: "Cancelled".to_string(),
            result_summary: Some(clip_summary(reason.clone())),
            duration_ms: *duration_ms,
        },
        Ev::TimedOut {
            operation_id,
            duration_ms,
        } => DaemonEvent::OpFinished {
            id: operation_id.clone(),
            status: "TimedOut".to_string(),
            result_summary: Some("operation timed out".to_string()),
            duration_ms: *duration_ms,
        },
        _ => return None,
    })
}

/// Extract a short human-readable summary from a result JSON value.
fn summary_from_value(result: &serde_json::Value) -> Option<String> {
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
    Some(clip_summary(summary))
}

/// Clip a summary string to a wire-friendly length.
fn clip_summary(summary: String) -> String {
    if summary.len() > 200 {
        let cut = summary
            .char_indices()
            .take_while(|(i, _)| *i <= 197)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &summary[..cut])
    } else {
        summary
    }
}

fn status_label(s: OperationStatus) -> String {
    match s {
        OperationStatus::Pending => "Pending",
        OperationStatus::InProgress => "InProgress",
        OperationStatus::Completed => "Completed",
        OperationStatus::Failed => "Failed",
        OperationStatus::Cancelled => "Cancelled",
        OperationStatus::TimedOut => "TimedOut",
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
