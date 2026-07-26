//! Session management for HTTP bridge session isolation mode.
//!
//! Per R8D, session isolation allows multiple IDE instances to share a single
//! HTTP server with per-session sandbox scopes. Each session spawns a separate
//! `ahma_mcp` subprocess with its own sandbox scope derived from the client's
//! workspace roots.
//!
//! ## Overview
//!
//! In HTTP mode, the server spawns a separate `ahma_mcp` subprocess per MCP session.
//! Each subprocess has its own sandbox scope derived from the client's workspace roots,
//! providing complete isolation between concurrent sessions.
//!
//! ## How It Works
//!
//! ### Protocol Flow
//!
//! 1. **Receive initialize request**: Generate session ID (UUID).
//! 2. **Spawn ahma_mcp subprocess**: Start the MCP engine for this session.
//! 3. **Forward initialize**: Hand over the initialization to the subprocess.
//! 4. **Subprocess requests roots/list**: The engine asks for workspace context.
//! 5. **Bridge intercept**: Capture the roots to define the sandbox boundary.
//!
//! ### Sandbox Scope Binding
//!
//! The sandbox scope is determined lazily via the MCP `roots/list` protocol:
//! 1. Client sends `initialize` with `capabilities.roots: { listChanged: true }`.
//! 2. Server spawns subprocess without sandbox restriction initially.
//! 3. Subprocess sends `roots/list` request to get workspace folders.
//! 4. Bridge intercepts and caches the first root as sandbox scope.
//! 5. Subsequent file operations are validated against this scope.
//!
//! **Security Invariant**: The sandbox scope is set **once** when the first `roots/list`
//! response is received and cannot be changed for that session.
//!
//! ### Handling Roots Changes
//!
//! The committed sandbox scope is immutable for the life of the instance and can
//! never be widened (R5.1 / R5.1.1 / R5.2.2). A client `notifications/roots/list_changed`
//! received after the sandbox is locked is therefore a tolerated no-op: the
//! locked scope is kept and the notification is ignored (not forwarded to the
//! subprocess), and the session stays alive. Sandbox escape is prevented by the
//! immutability of the commit, not by tearing down the session.

use crate::error::{BridgeError, Result};
use crate::peer::{PeerFactory, PeerShutdownFn, PeerStreams, SubprocessPeerFactory};
use ahma_common::sandbox_state::{SandboxState, SandboxStateMachine};
use ahma_common::state_machine::{FsmState, StateMachine};
use chrono::Local;
use dashmap::DashMap;
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Mutex, Notify, broadcast, mpsc, oneshot},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Maximum number of SSE events retained in the per-session replay buffer.
/// Older events are evicted when the buffer exceeds this size.
const EVENT_HISTORY_CAPACITY: usize = 1000;

/// Handshake state machine for MCP session coordination.
///
/// The MCP Streamable HTTP protocol requires a specific sequence:
/// 1. Client sends `initialize` request → server creates session
/// 2. Client opens SSE stream (GET /mcp with session header)
/// 3. Client sends `notifications/initialized` notification
/// 4. Server sends `notifications/roots/list_changed` to subprocess
/// 5. Subprocess sends `roots/list` request back through SSE
/// 6. Client responds with roots → sandbox is locked
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeState {
    /// Initial state: session created, waiting for SSE and MCP initialized
    AwaitingBoth,
    /// SSE connected, waiting for MCP initialized notification
    AwaitingSseOnly,
    /// MCP initialized received, waiting for SSE connection
    AwaitingMcpOnly,
    /// Both SSE and MCP initialized, roots/list_changed sent, awaiting sandbox lock
    RootsRequested,
    /// Sandbox locked, handshake complete
    Complete,
}

impl FsmState for HandshakeState {
    fn name(&self) -> &'static str {
        match self {
            HandshakeState::AwaitingBoth => "AwaitingBoth",
            HandshakeState::AwaitingSseOnly => "AwaitingSseOnly",
            HandshakeState::AwaitingMcpOnly => "AwaitingMcpOnly",
            HandshakeState::RootsRequested => "RootsRequested",
            HandshakeState::Complete => "Complete",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, HandshakeState::Complete)
    }
}

/// Action to perform after a state transition
#[derive(Debug)]
enum HandshakeAction {
    None,
    SendRootsListChanged,
}

/// Represents a workspace root provided by the client's `roots/list` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRoot {
    /// URI of the root (must be file://)
    pub uri: String,
    /// Optional human-readable name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Default handshake timeout in seconds.
pub const DEFAULT_HANDSHAKE_TIMEOUT_SECS: u64 = 45;

/// Get the request timeout in seconds for bridge → subprocess calls.
pub fn request_timeout_secs() -> u64 {
    std::env::var("AHMA_HTTP_BRIDGE_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

/// Get the tools/call request timeout in seconds.
pub fn tool_call_timeout_secs() -> u64 {
    std::env::var("AHMA_HTTP_BRIDGE_TOOL_CALL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

/// Session termination reason
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTerminationReason {
    /// Client requested termination (HTTP DELETE)
    ClientRequested,
    /// Roots change attempted after sandbox lock (security violation)
    RootsChangeRejected,
    /// Subprocess crashed
    ProcessCrashed,
    /// Session timed out
    Timeout,
}

/// Represents an active client session.
pub struct Session {
    /// Unique session identifier
    pub id: String,
    /// Channel to send messages to the subprocess (wrapped in Mutex for restart support)
    sender: Mutex<mpsc::Sender<String>>,
    /// Map of pending request IDs to response channels
    pending_requests: Arc<DashMap<String, oneshot::Sender<Value>>>,
    /// Broadcast channel for SSE events from this session.
    /// Each message is `(event_id, json_string)`.
    broadcast_tx: broadcast::Sender<(u64, String)>,
    /// Whether the session has been terminated
    terminated: AtomicBool,
    /// Termination reason (if terminated)
    termination_reason: Mutex<Option<SessionTerminationReason>>,
    /// Async cleanup hook invoked on explicit session termination.
    ///
    /// For subprocess peers this kills the child process.  For in-memory peers
    /// this is `None` (drop semantics handle cleanup).
    peer_shutdown: Mutex<Option<PeerShutdownFn>>,
    /// Classified cause of an abnormal peer death (SPEC R-SIGN.5), sent by the
    /// peer's exit monitor. `None` for peers without a monitor.
    exit_cause: Mutex<Option<oneshot::Receiver<String>>>,

    /// Handshake state machine (atomic transitions via the shared StateMachine
    /// wrapper). Transition methods return a `HandshakeAction` to run outside the
    /// lock, matching the workspace convention (SPEC R23).
    handshake_state: StateMachine<HandshakeState>,

    /// Notify for waiting on MCP initialization
    mcp_initialized_notify: Notify,

    /// Shared state machine for sandbox lifecycle (R18, R20)
    sandbox_state_machine: Arc<SandboxStateMachine>,

    /// When the session was created (for handshake timeout tracking)
    created_at: Instant,
    /// Per-session handshake timeout duration
    handshake_timeout: Duration,

    /// Cumulative count of SSE events lost due to broadcast channel lag.
    /// Incremented in the SSE stream handler when `BroadcastStreamRecvError::Lagged(n)` is received.
    lagged_events: AtomicU64,

    /// Monotonically increasing per-session SSE event ID counter.
    event_id_counter: AtomicU64,
    /// Bounded ring buffer of recent SSE events for `Last-Event-Id` replay.
    event_history: std::sync::Mutex<VecDeque<(u64, String)>>,

    /// Client info sent in initialize request
    pub client_info: Mutex<Option<Value>>,
    /// Client capabilities sent in initialize request
    pub capabilities: Mutex<Option<Value>>,
    /// Weak reference back to the session manager
    pub session_manager: Mutex<Option<std::sync::Weak<SessionManager>>>,
    /// Map of pending routed request IDs to response channels
    pub routed_requests: Arc<DashMap<String, oneshot::Sender<Value>>>,
    /// Map of pending server-to-client request IDs to their method name (e.g. roots/list)
    pub pending_client_requests: Arc<DashMap<String, String>>,
    /// Semaphore limiting concurrent routed sampling requests to the client (default: 3).
    /// A bounded semaphore replaces the old 1-at-a-time Mutex so that up to N sampling
    /// requests can be in-flight simultaneously, preventing head-of-line blocking when
    /// an IDE session hosts multiple agents.
    pub sampling_semaphore: Arc<tokio::sync::Semaphore>,
}

impl Session {
    /// Set the client info and capabilities for this session.
    pub async fn set_client_info(&self, client_info: Value, capabilities: Value) {
        *self.client_info.lock().await = Some(client_info);
        *self.capabilities.lock().await = Some(capabilities);
    }

    /// Check if the session is terminated
    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }

    /// Set the terminated status of the session. Primarily for testing.
    pub fn set_terminated(&self, terminated: bool) {
        self.terminated.store(terminated, Ordering::SeqCst);
    }

    /// Check if the sandbox is fully configured and active.
    ///
    /// Returns `true` only in `Active` state (subprocess confirmed sandbox configuration).
    /// During `Configuring` state the subprocess may still be processing the roots response,
    /// so tool calls must be held until the subprocess sends `notifications/sandbox/configured`.
    pub fn is_sandbox_locked(&self) -> bool {
        self.sandbox_state_machine.is_active()
    }

    /// Wait until the sandbox reaches `Active` state (subprocess confirmed configuration).
    ///
    /// Returns the configured scopes on success, or an error message if the sandbox
    /// enters a terminal state (Failed/Terminated).
    pub async fn wait_for_sandbox_active(&self) -> std::result::Result<Vec<PathBuf>, String> {
        self.sandbox_state_machine.wait_for_active().await
    }

    /// Get the current sandbox state
    pub fn current_sandbox_state(&self) -> ahma_common::sandbox_state::SandboxState {
        self.sandbox_state_machine.current()
    }

    /// Get the current handshake state
    pub fn handshake_state(&self) -> HandshakeState {
        *self.handshake_state.lock()
    }

    /// Check if the SSE stream is connected (client opened GET /mcp)
    pub fn is_sse_connected(&self) -> bool {
        matches!(
            self.handshake_state(),
            HandshakeState::AwaitingSseOnly
                | HandshakeState::RootsRequested
                | HandshakeState::Complete
        )
    }

    /// Check if MCP initialized notification was received
    pub fn is_mcp_initialized(&self) -> bool {
        matches!(
            self.handshake_state(),
            HandshakeState::AwaitingMcpOnly
                | HandshakeState::RootsRequested
                | HandshakeState::Complete
        )
    }

    /// Wait for MCP initialization.
    pub async fn wait_for_mcp_initialized(&self) {
        if self.is_mcp_initialized() {
            return;
        }
        self.mcp_initialized_notify.notified().await;

        // Double check in case of race/spurious wakeup
        if !self.is_mcp_initialized() {
            // This is rare but possible; the caller might want to loop
            // For now, simpler to just return as the notify implies state change
        }
    }

    /// Check if subprocess has applied sandbox scopes (Active state)
    pub fn is_sandbox_applied(&self) -> bool {
        self.sandbox_state_machine.is_active()
    }

    /// Wait for sandbox application
    pub async fn wait_for_sandbox_applied(&self) {
        if self.is_sandbox_applied() {
            return;
        }
        let _ = self.sandbox_state_machine.wait_for_active().await;
    }

    /// Check if the handshake has timed out
    pub fn is_handshake_timed_out(&self) -> Option<u64> {
        // If the sandbox is Active or Configuring, the handshake is done or in
        // progress — don't sweep the session.
        match self.sandbox_state_machine.current() {
            ahma_common::sandbox_state::SandboxState::Active { .. }
            | ahma_common::sandbox_state::SandboxState::Configuring { .. } => return None,
            _ => {}
        }
        let elapsed = self.created_at.elapsed();
        if elapsed >= self.handshake_timeout {
            Some(elapsed.as_secs())
        } else {
            None
        }
    }

    /// Get the first sandbox scope.
    ///
    /// Sourced from the sandbox state machine, the single source of truth for the
    /// committed scope (SPEC R20/R23). The session keeps no shadow copy.
    pub async fn get_sandbox_scope(&self) -> Option<PathBuf> {
        self.sandbox_state_machine
            .current()
            .scopes()
            .and_then(|s| s.first().cloned())
    }

    /// Get all sandbox scopes (from the sandbox state machine).
    pub async fn get_sandbox_scopes(&self) -> Option<Vec<PathBuf>> {
        self.sandbox_state_machine
            .current()
            .scopes()
            .map(|s| s.to_vec())
    }

    /// Subscribe to SSE events. Each item is `(event_id, json_string)`.
    pub fn subscribe(&self) -> broadcast::Receiver<(u64, String)> {
        self.broadcast_tx.subscribe()
    }

    /// Get the number of active SSE subscribers.
    pub fn sse_receivers(&self) -> usize {
        self.broadcast_tx.receiver_count()
    }

    /// Record `n` events lost due to broadcast receiver lag.
    pub fn record_lagged_events(&self, n: u64) {
        self.lagged_events.fetch_add(n, Ordering::Relaxed);
    }

    /// Total number of SSE events lost due to broadcast receiver lag (cumulative).
    pub fn total_lagged_events(&self) -> u64 {
        self.lagged_events.load(Ordering::Relaxed)
    }

    /// Assign a unique event ID to a message and store it in the replay buffer.
    ///
    /// Returns the assigned event ID.
    pub fn assign_event_id(&self, msg: &str) -> u64 {
        let id = self.event_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let mut history = self.event_history.lock().unwrap();
        history.push_back((id, msg.to_string()));
        if history.len() > EVENT_HISTORY_CAPACITY {
            history.pop_front();
        }
        id
    }

    /// Return events with ID > `last_id` from the replay buffer.
    pub fn replay_events_after(&self, last_id: u64) -> Vec<(u64, String)> {
        let history = self.event_history.lock().unwrap();
        history
            .iter()
            .filter(|(id, _)| *id > last_id)
            .cloned()
            .collect()
    }

    /// Broadcast a message to all SSE subscribers with an assigned event ID.
    /// Primarily for testing.
    pub fn broadcast(
        &self,
        message: String,
    ) -> std::result::Result<usize, broadcast::error::SendError<(u64, String)>> {
        let id = self.assign_event_id(&message);
        self.broadcast_tx.send((id, message))
    }

    /// Serialize and send a JSON message to the subprocess.
    async fn send_to_subprocess(&self, message: &Value, error_context: &str) -> Result<()> {
        let json_str = serde_json::to_string(message)?;
        self.send_serialized_to_subprocess(json_str, error_context)
            .await
    }

    /// Send an already-serialized JSON string to the subprocess.
    async fn send_serialized_to_subprocess(
        &self,
        json_str: String,
        error_context: &str,
    ) -> Result<()> {
        self.sender
            .lock()
            .await
            .send(json_str)
            .await
            .map_err(|e| BridgeError::Communication(format!("{error_context}: {e}")))?;
        Ok(())
    }

    /// Helper to transitions state and return necessary action
    fn transition_sse_connected(&self) -> HandshakeAction {
        self.handshake_state.transition(|state| match *state {
            HandshakeState::AwaitingBoth => {
                *state = HandshakeState::AwaitingSseOnly;
                info!(session_id = %self.id, from = ?HandshakeState::AwaitingBoth, to = ?HandshakeState::AwaitingSseOnly, "SSE connected");
                HandshakeAction::None
            }
            HandshakeState::AwaitingMcpOnly => {
                *state = HandshakeState::RootsRequested;
                info!(session_id = %self.id, from = ?HandshakeState::AwaitingMcpOnly, to = ?HandshakeState::RootsRequested, "SSE connected (completing handshake)");
                HandshakeAction::SendRootsListChanged
            }
            other => {
                debug!(session_id = %self.id, state = ?other, "SSE connected but already handled/advanced");
                HandshakeAction::None
            }
        })
    }

    /// Helper to transition state for MCP initialization
    fn transition_mcp_initialized(&self) -> HandshakeAction {
        let action = self.handshake_state.transition(|state| match *state {
            HandshakeState::AwaitingBoth => {
                *state = HandshakeState::AwaitingMcpOnly;
                info!(session_id = %self.id, from = ?HandshakeState::AwaitingBoth, to = ?HandshakeState::AwaitingMcpOnly, "MCP initialized");
                HandshakeAction::None
            }
            HandshakeState::AwaitingSseOnly => {
                *state = HandshakeState::RootsRequested;
                info!(session_id = %self.id, from = ?HandshakeState::AwaitingSseOnly, to = ?HandshakeState::RootsRequested, "MCP initialized (completing handshake)");
                HandshakeAction::SendRootsListChanged
            }
            other => {
                debug!(session_id = %self.id, state = ?other, "MCP initialized but already handled/advanced");
                HandshakeAction::None
            }
        });
        // Always notify waiters, even when the state was already advanced — the
        // notification must not be lost to a race with the transition.
        self.mcp_initialized_notify.notify_waiters();
        action
    }

    /// Mark SSE as connected and trigger action if needed
    pub async fn mark_sse_connected(&self) -> Result<bool> {
        match self.transition_sse_connected() {
            HandshakeAction::SendRootsListChanged => {
                self.send_roots_list_changed().await?;
                Ok(true)
            }
            HandshakeAction::None => Ok(false),
        }
    }

    /// Mark MCP as initialized and trigger action if needed
    pub async fn mark_mcp_initialized(&self) -> Result<bool> {
        match self.transition_mcp_initialized() {
            HandshakeAction::SendRootsListChanged => {
                self.send_roots_list_changed().await?;
                Ok(true)
            }
            HandshakeAction::None => Ok(false),
        }
    }

    /// Mark handshake as complete (sandbox locked).
    pub fn mark_handshake_complete(&self) {
        self.handshake_state.transition(|state| {
            if *state == HandshakeState::RootsRequested {
                *state = HandshakeState::Complete;
                info!(session_id = %self.id, "Handshake complete");
            }
        });
    }

    /// Send roots/list_changed notification to subprocess.
    pub async fn send_roots_list_changed(&self) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/roots/list_changed"
        });
        self.send_to_subprocess(&notification, "Failed to send roots/list_changed")
            .await?;
        let sse_receivers = self.broadcast_tx.receiver_count();
        info!(session_id = %self.id, sse_receivers = sse_receivers, "Sent roots/list_changed to subprocess; waiting for roots/list response via broadcast");
        Ok(())
    }
}

/// Configuration for the `SessionManager`.
#[derive(Clone)]
pub struct SessionManagerConfig {
    /// The executable command to start the MCP server (e.g., "ahma_mcp").
    ///
    /// Ignored when [`peer_factory`] is `Some`.
    ///
    /// [`peer_factory`]: Self::peer_factory
    pub server_command: String,
    /// Arguments to pass to the server executable.
    ///
    /// Ignored when [`peer_factory`] is `Some`.
    ///
    /// [`peer_factory`]: Self::peer_factory
    pub server_args: Vec<String>,
    /// Explicit fallback directory for clients that do not provide roots.
    ///
    /// If `None`, clients must provide at least one valid `file://` root during
    /// handshake before tool calls are allowed.
    pub default_scope: Option<PathBuf>,
    /// Whether to preserve ANSI colors in the server's output streams.
    ///
    /// Ignored when [`peer_factory`] is `Some`.
    ///
    /// [`peer_factory`]: Self::peer_factory
    pub enable_colored_output: bool,
    /// Timeout in seconds for the MCP handshake to complete.
    /// If the handshake (SSE connection + roots/list response) doesn't complete
    /// within this time, tool calls will return a timeout error.
    /// Defaults to 45 seconds if not specified.
    pub handshake_timeout_secs: u64,
    /// Maximum concurrent sessions allowed.
    pub max_sessions: usize,

    /// Optional override for the peer backend factory.
    ///
    /// When `Some`, this factory is called to produce the [`PeerStreams`] for
    /// each new session, bypassing the default subprocess logic entirely.  Use
    /// `InMemoryPeerFactory` (from `ahma_mcp::test_utils`) here to run bridge
    /// integration tests without spawning real subprocesses.
    ///
    /// When `None` (the default), a [`SubprocessPeerFactory`] is constructed
    /// from `server_command`, `server_args`, and `enable_colored_output`.
    ///
    /// [`PeerStreams`]: crate::peer::PeerStreams
    pub peer_factory: Option<Arc<dyn PeerFactory>>,
}

impl std::fmt::Debug for SessionManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManagerConfig")
            .field("server_command", &self.server_command)
            .field("server_args", &self.server_args)
            .field("default_scope", &self.default_scope)
            .field("enable_colored_output", &self.enable_colored_output)
            .field("handshake_timeout_secs", &self.handshake_timeout_secs)
            .field("max_sessions", &self.max_sessions)
            .field(
                "peer_factory",
                if self.peer_factory.is_some() {
                    &"Some(<PeerFactory>)"
                } else {
                    &"None"
                },
            )
            .finish()
    }
}

/// Manages the lifecycle of concurrent MCP sessions.
///
/// This component is responsible for:
/// - creating new sessions with unique IDs.
/// - spawning isolated subprocesses for each session.
/// - tracking active sessions in a thread-safe map.
/// - handling session termination.
///
/// Each session operates in its own `ahma_mcp` subprocess, ensuring that file access
/// rights and state are strictly isolated between different clients.
///
/// Use `create_session` to start a new handshake, and `lock_sandbox` to finalize security.
pub struct SessionManager {
    /// Active sessions indexed by session ID
    sessions: DashMap<String, Arc<Session>>,
    /// Configuration for spawning new sessions
    config: SessionManagerConfig,
    /// Shared atomic counter tracking active connections.
    pub active_sessions: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// Extract the request ID from a JSON-RPC request value.
/// Returns `None` for absent or null IDs (notifications).
fn extract_request_id(request: &Value) -> Option<String> {
    match request.get("id")? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        id => Some(id.to_string()),
    }
}

/// Register a pending request and return the receiver used to await its response.
fn register_pending_request(
    pending: &DashMap<String, oneshot::Sender<Value>>,
    id: Option<&String>,
) -> Option<oneshot::Receiver<Value>> {
    id.map(|id| {
        let (tx, rx) = oneshot::channel();
        pending.insert(id.clone(), tx);
        rx
    })
}

/// Remove a pending request registration if one exists.
fn clear_pending_request(pending: &DashMap<String, oneshot::Sender<Value>>, id: Option<&str>) {
    if let Some(id) = id {
        pending.remove(id);
    }
}

/// Remove and return the sender for a pending request, if present.
fn take_pending_request(
    pending: &DashMap<String, oneshot::Sender<Value>>,
    id: &str,
) -> Option<oneshot::Sender<Value>> {
    pending.remove(id).map(|(_, sender)| sender)
}

/// Wait for a JSON-RPC response via a oneshot channel, or return immediately for notifications.
/// How long a session's peer cleanup (killing the subprocess) may take before we
/// give up on it and carry on.
///
/// The wait used to be unbounded. A subprocess that would not die therefore kept
/// the bridge alive *forever*: it had already torn its sessions down, so it could
/// no longer answer anything, but it never exited and never closed its clients'
/// connections either. One such bridge sat "Shutting down bridge process..." for
/// five and a half hours while its client waited in silence.
const PEER_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Answer every in-flight request on `pending` with a JSON-RPC error.
///
/// A client that has a request in flight must be *told* the session is gone.
/// Dropping the response channels instead surfaces as "Response channel closed"
/// — or, if the drop never happens, as unbounded silence, which is the worst
/// failure mode there is: indistinguishable from a slow server.
fn fail_pending_requests(
    pending: &DashMap<String, oneshot::Sender<Value>>,
    session_id: &str,
    message: &str,
) {
    let ids: Vec<String> = pending.iter().map(|entry| entry.key().clone()).collect();
    if ids.is_empty() {
        return;
    }
    warn!(
        session_id = %session_id,
        pending_count = ids.len(),
        "Session terminated with requests in flight — answering each with an error"
    );
    for id in ids {
        if let Some(sender) = take_pending_request(pending, &id) {
            let _ = sender.send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": message }
            }));
        }
    }
}

async fn await_response(
    response_rx: Option<oneshot::Receiver<Value>>,
    timeout: Option<Duration>,
    id_opt: &Option<String>,
    pending: &DashMap<String, oneshot::Sender<Value>>,
) -> Result<Value> {
    let Some(rx) = response_rx else {
        return Ok(serde_json::json!({"jsonrpc": "2.0", "result": null}));
    };
    let wait_timeout = timeout.unwrap_or_else(|| Duration::from_secs(request_timeout_secs()));
    match tokio::time::timeout(wait_timeout, rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err(BridgeError::Communication(
            "Response channel closed".to_string(),
        )),
        Err(_) => {
            clear_pending_request(pending, id_opt.as_deref());
            Err(BridgeError::Timeout)
        }
    }
}

/// Extract the write scopes carried in a `notifications/sandbox/configured`
/// payload (`params.scope.write`, an array of display path strings).
///
/// Returns an empty vec when the notification omits the scope summary; the
/// caller then preserves any in-flight `Configuring` scopes instead.
fn parse_configured_scopes(value: &Value) -> Vec<PathBuf> {
    value
        .get("params")
        .and_then(|p| p.get("scope"))
        .and_then(|s| s.get("write"))
        .and_then(Value::as_array)
        .map(|writes| {
            writes
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Handle sandbox lifecycle notifications received from the subprocess.
///
/// Drives the `SandboxStateMachine` forward based on `notifications/sandbox/*` methods.
///
/// The subprocess's `notifications/sandbox/configured` is authoritative: it is
/// only emitted after the subprocess has applied and enforced its scopes, so the
/// bridge advances to `Active` from *any* non-terminal state. Requiring the
/// bridge to reach `Configuring` first was racy — if `configured` arrived before
/// `auto_lock_if_default_scope` transitioned `AwaitingRoots -> Configuring`, the
/// session stuck forever: the TUI showed `[LOCKED]` (it saw the SSE-forwarded
/// notification) while every `tools/call` returned HTTP 409 / JSON-RPC -32001.
fn handle_sandbox_configured(session: &Arc<Session>, value: &Value) {
    let scopes = parse_configured_scopes(value);
    if let Err(e) = session
        .sandbox_state_machine
        .transition_to_active_with_scopes(scopes)
    {
        warn!(
            session_id = %session.id,
            error = %e,
            "Failed to transition sandbox state to Active (received notifications/sandbox/configured)"
        );
    } else {
        info!(session_id = %session.id, "Observed notifications/sandbox/configured from subprocess - Sandbox is now ACTIVE");
    }
}

fn handle_sandbox_failed(session: &Arc<Session>, value: &Value) {
    let err_msg = value
        .get("params")
        .and_then(|p| p.get("error"))
        .and_then(|e| e.as_str())
        .unwrap_or("Unknown error");
    if let Err(e) = session
        .sandbox_state_machine
        .transition_to_failed(err_msg.to_string())
    {
        warn!(
            session_id = %session.id,
            error = %e,
            "Failed to transition sandbox state to Failed (received notifications/sandbox/failed)"
        );
    } else {
        warn!(
            session_id = %session.id,
            error_msg = %err_msg,
            "Subprocess reported sandbox configuration failed"
        );
    }
}

/// Handle sandbox lifecycle notifications received from the subprocess.
///
/// Drives the `SandboxStateMachine` forward based on `notifications/sandbox/*` methods.
fn handle_sandbox_notification(session: &Arc<Session>, value: &Value) {
    match value.get("method").and_then(Value::as_str) {
        Some("notifications/sandbox/configured") => {
            handle_sandbox_configured(session, value);
        }
        Some("notifications/sandbox/failed") => {
            handle_sandbox_failed(session, value);
        }
        Some(method_str) => {
            debug!(session_id = %session.id, method = %method_str, "Subprocess sent a different notification");
        }
        None if value.get("method").is_some() => {
            warn!(session_id = %session.id, "Subprocess sent a method that is not a string");
        }
        None => {}
    }
}

/// Process a single line received from subprocess stdout.
///
/// Routes JSON-RPC responses to waiting callers; broadcasts all other messages
/// (notifications) to SSE subscribers and drives sandbox state transitions.
fn dispatch_subprocess_line(session: &Arc<Session>, line: &str, colored_output: bool) {
    debug!(session_id = %session.id, "Received from subprocess: {}", line);

    if colored_output {
        let timestamp = format!("[{}]", Local::now().format("%H:%M:%S%.3f"));
        let display = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| line.to_string());
        eprintln!(
            "{} {} {}\n{}",
            timestamp,
            format!("[{}]", &session.id[..8]).green(),
            "← STDOUT:".green(),
            display.green()
        );
    }

    let value = match serde_json::from_str::<Value>(line) {
        Ok(v) => v,
        Err(_) => {
            warn!(session_id = %session.id, "Failed to parse JSON from subprocess: {}", line);
            return;
        }
    };

    // Route to waiting caller if this is a response to a pending request
    if let Some(id) = value.get("id") {
        let id_str = id.as_str().map_or_else(|| id.to_string(), str::to_string);
        if let Some(sender) = take_pending_request(&session.pending_requests, &id_str) {
            let _ = sender.send(value);
            return;
        }

        // Store server-to-client request method name to validate response routing
        if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
            session
                .pending_client_requests
                .insert(id_str, method.to_string());
        }
    }

    // Drive sandbox state machine for lifecycle notifications
    handle_sandbox_notification(session, &value);

    // Broadcast to SSE subscribers
    let receiver_count = session.broadcast_tx.receiver_count();
    if receiver_count == 0 {
        warn!(
            session_id = %session.id,
            method = %value.get("method").and_then(|m| m.as_str()).unwrap_or("<none>"),
            "Broadcasting to 0 SSE subscribers - event will be dropped (SSE stream not yet open)"
        );
    } else {
        debug!(session_id = %session.id, receiver_count = receiver_count, "Broadcasting to SSE subscribers");
    }
    let id = session.assign_event_id(line);
    let _ = session.broadcast_tx.send((id, line.to_string()));
}

impl SessionManager {
    /// Creates a new `SessionManager` with the given configuration.
    pub fn new(config: SessionManagerConfig) -> Self {
        Self {
            sessions: DashMap::new(),
            config,
            active_sessions: None,
        }
    }

    /// Spawns a background task that periodically sweeps the sessions map,
    /// terminating any sessions that have timed out during handshake or
    /// whose subprocesses have exited (terminated).
    pub fn start_sweeper(self: &Arc<Self>) {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let Some(manager) = manager.upgrade() else {
                    break;
                };

                let mut to_terminate = Vec::new();
                for entry in manager.sessions.iter() {
                    let session_id = entry.key().clone();
                    let session = entry.value();

                    if session.is_terminated() {
                        to_terminate.push((session_id, SessionTerminationReason::ProcessCrashed));
                    } else if session.is_handshake_timed_out().is_some() {
                        to_terminate.push((session_id, SessionTerminationReason::Timeout));
                    }
                }

                for (session_id, reason) in to_terminate {
                    tracing::info!(
                        session_id = %session_id,
                        reason = ?reason,
                        "Sweeper cleaning up inactive/terminated session"
                    );
                    let _ = manager.terminate_session(&session_id, reason).await;
                }
            }
        });
    }

    /// Returns true when this server requires client roots to complete sandbox lock.
    pub fn requires_client_roots(&self) -> bool {
        self.config.default_scope.is_none()
    }

    /// Auto-lock the sandbox using the configured `default_scope` when no client roots
    /// are expected (or before they arrive). This is a no-op when `default_scope` is
    /// `None` or the sandbox is already locked. The subprocess still controls the
    /// final `Active` transition via `notifications/sandbox/configured`.
    pub async fn auto_lock_if_default_scope(&self, session_id: &str) {
        if self.config.default_scope.is_none() {
            return;
        }
        match self.lock_sandbox(session_id, &[]).await {
            Ok(true) => info!(
                session_id = %session_id,
                "Bridge: auto-locked sandbox from default_scope (no client roots needed)"
            ),
            Ok(false) => {} // already locked
            Err(e) => tracing::warn!(
                session_id = %session_id,
                "Bridge: auto-lock from default_scope failed: {}",
                e
            ),
        }
    }

    /// Prune any terminated or timed-out sessions inline.
    pub async fn prune_stale_sessions(&self) {
        let mut to_terminate = Vec::new();
        for entry in self.sessions.iter() {
            let session_id = entry.key().clone();
            let session = entry.value();

            if session.is_terminated() {
                to_terminate.push((session_id, SessionTerminationReason::ProcessCrashed));
            } else if session.is_handshake_timed_out().is_some() {
                to_terminate.push((session_id, SessionTerminationReason::Timeout));
            }
        }

        for (session_id, reason) in to_terminate {
            tracing::info!(
                session_id = %session_id,
                reason = ?reason,
                "Inline pruning stale session before session creation"
            );
            let _ = self.terminate_session(&session_id, reason).await;
        }
    }

    /// Evict the oldest session that has 0 active SSE receivers for at least 5s to accommodate a new session.
    pub async fn evict_oldest_inactive_session(&self) -> bool {
        let mut oldest: Option<(String, Instant)> = None;
        let min_idle_duration = Duration::from_secs(5);
        for entry in self.sessions.iter() {
            let session_id = entry.key();
            let session = entry.value();
            if session.sse_receivers() == 0 && session.created_at.elapsed() >= min_idle_duration {
                let created = session.created_at;
                match oldest {
                    None => oldest = Some((session_id.clone(), created)),
                    Some((_, ref old_created)) if created < *old_created => {
                        oldest = Some((session_id.clone(), created));
                    }
                    _ => {}
                }
            }
        }

        if let Some((evict_id, _)) = oldest {
            tracing::info!(
                session_id = %evict_id,
                "Evicting oldest inactive session (0 SSE receivers for >5s) to accommodate new session"
            );
            let _ = self
                .terminate_session(&evict_id, SessionTerminationReason::Timeout)
                .await;
            true
        } else {
            false
        }
    }

    /// Initializes a new session and establishes a peer connection for it.
    ///
    /// This initiates the "deferred sandbox" flow:
    /// 1. A new session ID is generated.
    /// 2. A peer connection is created via [`SessionManagerConfig::peer_factory`]
    ///    (or a [`SubprocessPeerFactory`] built from `server_command`/`server_args`).
    /// 3. The peer waits for the bridge to provide the sandbox scope derived from
    ///    client roots.
    ///
    /// # Returns
    ///
    /// * `Ok(String)`: The new session ID (UUID v4). This ID must be included in
    ///   the `Mcp-Session-Id` header for all subsequent requests.
    /// * `Err(BridgeError)`: If the peer connection could not be established.
    pub async fn create_session(&self) -> Result<String> {
        let mut current_count = self.sessions.len();
        if current_count >= self.config.max_sessions {
            self.prune_stale_sessions().await;
            current_count = self.sessions.len();
            if current_count >= self.config.max_sessions
                && !self.evict_oldest_inactive_session().await
            {
                return Err(BridgeError::ServerProcess(format!(
                    "Session limit exceeded (max: {})",
                    self.config.max_sessions
                )));
            }
        }

        let session_id = Uuid::new_v4().to_string();
        info!(session_id = %session_id, "Creating new session");

        // Create peer streams — either via the injected factory or via the
        // default SubprocessPeerFactory built from config fields.
        // PeerFactory::create now returns anyhow::Result (P6); convert to BridgeError.
        let PeerStreams {
            stdin,
            stdout,
            stderr,
            shutdown_fn,
            exit_cause,
        } = match &self.config.peer_factory {
            Some(factory) => factory
                .create()
                .await
                .map_err(|e| BridgeError::ServerProcess(e.to_string()))?,
            None => SubprocessPeerFactory::new(
                self.config.server_command.clone(),
                self.config.server_args.clone(),
                self.config.enable_colored_output,
            )
            .with_default_sandbox_scope(self.config.default_scope.clone())
            .create()
            .await
            .map_err(|e| BridgeError::ServerProcess(e.to_string()))?,
        };

        // Message channel: bridge request handlers → I/O task
        let (tx, rx) = mpsc::channel::<String>(100);
        let (broadcast_tx, _) = broadcast::channel::<(u64, String)>(256);
        let pending_requests = Arc::new(DashMap::new());

        let handshake_timeout = Duration::from_secs(self.config.handshake_timeout_secs);

        let session = Arc::new(Session {
            id: session_id.clone(),
            sender: Mutex::new(tx),
            pending_requests: pending_requests.clone(),
            broadcast_tx: broadcast_tx.clone(),
            terminated: AtomicBool::new(false),
            termination_reason: Mutex::new(None),
            peer_shutdown: Mutex::new(shutdown_fn),
            exit_cause: Mutex::new(exit_cause),
            handshake_state: StateMachine::new(HandshakeState::AwaitingBoth),
            mcp_initialized_notify: Notify::new(),
            sandbox_state_machine: Arc::new(SandboxStateMachine::new()),
            created_at: Instant::now(),
            handshake_timeout,
            lagged_events: AtomicU64::new(0),
            event_id_counter: AtomicU64::new(0),
            event_history: std::sync::Mutex::new(VecDeque::new()),
            client_info: Mutex::new(None),
            capabilities: Mutex::new(None),
            session_manager: Mutex::new(None),
            routed_requests: Arc::new(DashMap::new()),
            pending_client_requests: Arc::new(DashMap::new()),
            sampling_semaphore: Arc::new(tokio::sync::Semaphore::new(3)),
        });

        // Spawn the I/O handler task
        let session_clone = session.clone();
        let colored_output = self.config.enable_colored_output;
        tokio::spawn(async move {
            Self::handle_session_io(session_clone, rx, stdin, stdout, stderr, colored_output).await;
        });

        self.sessions.insert(session_id.clone(), session);

        if let Some(ref count) = self.active_sessions {
            count.fetch_add(1, Ordering::SeqCst);
        }

        Ok(session_id)
    }

    /// Get a session by ID
    pub fn get_session(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.get(session_id).map(|s| s.clone())
    }

    /// Mark a session's sandbox as `Failed`, so `tools/call` returns a definite
    /// 403 with `reason` instead of leaving the client to guess.
    ///
    /// Used when the sandbox provably cannot be established — e.g. the client
    /// returned no roots and no fallback scope is configured. Saying so at once
    /// beats parking the session in `AwaitingRoots` until the handshake times out,
    /// which is both a worse message and a window in which some *other* event could
    /// open the gate.
    pub fn fail_sandbox(&self, session_id: &str, reason: &str) {
        let Some(session) = self.get_session(session_id) else {
            return;
        };
        if let Err(e) = session
            .sandbox_state_machine
            .transition_to_failed(reason.to_string())
        {
            warn!(
                session_id = %session_id,
                error = %e,
                "Could not mark sandbox Failed (already in a terminal state)"
            );
        }
    }

    /// Get all active sessions
    pub fn get_all_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Resolve the sandbox scopes that should be locked for this session.
    fn resolve_sandbox_scopes(&self, session_id: &str, roots: &[McpRoot]) -> Result<Vec<PathBuf>> {
        // Extract all roots as sandbox scopes.
        // Delegate to the shared file_uri parser so security improvements apply everywhere.
        let parsed_scopes: Vec<PathBuf> = roots
            .iter()
            .filter_map(|root| ahma_common::file_uri::parse_file_uri_to_path(&root.uri))
            .collect();

        if !parsed_scopes.is_empty() {
            return Ok(parsed_scopes);
        }

        if roots.is_empty() {
            return match &self.config.default_scope {
                Some(scope) => {
                    info!(
                        session_id = %session_id,
                        fallback_scope = %scope.display(),
                        "Client provided no roots; using explicit fallback sandbox scope"
                    );
                    Ok(vec![scope.clone()])
                }
                None => {
                    warn!(
                        session_id = %session_id,
                        "Rejecting sandbox lock: client provided no roots and no explicit fallback scope is configured"
                    );
                    Err(BridgeError::Communication(
                        "Client did not provide roots/list entries. Configure explicit sandbox scope on server startup (e.g. --sandbox-scope /path/to/project) or use a client that supports roots/list.".to_string()
                    ))
                }
            };
        }

        warn!(
            session_id = %session_id,
            provided_roots = roots.len(),
            "Rejecting sandbox lock: roots/list contained no valid file:// URIs"
        );
        Err(BridgeError::Communication(
            "No valid file:// sandbox roots were provided in roots/list response.".to_string(),
        ))
    }

    /// Send a message to a session's subprocess
    pub async fn send_message(&self, session_id: &str, message: &Value) -> Result<()> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            BridgeError::Communication(format!("Session not found: {}", session_id))
        })?;

        if session.is_terminated() {
            return Err(BridgeError::Communication(
                "Session has been terminated".to_string(),
            ));
        }

        session
            .send_to_subprocess(message, "Failed to send to subprocess")
            .await
    }

    /// Send a request and wait for response
    pub async fn send_request(
        &self,
        session_id: &str,
        request: &Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            BridgeError::Communication(format!("Session not found: {}", session_id))
        })?;

        if session.is_terminated() {
            return Err(BridgeError::Communication(
                "Session has been terminated".to_string(),
            ));
        }

        let id_opt = extract_request_id(request);

        let response_rx = register_pending_request(&session.pending_requests, id_opt.as_ref());

        // Send the request
        let json_str = serde_json::to_string(request)?;
        if let Err(err) = session
            .send_serialized_to_subprocess(json_str, "Failed to send to subprocess")
            .await
        {
            clear_pending_request(&session.pending_requests, id_opt.as_deref());
            return Err(err);
        }

        await_response(response_rx, timeout, &id_opt, &session.pending_requests).await
    }

    /// Lock sandbox scope for a session (called when observing first roots/list response).
    ///
    /// Per R8.4.4-R8.4.5, sandbox scope is determined from the first roots/list response
    /// and cannot be changed. In the simplified design, the subprocess is spawned with
    /// `--defer-sandbox` and configures its own sandbox after roots are received.
    ///
    /// This method only records the scopes for bridge-side enforcement (e.g. rejecting
    /// roots changes after lock) and for debugging.
    ///
    /// Returns `true` if the sandbox was newly locked by this call.
    /// Returns an error if no valid lock source is available.
    ///
    /// Lock source priority:
    /// 1) Client-provided valid file:// roots
    /// 2) Explicit server fallback scope (if configured)
    pub async fn lock_sandbox(&self, session_id: &str, roots: &[McpRoot]) -> Result<bool> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            BridgeError::Communication(format!("Session not found: {}", session_id))
        })?;

        // Check if already locked using state machine
        if !matches!(
            session.sandbox_state_machine.current(),
            SandboxState::AwaitingRoots
        ) {
            return Ok(false);
        }

        let scopes = self.resolve_sandbox_scopes(session_id, roots)?;

        info!(
            session_id = %session_id,
            sandbox_scopes = ?scopes,
            "Locking sandbox scope(s) for session"
        );

        // Transition to Configuring state. The state machine is the single owner
        // of the committed scope (SPEC R20/R23) — no separate cached copy.
        if let Err(e) = session
            .sandbox_state_machine
            .transition_to_configuring(scopes)
        {
            warn!(session_id = %session_id, error = %e, "Failed to transition sandbox state to Configuring");
        }

        // Transition handshake state to Complete
        session.mark_handshake_complete();

        Ok(true)
    }

    /// Handle a client `notifications/roots/list_changed`.
    ///
    /// Returns `Ok(true)` when the notification is a **tolerated no-op** (the
    /// sandbox is already locked) and the caller should acknowledge it without
    /// forwarding it to the subprocess. Returns `Ok(false)` when the sandbox is
    /// still `AwaitingRoots`, so the caller proceeds with the normal handshake
    /// (forwarding the notification).
    ///
    /// ## Why a locked roots change is a no-op (not a session kill)
    ///
    /// The sandbox scope is owned by the per-workspace **instance** and is
    /// committed exactly once at a single commit point, after which it is
    /// immutable and can never be widened by any means (SPEC R5.1 / R5.1.1 /
    /// R5.2.2). A session-level `roots/list_changed` therefore cannot mutate or
    /// widen the committed scope — there is nothing to apply. Real clients
    /// (Cursor, VS Code) routinely re-emit `roots/list_changed` during a session
    /// (reconnects, focus/workspace events); terminating the session over a
    /// benign re-emit was a self-inflicted DoS that produced 403 → stdio-proxy
    /// respawn churn. The security invariant is upheld by the immutability of the
    /// commit, so the safe and robust response is to keep the locked scope and
    /// ignore the notification.
    pub async fn handle_roots_changed(&self, session_id: &str) -> Result<bool> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            BridgeError::Communication(format!("Session not found: {}", session_id))
        })?;

        if matches!(
            session.sandbox_state_machine.current(),
            SandboxState::AwaitingRoots
        ) {
            // Sandbox not yet locked - allow as part of the normal handshake.
            warn!(
                session_id = %session_id,
                "Roots change received before sandbox lock - allowing"
            );
            return Ok(false);
        }

        // Sandbox already locked: the committed instance scope is immutable and
        // cannot be widened, so this is a tolerated no-op. Keep the session and
        // the locked scope; do not forward to the subprocess.
        let scopes = session
            .sandbox_state_machine
            .current()
            .scopes()
            .map(<[_]>::to_vec);
        warn!(
            session_id = %session_id,
            sandbox_scopes = ?scopes,
            "Roots change after sandbox lock ignored - committed scope is immutable (session kept)"
        );
        Ok(true)
    }

    /// Terminate a session
    pub async fn terminate_session(
        &self,
        session_id: &str,
        reason: SessionTerminationReason,
    ) -> Result<()> {
        if let Some((_, session)) = self.sessions.remove(session_id) {
            if let Some(ref count) = self.active_sessions {
                count.fetch_sub(1, Ordering::SeqCst);
            }
            info!(
                session_id = %session_id,
                reason = ?reason,
                "Terminating session"
            );

            session.terminated.store(true, Ordering::SeqCst);
            *session.termination_reason.lock().await = Some(reason);

            // Answer in-flight requests FIRST.
            //
            // This used to run *after* the peer shutdown below — and the peer
            // shutdown could hang, so it was never reached. A client with a
            // request in flight was then never told anything at all: no result,
            // no error, no closed channel. It simply waited until its own idle
            // timeout fired, tens of minutes later, with nothing to show for it.
            //
            // Nothing here can block, so a client always learns its request died.
            fail_pending_requests(
                &session.pending_requests,
                session_id,
                "Session terminated: the ahma bridge is shutting down or restarting",
            );

            // Run peer-specific cleanup (e.g. kill the subprocess) — BOUNDED.
            //
            // A subprocess that refuses to die must not be able to keep the bridge
            // alive: a bridge that has torn down its sessions can no longer serve
            // anyone, so staying up only strands its clients.
            if let Some(shutdown_fn) = session.peer_shutdown.lock().await.take()
                && tokio::time::timeout(PEER_SHUTDOWN_GRACE, shutdown_fn())
                    .await
                    .is_err()
            {
                warn!(
                    session_id = %session_id,
                    grace_secs = PEER_SHUTDOWN_GRACE.as_secs(),
                    "Peer shutdown did not complete within the grace period; \
                     abandoning it so termination can finish"
                );
            }

            // Anything registered after the drain above (a request that raced the
            // teardown) still gets its channel dropped rather than leaked.
            session.pending_requests.clear();

            // Transition state machine to Terminated
            let _ = session.sandbox_state_machine.transition_to_terminated();
        }

        Ok(())
    }

    /// Terminate all active sessions
    pub async fn terminate_all(&self, reason: SessionTerminationReason) {
        let session_ids: Vec<String> = self.sessions.iter().map(|r| r.key().clone()).collect();
        for id in session_ids {
            let _ = self.terminate_session(&id, reason).await;
        }
    }

    /// Check if a session exists and is not terminated
    pub fn session_exists(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|s| !s.is_terminated())
            .unwrap_or(false)
    }

    /// Get session count (for metrics/debugging)
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Handle I/O between the bridge and an MCP peer (subprocess or in-process).
    ///
    /// Works with any `AsyncWrite`/`AsyncRead` pair, enabling both the
    /// production subprocess path and the in-memory test path to share the
    /// same loop.
    async fn handle_session_io(
        session: Arc<Session>,
        mut rx: mpsc::Receiver<String>,
        mut stdin: Box<dyn AsyncWrite + Send + Unpin + 'static>,
        stdout: Box<dyn AsyncRead + Send + Unpin + 'static>,
        stderr: Option<Box<dyn AsyncRead + Send + Unpin + 'static>>,
        colored_output: bool,
    ) {
        // Spawn a dedicated stdout reader to make stdout reading cancel-safe.
        // `Lines::next_line()` is NOT cancel-safe inside `tokio::select!` — when
        // another branch wins (e.g. stderr), the stdout future is cancelled and
        // partially-read data can be lost (causing `roots/list` to be silently
        // dropped). An mpsc channel receive IS cancel-safe, so we forward lines
        // through one here.
        let (stdout_line_tx, mut stdout_line_rx) =
            mpsc::channel::<std::io::Result<Option<String>>>(64);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                let line = reader.next_line().await;
                let done = matches!(&line, Ok(None) | Err(_));
                if stdout_line_tx.send(line).await.is_err() {
                    break;
                }
                if done {
                    break;
                }
            }
        });

        let mut stderr_reader = stderr.map(|s| BufReader::new(s).lines());

        loop {
            tokio::select! {
                // Handle outgoing messages (HTTP -> Stdio)
                Some(msg) = rx.recv() => {
                    debug!(session_id = %session.id, "Sending to subprocess: {}", msg);

                    // Echo STDIN in cyan if colored output is enabled
                    if colored_output {
                        let timestamp = format!("[{}]", Local::now().format("%H:%M:%S%.3f"));
                        let display = serde_json::from_str::<Value>(&msg)
                            .ok()
                            .and_then(|v| serde_json::to_string_pretty(&v).ok())
                            .unwrap_or_else(|| msg.clone());
                        eprintln!("{} {} {}\n{}", timestamp, format!("[{}]", &session.id[..8]).cyan(), "→ STDIN:".cyan(), display.cyan());
                    }

                    if let Err(e) = stdin.write_all(msg.as_bytes()).await {
                        error!(session_id = %session.id, "Failed to write to stdin: {}", e);
                        break;
                    }
                    if let Err(e) = stdin.write_all(b"\n").await {
                        error!(session_id = %session.id, "Failed to write newline to stdin: {}", e);
                        break;
                    }
                    if let Err(e) = stdin.flush().await {
                        error!(session_id = %session.id, "Failed to flush stdin: {}", e);
                        break;
                    }
                }

                // Handle incoming messages (Stdio -> HTTP/SSE) via cancel-safe channel
                stdout_result = stdout_line_rx.recv() => {
                    match stdout_result {
                        Some(Ok(Some(line))) => {
                            if line.is_empty() { continue; }
                            dispatch_subprocess_line(&session, &line, colored_output);
                        }
                        Some(Ok(None)) | None => {
                            warn!(session_id = %session.id, "Subprocess stdout closed - assuming crash or exit");
                            break;
                        }
                        Some(Err(e)) => {
                            error!(session_id = %session.id, "Failed to read stdout: {}", e);
                            break;
                        }
                    }
                }

                // Handle stderr if colored output is enabled
                result = async {
                    if let Some(ref mut reader) = stderr_reader {
                        reader.next_line().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    match result {
                        Ok(Some(line)) if !line.is_empty() => {
                            let timestamp = format!("[{}]", Local::now().format("%H:%M:%S%.3f"));
                            let display = serde_json::from_str::<Value>(&line)
                                .ok()
                                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                                .unwrap_or_else(|| line.clone());
                            eprintln!("{} {} {}\n{}", timestamp, format!("[{}]", &session.id[..8]).red(), "STDERR:".yellow(), display.dimmed());
                        }
                        Ok(Some(_)) => {} // Empty line
                        Ok(None) => {} // stderr closed
                        Err(e) => {
                            error!(session_id = %session.id, "Failed to read stderr: {}", e);
                        }
                    }
                }

                // Check for termination
                _ = async {
                    while !session.terminated.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                } => {
                    info!(session_id = %session.id, "Session terminated, stopping I/O handler");
                    break;
                }
            }
        }

        // Mark session as terminated if not already
        session.terminated.store(true, Ordering::SeqCst);

        // Send explicit error responses to all pending requests before clearing.
        // This prevents "Response channel closed" errors that manifest as cryptic
        // "Canceled: canceled" messages in clients.
        let pending_count = session.pending_requests.len();
        if pending_count > 0 {
            warn!(
                session_id = %session.id,
                pending_count = pending_count,
                "Session terminated with pending requests - sending error responses"
            );
            // R-SIGN.5: if the peer died abnormally, its exit monitor has
            // classified the cause (e.g. the macOS code-signing SIGKILL).
            // Surface that to the client instead of a bare "terminated
            // unexpectedly". Wait briefly — the exit status can land a moment
            // after the pipe EOF that broke the I/O loop.
            let cause = match session.exit_cause.lock().await.take() {
                Some(rx) => tokio::time::timeout(Duration::from_millis(500), rx)
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => None,
            };
            let message = match cause {
                Some(cause) => format!("Session terminated: server subprocess died: {cause}"),
                None => "Session terminated unexpectedly - subprocess may have crashed or handshake failed".to_string(),
            };
            // Drain all pending requests and send error response
            let pending: Vec<_> = session
                .pending_requests
                .iter()
                .map(|entry| entry.key().clone())
                .collect();
            for id in pending {
                if let Some(sender) = take_pending_request(&session.pending_requests, &id) {
                    let error_response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32603,
                            "message": message
                        }
                    });
                    let _ = sender.send(error_response);
                }
            }
        }
    }
}

#[cfg(test)]
mod sandbox_configured_parse_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_configured_scopes_extracts_write_paths() {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/sandbox/configured",
            "params": {
                "scope": {
                    "write": ["/work/project", "/work/extra"],
                    "read": ["/etc"],
                    "tmp": false,
                    "enforced": true,
                    "source": "roots/list"
                }
            }
        });
        let scopes = parse_configured_scopes(&notif);
        assert_eq!(
            scopes,
            vec![PathBuf::from("/work/project"), PathBuf::from("/work/extra")]
        );
    }

    #[test]
    fn parse_configured_scopes_missing_scope_is_empty() {
        // The notification may omit the scope summary entirely. The caller then
        // preserves any in-flight Configuring scopes, so an empty vec is correct.
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/sandbox/configured"
        });
        assert!(parse_configured_scopes(&notif).is_empty());
    }

    #[test]
    fn parse_configured_scopes_empty_write_is_empty() {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/sandbox/configured",
            "params": { "scope": { "write": [] } }
        });
        assert!(parse_configured_scopes(&notif).is_empty());
    }

    #[test]
    fn parse_configured_scopes_non_array_write_is_empty() {
        // `write` present but not an array → treated as missing → empty.
        let notif = json!({
            "method": "notifications/sandbox/configured",
            "params": { "scope": { "write": "not-an-array" } }
        });
        assert!(parse_configured_scopes(&notif).is_empty());
    }
}

#[cfg(test)]
mod session_logic_tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Build a bare `Session` for pure-logic unit tests, bypassing the
    /// subprocess spawn entirely. The returned `mpsc::Receiver` is the
    /// subprocess-bound channel; keep it alive so `send_*` calls succeed and so
    /// the test can observe messages the session emits (e.g. roots/list_changed).
    fn make_test_session_with_timeout(
        handshake_timeout: Duration,
    ) -> (Arc<Session>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<String>(100);
        let (broadcast_tx, _) = broadcast::channel::<(u64, String)>(256);
        let session = Arc::new(Session {
            id: Uuid::new_v4().to_string(),
            sender: Mutex::new(tx),
            pending_requests: Arc::new(DashMap::new()),
            broadcast_tx,
            terminated: AtomicBool::new(false),
            termination_reason: Mutex::new(None),
            peer_shutdown: Mutex::new(None),
            exit_cause: Mutex::new(None),
            handshake_state: StateMachine::new(HandshakeState::AwaitingBoth),
            mcp_initialized_notify: Notify::new(),
            sandbox_state_machine: Arc::new(SandboxStateMachine::new()),
            created_at: Instant::now(),
            handshake_timeout,
            lagged_events: AtomicU64::new(0),
            event_id_counter: AtomicU64::new(0),
            event_history: std::sync::Mutex::new(VecDeque::new()),
            client_info: Mutex::new(None),
            capabilities: Mutex::new(None),
            session_manager: Mutex::new(None),
            routed_requests: Arc::new(DashMap::new()),
            pending_client_requests: Arc::new(DashMap::new()),
            sampling_semaphore: Arc::new(tokio::sync::Semaphore::new(3)),
        });
        (session, rx)
    }

    fn make_test_session() -> (Arc<Session>, mpsc::Receiver<String>) {
        make_test_session_with_timeout(Duration::from_secs(45))
    }

    // ── Handshake state machine: SSE connection ──────────────────────────────

    #[tokio::test]
    async fn mark_sse_connected_from_awaiting_both_is_partial() {
        // AwaitingBoth -> AwaitingSseOnly (no action). transition_sse_connected
        // arm `AwaitingBoth` (lines 430-434) / mark_sse_connected None arm (478).
        let (session, _rx) = make_test_session();
        let triggered = session.mark_sse_connected().await.unwrap();
        assert!(
            !triggered,
            "first SSE alone must not complete the handshake"
        );
        assert_eq!(session.handshake_state(), HandshakeState::AwaitingSseOnly);
        assert!(session.is_sse_connected());
        assert!(!session.is_mcp_initialized());
    }

    #[tokio::test]
    async fn mark_sse_connected_from_awaiting_mcp_only_completes() {
        // AwaitingMcpOnly -> RootsRequested with SendRootsListChanged
        // (transition_sse_connected lines 435-439, mark_sse_connected lines 474-477).
        let (session, mut rx) = make_test_session();
        // Drive to AwaitingMcpOnly first.
        assert!(!session.mark_mcp_initialized().await.unwrap());
        assert_eq!(session.handshake_state(), HandshakeState::AwaitingMcpOnly);

        let triggered = session.mark_sse_connected().await.unwrap();
        assert!(triggered, "SSE after MCP must complete the handshake");
        assert_eq!(session.handshake_state(), HandshakeState::RootsRequested);
        // The roots/list_changed notification was emitted to the subprocess.
        let sent = rx.try_recv().expect("roots/list_changed must be sent");
        let v: Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["method"], "notifications/roots/list_changed");
    }

    #[tokio::test]
    async fn mark_sse_connected_when_already_advanced_is_noop() {
        // `other` arm of transition_sse_connected (lines 440-443): a redundant SSE
        // connect after the handshake already advanced is ignored.
        let (session, _rx) = make_test_session();
        session.mark_mcp_initialized().await.unwrap();
        session.mark_sse_connected().await.unwrap(); // -> RootsRequested
        let again = session.mark_sse_connected().await.unwrap();
        assert!(!again, "redundant SSE connect must be a no-op");
        assert_eq!(session.handshake_state(), HandshakeState::RootsRequested);
    }

    // ── Handshake state machine: MCP initialized ─────────────────────────────

    #[tokio::test]
    async fn mark_mcp_initialized_from_awaiting_both_is_partial() {
        // AwaitingBoth -> AwaitingMcpOnly (transition_mcp_initialized lines 450-454).
        let (session, _rx) = make_test_session();
        let triggered = session.mark_mcp_initialized().await.unwrap();
        assert!(!triggered);
        assert_eq!(session.handshake_state(), HandshakeState::AwaitingMcpOnly);
        assert!(session.is_mcp_initialized());
        assert!(!session.is_sse_connected());
    }

    #[tokio::test]
    async fn mark_mcp_initialized_from_awaiting_sse_only_completes() {
        // AwaitingSseOnly -> RootsRequested with SendRootsListChanged
        // (transition_mcp_initialized lines 455-459, mark_mcp_initialized 485-488).
        let (session, mut rx) = make_test_session();
        assert!(!session.mark_sse_connected().await.unwrap()); // -> AwaitingSseOnly
        let triggered = session.mark_mcp_initialized().await.unwrap();
        assert!(triggered);
        assert_eq!(session.handshake_state(), HandshakeState::RootsRequested);
        let sent = rx.try_recv().expect("roots/list_changed must be sent");
        assert!(sent.contains("notifications/roots/list_changed"));
    }

    #[tokio::test]
    async fn mark_mcp_initialized_when_already_advanced_is_noop() {
        // `other` arm of transition_mcp_initialized (lines 460-463).
        let (session, _rx) = make_test_session();
        session.mark_sse_connected().await.unwrap();
        session.mark_mcp_initialized().await.unwrap(); // -> RootsRequested
        let again = session.mark_mcp_initialized().await.unwrap();
        assert!(!again);
        assert_eq!(session.handshake_state(), HandshakeState::RootsRequested);
    }

    // ── mark_handshake_complete ──────────────────────────────────────────────

    #[tokio::test]
    async fn mark_handshake_complete_from_roots_requested() {
        // RootsRequested -> Complete (lines 495-500).
        let (session, _rx) = make_test_session();
        session.mark_mcp_initialized().await.unwrap();
        session.mark_sse_connected().await.unwrap(); // -> RootsRequested
        session.mark_handshake_complete();
        assert_eq!(session.handshake_state(), HandshakeState::Complete);
        assert!(session.is_sse_connected());
        assert!(session.is_mcp_initialized());
    }

    #[test]
    fn mark_handshake_complete_noop_when_not_roots_requested() {
        // The `if` guard in mark_handshake_complete is false → stays AwaitingBoth.
        let (session, _rx) = make_test_session();
        session.mark_handshake_complete();
        assert_eq!(session.handshake_state(), HandshakeState::AwaitingBoth);
    }

    // ── wait_for_mcp_initialized ─────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_mcp_initialized_returns_immediately_when_set() {
        let (session, _rx) = make_test_session();
        session.mark_mcp_initialized().await.unwrap();
        // Already initialized → the early return path (lines 291-293).
        tokio::time::timeout(Duration::from_secs(1), session.wait_for_mcp_initialized())
            .await
            .expect("must return without waiting");
    }

    #[tokio::test]
    async fn wait_for_mcp_initialized_wakes_on_notification() {
        let (session, _rx) = make_test_session();
        let s2 = session.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            s2.mark_mcp_initialized().await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), session.wait_for_mcp_initialized())
            .await
            .expect("notify must wake the waiter");
        assert!(session.is_mcp_initialized());
    }

    // ── is_handshake_timed_out ───────────────────────────────────────────────

    #[test]
    fn handshake_times_out_when_elapsed_exceeds_timeout() {
        // Zero timeout → elapsed >= timeout → Some(secs) (lines 325-330).
        let (session, _rx) = make_test_session_with_timeout(Duration::ZERO);
        assert!(session.is_handshake_timed_out().is_some());
    }

    #[test]
    fn handshake_not_timed_out_within_window() {
        let (session, _rx) = make_test_session_with_timeout(Duration::from_secs(3600));
        assert!(session.is_handshake_timed_out().is_none());
    }

    #[test]
    fn handshake_timeout_suppressed_while_configuring() {
        // Configuring early-return arm (lines 321-323).
        let (session, _rx) = make_test_session_with_timeout(Duration::ZERO);
        session
            .sandbox_state_machine
            .transition_to_configuring(vec![std::env::temp_dir().join("p")])
            .unwrap();
        assert!(session.is_handshake_timed_out().is_none());
    }

    #[test]
    fn handshake_timeout_suppressed_when_active() {
        // Active early-return arm (lines 321-323).
        let (session, _rx) = make_test_session_with_timeout(Duration::ZERO);
        session
            .sandbox_state_machine
            .transition_to_active_with_scopes(vec![std::env::temp_dir().join("p")])
            .unwrap();
        assert!(session.is_handshake_timed_out().is_none());
    }

    // ── is_sse_connected / is_mcp_initialized state matrix ───────────────────

    #[tokio::test]
    async fn connection_predicates_track_state_transitions() {
        let (session, _rx) = make_test_session();
        // AwaitingBoth: neither.
        assert!(!session.is_sse_connected());
        assert!(!session.is_mcp_initialized());
        // Complete: both.
        session.mark_sse_connected().await.unwrap();
        session.mark_mcp_initialized().await.unwrap();
        session.mark_handshake_complete();
        assert!(session.is_sse_connected());
        assert!(session.is_mcp_initialized());
    }

    // ── Event replay buffer: assign_event_id / replay_events_after ───────────

    #[test]
    fn assign_event_id_is_monotonic_starting_at_one() {
        let (session, _rx) = make_test_session();
        assert_eq!(session.assign_event_id("a"), 1);
        assert_eq!(session.assign_event_id("b"), 2);
        assert_eq!(session.assign_event_id("c"), 3);
    }

    #[test]
    fn replay_events_after_filters_by_id() {
        let (session, _rx) = make_test_session();
        session.assign_event_id("one");
        session.assign_event_id("two");
        session.assign_event_id("three");
        let after_one = session.replay_events_after(1);
        assert_eq!(
            after_one,
            vec![(2, "two".to_string()), (3, "three".to_string())]
        );
        // Nothing newer than the latest id.
        assert!(session.replay_events_after(3).is_empty());
        // Everything when last_id is 0.
        assert_eq!(session.replay_events_after(0).len(), 3);
    }

    #[test]
    fn replay_buffer_prunes_at_capacity() {
        // Push one past capacity; the oldest entry is evicted (lines 379-381).
        let (session, _rx) = make_test_session();
        for i in 0..(EVENT_HISTORY_CAPACITY + 1) {
            session.assign_event_id(&format!("e{i}"));
        }
        let all = session.replay_events_after(0);
        assert_eq!(all.len(), EVENT_HISTORY_CAPACITY);
        // id 1 was evicted; the lowest retained id is 2.
        assert_eq!(all.first().unwrap().0, 2);
        assert_eq!(all.last().unwrap().0, (EVENT_HISTORY_CAPACITY + 1) as u64);
    }

    // ── broadcast / subscribe / lag counters ─────────────────────────────────

    #[test]
    fn broadcast_assigns_id_and_delivers_to_subscriber() {
        let (session, _rx) = make_test_session();
        let mut sub = session.subscribe();
        assert_eq!(session.sse_receivers(), 1);
        let n = session.broadcast("hello".to_string()).unwrap();
        assert_eq!(n, 1, "one receiver should receive the broadcast");
        let (id, msg) = sub.try_recv().unwrap();
        assert_eq!(id, 1);
        assert_eq!(msg, "hello");
    }

    #[test]
    fn lagged_event_counter_accumulates() {
        let (session, _rx) = make_test_session();
        assert_eq!(session.total_lagged_events(), 0);
        session.record_lagged_events(3);
        session.record_lagged_events(4);
        assert_eq!(session.total_lagged_events(), 7);
    }

    // ── terminated flag / sandbox predicates ─────────────────────────────────

    #[test]
    fn terminated_flag_round_trips() {
        let (session, _rx) = make_test_session();
        assert!(!session.is_terminated());
        session.set_terminated(true);
        assert!(session.is_terminated());
        session.set_terminated(false);
        assert!(!session.is_terminated());
    }

    #[test]
    fn sandbox_predicates_reflect_state_machine() {
        let (session, _rx) = make_test_session();
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));
        assert!(!session.is_sandbox_locked());
        assert!(!session.is_sandbox_applied());

        let scope = std::env::temp_dir().join("locked_proj");
        session
            .sandbox_state_machine
            .transition_to_active_with_scopes(vec![scope.clone()])
            .unwrap();
        assert!(session.is_sandbox_locked());
        assert!(session.is_sandbox_applied());
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Active { .. }
        ));
    }

    #[tokio::test]
    async fn get_sandbox_scope_and_scopes() {
        let (session, _rx) = make_test_session();
        assert!(session.get_sandbox_scope().await.is_none());
        assert!(session.get_sandbox_scopes().await.is_none());

        let a = std::env::temp_dir().join("a");
        let b = std::env::temp_dir().join("b");
        session
            .sandbox_state_machine
            .transition_to_configuring(vec![a.clone(), b.clone()])
            .unwrap();
        assert_eq!(session.get_sandbox_scope().await, Some(a.clone()));
        assert_eq!(session.get_sandbox_scopes().await, Some(vec![a, b]));
    }

    #[tokio::test]
    async fn wait_for_sandbox_active_returns_when_active() {
        let (session, _rx) = make_test_session();
        let scope = std::env::temp_dir().join("active_scope");
        session
            .sandbox_state_machine
            .transition_to_active_with_scopes(vec![scope.clone()])
            .unwrap();
        let scopes = session.wait_for_sandbox_active().await.unwrap();
        assert_eq!(scopes, vec![scope]);
        // wait_for_sandbox_applied also short-circuits when already active.
        tokio::time::timeout(Duration::from_secs(1), session.wait_for_sandbox_applied())
            .await
            .expect("should not block when active");
    }

    #[tokio::test]
    async fn set_client_info_stores_values() {
        let (session, _rx) = make_test_session();
        let info = json!({"name": "test-client"});
        let caps = json!({"roots": {"listChanged": true}});
        session.set_client_info(info.clone(), caps.clone()).await;
        assert_eq!(*session.client_info.lock().await, Some(info));
        assert_eq!(*session.capabilities.lock().await, Some(caps));
    }

    // ── sandbox notification handlers ────────────────────────────────────────

    #[test]
    fn handle_sandbox_configured_drives_to_active() {
        let (session, _rx) = make_test_session();
        let notif = json!({
            "method": "notifications/sandbox/configured",
            "params": { "scope": { "write": ["/work/p"] } }
        });
        handle_sandbox_configured(&session, &notif);
        assert!(session.is_sandbox_locked());
        assert_eq!(
            session.current_sandbox_state().scopes().map(<[_]>::to_vec),
            Some(vec![PathBuf::from("/work/p")])
        );
    }

    #[test]
    fn handle_sandbox_configured_on_terminal_state_warns_no_panic() {
        // Failed → configured cannot resurrect; transition errors, handler warns.
        let (session, _rx) = make_test_session();
        session
            .sandbox_state_machine
            .transition_to_failed("boom".to_string())
            .unwrap();
        let notif = json!({"method": "notifications/sandbox/configured"});
        handle_sandbox_configured(&session, &notif);
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Failed { .. }
        ));
    }

    #[test]
    fn handle_sandbox_failed_drives_to_failed_with_message() {
        let (session, _rx) = make_test_session();
        let notif = json!({
            "method": "notifications/sandbox/failed",
            "params": { "error": "scope denied" }
        });
        handle_sandbox_failed(&session, &notif);
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Failed { error } if error == "scope denied"
        ));
    }

    #[test]
    fn handle_sandbox_failed_defaults_error_message() {
        // No params.error → "Unknown error" default branch.
        let (session, _rx) = make_test_session();
        handle_sandbox_failed(&session, &json!({"method": "notifications/sandbox/failed"}));
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Failed { error } if error == "Unknown error"
        ));
    }

    #[test]
    fn handle_sandbox_failed_on_terminal_state_warns_no_panic() {
        let (session, _rx) = make_test_session();
        session
            .sandbox_state_machine
            .transition_to_terminated()
            .unwrap();
        handle_sandbox_failed(&session, &json!({"method": "notifications/sandbox/failed"}));
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Terminated
        ));
    }

    #[test]
    fn handle_sandbox_notification_routes_by_method() {
        // configured → Active
        let (s1, _r1) = make_test_session();
        handle_sandbox_notification(
            &s1,
            &json!({"method": "notifications/sandbox/configured",
                    "params": {"scope": {"write": ["/x"]}}}),
        );
        assert!(s1.is_sandbox_locked());

        // failed → Failed
        let (s2, _r2) = make_test_session();
        handle_sandbox_notification(
            &s2,
            &json!({"method": "notifications/sandbox/failed", "params": {"error": "e"}}),
        );
        assert!(matches!(
            s2.current_sandbox_state(),
            SandboxState::Failed { .. }
        ));

        // unrelated string method → ignored (still AwaitingRoots)
        let (s3, _r3) = make_test_session();
        handle_sandbox_notification(&s3, &json!({"method": "notifications/progress"}));
        assert!(matches!(
            s3.current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));

        // non-string method → warn branch, ignored
        let (s4, _r4) = make_test_session();
        handle_sandbox_notification(&s4, &json!({"method": 42}));
        assert!(matches!(
            s4.current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));

        // no method key → no-op
        let (s5, _r5) = make_test_session();
        handle_sandbox_notification(&s5, &json!({"id": 1, "result": null}));
        assert!(matches!(
            s5.current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));
    }

    // ── dispatch_subprocess_line ─────────────────────────────────────────────

    #[test]
    fn dispatch_routes_response_to_pending_request() {
        let (session, _rx) = make_test_session();
        let (tx, mut resp_rx) = oneshot::channel();
        session.pending_requests.insert("req-1".to_string(), tx);

        dispatch_subprocess_line(
            &session,
            &json!({"jsonrpc": "2.0", "id": "req-1", "result": {"ok": true}}).to_string(),
            false,
        );
        let received = resp_rx.try_recv().expect("response routed to caller");
        assert_eq!(received["result"]["ok"], true);
        // Pending entry consumed.
        assert!(!session.pending_requests.contains_key("req-1"));
    }

    #[test]
    fn dispatch_stores_server_to_client_request_method() {
        let (session, _rx) = make_test_session();
        // An id present but no matching pending request, and a method → recorded
        // in pending_client_requests (lines 798-803).
        dispatch_subprocess_line(
            &session,
            &json!({"jsonrpc": "2.0", "id": "srv-7", "method": "roots/list"}).to_string(),
            false,
        );
        assert_eq!(
            session
                .pending_client_requests
                .get("srv-7")
                .map(|m| m.value().clone()),
            Some("roots/list".to_string())
        );
    }

    #[test]
    fn dispatch_broadcasts_notification_and_assigns_event_id() {
        let (session, _rx) = make_test_session();
        let mut sub = session.subscribe();
        dispatch_subprocess_line(
            &session,
            &json!({"jsonrpc": "2.0", "method": "notifications/progress"}).to_string(),
            false,
        );
        let (id, msg) = sub.try_recv().expect("notification broadcast");
        assert_eq!(id, 1);
        assert!(msg.contains("notifications/progress"));
    }

    #[test]
    fn dispatch_drives_sandbox_state_from_notification() {
        let (session, _rx) = make_test_session();
        dispatch_subprocess_line(
            &session,
            &json!({"method": "notifications/sandbox/configured",
                    "params": {"scope": {"write": ["/w"]}}})
            .to_string(),
            false,
        );
        assert!(session.is_sandbox_locked());
    }

    #[test]
    fn dispatch_ignores_invalid_json() {
        // Unparseable line → warn + early return, no state change, no panic.
        let (session, _rx) = make_test_session();
        dispatch_subprocess_line(&session, "this is not json", false);
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));
    }

    // ── free helpers: request id / pending maps ──────────────────────────────

    #[test]
    fn extract_request_id_variants() {
        assert_eq!(
            extract_request_id(&json!({"id": "abc"})),
            Some("abc".to_string())
        );
        assert_eq!(
            extract_request_id(&json!({"id": 42})),
            Some("42".to_string())
        );
        assert_eq!(extract_request_id(&json!({"id": null})), None);
        assert_eq!(extract_request_id(&json!({"method": "x"})), None);
    }

    #[test]
    fn register_and_take_pending_request() {
        let pending: DashMap<String, oneshot::Sender<Value>> = DashMap::new();
        let id = Some("r1".to_string());
        let rx = register_pending_request(&pending, id.as_ref());
        assert!(rx.is_some());
        assert!(pending.contains_key("r1"));

        let sender = take_pending_request(&pending, "r1");
        assert!(sender.is_some());
        assert!(!pending.contains_key("r1"));
        // Taking again returns None.
        assert!(take_pending_request(&pending, "r1").is_none());
    }

    #[test]
    fn register_pending_request_none_id_registers_nothing() {
        let pending: DashMap<String, oneshot::Sender<Value>> = DashMap::new();
        assert!(register_pending_request(&pending, None).is_none());
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn clear_pending_request_removes_and_tolerates_none() {
        let pending: DashMap<String, oneshot::Sender<Value>> = DashMap::new();
        let (tx, _rx) = oneshot::channel();
        pending.insert("c1".to_string(), tx);
        clear_pending_request(&pending, Some("c1"));
        assert!(!pending.contains_key("c1"));
        // None id is a no-op (does not panic).
        clear_pending_request(&pending, None);
    }

    // ── await_response ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn await_response_none_rx_returns_null_result() {
        let pending: DashMap<String, oneshot::Sender<Value>> = DashMap::new();
        let res = await_response(None, None, &None, &pending).await.unwrap();
        assert_eq!(res["result"], Value::Null);
    }

    #[tokio::test]
    async fn await_response_returns_delivered_value() {
        let pending: DashMap<String, oneshot::Sender<Value>> = DashMap::new();
        let (tx, rx) = oneshot::channel();
        tx.send(json!({"jsonrpc": "2.0", "id": "1", "result": "done"}))
            .unwrap();
        let res = await_response(
            Some(rx),
            Some(Duration::from_secs(1)),
            &Some("1".to_string()),
            &pending,
        )
        .await
        .unwrap();
        assert_eq!(res["result"], "done");
    }

    #[tokio::test]
    async fn await_response_channel_closed_errors() {
        let pending: DashMap<String, oneshot::Sender<Value>> = DashMap::new();
        let (tx, rx) = oneshot::channel::<Value>();
        drop(tx); // sender dropped → recv yields Err
        let err = await_response(Some(rx), Some(Duration::from_secs(1)), &None, &pending)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Response channel closed"));
    }

    #[tokio::test]
    async fn await_response_timeout_clears_pending() {
        let pending: DashMap<String, oneshot::Sender<Value>> = DashMap::new();
        let (tx, rx) = oneshot::channel::<Value>();
        // Keep tx alive so the channel never delivers and never closes.
        let id = "timeout-id".to_string();
        pending.insert(id.clone(), oneshot::channel().0);
        let err = await_response(
            Some(rx),
            Some(Duration::from_millis(20)),
            &Some(id.clone()),
            &pending,
        )
        .await
        .unwrap_err();
        // Must be the dedicated, recoverable Timeout variant — not a generic
        // Communication error — so `forward_request` can keep the session alive.
        assert!(
            matches!(err, BridgeError::Timeout),
            "expected BridgeError::Timeout, got {err:?}"
        );
        assert!(err.to_string().contains("timed out"));
        // The pending entry was cleared on timeout.
        assert!(!pending.contains_key(&id));
        drop(tx);
    }

    // ── timeout env helpers ──────────────────────────────────────────────────

    static ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    #[test]
    fn request_timeout_secs_defaults_and_overrides() {
        let _g = ENV_LOCK.lock().unwrap();
        let key = "AHMA_HTTP_BRIDGE_REQUEST_TIMEOUT_SECS";
        let prev = std::env::var(key).ok();

        unsafe { std::env::remove_var(key) };
        assert_eq!(request_timeout_secs(), 60);

        unsafe { std::env::set_var(key, "123") };
        assert_eq!(request_timeout_secs(), 123);

        // Invalid value falls back to default.
        unsafe { std::env::set_var(key, "not-a-number") };
        assert_eq!(request_timeout_secs(), 60);

        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn tool_call_timeout_secs_defaults_and_overrides() {
        let _g = ENV_LOCK.lock().unwrap();
        let key = "AHMA_HTTP_BRIDGE_TOOL_CALL_TIMEOUT_SECS";
        let prev = std::env::var(key).ok();

        unsafe { std::env::remove_var(key) };
        assert_eq!(tool_call_timeout_secs(), 60);

        unsafe { std::env::set_var(key, "5") };
        assert_eq!(tool_call_timeout_secs(), 5);

        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    // ── SessionManager: config-only / in-memory peer logic ───────────────────

    /// A `PeerFactory` backed by in-memory duplex pipes. It retains the peer
    /// ends so the bridge's stdout reader does not see EOF (which would mark the
    /// session terminated) and stdin writes never block.
    struct DuplexPeerFactory {
        peer_ends: std::sync::Mutex<Vec<(tokio::io::DuplexStream, tokio::io::DuplexStream)>>,
    }

    impl DuplexPeerFactory {
        fn new() -> Self {
            Self {
                peer_ends: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl PeerFactory for DuplexPeerFactory {
        fn create(&self) -> crate::peer::BoxFuture<anyhow::Result<PeerStreams>> {
            let (bridge_stdin, peer_reader) = tokio::io::duplex(8192);
            let (peer_writer, bridge_stdout) = tokio::io::duplex(8192);
            self.peer_ends
                .lock()
                .unwrap()
                .push((peer_reader, peer_writer));
            Box::pin(async move {
                Ok(PeerStreams {
                    stdin: Box::new(bridge_stdin),
                    stdout: Box::new(bridge_stdout),
                    stderr: None,
                    shutdown_fn: None,
                    exit_cause: None,
                })
            })
        }
    }

    fn test_config(default_scope: Option<PathBuf>, max_sessions: usize) -> SessionManagerConfig {
        SessionManagerConfig {
            server_command: "unused".to_string(),
            server_args: vec![],
            default_scope,
            enable_colored_output: false,
            handshake_timeout_secs: 45,
            max_sessions,
            peer_factory: Some(Arc::new(DuplexPeerFactory::new())),
        }
    }

    #[test]
    fn requires_client_roots_depends_on_default_scope() {
        let mgr_none = SessionManager::new(test_config(None, 8));
        assert!(mgr_none.requires_client_roots());
        let mgr_some = SessionManager::new(test_config(Some(std::env::temp_dir().join("p")), 8));
        assert!(!mgr_some.requires_client_roots());
    }

    #[test]
    fn resolve_sandbox_scopes_parses_file_uris() {
        let mgr = SessionManager::new(test_config(None, 8));
        let dir = std::env::temp_dir().join("ahma_scope_dir");
        let uri = format!("file://{}", dir.to_string_lossy());
        let roots = vec![McpRoot {
            uri,
            name: Some("proj".to_string()),
        }];
        let scopes = mgr.resolve_sandbox_scopes("sid", &roots).unwrap();
        assert_eq!(scopes.len(), 1);
    }

    #[test]
    fn resolve_sandbox_scopes_empty_uses_default_scope() {
        let scope = std::env::temp_dir().join("fallback");
        let mgr = SessionManager::new(test_config(Some(scope.clone()), 8));
        let scopes = mgr.resolve_sandbox_scopes("sid", &[]).unwrap();
        assert_eq!(scopes, vec![scope]);
    }

    #[test]
    fn resolve_sandbox_scopes_empty_no_default_errors() {
        let mgr = SessionManager::new(test_config(None, 8));
        let err = mgr.resolve_sandbox_scopes("sid", &[]).unwrap_err();
        assert!(err.to_string().contains("did not provide roots"));
    }

    #[test]
    fn resolve_sandbox_scopes_invalid_uris_error() {
        // Non-empty roots, but none parse to a file:// path → error branch.
        let mgr = SessionManager::new(test_config(None, 8));
        let roots = vec![McpRoot {
            uri: "https://example.com/x".to_string(),
            name: None,
        }];
        let err = mgr.resolve_sandbox_scopes("sid", &roots).unwrap_err();
        assert!(err.to_string().contains("No valid file://"));
    }

    #[tokio::test]
    async fn create_session_tracks_counts_and_active_counter() {
        let mut mgr = SessionManager::new(test_config(None, 8));
        let counter = Arc::new(AtomicUsize::new(0));
        mgr.active_sessions = Some(counter.clone());

        let id = mgr.create_session().await.unwrap();
        assert_eq!(mgr.session_count(), 1);
        assert!(mgr.session_exists(&id));
        assert!(mgr.get_session(&id).is_some());
        assert_eq!(mgr.get_all_sessions().len(), 1);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        mgr.terminate_session(&id, SessionTerminationReason::ClientRequested)
            .await
            .unwrap();
        assert_eq!(mgr.session_count(), 0);
        assert!(!mgr.session_exists(&id));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn create_session_enforces_max_sessions() {
        let mgr = SessionManager::new(test_config(None, 1));
        mgr.create_session().await.unwrap();
        let err = mgr.create_session().await.unwrap_err();
        assert!(err.to_string().contains("Session limit exceeded"));
    }

    /// A peer that stays alive until the test drops its retained end, and whose
    /// `exit_cause` channel is handed back so the test can play the role of the
    /// exit monitor.
    struct DyingPeerFactory {
        peer_end: std::sync::Mutex<Option<tokio::io::DuplexStream>>,
        cause_tx: std::sync::Mutex<Option<oneshot::Sender<String>>>,
    }

    impl PeerFactory for DyingPeerFactory {
        fn create(&self) -> crate::peer::BoxFuture<anyhow::Result<PeerStreams>> {
            let (bridge_end, peer_end) = tokio::io::duplex(8192);
            *self.peer_end.lock().unwrap() = Some(peer_end);
            let (cause_tx, cause_rx) = oneshot::channel();
            *self.cause_tx.lock().unwrap() = Some(cause_tx);
            let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
            Box::pin(async move {
                Ok(PeerStreams {
                    stdin: Box::new(bridge_write),
                    stdout: Box::new(bridge_read),
                    stderr: None,
                    shutdown_fn: None,
                    exit_cause: Some(cause_rx),
                })
            })
        }
    }

    /// R-SIGN.5: when the peer dies abnormally, the classified cause from the
    /// exit monitor must reach the client's JSON-RPC error — not a bare
    /// "Session terminated unexpectedly".
    #[tokio::test]
    async fn peer_death_cause_reaches_pending_request_error() {
        let factory = Arc::new(DyingPeerFactory {
            peer_end: std::sync::Mutex::new(None),
            cause_tx: std::sync::Mutex::new(None),
        });
        let config = SessionManagerConfig {
            server_command: "unused".to_string(),
            server_args: vec![],
            default_scope: None,
            enable_colored_output: false,
            handshake_timeout_secs: 45,
            max_sessions: 8,
            peer_factory: Some(factory.clone()),
        };
        let mgr = SessionManager::new(config);
        let id = mgr.create_session().await.unwrap();
        let session = mgr.get_session(&id).unwrap();

        // A request is in flight when the peer dies.
        let (tx, rx) = oneshot::channel();
        session.pending_requests.insert("42".to_string(), tx);

        // Play the exit monitor: classify the death, THEN let the bridge see
        // the pipe EOF (the order the race can also produce in production).
        let cause_tx = factory.cause_tx.lock().unwrap().take().unwrap();
        cause_tx
            .send("killed by SIGKILL (possible OOM-kill or code-signing kill).".to_string())
            .unwrap();
        drop(factory.peer_end.lock().unwrap().take());

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("pending request must be answered")
            .expect("error response must be sent, not dropped");
        let message = response["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("SIGKILL"),
            "client error must name the classified cause: {message}"
        );
    }

    #[tokio::test]
    async fn lock_sandbox_locks_once_then_noop() {
        let scope = std::env::temp_dir().join("lock_proj");
        let mgr = SessionManager::new(test_config(Some(scope.clone()), 8));
        let id = mgr.create_session().await.unwrap();

        let first = mgr.lock_sandbox(&id, &[]).await.unwrap();
        assert!(first, "first lock returns true");
        let session = mgr.get_session(&id).unwrap();
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Configuring { .. }
        ));

        // Second lock is a no-op (already past AwaitingRoots).
        let second = mgr.lock_sandbox(&id, &[]).await.unwrap();
        assert!(!second);
    }

    #[tokio::test]
    async fn lock_sandbox_unknown_session_errors() {
        let mgr = SessionManager::new(test_config(None, 8));
        let err = mgr.lock_sandbox("does-not-exist", &[]).await.unwrap_err();
        assert!(err.to_string().contains("Session not found"));
    }

    #[tokio::test]
    async fn auto_lock_if_default_scope_behaviour() {
        // None scope → no-op, stays AwaitingRoots.
        let mgr_none = SessionManager::new(test_config(None, 8));
        let id_n = mgr_none.create_session().await.unwrap();
        mgr_none.auto_lock_if_default_scope(&id_n).await;
        assert!(matches!(
            mgr_none.get_session(&id_n).unwrap().current_sandbox_state(),
            SandboxState::AwaitingRoots
        ));

        // Some scope → locks into Configuring.
        let scope = std::env::temp_dir().join("auto_lock");
        let mgr_some = SessionManager::new(test_config(Some(scope), 8));
        let id_s = mgr_some.create_session().await.unwrap();
        mgr_some.auto_lock_if_default_scope(&id_s).await;
        assert!(matches!(
            mgr_some.get_session(&id_s).unwrap().current_sandbox_state(),
            SandboxState::Configuring { .. }
        ));
    }

    #[tokio::test]
    async fn handle_roots_changed_before_and_after_lock() {
        let scope = std::env::temp_dir().join("roots_changed");
        let mgr = SessionManager::new(test_config(Some(scope), 8));
        let id = mgr.create_session().await.unwrap();

        // Before lock: AwaitingRoots → false (normal handshake, forward it).
        assert!(!mgr.handle_roots_changed(&id).await.unwrap());

        mgr.lock_sandbox(&id, &[]).await.unwrap();
        // After lock: tolerated no-op → true (do not forward).
        assert!(mgr.handle_roots_changed(&id).await.unwrap());
    }

    #[tokio::test]
    async fn handle_roots_changed_unknown_session_errors() {
        let mgr = SessionManager::new(test_config(None, 8));
        let err = mgr.handle_roots_changed("nope").await.unwrap_err();
        assert!(err.to_string().contains("Session not found"));
    }

    #[tokio::test]
    async fn send_message_validates_session_state() {
        let mgr = SessionManager::new(test_config(None, 8));
        // Unknown session.
        assert!(
            mgr.send_message("nope", &json!({"method": "ping"}))
                .await
                .is_err()
        );

        let id = mgr.create_session().await.unwrap();
        // Live session accepts the message.
        mgr.send_message(&id, &json!({"jsonrpc": "2.0", "method": "ping"}))
            .await
            .unwrap();

        // Terminated session rejects.
        mgr.get_session(&id).unwrap().set_terminated(true);
        let err = mgr
            .send_message(&id, &json!({"method": "ping"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("terminated"));
    }

    #[tokio::test]
    async fn send_request_notification_returns_null() {
        let mgr = SessionManager::new(test_config(None, 8));
        let id = mgr.create_session().await.unwrap();
        // A request with no id is a notification: await_response short-circuits.
        let res = mgr
            .send_request(&id, &json!({"jsonrpc": "2.0", "method": "ping"}), None)
            .await
            .unwrap();
        assert_eq!(res["result"], Value::Null);
    }

    #[tokio::test]
    async fn send_request_terminated_and_unknown_session_error() {
        let mgr = SessionManager::new(test_config(None, 8));
        assert!(
            mgr.send_request("nope", &json!({"id": 1, "method": "ping"}), None)
                .await
                .is_err()
        );

        let id = mgr.create_session().await.unwrap();
        mgr.get_session(&id).unwrap().set_terminated(true);
        let err = mgr
            .send_request(&id, &json!({"id": 1, "method": "ping"}), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("terminated"));
    }

    #[tokio::test]
    async fn terminate_session_unknown_is_ok() {
        let mgr = SessionManager::new(test_config(None, 8));
        // Removing a session that does not exist is a no-op success.
        mgr.terminate_session("ghost", SessionTerminationReason::Timeout)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn terminate_all_clears_sessions() {
        let mgr = SessionManager::new(test_config(None, 8));
        mgr.create_session().await.unwrap();
        mgr.create_session().await.unwrap();
        assert_eq!(mgr.session_count(), 2);
        mgr.terminate_all(SessionTerminationReason::ClientRequested)
            .await;
        assert_eq!(mgr.session_count(), 0);
    }

    /// A client with a request in flight must be TOLD the session is gone.
    ///
    /// Regression: termination dropped the pending response channels instead of
    /// answering them — and it did so *after* the peer shutdown, which could hang,
    /// so in practice the client was told nothing at all and simply waited.
    #[tokio::test]
    async fn terminate_session_answers_in_flight_requests_with_an_error() {
        let mgr = SessionManager::new(test_config(None, 8));
        let session_id = mgr.create_session().await.unwrap();
        let session = mgr.sessions.get(&session_id).unwrap().clone();

        let rx = register_pending_request(&session.pending_requests, Some(&"7".to_string()))
            .expect("a request is now in flight");

        mgr.terminate_session(&session_id, SessionTerminationReason::ClientRequested)
            .await
            .unwrap();

        let response = rx
            .await
            .expect("the in-flight request must receive a response, not a dropped channel");
        assert_eq!(response["id"], "7");
        assert_eq!(response["error"]["code"], -32603);
        let message = response["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("shutting down") || message.contains("restarting"),
            "the error must say why the request died, got: {message}"
        );
    }

    /// A peer that refuses to die must not strand the client.
    ///
    /// This is the exact shape of the real incident: the bridge announced
    /// "Shutting down bridge process...", then blocked forever waiting for a
    /// subprocess that never exited. It never finished terminating, never exited,
    /// and never closed the client's connection — so the client heard nothing for
    /// tens of minutes. `PEER_SHUTDOWN_GRACE` bounds that wait.
    #[tokio::test(start_paused = true)]
    async fn hanging_peer_shutdown_cannot_strand_an_in_flight_request() {
        let mgr = SessionManager::new(test_config(None, 8));
        let session_id = mgr.create_session().await.unwrap();
        let session = mgr.sessions.get(&session_id).unwrap().clone();

        // A peer whose shutdown never completes — the subprocess that would not die.
        let hangs_forever: PeerShutdownFn =
            Box::new(|| Box::pin(async { std::future::pending::<()>().await }));
        *session.peer_shutdown.lock().await = Some(hangs_forever);

        let rx = register_pending_request(&session.pending_requests, Some(&"9".to_string()))
            .expect("a request is now in flight");

        // Termination must COMPLETE despite the hung peer.
        tokio::time::timeout(
            PEER_SHUTDOWN_GRACE + Duration::from_secs(5),
            mgr.terminate_session(&session_id, SessionTerminationReason::ClientRequested),
        )
        .await
        .expect("termination must not hang on an unresponsive peer")
        .unwrap();

        // ...and the client must have been told, not left waiting.
        let response = rx
            .await
            .expect("the in-flight request must be answered even when the peer hangs");
        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(mgr.session_count(), 0);
    }

    #[test]
    fn config_debug_renders_peer_factory_state() {
        let with_factory = test_config(None, 4);
        let dbg_some = format!("{with_factory:?}");
        assert!(dbg_some.contains("Some(<PeerFactory>)"));

        let without = SessionManagerConfig {
            server_command: "x".to_string(),
            server_args: vec![],
            default_scope: None,
            enable_colored_output: false,
            handshake_timeout_secs: 45,
            max_sessions: 1,
            peer_factory: None,
        };
        let dbg_none = format!("{without:?}");
        assert!(dbg_none.contains("peer_factory: \"None\""));
    }

    #[test]
    fn handshake_state_fsm_names_and_terminal() {
        assert_eq!(HandshakeState::AwaitingBoth.name(), "AwaitingBoth");
        assert_eq!(HandshakeState::AwaitingSseOnly.name(), "AwaitingSseOnly");
        assert_eq!(HandshakeState::AwaitingMcpOnly.name(), "AwaitingMcpOnly");
        assert_eq!(HandshakeState::RootsRequested.name(), "RootsRequested");
        assert_eq!(HandshakeState::Complete.name(), "Complete");
        assert!(HandshakeState::Complete.is_terminal());
        assert!(!HandshakeState::RootsRequested.is_terminal());
    }
}
