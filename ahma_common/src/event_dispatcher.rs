//! Unified operation event stream and dispatcher (P2 foundation).
//!
//! [`OperationEvent`] is a single, serialisable enum that carries all state
//! transitions for a running tool execution.  [`EventDispatcher`] fans out
//! events via a `tokio::sync::broadcast` channel, so any number of independent
//! subscribers can react to the same event stream:
//!
//! - **`OperationMonitor`** (in `ahma_mcp`) — persists state and signals completion watches
//! - **MCP push** — forwards JSON-RPC notifications to the connected client
//! - **Vault audit** — writes immutable audit records
//! - **Metrics / observability** — increments counters, records histograms
//!
//! ## Ordering invariant (SPEC R15.3)
//!
//! The `OperationMonitor` subscriber MUST write operation history **before**
//! signalling its `completion_watch` channel, so readers always observe the
//! complete history.  The dispatcher itself is non-blocking (broadcast::send
//! is a channel push); ordering guarantees are the responsibility of each
//! subscriber's `handle` implementation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::broadcast;

/// Default broadcast channel capacity for the event dispatcher.
pub const DEFAULT_EVENT_CAPACITY: usize = 256;

// ─── OperationEvent ───────────────────────────────────────────────────────────

/// A single event emitted during a tool operation lifecycle.
///
/// This is a strict superset of the legacy `ProgressUpdate` enum.
/// Subscribers pattern-match on only the variants they care about and
/// ignore the rest via `_ => {}`.
///
/// New variants may be added in minor versions (`#[non_exhaustive]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OperationEvent {
    /// Tool execution has started.
    Started {
        operation_id: String,
        tool_name: String,
        description: String,
        /// Operation (or synthetic group, e.g. `session:<id>`) that spawned this
        /// one. `None` for top-level operations. Lets subscribers (TUI task
        /// tree, audit) reconstruct the caller → subtask hierarchy.
        #[serde(default)]
        parent_id: Option<String>,
        /// Human title, computed here at the source where the command is known
        /// (SPEC R24.7). Subscribers **render** this; they do not derive a name of
        /// their own from the id or the description.
        #[serde(default)]
        title: Option<String>,
        /// Working directory.
        #[serde(default)]
        cwd: Option<String>,
        /// The full command, for detail views.
        #[serde(default)]
        command: Option<String>,
    },
    /// A line of output was produced (stdout or stderr from the child process).
    OutputLine {
        operation_id: String,
        line: String,
        is_stderr: bool,
    },
    /// A structured progress update emitted by the tool.
    Progress {
        operation_id: String,
        message: String,
        percent: Option<f32>,
    },
    /// An alert or warning was detected in the output.
    Alert {
        operation_id: String,
        message: String,
    },
    /// Tool execution completed successfully.
    Completed {
        operation_id: String,
        result: Value,
        duration_ms: u64,
    },
    /// Tool execution failed with an error.
    Failed {
        operation_id: String,
        error: String,
        duration_ms: u64,
    },
    /// Tool execution was cancelled.
    Cancelled {
        operation_id: String,
        reason: String,
        duration_ms: u64,
    },
    /// Tool execution timed out.
    TimedOut {
        operation_id: String,
        duration_ms: u64,
    },
    /// A JSON-RPC notification that should be pushed to the connected MCP client.
    McpNotification {
        operation_id: String,
        method: String,
        params: Option<Value>,
    },
}

impl OperationEvent {
    /// Returns the operation ID for this event.
    pub fn operation_id(&self) -> &str {
        match self {
            OperationEvent::Started { operation_id, .. }
            | OperationEvent::OutputLine { operation_id, .. }
            | OperationEvent::Progress { operation_id, .. }
            | OperationEvent::Alert { operation_id, .. }
            | OperationEvent::Completed { operation_id, .. }
            | OperationEvent::Failed { operation_id, .. }
            | OperationEvent::Cancelled { operation_id, .. }
            | OperationEvent::TimedOut { operation_id, .. }
            | OperationEvent::McpNotification { operation_id, .. } => operation_id,
        }
    }

    /// Returns `true` if this is a terminal event (the operation has ended).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OperationEvent::Completed { .. }
                | OperationEvent::Failed { .. }
                | OperationEvent::Cancelled { .. }
                | OperationEvent::TimedOut { .. }
        )
    }
}

// ─── EventDispatcher ─────────────────────────────────────────────────────────

/// Broadcasts [`OperationEvent`]s to all active subscribers.
///
/// Cheap to clone — all clones share the same underlying channel.
///
/// # Example
///
/// ```rust
/// use ahma_common::event_dispatcher::{EventDispatcher, OperationEvent};
/// use serde_json::json;
///
/// # #[tokio::main]
/// # async fn main() {
/// let dispatcher = EventDispatcher::new(64);
/// let mut sub = dispatcher.subscribe();
///
/// dispatcher.emit(OperationEvent::Started {
///     operation_id: "op-1".into(),
///     tool_name: "cargo_build".into(),
///     description: "Building project".into(),
///     parent_id: None,
///     title: None,
///     cwd: None,
///     command: None,
/// });
///
/// let event = sub.recv().await.unwrap();
/// assert_eq!(event.operation_id(), "op-1");
/// # }
/// ```
#[derive(Clone)]
pub struct EventDispatcher {
    tx: broadcast::Sender<Arc<OperationEvent>>,
}

impl std::fmt::Debug for EventDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventDispatcher")
            .field("receiver_count", &self.tx.receiver_count())
            .finish()
    }
}

impl EventDispatcher {
    /// Create a new dispatcher with `capacity` slots in the broadcast channel.
    ///
    /// Slow subscribers that fall behind by more than `capacity` events will
    /// receive a `RecvError::Lagged` error on their next receive.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Emit an event to all current subscribers.
    ///
    /// Never blocks.  If there are no subscribers the event is silently dropped.
    pub fn emit(&self, event: OperationEvent) {
        let _ = self.tx.send(Arc::new(event));
    }

    /// Subscribe to future events.
    ///
    /// Events emitted before this call are not replayed.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<OperationEvent>> {
        self.tx.subscribe()
    }

    /// Number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventDispatcher {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CAPACITY)
    }
}

// ─── EventSink ───────────────────────────────────────────────────────────────

/// A component that processes [`OperationEvent`]s received from an
/// [`EventDispatcher`] subscription.
///
/// Implement this trait to add independent, cross-cutting features without
/// modifying the `Adapter` or other core components.
///
/// ## Ordering invariant
///
/// If your sink stores terminal state (e.g. `OperationMonitor`), persist that
/// state **before** signalling any `watch` channel or condition variable.  This
/// ensures readers always observe the complete history before the "done" signal.
pub trait EventSink: Send + Sync + 'static {
    /// Process a single event.  Called concurrently with other sinks.
    fn handle(
        &self,
        event: Arc<OperationEvent>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatcher_broadcasts_to_multiple_subscribers() {
        let dispatcher = EventDispatcher::new(16);
        let mut sub1 = dispatcher.subscribe();
        let mut sub2 = dispatcher.subscribe();

        dispatcher.emit(OperationEvent::Started {
            operation_id: "op-1".into(),
            tool_name: "cargo_build".into(),
            description: "Building".into(),
            parent_id: None,
            title: None,
            cwd: None,
            command: None,
        });

        let ev1 = sub1.recv().await.unwrap();
        let ev2 = sub2.recv().await.unwrap();
        assert_eq!(ev1.operation_id(), "op-1");
        assert_eq!(ev2.operation_id(), "op-1");
    }

    #[tokio::test]
    async fn completed_is_terminal() {
        let ev = OperationEvent::Completed {
            operation_id: "op-1".into(),
            result: serde_json::json!({}),
            duration_ms: 100,
        };
        assert!(ev.is_terminal());
    }

    #[tokio::test]
    async fn progress_is_not_terminal() {
        let ev = OperationEvent::Progress {
            operation_id: "op-1".into(),
            message: "50%".into(),
            percent: Some(50.0),
        };
        assert!(!ev.is_terminal());
    }

    #[tokio::test]
    async fn subscriber_count_tracks_subscriptions() {
        let dispatcher = EventDispatcher::new(8);
        assert_eq!(dispatcher.subscriber_count(), 0);
        let _s1 = dispatcher.subscribe();
        assert_eq!(dispatcher.subscriber_count(), 1);
        let _s2 = dispatcher.subscribe();
        assert_eq!(dispatcher.subscriber_count(), 2);
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_does_not_panic() {
        let dispatcher = EventDispatcher::new(8);
        dispatcher.emit(OperationEvent::TimedOut {
            operation_id: "op-2".into(),
            duration_ms: 60_000,
        });
    }

    #[tokio::test]
    async fn operation_event_operation_id_matches_all_variants() {
        let variants: Vec<OperationEvent> = vec![
            OperationEvent::Started {
                operation_id: "id".into(),
                tool_name: "t".into(),
                description: "d".into(),
                parent_id: None,
                title: None,
                cwd: None,
                command: None,
            },
            OperationEvent::OutputLine {
                operation_id: "id".into(),
                line: "line".into(),
                is_stderr: false,
            },
            OperationEvent::Progress {
                operation_id: "id".into(),
                message: "m".into(),
                percent: None,
            },
            OperationEvent::Alert {
                operation_id: "id".into(),
                message: "a".into(),
            },
            OperationEvent::Completed {
                operation_id: "id".into(),
                result: serde_json::json!({}),
                duration_ms: 0,
            },
            OperationEvent::Failed {
                operation_id: "id".into(),
                error: "e".into(),
                duration_ms: 0,
            },
            OperationEvent::Cancelled {
                operation_id: "id".into(),
                reason: "r".into(),
                duration_ms: 0,
            },
            OperationEvent::TimedOut {
                operation_id: "id".into(),
                duration_ms: 0,
            },
            OperationEvent::McpNotification {
                operation_id: "id".into(),
                method: "notifications/foo".into(),
                params: None,
            },
        ];
        for ev in variants {
            assert_eq!(ev.operation_id(), "id", "variant: {ev:?}");
        }
    }
}
