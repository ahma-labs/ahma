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
use ahma_common::mcp_methods::{
    PUSH_CHANNEL_CHANGED_METHOD, PushChannelChangedParams, SANDBOX_CONFIGURED_METHOD,
    SANDBOX_FAILED_METHOD, SandboxLifecycleParams,
};
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
    /// Both SSE and MCP initialized, roots/list_changed sent. Terminal for this
    /// machine: the sandbox lock itself is tracked by the `SandboxStateMachine`.
    RootsRequested,
}

impl FsmState for HandshakeState {
    fn name(&self) -> &'static str {
        match self {
            HandshakeState::AwaitingBoth => "AwaitingBoth",
            HandshakeState::AwaitingSseOnly => "AwaitingSseOnly",
            HandshakeState::AwaitingMcpOnly => "AwaitingMcpOnly",
            HandshakeState::RootsRequested => "RootsRequested",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, HandshakeState::RootsRequested)
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

/// Default request timeout in seconds for bridge → subprocess calls
/// (see [`SessionManagerConfig::request_timeout_secs`]).
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 60;

/// Default `tools/call` request timeout in seconds
/// (see [`SessionManagerConfig::tool_call_timeout_secs`]).
pub const DEFAULT_TOOL_CALL_TIMEOUT_SECS: u64 = 60;

/// Default cap on concurrent sessions
/// (see [`SessionManagerConfig::max_sessions`]).
pub const DEFAULT_MAX_SESSIONS: usize = 100;

/// Session termination reason
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTerminationReason {
    /// Client requested termination (HTTP DELETE)
    ClientRequested,
    /// Subprocess crashed
    ProcessCrashed,
    /// Session timed out
    Timeout,
    /// The real client missed enough consecutive bridge-initiated liveness
    /// pings in a row to be treated as gone (SPEC RB.4).
    Unresponsive,
}

/// Represents an active client session.
pub struct Session {
    /// Unique session identifier
    pub id: String,
    /// Channel to send messages to the subprocess.
    sender: Mutex<mpsc::Sender<String>>,
    /// Map of pending request IDs to response channels
    pending_requests: Arc<DashMap<String, oneshot::Sender<Value>>>,
    /// Broadcast channel for SSE events from this session.
    /// Each message is `(event_id, json_string)`.
    broadcast_tx: broadcast::Sender<(u64, String)>,
    /// Whether the session has been terminated
    terminated: AtomicBool,
    /// Wakes the I/O task's termination branch when `terminated` flips to true,
    /// so it does not have to poll the flag.
    terminated_notify: Notify,
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
    event_history: parking_lot::Mutex<VecDeque<(u64, String)>>,

    /// Client info sent in initialize request
    pub client_info: Mutex<Option<Value>>,
    /// Client capabilities sent in initialize request
    pub capabilities: Mutex<Option<Value>>,
    /// Map of pending routed request IDs to response channels
    pub routed_requests: Arc<DashMap<String, oneshot::Sender<Value>>>,
    /// Map of pending server-to-client request IDs to their method name (e.g. roots/list)
    pub pending_client_requests: Arc<DashMap<String, String>>,
    /// Semaphore limiting concurrent routed sampling requests to the client (default: 3).
    /// A bounded semaphore replaces the old 1-at-a-time Mutex so that up to N sampling
    /// requests can be in-flight simultaneously, preventing head-of-line blocking when
    /// an IDE session hosts multiple agents.
    pub sampling_semaphore: Arc<tokio::sync::Semaphore>,
    /// Consecutive bridge-initiated liveness pings (see [`ping_session_client`])
    /// the real client has missed in a row. Reset to 0 by any answered ping;
    /// a session is terminated once this reaches
    /// [`SessionManager::MAX_MISSED_LIVENESS_PINGS`].
    ///
    /// Distinct from the subprocess↔bridge keepalive in `ahma_common::keepalive`:
    /// that one is answered by the bridge on the client's behalf
    /// (`answer_subprocess_ping`) whenever an SSE subscriber is merely
    /// *attached*, so it cannot tell a live client from a socket the OS still
    /// thinks is open but nobody is reading (a hung/crashed client, a dead
    /// network path). This counter tracks an actual round trip to that client.
    missed_liveness_pings: AtomicU64,
    /// Wall-clock time of the most recent request the real client sent the
    /// bridge on this session (any `POST /mcp` — a tool call, a client
    /// response, anything; see `touch_client_activity`). The liveness prober
    /// only pings sessions that have gone quiet for at least one interval, so
    /// a session that is plainly busy is never penalized merely for not also
    /// answering an unsolicited `ping` — many legitimate MCP clients (thin
    /// integrations, test harnesses) never implement the server→client `ping`
    /// side of the protocol at all, and treating silence to it as death would
    /// kill a session that is actively working.
    last_client_activity: parking_lot::Mutex<Instant>,
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

    /// Set the terminated status of the session.
    pub fn set_terminated(&self, terminated: bool) {
        self.terminated.store(terminated, Ordering::SeqCst);
        if terminated {
            self.terminated_notify.notify_waiters();
        }
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
            HandshakeState::AwaitingSseOnly | HandshakeState::RootsRequested
        )
    }

    /// Check if MCP initialized notification was received
    pub fn is_mcp_initialized(&self) -> bool {
        matches!(
            self.handshake_state(),
            HandshakeState::AwaitingMcpOnly | HandshakeState::RootsRequested
        )
    }

    /// Wait for MCP initialization.
    pub async fn wait_for_mcp_initialized(&self) {
        if self.is_mcp_initialized() {
            return;
        }
        self.mcp_initialized_notify.notified().await;
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
    pub fn get_sandbox_scope(&self) -> Option<PathBuf> {
        self.sandbox_state_machine
            .current()
            .scopes()
            .and_then(|s| s.first().cloned())
    }

    /// Get all sandbox scopes (from the sandbox state machine).
    pub fn get_sandbox_scopes(&self) -> Option<Vec<PathBuf>> {
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

    /// Record that a liveness ping to the real client was answered: reset the
    /// consecutive-miss counter.
    fn record_liveness_pong(&self) {
        self.missed_liveness_pings.store(0, Ordering::Relaxed);
    }

    /// Record that a liveness ping to the real client went unanswered.
    /// Returns the new consecutive-miss count.
    fn record_liveness_miss(&self) -> u64 {
        self.missed_liveness_pings.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record that the real client sent the bridge a request on this session
    /// (SPEC RB.4). Any genuine traffic is at least as strong a liveness
    /// signal as an answered ping, so this also clears the missed-ping
    /// counter — a session that just made a tool call must not be one
    /// stray unanswered ping away from termination.
    pub fn touch_client_activity(&self) {
        *self.last_client_activity.lock() = Instant::now();
        self.missed_liveness_pings.store(0, Ordering::Relaxed);
    }

    /// How long it has been since the real client last sent the bridge a
    /// request on this session.
    fn idle_since_last_activity(&self) -> Duration {
        self.last_client_activity.lock().elapsed()
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
        let mut history = self.event_history.lock();
        history.push_back((id, msg.to_string()));
        if history.len() > EVENT_HISTORY_CAPACITY {
            history.pop_front();
        }
        id
    }

    /// Return events with ID > `last_id` from the replay buffer.
    pub fn replay_events_after(&self, last_id: u64) -> Vec<(u64, String)> {
        let history = self.event_history.lock();
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
        // Clone the sender under a short lock rather than holding the Mutex
        // across the (potentially blocking) `send().await`.
        let sender = self.sender.lock().await.clone();
        sender
            .send(json_str)
            .await
            .map_err(|e| BridgeError::Communication(format!("{error_context}: {e}")))?;
        Ok(())
    }

    /// One handshake half has arrived (SSE stream opened, or MCP initialized):
    /// advance the state machine and return the follow-up action.
    ///
    /// The two halves are mirror images: from `AwaitingBoth` the arriving half
    /// moves to its partial state (`partial_to`); from the state that was only
    /// waiting for this half (`completing_from`) it completes the handshake.
    fn transition_handshake_half(
        &self,
        event: &'static str,
        partial_to: HandshakeState,
        completing_from: HandshakeState,
    ) -> HandshakeAction {
        self.handshake_state.transition(|state| match *state {
            HandshakeState::AwaitingBoth => {
                *state = partial_to;
                info!(session_id = %self.id, from = ?HandshakeState::AwaitingBoth, to = ?partial_to, "{event}");
                HandshakeAction::None
            }
            s if s == completing_from => {
                *state = HandshakeState::RootsRequested;
                info!(session_id = %self.id, from = ?completing_from, to = ?HandshakeState::RootsRequested, "{event} (completing handshake)");
                HandshakeAction::SendRootsListChanged
            }
            other => {
                debug!(session_id = %self.id, state = ?other, "{event} but already handled/advanced");
                HandshakeAction::None
            }
        })
    }

    /// Helper to transitions state and return necessary action
    fn transition_sse_connected(&self) -> HandshakeAction {
        self.transition_handshake_half(
            "SSE connected",
            HandshakeState::AwaitingSseOnly,
            HandshakeState::AwaitingMcpOnly,
        )
    }

    /// Helper to transition state for MCP initialization
    fn transition_mcp_initialized(&self) -> HandshakeAction {
        let action = self.transition_handshake_half(
            "MCP initialized",
            HandshakeState::AwaitingMcpOnly,
            HandshakeState::AwaitingSseOnly,
        );
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

    /// Send roots/list_changed notification to subprocess.
    async fn send_roots_list_changed(&self) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": ahma_common::mcp_methods::ROOTS_LIST_CHANGED_METHOD
        });
        self.send_to_subprocess(&notification, "Failed to send roots/list_changed")
            .await?;
        let sse_receivers = self.broadcast_tx.receiver_count();
        info!(session_id = %self.id, sse_receivers = sse_receivers, "Sent roots/list_changed to subprocess; waiting for roots/list response via broadcast");
        Ok(())
    }

    /// Tell the subprocess whether it currently has a live push channel to the
    /// real client (an open `GET /mcp` SSE stream) — the signal a future
    /// liveness probe needs before attempting a server-initiated request
    /// mid-`await`, since a subprocess-initiated message with no SSE
    /// subscriber is silently dropped (see `dispatch_subprocess_line`).
    ///
    /// Best-effort: the subprocess defaults to "no live channel" (the safe,
    /// conservative assumption) until the first `true` arrives, so a failure
    /// to deliver this notification only means the subprocess stays
    /// conservative — it never causes it to wrongly assume a channel exists.
    pub async fn send_push_channel_changed(&self, connected: bool) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": PUSH_CHANNEL_CHANGED_METHOD,
            "params": PushChannelChangedParams { connected }
        });
        self.send_to_subprocess(&notification, "Failed to send pushChannelChanged")
            .await
    }
}

/// What to tell the user when a session cannot get a sandbox scope: the client
/// reported no roots and no fallback is configured.
///
/// One string because the condition has one remedy, and it was previously
/// spelled three times — twice in `request_handler` and once here — which had
/// already drifted ("Configure **an** explicit sandbox scope" vs "Configure
/// explicit sandbox scope"), and only one of the three mentioned the
/// `[sandbox] container_root` setting. Which advice the user got depended on
/// which code path happened to fire.
pub const NO_SANDBOX_SCOPE_REMEDIATION: &str = "Start the bridge with `--sandbox-scope <project-dir>`, or set `[sandbox] container_root` \
     in ~/.ahma/settings.toml, or use a client that supports roots/list.";

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
    /// Default timeout in seconds for bridge → subprocess request/response calls
    /// (used for everything except `tools/call`, which has its own budget —
    /// see `tool_call_timeout_secs`). Defaults to 60 seconds.
    pub request_timeout_secs: u64,
    /// Default timeout in seconds for `tools/call` requests, unless the
    /// caller's `timeout_seconds` argument overrides it (still capped at
    /// `BRIDGE_TOOL_CALL_CEILING_SECS`). Defaults to 60 seconds.
    pub tool_call_timeout_secs: u64,
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

    /// Translates a session's query string into worker arguments.
    ///
    /// The allowlist that decides which options exist lives in `ahma_mcp`,
    /// which depends on this crate; the daemon injects the translator here so
    /// the dependency does not have to point the other way.
    pub session_options: Option<SessionOptionTranslator>,
}

/// Turns a session's URL query into worker arguments, or explains why it will
/// not (SPEC R-DAEMON.4).
pub type SessionOptionTranslator =
    Arc<dyn Fn(&str) -> std::result::Result<Vec<String>, String> + Send + Sync>;

impl Default for SessionManagerConfig {
    /// Production defaults: a subprocess-backed manager (`peer_factory: None`)
    /// running `ahma` with no arguments, no fallback scope (so clients must
    /// supply roots), and the `DEFAULT_*` timeouts and session cap.
    ///
    /// Construction sites should set only the fields they actually care about
    /// and fill the rest with `..Default::default()`.
    fn default() -> Self {
        Self {
            server_command: "ahma".to_string(),
            server_args: vec![],
            default_scope: None,
            enable_colored_output: false,
            handshake_timeout_secs: DEFAULT_HANDSHAKE_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            tool_call_timeout_secs: DEFAULT_TOOL_CALL_TIMEOUT_SECS,
            max_sessions: DEFAULT_MAX_SESSIONS,
            peer_factory: None,
            session_options: None,
        }
    }
}

impl std::fmt::Debug for SessionManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManagerConfig")
            .field("server_command", &self.server_command)
            .field("server_args", &self.server_args)
            .field("default_scope", &self.default_scope)
            .field("enable_colored_output", &self.enable_colored_output)
            .field("handshake_timeout_secs", &self.handshake_timeout_secs)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("tool_call_timeout_secs", &self.tool_call_timeout_secs)
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
    /// Set once the hosting daemon has been asked to drain: existing sessions
    /// run to completion, new ones are refused so they are not started inside a
    /// process that is about to go (SPEC R-DAEMON.5).
    pub draining: Option<Arc<std::sync::atomic::AtomicBool>>,
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

/// Wait for a JSON-RPC response via a oneshot channel, or return immediately for notifications.
async fn await_response(
    response_rx: Option<oneshot::Receiver<Value>>,
    timeout: Option<Duration>,
    default_timeout: Duration,
    id_opt: &Option<String>,
    pending: &DashMap<String, oneshot::Sender<Value>>,
) -> Result<Value> {
    let Some(rx) = response_rx else {
        return Ok(serde_json::json!({"jsonrpc": "2.0", "result": null}));
    };
    let wait_timeout = timeout.unwrap_or(default_timeout);
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
/// payload (`params.scope.write`, an array of display path strings —
/// [`SandboxLifecycleParams`], SPEC R5.4/R5.6).
///
/// Returns an empty vec when the notification omits the scope summary (or
/// carries a malformed one — parsing is lenient, never an error); the caller
/// then preserves any in-flight `Configuring` scopes instead.
fn parse_configured_scopes(value: &Value) -> Vec<PathBuf> {
    SandboxLifecycleParams::from_notification(value)
        .scope
        .map(|scope| scope.write.into_iter().map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// Handle sandbox lifecycle notifications received from the subprocess.
///
/// Drives the `SandboxStateMachine` forward based on `notifications/sandbox/*` methods.
///
/// The subprocess's `notifications/sandbox/configured` is authoritative — the
/// sole input that opens the `tools/call` gate (SPEC RB.2): it is
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
    // Lenient: a missing or malformed `params.error` falls back to a generic
    // message — the Failed transition must happen regardless of payload shape.
    let err_msg = SandboxLifecycleParams::from_notification(value)
        .error
        .unwrap_or_else(|| "Unknown error".to_string());
    if let Err(e) = session
        .sandbox_state_machine
        .transition_to_failed(err_msg.clone())
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
        Some(SANDBOX_CONFIGURED_METHOD) => {
            handle_sandbox_configured(session, value);
        }
        Some(SANDBOX_FAILED_METHOD) => {
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

/// The bare MCP `ping` the subprocess sends mid-`await` to verify the client
/// is still there (SPEC R2.6.5.3).
const PING_METHOD: &str = "ping";

/// The map key for a JSON-RPC id already known to be present: the raw string
/// for a string id, its JSON rendering otherwise (ids may legitimately be
/// numbers). Deliberately *not* [`extract_request_id`], which maps a `null` id
/// to `None`; here a `null` id keys as `"null"`, matches no registered pending
/// request, and so falls through to the broadcast — the behaviour the inline
/// version in [`dispatch_subprocess_line`] has always had.
fn json_id_key(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), str::to_string)
}

/// Timestamp prefix shared by the colored frame echoes.
fn echo_timestamp() -> String {
    format!("[{}]", Local::now().format("%H:%M:%S%.3f"))
}

/// Render one frame for the colored echo: pretty-printed when it parses as
/// JSON, verbatim otherwise.
fn format_frame_for_echo(line: &str) -> String {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| line.to_string())
}

/// The session-id prefix used by the colored echoes.
///
/// Truncating with `&session_id[..8]` — which all three echoes did
/// independently — panics on an id shorter than 8 bytes, and on any id whose
/// eighth byte is not a char boundary. Session ids are UUIDs today, so neither
/// happens; a test peer, a renamed session, or a future id format is all it
/// would take. `char_indices` costs nothing here and cannot panic.
fn short_id(session_id: &str) -> String {
    let end = session_id
        .char_indices()
        .nth(8)
        .map_or(session_id.len(), |(i, _)| i);
    format!("[{}]", &session_id[..end])
}

/// Echo one frame: timestamp, session tag, direction label, body.
///
/// The three call sites below differ only in their colours and label, and not
/// uniformly — stderr styles its tag, label and body three different ways
/// while the other two use one colour throughout. So the styles stay explicit
/// per call rather than collapsing into a single `color` parameter, which
/// would have changed how stderr renders.
fn echo_frame(
    tag: impl std::fmt::Display,
    label: impl std::fmt::Display,
    body: impl std::fmt::Display,
) {
    eprintln!("{} {} {}\n{}", echo_timestamp(), tag, label, body);
}

/// Echo a frame read from the peer's stdout (green).
fn echo_stdout_frame(session_id: &str, line: &str) {
    echo_frame(
        short_id(session_id).green(),
        "← STDOUT:".green(),
        format_frame_for_echo(line).green(),
    );
}

/// Echo a frame written to the peer's stdin (cyan).
fn echo_stdin_frame(session_id: &str, msg: &str) {
    echo_frame(
        short_id(session_id).cyan(),
        "→ STDIN:".cyan(),
        format_frame_for_echo(msg).cyan(),
    );
}

/// Echo a line read from the peer's stderr (red tag, dimmed body).
fn echo_stderr_line(session_id: &str, line: &str) {
    echo_frame(
        short_id(session_id).red(),
        "STDERR:".yellow(),
        format_frame_for_echo(line).dimmed(),
    );
}

/// Write one newline-delimited frame to the peer's stdin and flush it.
///
/// Each stage is logged with its own message before the error is handed back,
/// so the caller only has to decide to stop the I/O loop.
async fn write_frame_to_peer(
    stdin: &mut (dyn AsyncWrite + Send + Unpin + 'static),
    session_id: &str,
    msg: &str,
) -> std::io::Result<()> {
    if let Err(e) = stdin.write_all(msg.as_bytes()).await {
        error!(session_id = %session_id, "Failed to write to stdin: {}", e);
        return Err(e);
    }
    if let Err(e) = stdin.write_all(b"\n").await {
        error!(session_id = %session_id, "Failed to write newline to stdin: {}", e);
        return Err(e);
    }
    if let Err(e) = stdin.flush().await {
        error!(session_id = %session_id, "Failed to flush stdin: {}", e);
        return Err(e);
    }
    Ok(())
}

/// Answer a subprocess-initiated `ping` on the client's behalf **iff** this
/// session holds a live push channel (an open SSE stream), otherwise leave it
/// unanswered so the subprocess's probe times out (SPEC R2.6.5.3).
///
/// Why the bridge answers instead of forwarding: the ping cannot reach the
/// real client through ahma's own stdio proxy while the `await` that sent it
/// is in flight — rmcp's streamable-HTTP client awaits each POST inline, so
/// nothing pushed over SSE is relayed until the `tools/call` response lands.
/// Forwarding therefore timed out the probe against a perfectly healthy
/// client ~25s into every long `await`, which the subprocess then reported as
/// a timeout. The live SSE stream *is* the liveness the probe was meant to
/// verify: when the client dies, its proxy exits, the stream closes, the
/// subscriber count drops to zero, and the next probe correctly goes
/// unanswered.
///
/// Synchronous on purpose — it runs inside the I/O loop's line dispatch. The
/// sender mutex is only ever held for a clone, so `try_lock` is expected to
/// succeed; if it does not (or the channel is momentarily full) the send is
/// retried on a task when a runtime is available.
fn answer_subprocess_ping(session: &Arc<Session>, id: Value) {
    let subscribers = session.broadcast_tx.receiver_count();
    if subscribers == 0 {
        warn!(
            session_id = %session.id,
            "Subprocess liveness ping left unanswered: no SSE subscriber, so there is \
             no live push channel to vouch for the client (SPEC R2.6.5.3)"
        );
        return;
    }
    let answer = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}});
    let json_str = match serde_json::to_string(&answer) {
        Ok(s) => s,
        Err(e) => {
            error!(session_id = %session.id, "Failed to serialize ping answer: {e}");
            return;
        }
    };
    debug!(
        session_id = %session.id,
        subscribers,
        "Answering subprocess liveness ping on the client's behalf (live push channel)"
    );
    deliver_ping_answer(session, json_str);
}

/// Hand a serialized ping answer to the subprocess.
///
/// The fast path is a `try_lock` + `try_send` from this synchronous context;
/// the sender mutex is only ever held for a clone, so it is expected to
/// succeed. If either declines (or the channel is momentarily full) the send is
/// retried on a task, when a runtime is available.
fn deliver_ping_answer(session: &Arc<Session>, json_str: String) {
    if let Ok(sender) = session.sender.try_lock()
        && sender.try_send(json_str.clone()).is_ok()
    {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        warn!(
            session_id = %session.id,
            "Ping answer not delivered: subprocess channel busy and no runtime to retry on"
        );
        return;
    };
    let session = Arc::clone(session);
    handle.spawn(async move {
        if let Err(e) = session
            .send_serialized_to_subprocess(json_str, "Failed to answer liveness ping")
            .await
        {
            warn!(session_id = %session.id, "Ping answer not delivered: {e}");
        }
    });
}

/// Actively probe one session's real downstream client for liveness over its
/// existing SSE channel, distinguishing "the SSE socket is still attached"
/// from "the client is actually there and answering" (SPEC RB.4) — the gap
/// `answer_subprocess_ping` cannot see, since it answers the subprocess's own
/// ping locally the moment an SSE subscriber is attached, without ever
/// reaching the client (SPEC R2.6.5.3).
///
/// Reuses the same routed-request mechanism as `handle_routed_sampling_request`:
/// register a oneshot under a fresh id in `routed_requests`, push a `ping`
/// request over the session's SSE broadcast, and wait for the client to POST
/// its answer back — `match_client_response_id` already resolves that POST
/// against `routed_requests` before any subprocess-forwarding logic runs, so
/// the answer cannot be misrouted to the subprocess.
///
/// Returns `true` iff the client answered within `timeout`.
async fn ping_session_client(session: &Arc<Session>, timeout: Duration) -> bool {
    let ping_id = format!("liveness_{}", Uuid::new_v4());
    let (tx, rx) = oneshot::channel();
    session.routed_requests.insert(ping_id.clone(), tx);

    let payload = serde_json::json!({"jsonrpc": "2.0", "id": ping_id, "method": PING_METHOD});
    let sent = serde_json::to_string(&payload)
        .ok()
        .is_some_and(|json_str| session.broadcast(json_str).is_ok());
    if !sent {
        session.routed_requests.remove(&ping_id);
        return false;
    }

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(_)) => true,
        _ => {
            session.routed_requests.remove(&ping_id);
            false
        }
    }
}

/// How one JSON-RPC frame read from the subprocess must be routed.
///
/// A frame carrying a `method` is a request or notification the subprocess is
/// *sending*; only a frame **without** one can be a response. The two id spaces
/// are independent — the subprocess numbers its own requests from 0 (rmcp), the
/// client numbers its own — so a subprocess request can carry the id of a
/// client call still pending here. Classifying on the id alone consumed that
/// request as the pending call's "response": the caller got a body with no
/// `result`, and the request itself was never delivered.
enum SubprocessFrame {
    /// No `id`: a notification. Broadcast only.
    Notification,
    /// `id`, no `method`: a response to a request the bridge may be holding.
    Response { id: String },
    /// The subprocess's liveness `ping` (SPEC R2.6.5.3), answered by the bridge.
    Ping { id: Value },
    /// `id` plus `method`: a server → client request, forwarded to the client.
    Request { id: String, method: String },
}

/// Classify a parsed subprocess frame. A `method` that is present but is not a
/// string counts as absent, exactly as the original `as_str()` test did.
fn classify_subprocess_frame(value: &Value) -> SubprocessFrame {
    let Some(id) = value.get("id") else {
        return SubprocessFrame::Notification;
    };
    match value.get("method").and_then(Value::as_str) {
        None => SubprocessFrame::Response {
            id: json_id_key(id),
        },
        Some(PING_METHOD) => SubprocessFrame::Ping { id: id.clone() },
        Some(method) => SubprocessFrame::Request {
            id: json_id_key(id),
            method: method.to_string(),
        },
    }
}

/// Broadcast a frame verbatim to the session's SSE subscribers, noting when
/// there are none — the event is then dropped because the stream is not open.
fn broadcast_to_subscribers(session: &Arc<Session>, value: &Value, line: &str) {
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

/// Process a single line received from subprocess stdout.
///
/// Routes JSON-RPC responses to waiting callers; broadcasts everything else
/// (notifications and server → client requests) to SSE subscribers and drives
/// sandbox state transitions.
fn dispatch_subprocess_line(session: &Arc<Session>, line: &str, colored_output: bool) {
    debug!(session_id = %session.id, "Received from subprocess: {}", line);

    if colored_output {
        echo_stdout_frame(&session.id, line);
    }

    let Ok(value) = serde_json::from_str::<Value>(line) else {
        warn!(session_id = %session.id, "Failed to parse JSON from subprocess: {}", line);
        return;
    };

    match classify_subprocess_frame(&value) {
        SubprocessFrame::Response { id } => {
            // Route to the waiting caller; a response whose id we are not
            // holding still falls through to the broadcast below.
            if let Some(sender) = take_pending_request(&session.pending_requests, &id) {
                let _ = sender.send(value);
                return;
            }
        }
        SubprocessFrame::Ping { id } => {
            answer_subprocess_ping(session, id);
            return;
        }
        SubprocessFrame::Request { id, method } => {
            // Remember the server → client request's method name so the
            // client's eventual response can be routed against it.
            session.pending_client_requests.insert(id, method);
        }
        SubprocessFrame::Notification => {}
    }

    // Drive sandbox state machine for lifecycle notifications
    handle_sandbox_notification(session, &value);

    broadcast_to_subscribers(session, &value, line);
}

impl SessionManager {
    /// Creates a new `SessionManager` with the given configuration.
    pub fn new(config: SessionManagerConfig) -> Self {
        Self {
            sessions: DashMap::new(),
            config,
            active_sessions: None,
            draining: None,
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
                manager.prune_stale_sessions().await;
            }
        });
    }

    /// How often the bridge actively pings each connected session's real
    /// downstream client to confirm it is still there and answering — not
    /// just that its SSE socket is still attached.
    ///
    /// Deliberately long: a real IDE session goes idle for minutes at a time
    /// as a matter of course (the human reading output, thinking, switching
    /// tasks), and the idle-gate in `ping_connected_sessions` only starts
    /// this clock once real traffic has *already* stopped. This is a probe
    /// for a session abandoned for a long time (the orphaned-worker problem
    /// this mechanism exists to catch), not a fast health check — a tight
    /// interval here risks killing an active user's session over an entirely
    /// normal pause. Windows CI proved this at `Duration::from_secs(30)`: a
    /// legitimate ~132s gap between requests in one e2e test was long enough
    /// to rack up two missed pings and get the session torn down mid-test.
    const LIVENESS_PING_INTERVAL: Duration = Duration::from_secs(5 * 60);

    /// How long a client has to answer one liveness ping before it counts as
    /// missed. Generous on purpose — the client may itself be busy or slow.
    const LIVENESS_PING_TIMEOUT: Duration = Duration::from_secs(30);

    /// Consecutive missed liveness pings before a session's client is treated
    /// as gone and the session (and its worker subprocess) is torn down.
    /// Combined with `LIVENESS_PING_INTERVAL`/`LIVENESS_PING_TIMEOUT`, this
    /// requires roughly 15+ minutes of total silence from the real client
    /// before a session is killed.
    const MAX_MISSED_LIVENESS_PINGS: u64 = 3;

    /// Spawns a background task that periodically pings every session with a
    /// live SSE subscriber, terminating any whose client stops answering
    /// (SPEC RB.4).
    ///
    /// This closes a gap the handshake/eviction sweeper (`start_sweeper`)
    /// cannot: a fully-initialized session is only ever reaped when its SSE
    /// receiver count drops to zero (`is_evictable`), but a crashed client,
    /// a hung process, or a dead network path can leave the TCP socket
    /// looking open to the OS indefinitely with nobody actually reading it.
    /// Without an active probe that session — and its sandboxed worker
    /// subprocess — never gets cleaned up.
    pub fn start_liveness_prober(self: &Arc<Self>) {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Self::LIVENESS_PING_INTERVAL).await;
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                manager
                    .ping_connected_sessions(
                        Self::LIVENESS_PING_TIMEOUT,
                        Self::LIVENESS_PING_INTERVAL,
                    )
                    .await;
            }
        });
    }

    /// Ping every session with a live SSE subscriber that has gone quiet, in
    /// parallel, and terminate any whose client has missed
    /// `MAX_MISSED_LIVENESS_PINGS` in a row. A session with real traffic more
    /// recent than `min_idle` is skipped entirely — it is already known to be
    /// alive, and many legitimate clients never implement the server→client
    /// `ping` side of the protocol, so probing a busy session risks killing
    /// it over a ping it was never going to answer regardless of whether it
    /// is alive. `timeout` and `min_idle` are parameters (rather than always
    /// the `LIVENESS_PING_*`/`LIVENESS_PING_INTERVAL` constants) so tests can
    /// drive this deterministically with short bounds.
    async fn ping_connected_sessions(&self, timeout: Duration, min_idle: Duration) {
        let candidates: Vec<Arc<Session>> = self
            .sessions
            .iter()
            .filter(|entry| {
                let session = entry.value();
                session.sse_receivers() > 0 && session.idle_since_last_activity() >= min_idle
            })
            .map(|entry| Arc::clone(entry.value()))
            .collect();

        if candidates.is_empty() {
            return;
        }

        let answers = futures::future::join_all(
            candidates
                .iter()
                .map(|session| ping_session_client(session, timeout)),
        )
        .await;

        for (session, answered) in candidates.iter().zip(answers) {
            if answered {
                session.record_liveness_pong();
                continue;
            }
            let missed = session.record_liveness_miss();
            warn!(
                session_id = %session.id,
                missed,
                max = Self::MAX_MISSED_LIVENESS_PINGS,
                "Liveness ping to real client went unanswered"
            );
            if missed >= Self::MAX_MISSED_LIVENESS_PINGS {
                warn!(
                    session_id = %session.id,
                    "Client unresponsive to {} consecutive liveness pings; terminating session",
                    missed
                );
                let _ = self
                    .terminate_session(&session.id, SessionTerminationReason::Unresponsive)
                    .await;
            }
        }
    }

    /// Returns true when this server requires client roots to complete sandbox lock.
    pub fn requires_client_roots(&self) -> bool {
        self.config.default_scope.is_none()
    }

    /// The configured request timeout in seconds for bridge → subprocess calls
    /// (see [`SessionManagerConfig::request_timeout_secs`]).
    pub fn request_timeout_secs(&self) -> u64 {
        self.config.request_timeout_secs
    }

    /// The configured default `tools/call` timeout in seconds
    /// (see [`SessionManagerConfig::tool_call_timeout_secs`]).
    pub fn tool_call_timeout_secs(&self) -> u64 {
        self.config.tool_call_timeout_secs
    }

    /// The bridge's configured default sandbox scope (from `--sandbox-scope`
    /// or the `~/sandbox` fallback), if any. Exposed via `/health` (SPEC R7)
    /// so a client deciding whether to reuse an already-running bridge can
    /// tell whether it is actually scoped to the project the client wants,
    /// instead of silently reusing a stale daemon pinned to a different
    /// (or fallback) directory.
    pub fn default_scope(&self) -> Option<&std::path::Path> {
        self.config.default_scope.as_deref()
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

    /// Minimum age a session must have reached, with no SSE receiver attached,
    /// before it may be evicted: a younger one may simply not have opened its
    /// stream yet.
    const MIN_IDLE_BEFORE_EVICTION: Duration = Duration::from_secs(5);

    /// Whether `session` is eligible for eviction — nobody is listening to it
    /// and it is old enough that nobody plausibly still will.
    fn is_evictable(session: &Session) -> bool {
        session.sse_receivers() == 0
            && session.created_at.elapsed() >= Self::MIN_IDLE_BEFORE_EVICTION
    }

    /// Evict the oldest session that has 0 active SSE receivers for at least 5s to accommodate a new session.
    pub async fn evict_oldest_inactive_session(&self) -> bool {
        // `min_by_key` returns the *first* of equally old candidates, which is
        // the tie-break the hand-rolled scan this replaced also had. The map
        // reference is dropped at the end of the statement, before
        // `terminate_session` reaches back into the map.
        let oldest = self
            .sessions
            .iter()
            .filter(|entry| Self::is_evictable(entry.value()))
            .min_by_key(|entry| entry.value().created_at)
            .map(|entry| entry.key().clone());

        let Some(evict_id) = oldest else {
            return false;
        };

        tracing::info!(
            session_id = %evict_id,
            "Evicting oldest inactive session (0 SSE receivers for >5s) to accommodate new session"
        );
        let _ = self
            .terminate_session(&evict_id, SessionTerminationReason::Timeout)
            .await;
        true
    }

    /// Make room for one more session under `max_sessions`: prune what is
    /// already dead, then evict the oldest unobserved session, and fail only
    /// when neither frees a slot.
    async fn ensure_session_capacity(&self) -> Result<()> {
        if self
            .draining
            .as_ref()
            .is_some_and(|d| d.load(std::sync::atomic::Ordering::SeqCst))
        {
            // Accepting here would start a session inside a process that is
            // leaving, so the client would lose it moments later.
            return Err(BridgeError::Draining);
        }
        if self.sessions.len() < self.config.max_sessions {
            return Ok(());
        }
        self.prune_stale_sessions().await;
        if self.sessions.len() < self.config.max_sessions {
            return Ok(());
        }
        if self.evict_oldest_inactive_session().await {
            return Ok(());
        }
        Err(BridgeError::SessionLimitExceeded {
            max: self.config.max_sessions,
        })
    }

    /// Open the peer's stdio streams — via the injected factory when there is
    /// one, otherwise via the default [`SubprocessPeerFactory`] built from the
    /// config fields. `PeerFactory::create` returns `anyhow::Result` (P6), so
    /// the failure is converted to [`BridgeError::ServerProcess`] here.
    async fn create_peer_streams(
        &self,
        options: crate::peer::PeerSpawnOptions,
    ) -> Result<PeerStreams> {
        let streams = match &self.config.peer_factory {
            Some(factory) => factory.create(options).await,
            None => {
                SubprocessPeerFactory::new(
                    self.config.server_command.clone(),
                    self.config.server_args.clone(),
                    self.config.enable_colored_output,
                )
                .with_default_sandbox_scope(self.config.default_scope.clone())
                .create(options)
                .await
            }
        };
        streams.map_err(|e| BridgeError::ServerProcess(e.to_string()))
    }

    /// Turn a session's query string into worker arguments, refusing an option
    /// this build does not know (SPEC R-DAEMON.4).
    ///
    /// The allowlist lives in `ahma_mcp`, which depends on this crate, so the
    /// daemon injects the translator rather than this crate reaching upwards.
    pub fn worker_args_for_query(&self, query: &str) -> std::result::Result<Vec<String>, String> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        match &self.config.session_options {
            Some(translate) => translate(query),
            // No translator installed (an explicitly started bridge): options
            // are a daemon feature, and silently dropping them would be worse
            // than saying so.
            None => Err(
                "this server does not accept per-session options; they are a feature of the \
                 ahma daemon"
                    .to_string(),
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
        self.create_session_with_args(Vec::new()).await
    }

    /// Create a session whose worker carries `worker_args` — the options this
    /// client asked for, and nobody else (SPEC R-DAEMON.4).
    pub async fn create_session_with_args(&self, worker_args: Vec<String>) -> Result<String> {
        self.ensure_session_capacity().await?;

        let session_id = Uuid::new_v4().to_string();
        info!(session_id = %session_id, "Creating new session");

        let PeerStreams {
            stdin,
            stdout,
            stderr,
            shutdown_fn,
            exit_cause,
        } = self
            .create_peer_streams(crate::peer::PeerSpawnOptions {
                session_id: session_id.clone(),
                extra_args: worker_args,
            })
            .await?;

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
            terminated_notify: Notify::new(),
            peer_shutdown: Mutex::new(shutdown_fn),
            exit_cause: Mutex::new(exit_cause),
            handshake_state: StateMachine::new(HandshakeState::AwaitingBoth),
            mcp_initialized_notify: Notify::new(),
            sandbox_state_machine: Arc::new(SandboxStateMachine::new()),
            created_at: Instant::now(),
            handshake_timeout,
            lagged_events: AtomicU64::new(0),
            event_id_counter: AtomicU64::new(0),
            event_history: parking_lot::Mutex::new(VecDeque::new()),
            client_info: Mutex::new(None),
            capabilities: Mutex::new(None),
            routed_requests: Arc::new(DashMap::new()),
            pending_client_requests: Arc::new(DashMap::new()),
            sampling_semaphore: Arc::new(tokio::sync::Semaphore::new(3)),
            missed_liveness_pings: AtomicU64::new(0),
            last_client_activity: parking_lot::Mutex::new(Instant::now()),
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
                    Err(BridgeError::Communication(format!(
                        "Client did not provide roots/list entries. \
                         {NO_SANDBOX_SCOPE_REMEDIATION}"
                    )))
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

    /// Look up a session that exists and has not been terminated.
    fn live_session(&self, session_id: &str) -> Result<Arc<Session>> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            BridgeError::Communication(format!("Session not found: {}", session_id))
        })?;

        if session.is_terminated() {
            return Err(BridgeError::Communication(
                "Session has been terminated".to_string(),
            ));
        }

        Ok(session.clone())
    }

    /// Send a message to a session's subprocess
    pub async fn send_message(&self, session_id: &str, message: &Value) -> Result<()> {
        let session = self.live_session(session_id)?;

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
        let session = self.live_session(session_id)?;

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

        await_response(
            response_rx,
            timeout,
            Duration::from_secs(self.request_timeout_secs()),
            &id_opt,
            &session.pending_requests,
        )
        .await
    }

    /// Lock sandbox scope for a session (called when observing first roots/list response).
    ///
    /// Per SPEC R5.1.1 / R5.2.2 / R10.3, sandbox scope is determined from the first
    /// roots/list response and cannot be changed. In the simplified design, the subprocess is spawned with
    /// `--defer-sandbox` and configures its own sandbox after roots are received.
    ///
    /// This method only records the scopes for bridge-side enforcement (e.g. rejecting
    /// roots changes after lock) and for debugging. It *stages* `Configuring` — it never
    /// transitions the session to `Active`: only the subprocess's
    /// `notifications/sandbox/configured` opens the `tools/call` gate (SPEC RB.2).
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

            session.set_terminated(true);

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
        let mut stdout_line_rx = spawn_stdout_line_forwarder(stdout);

        let mut stderr_reader = stderr.map(|s| BufReader::new(s).lines());

        loop {
            tokio::select! {
                // Handle outgoing messages (HTTP -> Stdio)
                Some(msg) = rx.recv() => {
                    debug!(session_id = %session.id, "Sending to subprocess: {}", msg);

                    // Echo STDIN in cyan if colored output is enabled
                    if colored_output {
                        echo_stdin_frame(&session.id, &msg);
                    }

                    if write_frame_to_peer(&mut *stdin, &session.id, &msg).await.is_err() {
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
                        Ok(Some(line)) if !line.is_empty() => echo_stderr_line(&session.id, &line),
                        // An empty line, or stderr closed: nothing to echo.
                        Ok(Some(_) | None) => {}
                        Err(e) => {
                            error!(session_id = %session.id, "Failed to read stderr: {}", e);
                        }
                    }
                }

                // Wake on termination. The `Notified` future is created before
                // the flag check so a `notify_waiters` racing this branch is
                // observed by the subsequent await rather than missed.
                _ = async {
                    loop {
                        let notified = session.terminated_notify.notified();
                        if session.terminated.load(Ordering::SeqCst) {
                            break;
                        }
                        notified.await;
                    }
                } => {
                    info!(session_id = %session.id, "Session terminated, stopping I/O handler");
                    break;
                }
            }
        }

        finish_session_io(&session).await;
    }
}

/// Spawn a dedicated stdout reader task and return the cancel-safe channel it
/// forwards lines through.
///
/// `Lines::next_line()` is NOT cancel-safe inside `tokio::select!` — when
/// another branch wins (e.g. stderr), the stdout future is cancelled and
/// partially-read data can be lost (causing `roots/list` to be silently
/// dropped). An mpsc channel receive IS cancel-safe, so lines are forwarded
/// through one here.
fn spawn_stdout_line_forwarder(
    stdout: Box<dyn AsyncRead + Send + Unpin + 'static>,
) -> mpsc::Receiver<std::io::Result<Option<String>>> {
    let (stdout_line_tx, stdout_line_rx) = mpsc::channel::<std::io::Result<Option<String>>>(64);
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
    stdout_line_rx
}

/// After `handle_session_io`'s select loop exits, mark the session terminated
/// and answer any still-pending requests with an explicit error.
///
/// This prevents "Response channel closed" errors that manifest as cryptic
/// "Canceled: canceled" messages in clients.
async fn finish_session_io(session: &Arc<Session>) {
    session.set_terminated(true);

    if session.pending_requests.is_empty() {
        return;
    }

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
        None => "Session terminated unexpectedly - subprocess may have crashed or handshake failed"
            .to_string(),
    };
    fail_pending_requests(&session.pending_requests, &session.id, &message);
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

    #[test]
    fn parse_configured_scopes_skips_non_string_entries() {
        // Pins the historical `filter_map(Value::as_str)` leniency: a
        // mixed-type write array keeps its strings rather than erroring.
        let notif = json!({
            "method": "notifications/sandbox/configured",
            "params": { "scope": { "write": ["/keep", 7, null, "/also"] } }
        });
        assert_eq!(
            parse_configured_scopes(&notif),
            vec![PathBuf::from("/keep"), PathBuf::from("/also")]
        );
    }

    #[test]
    fn parse_configured_scopes_non_object_params_is_empty() {
        // `params` present but not an object → treated as missing → empty.
        let notif = json!({
            "method": "notifications/sandbox/configured",
            "params": "garbage"
        });
        assert!(parse_configured_scopes(&notif).is_empty());
    }
}

#[cfg(test)]
mod session_logic_tests {
    use super::*;
    use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
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
            terminated_notify: Notify::new(),
            peer_shutdown: Mutex::new(None),
            exit_cause: Mutex::new(None),
            handshake_state: StateMachine::new(HandshakeState::AwaitingBoth),
            mcp_initialized_notify: Notify::new(),
            sandbox_state_machine: Arc::new(SandboxStateMachine::new()),
            created_at: Instant::now(),
            handshake_timeout,
            lagged_events: AtomicU64::new(0),
            event_id_counter: AtomicU64::new(0),
            event_history: parking_lot::Mutex::new(VecDeque::new()),
            client_info: Mutex::new(None),
            capabilities: Mutex::new(None),
            routed_requests: Arc::new(DashMap::new()),
            pending_client_requests: Arc::new(DashMap::new()),
            sampling_semaphore: Arc::new(tokio::sync::Semaphore::new(3)),
            missed_liveness_pings: AtomicU64::new(0),
            last_client_activity: parking_lot::Mutex::new(Instant::now()),
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
    async fn send_push_channel_changed_notifies_the_subprocess() {
        let (session, mut rx) = make_test_session();

        session.send_push_channel_changed(true).await.unwrap();
        let sent = rx
            .try_recv()
            .expect("pushChannelChanged(true) must be sent");
        let v: Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["method"], "notifications/ahma/pushChannelChanged");
        assert_eq!(v["params"]["connected"], true);

        session.send_push_channel_changed(false).await.unwrap();
        let sent = rx
            .try_recv()
            .expect("pushChannelChanged(false) must be sent");
        let v: Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["params"]["connected"], false);
    }

    // ── Liveness ping (bridge → real client) ──────────────────────────────

    #[tokio::test]
    async fn ping_session_client_true_when_client_answers() {
        let (session, _rx) = make_test_session();
        let mut sub = session.subscribe();

        let ping_session = Arc::clone(&session);
        let ping_timeout = TestTimeouts::get(TimeoutCategory::Quick);
        let handle =
            tokio::spawn(async move { ping_session_client(&ping_session, ping_timeout).await });

        let (_event_id, json_str) = sub.recv().await.unwrap();
        let ping: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(ping["method"], PING_METHOD);
        assert_eq!(ping["jsonrpc"], "2.0");
        let ping_id = ping["id"]
            .as_str()
            .expect("ping id is a string")
            .to_string();

        let (_, sender) = session.routed_requests.remove(&ping_id).expect(
            "ping id must be registered in routed_requests, exactly like a routed sampling request",
        );
        sender
            .send(serde_json::json!({"jsonrpc": "2.0", "id": ping_id, "result": {}}))
            .unwrap();

        assert!(handle.await.unwrap(), "an answered ping must return true");
    }

    #[tokio::test]
    async fn ping_session_client_false_when_client_never_answers() {
        let (session, _rx) = make_test_session();
        let _sub = session.subscribe(); // socket "open", nobody reads it

        let answered = ping_session_client(&session, Duration::from_millis(20)).await;

        assert!(!answered);
        assert!(
            session.routed_requests.is_empty(),
            "a timed-out ping must remove its own routed_requests entry"
        );
    }

    #[tokio::test]
    async fn ping_session_client_false_when_no_sse_subscriber() {
        let (session, _rx) = make_test_session();
        // No `subscribe()` call: broadcast has 0 receivers, so the send itself
        // fails and this must return false immediately, not wait out the timeout.
        let answered = ping_session_client(&session, Duration::from_millis(20)).await;
        assert!(!answered);
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

    // ── wait_for_mcp_initialized ─────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_mcp_initialized_returns_immediately_when_set() {
        let (session, _rx) = make_test_session();
        session.mark_mcp_initialized().await.unwrap();
        // Already initialized → the early return path (lines 291-293).
        tokio::time::timeout(
            TestTimeouts::scale_secs(1),
            session.wait_for_mcp_initialized(),
        )
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
        tokio::time::timeout(
            TestTimeouts::scale_secs(2),
            session.wait_for_mcp_initialized(),
        )
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
        // RootsRequested: both.
        session.mark_sse_connected().await.unwrap();
        session.mark_mcp_initialized().await.unwrap();
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

    /// `short_id` replaced three independent `&session_id[..8]` slices. That
    /// form panics on an id shorter than 8 bytes and on any id whose eighth
    /// byte falls inside a multi-byte character; ids are UUIDs today, so
    /// nothing exercised either case, and the echo path has no test seam of
    /// its own to notice.
    #[test]
    fn short_id_truncates_without_panicking_on_awkward_ids() {
        assert_eq!(short_id("0123456789abcdef"), "[01234567]");
        assert_eq!(short_id("abcdef01"), "[abcdef01]", "exactly eight");
        assert_eq!(short_id("short"), "[short]", "shorter than eight");
        assert_eq!(short_id(""), "[]");
        // Eight characters, sixteen bytes: the byte-index slice would have
        // split a character here and panicked.
        assert_eq!(short_id("ääääääääx"), "[ääääääää]");
    }

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

        let scope = std::env::temp_dir().join("locked_proj");
        session
            .sandbox_state_machine
            .transition_to_active_with_scopes(vec![scope.clone()])
            .unwrap();
        assert!(session.is_sandbox_locked());
        assert!(matches!(
            session.current_sandbox_state(),
            SandboxState::Active { .. }
        ));
    }

    #[test]
    fn get_sandbox_scope_and_scopes() {
        let (session, _rx) = make_test_session();
        assert!(session.get_sandbox_scope().is_none());
        assert!(session.get_sandbox_scopes().is_none());

        let a = std::env::temp_dir().join("a");
        let b = std::env::temp_dir().join("b");
        session
            .sandbox_state_machine
            .transition_to_configuring(vec![a.clone(), b.clone()])
            .unwrap();
        assert_eq!(session.get_sandbox_scope(), Some(a.clone()));
        assert_eq!(session.get_sandbox_scopes(), Some(vec![a, b]));
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
    fn handle_sandbox_failed_malformed_error_defaults_message() {
        // A non-string `params.error` must not reject the notification: the
        // Failed transition still happens, with the generic fallback message
        // (pins the leniency of the typed parse against the old `.as_str()`).
        let (session, _rx) = make_test_session();
        handle_sandbox_failed(
            &session,
            &json!({"method": "notifications/sandbox/failed", "params": {"error": 42}}),
        );
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

    /// REGRESSION (SPEC R2.6.5.3): the subprocess mints its own ids for the
    /// requests it sends (rmcp counts from 0), the client mints its own for the
    /// requests it sends, and the two counters are independent — so a
    /// subprocess *request* can carry the same id as a client request the
    /// bridge is still waiting on. Matching on the id alone consumed the
    /// subprocess's request as the pending call's response: the caller got a
    /// message with no `result`, and the request itself vanished. A message
    /// with a `method` is a request, never a response, whatever its id.
    #[test]
    fn dispatch_never_mistakes_a_subprocess_request_for_a_response_with_the_same_id() {
        let (session, _rx) = make_test_session();
        let mut sub = session.subscribe();
        let (tx, mut resp_rx) = oneshot::channel();
        session.pending_requests.insert("5".to_string(), tx);

        dispatch_subprocess_line(
            &session,
            &json!({"jsonrpc": "2.0", "id": 5, "method": "elicitation/create", "params": {}})
                .to_string(),
            false,
        );

        assert!(
            session.pending_requests.contains_key("5"),
            "the client's pending call must survive a subprocess request with the same id"
        );
        assert!(
            resp_rx.try_recv().is_err(),
            "a request must never be delivered as the response to a pending call"
        );
        assert_eq!(
            session
                .pending_client_requests
                .get("5")
                .map(|m| m.value().clone()),
            Some("elicitation/create".to_string()),
            "the request is a server-to-client request and must be tracked as one"
        );
        let (_, msg) = sub
            .try_recv()
            .expect("the request must reach the client over SSE");
        assert!(msg.contains("elicitation/create"));
    }

    /// SPEC R2.6.5.3: the subprocess's mid-`await` liveness `ping` is answered
    /// by the bridge on the client's behalf **iff** the session holds a live
    /// push channel. Forwarding it to the client cannot work through the
    /// stdio proxy: rmcp's streamable-HTTP client awaits each POST inline, so
    /// while the `await` request is in flight nothing the bridge pushes is
    /// relayed, and the probe times out against a perfectly healthy client.
    /// The live SSE stream is the signal the probe was meant to verify.
    #[test]
    fn dispatch_answers_a_subprocess_ping_while_a_push_channel_is_open() {
        let (session, mut rx) = make_test_session();
        let mut sub = session.subscribe();
        // The pending-call collision from the test above, on the ping path: the
        // answer must go back even when the ping's id shadows a pending call.
        let (tx, mut resp_rx) = oneshot::channel();
        session.pending_requests.insert("9".to_string(), tx);

        dispatch_subprocess_line(
            &session,
            &json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}).to_string(),
            false,
        );

        let answer = rx
            .try_recv()
            .expect("the ping must be answered to the subprocess");
        let answer: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["id"], 9);
        assert!(
            answer.get("result").is_some() && answer.get("error").is_none(),
            "a live push channel means the client is reachable: {answer}"
        );
        assert!(
            sub.try_recv().is_err(),
            "an answered ping is not also forwarded — the client would answer a \
             second time into a request nobody is waiting on"
        );
        assert!(session.pending_requests.contains_key("9"));
        assert!(resp_rx.try_recv().is_err());
    }

    /// The complement: with no SSE subscriber there is no live channel, so the
    /// probe must be left to time out — that is the "client gone" verdict the
    /// subprocess's `await` ends the wait on. Answering here would report a
    /// vanished client as alive.
    #[test]
    fn dispatch_leaves_a_subprocess_ping_unanswered_with_no_push_channel() {
        let (session, mut rx) = make_test_session();
        assert_eq!(session.broadcast_tx.receiver_count(), 0);

        dispatch_subprocess_line(
            &session,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}).to_string(),
            false,
        );

        assert!(
            rx.try_recv().is_err(),
            "no push channel: the ping must not be answered on the client's behalf"
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
        let res = await_response(None, None, Duration::from_secs(60), &None, &pending)
            .await
            .unwrap();
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
            Duration::from_secs(60),
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
        let err = await_response(
            Some(rx),
            Some(Duration::from_secs(1)),
            Duration::from_secs(60),
            &None,
            &pending,
        )
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
            Duration::from_secs(60),
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

    // request_timeout_secs()/tool_call_timeout_secs() are now plain
    // SessionManagerConfig fields (SPEC R-CFG1.2: AHMA_* env vars are
    // retired) — see SessionManagerConfig's own construction tests for
    // coverage of the default/override values.

    // ── SessionManager: config-only / in-memory peer logic ───────────────────

    /// A `PeerFactory` backed by in-memory duplex pipes. It retains the peer
    /// ends so the bridge's stdout reader does not see EOF (which would mark the
    /// session terminated) and stdin writes never block.
    struct DuplexPeerFactory {
        peer_ends: parking_lot::Mutex<Vec<(tokio::io::DuplexStream, tokio::io::DuplexStream)>>,
    }

    impl DuplexPeerFactory {
        fn new() -> Self {
            Self {
                peer_ends: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    impl PeerFactory for DuplexPeerFactory {
        fn create(
            &self,
            _options: crate::peer::PeerSpawnOptions,
        ) -> crate::peer::BoxFuture<anyhow::Result<PeerStreams>> {
            let (bridge_stdin, peer_reader) = tokio::io::duplex(8192);
            let (peer_writer, bridge_stdout) = tokio::io::duplex(8192);
            self.peer_ends.lock().push((peer_reader, peer_writer));
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
            default_scope,
            max_sessions,
            peer_factory: Some(Arc::new(DuplexPeerFactory::new())),
            ..Default::default()
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

    // ── Liveness ping (bridge → real client) ──────────────────────────────

    #[tokio::test]
    async fn ping_connected_sessions_skips_a_session_with_no_sse_subscriber() {
        // No `subscribe()` call: sse_receivers() == 0, so the session is the
        // handshake/eviction sweeper's problem, not the liveness prober's —
        // pinging it would just always miss and wrongly count against it.
        let mgr = SessionManager::new(test_config(None, 8));
        let id = mgr.create_session().await.unwrap();

        mgr.ping_connected_sessions(Duration::from_millis(20), Duration::ZERO)
            .await;

        assert!(mgr.session_exists(&id));
        let session = mgr.get_session(&id).unwrap();
        assert_eq!(session.missed_liveness_pings.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn ping_connected_sessions_skips_a_session_with_recent_activity() {
        // A session that just had real client traffic must not be pinged at
        // all, regardless of whether it would ever answer a ping — many
        // legitimate clients never implement the server→client `ping` side of
        // the protocol, and this session is already known to be alive.
        let mgr = SessionManager::new(test_config(None, 8));
        let id = mgr.create_session().await.unwrap();
        let session = mgr.get_session(&id).unwrap();
        let _sub = session.subscribe(); // never answers anything
        session.touch_client_activity();

        mgr.ping_connected_sessions(
            Duration::from_millis(20),
            TestTimeouts::get(TimeoutCategory::Quick),
        )
        .await;

        assert!(mgr.session_exists(&id));
        assert_eq!(session.missed_liveness_pings.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn ping_connected_sessions_terminates_after_max_missed_pings() {
        let mgr = SessionManager::new(test_config(None, 8));
        let id = mgr.create_session().await.unwrap();
        let session = mgr.get_session(&id).unwrap();
        // A subscriber that never answers: the SSE socket looks open, but
        // nothing is actually reading it — exactly the gap this probe closes.
        let _sub = session.subscribe();

        let short_timeout = Duration::from_millis(20);
        for round in 1..=SessionManager::MAX_MISSED_LIVENESS_PINGS {
            mgr.ping_connected_sessions(short_timeout, Duration::ZERO)
                .await;
            if round < SessionManager::MAX_MISSED_LIVENESS_PINGS {
                assert!(
                    mgr.session_exists(&id),
                    "session must survive before the max is reached"
                );
            }
        }

        assert!(
            !mgr.session_exists(&id),
            "session must be terminated once its client misses \
             MAX_MISSED_LIVENESS_PINGS in a row"
        );
    }

    #[tokio::test]
    async fn ping_connected_sessions_resets_miss_count_when_client_answers() {
        let mgr = SessionManager::new(test_config(None, 8));
        let id = mgr.create_session().await.unwrap();
        let session = mgr.get_session(&id).unwrap();
        let mut sub = session.subscribe();

        let short_timeout = TestTimeouts::get(TimeoutCategory::Quick);
        // Run the ping round and the client's answer concurrently *without*
        // moving `sub` into a spawned task — a spawned task would drop the
        // receiver as soon as it finished, which would drop `sse_receivers()`
        // to 0 and make the second round below silently skip the session
        // instead of exercising the "missed" path it is meant to test.
        let respond_once = async {
            let (_event_id, json_str) = sub.recv().await.unwrap();
            let ping: Value = serde_json::from_str(&json_str).unwrap();
            assert_eq!(ping["method"], PING_METHOD);
            let ping_id = ping["id"].as_str().unwrap().to_string();
            let (_, sender) = session
                .routed_requests
                .remove(&ping_id)
                .expect("ping id must be registered in routed_requests");
            sender
                .send(serde_json::json!({"jsonrpc": "2.0", "id": ping_id, "result": {}}))
                .unwrap();
        };
        tokio::join!(
            mgr.ping_connected_sessions(short_timeout, Duration::ZERO),
            respond_once
        );
        assert_eq!(session.missed_liveness_pings.load(Ordering::Relaxed), 0);

        // One missed round after an answered one must not carry over any
        // prior misses — it takes MAX_MISSED_LIVENESS_PINGS in a row, not
        // cumulative misses, to terminate the session. `sub` is still held
        // here (still subscribed) but nobody drains/answers it this time.
        mgr.ping_connected_sessions(Duration::from_millis(20), Duration::ZERO)
            .await;
        assert!(mgr.session_exists(&id));
        assert_eq!(session.missed_liveness_pings.load(Ordering::Relaxed), 1);
    }

    /// A peer that stays alive until the test drops its retained end, and whose
    /// `exit_cause` channel is handed back so the test can play the role of the
    /// exit monitor.
    struct DyingPeerFactory {
        peer_end: parking_lot::Mutex<Option<tokio::io::DuplexStream>>,
        cause_tx: parking_lot::Mutex<Option<oneshot::Sender<String>>>,
    }

    impl PeerFactory for DyingPeerFactory {
        fn create(
            &self,
            _options: crate::peer::PeerSpawnOptions,
        ) -> crate::peer::BoxFuture<anyhow::Result<PeerStreams>> {
            let (bridge_end, peer_end) = tokio::io::duplex(8192);
            *self.peer_end.lock() = Some(peer_end);
            let (cause_tx, cause_rx) = oneshot::channel();
            *self.cause_tx.lock() = Some(cause_tx);
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
            peer_end: parking_lot::Mutex::new(None),
            cause_tx: parking_lot::Mutex::new(None),
        });
        let config = SessionManagerConfig {
            server_command: "unused".to_string(),
            max_sessions: 8,
            peer_factory: Some(factory.clone()),
            ..Default::default()
        };
        let mgr = SessionManager::new(config);
        let id = mgr.create_session().await.unwrap();
        let session = mgr.get_session(&id).unwrap();

        // A request is in flight when the peer dies.
        let (tx, rx) = oneshot::channel();
        session.pending_requests.insert("42".to_string(), tx);

        // Play the exit monitor: classify the death, THEN let the bridge see
        // the pipe EOF (the order the race can also produce in production).
        let cause_tx = factory.cause_tx.lock().take().unwrap();
        cause_tx
            .send("killed by SIGKILL (possible OOM-kill or code-signing kill).".to_string())
            .unwrap();
        drop(factory.peer_end.lock().take());

        let response = tokio::time::timeout(TestTimeouts::scale_secs(5), rx)
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
            max_sessions: 1,
            ..Default::default()
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
        assert!(HandshakeState::RootsRequested.is_terminal());
        assert!(!HandshakeState::AwaitingBoth.is_terminal());
    }
}
