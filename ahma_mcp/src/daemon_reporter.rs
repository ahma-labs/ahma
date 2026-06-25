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
use ahma_common::config::settings_path;
use ahma_common::daemon_hub::{
    ClientMsg, DaemonEvent, DaemonMsg, connect_to_daemon, ensure_daemon_running, recv_msg, send_msg,
};
use ahma_common::scope_grant::{
    GrantCoordinator, GrantResolveOutcome, ScopeGrantRequest, persist_grant,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

/// The shared scope-grant plumbing handed to the reporter. The `coordinator` is
/// the same instance the [`crate::sandbox::HubGrantNotifier`] uses, so a request it
/// emits and the answer routed back here resolve against one coordinator (dedup,
/// first-answer-wins, dismiss). `req_rx` receives fresh requests to forward to the
/// hub as [`ClientMsg::ScopeGrantRequested`].
pub struct GrantReporting {
    /// Resolves answers and persists approved grants.
    pub coordinator: Arc<GrantCoordinator>,
    /// Stream of fresh requests to forward to the hub.
    pub req_rx: UnboundedReceiver<ScopeGrantRequest>,
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
    grant: Option<GrantReporting>,
) {
    let mode = mode.into();
    let scope = scope.into();
    let label = label.into();

    tokio::spawn(async move {
        run_reporter_loop(monitor, mode, scope, label, grant).await;
    });
}

/// Await the next grant request, or pend forever when there is no receiver — the
/// idiom for an optional `tokio::select!` branch.
async fn recv_optional_grant(
    rx: Option<&mut UnboundedReceiver<ScopeGrantRequest>>,
) -> Option<ScopeGrantRequest> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Persist an approved grant to `~/.ahma/settings.toml` — never the live session
/// (SPEC R5.4.7). Stamps today's date and records the requesting tool as
/// `granted_by` provenance. Best-effort: a write failure is logged, not fatal.
fn persist_resolved_grant(
    path: &std::path::Path,
    access: ahma_common::config::ScopeAccess,
    tool: Option<String>,
) {
    let granted_at = chrono::Local::now().format("%Y-%m-%d").to_string();
    let granted_by = tool.or_else(|| Some("scope-grant prompt".to_string()));
    match settings_path() {
        Some(file) => {
            match persist_grant(&file, path, access, granted_by, Some(granted_at), None) {
                Ok(_) => tracing::info!(
                    path = %path.display(),
                    access = access.label(),
                    "scope grant approved and persisted; restart the bridge (the `restart` \
                     tool) to apply it now, otherwise it takes effect on the next server start"
                ),
                Err(e) => warn!(
                    "daemon_reporter: failed to persist scope grant for {}: {e:#}",
                    path.display()
                ),
            }
        }
        None => warn!("daemon_reporter: cannot persist scope grant (home directory unknown)"),
    }
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
    grant: Option<GrantReporting>,
) {
    let pid = std::process::id();
    let mut backoff_secs: u64 = 1;

    // Split the grant plumbing: the coordinator is an `Arc` (cheap to clone into
    // each handler), while the request receiver is the single `&mut`-borrowed
    // resource in the select. Keeping them separate avoids a double-mutable-borrow
    // of one struct across two select branches.
    let grant_coordinator = grant.as_ref().map(|g| g.coordinator.clone());
    let mut grant_req_rx = grant.map(|g| g.req_rx);

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

                // 2b. Fresh scope-grant requests to forward to the hub.
                maybe_req = recv_optional_grant(grant_req_rx.as_mut()) => {
                    match maybe_req {
                        Some(req) => {
                            if send_msg(&mut writer, &ClientMsg::ScopeGrantRequested { request: req }).await.is_err() {
                                debug!("daemon_reporter: send ScopeGrantRequested failed, reconnecting");
                                closed = true;
                            }
                        }
                        // Sender dropped — stop polling a dead receiver.
                        None => grant_req_rx = None,
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
                            info!(
                                provider = ?provider,
                                model = ?model,
                                messages = messages.len(),
                                "daemon_reporter: RunPrompt received"
                            );
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
                        Ok(DaemonMsg::SubmitScopeGrant { decision_id, decision }) => {
                            debug!("daemon_reporter: received SubmitScopeGrant id={decision_id} decision={decision:?}");
                            if let Some(coord) = &grant_coordinator {
                                match coord.resolve(&decision_id, decision) {
                                    GrantResolveOutcome::Persist { path, access, tool } => {
                                        persist_resolved_grant(&path, access, tool);
                                        // Dismiss any twin modal on other TUIs.
                                        let _ = send_msg(&mut writer, &ClientMsg::ScopeGrantResolved { decision_id }).await;
                                    }
                                    GrantResolveOutcome::Denied { .. } => {
                                        let _ = send_msg(&mut writer, &ClientMsg::ScopeGrantResolved { decision_id }).await;
                                    }
                                    GrantResolveOutcome::AlreadyResolved | GrantResolveOutcome::Unknown => {}
                                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_monitor::{Operation, OperationStatus};
    use ahma_common::daemon_hub::DaemonEvent;
    use ahma_common::event_dispatcher::OperationEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    // ── daemon_event_for ──────────────────────────────────────────────────────

    #[test]
    fn daemon_event_for_started() {
        let ev = OperationEvent::Started {
            operation_id: "op-1".into(),
            tool_name: "cargo_build".into(),
            description: "Build release".into(),
        };
        let result = daemon_event_for(&ev, "workspace/root");
        let Some(DaemonEvent::OpStarted {
            id,
            tool_name,
            description,
            scope,
        }) = result
        else {
            panic!("expected OpStarted, got {result:?}");
        };
        assert_eq!(id, "op-1");
        assert_eq!(tool_name, "cargo_build");
        assert_eq!(description, "Build release");
        assert_eq!(scope, "workspace/root");
    }

    #[test]
    fn daemon_event_for_output_line() {
        let ev = OperationEvent::OutputLine {
            operation_id: "op-2".into(),
            line: "hello stdout".into(),
            is_stderr: false,
        };
        let result = daemon_event_for(&ev, "ws");
        let Some(DaemonEvent::OpOutput {
            id,
            line,
            is_stderr,
        }) = result
        else {
            panic!("expected OpOutput, got {result:?}");
        };
        assert_eq!(id, "op-2");
        assert_eq!(line, "hello stdout");
        assert!(!is_stderr);
    }

    #[test]
    fn daemon_event_for_output_line_stderr() {
        let ev = OperationEvent::OutputLine {
            operation_id: "op-3".into(),
            line: "err msg".into(),
            is_stderr: true,
        };
        let Some(DaemonEvent::OpOutput { is_stderr, .. }) = daemon_event_for(&ev, "ws") else {
            panic!("expected OpOutput");
        };
        assert!(is_stderr);
    }

    #[test]
    fn daemon_event_for_alert() {
        let ev = OperationEvent::Alert {
            operation_id: "op-4".into(),
            message: "disk full".into(),
        };
        let Some(DaemonEvent::LogLine { level, message }) = daemon_event_for(&ev, "ws") else {
            panic!("expected LogLine");
        };
        assert_eq!(level, "alert");
        assert!(message.contains("op-4"));
        assert!(message.contains("disk full"));
    }

    #[test]
    fn daemon_event_for_completed() {
        let ev = OperationEvent::Completed {
            operation_id: "op-5".into(),
            result: json!({ "message": "ok" }),
            duration_ms: 42,
        };
        let Some(DaemonEvent::OpFinished {
            id,
            status,
            duration_ms,
            result_summary,
        }) = daemon_event_for(&ev, "ws")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(id, "op-5");
        assert_eq!(status, "Completed");
        assert_eq!(duration_ms, 42);
        assert_eq!(result_summary, Some("ok".into()));
    }

    #[test]
    fn daemon_event_for_failed() {
        let ev = OperationEvent::Failed {
            operation_id: "op-6".into(),
            error: "permission denied".into(),
            duration_ms: 10,
        };
        let Some(DaemonEvent::OpFinished {
            status,
            result_summary,
            ..
        }) = daemon_event_for(&ev, "ws")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(status, "Failed");
        assert_eq!(result_summary, Some("permission denied".into()));
    }

    #[test]
    fn daemon_event_for_cancelled() {
        let ev = OperationEvent::Cancelled {
            operation_id: "op-7".into(),
            reason: "user cancelled".into(),
            duration_ms: 5,
        };
        let Some(DaemonEvent::OpFinished {
            status,
            result_summary,
            ..
        }) = daemon_event_for(&ev, "ws")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(status, "Cancelled");
        assert_eq!(result_summary, Some("user cancelled".into()));
    }

    #[test]
    fn daemon_event_for_timed_out() {
        let ev = OperationEvent::TimedOut {
            operation_id: "op-8".into(),
            duration_ms: 30_000,
        };
        let Some(DaemonEvent::OpFinished {
            status,
            result_summary,
            duration_ms,
            ..
        }) = daemon_event_for(&ev, "ws")
        else {
            panic!("expected OpFinished");
        };
        assert_eq!(status, "TimedOut");
        assert_eq!(result_summary, Some("operation timed out".into()));
        assert_eq!(duration_ms, 30_000);
    }

    #[test]
    fn daemon_event_for_progress_is_none() {
        let ev = OperationEvent::Progress {
            operation_id: "op-9".into(),
            message: "50%".into(),
            percent: Some(0.5),
        };
        assert!(
            daemon_event_for(&ev, "ws").is_none(),
            "Progress should map to None"
        );
    }

    #[test]
    fn daemon_event_for_mcp_notification_is_none() {
        let ev = OperationEvent::McpNotification {
            operation_id: "op-10".into(),
            method: "notifications/message".into(),
            params: None,
        };
        assert!(
            daemon_event_for(&ev, "ws").is_none(),
            "McpNotification should map to None"
        );
    }

    // ── clip_summary ──────────────────────────────────────────────────────────

    #[test]
    fn clip_summary_short_unchanged() {
        let s = "hello world".to_string();
        assert_eq!(clip_summary(s.clone()), s);
    }

    #[test]
    fn clip_summary_exactly_200_unchanged() {
        let s = "x".repeat(200);
        let result = clip_summary(s.clone());
        assert_eq!(result, s, "200-char string must not be clipped");
    }

    #[test]
    fn clip_summary_over_200_appends_ellipsis() {
        let s = "a".repeat(250);
        let result = clip_summary(s);
        assert!(result.ends_with("..."), "should end with ...");
        assert!(
            result.len() <= 201,
            "clipped + '...' should stay short (got {})",
            result.len()
        );
    }

    #[test]
    fn clip_summary_unicode_does_not_split_char() {
        // '€' is 3 bytes. Place it so its start byte is before 197 but its end
        // byte would be past 197, verifying that the function never slices mid-char.
        let prefix = "x".repeat(196);
        let suffix = "€".repeat(20); // 3 bytes each
        let s = format!("{prefix}{suffix}");
        assert!(s.len() > 200);
        let result = clip_summary(s.clone());
        // Verify the result is valid UTF-8 (would panic on invalid slice)
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(result.ends_with("..."));
    }

    // ── summary_from_value ───────────────────────────────────────────────────

    #[test]
    fn summary_from_value_message_field() {
        let v = json!({ "message": "all good" });
        assert_eq!(summary_from_value(&v), Some("all good".into()));
    }

    #[test]
    fn summary_from_value_error_string_field() {
        let v = json!({ "error": "something failed" });
        assert_eq!(summary_from_value(&v), Some("something failed".into()));
    }

    #[test]
    fn summary_from_value_nested_error_message() {
        let v = json!({ "error": { "message": "nested error" } });
        assert_eq!(summary_from_value(&v), Some("nested error".into()));
    }

    #[test]
    fn summary_from_value_fallback_serializes_json() {
        let v = json!({ "code": 42 });
        let result = summary_from_value(&v).unwrap();
        // The exact serialization is platform-stable: check it's non-empty JSON.
        assert!(result.contains("42"), "fallback should serialize the value");
    }

    #[test]
    fn summary_from_value_long_message_is_clipped() {
        let long = "z".repeat(300);
        let v = json!({ "message": long });
        let result = summary_from_value(&v).unwrap();
        assert!(result.len() <= 203, "summary must be clipped");
        assert!(result.ends_with("..."));
    }

    // ── status_label ─────────────────────────────────────────────────────────

    #[test]
    fn status_label_all_variants() {
        assert_eq!(status_label(OperationStatus::Pending), "Pending");
        assert_eq!(status_label(OperationStatus::InProgress), "InProgress");
        assert_eq!(status_label(OperationStatus::Completed), "Completed");
        assert_eq!(status_label(OperationStatus::Failed), "Failed");
        assert_eq!(status_label(OperationStatus::Cancelled), "Cancelled");
        assert_eq!(status_label(OperationStatus::TimedOut), "TimedOut");
    }

    // ── result_summary_from ──────────────────────────────────────────────────

    #[test]
    fn result_summary_from_no_result_is_none() {
        let op = Operation::new("id".into(), "tool".into(), "desc".into(), None);
        assert!(result_summary_from(&op).is_none());
    }

    #[test]
    fn result_summary_from_message_field() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "message": "build succeeded" })),
        );
        assert_eq!(result_summary_from(&op), Some("build succeeded".into()));
    }

    #[test]
    fn result_summary_from_long_result_clipped() {
        let long_msg = "y".repeat(300);
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "message": long_msg })),
        );
        let result = result_summary_from(&op).unwrap();
        assert!(result.len() <= 203);
        assert!(result.ends_with("..."));
    }

    // ── recv_optional_grant ──────────────────────────────────────────────────

    #[tokio::test]
    async fn recv_optional_grant_none_is_pending() {
        // With no receiver, the future must never resolve within the timeout.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            recv_optional_grant(None),
        )
        .await;
        assert!(
            result.is_err(),
            "None receiver should remain pending indefinitely"
        );
    }

    #[tokio::test]
    async fn recv_optional_grant_some_returns_value() {
        use ahma_common::config::ScopeAccess;
        use ahma_common::scope_grant::{GrantReason, ScopeGrantRequest};

        let (tx, mut rx) = mpsc::unbounded_channel::<ScopeGrantRequest>();
        let req = ScopeGrantRequest {
            decision_id: "d-1".into(),
            path: std::path::PathBuf::from("/tmp/test"),
            access: ScopeAccess::Ro,
            reason: GrantReason::PreExecViolation,
            tool: Some("cargo_build".into()),
        };
        tx.send(req.clone()).unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            recv_optional_grant(Some(&mut rx)),
        )
        .await
        .expect("should resolve");
        let got = result.expect("should have a value");
        assert_eq!(got.decision_id, "d-1");
        assert_eq!(got.access, ScopeAccess::Ro);
    }

    #[tokio::test]
    async fn recv_optional_grant_some_closed_returns_none() {
        use ahma_common::scope_grant::ScopeGrantRequest;
        // When the sender is dropped, recv() resolves to None (channel closed).
        let (tx, mut rx) = mpsc::unbounded_channel::<ScopeGrantRequest>();
        drop(tx);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            recv_optional_grant(Some(&mut rx)),
        )
        .await
        .expect("closed channel should resolve immediately");
        assert!(result.is_none(), "closed channel yields None");
    }

    // ── daemon_event_for: clip paths in Failed / Cancelled arms ───────────────

    #[test]
    fn daemon_event_for_failed_long_error_is_clipped() {
        let ev = OperationEvent::Failed {
            operation_id: "op-f".into(),
            error: "e".repeat(300),
            duration_ms: 1,
        };
        let Some(DaemonEvent::OpFinished { result_summary, .. }) = daemon_event_for(&ev, "ws")
        else {
            panic!("expected OpFinished");
        };
        let s = result_summary.expect("summary present");
        assert!(s.ends_with("..."), "long error must be clipped");
        assert!(s.len() <= 201);
    }

    #[test]
    fn daemon_event_for_cancelled_long_reason_is_clipped() {
        let ev = OperationEvent::Cancelled {
            operation_id: "op-c".into(),
            reason: "r".repeat(300),
            duration_ms: 2,
        };
        let Some(DaemonEvent::OpFinished { result_summary, .. }) = daemon_event_for(&ev, "ws")
        else {
            panic!("expected OpFinished");
        };
        let s = result_summary.expect("summary present");
        assert!(s.ends_with("..."));
    }

    // ── summary_from_value: message field is non-string falls through ─────────

    #[test]
    fn summary_from_value_non_string_message_falls_through() {
        // `message` exists but is not a string → as_str() is None → fall to error,
        // then nested, then JSON fallback.
        let v = json!({ "message": 7 });
        let result = summary_from_value(&v).expect("fallback summary");
        assert!(
            result.contains('7') && result.contains("message"),
            "should serialize whole value as fallback, got {result}"
        );
    }

    // ── result_summary_from: error-string and nested-error branches ───────────

    #[test]
    fn result_summary_from_error_string_branch() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "error": "boom" })),
        );
        assert_eq!(result_summary_from(&op), Some("boom".into()));
    }

    #[test]
    fn result_summary_from_nested_error_message_branch() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "error": { "message": "deep boom" } })),
        );
        assert_eq!(result_summary_from(&op), Some("deep boom".into()));
    }

    #[test]
    fn result_summary_from_json_fallback_branch() {
        let op = Operation::new(
            "id".into(),
            "tool".into(),
            "desc".into(),
            Some(json!({ "code": 99 })),
        );
        let s = result_summary_from(&op).expect("fallback");
        assert!(s.contains("99"), "fallback serializes the JSON, got {s}");
    }

    // ── persist_resolved_grant ────────────────────────────────────────────────
    //
    // `persist_resolved_grant` resolves the settings path via `settings_path()`,
    // which derives from the home directory. On Unix the home directory is the
    // `HOME` env var, so we redirect it to a TempDir. (On Windows `dirs::home_dir`
    // uses the Known-Folder API and ignores env vars, so these write-path tests
    // are genuinely Unix-only.)
    #[cfg(unix)]
    static HOME_ENV_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    #[cfg(unix)]
    fn with_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let _guard = HOME_ENV_MUTEX.lock().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let out = f();
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn persist_resolved_grant_writes_settings_with_tool_provenance() {
        use ahma_common::config::ScopeAccess;
        let home = tempfile::tempdir().unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        let settings = home.path().join(".ahma").join("settings.toml");

        with_home(home.path(), || {
            persist_resolved_grant(
                grant_dir.path(),
                ScopeAccess::Rw,
                Some("sccache".to_string()),
            );
        });

        assert!(settings.exists(), "settings.toml must be created");
        let contents = std::fs::read_to_string(&settings).unwrap();
        assert!(
            contents.contains(&grant_dir.path().display().to_string()),
            "granted path must be recorded, got:\n{contents}"
        );
        assert!(
            contents.contains("sccache"),
            "granted_by provenance from tool must be recorded, got:\n{contents}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persist_resolved_grant_defaults_granted_by_when_no_tool() {
        use ahma_common::config::ScopeAccess;
        let home = tempfile::tempdir().unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        let settings = home.path().join(".ahma").join("settings.toml");

        with_home(home.path(), || {
            persist_resolved_grant(grant_dir.path(), ScopeAccess::Ro, None);
        });

        let contents = std::fs::read_to_string(&settings).unwrap();
        assert!(
            contents.contains("scope-grant prompt"),
            "granted_by must default to the prompt label, got:\n{contents}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persist_resolved_grant_corrupt_settings_is_non_fatal_and_preserved() {
        use ahma_common::config::ScopeAccess;
        let home = tempfile::tempdir().unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        let ahma_dir = home.path().join(".ahma");
        std::fs::create_dir_all(&ahma_dir).unwrap();
        let settings = ahma_dir.join("settings.toml");
        // Invalid TOML so the strict loader errors → persist_grant returns Err →
        // persist_resolved_grant logs a warning and does NOT overwrite the file.
        let corrupt = "this = is = not valid toml {{{";
        std::fs::write(&settings, corrupt).unwrap();

        with_home(home.path(), || {
            // Must not panic even though persistence fails.
            persist_resolved_grant(grant_dir.path(), ScopeAccess::Rw, Some("t".to_string()));
        });

        let after = std::fs::read_to_string(&settings).unwrap();
        assert_eq!(
            after, corrupt,
            "corrupt settings file must be left untouched on the error path"
        );
    }
}
