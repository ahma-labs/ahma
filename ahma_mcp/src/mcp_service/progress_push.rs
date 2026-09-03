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
pub struct ProgressPushRouter {
    targets: tokio::sync::RwLock<HashMap<String, PushTarget>>,
    /// Authoritative source for "is this operation still active" (SPEC
    /// R2.5.3), consulted by [`Self::restore`] — see its docs for why a
    /// derived signal (e.g. presence in `targets`) isn't safe to use instead.
    monitor: Arc<OperationMonitor>,
}

impl ProgressPushRouter {
    pub fn new(monitor: Arc<OperationMonitor>) -> Arc<Self> {
        Arc::new(Self {
            targets: tokio::sync::RwLock::new(HashMap::new()),
            monitor,
        })
    }

    /// Register the client destination for an operation.
    ///
    /// No-op when `progress_enabled` is `false` — matching the legacy
    /// `McpCallbackSender` behaviour. The caller resolves this (normally
    /// `AhmaMcpService::effective_supports_progress`, which layers the
    /// operator's `force_progress_notifications` override over
    /// `client_type`'s default) rather than this router deciding on its own,
    /// since only the caller has access to that override.
    pub async fn register(
        &self,
        op_id: &str,
        peer: Peer<RoleServer>,
        progress_token: ProgressToken,
        client_type: McpClientType,
        progress_enabled: bool,
    ) {
        if !progress_enabled {
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

    /// Atomically hand back and remove whoever is currently registered for a
    /// terminal event, in a single lock acquisition.
    ///
    /// A caller that instead does `get` (to decide who to push to) and a
    /// later, separate `remove` leaves a window — spanning the network
    /// `.await` of the push in between — for a concurrent [`Self::redirect`]
    /// to land unseen: the redirect's write and the stale `remove` don't
    /// conflict at the type level, so the new target is silently dropped and
    /// the client now waiting on it never hears the completion (SPEC
    /// R2.5.3). Reading and removing under one write-lock closes that window:
    /// whatever `redirect` most recently installed is exactly what this
    /// returns.
    async fn claim_terminal(&self, op_id: &str) -> Option<PushTarget> {
        self.targets.write().await.remove(op_id)
    }

    /// Point an operation's progress at the request that is waiting on it *now*,
    /// returning the target it displaced (SPEC R2.5.3).
    ///
    /// A progress token belongs to an in-flight request. The token registered
    /// when the operation started dies with that `tools/call` — so an `await`
    /// that then blocks for a minute emitted every "still running" notification
    /// under a token the client had already retired, and saw no liveness at all
    /// on the request it was actually waiting on. Callers restore the displaced
    /// target with [`Self::restore`] when their request completes, so the
    /// best-effort completion push (R2.2) still lands afterwards.
    pub async fn redirect(
        &self,
        op_id: &str,
        peer: Peer<RoleServer>,
        progress_token: ProgressToken,
        progress_enabled: bool,
    ) -> Option<PushTarget> {
        if !progress_enabled {
            return None;
        }
        self.targets.write().await.insert(
            op_id.to_string(),
            PushTarget {
                peer,
                progress_token,
            },
        )
    }

    /// Put back a target displaced by [`Self::redirect`]. `None` means the
    /// operation had no target before, so leave it without one.
    ///
    /// No-ops once the operation is no longer active. This is a *separate*
    /// task from the one waiting on the operation (`Drop` cannot await, so
    /// restoration is spawned — see `ProgressRedirect`), racing against the
    /// forwarder's own `claim_terminal` for the same terminal event.
    /// Checking `targets` for "is there still an entry to restore onto"
    /// doesn't resolve that race: if this task runs *before* the forwarder's
    /// claim, the entry is still present and gets overwritten with the
    /// displaced (retired) target — which `claim_terminal` then hands the
    /// completion to, addressing it to a request that already returned
    /// instead of the one SPEC R2.5.3 requires.
    ///
    /// `OperationMonitor::get_operation` is the fix: an operation is removed
    /// from the active map in the same lock scope that transitions it to
    /// terminal, strictly *before* either the forwarder's broadcast event or
    /// this task's own await response can observe completion (see
    /// `OperationMonitor::move_to_history_and_notify`). So by the time this
    /// runs for an operation that actually finished, `get_operation` is
    /// guaranteed to already report it gone — regardless of which task the
    /// scheduler happened to run first — and restoring is skipped in favor of
    /// leaving the terminal dispatch entirely to `claim_terminal`.
    pub async fn restore(&self, op_id: &str, previous: Option<PushTarget>) {
        if self.monitor.get_operation(op_id).await.is_none() {
            return;
        }
        let mut targets = self.targets.write().await;
        if !targets.contains_key(op_id) {
            return;
        }
        match previous {
            Some(target) => {
                targets.insert(op_id.to_string(), target);
            }
            None => {
                targets.remove(op_id);
            }
        }
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

                let Some((progress, message)) = progress_for_event(&event) else {
                    continue;
                };
                let op_id = event.operation_id().to_string();

                if event.is_terminal() {
                    // Single atomic get-and-remove — see `claim_terminal` for
                    // why a separate read-then-remove isn't safe here.
                    if let Some(target) = router.claim_terminal(&op_id).await {
                        push_progress(&target.peer, target.progress_token, progress, message, true)
                            .await;
                    }
                } else if let Some(target) = { router.targets.read().await.get(&op_id).cloned() } {
                    push_progress(
                        &target.peer,
                        target.progress_token.clone(),
                        progress,
                        message,
                        false,
                    )
                    .await;
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
    let mut params = ProgressNotificationParam::new(progress_token, progress);
    params.total = Some(100.0);
    params.message = Some(message);
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
            title: None,
            cwd: None,
            command: None,
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

#[cfg(test)]
mod redirect_tests {
    use super::*;
    use crate::client_type::McpClientType;
    use crate::operation_monitor::{MonitorConfig, Operation, OperationStatus};
    use rmcp::model::NumberOrString;
    use std::time::Duration;

    fn token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    /// A real `Peer<RoleServer>` — rmcp's `Peer::new` is crate-private, so the
    /// in-process harness is the only way to obtain one. Also stands up a real
    /// `OperationMonitor`, since `restore()` now consults it directly (see its
    /// doc comment) rather than inferring liveness from the router's own map.
    async fn test_router() -> (
        Peer<RoleServer>,
        Arc<OperationMonitor>,
        Arc<ProgressPushRouter>,
        crate::test_utils::in_process::InProcessMcp,
    ) {
        let mcp = crate::test_utils::in_process::create_in_process_mcp_empty()
            .await
            .expect("in-process mcp");
        let peer = mcp._server.peer().clone();
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            Duration::from_secs(30),
        )));
        let router = ProgressPushRouter::new(monitor.clone());
        (peer, monitor, router, mcp)
    }

    async fn active_op(monitor: &OperationMonitor, id: &str) {
        monitor
            .add_operation(Operation::new(
                id.to_string(),
                "tool".to_string(),
                "desc".to_string(),
                None,
            ))
            .await;
    }

    #[tokio::test]
    async fn await_redirects_progress_and_gives_the_original_target_back() {
        // REGRESSION: progress for an operation used to stay pinned to the token
        // of the `tools/call` that started it. That request is long finished by
        // the time anyone calls `await`, so the awaiting client saw no liveness
        // at all on the request it was actually blocked on — 85 seconds of
        // silence in a captured session, after which the client gave up.
        let (peer, monitor, router, _mcp) = test_router().await;
        active_op(&monitor, "op_1").await;

        router
            .register(
                "op_1",
                peer.clone(),
                token(1),
                McpClientType::ClaudeDesktop,
                true,
            )
            .await;

        let displaced = router
            .redirect("op_1", peer.clone(), token(2), true)
            .await
            .expect("the original registration must be handed back");
        assert_eq!(displaced.progress_token, token(1));

        {
            let targets = router.targets.read().await;
            assert_eq!(
                targets.get("op_1").unwrap().progress_token,
                token(2),
                "progress must follow the request that is waiting now"
            );
        }

        // When the await returns (still running — a timeout, not a
        // completion), the completion push (SPEC R2.2) still has to land
        // eventually, so the displaced target goes back.
        router.restore("op_1", Some(displaced)).await;
        let targets = router.targets.read().await;
        assert_eq!(targets.get("op_1").unwrap().progress_token, token(1));
    }

    #[tokio::test]
    async fn restoring_nothing_leaves_the_operation_unregistered() {
        // An `await` on an operation nobody registered (no progress token on the
        // original call) must not leave a dangling target behind.
        let (peer, monitor, router, _mcp) = test_router().await;
        active_op(&monitor, "op_1").await;

        let displaced = router.redirect("op_1", peer, token(2), true).await;
        assert!(displaced.is_none(), "nothing was registered to displace");

        router.restore("op_1", None).await;
        assert_eq!(router.target_count().await, 0);
    }

    #[tokio::test]
    async fn an_operation_that_finished_during_the_await_is_not_resurrected() {
        // The forwarder claims the target on the terminal event. If that lands
        // while the await is returning, putting the old target back would leave
        // a target for a finished operation that nothing will ever remove.
        let (peer, monitor, router, _mcp) = test_router().await;
        active_op(&monitor, "op_1").await;

        router
            .register(
                "op_1",
                peer.clone(),
                token(1),
                McpClientType::ClaudeDesktop,
                true,
            )
            .await;
        let displaced = router.redirect("op_1", peer, token(2), true).await;

        monitor
            .update_status("op_1", OperationStatus::Completed, None)
            .await; // the operation actually finishes...
        router.claim_terminal("op_1").await; // ...and the forwarder claims it

        router.restore("op_1", displaced).await;

        assert_eq!(router.target_count().await, 0, "must stay unregistered");
    }

    #[tokio::test]
    async fn restore_does_not_clobber_a_target_the_forwarder_is_about_to_claim() {
        // REGRESSION: restoration runs on a task spawned from `Drop` (it can't
        // await), racing independently against the forwarder's own
        // `claim_terminal` for the very same terminal event — both are woken by
        // the operation reaching Completed. If restore() ran first and only
        // checked "is there still a map entry" (there was: the redirect's),
        // it would overwrite the live redirect target with the retired one,
        // and the forwarder would then deliver the completion to the request
        // that already returned instead of the one still waiting (SPEC
        // R2.5.3). Consulting `OperationMonitor` — which removes the operation
        // from its active map strictly before either consumer can observe
        // completion — closes this regardless of which task wins the race.
        let (peer, monitor, router, _mcp) = test_router().await;
        active_op(&monitor, "op_1").await;

        router
            .register(
                "op_1",
                peer.clone(),
                token(1),
                McpClientType::ClaudeDesktop,
                true,
            )
            .await;
        let displaced = router.redirect("op_1", peer, token(2), true).await;

        // The operation completes...
        monitor
            .update_status("op_1", OperationStatus::Completed, None)
            .await;
        // ...and restore() (from the await's Drop-spawned task) wins the race,
        // running before the forwarder's `claim_terminal`.
        router.restore("op_1", displaced).await;

        // The redirect target must be exactly what's left for the forwarder to
        // claim — restore() must not have touched it.
        let claimed = router.claim_terminal("op_1").await;
        assert_eq!(
            claimed.map(|t| t.progress_token),
            Some(token(2)),
            "the completion must still go to the request that was waiting, not the retired one"
        );
    }

    #[tokio::test]
    async fn claim_terminal_hands_back_and_removes_whatever_is_registered() {
        let (peer, monitor, router, _mcp) = test_router().await;
        active_op(&monitor, "op_1").await;

        router
            .register("op_1", peer, token(1), McpClientType::ClaudeDesktop, true)
            .await;

        let claimed = router.claim_terminal("op_1").await;
        assert_eq!(claimed.map(|t| t.progress_token), Some(token(1)));
        assert_eq!(router.target_count().await, 0);
    }

    #[tokio::test]
    async fn claim_terminal_is_none_when_nothing_registered() {
        let (_peer, _monitor, router, _mcp) = test_router().await;
        assert!(router.claim_terminal("op_1").await.is_none());
    }

    #[tokio::test]
    async fn a_disabled_caller_is_never_redirected() {
        // The router itself no longer decides who handles progress — the
        // caller resolves `progress_enabled` (normally
        // `AhmaMcpService::effective_supports_progress`, e.g. `false` for
        // Cursor by default) and passes the answer in.
        let (peer, _monitor, router, _mcp) = test_router().await;

        assert!(
            router
                .redirect("op_1", peer, token(2), false)
                .await
                .is_none()
        );
        assert_eq!(router.target_count().await, 0);
    }
}
