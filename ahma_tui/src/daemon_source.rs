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
    ClientMsg, DaemonEvent, DaemonMsg, HubRelay, InstanceInfo, connect_to_daemon,
    ensure_daemon_running, recv_msg, send_msg,
};
use tokio::{io::BufReader, sync::mpsc};
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
    /// The start record was reconstructed from the terminal event: the outcome
    /// is true, the tool/command/working directory are unknown. Rendered as
    /// such rather than as blanks (SPEC R24.8).
    pub partial: bool,
    /// The command ran outside the kernel sandbox — today only the TUI's
    /// human-typed `!` escape. Carried so the row can say so.
    pub unsandboxed: bool,
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

/// Apply one daemon message to `state` and forward the resulting UI
/// event(s) on `tx`.
///
/// Used by [`daemon_source_task`] for every message the hub sends.
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
        Applied::Event(event) => {
            if tx.send(event).await.is_err() {
                return None;
            }
            Some(false)
        }
        Applied::None => Some(false),
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
        op.partial = identity.partial;
        op.unsandboxed = identity.unsandboxed;
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
        status: ahma_common::daemon_hub::OpStatus,
        result_summary: Option<String>,
        duration_ms: u64,
        ended_epoch_ms: Option<u64>,
        exit_code: Option<i64>,
        denial: Option<ahma_common::daemon_hub::OpDenial>,
        interrupted: bool,
    ) {
        if let Some(instance_ops) = self.ops.get_mut(instance_id)
            && let Some(op) = instance_ops.get_mut(op_id)
        {
            // A denial and an interruption both arrive as status "Failed" plus
            // a field (the wire evolves by adding fields only, R24.5). The
            // richer local status wins, so the row can say `denied: <path>` or
            // `interrupted` rather than claiming a failure that may not have
            // happened.
            op.status = match (&denial, interrupted) {
                (Some(_), _) => OpStatus::Denied,
                (None, true) => OpStatus::Interrupted,
                (None, false) => parse_op_status(status),
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
        let Some(stream) = connect_or_retry_daemon(&tx, &mut backoff).await else {
            continue;
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
        if !run_daemon_event_loop(&mut reader, &tx, &mut state, &mut prune_counter).await {
            return;
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn connect_or_retry_daemon(
    tx: &mpsc::Sender<SourceEvent>,
    backoff: &mut Duration,
) -> Option<ahma_common::daemon_hub::DaemonStream> {
    if let Err(e) = ensure_daemon_running().await {
        debug!(
            "daemon_source: daemon unavailable ({e}); retry in {:?}",
            *backoff
        );
        let _ = tx
            .send(SourceEvent::DaemonHealthChanged { healthy: false })
            .await;
        tokio::time::sleep(*backoff).await;
        *backoff = (*backoff * 2).min(Duration::from_secs(30));
        return None;
    }

    match connect_to_daemon().await {
        Ok(s) => Some(s),
        Err(e) => {
            debug!(
                "daemon_source: connect failed ({e}); retry in {:?}",
                *backoff
            );
            let _ = tx
                .send(SourceEvent::DaemonHealthChanged { healthy: false })
                .await;
            tokio::time::sleep(*backoff).await;
            *backoff = (*backoff * 2).min(Duration::from_secs(30));
            None
        }
    }
}

async fn run_daemon_event_loop<R>(
    reader: &mut BufReader<R>,
    tx: &mpsc::Sender<SourceEvent>,
    state: &mut DaemonState,
    prune_counter: &mut u8,
) -> bool
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        match recv_msg::<_, DaemonMsg>(reader).await {
            Ok(msg) => match handle_daemon_msg(state, tx, msg).await {
                Some(true) => {
                    *prune_counter += 1;
                    if *prune_counter >= 10 {
                        state.prune_terminal();
                        *prune_counter = 0;
                    }
                }
                Some(false) => {}
                None => return false, // Channel closed — TUI exited.
            },
            Err(e) => {
                debug!("daemon_source: connection lost ({e})");
                let _ = tx
                    .send(SourceEvent::DaemonHealthChanged { healthy: false })
                    .await;
                return true;
            }
        }
    }
}

/// Result of applying one daemon message to the state.
///
/// Only three outcomes are possible, so only three variants exist. This used
/// to be a fifteen-variant enum shadowing [`SourceEvent`] one variant at a
/// time, with a companion function that was a fourteen-arm identity map — a
/// second copy of `SourceEvent`'s shape that had to be updated alongside it,
/// paying for nothing. `apply_msg` builds the `SourceEvent` directly now.
enum Applied {
    /// Nothing the TUI needs to react to.
    None,
    /// The operation list changed — re-emit the merged snapshot. Handled by
    /// the caller rather than carried as an event, because it needs
    /// `DaemonState::all_ops()` (and, in [`daemon_source_task`], a prune
    /// counter) rather than data on the value itself.
    ListChanged,
    /// Forward this event to the TUI as-is.
    Event(SourceEvent),
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
                partial,
                unsandboxed,
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
                        partial,
                        unsandboxed,
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
                interrupted,
            } => {
                state.on_op_finished(
                    &instance_id,
                    &id,
                    status,
                    result_summary,
                    duration_ms,
                    ended_epoch_ms,
                    exit_code,
                    denial,
                    interrupted,
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
            } => Applied::Event(SourceEvent::OperationOutput {
                instance_id: Some(instance_id),
                op_id: id,
                line,
                is_stderr,
            }),
            DaemonEvent::LogLine { .. } => Applied::None, // not yet surfaced in TUI
        },
        DaemonMsg::Ping { .. } => Applied::None, // hub-to-instance ping; no state change for subscribers
        // Instance-directed: the hub routes a TUI's re-raise request to the
        // instance that owns the path. A subscriber seeing it has nothing to do
        // — the re-raised question arrives as a normal ScopeGrantRequested.
        DaemonMsg::ReRaiseScopeGrant { .. } => Applied::None,
        DaemonMsg::RunPrompt { .. }
        | DaemonMsg::CancelPrompt
        | DaemonMsg::CancelOperation { .. } => Applied::None,
        DaemonMsg::SubmitApproval { .. } => Applied::None,
        DaemonMsg::ScopeGrantDismiss { decision_id } => {
            Applied::Event(SourceEvent::ScopeGrantDismiss { decision_id })
        }
        DaemonMsg::WebApprovalDismiss { decision_id } => {
            Applied::Event(SourceEvent::WebApprovalDismiss { decision_id })
        }
        // Everything the hub forwards untouched. Kept as one nested match so
        // the relayed set reads as a set: a message added to `HubRelay` shows
        // up here as a missing arm rather than falling through to a default.
        DaemonMsg::Relay(relay) => Applied::Event(match relay {
            HubRelay::ChatToken { token } => SourceEvent::ChatToken { token },
            HubRelay::ChatThinking { token } => SourceEvent::ChatThinking { token },
            HubRelay::ApprovalRequested { id, tool, args } => {
                SourceEvent::ApprovalRequested { id, tool, args }
            }
            HubRelay::AgentDone => SourceEvent::AgentDone,
            HubRelay::AgentError { error } => SourceEvent::AgentError { error },
            HubRelay::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            } => SourceEvent::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            },
            HubRelay::ToolCallStarted { id, name, args } => {
                SourceEvent::ToolCallStarted { id, name, args }
            }
            HubRelay::ToolCallFinished { id, result, failed } => {
                SourceEvent::ToolCallFinished { id, result, failed }
            }
            HubRelay::ScopeGrantRequested { request } => {
                SourceEvent::ScopeGrantRequested { request }
            }
            HubRelay::WebApprovalRequested { request } => {
                SourceEvent::WebApprovalRequested { request }
            }
            HubRelay::Truncated { reason } => SourceEvent::Truncated { reason },
        }),
        // Instance-bound; a subscriber never receives it.
        DaemonMsg::SubmitScopeGrant { .. } | DaemonMsg::SubmitWebApproval { .. } => Applied::None,
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Map the hub wire status onto the TUI's display status.
///
/// Exhaustive on purpose: the wire enum gaining a state must be a compile error
/// here, not a silent fold into `Failed`. `TimedOut` folding into `Failed` is a
/// deliberate display choice (the TUI has no timeout glyph) — it is now stated
/// as its own arm rather than hidden in a catch-all, and `Pending`/`InProgress`
/// are non-terminal and only reachable if a producer mislabels a finish.
fn parse_op_status(s: ahma_common::daemon_hub::OpStatus) -> OpStatus {
    use ahma_common::daemon_hub::OpStatus as Wire;
    match s {
        Wire::Completed => OpStatus::Succeeded,
        Wire::Failed => OpStatus::Failed,
        Wire::Cancelled => OpStatus::Cancelled,
        Wire::TimedOut => OpStatus::Failed,
        Wire::Pending | Wire::InProgress => OpStatus::Failed,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OpStatus;
    use ahma_common::daemon_hub::{DaemonEvent, DaemonMsg, HubRelay, InstanceInfo};
    use ahma_common::timeouts::TestTimeouts;

    fn inst(id: &str, label: &str) -> InstanceInfo {
        InstanceInfo {
            id: id.to_string(),
            pid: 1,
            mode: "stdio".to_string(),
            scope: "/test".to_string(),
            label: label.to_string(),
            client: None,
            session_id: None,
            client_pid: None,
            ended_epoch_ms: None,
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
            ahma_common::daemon_hub::OpStatus::Completed,
            Some("ok".to_string()),
            100,
            None,
            Some(0),
            None,
            false,
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
            ahma_common::daemon_hub::OpStatus::Completed,
            None,
            0,
            None,
            None,
            None,
            false,
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
            ahma_common::daemon_hub::OpStatus::Failed,
            Some("Operation not permitted".into()),
            5,
            None,
            None,
            Some(OpDenial {
                path: "/etc".into(),
                access: ahma_common::config::ScopeAccess::Rw,
            }),
            false,
        );

        let ops = s.all_ops();
        assert_eq!(ops[0].status, OpStatus::Denied);
        assert_eq!(
            ops[0].denial,
            Some(("/etc".into(), ahma_common::config::ScopeAccess::Rw))
        );
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
            ahma_common::daemon_hub::OpStatus::Failed,
            Some("boom".into()),
            5,
            None,
            None,
            None,
            false,
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
        s.on_op_finished(
            "i1",
            "op-a",
            ahma_common::daemon_hub::OpStatus::Completed,
            None,
            0,
            None,
            None,
            None,
            false,
        );

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
        s.on_op_finished(
            "i1",
            "op-2",
            ahma_common::daemon_hub::OpStatus::Failed,
            None,
            0,
            None,
            None,
            None,
            false,
        );

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
                    partial: false,
                    unsandboxed: false,
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
                    status: ahma_common::daemon_hub::OpStatus::Completed,
                    result_summary: Some("success".to_string()),
                    duration_ms: 1200,
                    ended_epoch_ms: None,
                    exit_code: None,
                    denial: None,
                    interrupted: false,
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
                    partial: false,
                    unsandboxed: false,
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
            Applied::Event(SourceEvent::OperationOutput {
                instance_id,
                op_id,
                line,
                is_stderr,
            }) => {
                assert_eq!(instance_id.as_deref(), Some("i1"));
                assert_eq!(op_id, "op-1");
                assert_eq!(line, "compiling...");
                assert!(!is_stderr);
            }
            _ => panic!("OpOutput must map to SourceEvent::OperationOutput"),
        }
        // Output is NOT accumulated in DaemonState (the TUI app state owns
        // the tail buffer) — the snapshot list stays line-free.
        assert!(s.all_ops()[0].stdout_tail.is_empty());
    }

    // ── parse_op_status ────────────────────────────────────────────────────────

    /// The wire status is a typed enum, so the old "Unknown"/"" cases this test
    /// used to cover are no longer representable — the mapping is total by
    /// construction. What remains worth asserting is the deliberate collapses.
    #[test]
    fn parse_op_status_all_variants() {
        use ahma_common::daemon_hub::OpStatus as Wire;
        assert_eq!(parse_op_status(Wire::Completed), OpStatus::Succeeded);
        assert_eq!(parse_op_status(Wire::Failed), OpStatus::Failed);
        assert_eq!(parse_op_status(Wire::Cancelled), OpStatus::Cancelled);
        assert_eq!(
            parse_op_status(Wire::TimedOut),
            OpStatus::Failed,
            "TimedOut → Failed: the TUI has no timeout glyph"
        );
        assert_eq!(
            parse_op_status(Wire::Pending),
            OpStatus::Failed,
            "a non-terminal state on a finish message is a producer bug"
        );
        assert_eq!(parse_op_status(Wire::InProgress), OpStatus::Failed);
    }

    // ── coverage batch: apply_msg arms, DaemonState branches, embedded hub ─────
    async fn next_ev(rx: &mut mpsc::Receiver<SourceEvent>) -> SourceEvent {
        tokio::time::timeout(TestTimeouts::scale_secs(2), rx.recv())
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
        s.on_op_finished(
            "i1",
            "op-1",
            ahma_common::daemon_hub::OpStatus::Completed,
            None,
            5,
            None,
            None,
            None,
            false,
        );
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
            DaemonMsg::Relay(HubRelay::ChatToken {
                token: "hi".to_string(),
            }),
        ) {
            Applied::Event(SourceEvent::ChatToken { token: t }) => assert_eq!(t, "hi"),
            _ => panic!("ChatToken must map to SourceEvent::ChatToken"),
        }
    }

    #[test]
    fn apply_msg_usage_carries_token_counts() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::Relay(HubRelay::Usage {
                prompt_tokens: 10,
                completion_tokens: 3,
                total_tokens: 13,
            }),
        ) {
            Applied::Event(SourceEvent::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            }) => {
                assert_eq!(
                    (prompt_tokens, completion_tokens, total_tokens),
                    (10, 3, 13)
                );
            }
            _ => panic!("Usage must map to SourceEvent::Usage"),
        }
    }

    #[test]
    fn apply_msg_tool_call_lifecycle_carries_fields() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::Relay(HubRelay::ToolCallStarted {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                args: "{}".to_string(),
            }),
        ) {
            Applied::Event(SourceEvent::ToolCallStarted { id, name, .. }) => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read_file");
            }
            _ => panic!("ToolCallStarted must map to SourceEvent::ToolCallStarted"),
        }
        match apply_msg(
            &mut s,
            DaemonMsg::Relay(HubRelay::ToolCallFinished {
                id: "t1".to_string(),
                result: "ok".to_string(),
                failed: false,
            }),
        ) {
            Applied::Event(SourceEvent::ToolCallFinished { id, failed, .. }) => {
                assert_eq!(id, "t1");
                assert!(!failed);
            }
            _ => panic!("ToolCallFinished must map to SourceEvent::ToolCallFinished"),
        }
    }

    #[test]
    fn apply_msg_approval_requested_carries_fields() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::Relay(HubRelay::ApprovalRequested {
                id: "a1".to_string(),
                tool: "shell".to_string(),
                args: "ls".to_string(),
            }),
        ) {
            Applied::Event(SourceEvent::ApprovalRequested { id, tool, args }) => {
                assert_eq!(id, "a1");
                assert_eq!(tool, "shell");
                assert_eq!(args, "ls");
            }
            _ => panic!("must map to SourceEvent::ApprovalRequested"),
        }
    }

    #[test]
    fn apply_msg_agent_done() {
        let mut s = DaemonState::new();
        assert!(matches!(
            apply_msg(&mut s, DaemonMsg::Relay(HubRelay::AgentDone)),
            Applied::Event(SourceEvent::AgentDone)
        ));
    }

    #[test]
    fn apply_msg_agent_error_carries_message() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::Relay(HubRelay::AgentError {
                error: "boom".to_string(),
            }),
        ) {
            Applied::Event(SourceEvent::AgentError { error: e }) => assert_eq!(e, "boom"),
            _ => panic!("must map to SourceEvent::AgentError"),
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
            DaemonMsg::Relay(HubRelay::ScopeGrantRequested {
                request: sample_scope_grant(),
            }),
        ) {
            Applied::Event(SourceEvent::ScopeGrantRequested { request }) => {
                assert_eq!(request.decision_id, "d1");
                assert_eq!(request.access, ahma_common::config::ScopeAccess::Ro);
            }
            _ => panic!("must map to SourceEvent::ScopeGrantRequested"),
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
            Applied::Event(SourceEvent::ScopeGrantDismiss { decision_id }) => {
                assert_eq!(decision_id, "d9")
            }
            _ => panic!("must map to SourceEvent::ScopeGrantDismiss"),
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

    /// Every hub message maps to the UI event the TUI expects.
    ///
    /// Drives `handle_daemon_msg` — the fan-out both the socket task and the
    /// (now removed) in-process source shared — so the mapping is pinned once,
    /// without a socket.
    #[tokio::test]
    async fn handle_daemon_msg_maps_every_event() {
        let (tx_s, mut rx_s) = mpsc::channel::<SourceEvent>(64);
        let mut st = DaemonState::new();
        let feed = async |st: &mut DaemonState, msg: DaemonMsg| {
            handle_daemon_msg(st, &tx_s, msg)
                .await
                .expect("TUI channel stays open");
        };

        feed(
            &mut st,
            DaemonMsg::InstanceList {
                instances: vec![inst("i1", "IDE")],
            },
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::InstancesUpdated { instances } => assert_eq!(instances.len(), 1),
            other => panic!("expected InstancesUpdated, got {other:?}"),
        }
        match next_ev(&mut rx_s).await {
            SourceEvent::OperationsUpdated { ops } => assert!(ops.is_empty()),
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Event {
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
                    partial: false,
                    unsandboxed: false,
                },
            },
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::OperationsUpdated { ops } => {
                assert_eq!(ops.len(), 1);
                assert_eq!(ops[0].id, "op1");
                assert_eq!(ops[0].status, OpStatus::Running);
            }
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Event {
                instance_id: "i1".to_string(),
                payload: DaemonEvent::OpOutput {
                    id: "op1".to_string(),
                    line: "hello".to_string(),
                    is_stderr: true,
                },
            },
        )
        .await;
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

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::ChatToken {
                token: "tok".to_string(),
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::ChatToken { token } => assert_eq!(token, "tok"),
            other => panic!("expected ChatToken, got {other:?}"),
        }

        feed(&mut st, DaemonMsg::Ping { seq: 3 }).await;
        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::ApprovalRequested {
                id: "a1".to_string(),
                tool: "tool".to_string(),
                args: "args".to_string(),
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::ApprovalRequested { id, tool, args } => {
                assert_eq!(id, "a1");
                assert_eq!(tool, "tool");
                assert_eq!(args, "args");
            }
            other => panic!("Ping must be a no-op; got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::ScopeGrantRequested {
                request: sample_scope_grant(),
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::ScopeGrantRequested { request } => {
                assert_eq!(request.decision_id, "d1");
            }
            other => panic!("expected ScopeGrantRequested, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::ScopeGrantDismiss {
                decision_id: "d1".to_string(),
            },
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::ScopeGrantDismiss { decision_id } => assert_eq!(decision_id, "d1"),
            other => panic!("expected ScopeGrantDismiss, got {other:?}"),
        }

        feed(&mut st, DaemonMsg::Relay(HubRelay::AgentDone)).await;
        match next_ev(&mut rx_s).await {
            SourceEvent::AgentDone => {}
            other => panic!("expected AgentDone, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::AgentError {
                error: "boom".to_string(),
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::AgentError { error } => assert_eq!(error, "boom"),
            other => panic!("expected AgentError, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::InstanceUnregistered {
                id: "i1".to_string(),
            },
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::InstancesUpdated { instances } => assert!(instances.is_empty()),
            other => panic!("expected InstancesUpdated, got {other:?}"),
        }
        match next_ev(&mut rx_s).await {
            SourceEvent::OperationsUpdated { ops } => assert!(ops.is_empty()),
            other => panic!("expected OperationsUpdated, got {other:?}"),
        }
    }

    /// The fan-out reports a closed TUI channel so its caller can stop reading
    /// the socket, rather than looping on a receiver nobody drains.
    #[tokio::test]
    async fn handle_daemon_msg_reports_a_closed_tui_channel() {
        let (tx_s, rx_s) = mpsc::channel::<SourceEvent>(8);
        let mut st = DaemonState::new();
        drop(rx_s);
        assert!(
            handle_daemon_msg(
                &mut st,
                &tx_s,
                DaemonMsg::InstanceList { instances: vec![] }
            )
            .await
            .is_none(),
            "a closed TUI channel must be reported, not ignored"
        );
    }

    // ── apply_msg: remaining variants (ChatThinking, WebApproval*) ─────────────

    #[test]
    fn apply_msg_chat_thinking_carries_token() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::Relay(HubRelay::ChatThinking {
                token: "pondering".to_string(),
            }),
        ) {
            Applied::Event(SourceEvent::ChatThinking { token: t }) => assert_eq!(t, "pondering"),
            _ => panic!("ChatThinking must map to SourceEvent::ChatThinking"),
        }
    }

    #[test]
    fn apply_msg_web_approval_requested_carries_request() {
        let mut s = DaemonState::new();
        match apply_msg(
            &mut s,
            DaemonMsg::Relay(HubRelay::WebApprovalRequested {
                request: sample_web_approval(),
            }),
        ) {
            Applied::Event(SourceEvent::WebApprovalRequested { request }) => {
                assert_eq!(request.decision_id, "w1");
                assert_eq!(request.domain, "api.github.com");
                assert_eq!(request.tool, Some("fetch_webpage".to_string()));
            }
            _ => panic!("must map to SourceEvent::WebApprovalRequested"),
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
            Applied::Event(SourceEvent::WebApprovalDismiss { decision_id }) => {
                assert_eq!(decision_id, "w9")
            }
            _ => panic!("must map to SourceEvent::WebApprovalDismiss"),
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

    // ── fan-out: variants not covered by `handle_daemon_msg_maps_every_event` ──

    #[tokio::test]
    async fn handle_daemon_msg_maps_remaining_events() {
        let (tx_s, mut rx_s) = mpsc::channel::<SourceEvent>(64);
        let mut st = DaemonState::new();
        let feed = async |st: &mut DaemonState, msg: DaemonMsg| {
            handle_daemon_msg(st, &tx_s, msg)
                .await
                .expect("TUI channel stays open");
        };

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::ChatThinking {
                token: "pondering".to_string(),
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::ChatThinking { token } => assert_eq!(token, "pondering"),
            other => panic!("expected ChatThinking, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::WebApprovalRequested {
                request: sample_web_approval(),
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::WebApprovalRequested { request } => {
                assert_eq!(request.decision_id, "w1");
            }
            other => panic!("expected WebApprovalRequested, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::WebApprovalDismiss {
                decision_id: "w1".to_string(),
            },
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::WebApprovalDismiss { decision_id } => assert_eq!(decision_id, "w1"),
            other => panic!("expected WebApprovalDismiss, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::ToolCallStarted {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                args: "{}".to_string(),
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::ToolCallStarted { id, name, .. } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::ToolCallFinished {
                id: "t1".to_string(),
                result: "ok".to_string(),
                failed: true,
            }),
        )
        .await;
        match next_ev(&mut rx_s).await {
            SourceEvent::ToolCallFinished { id, failed, .. } => {
                assert_eq!(id, "t1");
                assert!(failed);
            }
            other => panic!("expected ToolCallFinished, got {other:?}"),
        }

        feed(
            &mut st,
            DaemonMsg::Relay(HubRelay::Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
            }),
        )
        .await;
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
    // variant into a `SourceEvent` send) that the `handle_daemon_msg` tests
    // above cannot reach, because they never touch the socket read loop.
    //
    // A second raw connection plays the role of a registered ahma instance,
    // driving the hub exactly the way a real `ahma` process would.
    #[tokio::test]
    async fn daemon_source_task_full_round_trip_over_socket() {
        let _isolation_guard = isolate_daemon_socket_for_test();

        let hub = ahma_common::daemon_hub::HubServer::bind_at(
            ahma_common::daemon_hub::default_socket_path(),
        )
        .await
        .expect("this test owns a freshly isolated socket, so bind must succeed");
        let hub = tokio::spawn(hub.serve());

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
                session_id: None,
                client_pid: None,
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
                    partial: false,
                    unsandboxed: false,
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
                    status: ahma_common::daemon_hub::OpStatus::Completed,
                    result_summary: Some("ok".to_string()),
                    duration_ms: 42,
                    ended_epoch_ms: None,
                    exit_code: None,
                    denial: None,
                    interrupted: false,
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
            &ClientMsg::Relay(HubRelay::ChatToken {
                token: "hi".to_string(),
            }),
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ChatToken { token } => assert_eq!(token, "hi"),
            other => panic!("LogLine must be a no-op; expected ChatToken, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::Relay(HubRelay::ChatThinking {
                token: "pondering".to_string(),
            }),
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::ChatThinking { token } => assert_eq!(token, "pondering"),
            other => panic!("expected ChatThinking, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::Relay(HubRelay::ApprovalRequested {
                id: "a1".to_string(),
                tool: "shell".to_string(),
                args: "ls".to_string(),
            }),
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
            &ClientMsg::Relay(HubRelay::ScopeGrantRequested {
                request: sample_scope_grant(),
            }),
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
            &ClientMsg::Relay(HubRelay::WebApprovalRequested {
                request: sample_web_approval(),
            }),
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
            &ClientMsg::Relay(HubRelay::ToolCallStarted {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                args: "{}".to_string(),
            }),
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
            &ClientMsg::Relay(HubRelay::ToolCallFinished {
                id: "t1".to_string(),
                result: "ok".to_string(),
                failed: false,
            }),
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
            &ClientMsg::Relay(HubRelay::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
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

        send_msg(&mut inst_w, &ClientMsg::Relay(HubRelay::AgentDone))
            .await
            .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::AgentDone => {}
            other => panic!("expected AgentDone, got {other:?}"),
        }

        send_msg(
            &mut inst_w,
            &ClientMsg::Relay(HubRelay::AgentError {
                error: "boom".to_string(),
            }),
        )
        .await
        .unwrap();
        match next_ev(&mut rx).await {
            SourceEvent::AgentError { error } => assert_eq!(error, "boom"),
            other => panic!("expected AgentError, got {other:?}"),
        }

        // Clean up: stop the subscriber task, then the hub's accept loop. The
        // socket file lives in this test's isolated directory.
        task.abort();
        hub.abort();
    }
}
