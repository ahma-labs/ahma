//! Daemon source — background task that subscribes to the hub daemon and
//! feeds multi-instance operation events into the TUI event loop.
//!
//! Unlike [`crate::mcp_source`], which polls a single HTTP/Unix server, this
//! source connects to the hub daemon socket and aggregates events from **all**
//! registered ahma instances (including stdio instances that have no HTTP
//! endpoint).
//!
//! ## Protocol
//!
//! 1. Connect to the daemon socket (`~/.ahma/daemon.sock` on Unix).
//! 2. Send `Subscribe` → receive `InstanceList` (current snapshot).
//! 3. Stream `DaemonMsg` events until EOF.
//!
//! On disconnect the task waits 5 s then reconnects.

use std::{collections::HashMap, time::Duration};

use ahma_common::daemon_hub::{
    ClientMsg, DaemonEvent, DaemonMsg, InstanceInfo, connect_to_daemon, ensure_daemon_running,
    recv_msg, send_msg,
};
use tokio::{
    io::BufReader,
    sync::{broadcast, mpsc},
};
use tracing::{debug, warn};

use crate::state::{OpStatus, Operation};

pub use crate::mcp_source::SourceEvent;

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn the daemon source background task.
///
/// All `OperationsUpdated` events contain the merged operation list from
/// **all** connected instances.  Each [`Operation`] has `instance_id` and
/// `instance_label` set so the UI can display an attribution badge.
pub fn spawn_daemon_source(tx: mpsc::Sender<SourceEvent>) {
    tokio::spawn(async move {
        daemon_source_task(tx).await;
    });
}

/// Spawn the embedded hub source using a direct in-process broadcast channel.
///
/// Unlike [`spawn_daemon_source`], this does not connect via a socket — it
/// receives [`DaemonMsg`] events pushed directly by the hub running inside
/// the same TUI process, eliminating the socket round-trip.
///
/// Used when the TUI itself starts the hub via
/// [`ahma_common::daemon_hub::try_start_hub_server`].
pub fn spawn_embedded_hub_source(
    mut rx: broadcast::Receiver<DaemonMsg>,
    tx: mpsc::Sender<SourceEvent>,
) {
    tokio::spawn(async move {
        let _ = tx
            .send(SourceEvent::DaemonHealthChanged { healthy: true })
            .await;
        let mut state = DaemonState::new();
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let is_instance_change = matches!(
                        msg,
                        DaemonMsg::InstanceList { .. }
                            | DaemonMsg::InstanceRegistered { .. }
                            | DaemonMsg::InstanceUnregistered { .. }
                    );
                    let applied = apply_msg(&mut state, msg);
                    if is_instance_change {
                        let _ = tx
                            .send(SourceEvent::InstancesUpdated {
                                instances: state.all_instances(),
                            })
                            .await;
                    }
                    match applied {
                        Applied::ListChanged => {
                            let ops = state.all_ops();
                            if tx
                                .send(SourceEvent::OperationsUpdated { ops })
                                .await
                                .is_err()
                            {
                                return; // TUI channel closed
                            }
                        }
                        Applied::Output {
                            instance_id,
                            op_id,
                            line,
                            is_stderr,
                        } => {
                            if tx
                                .send(SourceEvent::OperationOutput {
                                    instance_id: Some(instance_id),
                                    op_id,
                                    line,
                                    is_stderr,
                                })
                                .await
                                .is_err()
                            {
                                return; // TUI channel closed
                            }
                        }
                        Applied::ChatToken(token) => {
                            if tx.send(SourceEvent::ChatToken { token }).await.is_err() {
                                return;
                            }
                        }
                        Applied::ApprovalRequested { id, tool, args } => {
                            if tx
                                .send(SourceEvent::ApprovalRequested { id, tool, args })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Applied::AgentDone => {
                            if tx.send(SourceEvent::AgentDone).await.is_err() {
                                return;
                            }
                        }
                        Applied::AgentError(error) => {
                            if tx.send(SourceEvent::AgentError { error }).await.is_err() {
                                return;
                            }
                        }
                        Applied::None => {}
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("embedded_hub_source: lagged {n} messages");
                    // Non-fatal — continue from the oldest available message.
                }
                Err(broadcast::error::RecvError::Closed) => return, // Hub shut down
            }
        }
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal state
// ─────────────────────────────────────────────────────────────────────────────

/// Per-instance operation map.
type InstanceOps = HashMap<String, Operation>; // keyed by op_id

struct DaemonState {
    /// Metadata for each registered instance.
    instances: HashMap<String, InstanceInfo>,
    /// Live operations per instance.
    ops: HashMap<String, InstanceOps>, // outer key = instance_id
}

impl DaemonState {
    fn new() -> Self {
        Self {
            instances: HashMap::new(),
            ops: HashMap::new(),
        }
    }

    fn all_instances(&self) -> Vec<InstanceInfo> {
        self.instances.values().cloned().collect()
    }

    fn add_instance(&mut self, info: InstanceInfo) {
        self.ops.entry(info.id.clone()).or_default();
        self.instances.insert(info.id.clone(), info);
    }

    fn remove_instance(&mut self, id: &str) {
        self.instances.remove(id);
        self.ops.remove(id);
    }

    fn on_op_started(
        &mut self,
        instance_id: &str,
        op_id: String,
        tool_name: String,
        description: String,
        scope: Option<String>,
    ) {
        let (label, pid) = self
            .instances
            .get(instance_id)
            .map(|i| (i.label.clone(), Some(i.pid)))
            .unwrap_or_else(|| (instance_id.to_string(), None));

        let mut op = Operation::new(&op_id, &tool_name, OpStatus::Running);
        op.instance_id = Some(instance_id.to_string());
        op.instance_label = Some(label);
        op.pid = pid;
        op.scope = scope;
        op.description = description;

        self.ops
            .entry(instance_id.to_string())
            .or_default()
            .insert(op_id, op);
    }

    fn on_op_finished(
        &mut self,
        instance_id: &str,
        op_id: &str,
        status_str: &str,
        result_summary: Option<String>,
        duration_ms: u64,
    ) {
        if let Some(instance_ops) = self.ops.get_mut(instance_id)
            && let Some(op) = instance_ops.get_mut(op_id)
        {
            op.status = parse_op_status(status_str);
            op.result_summary = result_summary;
            op.duration_ms = Some(duration_ms);
            op.completed_at = Some(std::time::Instant::now());
        }
    }

    /// Flatten all instances' operations into one sorted list.
    fn all_ops(&self) -> Vec<Operation> {
        let mut ops: Vec<Operation> = self
            .ops
            .values()
            .flat_map(|m| m.values().cloned())
            .collect();
        // Sort running first, then by instance_id + op_id for stable ordering.
        ops.sort_by(|a, b| {
            let a_running = matches!(a.status, OpStatus::Running);
            let b_running = matches!(b.status, OpStatus::Running);
            b_running
                .cmp(&a_running)
                .then_with(|| a.instance_id.cmp(&b.instance_id))
                .then_with(|| a.id.cmp(&b.id))
        });
        ops
    }

    fn prune_terminal(&mut self) {
        for instance_ops in self.ops.values_mut() {
            instance_ops.retain(|_, op| {
                matches!(op.status, OpStatus::Running | OpStatus::Pending)
                    || op
                        .completed_at
                        .map(|t| t.elapsed() < Duration::from_secs(300))
                        .unwrap_or(true)
            });
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Task implementation
// ─────────────────────────────────────────────────────────────────────────────

async fn daemon_source_task(tx: mpsc::Sender<SourceEvent>) {
    let mut backoff = Duration::from_secs(5);
    let mut prune_counter: u8 = 0;

    loop {
        // Ensure daemon is running (may spawn it).
        if let Err(e) = ensure_daemon_running().await {
            debug!(
                "daemon_source: daemon unavailable ({e}); retry in {:?}",
                backoff
            );
            let _ = tx
                .send(SourceEvent::DaemonHealthChanged { healthy: false })
                .await;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            continue;
        }

        let stream = match connect_to_daemon().await {
            Ok(s) => s,
            Err(e) => {
                debug!(
                    "daemon_source: connect failed ({e}); retry in {:?}",
                    backoff
                );
                let _ = tx
                    .send(SourceEvent::DaemonHealthChanged { healthy: false })
                    .await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };

        backoff = Duration::from_secs(5);
        debug!("daemon_source: connected");
        let _ = tx
            .send(SourceEvent::DaemonHealthChanged { healthy: true })
            .await;

        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;

        // Subscribe to the event stream.
        if let Err(e) = send_msg(&mut writer, &ClientMsg::Subscribe).await {
            warn!("daemon_source: subscribe failed: {e}");
            let _ = tx
                .send(SourceEvent::DaemonHealthChanged { healthy: false })
                .await;
            continue;
        }

        let mut state = DaemonState::new();

        loop {
            match recv_msg::<_, DaemonMsg>(&mut reader).await {
                Ok(msg) => {
                    let is_instance_change = matches!(
                        msg,
                        DaemonMsg::InstanceList { .. }
                            | DaemonMsg::InstanceRegistered { .. }
                            | DaemonMsg::InstanceUnregistered { .. }
                    );
                    let applied = apply_msg(&mut state, msg);
                    if is_instance_change {
                        let _ = tx
                            .send(SourceEvent::InstancesUpdated {
                                instances: state.all_instances(),
                            })
                            .await;
                    }
                    match applied {
                        Applied::ListChanged => {
                            prune_counter += 1;
                            if prune_counter >= 10 {
                                state.prune_terminal();
                                prune_counter = 0;
                            }
                            let ops = state.all_ops();
                            if tx
                                .send(SourceEvent::OperationsUpdated { ops })
                                .await
                                .is_err()
                            {
                                // Channel closed — TUI exited.
                                return;
                            }
                        }
                        Applied::Output {
                            instance_id,
                            op_id,
                            line,
                            is_stderr,
                        } => {
                            if tx
                                .send(SourceEvent::OperationOutput {
                                    instance_id: Some(instance_id),
                                    op_id,
                                    line,
                                    is_stderr,
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Applied::ChatToken(token) => {
                            if tx.send(SourceEvent::ChatToken { token }).await.is_err() {
                                return;
                            }
                        }
                        Applied::ApprovalRequested { id, tool, args } => {
                            if tx
                                .send(SourceEvent::ApprovalRequested { id, tool, args })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Applied::AgentDone => {
                            if tx.send(SourceEvent::AgentDone).await.is_err() {
                                return;
                            }
                        }
                        Applied::AgentError(error) => {
                            if tx.send(SourceEvent::AgentError { error }).await.is_err() {
                                return;
                            }
                        }
                        Applied::None => {}
                    }
                }
                Err(e) => {
                    debug!("daemon_source: connection lost ({e})");
                    let _ = tx
                        .send(SourceEvent::DaemonHealthChanged { healthy: false })
                        .await;
                    break;
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Result of applying one daemon message to the state.
enum Applied {
    /// Nothing the TUI needs to react to.
    None,
    /// The operation list changed — re-emit the merged snapshot.
    ListChanged,
    /// A live output line — forward incrementally, do NOT re-emit the list.
    Output {
        instance_id: String,
        op_id: String,
        line: String,
        is_stderr: bool,
    },
    ChatToken(String),
    ApprovalRequested {
        id: String,
        tool: String,
        args: String,
    },
    AgentDone,
    AgentError(String),
}

impl Applied {
    /// True when the merged operation list changed and should be re-emitted.
    #[cfg(test)]
    fn is_list_changed(&self) -> bool {
        matches!(self, Applied::ListChanged)
    }
}

/// Apply one daemon message to the state.
fn apply_msg(state: &mut DaemonState, msg: DaemonMsg) -> Applied {
    match msg {
        DaemonMsg::InstanceList { instances } => {
            for info in instances {
                state.add_instance(info);
            }
            // Initial snapshot — emit even if empty so TUI sees "daemon connected".
            Applied::ListChanged
        }
        DaemonMsg::InstanceRegistered { instance } => {
            state.add_instance(instance);
            Applied::ListChanged
        }
        DaemonMsg::InstanceUnregistered { id } => {
            state.remove_instance(&id);
            Applied::ListChanged
        }
        DaemonMsg::Event {
            instance_id,
            payload,
        } => match payload {
            DaemonEvent::OpStarted {
                id,
                tool_name,
                description,
                scope,
            } => {
                state.on_op_started(&instance_id, id, tool_name, description, Some(scope));
                Applied::ListChanged
            }
            DaemonEvent::OpFinished {
                id,
                status,
                result_summary,
                duration_ms,
            } => {
                state.on_op_finished(&instance_id, &id, &status, result_summary, duration_ms);
                Applied::ListChanged
            }
            // Output lines are forwarded incrementally to the TUI and are NOT
            // accumulated in DaemonState: the merged-list snapshot would be
            // re-appended by `upsert_operation` on every re-emit, duplicating
            // lines.  The TUI app state owns the per-operation tail buffer.
            DaemonEvent::OpOutput {
                id,
                line,
                is_stderr,
            } => Applied::Output {
                instance_id,
                op_id: id,
                line,
                is_stderr,
            },
            DaemonEvent::LogLine { .. } => Applied::None, // not yet surfaced in TUI
        },
        DaemonMsg::Ping { .. } => Applied::None, // hub-to-instance ping; no state change for subscribers
        DaemonMsg::ChatToken { token } => Applied::ChatToken(token),
        DaemonMsg::ApprovalRequested { id, tool, args } => {
            Applied::ApprovalRequested { id, tool, args }
        }
        DaemonMsg::AgentDone => Applied::AgentDone,
        DaemonMsg::AgentError { error } => Applied::AgentError(error),
        DaemonMsg::RunPrompt { .. } => Applied::None,
        DaemonMsg::SubmitApproval { .. } => Applied::None,
        // Scope-grant modal is wired in a later PR; ignore for now so the new hub
        // protocol variants do not break the TUI build.
        DaemonMsg::ScopeGrantRequested { .. } => Applied::None,
        DaemonMsg::SubmitScopeGrant { .. } => Applied::None,
        DaemonMsg::ScopeGrantDismiss { .. } => Applied::None,
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn parse_op_status(s: &str) -> OpStatus {
    match s {
        "Completed" => OpStatus::Succeeded,
        "Failed" => OpStatus::Failed,
        "Cancelled" => OpStatus::Cancelled,
        _ => OpStatus::Failed, // TimedOut → Failed for display
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OpStatus;
    use ahma_common::daemon_hub::{DaemonEvent, DaemonMsg, InstanceInfo};

    fn inst(id: &str, label: &str) -> InstanceInfo {
        InstanceInfo {
            id: id.to_string(),
            pid: 1,
            mode: "stdio".to_string(),
            scope: "/test".to_string(),
            label: label.to_string(),
        }
    }

    // ── DaemonState structural mutations ──────────────────────────────────────

    #[test]
    fn add_and_remove_instance() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "VS Code"));
        assert_eq!(s.instances.len(), 1);
        assert!(s.ops.contains_key("i1"));

        s.remove_instance("i1");
        assert!(s.instances.is_empty());
        assert!(s.ops.is_empty());
    }

    #[test]
    fn remove_unknown_instance_is_noop() {
        let mut s = DaemonState::new();
        // Should not panic.
        s.remove_instance("nonexistent");
        assert!(s.instances.is_empty());
    }

    #[test]
    fn op_started_sets_running_with_instance_metadata() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Cursor"));
        s.on_op_started(
            "i1",
            "op-1".to_string(),
            "cargo_build".to_string(),
            "Build".to_string(),
            Some("/test/scope".to_string()),
        );

        let ops = s.all_ops();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].status, OpStatus::Running);
        assert_eq!(ops[0].instance_label, Some("Cursor".to_string()));
        assert_eq!(ops[0].instance_id, Some("i1".to_string()));
        assert_eq!(ops[0].tool_name, "cargo_build");
        assert_eq!(ops[0].scope, Some("/test/scope".to_string()));
    }

    #[test]
    fn op_finished_updates_status() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        s.on_op_started(
            "i1",
            "op-1".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
        );
        s.on_op_finished("i1", "op-1", "Completed", Some("ok".to_string()), 100);

        let ops = s.all_ops();
        assert_eq!(ops[0].status, OpStatus::Succeeded);
        assert_eq!(ops[0].result_summary, Some("ok".to_string()));
        assert_eq!(ops[0].duration_ms, Some(100));
        assert!(ops[0].completed_at.is_some());
    }

    #[test]
    fn op_finished_on_unknown_op_is_noop() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        // Should not panic.
        s.on_op_finished("i1", "nonexistent-op", "Completed", None, 0);
    }

    #[test]
    fn all_ops_running_sorted_before_terminal() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        s.on_op_started(
            "i1",
            "op-a".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
        );
        s.on_op_started(
            "i1",
            "op-b".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
        );
        s.on_op_finished("i1", "op-a", "Completed", None, 0);

        let ops = s.all_ops();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[0].status,
            OpStatus::Running,
            "running first; got {:?}",
            ops[0].id
        );
    }

    #[test]
    fn all_ops_spans_multiple_instances() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "VS Code"));
        s.add_instance(inst("i2", "Cursor"));
        s.on_op_started(
            "i1",
            "op-x".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
        );
        s.on_op_started(
            "i2",
            "op-y".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
        );
        assert_eq!(s.all_ops().len(), 2);
    }

    #[test]
    fn prune_terminal_removes_non_running() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        s.on_op_started(
            "i1",
            "op-1".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
        );
        s.on_op_started(
            "i1",
            "op-2".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
        );
        s.on_op_finished("i1", "op-2", "Failed", None, 0);

        // Modify completed_at so it is older than 300s to trigger pruning
        if let Some(ops) = s.ops.get_mut("i1")
            && let Some(op) = ops.get_mut("op-2")
        {
            op.completed_at = Some(std::time::Instant::now() - Duration::from_secs(301));
        }

        s.prune_terminal();
        let ops = s.all_ops();
        assert_eq!(ops.len(), 1, "only running op remains");
        assert_eq!(ops[0].id, "op-1");
    }

    #[test]
    fn prune_terminal_keeps_pending() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        let mut op = Operation::new("op-p", "tool", OpStatus::Pending);
        op.instance_id = Some("i1".to_string());
        s.ops
            .entry("i1".to_string())
            .or_default()
            .insert("op-p".to_string(), op);

        s.prune_terminal();
        assert_eq!(s.all_ops().len(), 1, "Pending ops survive prune");
    }

    // ── apply_msg routing ─────────────────────────────────────────────────────

    #[test]
    fn apply_msg_instance_list_initial_snapshot() {
        let mut s = DaemonState::new();
        let changed = apply_msg(
            &mut s,
            DaemonMsg::InstanceList {
                instances: vec![inst("i1", "IDE")],
            },
        );
        assert!(changed.is_list_changed(), "InstanceList should flag change");
        assert_eq!(s.instances.len(), 1);
    }

    #[test]
    fn apply_msg_empty_instance_list_signals_change() {
        // Empty InstanceList = "daemon connected" signal → changed = true.
        let mut s = DaemonState::new();
        let changed = apply_msg(&mut s, DaemonMsg::InstanceList { instances: vec![] });
        assert!(
            changed.is_list_changed(),
            "empty InstanceList is still a change (daemon-connected signal)"
        );
    }

    #[test]
    fn apply_msg_instance_registered() {
        let mut s = DaemonState::new();
        let changed = apply_msg(
            &mut s,
            DaemonMsg::InstanceRegistered {
                instance: inst("i2", "IDE"),
            },
        );
        assert!(changed.is_list_changed());
        assert!(s.instances.contains_key("i2"));
    }

    #[test]
    fn apply_msg_instance_unregistered() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i3", "X"));
        let changed = apply_msg(
            &mut s,
            DaemonMsg::InstanceUnregistered {
                id: "i3".to_string(),
            },
        );
        assert!(changed.is_list_changed());
        assert!(s.instances.is_empty());
    }

    #[test]
    fn apply_msg_op_started_then_finished() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));

        let c1 = apply_msg(
            &mut s,
            DaemonMsg::Event {
                instance_id: "i1".to_string(),
                payload: DaemonEvent::OpStarted {
                    id: "op-1".to_string(),
                    tool_name: "tool".to_string(),
                    description: "".to_string(),
                    scope: "/test/scope".to_string(),
                },
            },
        );
        assert!(c1.is_list_changed());
        assert_eq!(s.all_ops().len(), 1);
        assert_eq!(s.all_ops()[0].scope, Some("/test/scope".to_string()));

        let c2 = apply_msg(
            &mut s,
            DaemonMsg::Event {
                instance_id: "i1".to_string(),
                payload: DaemonEvent::OpFinished {
                    id: "op-1".to_string(),
                    status: "Completed".to_string(),
                    result_summary: Some("success".to_string()),
                    duration_ms: 1200,
                },
            },
        );
        assert!(c2.is_list_changed());
        assert_eq!(s.all_ops()[0].status, OpStatus::Succeeded);
        assert_eq!(s.all_ops()[0].result_summary, Some("success".to_string()));
        assert_eq!(s.all_ops()[0].duration_ms, Some(1200));
        assert!(s.all_ops()[0].completed_at.is_some());
    }

    #[test]
    fn apply_msg_log_line_returns_false() {
        let mut s = DaemonState::new();
        let changed = apply_msg(
            &mut s,
            DaemonMsg::Event {
                instance_id: "x".to_string(),
                payload: DaemonEvent::LogLine {
                    level: "info".to_string(),
                    message: "hello".to_string(),
                },
            },
        );
        assert!(
            !changed.is_list_changed(),
            "LogLine should not trigger state change"
        );
    }

    #[test]
    fn apply_msg_op_output_forwards_incrementally() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        apply_msg(
            &mut s,
            DaemonMsg::Event {
                instance_id: "i1".to_string(),
                payload: DaemonEvent::OpStarted {
                    id: "op-1".to_string(),
                    tool_name: "tool".to_string(),
                    description: "".to_string(),
                    scope: "/test".to_string(),
                },
            },
        );

        let applied = apply_msg(
            &mut s,
            DaemonMsg::Event {
                instance_id: "i1".to_string(),
                payload: DaemonEvent::OpOutput {
                    id: "op-1".to_string(),
                    line: "compiling...".to_string(),
                    is_stderr: false,
                },
            },
        );
        match applied {
            Applied::Output {
                instance_id,
                op_id,
                line,
                is_stderr,
            } => {
                assert_eq!(instance_id, "i1");
                assert_eq!(op_id, "op-1");
                assert_eq!(line, "compiling...");
                assert!(!is_stderr);
            }
            _ => panic!("OpOutput must map to Applied::Output"),
        }
        // Output is NOT accumulated in DaemonState (the TUI app state owns
        // the tail buffer) — the snapshot list stays line-free.
        assert!(s.all_ops()[0].stdout_tail.is_empty());
    }

    // ── parse_op_status ────────────────────────────────────────────────────────

    #[test]
    fn parse_op_status_all_variants() {
        assert_eq!(parse_op_status("Completed"), OpStatus::Succeeded);
        assert_eq!(parse_op_status("Failed"), OpStatus::Failed);
        assert_eq!(parse_op_status("Cancelled"), OpStatus::Cancelled);
        assert_eq!(
            parse_op_status("TimedOut"),
            OpStatus::Failed,
            "TimedOut → Failed"
        );
        assert_eq!(
            parse_op_status("Unknown"),
            OpStatus::Failed,
            "Unknown → Failed"
        );
        assert_eq!(parse_op_status(""), OpStatus::Failed, "empty → Failed");
    }
}
