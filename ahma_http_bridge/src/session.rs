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
//! For security, session isolation mode rejects roots changes after the sandbox is locked.
//! If `notifications/roots/list_changed` is received after locking, the subprocess is
//! immediately terminated to prevent sandbox escape.

use crate::error::{BridgeError, Result};
use crate::peer::{PeerFactory, PeerShutdownFn, PeerStreams, SubprocessPeerFactory};
use ahma_common::sandbox_state::{SandboxState, SandboxStateMachine};
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
    /// Sandbox scopes (set on first roots/list response) - supports multiple roots
    sandbox_scopes: Mutex<Option<Vec<PathBuf>>>,
    /// Whether the session has been terminated
    terminated: AtomicBool,
    /// Termination reason (if terminated)
    termination_reason: Mutex<Option<SessionTerminationReason>>,
    /// Async cleanup hook invoked on explicit session termination.
    ///
    /// For subprocess peers this kills the child process.  For in-memory peers
    /// this is `None` (drop semantics handle cleanup).
    peer_shutdown: Mutex<Option<PeerShutdownFn>>,

    /// Handshake state machine protected by a sync Mutex for atomic transitions
    handshake_state: std::sync::Mutex<HandshakeState>,

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
        *self.handshake_state.lock().unwrap()
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

    /// Get the first sandbox scope
    pub async fn get_sandbox_scope(&self) -> Option<PathBuf> {
        self.sandbox_scopes
            .lock()
            .await
            .as_ref()
            .and_then(|v| v.first().cloned())
    }

    /// Get all sandbox scopes
    pub async fn get_sandbox_scopes(&self) -> Option<Vec<PathBuf>> {
        self.sandbox_scopes.lock().await.clone()
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
        let mut state = self.handshake_state.lock().unwrap();
        match *state {
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
            _ => {
                debug!(session_id = %self.id, state = ?*state, "SSE connected but already handled/advanced");
                HandshakeAction::None
            }
        }
    }

    /// Helper to transition state for MCP initialization
    fn transition_mcp_initialized(&self) -> HandshakeAction {
        let mut state = self.handshake_state.lock().unwrap();
        match *state {
            HandshakeState::AwaitingBoth => {
                *state = HandshakeState::AwaitingMcpOnly;
                info!(session_id = %self.id, from = ?HandshakeState::AwaitingBoth, to = ?HandshakeState::AwaitingMcpOnly, "MCP initialized");
                self.mcp_initialized_notify.notify_waiters();
                HandshakeAction::None
            }
            HandshakeState::AwaitingSseOnly => {
                *state = HandshakeState::RootsRequested;
                info!(session_id = %self.id, from = ?HandshakeState::AwaitingSseOnly, to = ?HandshakeState::RootsRequested, "MCP initialized (completing handshake)");
                self.mcp_initialized_notify.notify_waiters();
                HandshakeAction::SendRootsListChanged
            }
            _ => {
                debug!(session_id = %self.id, state = ?*state, "MCP initialized but already handled/advanced");
                // Ensure waiters are notified even if state was already advanced
                self.mcp_initialized_notify.notify_waiters();
                HandshakeAction::None
            }
        }
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
        let mut state = self.handshake_state.lock().unwrap();
        if *state == HandshakeState::RootsRequested {
            *state = HandshakeState::Complete;
            info!(session_id = %self.id, "Handshake complete");
        }
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
            Err(BridgeError::Communication("Request timed out".to_string()))
        }
    }
}

/// Handle sandbox lifecycle notifications received from the subprocess.
///
/// Drives the `SandboxStateMachine` forward based on `notifications/sandbox/*` methods.
fn handle_sandbox_configured(session: &Arc<Session>) {
    if let Err(e) = session.sandbox_state_machine.transition_to_active() {
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
            handle_sandbox_configured(session);
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
        let current_count = self.sessions.len();
        if current_count >= self.config.max_sessions {
            return Err(BridgeError::ServerProcess(format!(
                "Session limit exceeded (max: {})",
                self.config.max_sessions
            )));
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
            sandbox_scopes: Mutex::new(None),
            terminated: AtomicBool::new(false),
            termination_reason: Mutex::new(None),
            peer_shutdown: Mutex::new(shutdown_fn),
            handshake_state: std::sync::Mutex::new(HandshakeState::AwaitingBoth),
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

        // Store the sandbox scopes
        *session.sandbox_scopes.lock().await = Some(scopes.clone());

        // Transition to Configuring state
        if let Err(e) = session
            .sandbox_state_machine
            .transition_to_configuring(scopes.clone())
        {
            warn!(session_id = %session_id, error = %e, "Failed to transition sandbox state to Configuring");
        }

        // Transition handshake state to Complete
        session.mark_handshake_complete();

        Ok(true)
    }

    /// Handle roots/list_changed notification
    ///
    /// Per R8D.12-R8D.13, if sandbox is locked, terminate the session immediately
    pub async fn handle_roots_changed(&self, session_id: &str) -> Result<()> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            BridgeError::Communication(format!("Session not found: {}", session_id))
        })?;

        if !matches!(
            session.sandbox_state_machine.current(),
            SandboxState::AwaitingRoots
        ) {
            // Security violation: attempt to change roots after sandbox lock
            let scopes = session.sandbox_scopes.lock().await.clone();
            error!(
                session_id = %session_id,
                sandbox_scopes = ?scopes,
                "Roots change rejected after sandbox lock - terminating session"
            );

            // Drop the session reference before terminating to avoid deadlock
            drop(session);

            self.terminate_session(session_id, SessionTerminationReason::RootsChangeRejected)
                .await?;

            return Err(BridgeError::Communication(
                "Session terminated: roots change not allowed after sandbox lock".to_string(),
            ));
        }

        // Sandbox not yet locked - this is unusual but allowed
        // (roots/list hasn't been processed yet)
        warn!(
            session_id = %session_id,
            "Roots change received before sandbox lock - allowing"
        );
        Ok(())
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

            // Run peer-specific cleanup (e.g. kill the subprocess).
            if let Some(shutdown_fn) = session.peer_shutdown.lock().await.take() {
                shutdown_fn().await;
            }

            // Clear pending requests
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
                            "message": "Session terminated unexpectedly - subprocess may have crashed or handshake failed"
                        }
                    });
                    let _ = sender.send(error_response);
                }
            }
        }
    }
}
