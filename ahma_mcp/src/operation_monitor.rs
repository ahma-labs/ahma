//! Operation monitoring and management system
//!
//! This module provides comprehensive monitoring, timeout handling, and cancellation
//! support for long-running cargo operations. It enables tracking of operation state,
//! automatic cleanup, and detailed logging for debugging.

use crate::utils::time;
use ahma_common::event_dispatcher::{EventDispatcher, OperationEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{
    RwLock, broadcast,
    watch::{self, Receiver as WatchReceiver},
};
use tokio_util::sync::CancellationToken;
use tracing;

/// Broadcast capacity for the unified operation event stream.
///
/// Sized for high-volume `OutputLine` traffic: a slow subscriber that falls
/// more than this many events behind receives `RecvError::Lagged` and must
/// reconcile from [`OperationMonitor`] state (the store of record).  Operation
/// completion semantics never depend on the broadcast — `await` uses the
/// per-operation watch channel plus `completion_history`.
const EVENT_STREAM_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Represents the current state of an operation
pub enum OperationStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl OperationStatus {
    /// Check if this state represents a terminal (completed) operation
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OperationStatus::Completed
                | OperationStatus::Failed
                | OperationStatus::Cancelled
                | OperationStatus::TimedOut
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Information about a running operation
pub struct Operation {
    pub id: String,
    pub tool_name: String,
    pub description: String,
    pub state: OperationStatus,
    pub result: Option<Value>,
    /// When the operation was created
    #[serde(with = "time")]
    pub start_time: SystemTime,
    /// When the operation completed (None if still running)
    #[serde(with = "time::option", default)]
    pub end_time: Option<SystemTime>,
    /// When wait_for_operation was first called for this operation (None if never waited for)
    #[serde(with = "time::option", default)]
    pub first_wait_time: Option<SystemTime>,
    /// Timeout duration for this specific operation (None means use default)
    pub timeout_duration: Option<Duration>,
    /// Cancellation token for this operation (not serialized)
    #[serde(skip)]
    pub cancellation_token: CancellationToken,
    /// Completion watch channel sender: false = in-progress, true = done.
    /// All subscribers observe completion even if they subscribe after the signal fires.
    /// Not serialised — only meaningful for live operations held in OperationMonitor.
    #[serde(skip, default = "default_completion_watch")]
    pub completion_watch: Arc<watch::Sender<bool>>,
    /// Tail of stdout/stderr lines for this operation (max 100)
    #[serde(default)]
    pub stdout_tail: Vec<String>,
    /// Any warnings/errors detected for this operation
    #[serde(default)]
    pub alerts: Vec<String>,
    /// Path of the full-output spill file for this operation, when spilling
    /// is active.  `stdout_tail` is a bounded window; the spill file holds the
    /// complete output and can be queried with the file tools (tail/grep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<std::path::PathBuf>,
}

/// Default factory for `completion_watch` used during serde deserialisation.
fn default_completion_watch() -> Arc<watch::Sender<bool>> {
    Arc::new(watch::channel(false).0)
}

impl Operation {
    /// Create a new operation info
    pub fn new(id: String, tool_name: String, description: String, result: Option<Value>) -> Self {
        Self {
            id,
            tool_name,
            description,
            state: OperationStatus::Pending,
            result,
            start_time: SystemTime::now(),
            end_time: None,
            first_wait_time: None,
            timeout_duration: None,
            cancellation_token: CancellationToken::new(),
            completion_watch: Arc::new(watch::channel(false).0),
            stdout_tail: Vec::new(),
            alerts: Vec::new(),
            output_file: None,
        }
    }

    /// Create a new operation info with timeout
    pub fn new_with_timeout(
        id: String,
        tool_name: String,
        description: String,
        result: Option<Value>,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            id,
            tool_name,
            description,
            state: OperationStatus::Pending,
            result,
            start_time: SystemTime::now(),
            end_time: None,
            first_wait_time: None,
            timeout_duration: timeout,
            cancellation_token: CancellationToken::new(),
            completion_watch: Arc::new(watch::channel(false).0),
            stdout_tail: Vec::new(),
            alerts: Vec::new(),
            output_file: None,
        }
    }

    /// Subscribe to the completion channel.
    /// Returns a receiver that yields `true` when the operation reaches a terminal state.
    /// Safe to call at any point — if the operation already completed the receiver
    /// immediately observes `true` on the first `borrow()` / `wait_for` poll.
    pub fn subscribe_completion(&self) -> WatchReceiver<bool> {
        self.completion_watch.subscribe()
    }
}

/// Configuration for operation monitoring
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Default timeout for operations (reduced from 5 minutes to 30 seconds)
    pub default_timeout: Duration,
    /// Maximum time to await for graceful shutdown
    pub shutdown_timeout: Duration,
}

#[derive(Debug, Clone)]
/// Summary of active operations for shutdown coordination
pub struct ShutdownSummary {
    pub total_active: usize,
    pub operations: Vec<Operation>,
}

impl MonitorConfig {
    /// Create a MonitorConfig with a custom timeout
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            default_timeout: timeout,
            shutdown_timeout: Duration::from_secs(30), // Reduced from 360s to 30s
        }
    }

    /// Create a MonitorConfig with custom timeouts
    pub fn with_timeouts(operation_timeout: Duration, shutdown_timeout: Duration) -> Self {
        Self {
            default_timeout: operation_timeout,
            shutdown_timeout,
        }
    }
}

/// Check if an operation's tool name matches any of the given filter prefixes.
/// Returns true if no filters are provided (i.e., all operations match).
fn matches_tool_filter(op: &Operation, filters: &Option<Vec<String>>) -> bool {
    filters.as_ref().is_none_or(|f| {
        let name = op.tool_name.to_lowercase();
        f.iter().any(|filter| name.starts_with(filter))
    })
}

/// Build a structured JSON cancellation result for debugging/LLM visibility.
fn build_cancellation_result(reason: Option<String>) -> Value {
    let reason_str = reason.unwrap_or_else(|| "Cancelled by user".to_string());
    serde_json::json!({
        "cancelled": true,
        "reason": reason_str
    })
}

/// Parse a comma-separated tool filter string into lowercase prefixes.
fn parse_tool_filters(tool_filter: Option<&str>) -> Option<Vec<String>> {
    tool_filter.map(|filters| {
        filters
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .collect()
    })
}

/// Log progressive timeout warnings at 50%, 75%, and 90% thresholds.
fn log_progress_warnings(progress_percent: u8, remaining_secs: i64, warnings_sent: &mut [bool; 3]) {
    const THRESHOLDS: [u8; 3] = [50, 75, 90];
    const MESSAGES: [&str; 3] = [
        "Wait operation 50% complete. Current active operations being monitored.",
        "Wait operation 75% complete. Consider checking operation status.",
        "Wait operation 90% complete. Operations may timeout soon!",
    ];

    for (i, &threshold) in THRESHOLDS.iter().enumerate() {
        if progress_percent >= threshold && !warnings_sent[i] {
            warnings_sent[i] = true;
            tracing::warn!("{} - {}s remaining", MESSAGES[i], remaining_secs.max(0));
        }
    }
}

/// Operation monitor that tracks and manages cargo operations
#[derive(Debug, Clone)]
pub struct OperationMonitor {
    operations: Arc<RwLock<HashMap<String, Operation>>>,
    completion_history: Arc<RwLock<HashMap<String, Operation>>>,
    #[allow(dead_code)]
    config: MonitorConfig,
    /// Unified event stream (SPEC R15).  The monitor is the single emitter of
    /// operation lifecycle events: `Started` on insert, `OutputLine`/`Alert`
    /// while streaming, and exactly one terminal event when the operation
    /// moves to `completion_history`.
    events: EventDispatcher,
}

impl OperationMonitor {
    /// Create a new operation monitor
    pub fn new(config: MonitorConfig) -> Self {
        Self {
            operations: Arc::new(RwLock::new(HashMap::new())),
            completion_history: Arc::new(RwLock::new(HashMap::new())),
            config,
            events: EventDispatcher::new(EVENT_STREAM_CAPACITY),
        }
    }

    /// Subscribe to the unified operation event stream.
    ///
    /// Subscribers MUST handle `RecvError::Lagged` by reconciling from monitor
    /// state (`get_all_active_operations` / `get_completed_operations`) — the
    /// broadcast is a live feed, not the store of record.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<OperationEvent>> {
        self.events.subscribe()
    }

    /// The shared event dispatcher.  Components that emit supplementary events
    /// (e.g. the adapter) clone this so all events flow through one stream.
    pub fn event_dispatcher(&self) -> &EventDispatcher {
        &self.events
    }

    pub async fn add_operation(&self, operation: Operation) {
        let mut ops = self.operations.write().await;
        tracing::info!(
            "Adding operation to monitor: {} (status: {:?})",
            operation.id,
            operation.state
        );
        let started_event = OperationEvent::Started {
            operation_id: operation.id.clone(),
            tool_name: operation.tool_name.clone(),
            description: operation.description.clone(),
        };
        let was_new = ops.insert(operation.id.clone(), operation).is_none();
        tracing::debug!("Total operations in monitor after add: {}", ops.len());
        drop(ops);

        // `add_operation` doubles as an upsert for in-place updates; only a
        // genuinely new operation emits `Started`.
        if was_new {
            self.events.emit(started_event);
        }
    }

    /// Append a line of live output to the operation's tail buffer and stream
    /// it to event subscribers.
    pub async fn append_output_line(&self, id: &str, line: String, is_stderr: bool) {
        let mut ops = self.operations.write().await;
        if let Some(op) = ops.get_mut(id) {
            if op.stdout_tail.len() >= 100 {
                op.stdout_tail.remove(0);
            }
            op.stdout_tail.push(line.clone());
        } else {
            return;
        }
        drop(ops);

        self.events.emit(OperationEvent::OutputLine {
            operation_id: id.to_string(),
            line,
            is_stderr,
        });
    }

    /// Backwards-compatible wrapper for stdout lines.
    pub async fn append_stdout_line(&self, id: &str, line: String) {
        self.append_output_line(id, line, false).await;
    }

    pub async fn append_alert(&self, id: &str, alert: String) {
        let mut ops = self.operations.write().await;
        if let Some(op) = ops.get_mut(id) {
            op.alerts.push(alert.clone());
        } else {
            return;
        }
        drop(ops);

        self.events.emit(OperationEvent::Alert {
            operation_id: id.to_string(),
            message: alert,
        });
    }

    pub async fn get_operation(&self, id: &str) -> Option<Operation> {
        let ops = self.operations.read().await;
        ops.get(id).cloned()
    }

    /// Starts a background task that periodically checks for timed-out operations.
    pub fn start_background_monitor(monitor: Arc<Self>) {
        let weak_monitor = Arc::downgrade(&monitor);
        tokio::spawn(async move {
            let check_interval = Duration::from_secs(1);
            loop {
                if let Some(monitor) = weak_monitor.upgrade() {
                    monitor.check_timeouts().await;
                } else {
                    tracing::debug!("OperationMonitor dropped, stopping background monitor task");
                    break;
                }
                tokio::time::sleep(check_interval).await;
            }
        });
    }

    /// Checks all active operations for timeouts and cancels them if necessary.
    pub async fn check_timeouts(&self) {
        let now = SystemTime::now();
        let timed_out_ops = {
            let ops = self.operations.read().await;
            ops.values()
                .filter(|op| !op.state.is_terminal())
                .filter_map(|op| {
                    let timeout = op.timeout_duration.unwrap_or(self.config.default_timeout);
                    let elapsed = now.duration_since(op.start_time).ok()?;
                    (elapsed > timeout).then_some((op.id.clone(), elapsed, timeout))
                })
                .collect::<Vec<_>>()
        };

        for (op_id, elapsed, timeout) in timed_out_ops {
            let reason = format!(
                "Operation timed out after {:.1}s (limit: {:.1}s)",
                elapsed.as_secs_f64(),
                timeout.as_secs_f64()
            );
            self.timeout_operation(&op_id, reason).await;
        }
    }

    async fn timeout_operation(&self, id: &str, reason: String) {
        let mut ops = self.operations.write().await;
        let Some(op) = ops.get_mut(id) else {
            return;
        };
        if op.state.is_terminal() {
            return;
        }

        tracing::warn!("Timing out operation: {} - {}", id, reason);

        op.state = OperationStatus::TimedOut;
        op.end_time = Some(SystemTime::now());
        op.cancellation_token.cancel();
        op.result = Some(serde_json::json!({
            "timed_out": true,
            "reason": reason
        }));

        let timed_out_op = ops.remove(id);
        drop(ops);

        self.move_to_history_and_notify(id, timed_out_op).await;
    }

    /// Move a completed operation to history and signal all completion waiters.
    ///
    /// Inserts into `completion_history` first, then broadcasts `true` through
    /// the watch channel.  Because the watch channel stores the current value,
    /// any subscriber that calls `subscribe_completion()` *after* this point
    /// will immediately observe `true` — eliminating the race that existed with
    /// the old `Arc<Notify>` approach where late subscribers missed the wakeup.
    ///
    /// Ordering invariant (SPEC R15.3): history write → watch signal → event
    /// emission, so any consumer woken by either channel observes final state.
    async fn move_to_history_and_notify(&self, id: &str, operation: Option<Operation>) {
        let Some(op) = operation else { return };
        let mut history = self.completion_history.write().await;
        history.insert(id.to_string(), op.clone());
        drop(history);
        // Signal completion.  Ignore errors: a SendError means no subscribers,
        // which is fine — the result is already in completion_history.
        let _ = op.completion_watch.send(true);
        self.events.emit(terminal_event_for(&op));
    }

    /// Returns all currently active (non-terminal) operations.
    ///
    /// Note: Completed operations are accessible via `get_completed_operations`.
    pub async fn get_all_active_operations(&self) -> Vec<Operation> {
        self.get_active_operations().await
    }

    pub async fn update_status(&self, id: &str, status: OperationStatus, result: Option<Value>) {
        let mut ops = self.operations.write().await;
        let mut operation_to_move = None;
        let mut updated_op = None;

        if let Some(op) = ops.get_mut(id) {
            // Guard: never overwrite an already-terminal state.  Terminal ops are removed
            // from the active map immediately, so if one is still present here a concurrent
            // transition must be in flight.  The first terminal writer wins; subsequent
            // attempts become a no-op with a debug trace rather than silently corrupting state.
            if op.state.is_terminal() {
                tracing::debug!(
                    "update_status: ignoring {:?} for op {} — already terminal ({:?})",
                    status,
                    id,
                    op.state
                );
                return;
            }

            tracing::debug!(
                "Updating operation {} from {:?} to {:?}",
                id,
                op.state,
                status
            );
            op.state = status;
            op.result = result;

            if status.is_terminal() {
                op.end_time = Some(SystemTime::now());
                operation_to_move = ops.remove(id);
            } else {
                updated_op = Some(op.clone());
            }
        }

        drop(ops);

        if let Some(op) = operation_to_move {
            tracing::debug!("Moving operation {} to completion history.", id);
            self.move_to_history_and_notify(id, Some(op)).await;
        } else if let Some(op) = updated_op {
            self.events.emit(OperationEvent::Progress {
                operation_id: op.id.clone(),
                message: format!("status: {:?}", op.state),
                percent: None,
            });
        }
    }

    /// Cancel an operation by ID with an optional reason string.
    /// Returns true if the operation was found and cancelled, false if not found.
    pub async fn cancel_operation_with_reason(&self, id: &str, reason: Option<String>) -> bool {
        let mut ops = self.operations.write().await;

        let Some(op) = ops.get_mut(id) else {
            tracing::warn!("Attempted to cancel non-existent operation: {}", id);
            return false;
        };

        if op.state.is_terminal() {
            tracing::warn!("Attempted to cancel already terminal operation: {}", id);
            return false;
        }

        tracing::info!("Cancelling operation: {}", id);
        tracing::debug!(
            "CANCEL_OPERATION_WITH_REASON: id='{}', reason={:?}, current_state={:?}",
            id,
            reason,
            op.state
        );

        op.state = OperationStatus::Cancelled;
        op.end_time = Some(SystemTime::now());
        op.cancellation_token.cancel();
        op.result = Some(build_cancellation_result(reason));

        let cancelled_op = op.clone();
        ops.remove(id);
        drop(ops);

        tracing::debug!("Moving cancelled operation {} to completion history.", id);
        self.move_to_history_and_notify(id, Some(cancelled_op))
            .await;

        true
    }

    /// Backward-compatible helper without explicit reason
    pub async fn cancel_operation(&self, id: &str) -> bool {
        self.cancel_operation_with_reason(id, None).await
    }

    pub async fn get_active_operations(&self) -> Vec<Operation> {
        let ops = self.operations.read().await;
        ops.values()
            .filter(|op| !op.state.is_terminal())
            .cloned()
            .collect()
    }

    pub async fn get_completed_operations(&self) -> Vec<Operation> {
        let history = self.completion_history.read().await;
        history.values().cloned().collect()
    }

    pub async fn get_shutdown_summary(&self) -> ShutdownSummary {
        let operations = self.get_active_operations().await;
        let total_active = operations.len();
        ShutdownSummary {
            total_active,
            operations,
        }
    }

    /// Look up an operation in the completion history.
    pub async fn check_completion_history_pub(&self, id: &str) -> Option<Operation> {
        let history = self.completion_history.read().await;
        history.get(id).cloned()
    }

    /// Get a completion watch receiver for an active operation, recording first-wait time.
    ///
    /// Returns:
    /// - `Ok(Some(rx))` — operation is active; `rx` fires when it reaches a terminal state
    /// - `Ok(None)` — operation not found in active ops (may have just moved to history)
    /// - `Err(op)` — operation is already terminal in active ops (edge case)
    ///
    /// Unlike the old `get_notifier_or_terminal_pub`, the returned receiver will immediately
    /// yield `true` if the operation completed between this call and the first `wait_for` poll,
    /// eliminating the race that `wait_for_history_propagation_pub` papered over.
    pub async fn get_completion_receiver_or_terminal_pub(
        &self,
        id: &str,
    ) -> Result<Option<WatchReceiver<bool>>, Operation> {
        let mut ops = self.operations.write().await;
        let Some(op) = ops.get_mut(id) else {
            return Ok(None);
        };

        if op.first_wait_time.is_none() {
            op.first_wait_time = Some(SystemTime::now());
        }

        if op.state.is_terminal() {
            return Err(op.clone());
        }

        Ok(Some(op.subscribe_completion()))
    }

    /// Wait for an operation to reach a terminal state and return it from completion history.
    ///
    /// The implementation is race-free:
    /// 1. Check history first (fast path for already-completed ops).
    /// 2. Subscribe to the watch channel *while* the op is still in active ops, so we
    ///    cannot miss the completion signal even if it fires between steps 1 and 2.
    /// 3. If the op transitioned to history between steps 1 and 2, the channel already
    ///    holds `true` and `wait_for` returns immediately.
    /// 4. After the channel signals, the op is guaranteed to be in history (the channel
    ///    is sent *after* the history write), so a single history lookup suffices.
    pub async fn wait_for_operation(&self, id: &str) -> Option<Operation> {
        let timeout = Duration::from_secs(300);

        // Fast path: already completed.
        if let Some(op) = self.check_completion_history_pub(id).await {
            return Some(op);
        }

        let mut rx = match self.get_completion_receiver_or_terminal_pub(id).await {
            Err(terminal_op) => return Some(terminal_op),
            Ok(None) => {
                // Op is not in active ops.  It may have completed between the history
                // check above and here.  Re-check history once.
                return self.check_completion_history_pub(id).await;
            }
            Ok(Some(rx)) => rx,
        };

        // Wait until the completion flag turns true (or timeout).
        // `wait_for` is safe against missed signals: the watch channel stores its
        // current value, so even if `send(true)` fired between
        // `get_completion_receiver_or_terminal_pub` and this await, the receiver
        // will see `true` on the very first poll.
        //
        // Note: `watch::Ref` wraps an `RwLockReadGuard` which is not `Send`, so we
        // extract a plain `bool` and drop the guard before the next `await`.
        let timed_out = tokio::time::timeout(timeout, rx.wait_for(|done| *done))
            .await
            .is_err();
        if timed_out {
            tracing::warn!("Wait for operation {} timed out.", id);
            None
        } else {
            // The completion signal was sent *after* history insertion, so the
            // operation must already be in history at this point.
            self.check_completion_history_pub(id).await
        }
    }

    /// Get active operations that match the given tool filter.
    async fn get_filtered_active_operations(
        &self,
        filters: &Option<Vec<String>>,
    ) -> Vec<Operation> {
        let ops = self.operations.read().await;
        ops.values()
            .filter(|op| !op.state.is_terminal())
            .filter(|op| matches_tool_filter(op, filters))
            .cloned()
            .collect()
    }

    /// Collect completed operations from history that match the filter and finished
    /// after the given start time.
    async fn collect_completed_since(
        &self,
        filters: &Option<Vec<String>>,
        since: SystemTime,
    ) -> Vec<Operation> {
        let history = self.completion_history.read().await;
        history
            .values()
            .filter(|op| matches_tool_filter(op, filters))
            .filter(|op| op.end_time.is_some_and(|t| t >= since))
            .cloned()
            .collect()
    }

    /// Collect all completed operations from history that match the filter.
    async fn collect_all_completed(&self, filters: &Option<Vec<String>>) -> Vec<Operation> {
        let history = self.completion_history.read().await;
        history
            .values()
            .filter(|op| matches_tool_filter(op, filters))
            .cloned()
            .collect()
    }

    /// Advanced await functionality that waits for multiple operations with progressive timeout warnings.
    ///
    /// # Arguments
    /// * `tool_filter` - Optional comma-separated list of tool prefixes to await for
    /// * `timeout_seconds` - Timeout in seconds (1-1800 range, defaults to 240)
    ///
    /// # Returns
    /// A vector of completed operations that match the filter criteria
    pub async fn wait_for_operations_advanced(
        &self,
        tool_filter: Option<&str>,
        timeout_seconds: Option<u32>,
    ) -> Vec<Operation> {
        let timeout_secs = timeout_seconds.unwrap_or(240).clamp(1, 1800);
        let timeout = Duration::from_secs(timeout_secs as u64);
        let start_time = Instant::now();
        let tool_filters = parse_tool_filters(tool_filter);

        tracing::info!(
            "Starting advanced await operation: timeout={}s, tool_filter={:?}",
            timeout_secs,
            tool_filters
        );

        let mut warnings_sent = [false; 3];
        let mut completed_operations = Vec::new();

        loop {
            let elapsed = start_time.elapsed();

            if elapsed >= timeout {
                tracing::warn!("Advanced await operation timed out after {}s", timeout_secs);
                break;
            }

            let progress_percent = (elapsed.as_secs_f64() / timeout.as_secs_f64() * 100.0) as u8;
            let remaining_secs = timeout_secs as i64 - elapsed.as_secs() as i64;
            log_progress_warnings(progress_percent, remaining_secs, &mut warnings_sent);

            let active_ops = self.get_filtered_active_operations(&tool_filters).await;

            if active_ops.is_empty() {
                completed_operations = self.collect_all_completed(&tool_filters).await;
                tracing::info!(
                    "Advanced await completed: {} operations finished, no active operations remaining",
                    completed_operations.len()
                );
                break;
            }

            let wait_start_system_time = SystemTime::now() - elapsed;
            let newly_completed = self
                .collect_completed_since(&tool_filters, wait_start_system_time)
                .await;
            completed_operations.extend(newly_completed);

            tokio::time::sleep(Duration::from_millis(
                crate::constants::SEQUENCE_STEP_DELAY_MS,
            ))
            .await;
        }

        completed_operations
    }
}

/// Map a terminal [`Operation`] to its unified terminal event.
///
/// Exactly one terminal event is emitted per operation, at the moment it
/// moves into `completion_history`.
fn terminal_event_for(op: &Operation) -> OperationEvent {
    let duration_ms = op
        .end_time
        .and_then(|end| end.duration_since(op.start_time).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    match op.state {
        OperationStatus::Completed => OperationEvent::Completed {
            operation_id: op.id.clone(),
            result: op.result.clone().unwrap_or(Value::Null),
            duration_ms,
        },
        OperationStatus::Failed => OperationEvent::Failed {
            operation_id: op.id.clone(),
            error: op
                .result
                .as_ref()
                .and_then(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| serde_json::to_string(v).ok())
                })
                .unwrap_or_else(|| "unknown error".to_string()),
            duration_ms,
        },
        OperationStatus::Cancelled => OperationEvent::Cancelled {
            operation_id: op.id.clone(),
            reason: op
                .result
                .as_ref()
                .and_then(|v| {
                    v.get("reason")
                        .and_then(|r| r.as_str())
                        .or_else(|| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "cancelled".to_string()),
            duration_ms,
        },
        OperationStatus::TimedOut => OperationEvent::TimedOut {
            operation_id: op.id.clone(),
            duration_ms,
        },
        // Non-terminal states should never reach here; map defensively.
        OperationStatus::Pending | OperationStatus::InProgress => OperationEvent::Failed {
            operation_id: op.id.clone(),
            error: "operation ended in unexpected non-terminal state".to_string(),
            duration_ms,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::logging::init_test_logging;
    use std::time::Duration;

    /// This test simulates the race condition where an operation completes
    /// so quickly that a `await` call might miss it. By using the `completion_history`
    /// map, the monitor should now correctly retrieve the status of already-completed
    /// operations.
    #[tokio::test]
    async fn test_wait_for_fast_completion_race_condition() {
        init_test_logging();
        let monitor = OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(5)));
        let op_id = "fast_op_1".to_string();
        let op = Operation::new(
            op_id.clone(),
            "test_tool".to_string(),
            "A test operation".to_string(),
            None,
        );

        // 1. Add the operation
        monitor.add_operation(op).await;

        // 2. Immediately update its status to Completed, which moves it to history
        monitor
            .update_status(
                &op_id,
                OperationStatus::Completed,
                Some(serde_json::json!({"result": "success"})),
            )
            .await;

        // 3. Now, try to await for it. The old system might have failed here.
        let result = monitor.wait_for_operation(&op_id).await;

        // 4. Assert that we correctly found the completed operation.
        assert!(result.is_some());
        let completed_op = result.unwrap();
        assert_eq!(completed_op.id, op_id);
        assert_eq!(completed_op.state, OperationStatus::Completed);
        assert_eq!(
            completed_op.result,
            Some(serde_json::json!({"result": "success"}))
        );

        // 5. Verify it's not in the active operations map anymore
        let active_ops = monitor.operations.read().await;
        assert!(!active_ops.contains_key(&op_id));

        // 6. Verify it IS in the completion history map
        let history = monitor.completion_history.read().await;
        assert!(history.contains_key(&op_id));
    }

    /// Tests that waiting for an operation that never existed returns `None`
    /// immediately instead of blocking indefinitely.
    #[tokio::test]
    async fn test_wait_for_nonexistent_operation() {
        init_test_logging();
        let monitor = OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(5)));

        // Use a timeout to ensure the test completes quickly
        let wait_result = tokio::time::timeout(
            Duration::from_millis(200),
            monitor.wait_for_operation("nonexistent-id"),
        )
        .await;

        // Should complete quickly and return None for nonexistent operation
        match wait_result {
            Ok(result) => {
                assert!(
                    result.is_none(),
                    "Should return None for nonexistent operation"
                );
            }
            Err(_) => {
                panic!(
                    "wait_for_operation should return quickly for nonexistent operation, not timeout"
                );
            }
        }
    }

    /// Verifies that `update_status` does NOT overwrite a terminal state that was
    /// set by a racing concurrent transition.  The guard added in the fix should
    /// keep the first terminal state and discard the later update.
    #[tokio::test]
    async fn test_update_status_ignores_overwrite_of_terminal_state() {
        init_test_logging();
        let monitor = OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(5)));
        let op_id = "terminal-guard-test".to_string();

        monitor
            .add_operation(Operation::new(
                op_id.clone(),
                "test_tool".to_string(),
                "terminal guard test".to_string(),
                None,
            ))
            .await;

        // Simulate: cancellation fires first, moves the op to terminal state.
        let cancelled = monitor
            .cancel_operation_with_reason(&op_id, Some("test cancel".to_string()))
            .await;
        assert!(cancelled, "cancel_operation_with_reason should succeed");

        // Op should now be in history as Cancelled.
        let in_history = monitor.check_completion_history_pub(&op_id).await;
        assert!(
            in_history.is_some(),
            "cancelled op must appear in completion history"
        );
        assert_eq!(
            in_history.unwrap().state,
            OperationStatus::Cancelled,
            "state in history should be Cancelled"
        );

        // Now simulate the background task calling update_status(Completed) after the cancel.
        // With the guard this should be a no-op (op was already removed from active ops).
        monitor
            .update_status(
                &op_id,
                OperationStatus::Completed,
                Some(serde_json::json!({"result": "late completion"})),
            )
            .await;

        // History entry must remain Cancelled — the Completed update was discarded.
        let after = monitor.check_completion_history_pub(&op_id).await.unwrap();
        assert_eq!(
            after.state,
            OperationStatus::Cancelled,
            "terminal state must not be overwritten by a late Completed update"
        );
    }

    /// Verifies that `update_status` blocks a state downgrade (non-terminal
    /// replacing an already-terminal state) when the op is still in the active map.
    ///
    /// In normal operation terminal ops are removed immediately, so this path is
    /// defensive.  The guard must still prevent the downgrade.
    #[tokio::test]
    async fn test_update_status_blocks_downgrade_while_in_active_map() {
        init_test_logging();
        let monitor = OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(5)));
        let op_id = "downgrade-guard-test".to_string();

        // Insert an operation and manually set it to a terminal state without removing it
        // from the active map.  This mimics the edge-case the guard is designed for.
        {
            let op = Operation::new(
                op_id.clone(),
                "test_tool".to_string(),
                "downgrade guard test".to_string(),
                None,
            );
            let mut ops = monitor.operations.write().await;
            let mut op_mut = op;
            op_mut.state = OperationStatus::Completed; // force terminal while in active map
            ops.insert(op_id.clone(), op_mut);
        }

        // update_status(InProgress) must be rejected because the op is already terminal.
        monitor
            .update_status(&op_id, OperationStatus::InProgress, None)
            .await;

        let ops = monitor.operations.read().await;
        let op = ops.get(&op_id).unwrap();
        assert_eq!(
            op.state,
            OperationStatus::Completed,
            "state must stay Completed; downgrade to InProgress must be blocked"
        );
    }

    /// Verifies that subscribing to events on `OperationMonitor` yields `Started`
    /// followed by `Updated` events in the correct order when operations are
    /// added and transition to terminal status.
    #[tokio::test]
    async fn test_event_propagation_order() {
        init_test_logging();
        let monitor = OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(5)));

        let mut rx = monitor.subscribe_events();

        let op_id = "test-event-op".to_string();
        let op = Operation::new(
            op_id.clone(),
            "test_tool".to_string(),
            "event test".to_string(),
            None,
        );

        // 1. Add operation
        monitor.add_operation(op).await;

        // 2. We should receive Started event
        let event1 = rx.recv().await.expect("Failed to receive Started event");
        if let OperationEvent::Started {
            operation_id,
            tool_name,
            ..
        } = event1.as_ref()
        {
            assert_eq!(operation_id, &op_id);
            assert_eq!(tool_name, "test_tool");
        } else {
            panic!("Expected OperationEvent::Started, got {:?}", event1);
        }

        // 3. Update status to Completed
        monitor
            .update_status(
                &op_id,
                OperationStatus::Completed,
                Some(serde_json::json!({"ok": true})),
            )
            .await;

        // 4. We should receive exactly one terminal Completed event
        let event2 = rx.recv().await.expect("Failed to receive Completed event");
        if let OperationEvent::Completed {
            operation_id,
            result,
            ..
        } = event2.as_ref()
        {
            assert_eq!(operation_id, &op_id);
            assert_eq!(result, &serde_json::json!({"ok": true}));
        } else {
            panic!("Expected OperationEvent::Completed, got {:?}", event2);
        }
    }

    /// Output lines appended to a live operation are streamed on the unified
    /// event channel, and `Started` is emitted only for genuinely new ops
    /// (re-inserting via the upsert path must not duplicate it).
    #[tokio::test]
    async fn test_output_line_streaming_and_upsert_dedup() {
        init_test_logging();
        let monitor = OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(5)));
        let mut rx = monitor.subscribe_events();

        let op_id = "stream-test".to_string();
        let op = Operation::new(
            op_id.clone(),
            "test_tool".to_string(),
            "stream test".to_string(),
            None,
        );
        monitor.add_operation(op.clone()).await;
        // Upsert the same op again — must NOT emit a second Started.
        monitor.add_operation(op).await;

        monitor
            .append_output_line(&op_id, "hello".to_string(), false)
            .await;
        monitor
            .append_output_line(&op_id, "oops".to_string(), true)
            .await;

        // Started (exactly once)
        match rx.recv().await.expect("Started").as_ref() {
            OperationEvent::Started { operation_id, .. } => assert_eq!(operation_id, &op_id),
            other => panic!("expected Started, got {other:?}"),
        }
        // First output line (stdout)
        match rx.recv().await.expect("OutputLine 1").as_ref() {
            OperationEvent::OutputLine {
                line, is_stderr, ..
            } => {
                assert_eq!(line, "hello");
                assert!(!is_stderr);
            }
            other => panic!("expected OutputLine, got {other:?}"),
        }
        // Second output line (stderr)
        match rx.recv().await.expect("OutputLine 2").as_ref() {
            OperationEvent::OutputLine {
                line, is_stderr, ..
            } => {
                assert_eq!(line, "oops");
                assert!(is_stderr);
            }
            other => panic!("expected OutputLine, got {other:?}"),
        }
    }
}
