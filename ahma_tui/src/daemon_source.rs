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

/// The identity fields the server computed and put on the wire (SPEC R24.7).
///
/// Bundled rather than passed as four more positional arguments, and named for
/// what they are: things the observer is **given**, not things it works out for
/// itself. Reverse-engineering an operation's name from its id is the bug this
/// whole struct exists to retire.
#[derive(Debug, Clone, Default)]
pub struct OpWireIdentity {
    /// Human title of the command (see `ahma_common::op_identity::title_for`).
    pub title: Option<String>,
    /// Working directory it ran in.
    pub cwd: Option<String>,
    /// The full command, for the detail pane.
    pub command: Option<String>,
    /// Which attached session initiated it.
    pub origin: Option<String>,
}

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
                    if handle_daemon_msg(&mut state, &tx, msg).await.is_none() {
                        return; // TUI channel closed
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

/// Apply one daemon message to `state` and forward the resulting UI
/// event(s) on `tx`.
///
/// Shared by [`spawn_embedded_hub_source`] and [`daemon_source_task`], which
/// otherwise duplicate this instance-change / operation-list-changed
/// dispatch verbatim.
///
/// Returns `None` when the TUI channel closed (caller should stop
/// processing). Returns `Some(true)` when the merged operation list changed
/// — callers that periodically prune terminal operations use this to drive
/// their prune counter — and `Some(false)` otherwise.
async fn handle_daemon_msg(
    state: &mut DaemonState,
    tx: &mpsc::Sender<SourceEvent>,
    msg: DaemonMsg,
) -> Option<bool> {
    let is_instance_change = matches!(
        msg,
        DaemonMsg::InstanceList { .. }
            | DaemonMsg::InstanceRegistered { .. }
            | DaemonMsg::InstanceUnregistered { .. }
    );
    let applied = apply_msg(state, msg);
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
                return None;
            }
            Some(true)
        }
        applied => {
            if let Some(event) = applied_to_event(applied)
                && tx.send(event).await.is_err()
            {
                return None;
            }
            Some(false)
        }
    }
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

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn on_op_started(
        &mut self,
        instance_id: &str,
        op_id: String,
        tool_name: String,
        description: String,
        scope: Option<String>,
        parent_id: Option<String>,
        started_epoch_ms: Option<u64>,
        identity: OpWireIdentity,
    ) {
        let (label, pid) = self
            .instances
            .get(instance_id)
            // Prefer the detected MCP client identity ("claude-code",
            // "cursor") over the generic instance label when available.
            .map(|i| {
                (
                    i.client.clone().unwrap_or_else(|| i.label.clone()),
                    Some(i.pid),
                )
            })
            .unwrap_or_else(|| (instance_id.to_string(), None));

        let mut op = Operation::new(&op_id, &tool_name, OpStatus::Running);
        op.instance_id = Some(instance_id.to_string());
        op.instance_label = Some(label);
        op.pid = pid;
        op.scope = scope;
        op.description = description;
        op.parent_id = parent_id;
        // The server computed these; do not re-derive them (SPEC R24.7).
        op.title = identity.title;
        op.command = identity.command;
        op.origin = identity.origin;
        if op.cwd.is_none() {
            op.cwd = identity.cwd;
        }
        // Back-date the start so a replayed operation (TUI opened after the
        // work began) shows its true elapsed time, not time-since-receipt.
        if let Some((instant, local)) = backdate(started_epoch_ms) {
            if let Some(i) = instant {
                op.started_at = Some(i);
            }
            op.started_time = local;
        }

        self.ops
            .entry(instance_id.to_string())
            .or_default()
            .insert(op_id, op);
    }

    #[allow(clippy::too_many_arguments)]
    fn on_op_finished(
        &mut self,
        instance_id: &str,
        op_id: &str,
        status_str: &str,
        result_summary: Option<String>,
        duration_ms: u64,
        ended_epoch_ms: Option<u64>,
        exit_code: Option<i64>,
        denial: Option<ahma_common::daemon_hub::OpDenial>,
    ) {
        if let Some(instance_ops) = self.ops.get_mut(instance_id)
            && let Some(op) = instance_ops.get_mut(op_id)
        {
            // A denial arrives as status "Failed" plus the denial field (the
            // wire evolves by adding fields only, R24.5). The richer local
            // status wins so the row can say `denied: <path>`.
            op.status = match &denial {
                Some(_) => OpStatus::Denied,
                None => parse_op_status(status_str),
            };
            op.denial = denial.map(|d| (d.path, d.access));
            op.result_summary = result_summary;
            op.duration_ms = Some(duration_ms);
            op.exit_code = exit_code;
            // Back-date replayed completions so the retention window measures
            // from when the operation actually finished.
            op.completed_at = backdate(ended_epoch_ms)
                .and_then(|(instant, _)| instant)
                .or(Some(std::time::Instant::now()));
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
        self.prune_terminal_older_than(TERMINAL_OP_RETENTION);
    }

    /// Core of [`Self::prune_terminal`], parameterized on the retention
    /// window so tests can exercise the boundary with a small duration
    /// instead of subtracting the full production window (1h) from
    /// `Instant::now()` — on a freshly-booted Windows CI runner (`Instant`
    /// there is `QueryPerformanceCounter`-based, i.e. uptime-relative, not
    /// wall-clock) that subtraction can underflow and panic.
    fn prune_terminal_older_than(&mut self, retention: Duration) {
        for instance_ops in self.ops.values_mut() {
            instance_ops.retain(|_, op| {
                matches!(op.status, OpStatus::Running | OpStatus::Pending)
                    || op
                        .completed_at
                        .map(|t| t.elapsed() < retention)
                        .unwrap_or(true)
            });
        }
    }
}

/// How long finished operations stay in the merged view. Long enough that a
/// user opening the TUI mid-session sees the recent history of what ran on
/// their behalf, not just what is running right now.
const TERMINAL_OP_RETENTION: Duration = Duration::from_secs(3600);

/// Convert a wire epoch-ms timestamp into a back-dated (`Instant`, local time)
/// pair. The `Instant` is `None` when the timestamp is in the future (clock
/// skew) or predates what a monotonic-clock subtraction can represent.
fn backdate(
    epoch_ms: Option<u64>,
) -> Option<(Option<std::time::Instant>, chrono::DateTime<chrono::Local>)> {
    let ms = epoch_ms?;
    let sys = std::time::UNIX_EPOCH + Duration::from_millis(ms);
    let local = chrono::DateTime::<chrono::Local>::from(sys);
    let instant = std::time::SystemTime::now()
        .duration_since(sys)
        .ok()
        .and_then(|age| std::time::Instant::now().checked_sub(age));
    Some((instant, local))
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
                Ok(msg) => match handle_daemon_msg(&mut state, &tx, msg).await {
                    Some(true) => {
                        prune_counter += 1;
                        if prune_counter >= 10 {
                            state.prune_terminal();
                            prune_counter = 0;
                        }
                    }
                    Some(false) => {}
                    None => return, // Channel closed — TUI exited.
                },
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
    ChatThinking(String),
    ApprovalRequested {
        id: String,
        tool: String,
        args: String,
    },
    ScopeGrantRequested {
        request: ahma_common::scope_grant::ScopeGrantRequest,
    },
    ScopeGrantDismiss {
        decision_id: String,
    },
    WebApprovalRequested {
        request: ahma_common::web_approval::WebApprovalRequest,
    },
    WebApprovalDismiss {
        decision_id: String,
    },
    AgentDone,
    AgentError(String),
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        total_tokens: u32,
    },
    ToolCallStarted {
        id: String,
        name: String,
        args: String,
    },
    ToolCallFinished {
        id: String,
        result: String,
        failed: bool,
    },
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
                parent_id,
                started_epoch_ms,
                title,
                cwd,
                command,
                origin,
            } => {
                state.on_op_started(
                    &instance_id,
                    id,
                    tool_name,
                    description,
                    Some(scope),
                    parent_id,
                    started_epoch_ms,
                    OpWireIdentity {
                        title,
                        cwd,
                        command,
                        origin,
                    },
                );
                Applied::ListChanged
            }
            DaemonEvent::OpFinished {
                id,
                status,
                result_summary,
                duration_ms,
                ended_epoch_ms,
                exit_code,
                denial,
            } => {
                state.on_op_finished(
                    &instance_id,
                    &id,
                    &status,
                    result_summary,
                    duration_ms,
                    ended_epoch_ms,
                    exit_code,
                    denial,
                );
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
        // Instance-directed: the hub routes a TUI's re-raise request to the
        // instance that owns the path. A subscriber seeing it has nothing to do
        // — the re-raised question arrives as a normal ScopeGrantRequested.
        DaemonMsg::ReRaiseScopeGrant { .. } => Applied::None,
        DaemonMsg::ChatToken { token } => Applied::ChatToken(token),
        DaemonMsg::ChatThinking { token } => Applied::ChatThinking(token),
        DaemonMsg::ApprovalRequested { id, tool, args } => {
            Applied::ApprovalRequested { id, tool, args }
        }
        DaemonMsg::AgentDone => Applied::AgentDone,
        DaemonMsg::AgentError { error } => Applied::AgentError(error),
        DaemonMsg::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        } => Applied::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        },
        DaemonMsg::ToolCallStarted { id, name, args } => {
            Applied::ToolCallStarted { id, name, args }
        }
        DaemonMsg::ToolCallFinished { id, result, failed } => {
            Applied::ToolCallFinished { id, result, failed }
        }
        DaemonMsg::RunPrompt { .. } => Applied::None,
        DaemonMsg::SubmitApproval { .. } => Applied::None,
        DaemonMsg::ScopeGrantRequested { request } => Applied::ScopeGrantRequested { request },
        DaemonMsg::ScopeGrantDismiss { decision_id } => Applied::ScopeGrantDismiss { decision_id },
        DaemonMsg::WebApprovalRequested { request } => Applied::WebApprovalRequested { request },
        DaemonMsg::WebApprovalDismiss { decision_id } => {
            Applied::WebApprovalDismiss { decision_id }
        }
        // Instance-bound; a subscriber never receives it.
        DaemonMsg::SubmitScopeGrant { .. } | DaemonMsg::SubmitWebApproval { .. } => Applied::None,
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Translate an [`Applied`] outcome into the [`SourceEvent`] the TUI should
/// receive, or `None` when nothing should be forwarded.
///
/// `Applied::ListChanged` is handled separately by callers because it needs
/// access to `DaemonState::all_ops()` (and, in [`daemon_source_task`], a
/// prune counter) rather than data carried on the `Applied` value itself.
fn applied_to_event(applied: Applied) -> Option<SourceEvent> {
    match applied {
        Applied::None | Applied::ListChanged => None,
        Applied::Output {
            instance_id,
            op_id,
            line,
            is_stderr,
        } => Some(SourceEvent::OperationOutput {
            instance_id: Some(instance_id),
            op_id,
            line,
            is_stderr,
        }),
        Applied::ChatToken(token) => Some(SourceEvent::ChatToken { token }),
        Applied::ChatThinking(token) => Some(SourceEvent::ChatThinking { token }),
        Applied::ApprovalRequested { id, tool, args } => {
            Some(SourceEvent::ApprovalRequested { id, tool, args })
        }
        Applied::ScopeGrantRequested { request } => {
            Some(SourceEvent::ScopeGrantRequested { request })
        }
        Applied::ScopeGrantDismiss { decision_id } => {
            Some(SourceEvent::ScopeGrantDismiss { decision_id })
        }
        Applied::WebApprovalRequested { request } => {
            Some(SourceEvent::WebApprovalRequested { request })
        }
        Applied::WebApprovalDismiss { decision_id } => {
            Some(SourceEvent::WebApprovalDismiss { decision_id })
        }
        Applied::AgentDone => Some(SourceEvent::AgentDone),
        Applied::AgentError(error) => Some(SourceEvent::AgentError { error }),
        Applied::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        } => Some(SourceEvent::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        }),
        Applied::ToolCallStarted { id, name, args } => {
            Some(SourceEvent::ToolCallStarted { id, name, args })
        }
        Applied::ToolCallFinished { id, result, failed } => {
            Some(SourceEvent::ToolCallFinished { id, result, failed })
        }
    }
}

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
            client: None,
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
            None,
            None,
            OpWireIdentity::default(),
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
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_finished(
            "i1",
            "op-1",
            "Completed",
            Some("ok".to_string()),
            100,
            None,
            Some(0),
            None,
        );

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
        s.on_op_finished(
            "i1",
            "nonexistent-op",
            "Completed",
            None,
            0,
            None,
            None,
            None,
        );
    }

    /// A denial arrives as status "Failed" plus the `denial` field (the wire
    /// evolves by adding fields only). The richer local status must win, so the
    /// row can say *which path* was refused instead of a bare "failed", and so
    /// the grant question can be re-raised for that pair (SPEC R-PERM.7/.7.1).
    #[test]
    fn denial_field_promotes_failed_to_denied() {
        use ahma_common::daemon_hub::OpDenial;
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        s.on_op_started(
            "i1",
            "op-1".to_string(),
            "run_terminal_command".to_string(),
            "touch /etc/foo".to_string(),
            None,
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_finished(
            "i1",
            "op-1",
            "Failed",
            Some("Operation not permitted".into()),
            5,
            None,
            None,
            Some(OpDenial {
                path: "/etc".into(),
                access: "rw".into(),
            }),
        );

        let ops = s.all_ops();
        assert_eq!(ops[0].status, OpStatus::Denied);
        assert_eq!(ops[0].denial, Some(("/etc".into(), "rw".into())));
        // The shared identity renderer must say "denied", not "failed".
        let line = ops[0].identity().render(false);
        assert!(line.contains("denied"), "identity line: {line}");
        assert!(
            line.contains("/etc"),
            "identity line names the path: {line}"
        );
    }

    /// An ordinary failure with no denial field stays a failure — the promotion
    /// is driven by the field, never guessed from the status word.
    #[test]
    fn plain_failure_without_denial_stays_failed() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        s.on_op_started(
            "i1",
            "op-1".to_string(),
            "cargo".to_string(),
            "cargo build".to_string(),
            None,
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_finished(
            "i1",
            "op-1",
            "Failed",
            Some("boom".into()),
            5,
            None,
            None,
            None,
        );
        assert_eq!(s.all_ops()[0].status, OpStatus::Failed);
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
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_started(
            "i1",
            "op-b".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_finished("i1", "op-a", "Completed", None, 0, None, None, None);

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
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_started(
            "i2",
            "op-y".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
            None,
            None,
            OpWireIdentity::default(),
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
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_started(
            "i1",
            "op-2".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_finished("i1", "op-2", "Failed", None, 0, None, None, None);

        // Stamp op-2's completion now, then let real time pass past a tiny
        // retention window — exercises the same "older than retention"
        // boundary as production's TERMINAL_OP_RETENTION without ever
        // subtracting a duration from `Instant::now()`, which can underflow
        // and panic on a freshly-booted Windows CI runner (`Instant` there is
        // `QueryPerformanceCounter`-based, i.e. uptime-relative, not
        // wall-clock; see `prune_terminal_older_than` doc comment).
        if let Some(ops) = s.ops.get_mut("i1")
            && let Some(op) = ops.get_mut("op-2")
        {
            op.completed_at = Some(std::time::Instant::now());
        }
        std::thread::sleep(Duration::from_millis(20));

        s.prune_terminal_older_than(Duration::from_millis(1));
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
                    parent_id: None,
                    started_epoch_ms: None,
                    title: None,
                    cwd: None,
                    command: None,
                    origin: None,
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
                    ended_epoch_ms: None,
                    exit_code: None,
                    denial: None,
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
                    parent_id: None,
                    started_epoch_ms: None,
                    title: None,
                    cwd: None,
                    command: None,
                    origin: None,
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

    // ── coverage batch: apply_msg arms, DaemonState branches, embedded hub ─────
    async fn next_ev(rx: &mut mpsc::Receiver<SourceEvent>) -> SourceEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a SourceEvent should arrive within the timeout")
            .expect("the source channel should remain open")
    }

    fn sample_scope_grant() -> ahma_common::scope_grant::ScopeGrantRequest {
        ahma_common::scope_grant::ScopeGrantRequest {
            decision_id: "d1".to_string(),
            path: std::env::temp_dir().join("ahma_scope_grant_test"),
            access: ahma_common::config::ScopeAccess::Ro,
            reason: ahma_common::scope_grant::GrantReason::PreExecViolation,
            tool: Some("run_terminal_command".to_string()),
        }
    }

    fn sample_web_approval() -> ahma_common::web_approval::WebApprovalRequest {
        ahma_common::web_approval::WebApprovalRequest {
            decision_id: "w1".to_string(),
            domain: "api.github.com".to_string(),
            url: "https://api.github.com/repos".to_string(),
            tool: Some("fetch_webpage".to_string()),
        }
    }

    /// Point `AHMA_DAEMON_SOCK` (Unix) / `AHMA_DAEMON_PORT` (Windows) at a
    /// throwaway location so this test's embedded hub cannot collide with a
    /// real daemon or with another test's hub. Mirrors the isolation done by
    /// `ahma_common::daemon_hub::init_test_daemon_isolation` (a `#[cfg(test)]`
    /// item private to that crate and thus unavailable here).
    ///
    /// The returned `TempDir` must be kept alive for the duration of the test.
    fn isolate_daemon_socket_for_test() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir for isolated daemon socket");
        let sock_path = dir.path().join("daemon_test.sock");
        // SAFETY: debug-only test seam; nextest isolates each test in its own process.
        unsafe {
            std::env::set_var("AHMA_DAEMON_SOCK", &sock_path);
        }
        if let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0")
            && let Ok(addr) = listener.local_addr()
        {
            // SAFETY: debug-only test seam; nextest isolates each test in its own process.
            unsafe {
                std::env::set_var("AHMA_DAEMON_PORT", addr.port().to_string());
            }
        }
        dir
    }

    #[test]
    fn all_instances_returns_clones_of_registered() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "VS Code"));
        s.add_instance(inst("i2", "Cursor"));
        let mut got: Vec<String> = s.all_instances().into_iter().map(|i| i.id).collect();
        got.sort();
        assert_eq!(got, vec!["i1".to_string(), "i2".to_string()]);
    }

    #[test]
    fn op_started_unknown_instance_uses_id_as_label_and_no_pid() {
        let mut s = DaemonState::new();
        s.on_op_started(
            "ghost-instance",
            "op-1".to_string(),
            "tool".to_string(),
            "desc".to_string(),
            None,
            None,
            None,
            OpWireIdentity::default(),
        );
        let ops = s.all_ops();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].instance_label, Some("ghost-instance".to_string()));
        assert_eq!(ops[0].instance_id, Some("ghost-instance".to_string()));
        assert_eq!(ops[0].pid, None);
        assert_eq!(ops[0].description, "desc");
        assert_eq!(ops[0].scope, None);
    }

    #[test]
    fn prune_terminal_keeps_recently_completed() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        s.on_op_started(
            "i1",
            "op-1".to_string(),
            "tool".to_string(),
            "".to_string(),
            None,
            None,
            None,
            OpWireIdentity::default(),
        );
        s.on_op_finished("i1", "op-1", "Completed", None, 5, None, None, None);
        s.prune_terminal();
        let ops = s.all_ops();
        assert_eq!(ops.len(), 1, "recently completed op is retained");
        assert_eq!(ops[0].status, OpStatus::Succeeded);
    }

    #[test]
    fn prune_terminal_keeps_terminal_without_completed_at() {
        let mut s = DaemonState::new();
        s.add_instance(inst("i1", "Test"));
        let mut op = Operation::new("op-x", "tool", OpStatus::Failed);
        op.instance_id = Some("i1".to_string());
        op.completed_at = None;
        s.ops
            .entry("i1".to_string())
            .or_default()
            .insert("op-x".to_string(), op);
        s.prune_terminal();
        assert_eq!(
            s.all_ops().len(),
            1,
            "terminal op without completed_at is kept"
        );
    }

    #[test]
    fn apply_msg_ping_is_noop() {
        let mut s = DaemonState::new();
        let a = apply_msg(&mut s, DaemonMsg::Ping { seq: 7 });
        assert!(matches!(a, Applied::None));
        assert!(!a.is_list_changed());
    }

    #[test]
    fn apply_msg_chat_token_carries_token() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::ChatToken {
                token: "hi".to_string(),
            },
        ) {
            Applied::ChatToken(t) => assert_eq!(t, "hi"),
            _ => panic!("ChatToken must map to Applied::ChatToken"),
        }
    }

    #[test]
    fn apply_msg_usage_carries_token_counts() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::Usage {
                prompt_tokens: 10,
                completion_tokens: 3,
                total_tokens: 13,
            },
        ) {
            Applied::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            } => {
                assert_eq!(
                    (prompt_tokens, completion_tokens, total_tokens),
                    (10, 3, 13)
                );
            }
            _ => panic!("Usage must map to Applied::Usage"),
        }
    }

    #[test]
    fn apply_msg_tool_call_lifecycle_carries_fields() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::ToolCallStarted {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                args: "{}".to_string(),
            },
        ) {
            Applied::ToolCallStarted { id, name, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read_file");
            }
            _ => panic!("ToolCallStarted must map to Applied::ToolCallStarted"),
        }
        match apply_msg(
            &mut s,
            DaemonMsg::ToolCallFinished {
                id: "t1".to_string(),
                result: "ok".to_string(),
                failed: false,
            },
        ) {
            Applied::ToolCallFinished { id, failed, .. } => {
                assert_eq!(id, "t1");
                assert!(!failed);
            }
            _ => panic!("ToolCallFinished must map to Applied::ToolCallFinished"),
        }
    }

    #[test]
    fn apply_msg_approval_requested_carries_fields() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::ApprovalRequested {
                id: "a1".to_string(),
                tool: "shell".to_string(),
                args: "ls".to_string(),
            },
        ) {
            Applied::ApprovalRequested { id, tool, args } => {
                assert_eq!(id, "a1");
                assert_eq!(tool, "shell");
                assert_eq!(args, "ls");
            }
            _ => panic!("must map to Applied::ApprovalRequested"),
        }
    }

    #[test]
    fn apply_msg_agent_done() {
        let mut s = DaemonState::new();
        assert!(matches!(
            apply_msg(&mut s, DaemonMsg::AgentDone),
            Applied::AgentDone
        ));
    }

    #[test]
    fn apply_msg_agent_error_carries_message() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::AgentError {
                error: "boom".to_string(),
            },
        ) {
            Applied::AgentError(e) => assert_eq!(e, "boom"),
            _ => panic!("must map to Applied::AgentError"),
        }
    }

    #[test]
    fn apply_msg_run_prompt_is_noop() {
        let mut s = DaemonState::new();
        let a = apply_msg(
            &mut s,
            DaemonMsg::RunPrompt {
                messages: vec![],
                system_prompt: None,
                provider: None,
                model: None,
            },
        );
        assert!(matches!(a, Applied::None));
    }

    #[test]
    fn apply_msg_submit_approval_is_noop() {
        let mut s = DaemonState::new();
        let a = apply_msg(
            &mut s,
            DaemonMsg::SubmitApproval {
                id: None,
                approved: true,
            },
        );
        assert!(matches!(a, Applied::None));
    }

    #[test]
    fn apply_msg_scope_grant_requested_carries_request() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::ScopeGrantRequested {
                request: sample_scope_grant(),
            },
        ) {
            Applied::ScopeGrantRequested { request } => {
                assert_eq!(request.decision_id, "d1");
                assert_eq!(request.access, ahma_common::config::ScopeAccess::Ro);
            }
            _ => panic!("must map to Applied::ScopeGrantRequested"),
        }
    }

    #[test]
    fn apply_msg_scope_grant_dismiss_carries_decision_id() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::ScopeGrantDismiss {
                decision_id: "d9".to_string(),
            },
        ) {
            Applied::ScopeGrantDismiss { decision_id } => assert_eq!(decision_id, "d9"),
            _ => panic!("must map to Applied::ScopeGrantDismiss"),
        }
    }

    #[test]
    fn apply_msg_submit_scope_grant_is_noop() {
        let mut s = DaemonState::new();
        let a = apply_msg(
            &mut s,
            DaemonMsg::SubmitScopeGrant {
                decision_id: "d1".to_string(),
                decision: ahma_common::scope_grant::GrantDecision::Deny,
            },
        );
        assert!(matches!(a, Applied::None));
    }

    #[tokio::test]
    async fn embedded_hub_source_maps_every_event() {
        let (tx_b, rx_b) = broadcast::channel::<DaemonMsg>(64);
        let (tx_s, mut rx_s) = mpsc::channel::<SourceEvent>(64);
        spawn_embedded_hub_source(rx_b, tx_s);

        match next_ev(&mut rx_s).await {
            SourceEvent::DaemonHealthChanged { healthy } => assert!(healthy),
            other => panic!("expected DaemonHealthChanged, got {other:?}"),
        }

        tx_b.send(DaemonMsg::InstanceList {
            instances: vec![inst("i1", "IDE")],
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::InstancesUpdated { instances } => assert_eq!(instances.len(), 1),
            other => panic!("expected InstancesUpdated, got {other:?}"),
        }
        match next_ev(&mut rx_s).await {
            SourceEvent::OperationsUpdated { ops } => assert!(ops.is_empty()),
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        tx_b.send(DaemonMsg::Event {
            instance_id: "i1".to_string(),
            payload: DaemonEvent::OpStarted {
                id: "op1".to_string(),
                tool_name: "t".to_string(),
                description: "d".to_string(),
                scope: "/s".to_string(),
                parent_id: None,
                started_epoch_ms: None,
                title: None,
                cwd: None,
                command: None,
                origin: None,
            },
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::OperationsUpdated { ops } => {
                assert_eq!(ops.len(), 1);
                assert_eq!(ops[0].id, "op1");
                assert_eq!(ops[0].status, OpStatus::Running);
            }
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        tx_b.send(DaemonMsg::Event {
            instance_id: "i1".to_string(),
            payload: DaemonEvent::OpOutput {
                id: "op1".to_string(),
                line: "hello".to_string(),
                is_stderr: true,
            },
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::OperationOutput {
                instance_id,
                op_id,
                line,
                is_stderr,
            } => {
                assert_eq!(instance_id, Some("i1".to_string()));
                assert_eq!(op_id, "op1");
                assert_eq!(line, "hello");
                assert!(is_stderr);
            }
            other => panic!("expected OperationOutput, got {other:?}"),
        }

        tx_b.send(DaemonMsg::ChatToken {
            token: "tok".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::ChatToken { token } => assert_eq!(token, "tok"),
            other => panic!("expected ChatToken, got {other:?}"),
        }

        tx_b.send(DaemonMsg::Ping { seq: 3 }).unwrap();
        tx_b.send(DaemonMsg::ApprovalRequested {
            id: "a1".to_string(),
            tool: "tool".to_string(),
            args: "args".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::ApprovalRequested { id, tool, args } => {
                assert_eq!(id, "a1");
                assert_eq!(tool, "tool");
                assert_eq!(args, "args");
            }
            other => panic!("Ping must be a no-op; got {other:?}"),
        }

        tx_b.send(DaemonMsg::ScopeGrantRequested {
            request: sample_scope_grant(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::ScopeGrantRequested { request } => {
                assert_eq!(request.decision_id, "d1");
            }
            other => panic!("expected ScopeGrantRequested, got {other:?}"),
        }

        tx_b.send(DaemonMsg::ScopeGrantDismiss {
            decision_id: "d1".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::ScopeGrantDismiss { decision_id } => assert_eq!(decision_id, "d1"),
            other => panic!("expected ScopeGrantDismiss, got {other:?}"),
        }

        tx_b.send(DaemonMsg::AgentDone).unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::AgentDone => {}
            other => panic!("expected AgentDone, got {other:?}"),
        }

        tx_b.send(DaemonMsg::AgentError {
            error: "boom".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::AgentError { error } => assert_eq!(error, "boom"),
            other => panic!("expected AgentError, got {other:?}"),
        }

        tx_b.send(DaemonMsg::InstanceUnregistered {
            id: "i1".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::InstancesUpdated { instances } => assert!(instances.is_empty()),
            other => panic!("expected InstancesUpdated, got {other:?}"),
        }
        match next_ev(&mut rx_s).await {
            SourceEvent::OperationsUpdated { ops } => assert!(ops.is_empty()),
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn embedded_hub_source_recovers_after_lag() {
        let (tx_b, rx_b) = broadcast::channel::<DaemonMsg>(2);
        let (tx_s, mut rx_s) = mpsc::channel::<SourceEvent>(256);
        spawn_embedded_hub_source(rx_b, tx_s);

        for i in 0..8 {
            tx_b.send(DaemonMsg::ChatToken {
                token: format!("t{i}"),
            })
            .unwrap();
        }
        tx_b.send(DaemonMsg::ChatToken {
            token: "final".to_string(),
        })
        .unwrap();

        let mut saw_final = false;
        for _ in 0..32 {
            match tokio::time::timeout(Duration::from_secs(2), rx_s.recv()).await {
                Ok(Some(SourceEvent::ChatToken { token })) if token == "final" => {
                    saw_final = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(
            saw_final,
            "embedded hub source must continue past a Lagged error and deliver later tokens"
        );
    }

    #[tokio::test]
    async fn embedded_hub_source_exits_when_tui_channel_closed() {
        let (tx_b, rx_b) = broadcast::channel::<DaemonMsg>(16);
        let (tx_s, rx_s) = mpsc::channel::<SourceEvent>(8);
        spawn_embedded_hub_source(rx_b, tx_s);

        drop(rx_s);
        tx_b.send(DaemonMsg::InstanceList { instances: vec![] })
            .unwrap();

        let mut exited = false;
        for _ in 0..100 {
            if tx_b.receiver_count() == 0 {
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            exited,
            "task must exit (drop its receiver) once the TUI channel is closed"
        );
    }

    // ── apply_msg: remaining variants (ChatThinking, WebApproval*) ─────────────

    #[test]
    fn apply_msg_chat_thinking_carries_token() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::ChatThinking {
                token: "pondering".to_string(),
            },
        ) {
            Applied::ChatThinking(t) => assert_eq!(t, "pondering"),
            _ => panic!("ChatThinking must map to Applied::ChatThinking"),
        }
    }

    #[test]
    fn apply_msg_web_approval_requested_carries_request() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::WebApprovalRequested {
                request: sample_web_approval(),
            },
        ) {
            Applied::WebApprovalRequested { request } => {
                assert_eq!(request.decision_id, "w1");
                assert_eq!(request.domain, "api.github.com");
                assert_eq!(request.tool, Some("fetch_webpage".to_string()));
            }
            _ => panic!("must map to Applied::WebApprovalRequested"),
        }
    }

    #[test]
    fn apply_msg_web_approval_dismiss_carries_decision_id() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::WebApprovalDismiss {
                decision_id: "w9".to_string(),
            },
        ) {
            Applied::WebApprovalDismiss { decision_id } => assert_eq!(decision_id, "w9"),
            _ => panic!("must map to Applied::WebApprovalDismiss"),
        }
    }

    #[test]
    fn apply_msg_submit_web_approval_is_noop() {
        let mut s = DaemonState::new();
        let a = apply_msg(
            &mut s,
            DaemonMsg::SubmitWebApproval {
                decision_id: "w1".to_string(),
                decision: ahma_common::web_approval::WebApprovalDecision::Deny,
            },
        );
        assert!(matches!(a, Applied::None));
    }

    // ── embedded hub: variants not covered by `embedded_hub_source_maps_every_event` ──

    #[tokio::test]
    async fn embedded_hub_source_maps_remaining_events() {
        let (tx_b, rx_b) = broadcast::channel::<DaemonMsg>(64);
        let (tx_s, mut rx_s) = mpsc::channel::<SourceEvent>(64);
        spawn_embedded_hub_source(rx_b, tx_s);

        match next_ev(&mut rx_s).await {
            SourceEvent::DaemonHealthChanged { healthy } => assert!(healthy),
            other => panic!("expected DaemonHealthChanged, got {other:?}"),
        }

        tx_b.send(DaemonMsg::ChatThinking {
            token: "pondering".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::ChatThinking { token } => assert_eq!(token, "pondering"),
            other => panic!("expected ChatThinking, got {other:?}"),
        }

        tx_b.send(DaemonMsg::WebApprovalRequested {
            request: sample_web_approval(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::WebApprovalRequested { request } => {
                assert_eq!(request.decision_id, "w1");
            }
            other => panic!("expected WebApprovalRequested, got {other:?}"),
        }

        tx_b.send(DaemonMsg::WebApprovalDismiss {
            decision_id: "w1".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::WebApprovalDismiss { decision_id } => assert_eq!(decision_id, "w1"),
            other => panic!("expected WebApprovalDismiss, got {other:?}"),
        }

        tx_b.send(DaemonMsg::ToolCallStarted {
            id: "t1".to_string(),
            name: "read_file".to_string(),
            args: "{}".to_string(),
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::ToolCallStarted { id, name, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }

        tx_b.send(DaemonMsg::ToolCallFinished {
            id: "t1".to_string(),
            result: "ok".to_string(),
            failed: true,
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::ToolCallFinished { id, failed, .. } => {
                assert_eq!(id, "t1");
                assert!(failed);
            }
            other => panic!("expected ToolCallFinished, got {other:?}"),
        }

        tx_b.send(DaemonMsg::Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        })
        .unwrap();
        match next_ev(&mut rx_s).await {
            SourceEvent::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            } => {
                assert_eq!((prompt_tokens, completion_tokens, total_tokens), (1, 2, 3));
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    // ── daemon_source_task: full round trip over a real (isolated) socket ──────
    //
    // Exercises the actual connect → Subscribe → recv loop in
    // `daemon_source_task`, not just `apply_msg`/`Applied` in isolation. This
    // covers the socket-framed match arms (lines that forward each `Applied`
    // variant into a `SourceEvent` send) that the pure in-process
    // `embedded_hub_source_*` tests above cannot reach, because that source
    // takes a different code path (`spawn_embedded_hub_source`, no socket).
    //
    // A second raw connection plays the role of a registered ahma instance,
    // driving the hub exactly the way a real `ahma` process would.
    #[tokio::test]
    async fn daemon_source_task_full_round_trip_over_socket() {
        let _isolation_guard = isolate_daemon_socket_for_test();

        let hub = ahma_common::daemon_hub::try_start_hub_server_at(
            ahma_common::daemon_hub::default_socket_path(),
        )
        .await
        .expect("binding the isolated test socket should not error")
        .expect("this test owns a freshly isolated socket, so bind must succeed");

        let (tx, mut rx) = mpsc::channel::<SourceEvent>(64);
        let task = tokio::spawn(daemon_source_task(tx));

        // Connect succeeds → health true, then the initial (empty) snapshot.
        match next_ev(&mut rx).await {
            SourceEvent::DaemonHealthChanged { healthy } => assert!(healthy),
            other => panic!("expected DaemonHealthChanged, got {other:?}"),
        }
        match next_ev(&mut rx).await {
            SourceEvent::InstancesUpdated { instances } => assert!(instances.is_empty()),
            other => panic!("expected InstancesUpdated, got {other:?}"),
        }
        match next_ev(&mut rx).await {
            SourceEvent::OperationsUpdated { ops } => assert!(ops.is_empty()),
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        // Give the server a beat to finish `hub.broadcast.subscribe()` (it runs
        // immediately after writing the InstanceList snapshot, with no other
        // await point in between) before a second connection starts broadcasting
        // — otherwise events raised in the gap would be silently dropped.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A second raw connection plays the role of a registered ahma instance.
        let inst_stream = ahma_common::daemon_hub::connect_to_daemon()
            .await
            .expect("instance connect");
        let (_inst_r, mut inst_w) = tokio::io::split(inst_stream);
        send_msg(
            &mut inst_w,
            &ClientMsg::Register {
                pid: 42,
                mode: "stdio".to_string(),
                scope: "/test".to_string(),
                label: "IntegrationInstance".to_string(),
                client: None,
            },
        )
        .await
        .unwrap();

        match next_ev(&mut rx).await {
            SourceEvent::InstancesUpdated { instances } => assert_eq!(instances.len(), 1),
            other => panic!("expected InstancesUpdated, got {other:?}"),
        }
        match next_ev(&mut rx).await {
            SourceEvent::OperationsUpdated { ops } => assert!(ops.is_empty()),
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        // OpStarted → ListChanged → OperationsUpdated with one running op.
        send_msg(
            &mut inst_w,
            &ClientMsg::Event {
                payload: DaemonEvent::OpStarted {
                    id: "op-1".to_string(),
                    tool_name: "cargo_build".to_string(),
                    description: "Build".to_string(),
                    scope: "/test/scope".to_string(),
                    parent_id: None,
                    started_epoch_ms: None,
                    title: None,
                    cwd: None,
                    command: None,
                    origin: None,
                },
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::OperationsUpdated { ops } => {
                assert_eq!(ops.len(), 1);
                assert_eq!(ops[0].status, OpStatus::Running);
                assert_eq!(ops[0].id, "op-1");
            }
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        // OpOutput → forwarded incrementally as OperationOutput.
        send_msg(
            &mut inst_w,
            &ClientMsg::Event {
                payload: DaemonEvent::OpOutput {
                    id: "op-1".to_string(),
                    line: "compiling".to_string(),
                    is_stderr: false,
                },
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::OperationOutput {
                op_id,
                line,
                is_stderr,
                ..
            } => {
                assert_eq!(op_id, "op-1");
                assert_eq!(line, "compiling");
                assert!(!is_stderr);
            }
            other => panic!("expected OperationOutput, got {other:?}"),
        }

        // OpFinished → ListChanged → OperationsUpdated with the terminal status.
        send_msg(
            &mut inst_w,
            &ClientMsg::Event {
                payload: DaemonEvent::OpFinished {
                    id: "op-1".to_string(),
                    status: "Completed".to_string(),
                    result_summary: Some("ok".to_string()),
                    duration_ms: 42,
                    ended_epoch_ms: None,
                    exit_code: None,
                    denial: None,
                },
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::OperationsUpdated { ops } => {
                assert_eq!(ops[0].status, OpStatus::Succeeded);
                assert_eq!(ops[0].result_summary, Some("ok".to_string()));
            }
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        // A LogLine event is a documented no-op (not yet surfaced in the TUI) —
        // sending one here proves it does NOT produce a spurious SourceEvent by
        // checking the *next* observable event still lines up with what follows.
        send_msg(
            &mut inst_w,
            &ClientMsg::Event {
                payload: DaemonEvent::LogLine {
                    level: "info".to_string(),
                    message: "hello".to_string(),
                },
            },
        )
        .await
        .unwrap();

        send_msg(
            &mut inst_w,
            &ClientMsg::ChatToken {
                token: "hi".to_string(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ChatToken { token } => assert_eq!(token, "hi"),
            other => panic!("LogLine must be a no-op; expected ChatToken, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::ChatThinking {
                token: "pondering".to_string(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ChatThinking { token } => assert_eq!(token, "pondering"),
            other => panic!("expected ChatThinking, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::ApprovalRequested {
                id: "a1".to_string(),
                tool: "shell".to_string(),
                args: "ls".to_string(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ApprovalRequested { id, tool, args } => {
                assert_eq!(id, "a1");
                assert_eq!(tool, "shell");
                assert_eq!(args, "ls");
            }
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::ScopeGrantRequested {
                request: sample_scope_grant(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ScopeGrantRequested { request } => {
                assert_eq!(request.decision_id, "d1");
            }
            other => panic!("expected ScopeGrantRequested, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::ScopeGrantResolved {
                decision_id: "d1".to_string(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ScopeGrantDismiss { decision_id } => assert_eq!(decision_id, "d1"),
            other => panic!("expected ScopeGrantDismiss, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::WebApprovalRequested {
                request: sample_web_approval(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::WebApprovalRequested { request } => {
                assert_eq!(request.decision_id, "w1");
            }
            other => panic!("expected WebApprovalRequested, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::WebApprovalResolved {
                decision_id: "w1".to_string(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::WebApprovalDismiss { decision_id } => assert_eq!(decision_id, "w1"),
            other => panic!("expected WebApprovalDismiss, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::ToolCallStarted {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                args: "{}".to_string(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ToolCallStarted { id, name, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::ToolCallFinished {
                id: "t1".to_string(),
                result: "ok".to_string(),
                failed: false,
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ToolCallFinished { id, failed, .. } => {
                assert_eq!(id, "t1");
                assert!(!failed);
            }
            other => panic!("expected ToolCallFinished, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            } => {
                assert_eq!(
                    (prompt_tokens, completion_tokens, total_tokens),
                    (10, 5, 15)
                );
            }
            other => panic!("expected Usage, got {other:?}"),
        }

        send_msg(&mut inst_w, &ClientMsg::AgentDone).await.unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::AgentDone => {}
            other => panic!("expected AgentDone, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::AgentError {
                error: "boom".to_string(),
            },
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::AgentError { error } => assert_eq!(error, "boom"),
            other => panic!("expected AgentError, got {other:?}"),
        }

        // Clean up: stop the subscriber task, then tear down the hub (removes
        // the socket file via `EmbeddedHub::drop`).
        task.abort();
        drop(hub);
    }
}
