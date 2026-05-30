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

use std::{
    collections::HashMap,
    time::Duration,
};

use ahma_common::daemon_hub::{
    ClientMsg, DaemonEvent, DaemonMsg, InstanceInfo, connect_to_daemon, ensure_daemon_running,
    recv_msg, send_msg,
};
use tokio::{io::BufReader, sync::mpsc};
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
        _description: String,
    ) {
        let label = self
            .instances
            .get(instance_id)
            .map(|i| i.label.clone())
            .unwrap_or_else(|| instance_id.to_string());

        let mut op = Operation::new(&op_id, &tool_name, OpStatus::Running);
        op.instance_id = Some(instance_id.to_string());
        op.instance_label = Some(label);

        self.ops
            .entry(instance_id.to_string())
            .or_default()
            .insert(op_id, op);
    }

    fn on_op_finished(&mut self, instance_id: &str, op_id: &str, status_str: &str) {
        if let Some(instance_ops) = self.ops.get_mut(instance_id) {
            if let Some(op) = instance_ops.get_mut(op_id) {
                op.status = parse_op_status(status_str);
            }
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
            instance_ops.retain(|_, op| matches!(op.status, OpStatus::Running | OpStatus::Pending));
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
            debug!("daemon_source: daemon unavailable ({e}); retry in {:?}", backoff);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            continue;
        }

        let stream = match connect_to_daemon().await {
            Ok(s) => s,
            Err(e) => {
                debug!("daemon_source: connect failed ({e}); retry in {:?}", backoff);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };

        backoff = Duration::from_secs(5);
        debug!("daemon_source: connected");

        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;

        // Subscribe to the event stream.
        if let Err(e) = send_msg(&mut writer, &ClientMsg::Subscribe).await {
            warn!("daemon_source: subscribe failed: {e}");
            continue;
        }

        let mut state = DaemonState::new();

        loop {
            match recv_msg::<_, DaemonMsg>(&mut reader).await {
                Ok(msg) => {
                    let changed = apply_msg(&mut state, msg);
                    if changed {
                        prune_counter += 1;
                        if prune_counter >= 10 {
                            state.prune_terminal();
                            prune_counter = 0;
                        }
                        let ops = state.all_ops();
                        if tx.send(SourceEvent::OperationsUpdated { ops }).await.is_err() {
                            // Channel closed — TUI exited.
                            return;
                        }
                    }
                }
                Err(e) => {
                    debug!("daemon_source: connection lost ({e})");
                    break;
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Apply one daemon message to the state.  Returns `true` if the operation
/// list changed and should be re-emitted.
fn apply_msg(state: &mut DaemonState, msg: DaemonMsg) -> bool {
    match msg {
        DaemonMsg::InstanceList { instances } => {
            for info in instances {
                state.add_instance(info);
            }
            // Initial snapshot — emit even if empty so TUI sees "daemon connected".
            true
        }
        DaemonMsg::InstanceRegistered { instance } => {
            state.add_instance(instance);
            true
        }
        DaemonMsg::InstanceUnregistered { id } => {
            state.remove_instance(&id);
            true
        }
        DaemonMsg::Event {
            instance_id,
            payload,
        } => match payload {
            DaemonEvent::OpStarted {
                id,
                tool_name,
                description,
            } => {
                state.on_op_started(&instance_id, id, tool_name, description);
                true
            }
            DaemonEvent::OpFinished { id, status } => {
                state.on_op_finished(&instance_id, &id, &status);
                true
            }
            DaemonEvent::LogLine { .. } => false, // not yet surfaced in TUI
        },
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
