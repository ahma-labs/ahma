//! MCP progress push — forwards unified operation events to the connected
//! client as `notifications/progress`.
//!
//! Replaces the legacy per-operation `CallbackSender` chain: tool handlers
//! register a [`PushTarget`] (peer + client progress token) for an operation
//! id, and a single forwarder task subscribed to the `OperationMonitor` event
//! stream translates every event for a registered operation into an MCP
//! progress notification.
//!
//! Push is strictly best-effort: terminal results are stored in the
//! `OperationMonitor` and retrievable via the `await` tool, so a failed push
//! is logged but never an error.  Notifications are only sent when the client
//! provided a `progressToken` in the request `_meta` AND the client type is
//! known to handle progress notifications (Cursor, for example, logs errors
//! for them even with valid tokens).

use std::collections::HashMap;
use std::sync::Arc;

use ahma_common::event_dispatcher::OperationEvent;
use rmcp::{
    model::{ProgressNotificationParam, ProgressToken},
    service::{Peer, RoleServer},
};

use crate::client_type::McpClientType;
use crate::operation_monitor::OperationMonitor;

/// Destination for progress notifications of one operation.
#[derive(Clone)]
pub struct PushTarget {
    pub peer: Peer<RoleServer>,
    pub progress_token: ProgressToken,
}

/// Routes operation events to the MCP client that started each operation.
///
/// Cheap to share — handlers hold an `Arc` and register/unregister targets;
/// the forwarder task spawned by [`ProgressPushRouter::spawn_forwarder`]
/// consumes the unified event stream.
#[derive(Default)]
pub struct ProgressPushRouter {
    targets: tokio::sync::RwLock<HashMap<String, PushTarget>>,
}

impl ProgressPushRouter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register the client destination for an operation.
    ///
    /// No-op when the client does not handle progress notifications —
    /// matching the legacy `McpCallbackSender` behaviour.
    pub async fn register(
        &self,
        op_id: &str,
        peer: Peer<RoleServer>,
        progress_token: ProgressToken,
        client_type: McpClientType,
    ) {
        if !client_type.supports_progress() {
            tracing::trace!(
                "Skipping progress registration for {} client",
                client_type.display_name()
            );
            return;
        }
        self.targets.write().await.insert(
            op_id.to_string(),
            PushTarget {
                peer,
                progress_token,
            },
        );
    }

    pub async fn unregister(&self, op_id: &str) {
        self.targets.write().await.remove(op_id);
    }

    #[cfg(test)]
    pub async fn target_count(&self) -> usize {
        self.targets.read().await.len()
    }

    /// Spawn the forwarder task that pushes progress notifications for all
    /// registered operations until the monitor's event stream closes.
    pub fn spawn_forwarder(self: &Arc<Self>, monitor: &Arc<OperationMonitor>) {
        let router = Arc::clone(self);
        let mut rx = monitor.subscribe_events();
        tokio::spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Push is a live feed, not the store of record — the
                        // client can always reconcile via `await`/`status`.
                        tracing::debug!("progress push fell behind by {n} events (best-effort)");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };

                let op_id = event.operation_id().to_string();
                let target = { router.targets.read().await.get(&op_id).cloned() };
                if let Some(target) = target {
                    if let Some((progress, message)) = progress_for_event(&event) {
                        push_progress(
                            &target.peer,
                            target.progress_token.clone(),
                            progress,
                            message,
                            event.is_terminal(),
                        )
                        .await;
                    }
                    if event.is_terminal() {
                        router.unregister(&op_id).await;
                    }
                }
            }
        });
    }
}

/// Map a unified operation event to `(progress, message)` for an MCP progress
/// notification.  Returns `None` for events that are not pushed — per-line
/// output is far too chatty for `notifications/progress`.
pub fn progress_for_event(event: &OperationEvent) -> Option<(f64, String)> {
    match event {
        OperationEvent::Started {
            tool_name,
            description,
            ..
        } => Some((0.0, format!("{tool_name}: {description}"))),
        OperationEvent::Progress {
            message, percent, ..
        } => Some((percent.map(f64::from).unwrap_or(50.0), message.clone())),
        // Distinctive mid-range progress signals "alert, not completion".
        OperationEvent::Alert { message, .. } => Some((50.0, message.clone())),
        OperationEvent::OutputLine { .. } => None,
        OperationEvent::Completed {
            operation_id,
            result,
            duration_ms,
        } => Some((
            100.0,
            final_result_message(operation_id, true, *duration_ms, result),
        )),
        OperationEvent::Failed {
            operation_id,
            error,
            duration_ms,
        } => Some((
            100.0,
            format!(
                "OPERATION FAILED: '{operation_id}'\nDuration: {duration_ms}ms\n\nError: {error}"
            ),
        )),
        OperationEvent::Cancelled {
            operation_id,
            reason,
            duration_ms,
        } => Some((
            100.0,
            format!(
                "CANCELLED after {duration_ms}ms: {}",
                crate::utils::cancellation::format_cancellation_message(
                    reason,
                    None,
                    Some(operation_id)
                )
            ),
        )),
        OperationEvent::TimedOut {
            operation_id,
            duration_ms,
        } => Some((
            100.0,
            format!("Operation '{operation_id}' timed out after {duration_ms}ms"),
        )),
        OperationEvent::McpNotification { .. } => None,
        // `OperationEvent` is #[non_exhaustive]; ignore future variants.
        _ => None,
    }
}

/// Render a terminal result `Value` (the monitor's stored result) into the
/// human-readable final message pushed to the client.
fn final_result_message(
    op_id: &str,
    success: bool,
    duration_ms: u64,
    result: &serde_json::Value,
) -> String {
    let status = if success { "COMPLETED" } else { "FAILED" };
    let body = match result {
        serde_json::Value::Object(map) => {
            let mut s = String::new();
            if let Some(code) = map.get("exit_code").and_then(|v| v.as_i64()) {
                s.push_str(&format!("Exit code: {code}\n"));
            }
            let stdout = map.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = map.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            s.push_str(&format!("Stdout:\n{stdout}\nStderr:\n{stderr}"));
            if let Some(f) = map.get("output_file").and_then(|v| v.as_str()) {
                s.push_str(&format!("\nFull output: {f}"));
            }
            s
        }
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };
    format!(
        "OPERATION {status}: '{op_id}'\nDuration: {duration_ms}ms\n\n=== FULL OUTPUT ===\n{body}"
    )
}

/// Build the final message for a synchronous tool call (no monitor event —
/// the sync path pushes directly).  Mirrors the legacy `FinalResult` format.
pub fn sync_final_message(
    id: &str,
    command: &str,
    description: &str,
    working_directory: &str,
    success: bool,
    duration_ms: u64,
    full_output: &str,
) -> String {
    let status = if success { "COMPLETED" } else { "FAILED" };
    format!(
        "OPERATION {status}: '{id}'\nCommand: {command}\nDescription: {description}\nWorking Directory: {working_directory}\nDuration: {duration_ms}ms\n\n=== FULL OUTPUT ===\n{full_output}"
    )
}

/// Push a single best-effort progress notification.
///
/// `must_deliver` only affects the log level on failure: terminal results are
/// stored in the `OperationMonitor` and retrievable via `await`, so a push
/// failure is never fatal.
pub async fn push_progress(
    peer: &Peer<RoleServer>,
    progress_token: ProgressToken,
    progress: f64,
    message: String,
    must_deliver: bool,
) {
    let params = ProgressNotificationParam {
        progress_token,
        progress,
        total: Some(100.0),
        message: Some(message),
    };
    if let Err(e) = peer.notify_progress(params).await {
        if must_deliver {
            tracing::warn!(
                "Failed to push terminal-state notification (result is still \
                 available via the `await` tool): {e:?}"
            );
        } else {
            tracing::debug!("Failed to push best-effort progress notification (non-fatal): {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn started_maps_to_zero_progress() {
        let (progress, message) = progress_for_event(&OperationEvent::Started {
            operation_id: "op-1".into(),
            tool_name: "cargo_build".into(),
            description: "Building".into(),
            parent_id: None,
        })
        .unwrap();
        assert_eq!(progress, 0.0);
        assert!(message.contains("cargo_build"));
        assert!(message.contains("Building"));
    }

    #[test]
    fn output_lines_are_not_pushed() {
        assert!(
            progress_for_event(&OperationEvent::OutputLine {
                operation_id: "op-1".into(),
                line: "compiling...".into(),
                is_stderr: false,
            })
            .is_none()
        );
    }

    #[test]
    fn progress_uses_percent_when_available() {
        let (progress, _) = progress_for_event(&OperationEvent::Progress {
            operation_id: "op-1".into(),
            message: "halfway-ish".into(),
            percent: Some(75.0),
        })
        .unwrap();
        assert_eq!(progress, 75.0);

        let (progress, _) = progress_for_event(&OperationEvent::Progress {
            operation_id: "op-1".into(),
            message: "unknown".into(),
            percent: None,
        })
        .unwrap();
        assert_eq!(progress, 50.0);
    }

    #[test]
    fn completed_includes_output_and_spill_path() {
        let (progress, message) = progress_for_event(&OperationEvent::Completed {
            operation_id: "op-9".into(),
            result: json!({
                "stdout": "hello",
                "stderr": "warning: x",
                "exit_code": 0,
                "output_file": "/logs/operations/op-9.log",
            }),
            duration_ms: 1234,
        })
        .unwrap();
        assert_eq!(progress, 100.0);
        assert!(message.contains("OPERATION COMPLETED: 'op-9'"));
        assert!(message.contains("Exit code: 0"));
        assert!(message.contains("hello"));
        assert!(message.contains("warning: x"));
        assert!(message.contains("/logs/operations/op-9.log"));
        assert!(message.contains("1234ms"));
    }

    #[test]
    fn failed_includes_error() {
        let (progress, message) = progress_for_event(&OperationEvent::Failed {
            operation_id: "op-2".into(),
            error: "exit code 101".into(),
            duration_ms: 10,
        })
        .unwrap();
        assert_eq!(progress, 100.0);
        assert!(message.contains("OPERATION FAILED: 'op-2'"));
        assert!(message.contains("exit code 101"));
    }

    #[test]
    fn cancelled_gets_actionable_formatting() {
        let (_, message) = progress_for_event(&OperationEvent::Cancelled {
            operation_id: "op-3".into(),
            reason: "Canceled: canceled".into(),
            duration_ms: 5,
        })
        .unwrap();
        assert!(message.contains("Operation was cancelled (source: unknown)"));
        assert!(message.contains("op-3"));
        assert!(message.contains("Suggestions"));
    }

    #[test]
    fn timed_out_reports_duration() {
        let (_, message) = progress_for_event(&OperationEvent::TimedOut {
            operation_id: "op-4".into(),
            duration_ms: 60_000,
        })
        .unwrap();
        assert!(message.contains("op-4"));
        assert!(message.contains("60000ms"));
    }

    #[test]
    fn alert_maps_to_midrange_progress() {
        let (progress, message) = progress_for_event(&OperationEvent::Alert {
            operation_id: "op-5".into(),
            message: "ERROR: panic at src/main.rs".into(),
        })
        .unwrap();
        assert_eq!(progress, 50.0);
        assert!(message.contains("panic"));
    }

    #[test]
    fn completed_with_string_result_passes_through() {
        let (_, message) = progress_for_event(&OperationEvent::Completed {
            operation_id: "op-6".into(),
            result: json!("plain text output"),
            duration_ms: 1,
        })
        .unwrap();
        assert!(message.contains("plain text output"));
    }

    #[test]
    fn sync_final_message_mirrors_legacy_format() {
        let message = sync_final_message(
            "op-7",
            "cargo",
            "Execute cargo in /work",
            "/work",
            false,
            42,
            "Error: boom",
        );
        assert!(message.contains("OPERATION FAILED: 'op-7'"));
        assert!(message.contains("Command: cargo"));
        assert!(message.contains("Working Directory: /work"));
        assert!(message.contains("=== FULL OUTPUT ===\nError: boom"));
    }
}
