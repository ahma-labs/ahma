//! Ahma Hub Daemon – lightweight IPC hub for multi-instance aggregation.
//!
//! The hub daemon is a tiny user-space process that:
//! * Accepts connections from all running ahma instances (stdio, http, unix).
//! * Accepts subscription connections from TUI clients.
//! * Fans out operation events to all subscribers in real time.
//!
//! ## Transport
//!
//! | Platform        | Transport                                |
//! |-----------------|------------------------------------------|
//! | Unix / macOS    | Unix domain socket (`~/.ahma/daemon.sock`) |
//! | Windows         | TCP loopback `127.0.0.1:7395`            |
//!
//! Override the path/address with the `--daemon-socket` CLI flag or the `AHMA_DAEMON_SOCK`
//! internal environment variable (set only by `init_test_daemon_isolation()` in test builds).
//!
//! ## Protocol
//!
//! All messages are newline-delimited JSON (NDJ).  Each line is one serialised
//! [`ClientMsg`] (instance → daemon or subscriber → daemon) or [`DaemonMsg`]
//! (daemon → subscriber).
//!
//! ## Startup / race-condition handling
//!
//! The bind-is-the-mutex approach avoids lock files entirely:
//!
//! **Instance side** (`ensure_daemon_running`):
//! 1. Try `connect()` → success → done, use existing daemon.
//! 2. Spawn `ahma daemon` as a detached child.
//! 3. Poll `connect()` every 50 ms × 20 attempts (~1 s).
//!
//! **Daemon startup** (`run_daemon`):
//! 1. Try `bind()` → `Ok` → start serving (won the race).
//! 2. `EADDRINUSE` → try `connect()` → `Ok` → `exit(0)` (another daemon won).
//! 3. `EADDRINUSE` + `ECONNREFUSED` → unlink stale socket file (ENOENT benign)
//!    → go back to step 1.
//!
//! **Idle exit**: the daemon resets a 60-second timer on every new connection.
//! When the timer fires and the active connection count is zero it unlinks the
//! socket file *first* (so late arrivals get ENOENT, not ECONNREFUSED) and then
//! exits cleanly.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, broadcast},
};
use tracing::{debug, info, warn};

// ─── Windows constant ────────────────────────────────────────────────────────

/// TCP port used on Windows (unix sockets not supported there).
pub const WINDOWS_DAEMON_PORT: u16 = 7395;

/// Get the daemon port.
///
/// In test builds, `AHMA_DAEMON_PORT` can be set by `init_test_daemon_isolation()` to
/// isolate concurrent test processes. In production this always returns `WINDOWS_DAEMON_PORT`
/// unless `set_socket_path_override` was used (the Windows equivalent of `--daemon-socket`).
pub fn daemon_port() -> u16 {
    if let Some(p) = std::env::var("AHMA_DAEMON_PORT")
        .ok()
        .and_then(|p_str| p_str.parse::<u16>().ok())
    {
        return p;
    }
    // Safety net (SPEC R-ISO.1): a test-spawned process that did not go through
    // `init_test_daemon_isolation` must still not reach the live daemon port.
    // Derive a stable per-run port so every process in the test run agrees.
    if crate::test_isolation::spawned_under_test_harness() {
        let disc = crate::test_isolation::test_run_discriminator();
        let hash: u32 = disc.bytes().fold(0u32, |acc, b| {
            acc.wrapping_mul(31).wrapping_add(u32::from(b))
        });
        return 49152 + (hash % 16000) as u16;
    }
    WINDOWS_DAEMON_PORT
}

/// Interval at which the hub sends liveness pings to connected instances.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Upper bound on retained per-instance operation snapshots. Bounds the memory
/// the hub spends remembering history for replay to late-joining subscribers;
/// once exceeded the oldest *finished* op is dropped (running ops are kept).
const MAX_OPS_PER_INSTANCE: usize = 500;

/// Bounded output lines retained per operation, replayed to a late subscriber
/// so a TUI opened mid-build shows what the command has been printing rather
/// than an empty pane. Matches `ahma_mcp::operation_monitor::MAX_TAIL_LINES`,
/// which is the window the producing side keeps.
pub const MAX_TAIL_LINES: usize = 100;

/// How long a finished operation — and an instance that has since
/// disconnected — stays replayable (SPEC R-DAEMON.7). One hour is what a
/// developer means by "what just happened".
pub const HISTORY_REPLAY_WINDOW: Duration = Duration::from_secs(3600);

/// Ceiling on retained operations across every instance, live or ended. The
/// per-instance cap alone is unbounded in the number of instances, and a hook
/// registers one instance per hooked command.
const MAX_RETAINED_OPS: usize = 2000;

// ─────────────────────────────────────────────────────────────────────────────
// Protocol types
// ─────────────────────────────────────────────────────────────────────────────

/// A chat message sent over the daemon hub protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonChatMessage {
    pub role: String,
    pub content: String,
}

/// A sandbox denial attached to a finished operation (SPEC R-PERM.7).
///
/// Carries what the kernel refused, so every surface can say *which path* was
/// denied rather than showing a bare "Failed", and so the TUI can offer to
/// re-raise the grant question for exactly that `(path, access)` pair
/// (R-PERM.7.1) without re-parsing an error string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpDenial {
    /// The path the denial referenced, as the scanner found it.
    pub path: String,
    /// The access the denied operation needed. Typed rather than a `"ro"`/`"rw"`
    /// string: this is a permission decision, and the string form was decoded
    /// with a `_ => Ro` catch-all that silently downgraded a refused *write* to
    /// read-only. `ScopeAccess` already serialises as exactly `"ro"`/`"rw"`
    /// (`#[serde(rename_all = "lowercase")]`), so the wire form is unchanged.
    pub access: crate::config::ScopeAccess,
}

/// The terminal (or in-flight) state of an operation, as it travels the hub wire.
///
/// Serde's default unit-variant encoding makes each variant name its own wire
/// string — `"Completed"`, `"TimedOut"`, … — which is exactly what the previous
/// `status: String` field carried, so this is a type change and not a wire
/// change. Typing it means a new state is a compile error at every consumer
/// instead of falling into a `_` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

/// Metadata about a registered ahma instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// Random UUID assigned by the daemon at registration time.
    pub id: String,
    pub pid: u32,
    /// "stdio", "http", or "unix"
    pub mode: String,
    /// Sandbox scope / workspace root, e.g. `/home/user/project`.
    pub scope: String,
    /// Human-readable label, e.g. `"VS Code"` or `"Cursor"`.
    pub label: String,
    /// MCP client identity detected from the `initialize` handshake
    /// (`clientInfo.name`, e.g. `"claude-code"` or `"cursor"`). `None` until a
    /// client has attached or when the instance predates this field.
    #[serde(default)]
    pub client: Option<String>,
    /// The MCP session this instance serves (the bridge's `Mcp-Session-Id`).
    ///
    /// Three Claude Code windows open on one repository register with the same
    /// pid-less identity — same `client`, same `label`, same `scope` — so
    /// without this they are indistinguishable, and the daemon-minted `id`
    /// changes on every reconnect-to-relabel, which reshuffles them in any view
    /// that sorts by it. The session id is stable for the life of the session,
    /// so the hub keys an instance's identity off it.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Pid of the client-facing frontend process (the one the editor spawned),
    /// as opposed to `pid`, which is the worker that executes the tools.
    #[serde(default)]
    pub client_pid: Option<u32>,
    /// When this instance disconnected (Unix epoch, milliseconds), for an
    /// instance retained only so its recent operations still have somewhere to
    /// belong. `None` for a live instance.
    #[serde(default)]
    pub ended_epoch_ms: Option<u64>,
}

/// An operation event forwarded from an instance to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    OpStarted {
        id: String,
        tool_name: String,
        description: String,
        scope: String,
        /// Operation (or synthetic group, e.g. `session:<id>`) that spawned this
        /// one; `None` for top-level operations. Drives the TUI task tree.
        #[serde(default)]
        parent_id: Option<String>,
        /// Wall-clock start time (Unix epoch, milliseconds). Present so a
        /// subscriber joining late — or receiving a replay — shows the true
        /// elapsed time instead of measuring from receipt.
        #[serde(default)]
        started_epoch_ms: Option<u64>,
        /// The human title of this operation, computed **server-side** where the
        /// command is actually known (SPEC R24.7,
        /// [`crate::op_identity::title_for`]).
        ///
        /// This exists because observers used to reverse-engineer a name from the
        /// operation *id* (`op_41_echo_hello`), or fall back to the bare tool
        /// name, which made every row in a TUI look alike. No formatter can repair
        /// data that was never sent — so we send it.
        #[serde(default)]
        title: Option<String>,
        /// Working directory the operation ran in.
        #[serde(default)]
        cwd: Option<String>,
        /// The full command, for the detail pane (`title` is the one-line form).
        #[serde(default)]
        command: Option<String>,
        /// Which attached session initiated the work — `cursor`, `claude-code`,
        /// `tui`, `cli`, `hook`. What lets one timeline interleave IDE work and the
        /// user's own TUI commands and stay readable.
        #[serde(default)]
        origin: Option<String>,
        /// True when this start record was **reconstructed** from the
        /// operation's terminal event, because the original was never seen or
        /// had aged out — a hook whose command outlived a daemon restart, say.
        /// The outcome is true; the preamble (tool, command, working directory)
        /// is genuinely unknown, and a reader must say so rather than render
        /// blanks as fact.
        #[serde(default)]
        partial: bool,
        /// The operation ran **outside** the kernel sandbox, at the user's full
        /// privilege. Today that is only the TUI's human-typed `!` escape
        /// (SPEC R-DAEMON.9), and a unified view that drew it like any other
        /// row would be hiding the one thing about it worth knowing.
        ///
        /// Absent means confined: a producer that predates this field had no
        /// unsandboxed path to report.
        #[serde(default)]
        unsandboxed: bool,
    },
    OpFinished {
        id: String,
        /// The terminal state the operation reached. Typed so producer and
        /// consumer cannot drift: the variant names *are* the wire strings
        /// (serde's default for a unit enum), so this is the same JSON the
        /// hand-written `status_label`/`parse_op_status` pair produced.
        status: OpStatus,
        result_summary: Option<String>,
        duration_ms: u64,
        /// Wall-clock completion time (Unix epoch, milliseconds), for accurate
        /// historic views after replay.
        #[serde(default)]
        ended_epoch_ms: Option<u64>,
        /// Process exit code, when the operation was a process and the runner
        /// reported one. "Failed" without an exit code is not actionable; `exit
        /// 101` is. `None` for cancellations, timeouts, and non-process work, where
        /// the surface says the status word rather than inventing a code.
        #[serde(default)]
        exit_code: Option<i64>,
        /// Set when the failure was a **sandbox denial** rather than an ordinary
        /// error: the path (and access) the kernel refused. A denial is a
        /// first-class, visible event, not an error string (SPEC R-PERM.7) — it
        /// is what lets the TUI render `denied: <path>` and offer to re-raise the
        /// grant question for that path.
        ///
        /// `status` deliberately stays `"Failed"` so pre-upgrade readers still
        /// see a failure (R24.5: the wire evolves by adding fields only).
        #[serde(default)]
        denial: Option<OpDenial>,
        /// The operation did not finish so much as stop being observed: it was
        /// still running when the daemon that was watching it went away, and
        /// this record was reconstructed from the history file at the next
        /// start. Its exit is genuinely unknown — the command may well have
        /// completed — so a reader must say "interrupted", not "failed".
        /// `status` stays `Failed` for readers that predate this field.
        #[serde(default)]
        interrupted: bool,
    },
    /// A single line of live output from a running operation.
    /// Streamed as the child process produces it, so subscribers (TUI) can
    /// render output in real time instead of waiting for completion.
    OpOutput {
        id: String,
        line: String,
        is_stderr: bool,
    },
    LogLine {
        level: String,
        message: String,
    },
}

/// The messages the hub forwards verbatim between an instance and the TUIs
/// watching it.
///
/// These used to be written out twice — once in [`ClientMsg`] for the leg into
/// the hub, once in [`DaemonMsg`] for the leg out — with a hand-written arm in
/// the hub copying each one across. Two declarations of the same payload drift,
/// and the copying arm is where the drift shows up: a variant added to one side
/// and not the other compiles fine and is silently dropped at run time.
///
/// Naming the set once removes the possibility. The hub's forwarding is now
/// `ClientMsg::Relay(r) => DaemonMsg::Relay(r)`, so a message added here is
/// carried in both directions without the hub being touched at all.
///
/// **The wire is unchanged.** `#[serde(untagged)]` on the `Relay` variant of the
/// two outer enums means these serialize exactly as they did when they were flat
/// variants — `{"type":"ChatToken","token":"…"}`, not a nested envelope. That is
/// a requirement, not a nicety: the hub socket has no protocol version (R24.5
/// evolves it by adding fields), so a daemon left running across an upgrade must
/// keep understanding a newer instance. `relay_wire_compat` pins the bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HubRelay {
    /// Stream a chat token from the instance to the TUI.
    ChatToken { token: String },
    /// Stream a reasoning/"thinking" token (rendered in lower contrast by the TUI).
    ChatThinking { token: String },
    /// Ask the TUI for approval to execute a tool or elevate.
    ApprovalRequested {
        id: String,
        tool: String,
        args: String,
    },
    /// Raise a "grant access to X?" prompt for an auto-detected out-of-scope
    /// path. Mirrors [`Self::ApprovalRequested`] but the decision is three-valued
    /// and the grant is persisted for the next start, never applied to the live
    /// session (SPEC R5.4.7). The default/Enter choice must be the safe Deny
    /// (SPEC R5.3.1).
    ScopeGrantRequested {
        request: crate::scope_grant::ScopeGrantRequest,
    },
    /// Raise an "allow web access to X?" prompt for an unknown domain under a
    /// `deny` web policy (SPEC R-WEB.6). The parallel of
    /// [`Self::ScopeGrantRequested`] for network egress; an approval takes effect
    /// for the session (or is persisted) rather than for a filesystem scope, and
    /// never retroactively for the request that raised it.
    WebApprovalRequested {
        request: crate::web_approval::WebApprovalRequest,
    },
    /// A tool-call start, so the TUI can show which tool is running.
    ToolCallStarted {
        id: String,
        name: String,
        args: String,
    },
    /// A tool-call result.
    ToolCallFinished {
        id: String,
        result: String,
        failed: bool,
    },
    /// Token usage for the latest model turn, so the TUI counter updates.
    /// Carried as plain fields to keep this crate free of an LLM-client dep.
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        total_tokens: u32,
    },
    /// The agent turn is done.
    AgentDone,
    /// The agent turn encountered an error.
    AgentError { error: String },
}

impl From<HubRelay> for ClientMsg {
    fn from(relay: HubRelay) -> Self {
        ClientMsg::Relay(relay)
    }
}

impl From<HubRelay> for DaemonMsg {
    fn from(relay: HubRelay) -> Self {
        DaemonMsg::Relay(relay)
    }
}

/// Message from any client (instance or TUI subscriber) to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    /// An ahma instance announcing itself.  Sent once immediately after connecting.
    /// When the MCP client identity becomes known after registration (the
    /// `initialize` handshake happens later), the instance reconnects and
    /// re-registers with `client` set — field-only protocol evolution keeps
    /// mixed-version daemons working.
    Register {
        pid: u32,
        mode: String,
        scope: String,
        label: String,
        /// MCP client identity (`clientInfo.name`), when already known.
        #[serde(default)]
        client: Option<String>,
        /// The MCP session this instance serves. Re-registering with the same
        /// value re-uses the instance id the hub already assigned, so a
        /// reconnect-to-relabel does not look like one instance leaving and a
        /// different one arriving.
        #[serde(default)]
        session_id: Option<String>,
        /// Pid of the client-facing frontend process.
        #[serde(default)]
        client_pid: Option<u32>,
    },
    /// An operation event from a registered instance.
    Event { payload: DaemonEvent },
    /// A TUI/subscriber requesting the live event stream.
    Subscribe,
    /// A TUI/subscriber requesting a one-shot snapshot of current instances.
    ListInstances,
    /// An instance gracefully unregistering (optional — EOF works too).
    Unregister,
    /// Liveness response to a hub [`DaemonMsg::Ping`].
    Pong { seq: u32 },
    /// Ask the daemon to shut down and exit immediately.
    Shutdown,
    /// Submit a user prompt to start/resume an agent loop.
    SubmitPrompt {
        messages: Vec<DaemonChatMessage>,
        system_prompt: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        target_instance_id: Option<String>,
    },
    /// TUI client response containing user's approval decision.
    SubmitApproval {
        #[serde(default)]
        id: Option<String>,
        approved: bool,
        target_instance_id: Option<String>,
    },
    /// A TUI's three-valued answer to a scope-grant prompt, routed back to the
    /// instance that raised it.
    SubmitScopeGrant {
        decision_id: String,
        decision: crate::scope_grant::GrantDecision,
        target_instance_id: Option<String>,
    },
    /// An instance announcing a scope-grant decision is resolved, so the hub can
    /// dismiss the prompt on any other TUI showing the same `decision_id`.
    ScopeGrantResolved { decision_id: String },
    /// A TUI asking an instance to raise the grant question again for a path it
    /// already refused this session (SPEC R-PERM.7.1). Sent when the user picks
    /// a denied operation and confirms — an explicit human action, which is why
    /// it is allowed past the ask-once memo that suppresses automatic re-asks.
    ReRaiseScopeGrant {
        /// The denied path, as carried on the operation that was refused.
        path: String,
        /// The access the denied operation needed.
        access: crate::config::ScopeAccess,
        target_instance_id: Option<String>,
    },
    /// A TUI's answer to a web-approval prompt, routed back to the instance that
    /// raised it.
    SubmitWebApproval {
        decision_id: String,
        decision: crate::web_approval::WebApprovalDecision,
        target_instance_id: Option<String>,
    },
    /// An instance announcing a web-approval decision is resolved, so the hub can
    /// dismiss the prompt on any other TUI showing the same `decision_id`.
    WebApprovalResolved { decision_id: String },
    /// A message the hub forwards to every subscriber untouched. Flattened onto
    /// the wire, so each [`HubRelay`] variant is its own `"type"` exactly as when
    /// these were written out here one by one.
    #[serde(untagged)]
    Relay(HubRelay),
}

/// Message from the daemon to a subscriber (TUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonMsg {
    /// Sent once in response to `Subscribe` or `ListInstances`.
    InstanceList { instances: Vec<InstanceInfo> },
    /// Broadcast whenever a new instance registers.
    InstanceRegistered { instance: InstanceInfo },
    /// Broadcast whenever an instance disconnects or sends `Unregister`.
    InstanceUnregistered { id: String },
    /// An event forwarded from a registered instance.
    Event {
        instance_id: String,
        payload: DaemonEvent,
    },
    /// Liveness probe sent from hub to a connected instance.
    /// The instance should respond with a matching [`ClientMsg::Pong`].
    Ping { seq: u32 },
    /// Forward prompt run command to registered instance.
    RunPrompt {
        messages: Vec<DaemonChatMessage>,
        system_prompt: Option<String>,
        provider: Option<String>,
        model: Option<String>,
    },
    /// Forward user approval to registered instance.
    SubmitApproval {
        #[serde(default)]
        id: Option<String>,
        approved: bool,
    },
    /// Forward a TUI's scope-grant decision to the registered instance that raised
    /// it, where it is resolved and (if approved) persisted.
    SubmitScopeGrant {
        decision_id: String,
        decision: crate::scope_grant::GrantDecision,
    },
    /// Tell every TUI to dismiss the scope-grant modal for `decision_id` (a twin
    /// surface answered, or the instance withdrew it).
    ScopeGrantDismiss { decision_id: String },
    /// Ask this instance to re-raise the grant question for a path it already
    /// refused this session, because the user explicitly asked for it from a
    /// denied operation row (SPEC R-PERM.7.1).
    ReRaiseScopeGrant {
        path: String,
        access: crate::config::ScopeAccess,
    },
    /// Forward a TUI's web-approval decision to the registered instance that raised
    /// it, where it is resolved and (if approved) applied/persisted.
    SubmitWebApproval {
        decision_id: String,
        decision: crate::web_approval::WebApprovalDecision,
    },
    /// Tell every TUI to dismiss the web-approval modal for `decision_id` (a twin
    /// surface answered, or the instance withdrew it).
    WebApprovalDismiss { decision_id: String },
    /// A message forwarded from the instance to every subscriber untouched.
    /// Flattened onto the wire, so each [`HubRelay`] variant is its own `"type"`
    /// exactly as when these were written out here one by one.
    #[serde(untagged)]
    Relay(HubRelay),
}

// ─────────────────────────────────────────────────────────────────────────────
// Socket path
// ─────────────────────────────────────────────────────────────────────────────

/// Process-wide socket path override set from the `--daemon-socket` CLI flag.
static SOCKET_PATH_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set the daemon socket path from the `--daemon-socket` CLI flag.
/// Call once, early in startup. Takes precedence over `AHMA_DAEMON_SOCK`.
pub fn set_socket_path_override(path: PathBuf) {
    let _ = SOCKET_PATH_OVERRIDE.set(path);
}

#[cfg(test)]
static DAEMON_ISOLATION_INIT: std::sync::Once = std::sync::Once::new();
#[cfg(test)]
static DAEMON_SOCK_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Automatically configures environment variables to isolate the ahma daemon
/// socket (Unix) and port (Windows) for the current test process.
///
/// Call this at the top of any test that exercises the daemon. It is idempotent
/// (guarded by a `Once`). Only compiled into test builds.
#[cfg(test)]
pub fn init_test_daemon_isolation() {
    DAEMON_ISOLATION_INIT.call_once(|| {
        isolate_test_unix_socket();
        isolate_test_windows_port();
    });
}

#[cfg(test)]
fn isolate_test_unix_socket() {
    if std::env::var_os("AHMA_DAEMON_SOCK").is_none() {
        let pid = std::process::id();
        let count = DAEMON_SOCK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket_name = format!("ah_t_{}_{}.sock", pid, count);
        let temp_dir = std::env::temp_dir();
        let socket_path = temp_dir.join(socket_name);
        unsafe {
            std::env::set_var("AHMA_DAEMON_SOCK", socket_path);
        }
    }
}

#[cfg(test)]
fn isolate_test_windows_port() {
    if std::env::var_os("AHMA_DAEMON_PORT").is_none() {
        let bind_res = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr());
        if let Ok(addr) = bind_res {
            unsafe {
                std::env::set_var("AHMA_DAEMON_PORT", addr.port().to_string());
            }
        }
    }
}

/// Return the platform-default socket path for the hub daemon.
///
/// Resolution order:
/// 1. `--daemon-socket` CLI flag (`set_socket_path_override`).
/// 2. `AHMA_DAEMON_SOCK` env var — accepted for backward compat and test isolation;
///    emits a deprecation warning in production if not set by `init_test_daemon_isolation`.
/// 3. Platform default (`$XDG_RUNTIME_DIR/ahma/daemon.sock` on Linux,
///    `~/.ahma/daemon.sock` on macOS, unused on Windows).
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = SOCKET_PATH_OVERRIDE.get() {
        return p.clone();
    }
    if let Ok(v) = std::env::var("AHMA_DAEMON_SOCK") {
        // Allow the var in test builds without a warning; in production builds
        // it is only valid when set by the CLI flag path (via set_socket_path_override)
        // or by test isolation. Direct user configuration should use --daemon-socket.
        #[cfg(not(test))]
        warn!(
            "Deprecated: AHMA_DAEMON_SOCK is set but IGNORED for production config. \
             Use the --daemon-socket flag instead."
        );
        return PathBuf::from(v);
    }

    // Safety net (SPEC R-ISO.1): a test-spawned process that did not go through
    // `init_test_daemon_isolation` (which sets AHMA_DAEMON_SOCK, handled above)
    // must still never rendezvous on the developer's live daemon socket. The
    // discriminator is stable across the whole test run's process tree, so a
    // test and the binaries it spawns agree on the same private path.
    if crate::test_isolation::spawned_under_test_harness() {
        return std::env::temp_dir().join(format!(
            "ahma-test-daemon-{}.sock",
            crate::test_isolation::test_run_discriminator()
        ));
    }

    platform_default_socket_path()
}

/// The per-user runtime directory holding every daemon rendezvous file
/// (SPEC R-DAEMON.2): the hub socket, the MCP socket, and on Windows the
/// endpoint descriptor.
///
/// Prefers `$XDG_RUNTIME_DIR/ahma` (per-user, tmpfs, cleaned at logout) and
/// falls back to `~/.ahma`. Created with mode `0700` as a side effect, so the
/// returned directory is immediately usable — a shared directory is how a
/// second local user gets to see, and squat, another user's endpoints.
#[cfg(unix)]
pub fn runtime_dir() -> Option<PathBuf> {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|x| !x.is_empty())
        .map(|xdg| PathBuf::from(xdg).join("ahma"))
        .or_else(|| crate::config::ahma_home_dir().map(|home| home.join(".ahma")))?;
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

/// Windows counterpart of [`runtime_dir`]: `%LOCALAPPDATA%\ahma\run`, falling
/// back to `~/.ahma`. Access control comes from the per-user profile ACL rather
/// than a mode bit.
#[cfg(not(unix))]
pub fn runtime_dir() -> Option<PathBuf> {
    let dir = std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|x| !x.is_empty())
        .map(|local| PathBuf::from(local).join("ahma").join("run"))
        .or_else(|| crate::config::ahma_home_dir().map(|home| home.join(".ahma")))?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

/// Refuse a runtime directory another local user can reach (SPEC R-DAEMON.2).
///
/// The daemon executes shell and build commands on behalf of anything that can
/// connect to its sockets, so the directory holding them must be the caller's
/// own and unreachable by group or other — the same rule sshd applies to
/// `~/.ssh`. A socket chmodded `0600` inside a `0777` directory is still
/// squattable: another user can unlink the path and bind their own listener
/// there before the real client connects.
#[cfg(unix)]
pub fn verify_runtime_dir_secure(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(dir) else {
        bail!("cannot stat runtime directory {}", dir.display());
    };
    // Our own uid without a `libc` dependency in this crate: a file we just
    // created is owned by our effective uid by definition, so its `uid()` is
    // the number to compare the directory against.
    let probe_path = dir.join(format!(".ahma-owner-probe-{}", std::process::id()));
    let our_uid = match std::fs::File::create(&probe_path) {
        Ok(f) => {
            let uid = f.metadata().ok().map(|m| m.uid());
            drop(f);
            let _ = std::fs::remove_file(&probe_path);
            uid
        }
        Err(e) => {
            bail!(
                "cannot write inside runtime directory {}: {e}. Fix its ownership \
                 and permissions (chmod 700), or set XDG_RUNTIME_DIR.",
                dir.display()
            );
        }
    };
    if let Some(our_uid) = our_uid
        && meta.uid() != our_uid
    {
        bail!(
            "runtime directory {} is owned by uid {}, not by this user (uid {}); \
             refusing to use it. Remove or chown it, or set XDG_RUNTIME_DIR.",
            dir.display(),
            meta.uid(),
            our_uid
        );
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "runtime directory {} is mode {:o}; it must not be readable or writable by group \
             or others (0700). Fix with: chmod 700 {}",
            dir.display(),
            mode,
            dir.display()
        );
    }
    Ok(())
}

/// Windows has no mode bits to check; the per-user profile ACL is the boundary.
#[cfg(not(unix))]
pub fn verify_runtime_dir_secure(dir: &std::path::Path) -> Result<()> {
    if !dir.is_dir() {
        bail!("runtime directory {} does not exist", dir.display());
    }
    Ok(())
}

/// Step 3 of [`default_socket_path`]'s resolution order: the platform default,
/// ignoring every override. Creates the parent directory as a side effect so the
/// returned path is immediately bindable.
#[cfg(unix)]
fn platform_default_socket_path() -> PathBuf {
    match runtime_dir() {
        Some(dir) => dir.join("daemon.sock"),
        None => PathBuf::from("/tmp/ahma-daemon.sock"),
    }
}

/// On Windows the path is unused; callers use the TCP address.
#[cfg(not(unix))]
fn platform_default_socket_path() -> PathBuf {
    PathBuf::from("unused-on-windows")
}

/// The per-user MCP endpoint socket, ignoring test isolation and any explicit
/// override — the path the daemon binds in production.
///
/// It lives beside the hub socket in [`runtime_dir`] rather than at the old
/// machine-global `/tmp/ahma.sock`, which every local user could see and, since
/// nothing owned the path, pre-create.
pub fn platform_mcp_socket_path() -> PathBuf {
    match runtime_dir() {
        Some(dir) => dir.join("mcp.sock"),
        None => PathBuf::from("/tmp/ahma-mcp.sock"),
    }
}

/// The hub socket that belongs with `mcp_socket`.
///
/// The rendezvous is a **pair**, and the hub half is the mutex: a daemon told
/// to serve a private MCP socket but left on the shared hub socket would lose
/// the bind to whichever daemon already held it, stand down, and leave nobody
/// serving the path its caller asked for. So an explicitly chosen MCP socket
/// brings its own hub, beside it.
pub fn hub_socket_beside(mcp_socket: &str) -> PathBuf {
    let mcp = PathBuf::from(mcp_socket);
    if mcp == platform_mcp_socket_path() {
        return platform_default_socket_path();
    }
    let dir = mcp.parent().map(PathBuf::from).unwrap_or_default();
    let stem = mcp
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ahma".to_string());
    dir.join(format!("{stem}.hub.sock"))
}

/// Resolve the MCP endpoint socket path (SPEC R-DAEMON.2).
///
/// Resolution order, mirroring [`default_socket_path`] so the two rendezvous
/// files can never disagree about which run they belong to:
/// 1. `explicit` — the `--unix-socket-path` flag or `[http] unix_socket_path`.
/// 2. Under a test harness, a private per-run path keyed by the same
///    discriminator as the hub socket (SPEC R-ISO.1). Parent and child
///    processes in one test run therefore agree without plumbing.
/// 3. [`platform_mcp_socket_path`].
pub fn mcp_socket_path(explicit: Option<&str>) -> String {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        return p.to_string();
    }
    if crate::test_isolation::spawned_under_test_harness() {
        return std::env::temp_dir()
            .join(format!(
                "ahma-test-mcp-{}.sock",
                crate::test_isolation::test_run_discriminator()
            ))
            .to_string_lossy()
            .into_owned();
    }
    platform_mcp_socket_path().to_string_lossy().into_owned()
}

// ─────────────────────────────────────────────────────────────────────────────
// Client-side transport abstraction
// ─────────────────────────────────────────────────────────────────────────────

/// Platform stream type returned by [`connect_to_daemon`].
///
/// On Unix this is a `UnixStream`; on Windows a `TcpStream`.
/// Both implement `AsyncRead + AsyncWrite + Unpin + Send`.
#[cfg(unix)]
pub type DaemonStream = tokio::net::UnixStream;

#[cfg(not(unix))]
pub type DaemonStream = tokio::net::TcpStream;

/// Try to connect to the hub daemon.
///
/// Returns `Ok(stream)` on success, or an error if the daemon is not running.
pub async fn connect_to_daemon() -> Result<DaemonStream> {
    #[cfg(unix)]
    {
        let path = default_socket_path();
        Ok(tokio::net::UnixStream::connect(&path).await?)
    }
    #[cfg(not(unix))]
    {
        Ok(tokio::net::TcpStream::connect(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            daemon_port(),
        )))
        .await?)
    }
}

/// One-shot query of a daemon at an explicit socket: connect, ask for the
/// instance list, read the answer, hang up.
///
/// Takes the path rather than resolving it so a test can address the daemon it
/// started, and so a diagnostic can address one that is not the default.
#[cfg(unix)]
pub async fn list_instances_at(socket_path: &std::path::Path) -> Result<Vec<InstanceInfo>> {
    let stream = tokio::net::UnixStream::connect(socket_path).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    send_msg(&mut write_half, &ClientMsg::ListInstances).await?;
    match recv_msg::<_, DaemonMsg>(&mut reader).await? {
        DaemonMsg::InstanceList { instances } => Ok(instances),
        other => bail!("expected an instance list, got {other:?}"),
    }
}

/// Returns `true` if a daemon is currently accepting connections.
async fn try_connect() -> bool {
    connect_to_daemon().await.is_ok()
}

/// Spawn `ahma daemon` as a detached child and return as soon as the fork
/// succeeded — readiness is the caller's concern.
///
/// Uses the current executable so this works regardless of `PATH`.
///
/// Intentionally detached (SPEC R-PROC.3): this daemon must outlive us, so it
/// deliberately does NOT set `kill_on_drop`. `process_group(0)` is used here for
/// the opposite reason to an owned child (R-PROC.2) — `setpgid(0,0)` puts the
/// daemon in its own group so it is *not* killed when the spawning terminal/IDE
/// exits, rather than so it can be reaped with us.
fn spawn_detached_daemon() -> Result<()> {
    // Never from a test binary (SPEC R-ISO.1). `current_exe()` inside one is
    // the *test harness*, not `ahma`, so this would re-run the test binary with
    // `daemon` as its filter argument. If any test name matches that filter,
    // each spawned copy re-runs the tests that spawn — a fork bomb that takes
    // the whole machine's process table with it, which is exactly what happened
    // the first time a test exercised this path with no daemon running.
    if crate::test_isolation::spawned_under_test_harness() {
        bail!(
            "refusing to spawn a daemon from a test binary: start one explicitly \
             (`run_daemon_at`/`HubServer::bind_at`) and point the client at its socket"
        );
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ahma"));
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    cmd.process_group(0);

    if let Err(e) = cmd.spawn() {
        bail!("Failed to spawn ahma daemon: {e}");
    }
    debug!("daemon_hub: spawned ahma daemon from {:?}", exe);
    Ok(())
}

/// Ensure a hub daemon is running, starting one if necessary.
///
/// * Fast path: daemon already up — returns immediately.
/// * Slow path: spawns `ahma daemon` as a detached child, then polls until
///   it accepts connections (up to ~1 second / 20 attempts at 50 ms).
///
/// Returns `Ok(())` when a daemon is reachable, or an error if it could not
/// be started.
pub async fn ensure_daemon_running() -> Result<()> {
    if try_connect().await {
        return Ok(());
    }

    spawn_detached_daemon()?;

    // Poll until connected (max ~1 s).
    for attempt in 1..=20u32 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if try_connect().await {
            debug!("daemon_hub: connected after {}ms", attempt * 50);
            return Ok(());
        }
    }

    bail!(
        "ahma daemon did not become ready within 1 s. \
         Try starting it manually with `ahma daemon`."
    )
}

/// Stop the running hub daemon immediately.
pub async fn stop_daemon() -> Result<()> {
    if let Ok(mut stream) = connect_to_daemon().await {
        send_msg(&mut stream, &ClientMsg::Shutdown).await?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Framing helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Serialise `msg` as JSON and write it as a single line to `writer`.
pub async fn send_msg<W, M>(writer: &mut W, msg: &M) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    M: Serialize,
{
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Read one newline-delimited JSON message from a buffered reader.
pub async fn recv_msg<R, M>(reader: &mut BufReader<R>) -> Result<M>
where
    R: tokio::io::AsyncRead + Unpin,
    M: for<'de> Deserialize<'de>,
{
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            bail!("daemon connection closed (EOF)");
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(msg) => return Ok(msg),
            // A message this build does not know is skipped, not fatal. This
            // socket has no version to negotiate (R24.5), so a daemon left
            // running across an upgrade is the reader that decides — and it
            // used to decide by dropping the connection, which turned every
            // future message addition into a hard incompatibility. Skipping is
            // what makes a new variant possible at all.
            Err(e) if is_unknown_message(&e) => {
                // Warn, not debug: skipping is deliberate, but a build that
                // keeps skipping is a version skew somebody should see.
                let preview: String = line.chars().take(120).collect();
                warn!("hub: skipping a message this build does not understand ({e}): {preview}");
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// True when a decode failure is "I do not know this message", as opposed to
/// malformed JSON or a field of the wrong type — which are real protocol
/// errors and must still close the connection.
fn is_unknown_message(e: &serde_json::Error) -> bool {
    e.is_data() && {
        let msg = e.to_string();
        // `ClientMsg`/`DaemonMsg` carry an untagged `Relay` variant, so an
        // unrecognised `"type"` surfaces as "did not match any variant" rather
        // than "unknown variant"; both mean the same thing here.
        msg.contains("unknown variant")
            || msg.contains("did not match any variant")
            || msg.contains("missing field `type`")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Server-side: run_daemon
// ─────────────────────────────────────────────────────────────────────────────

/// One operation's replayable state: the `OpStarted` event plus the terminal
/// `OpFinished` event once it completes. Streaming output (`OpOutput`) is a live
/// tail and is intentionally not retained for replay.
#[derive(Clone)]
struct OpSnapshot {
    /// Monotonic insertion order, used to evict the oldest finished op first.
    seq: u64,
    started: DaemonEvent,
    finished: Option<DaemonEvent>,
    /// The last [`MAX_TAIL_LINES`] output lines, replayed after `started` so a
    /// subscriber that attaches mid-operation sees what it has been printing.
    /// Bounded on purpose: this is a window, not a log — the complete output
    /// lives in the operation's own output file.
    tail: std::collections::VecDeque<(String, bool)>,
    /// When this op reached a terminal state (Unix epoch, milliseconds), for
    /// the replay window. Taken from the wire event when it carries one, and
    /// from the hub's own clock when it does not — a cancellation has no exit
    /// code and need not carry an end time, but it still has to age out.
    /// `None` while the op is still running.
    finished_at_ms: Option<u64>,
}

/// Per-instance operation history, keyed by op id.
type InstanceOpHistory = std::collections::HashMap<String, OpSnapshot>;

/// Wall-clock milliseconds since the Unix epoch.
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The operation id an event refers to, if it is about an operation at all.
fn op_id_of(event: &DaemonEvent) -> Option<String> {
    match event {
        DaemonEvent::OpStarted { id, .. }
        | DaemonEvent::OpFinished { id, .. }
        | DaemonEvent::OpOutput { id, .. } => Some(id.clone()),
        DaemonEvent::LogLine { .. } => None,
    }
}

/// Rebuild the `OpStarted` an orphaned terminal event never had.
///
/// The title is the one thing a reader needs and the one thing the terminal
/// event carries (its result summary); the start time is derived from the end
/// time and the duration, both of which are on the wire. Everything else is
/// left empty rather than guessed.
fn synthetic_started(
    op_id: &str,
    finished: &DaemonEvent,
    duration_ms: u64,
    ended_epoch_ms: Option<u64>,
) -> DaemonEvent {
    let summary = match finished {
        DaemonEvent::OpFinished { result_summary, .. } => result_summary.clone(),
        _ => None,
    };
    DaemonEvent::OpStarted {
        id: op_id.to_string(),
        tool_name: String::new(),
        description: summary.clone().unwrap_or_default(),
        scope: String::new(),
        parent_id: None,
        started_epoch_ms: ended_epoch_ms.map(|e| e.saturating_sub(duration_ms)),
        title: summary,
        cwd: None,
        command: None,
        origin: None,
        // The whole point of this record: it was reconstructed, and a reader
        // must not present its blanks as fact.
        partial: true,
        // Unknown, and "unknown" is not a claim we get to make in the
        // alarming direction.
        unsandboxed: false,
    }
}

/// Internal shared state for the running daemon.
#[derive(Debug, Clone)]
struct PendingApproval {
    id: String,
    tool: String,
    args: String,
}

/// What the hub does when it is asked to stop.
///
/// The hub used to call [`std::process::exit`] from inside a connection
/// handler. That is correct for the standalone `ahma daemon` and wrong for
/// anything that hosts the hub alongside something else: the per-user daemon
/// also owns an MCP endpoint with live sessions and a history file to flush, so
/// "stop" has to run one shutdown choreography, not `exit(0)` from whichever
/// task noticed first. The composer installs a hook; with no hook installed the
/// default is the historical exit.
type ExitHook = Arc<dyn Fn(&str) + Send + Sync>;

struct DaemonHub {
    instances: Arc<Mutex<std::collections::HashMap<String, InstanceInfo>>>,
    instance_txs:
        Arc<Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<DaemonMsg>>>>,
    broadcast: broadcast::Sender<DaemonMsg>,
    /// Last-known operation state per instance. The `broadcast` channel only
    /// reaches subscribers connected at send time, so without this a TUI opened
    /// (or reconnected) after calls already ran would show the instance with an
    /// empty operation list. Replayed to each subscriber right after the initial
    /// `InstanceList` so the monitor reflects all calls, not just future ones.
    op_history: Arc<Mutex<std::collections::HashMap<String, InstanceOpHistory>>>,
    op_seq: AtomicU64,
    connection_count: Arc<AtomicUsize>,
    socket_path: Option<PathBuf>,
    pending_approvals: Arc<Mutex<std::collections::HashMap<String, PendingApproval>>>,
    /// `session_id` → the instance id first assigned to it, so a re-register
    /// keeps its identity (and its retained history) instead of arriving as a
    /// stranger.
    session_ids: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// `decision_id` → the instance that raised it, so an answer is routed
    /// back to the session that asked the question.
    pending_decisions: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// Instances that have disconnected but whose operations are still inside
    /// the replay window, stamped with when they went (SPEC R-DAEMON.7).
    ///
    /// History used to be dropped the instant an instance disconnected, which
    /// made a whole class of work invisible: a hooked command is an instance
    /// that lives for the length of one command, so by the time anyone looked
    /// at the TUI it had always already gone.
    ended_instances: Arc<Mutex<std::collections::HashMap<String, InstanceInfo>>>,
    /// What to run when a client sends [`ClientMsg::Shutdown`]. `None` means
    /// the historical behaviour: unlink our socket and `exit(0)`.
    exit_hook: parking_lot::Mutex<Option<ExitHook>>,
    /// Appends operation history to disk so it outlives this process
    /// (SPEC R-DAEMON.7). `None` keeps history in memory only.
    history: parking_lot::Mutex<Option<Arc<crate::daemon_history::HistoryWriter>>>,
}

impl DaemonHub {
    fn new(socket_path: Option<PathBuf>) -> (Self, broadcast::Receiver<DaemonMsg>) {
        let (tx, rx) = broadcast::channel(512);
        (
            Self {
                instances: Arc::new(Mutex::new(std::collections::HashMap::new())),
                instance_txs: Arc::new(Mutex::new(std::collections::HashMap::new())),
                broadcast: tx,
                op_history: Arc::new(Mutex::new(std::collections::HashMap::new())),
                op_seq: AtomicU64::new(0),
                connection_count: Arc::new(AtomicUsize::new(0)),
                socket_path,
                pending_approvals: Arc::new(Mutex::new(std::collections::HashMap::new())),
                session_ids: Arc::new(Mutex::new(std::collections::HashMap::new())),
                pending_decisions: Arc::new(Mutex::new(std::collections::HashMap::new())),
                ended_instances: Arc::new(Mutex::new(std::collections::HashMap::new())),
                exit_hook: parking_lot::Mutex::new(None),
                history: parking_lot::Mutex::new(None),
            },
            rx,
        )
    }

    /// Evict the oldest finished op once an instance's retained history grows
    /// past the cap; a running op is never dropped.
    fn evict_oldest_finished(inst: &mut InstanceOpHistory) {
        if let Some(oldest) = inst
            .iter()
            .filter(|(_, s)| s.finished.is_some())
            .min_by_key(|(_, s)| s.seq)
            .map(|(k, _)| k.clone())
        {
            inst.remove(&oldest);
        }
    }

    /// Record an operation event so it can be replayed to subscribers that join
    /// later: `OpStarted`, a bounded window of the output that followed it, and
    /// the terminal `OpFinished`. Log lines are live-only.
    async fn record_op_event(&self, instance_id: &str, payload: &DaemonEvent) {
        match payload {
            DaemonEvent::OpStarted { id, .. } => {
                self.record_op_started(instance_id, id, payload).await
            }
            DaemonEvent::OpFinished { id, .. } => {
                self.record_op_finished(instance_id, id, payload).await
            }
            DaemonEvent::OpOutput {
                id,
                line,
                is_stderr,
            } => {
                self.record_op_output(instance_id, id, line, *is_stderr)
                    .await
            }
            DaemonEvent::LogLine { .. } => {}
        }
    }

    /// Append one output line to `op_id`'s retained window, evicting the oldest
    /// once [`MAX_TAIL_LINES`] is reached. Output for an op the hub never saw
    /// start is dropped: it has no row to belong to.
    async fn record_op_output(&self, instance_id: &str, op_id: &str, line: &str, is_stderr: bool) {
        let mut hist = self.op_history.lock().await;
        if let Some(snap) = hist.get_mut(instance_id).and_then(|i| i.get_mut(op_id)) {
            if snap.tail.len() >= MAX_TAIL_LINES {
                snap.tail.pop_front();
            }
            snap.tail.push_back((line.to_string(), is_stderr));
        }
    }

    /// Retain `started` as the new head of `op_id`'s history, enforcing the
    /// per-instance retention cap.
    async fn record_op_started(&self, instance_id: &str, op_id: &str, started: &DaemonEvent) {
        let history = self.history.lock().clone();
        if let Some(writer) = history {
            // The instance travels with the record so a replayed op still has a
            // section to belong to after a restart, when nothing is attached.
            let instance = self
                .instances
                .lock()
                .await
                .get(instance_id)
                .cloned()
                .unwrap_or_else(|| InstanceInfo {
                    id: instance_id.to_string(),
                    pid: 0,
                    mode: String::new(),
                    scope: String::new(),
                    label: String::new(),
                    client: None,
                    session_id: None,
                    client_pid: None,
                    ended_epoch_ms: None,
                });
            writer.record(crate::daemon_history::HistoryRecord::Started {
                ts: now_epoch_ms(),
                instance,
                event: started.clone(),
            });
        }
        let seq = self.op_seq.fetch_add(1, Ordering::Relaxed);
        let mut hist = self.op_history.lock().await;
        let inst = hist.entry(instance_id.to_string()).or_default();
        inst.insert(
            op_id.to_string(),
            OpSnapshot {
                seq,
                started: started.clone(),
                finished: None,
                tail: std::collections::VecDeque::new(),
                finished_at_ms: None,
            },
        );
        if inst.len() > MAX_OPS_PER_INSTANCE {
            Self::evict_oldest_finished(inst);
        }
    }

    /// Attach the terminal event to `op_id`'s retained snapshot.
    ///
    /// An op whose `OpStarted` the hub never saw — because it was evicted, or
    /// because the daemon restarted while the op was running — is reconstructed
    /// from the terminal event alone and flagged `partial`. Dropping it instead
    /// (the old behaviour) lost the outcome of exactly the work a user is most
    /// likely to ask about, and left the instance's tallies wrong.
    async fn record_op_finished(&self, instance_id: &str, op_id: &str, finished: &DaemonEvent) {
        let DaemonEvent::OpFinished {
            duration_ms,
            ended_epoch_ms,
            ..
        } = finished
        else {
            return;
        };
        let mut hist = self.op_history.lock().await;
        let inst = hist.entry(instance_id.to_string()).or_default();
        // One disk write per operation, at the end, carrying the output window
        // as it finally stood — persisting each line would turn a chatty build
        // into megabytes for a window that is bounded anyway.
        let history = self.history.lock().clone();
        if let Some(writer) = history {
            let tail = inst
                .get(op_id)
                .map(|s| s.tail.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            writer.record(crate::daemon_history::HistoryRecord::Finished {
                ts: now_epoch_ms(),
                instance_id: instance_id.to_string(),
                event: finished.clone(),
                tail,
            });
        }
        match inst.get_mut(op_id) {
            Some(snap) => {
                snap.finished = Some(finished.clone());
                snap.finished_at_ms = Some(ended_epoch_ms.unwrap_or_else(now_epoch_ms));
            }
            None => {
                let seq = self.op_seq.fetch_add(1, Ordering::Relaxed);
                inst.insert(
                    op_id.to_string(),
                    OpSnapshot {
                        seq,
                        started: synthetic_started(op_id, finished, *duration_ms, *ended_epoch_ms),
                        finished: Some(finished.clone()),
                        tail: std::collections::VecDeque::new(),
                        finished_at_ms: Some(ended_epoch_ms.unwrap_or_else(now_epoch_ms)),
                    },
                );
                if inst.len() > MAX_OPS_PER_INSTANCE {
                    Self::evict_oldest_finished(inst);
                }
            }
        }
    }

    /// Restore history written by a previous daemon (SPEC R-DAEMON.7).
    ///
    /// An operation that has a start record but no terminal one was still
    /// running when that daemon went away. Its real outcome is unknowable — the
    /// command may well have finished — so it is closed as `interrupted`
    /// rather than left running forever (a spinner that never resolves) or
    /// called failed (an invention).
    async fn load_history(&self, records: Vec<crate::daemon_history::HistoryRecord>) {
        use crate::daemon_history::HistoryRecord as R;
        let mut hist = self.op_history.lock().await;
        let mut ended = self.ended_instances.lock().await;
        let mut ended_at: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

        for record in records {
            match record {
                R::Started {
                    instance, event, ..
                } => {
                    let Some(op_id) = op_id_of(&event) else {
                        continue;
                    };
                    let seq = self.op_seq.fetch_add(1, Ordering::Relaxed);
                    hist.entry(instance.id.clone()).or_default().insert(
                        op_id,
                        OpSnapshot {
                            seq,
                            started: event,
                            finished: None,
                            tail: std::collections::VecDeque::new(),
                            finished_at_ms: None,
                        },
                    );
                    ended.entry(instance.id.clone()).or_insert(instance);
                }
                R::Finished {
                    ts,
                    instance_id,
                    event,
                    tail,
                } => {
                    let Some(op_id) = op_id_of(&event) else {
                        continue;
                    };
                    if let Some(snap) = hist.get_mut(&instance_id).and_then(|i| i.get_mut(&op_id)) {
                        snap.finished = Some(event);
                        snap.finished_at_ms = Some(ts);
                        snap.tail = tail.into_iter().collect();
                    }
                }
                R::InstanceEnded { ts, instance_id } => {
                    ended_at.insert(instance_id, ts);
                }
            }
        }

        for (id, info) in ended.iter_mut() {
            // Nothing loaded from disk is attached: this daemon has only just
            // started. Anything without a recorded end is stamped now, so it
            // still ages out of the window.
            info.ended_epoch_ms = Some(
                ended_at
                    .get(id)
                    .copied()
                    .or(info.ended_epoch_ms)
                    .unwrap_or_else(now_epoch_ms),
            );
        }

        let mut interrupted = 0usize;
        for ops in hist.values_mut() {
            for snap in ops.values_mut() {
                if snap.finished.is_some() {
                    continue;
                }
                let Some(op_id) = op_id_of(&snap.started) else {
                    continue;
                };
                interrupted += 1;
                snap.finished_at_ms = Some(now_epoch_ms());
                snap.finished = Some(DaemonEvent::OpFinished {
                    id: op_id,
                    // "Failed" is what a reader that predates `interrupted`
                    // sees; a current one renders the flag instead (R24.5).
                    status: OpStatus::Failed,
                    result_summary: Some(
                        "interrupted: the daemon watching this operation exited".to_string(),
                    ),
                    duration_ms: 0,
                    ended_epoch_ms: snap.finished_at_ms,
                    exit_code: None,
                    denial: None,
                    interrupted: true,
                });
            }
        }
        if interrupted > 0 {
            info!("ahma hub: {interrupted} operation(s) restored as interrupted");
        }
    }

    /// Every instance a subscriber should know about: those attached now, plus
    /// those retained inside the replay window so their operations have a
    /// section to belong to. An ended instance carries `ended_epoch_ms`, which
    /// is how a reader tells the two apart.
    async fn instance_snapshot(&self) -> Vec<InstanceInfo> {
        let mut out: Vec<InstanceInfo> = self.instances.lock().await.values().cloned().collect();
        let live: std::collections::HashSet<String> = out.iter().map(|i| i.id.clone()).collect();
        out.extend(
            self.ended_instances
                .lock()
                .await
                .values()
                .filter(|i| !live.contains(&i.id))
                .cloned(),
        );
        out
    }

    /// Snapshot the retained op events for every instance, ordered for replay:
    /// each op's `OpStarted`, then the output window that followed it, then its
    /// `OpFinished` if it has one.
    ///
    /// The output is replayed as ordinary `OpOutput` events rather than a new
    /// message or a field on `OpStarted`: every subscriber already appends
    /// those to the right pane, so replay is byte-for-byte the shape the live
    /// stream has, and there is no second code path to keep in step (R24.5).
    async fn replay_events(&self) -> Vec<DaemonMsg> {
        let hist = self.op_history.lock().await;
        let mut snaps: Vec<(String, OpSnapshot)> = hist
            .iter()
            .flat_map(|(inst, ops)| ops.values().cloned().map(move |s| (inst.clone(), s)))
            .collect();
        snaps.sort_by_key(|(_, s)| s.seq);
        let mut out = Vec::with_capacity(snaps.len() * 2);
        for (instance_id, snap) in snaps {
            let op_id = op_id_of(&snap.started).unwrap_or_default();
            out.push(DaemonMsg::Event {
                instance_id: instance_id.clone(),
                payload: snap.started,
            });
            for (line, is_stderr) in snap.tail {
                out.push(DaemonMsg::Event {
                    instance_id: instance_id.clone(),
                    payload: DaemonEvent::OpOutput {
                        id: op_id.clone(),
                        line,
                        is_stderr,
                    },
                });
            }
            if let Some(finished) = snap.finished {
                out.push(DaemonMsg::Event {
                    instance_id,
                    payload: finished,
                });
            }
        }
        out
    }

    /// Drop retained operations that have aged out of the replay window, and
    /// any instance retained only for them; then enforce the global ceiling by
    /// evicting oldest-finished-first across every instance.
    async fn prune_history(&self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(HISTORY_REPLAY_WINDOW.as_millis() as u64);
        let mut hist = self.op_history.lock().await;
        for ops in hist.values_mut() {
            // A finished op ages out of the window; one still running never
            // does, however long it takes.
            ops.retain(|_, snap| snap.finished_at_ms.is_none_or(|ended| ended >= cutoff));
        }
        hist.retain(|_, ops| !ops.is_empty());

        let mut total: usize = hist.values().map(|o| o.len()).sum();
        while total > MAX_RETAINED_OPS {
            let oldest = hist
                .iter()
                .flat_map(|(inst, ops)| {
                    ops.iter()
                        .filter(|(_, s)| s.finished.is_some())
                        .map(move |(op, s)| (s.seq, inst.clone(), op.clone()))
                })
                .min();
            let Some((_, inst, op)) = oldest else { break };
            if let Some(ops) = hist.get_mut(&inst) {
                ops.remove(&op);
                if ops.is_empty() {
                    hist.remove(&inst);
                }
            }
            total -= 1;
        }

        // An instance retained only so its operations had somewhere to belong
        // goes with the last of them.
        let live: std::collections::HashSet<String> = hist.keys().cloned().collect();
        let mut ended = self.ended_instances.lock().await;
        ended.retain(|id, info| {
            live.contains(id)
                || info
                    .ended_epoch_ms
                    .is_some_and(|ended_at| ended_at >= cutoff)
        });
    }
}

/// Start the hub daemon.
///
/// Binds a unix socket (macOS / Linux) or TCP socket (Windows), accepts
/// connections from instances and TUI subscribers, and fans out events.
///
/// Exits automatically when no connections have been active for 60 seconds.
///
/// This function is called by `ahma daemon` via the CLI dispatch.
pub async fn run_daemon() -> Result<()> {
    run_daemon_at(default_socket_path()).await
}

/// Like [`run_daemon`] but binds at `socket_path` instead of the default.
///
/// Exposed for testing — callers can pass a temp-directory path to avoid
/// colliding with a real daemon running on the default socket.
pub async fn run_daemon_at(socket_path: PathBuf) -> Result<()> {
    let server = match HubServer::bind_at(socket_path).await {
        Ok(server) => server,
        Err(HubBindError::AlreadyRunning) => {
            info!("ahma daemon: another instance is already running, exiting");
            return Ok(());
        }
        Err(HubBindError::Failed(e)) => return Err(e),
    };
    server.spawn_standalone_idle_watcher();
    server.serve().await
}

// ─────────────────────────────────────────────────────────────────────────────
// Hub server — bind, serve and stop as three separate steps
// ─────────────────────────────────────────────────────────────────────────────

/// Why [`HubServer::bind_at`] did not produce a server.
#[derive(Debug)]
pub enum HubBindError {
    /// A live hub already owns the rendezvous. There is exactly one hub per
    /// user by design (SPEC R-DAEMON.1), so this is an ordinary outcome for the
    /// loser of a startup race, not a failure: connect to the winner instead.
    AlreadyRunning,
    /// The bind genuinely failed (permissions, a bad path, a port held by a
    /// foreign process).
    Failed(anyhow::Error),
}

impl std::fmt::Display for HubBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => write!(f, "another ahma hub already owns the rendezvous"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HubBindError {}

#[cfg(unix)]
type HubListener = tokio::net::UnixListener;
#[cfg(not(unix))]
type HubListener = tokio::net::TcpListener;

/// A bound, not-yet-serving hub.
///
/// Binding, serving and stopping are separate so one process can host the hub
/// *and* the MCP endpoint on one runtime with a single idle policy and a single
/// exit path (SPEC R-DAEMON.3). Fused together — as they were, inside
/// `run_daemon_at` — the hub's own idle timer and its `exit(0)` would race the
/// MCP endpoint's live sessions.
pub struct HubServer {
    listener: HubListener,
    hub: Arc<DaemonHub>,
    socket_path: PathBuf,
}

impl HubServer {
    /// Bind the hub rendezvous at `socket_path` (bind-is-the-mutex).
    ///
    /// A stale socket file — one nothing answers on — is removed and the bind
    /// retried; a **live** one is never stolen (SPEC R-ISO.2), it yields
    /// [`HubBindError::AlreadyRunning`].
    pub async fn bind_at(socket_path: PathBuf) -> std::result::Result<Self, HubBindError> {
        #[cfg(unix)]
        let listener = bind_unix(&socket_path).await?;
        #[cfg(not(unix))]
        let listener = bind_tcp().await?;

        info!("ahma hub: listening on {}", socket_path.display());
        let (hub, _) = DaemonHub::new(Some(socket_path.clone()));
        Ok(Self {
            listener,
            hub: Arc::new(hub),
            socket_path,
        })
    }

    /// Bind at the platform default hub socket.
    pub async fn bind() -> std::result::Result<Self, HubBindError> {
        Self::bind_at(default_socket_path()).await
    }

    /// Live connection count (instances, subscribers and one-shot queries).
    /// Shared with the caller so a composed daemon can fold it into one idle
    /// policy alongside its MCP session count.
    pub fn connection_count(&self) -> Arc<AtomicUsize> {
        self.hub.connection_count.clone()
    }

    /// The path this server bound, for an identity-checked unlink at shutdown.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Install what runs when a client sends `Shutdown`. Without one the hub
    /// unlinks its socket and exits the process, which is right only when the
    /// hub is the whole process.
    pub fn set_exit_hook(&self, hook: ExitHook) {
        *self.hub.exit_hook.lock() = Some(hook);
    }

    /// Load the recent on-disk history and start persisting to it.
    ///
    /// Returns the writer so the caller can flush it on the way out; `None`
    /// when no path is available, in which case history is in-memory only — a
    /// degraded mode, not a failure.
    pub async fn attach_history(
        &self,
        path: Option<PathBuf>,
    ) -> Option<Arc<crate::daemon_history::HistoryWriter>> {
        let path = path?;
        let cutoff = crate::daemon_history::replay_cutoff_ms(now_epoch_ms());
        let records = crate::daemon_history::load_recent(&path, cutoff).await;
        if !records.is_empty() {
            info!(
                "ahma hub: restoring {} history record(s) from {}",
                records.len(),
                path.display()
            );
            self.hub.load_history(records).await;
        }
        let writer = Arc::new(crate::daemon_history::HistoryWriter::start(Some(path))?);
        *self.hub.history.lock() = Some(writer.clone());
        Some(writer)
    }

    /// The standalone `ahma daemon`'s idle policy: exit once nothing has been
    /// connected for 60 s. A composed daemon does **not** call this — it owns a
    /// combined policy that also counts live MCP sessions (SPEC R-DAEMON.3).
    fn spawn_standalone_idle_watcher(&self) {
        let idle_count = self.hub.connection_count.clone();
        let socket_path = self.socket_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if idle_count.load(Ordering::Relaxed) != 0 {
                    continue;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
                if idle_count.load(Ordering::Relaxed) == 0 {
                    info!("ahma daemon: idle timeout, exiting");
                    // Unlink FIRST so a late arrival gets ENOENT (clean start)
                    // rather than ECONNREFUSED (ambiguous stale).
                    #[cfg(unix)]
                    crate::fs_lock::remove_stale_socket(&socket_path);
                    #[cfg(not(unix))]
                    let _ = &socket_path;
                    std::process::exit(0);
                }
            }
        });
    }

    /// Accept and serve until the listener fails.
    pub async fn serve(self) -> Result<()> {
        accept_loop(self.listener, self.hub).await
    }
}

// ── Unix bind/accept ──────────────────────────────────────────────────────────

/// Restrict a filesystem-backed Unix socket to owner-only (mode `0600`) so that
/// no other local user can `connect()` and drive the server (which executes
/// shell/build commands). Left to the process umask otherwise, a lax umask
/// (0, common in some containers/CI) yields a world-connectable socket.
///
/// No-op for Linux abstract-namespace sockets (path begins with NUL), which have
/// no filesystem entry to chmod. Best-effort: a failure is logged, not fatal.
#[cfg(unix)]
pub fn restrict_unix_socket_permissions(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    if path.as_os_str().as_bytes().first() == Some(&0) {
        return; // abstract socket — no filesystem permissions apply
    }
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        warn!(
            "failed to chmod 0600 unix socket {}: {e} (other local users may be able to connect)",
            path.display()
        );
    }
}

#[cfg(unix)]
async fn bind_unix(
    path: &std::path::Path,
) -> std::result::Result<tokio::net::UnixListener, HubBindError> {
    use tokio::net::UnixListener;
    loop {
        match UnixListener::bind(path) {
            Ok(l) => {
                restrict_unix_socket_permissions(path);
                return Ok(l);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Probe before unlinking: a socket someone answers on is a live
                // server and must never be stolen (SPEC R-ISO.2).
                match tokio::net::UnixStream::connect(path).await {
                    Ok(_) => return Err(HubBindError::AlreadyRunning),
                    Err(_) => {
                        // Nothing answers — a stale file from a crash or reboot.
                        debug!("ahma hub: removing stale socket at {}", path.display());
                        crate::fs_lock::remove_stale_socket(path);
                        // Small delay before retry to avoid a tight loop on odd filesystems.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
            Err(e) => {
                return Err(HubBindError::Failed(anyhow::anyhow!(
                    "ahma hub: failed to bind unix socket {}: {e}",
                    path.display()
                )));
            }
        }
    }
}

#[cfg(unix)]
async fn accept_loop(listener: tokio::net::UnixListener, hub: Arc<DaemonHub>) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                hub.connection_count.fetch_add(1, Ordering::Relaxed);
                let hub2 = hub.clone();
                tokio::spawn(async move {
                    handle_connection(stream, hub2).await;
                });
            }
            Err(e) => warn!("ahma daemon: accept error: {e}"),
        }
    }
}

// ── Windows bind/accept ───────────────────────────────────────────────────────

#[cfg(not(unix))]
async fn bind_tcp() -> std::result::Result<tokio::net::TcpListener, HubBindError> {
    use tokio::net::TcpListener;
    let port = daemon_port();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Try connecting to confirm a live hub rather than a foreign
            // process squatting the port.
            match tokio::net::TcpStream::connect(addr).await {
                Ok(_) => Err(HubBindError::AlreadyRunning),
                Err(_) => Err(HubBindError::Failed(anyhow::anyhow!(
                    "ahma hub: port {port} is in use by another process"
                ))),
            }
        }
        Err(e) => Err(HubBindError::Failed(anyhow::anyhow!(
            "ahma hub: failed to bind TCP socket: {e}"
        ))),
    }
}

#[cfg(not(unix))]
async fn accept_loop(listener: tokio::net::TcpListener, hub: Arc<DaemonHub>) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                hub.connection_count.fetch_add(1, Ordering::Relaxed);
                let hub2 = hub.clone();
                tokio::spawn(async move {
                    handle_connection(stream, hub2).await;
                });
            }
            Err(e) => warn!("ahma daemon: accept error: {e}"),
        }
    }
}

// ── Per-connection handler (generic over stream type) ─────────────────────────

async fn handle_connection<S>(stream: S, hub: Arc<DaemonHub>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // Read the first message to classify the connection.
    let first = match recv_msg::<_, ClientMsg>(&mut reader).await {
        Ok(m) => m,
        Err(e) => {
            debug!("daemon: connection closed before first message: {e}");
            hub.connection_count.fetch_sub(1, Ordering::Relaxed);
            return;
        }
    };

    // Each arm handles one connection role end-to-end; the larger long-lived
    // loops (instance / subscriber) live in dedicated helpers. Every arm returns
    // normally so the single `fetch_sub` below runs exactly once.
    match first {
        ClientMsg::Register {
            pid,
            mode,
            scope,
            label,
            client,
            session_id,
            client_pid,
        } => {
            serve_instance(
                &mut reader,
                &mut writer,
                &hub,
                Registration {
                    pid,
                    mode,
                    scope,
                    label,
                    client,
                    session_id,
                    client_pid,
                },
            )
            .await
        }

        ClientMsg::Subscribe => serve_subscriber(&mut writer, &hub).await,

        ClientMsg::ListInstances => {
            let instances = hub.instance_snapshot().await;
            let _ = send_msg(&mut writer, &DaemonMsg::InstanceList { instances }).await;
            // One-shot query — connection closes after response.
        }

        ClientMsg::Shutdown => {
            info!("daemon: shutdown requested");
            let hook = hub.exit_hook.lock().clone();
            match hook {
                // The composer owns the shutdown choreography (live MCP
                // sessions to terminate, history to flush, two sockets to
                // unlink) — it must not be short-circuited from here.
                Some(hook) => hook("client requested shutdown"),
                None => {
                    if let Some(ref path) = hub.socket_path {
                        crate::fs_lock::remove_stale_socket(path);
                    }
                    std::process::exit(0);
                }
            }
        }

        ClientMsg::SubmitPrompt {
            messages,
            system_prompt,
            provider,
            model,
            target_instance_id,
        } => {
            route_submit_prompt(
                &hub,
                target_instance_id,
                messages,
                system_prompt,
                provider,
                model,
            )
            .await
        }

        ClientMsg::SubmitApproval {
            id,
            approved,
            target_instance_id,
        } => route_submit_approval(&hub, id, approved, target_instance_id).await,

        ClientMsg::SubmitScopeGrant {
            decision_id,
            decision,
            target_instance_id,
        } => {
            // The instance that raised the question is the one that can apply
            // the answer; an explicit target is honoured, but never a guess.
            let target = match instance_for_decision(&hub, &decision_id).await {
                Some(owner) => Some(owner),
                None => target_instance_id,
            };
            route_to_instance(
                &hub,
                target,
                DaemonMsg::SubmitScopeGrant {
                    decision_id,
                    decision,
                },
            )
            .await
        }

        ClientMsg::SubmitWebApproval {
            decision_id,
            decision,
            target_instance_id,
        } => {
            let target = match instance_for_decision(&hub, &decision_id).await {
                Some(owner) => Some(owner),
                None => target_instance_id,
            };
            route_to_instance(
                &hub,
                target,
                DaemonMsg::SubmitWebApproval {
                    decision_id,
                    decision,
                },
            )
            .await
        }

        ClientMsg::ReRaiseScopeGrant {
            path,
            access,
            target_instance_id,
        } => {
            route_to_instance(
                &hub,
                target_instance_id,
                DaemonMsg::ReRaiseScopeGrant { path, access },
            )
            .await
        }

        _ => {
            debug!("daemon: unexpected message, closing connection");
        }
    }

    hub.connection_count.fetch_sub(1, Ordering::Relaxed);
}

/// Route a `SubmitPrompt` to the chosen instance. Every failure path must
/// broadcast an `AgentError` so the TUI stops its elapsed counter and shows
/// feedback — silently dropping the prompt leaves the user staring at a
/// forever-incrementing timer with no answer and no error.
async fn route_submit_prompt(
    hub: &Arc<DaemonHub>,
    target_instance_id: Option<String>,
    messages: Vec<DaemonChatMessage>,
    system_prompt: Option<String>,
    provider: Option<String>,
    model: Option<String>,
) {
    let target_id = resolve_target(hub, target_instance_id.as_deref()).await;

    let delivered = match target_id {
        Some(tid) => match hub.instance_txs.lock().await.get(&tid).cloned() {
            Some(tx) => tx
                .send(DaemonMsg::RunPrompt {
                    messages,
                    system_prompt,
                    provider,
                    model,
                })
                .await
                .is_ok(),
            None => false,
        },
        None => false,
    };

    if !delivered {
        warn!("daemon: SubmitPrompt could not be routed — no instance available to run it");
        let _ = hub.broadcast.send(DaemonMsg::Relay(HubRelay::AgentError {
            error: "No ahma instance is available to run the prompt. \
                    Make sure an ahma server is connected (it normally \
                    auto-starts); try reopening the TUI."
                .to_string(),
        }));
    }
}

/// Route a `SubmitApproval` to the chosen instance, clearing the pending
/// approval so it isn't replayed to newly-connected subscribers.
async fn route_submit_approval(
    hub: &Arc<DaemonHub>,
    id: Option<String>,
    approved: bool,
    target_instance_id: Option<String>,
) {
    // Prefer the instance that raised this exact call's question.
    let owner = match id.as_deref() {
        Some(call_id) => instance_for_decision(hub, call_id).await,
        None => None,
    };
    let target = owner.or(target_instance_id);
    if let Some(tid) = resolve_target(hub, target.as_deref()).await {
        hub.pending_decisions
            .lock()
            .await
            .retain(|_, owner| owner != &tid);
        hub.pending_approvals.lock().await.remove(&tid);
        if let Some(tx) = hub.instance_txs.lock().await.get(&tid) {
            let _ = tx.send(DaemonMsg::SubmitApproval { id, approved }).await;
        }
    }
}

/// Send `msg` to the instance a TUI request targets, if that instance is still
/// connected.
///
/// Every hub → instance route is this: resolve the target, look up its channel,
/// send. It was written out once per message type, which is three chances to
/// resolve against one instance and send to another. Silently dropping when the
/// instance has gone is deliberate and the reason there is no error to return —
/// a decision for an instance that disconnected has nowhere to be applied, and
/// the TUI has already closed its modal.
async fn route_to_instance(
    hub: &Arc<DaemonHub>,
    target_instance_id: Option<String>,
    msg: DaemonMsg,
) {
    if let Some(tid) = resolve_target(hub, target_instance_id.as_deref()).await
        && let Some(tx) = hub.instance_txs.lock().await.get(&tid)
    {
        let _ = tx.send(msg).await;
    }
}

/// Resolve which instance a TUI request targets (SPEC R-DAEMON.6).
///
/// An explicit id must name a **live** instance; one that has gone resolves to
/// nothing rather than falling through to somebody else's session. Without an
/// explicit id there must be exactly one candidate: the old rule took
/// `HashMap::keys().next()`, an arbitrary entry, which with one attached client
/// was right by construction and with several sent a user's answer to a
/// different window's question.
///
/// Hook and TUI instances are not candidates for an untargeted request: a hook
/// has no agent loop to run a prompt, and the TUI is the thing asking.
async fn resolve_target(hub: &DaemonHub, target: Option<&str>) -> Option<String> {
    let instances = hub.instances.lock().await;
    match target {
        Some(tid) => instances.contains_key(tid).then(|| tid.to_string()),
        None => {
            let mut candidates: Vec<&InstanceInfo> = instances
                .values()
                .filter(|i| i.mode != "hook" && i.mode != "tui")
                .collect();
            match candidates.len() {
                1 => Some(candidates.remove(0).id.clone()),
                0 => None,
                n => {
                    warn!(
                        "hub: refusing to guess among {n} attached instances for an \
                         untargeted request; the sender must name one"
                    );
                    None
                }
            }
        }
    }
}

/// Which instance raised `decision_id`, so its answer goes back to the session
/// that asked rather than to whichever one happens to be first.
async fn instance_for_decision(hub: &DaemonHub, decision_id: &str) -> Option<String> {
    hub.pending_decisions.lock().await.get(decision_id).cloned()
}

/// Serve a registered ahma instance: register it, then exchange events and
/// liveness pings until it disconnects, finally cleaning up its state.
#[allow(clippy::too_many_arguments)]
/// The fields an instance announces when it registers.
struct Registration {
    pid: u32,
    mode: String,
    scope: String,
    label: String,
    client: Option<String>,
    session_id: Option<String>,
    client_pid: Option<u32>,
}

async fn serve_instance<R, W>(
    reader: &mut BufReader<R>,
    writer: &mut W,
    hub: &Arc<DaemonHub>,
    reg: Registration,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // One session keeps one instance id for its whole life. An instance
    // re-registers whenever it learns something about itself (its client's
    // name, its committed sandbox scope), and a fresh id each time made that
    // look like a departure and an arrival: collapse state and selection keyed
    // on the id were lost, and any view sorting by id reshuffled.
    let id = match reg.session_id.as_deref() {
        Some(session) => hub
            .session_ids
            .lock()
            .await
            .entry(session.to_string())
            .or_insert_with(local_instance_id)
            .clone(),
        None => local_instance_id(),
    };
    // A session that comes back is live again, not history.
    hub.ended_instances.lock().await.remove(&id);
    let info = InstanceInfo {
        id: id.clone(),
        pid: reg.pid,
        mode: reg.mode,
        scope: reg.scope,
        label: reg.label,
        client: reg.client,
        session_id: reg.session_id,
        client_pid: reg.client_pid,
        ended_epoch_ms: None,
    };
    let pid = reg.pid;
    hub.instances.lock().await.insert(id.clone(), info.clone());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<DaemonMsg>(100);
    hub.instance_txs.lock().await.insert(id.clone(), tx);

    let _ = hub
        .broadcast
        .send(DaemonMsg::InstanceRegistered { instance: info });
    info!("daemon: instance registered id={id} pid={pid}");

    // Exchange events and liveness pings until the instance disconnects.
    let mut ping_interval =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ping_seq: u32 = 0;

    loop {
        tokio::select! {
            biased;

            msg = recv_msg::<_, ClientMsg>(reader) => {
                match msg {
                    Ok(ClientMsg::Event { payload }) => {
                        hub.record_op_event(&id, &payload).await;
                        let _ = hub.broadcast.send(DaemonMsg::Event {
                            instance_id: id.clone(),
                            payload,
                        });
                    }
                    Ok(ClientMsg::Pong { .. }) => {
                        // Liveness confirmed — nothing else to do for now.
                        debug!("daemon: pong received from id={id}");
                    }
                    Ok(ClientMsg::ScopeGrantResolved { decision_id }) => {
                        hub.pending_decisions.lock().await.remove(&decision_id);
                        let _ = hub.broadcast.send(DaemonMsg::ScopeGrantDismiss { decision_id });
                    }
                    Ok(ClientMsg::WebApprovalResolved { decision_id }) => {
                        hub.pending_decisions.lock().await.remove(&decision_id);
                        let _ = hub.broadcast.send(DaemonMsg::WebApprovalDismiss { decision_id });
                    }
                    // Everything the hub forwards untouched. One arm, so a new
                    // HubRelay message reaches subscribers without this loop
                    // being edited — and cannot be half-added.
                    Ok(ClientMsg::Relay(relay)) => {
                        // Relays with a side effect: remember the question so a
                        // TUI attaching mid-prompt is still shown it, and record
                        // which instance asked so the answer goes back to that
                        // session and no other (SPEC R-DAEMON.6).
                        match &relay {
                            HubRelay::ApprovalRequested { id: call_id, tool, args } => {
                                hub.pending_approvals.lock().await.insert(
                                    id.clone(),
                                    PendingApproval {
                                        id: call_id.clone(),
                                        tool: tool.clone(),
                                        args: args.clone(),
                                    },
                                );
                                hub.pending_decisions
                                    .lock()
                                    .await
                                    .insert(call_id.clone(), id.clone());
                            }
                            HubRelay::ScopeGrantRequested { request } => {
                                hub.pending_decisions
                                    .lock()
                                    .await
                                    .insert(request.decision_id.clone(), id.clone());
                            }
                            HubRelay::WebApprovalRequested { request } => {
                                hub.pending_decisions
                                    .lock()
                                    .await
                                    .insert(request.decision_id.clone(), id.clone());
                            }
                            _ => {}
                        }
                        let _ = hub.broadcast.send(DaemonMsg::Relay(relay));
                    }
                    Ok(ClientMsg::Unregister) | Err(_) => break,

                    // The rest are listed rather than swallowed by a catch-all,
                    // so that adding a ClientMsg variant is a compile error here
                    // — the previous `Ok(_) => {}` accepted a new message and
                    // dropped it, which looks exactly like a working relay until
                    // someone notices the TUI never updates.
                    //
                    // A TUI's own requests: answered on the subscriber
                    // connection, so an instance sending one is a client bug.
                    Ok(ClientMsg::Register { .. }
                        | ClientMsg::Subscribe
                        | ClientMsg::ListInstances
                        | ClientMsg::Shutdown
                        | ClientMsg::SubmitPrompt { .. }) => {
                        debug!("daemon: ignoring subscriber-only message from instance id={id}");
                    }
                    // Decisions the hub routes *to* an instance. They arrive on
                    // the TUI's connection and are forwarded from there; an
                    // instance never sends one back up its own.
                    Ok(ClientMsg::SubmitApproval { .. }
                        | ClientMsg::SubmitScopeGrant { .. }
                        | ClientMsg::ReRaiseScopeGrant { .. }
                        | ClientMsg::SubmitWebApproval { .. }) => {
                        debug!("daemon: ignoring instance-bound decision from instance id={id}");
                    }
                }
            }

            daemon_msg = rx.recv() => {
                match daemon_msg {
                    Some(msg) if send_msg(writer, &msg).await.is_ok() => {}
                    _ => break,
                }
            }

            _ = ping_interval.tick() => {
                ping_seq = ping_seq.wrapping_add(1);
                debug!("daemon: sending ping to id={id} seq={ping_seq}");
                if send_msg(writer, &DaemonMsg::Ping { seq: ping_seq })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    let departed = hub.instances.lock().await.remove(&id);
    hub.instance_txs.lock().await.remove(&id);
    hub.pending_approvals.lock().await.remove(&id);
    hub.pending_decisions
        .lock()
        .await
        .retain(|_, owner| owner != &id);
    // The operation history deliberately stays (SPEC R-DAEMON.7): it ages out
    // of the replay window instead, so work done by a session that has since
    // closed — or by a hook, which is an instance for the length of one
    // command — is still there when someone opens a TUI a minute later.
    if let Some(mut info) = departed {
        info.ended_epoch_ms = Some(now_epoch_ms());
        hub.ended_instances.lock().await.insert(id.clone(), info);
    }
    hub.prune_history(now_epoch_ms()).await;
    let _ = hub
        .broadcast
        .send(DaemonMsg::InstanceUnregistered { id: id.clone() });
    info!("daemon: instance unregistered id={id}");
}

/// Serve a TUI subscriber: send the current instance list, replay retained op
/// history, then stream live events until the connection closes.
async fn serve_subscriber<W>(writer: &mut W, hub: &Arc<DaemonHub>)
where
    W: AsyncWriteExt + Unpin,
{
    // Send current instance list, then stream events.
    let instances = hub.instance_snapshot().await;
    if let Err(e) = send_msg(writer, &DaemonMsg::InstanceList { instances }).await {
        debug!("daemon: subscriber write failed: {e}");
        return;
    }

    // Subscribe to live events BEFORE replaying retained history, so any
    // event that arrives during replay is queued by the broadcast channel
    // rather than lost in the gap between snapshot and live stream.
    let rx = hub.broadcast.subscribe();

    if replay_subscriber_backlog(writer, hub).await.is_err() {
        return;
    }

    stream_subscriber_events(writer, rx, hub).await;
}

async fn replay_subscriber_backlog<W>(writer: &mut W, hub: &Arc<DaemonHub>) -> Result<(), ()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut backlog = hub.replay_events().await;
    backlog.extend(
        hub.pending_approvals
            .lock()
            .await
            .values()
            .cloned()
            .map(|pending| {
                DaemonMsg::Relay(HubRelay::ApprovalRequested {
                    id: pending.id,
                    tool: pending.tool,
                    args: pending.args,
                })
            }),
    );
    for msg in backlog {
        if let Err(e) = send_msg(writer, &msg).await {
            debug!("daemon: subscriber backlog replay write failed: {e}");
            return Err(());
        }
    }
    Ok(())
}

async fn stream_subscriber_events<W>(
    writer: &mut W,
    mut rx: broadcast::Receiver<DaemonMsg>,
    hub: &Arc<DaemonHub>,
) where
    W: AsyncWriteExt + Unpin,
{
    loop {
        match rx.recv().await {
            Ok(msg) => {
                if let Err(e) = send_msg(writer, &msg).await {
                    debug!("daemon: subscriber write failed: {e}");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Continuing from the oldest available message is not enough:
                // the dropped messages may have included an `OpFinished`, and a
                // subscriber that misses one shows a spinner that never
                // resolves. Re-send the authoritative state instead — the same
                // snapshot a fresh subscriber gets — so the gap is repaired
                // rather than merely survived.
                warn!("daemon: subscriber lagged by {n} messages; resynchronising");
                if resync_subscriber(writer, hub).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Re-send the instance list and the retained history to one subscriber, after
/// its stream fell behind far enough to drop messages.
async fn resync_subscriber<W>(writer: &mut W, hub: &Arc<DaemonHub>) -> Result<(), ()>
where
    W: AsyncWriteExt + Unpin,
{
    let instances = hub.instance_snapshot().await;
    send_msg(writer, &DaemonMsg::InstanceList { instances })
        .await
        .map_err(|_| ())?;
    replay_subscriber_backlog(writer, hub).await
}

// ─── Instance ids ─────────────────────────────────────────────────────────────

/// A label distinguishing one attached instance from another.
///
/// **Not a secret, and not a UUID.** It is `pid-nanos-counter`: unique among
/// the instances of one machine, guessable by anyone who can guess a pid, and
/// broadcast in cleartext to every subscriber inside `InstanceInfo` because
/// naming an instance is its entire job.
///
/// It was called `uuid_v4`, which claimed randomness it has never had — the
/// misreading a static analyser made before a human did, and a dangerous one
/// to leave sitting one module away from
/// [`crate::daemon_endpoint::DaemonEndpoint`], whose bearer token *is* a
/// secret and *is* minted from a CSPRNG-backed `Uuid::new_v4`. When you need
/// unguessability, that is the function to reach for; this one cannot give it
/// to you.
fn local_instance_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let pid = std::process::id();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{pid:08x}-{ts:08x}-{seq:08x}")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::timeouts::TestTimeouts;
    // ── Wire compatibility for the R24.7 identity fields ─────────────────────

    /// A *new* event must deserialize into an *old* reader.
    ///
    /// R24.5 permits field-only evolution precisely so mixed-version daemon /
    /// instance / TUI combinations keep working. If a new field made an old reader
    /// fail, upgrading one component would silently blind the others.
    #[test]
    fn an_old_reader_ignores_the_new_identity_fields() {
        #[derive(serde::Deserialize)]
        struct OldOpStarted {
            id: String,
            tool_name: String,
        }

        let new = DaemonEvent::OpStarted {
            id: "op_1".into(),
            tool_name: "run_terminal_command".into(),
            description: "d".into(),
            scope: "/ws".into(),
            parent_id: None,
            started_epoch_ms: Some(1),
            title: Some("cargo build".into()),
            cwd: Some("/ws".into()),
            command: Some("cargo build".into()),
            origin: Some("cursor".into()),
            partial: false,
            unsandboxed: false,
        };
        let json = serde_json::to_string(&new).unwrap();
        let old: OldOpStarted = serde_json::from_str(&json).expect("old readers still parse");
        assert_eq!(old.id, "op_1");
        assert_eq!(old.tool_name, "run_terminal_command");
    }

    /// A daemon left running across an upgrade is the reader that decides
    /// (R24.5), so a `Register` carrying the session fields must still parse
    /// into one that has never heard of them.
    #[test]
    fn an_old_reader_ignores_the_new_registration_fields() {
        #[derive(serde::Deserialize)]
        struct OldRegister {
            pid: u32,
            mode: String,
            scope: String,
            label: String,
        }

        let msg = ClientMsg::Register {
            pid: 4242,
            mode: "stdio".into(),
            scope: "/ws".into(),
            label: "ahma".into(),
            client: Some("claude-code".into()),
            session_id: Some("sess-1".into()),
            client_pid: Some(99),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains("\"session_id\":\"sess-1\"") && json.contains("\"client_pid\":99"),
            "the new fields must actually be on the wire: {json}"
        );
        let old: OldRegister = serde_json::from_str(&json).expect("old daemons still parse");
        assert_eq!(old.pid, 4242);
        assert_eq!(old.mode, "stdio");
        assert_eq!(old.scope, "/ws");
        assert_eq!(old.label, "ahma");
    }

    /// ...and the reverse: an instance built before these fields existed still
    /// registers against a new daemon.
    #[test]
    fn a_new_reader_accepts_a_registration_without_the_session_fields() {
        let old_json = serde_json::json!({
            "type": "Register",
            "pid": 7,
            "mode": "stdio",
            "scope": "/ws",
            "label": "ahma"
        })
        .to_string();
        match serde_json::from_str::<ClientMsg>(&old_json).expect("old Register still parses") {
            ClientMsg::Register {
                session_id,
                client_pid,
                client,
                ..
            } => {
                assert_eq!(session_id, None);
                assert_eq!(client_pid, None);
                assert_eq!(client, None);
            }
            other => panic!("expected Register, got {other:?}"),
        }

        let old_instance = serde_json::json!({
            "id": "i1", "pid": 7, "mode": "stdio", "scope": "/ws", "label": "ahma"
        })
        .to_string();
        let info: InstanceInfo =
            serde_json::from_str(&old_instance).expect("old InstanceInfo still parses");
        assert_eq!(info.session_id, None);
        assert_eq!(info.client_pid, None);
        assert_eq!(info.ended_epoch_ms, None);
    }

    /// An *old* event must deserialize into a *new* reader, with the new fields
    /// absent rather than fatal — the TUI then falls back to its legacy naming.
    #[test]
    fn a_new_reader_accepts_an_event_with_no_identity_fields() {
        let old_json = serde_json::json!({
            "kind": "OpStarted",
            "id": "op_1",
            "tool_name": "run_terminal_command",
            "description": "d",
            "scope": "/ws"
        })
        .to_string();

        let ev: DaemonEvent = serde_json::from_str(&old_json).expect("old events still parse");
        match ev {
            DaemonEvent::OpStarted { title, origin, .. } => {
                assert!(title.is_none(), "no title from a pre-R24.7 server");
                assert!(origin.is_none());
            }
            other => panic!("expected OpStarted, got {other:?}"),
        }
    }

    /// Work that ran **outside** the sandbox says so on the wire.
    ///
    /// The TUI's `!` escape runs at the user's full privilege by design, and a
    /// unified view that renders it identically to sandboxed work would be
    /// lying by omission. An event from a producer that predates the field
    /// reads as sandboxed, which is the only safe default: those producers had
    /// no unsandboxed path to report.
    #[test]
    fn the_unsandboxed_flag_round_trips_and_defaults_to_confined() {
        let ev = DaemonEvent::OpStarted {
            id: "op_1".into(),
            tool_name: "shell".into(),
            description: "d".into(),
            scope: "/ws".into(),
            parent_id: None,
            started_epoch_ms: None,
            title: Some("rm -rf build".into()),
            cwd: Some("/ws".into()),
            command: Some("rm -rf build".into()),
            origin: Some("tui".into()),
            partial: false,
            unsandboxed: true,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"unsandboxed\":true"),
            "the flag must be on the wire: {json}"
        );
        match serde_json::from_str::<DaemonEvent>(&json).unwrap() {
            DaemonEvent::OpStarted { unsandboxed, .. } => assert!(unsandboxed),
            other => panic!("expected OpStarted, got {other:?}"),
        }

        let old = serde_json::json!({
            "kind": "OpStarted",
            "id": "op_1",
            "tool_name": "run_terminal_command",
            "description": "d",
            "scope": "/ws"
        })
        .to_string();
        match serde_json::from_str::<DaemonEvent>(&old).expect("old events still parse") {
            DaemonEvent::OpStarted { unsandboxed, .. } => assert!(
                !unsandboxed,
                "a producer with no unsandboxed path must not be read as having used one"
            ),
            other => panic!("expected OpStarted, got {other:?}"),
        }
    }

    /// The exit code survives the round trip, because "failed" without one is not
    /// actionable.
    #[test]
    fn the_exit_code_round_trips() {
        let ev = DaemonEvent::OpFinished {
            id: "op_1".into(),
            status: OpStatus::Failed,
            result_summary: None,
            duration_ms: 10,
            ended_epoch_ms: None,
            exit_code: Some(101),
            denial: None,
            interrupted: false,
        };
        let back: DaemonEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        match back {
            DaemonEvent::OpFinished { exit_code, .. } => assert_eq!(exit_code, Some(101)),
            other => panic!("expected OpFinished, got {other:?}"),
        }
    }

    use super::*;
    use tokio::io::BufReader;

    /// A filesystem-backed Unix socket must end up mode 0600, regardless of the
    /// process umask, so no other local user can connect and drive the server.
    #[cfg(unix)]
    #[tokio::test]
    async fn restrict_unix_socket_permissions_sets_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let _listener = tokio::net::UnixListener::bind(&sock).unwrap();
        restrict_unix_socket_permissions(&sock);
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be owner-only, got {mode:o}");
    }

    /// An abstract-namespace path (leading NUL) has no filesystem entry, so the
    /// helper must be a no-op and never error.
    #[cfg(unix)]
    #[test]
    fn restrict_unix_socket_permissions_ignores_abstract() {
        use std::os::unix::ffi::OsStrExt;
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(b"\0abstract-name"));
        restrict_unix_socket_permissions(p); // must not panic / error
    }

    // ── SubmitPrompt routing ──────────────────────────────────────────────────

    /// Regression: a `SubmitPrompt` that cannot be routed (no instance is
    /// registered) MUST broadcast an `AgentError` back to subscribers. Silently
    /// dropping it left the TUI's elapsed counter incrementing forever with no
    /// answer and no error — exactly the "ahma tui says nothing" symptom.
    #[cfg(unix)]
    #[tokio::test]
    async fn submit_prompt_without_instance_broadcasts_agent_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("regress.sock");

        let server = HubServer::bind_at(socket_path.clone())
            .await
            .expect("hub should bind a fresh socket");
        tokio::spawn(server.serve());

        // Subscribe over the socket before sending, so the broadcast is observed.
        let mut sub = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        send_msg(&mut sub, &ClientMsg::Subscribe).await.unwrap();
        let mut sub_reader = BufReader::new(sub);
        // Drain the initial InstanceList so the next read is the AgentError.
        let _ = recv_msg::<_, DaemonMsg>(&mut sub_reader).await.unwrap();

        // Connect as a client and submit a prompt with no instances registered.
        let mut client = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        send_msg(
            &mut client,
            &ClientMsg::SubmitPrompt {
                messages: vec![],
                system_prompt: None,
                provider: None,
                model: None,
                target_instance_id: None,
            },
        )
        .await
        .unwrap();

        let msg = tokio::time::timeout(
            TestTimeouts::scale_secs(2),
            recv_msg::<_, DaemonMsg>(&mut sub_reader),
        )
        .await
        .expect("AgentError should be broadcast, not dropped")
        .expect("subscriber connection open");

        match msg {
            DaemonMsg::Relay(HubRelay::AgentError { error }) => {
                assert!(
                    error.contains("No ahma instance"),
                    "unexpected error text: {error}"
                );
            }
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    // ── NDJ framing ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ndj_roundtrip_client_msg_register() {
        let msg = ClientMsg::Register {
            pid: 42,
            mode: "stdio".to_string(),
            scope: "/test".to_string(),
            label: "TestLabel".to_string(),
            client: None,
            session_id: None,
            client_pid: None,
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();
        assert!(buf.ends_with(b"\n"), "NDJ line must end with newline");
        assert_eq!(
            buf.iter().filter(|&&b| b == b'\n').count(),
            1,
            "exactly one newline"
        );

        let mut reader = BufReader::new(&buf[..]);
        let decoded: ClientMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            ClientMsg::Register {
                pid,
                mode,
                scope,
                label,
                client,
                ..
            } => {
                assert_eq!(pid, 42);
                assert_eq!(mode, "stdio");
                assert_eq!(scope, "/test");
                assert_eq!(label, "TestLabel");
                assert_eq!(client, None, "client field defaults to None");
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }

    /// Wire back-compat: messages serialized by a pre-causality peer (no
    /// `parent_id` / `started_epoch_ms` / `ended_epoch_ms` / `client` fields)
    /// must still deserialize — the task-tree protocol evolution is
    /// field-only, so mixed-version daemon/instance/TUI combinations keep
    /// working (SPEC R24.5).
    #[test]
    fn old_wire_format_without_causality_fields_still_parses() {
        let old_started = r#"{"kind":"OpStarted","id":"op-1","tool_name":"cargo_build","description":"Build","scope":"/w"}"#;
        let ev: DaemonEvent = serde_json::from_str(old_started).unwrap();
        match ev {
            DaemonEvent::OpStarted {
                parent_id,
                started_epoch_ms,
                ..
            } => {
                assert_eq!(parent_id, None);
                assert_eq!(started_epoch_ms, None);
            }
            other => panic!("expected OpStarted, got {other:?}"),
        }

        let old_finished = r#"{"kind":"OpFinished","id":"op-1","status":"Completed","result_summary":null,"duration_ms":5}"#;
        let ev: DaemonEvent = serde_json::from_str(old_finished).unwrap();
        match ev {
            DaemonEvent::OpFinished { ended_epoch_ms, .. } => assert_eq!(ended_epoch_ms, None),
            other => panic!("expected OpFinished, got {other:?}"),
        }

        let old_register =
            r#"{"type":"Register","pid":1,"mode":"stdio","scope":"/w","label":"VS Code"}"#;
        let msg: ClientMsg = serde_json::from_str(old_register).unwrap();
        match msg {
            ClientMsg::Register { client, .. } => assert_eq!(client, None),
            other => panic!("expected Register, got {other:?}"),
        }

        let old_instance = r#"{"id":"i1","pid":1,"mode":"stdio","scope":"/w","label":"Cursor"}"#;
        let info: InstanceInfo = serde_json::from_str(old_instance).unwrap();
        assert_eq!(info.client, None);
    }

    /// New causality/timing fields survive an NDJ round-trip intact.
    #[test]
    fn causality_fields_roundtrip() {
        let ev = DaemonEvent::OpStarted {
            id: "op-2".into(),
            tool_name: "run_terminal_command".into(),
            description: "cargo nextest run".into(),
            scope: "/w".into(),
            parent_id: Some("session:build-loop".into()),
            started_epoch_ms: Some(1_750_000_000_000),
            title: None,
            cwd: None,
            command: None,
            origin: None,
            partial: false,
            unsandboxed: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: DaemonEvent = serde_json::from_str(&json).unwrap();
        match back {
            DaemonEvent::OpStarted {
                parent_id,
                started_epoch_ms,
                ..
            } => {
                assert_eq!(parent_id.as_deref(), Some("session:build-loop"));
                assert_eq!(started_epoch_ms, Some(1_750_000_000_000));
            }
            other => panic!("expected OpStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_daemon_msg_instance_list() {
        let msg = DaemonMsg::InstanceList {
            instances: vec![InstanceInfo {
                id: "abc123".to_string(),
                pid: 99,
                mode: "http".to_string(),
                scope: "/project".to_string(),
                label: "Cursor".to_string(),
                client: None,
                session_id: None,
                client_pid: None,
                ended_epoch_ms: None,
            }],
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let decoded: DaemonMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            DaemonMsg::InstanceList { instances } => {
                assert_eq!(instances.len(), 1);
                assert_eq!(instances[0].id, "abc123");
                assert_eq!(instances[0].label, "Cursor");
            }
            other => panic!("expected InstanceList, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_daemon_event_op_started() {
        let msg = DaemonMsg::Event {
            instance_id: "inst-1".to_string(),
            payload: DaemonEvent::OpStarted {
                id: "op-1".to_string(),
                tool_name: "cargo_build".to_string(),
                description: "Build workspace".to_string(),
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
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let decoded: DaemonMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            DaemonMsg::Event {
                instance_id,
                payload: DaemonEvent::OpStarted { id, tool_name, .. },
            } => {
                assert_eq!(instance_id, "inst-1");
                assert_eq!(id, "op-1");
                assert_eq!(tool_name, "cargo_build");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_daemon_event_op_finished() {
        let msg = DaemonMsg::Event {
            instance_id: "inst-2".to_string(),
            payload: DaemonEvent::OpFinished {
                id: "op-2".to_string(),
                status: OpStatus::Completed,
                result_summary: Some("success".to_string()),
                duration_ms: 1500,
                ended_epoch_ms: None,
                exit_code: None,
                denial: None,
                interrupted: false,
            },
        };
        let mut buf = Vec::<u8>::new();
        send_msg(&mut buf, &msg).await.unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let decoded: DaemonMsg = recv_msg(&mut reader).await.unwrap();
        match decoded {
            DaemonMsg::Event {
                payload: DaemonEvent::OpFinished { id, status, .. },
                ..
            } => {
                assert_eq!(id, "op-2");
                assert_eq!(status, OpStatus::Completed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndj_roundtrip_subscribe_and_unregister() {
        // Verify unit variants round-trip correctly.
        for msg in [
            ClientMsg::Subscribe,
            ClientMsg::ListInstances,
            ClientMsg::Unregister,
        ] {
            let mut buf = Vec::<u8>::new();
            send_msg(&mut buf, &msg).await.unwrap();
            assert!(buf.ends_with(b"\n"));
        }
    }

    #[tokio::test]
    async fn ndj_eof_returns_error() {
        let empty: &[u8] = b"";
        let mut reader = BufReader::new(empty);
        let result: Result<ClientMsg> = recv_msg(&mut reader).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EOF") || msg.contains("closed"),
            "expected EOF error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn ndj_malformed_json_returns_error() {
        let bad = b"not-valid-json\n";
        let mut reader = BufReader::new(bad.as_slice());
        let result: Result<ClientMsg> = recv_msg(&mut reader).await;
        assert!(result.is_err());
    }

    // ── Instance ids ──────────────────────────────────────────────────────────

    /// Two instances attached at once must never share an id: the hub keys
    /// every operation, every routed decision and every TUI section on it.
    #[test]
    fn instance_ids_are_unique_within_a_process() {
        let ids: Vec<String> = (0..20).map(|_| local_instance_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "ids collided: {ids:?}");
    }

    /// The three fields stay three fields.
    ///
    /// The value is deliberately **not** interpolated into the failure
    /// message. It is not a secret — it is broadcast to every subscriber — but
    /// it reads like one, and a scanner that cannot tell an instance label from
    /// a credential was right to ask; the shape of the id is what this asserts,
    /// and the shape is in the assertion.
    #[test]
    fn an_instance_id_has_three_dash_separated_fields() {
        assert_eq!(
            local_instance_id().split('-').count(),
            3,
            "the id is pid-nanos-counter"
        );
    }

    #[test]
    fn scope_grant_messages_round_trip() {
        use crate::scope_grant::{GrantDecision, GrantReason, ScopeGrantRequest};

        let request = ScopeGrantRequest {
            decision_id: "dec-42".into(),
            path: std::path::PathBuf::from("/opt/ext/cache"),
            access: crate::config::ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: Some("sccache".into()),
        };

        // ClientMsg side (instance → hub, and TUI → hub).
        for msg in [
            ClientMsg::Relay(HubRelay::ScopeGrantRequested {
                request: request.clone(),
            }),
            ClientMsg::SubmitScopeGrant {
                decision_id: "dec-42".into(),
                decision: GrantDecision::GrantRo,
                target_instance_id: Some("inst-1".into()),
            },
            ClientMsg::ScopeGrantResolved {
                decision_id: "dec-42".into(),
            },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{msg:?}"), format!("{back:?}"));
        }

        // DaemonMsg side (hub → TUI, and hub → instance).
        for msg in [
            DaemonMsg::Relay(HubRelay::ScopeGrantRequested { request }),
            DaemonMsg::SubmitScopeGrant {
                decision_id: "dec-42".into(),
                decision: GrantDecision::Deny,
            },
            DaemonMsg::ScopeGrantDismiss {
                decision_id: "dec-42".into(),
            },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            let back: DaemonMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{msg:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn web_approval_messages_round_trip() {
        use crate::web_approval::{WebApprovalDecision, WebApprovalRequest};

        let request = WebApprovalRequest {
            decision_id: "web-7".into(),
            domain: "api.github.com".into(),
            url: "https://api.github.com/repos".into(),
            tool: Some("fetch_webpage".into()),
        };

        // ClientMsg side (instance → hub, and TUI → hub).
        for msg in [
            ClientMsg::Relay(HubRelay::WebApprovalRequested {
                request: request.clone(),
            }),
            ClientMsg::SubmitWebApproval {
                decision_id: "web-7".into(),
                decision: WebApprovalDecision::AllowSession,
                target_instance_id: Some("inst-1".into()),
            },
            ClientMsg::WebApprovalResolved {
                decision_id: "web-7".into(),
            },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{msg:?}"), format!("{back:?}"));
        }

        // DaemonMsg side (hub → TUI, and hub → instance).
        for msg in [
            DaemonMsg::Relay(HubRelay::WebApprovalRequested { request }),
            DaemonMsg::SubmitWebApproval {
                decision_id: "web-7".into(),
                decision: WebApprovalDecision::Deny,
            },
            DaemonMsg::WebApprovalDismiss {
                decision_id: "web-7".into(),
            },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            let back: DaemonMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{msg:?}"), format!("{back:?}"));
        }
    }

    // ── In-process daemon integration (Unix only) ─────────────────────────────
    //
    // We spin up `run_daemon_at()` in a background tokio task pointing to a
    // temp socket, then exercise the register → subscribe → event → unregister
    // flow end-to-end.  All spawned tasks are automatically cancelled when the
    // test runtime drops.

    /// Wait until a Unix socket file exists and accepts a connection.
    #[cfg(unix)]
    async fn wait_for_daemon(sock: &std::path::Path) {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if tokio::net::UnixStream::connect(sock).await.is_ok() {
                return;
            }
        }
        panic!("daemon did not start within 1 s on {}", sock.display());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_subscribe_register_event_unregister_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("daemon.sock");
        let sock2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(sock2).await;
        });
        wait_for_daemon(&sock).await;

        // ── Subscriber connects ────────────────────────────────────────────
        let sub = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connect subscriber");
        let (sr, sw) = tokio::io::split(sub);
        let mut sub_reader = BufReader::new(sr);
        let mut sub_writer = sw;
        send_msg(&mut sub_writer, &ClientMsg::Subscribe)
            .await
            .unwrap();

        // Initial InstanceList should be empty.
        let first: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        let DaemonMsg::InstanceList { instances } = first else {
            panic!("expected InstanceList, got {first:?}");
        };
        assert!(instances.is_empty(), "no instances registered yet");

        // ── Instance registers ─────────────────────────────────────────────
        let inst = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connect instance");
        let (_, mut iw) = tokio::io::split(inst);
        send_msg(
            &mut iw,
            &ClientMsg::Register {
                pid: std::process::id(),
                mode: "stdio".to_string(),
                scope: "/test/scope".to_string(),
                label: "TestInstance".to_string(),
                client: None,
                session_id: None,
                client_pid: None,
            },
        )
        .await
        .unwrap();

        // Subscriber receives InstanceRegistered.
        let reg: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        let DaemonMsg::InstanceRegistered { instance } = reg else {
            panic!("expected InstanceRegistered, got {reg:?}");
        };
        assert_eq!(instance.label, "TestInstance");
        assert_eq!(instance.mode, "stdio");
        let instance_id = instance.id.clone();

        // ── OpStarted event ────────────────────────────────────────────────
        send_msg(
            &mut iw,
            &ClientMsg::Event {
                payload: DaemonEvent::OpStarted {
                    id: "op-001".to_string(),
                    tool_name: "cargo_test".to_string(),
                    description: "Run tests".to_string(),
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

        let ev: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        match ev {
            DaemonMsg::Event {
                instance_id: iid,
                payload: DaemonEvent::OpStarted { id, tool_name, .. },
            } => {
                assert_eq!(iid, instance_id);
                assert_eq!(id, "op-001");
                assert_eq!(tool_name, "cargo_test");
            }
            other => panic!("expected Event::OpStarted, got {other:?}"),
        }

        // ── OpFinished event ───────────────────────────────────────────────
        send_msg(
            &mut iw,
            &ClientMsg::Event {
                payload: DaemonEvent::OpFinished {
                    id: "op-001".to_string(),
                    status: OpStatus::Completed,
                    result_summary: Some("success".to_string()),
                    duration_ms: 1200,
                    ended_epoch_ms: None,
                    exit_code: None,
                    denial: None,
                    interrupted: false,
                },
            },
        )
        .await
        .unwrap();

        let fin: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        match fin {
            DaemonMsg::Event {
                payload: DaemonEvent::OpFinished { id, status, .. },
                ..
            } => {
                assert_eq!(id, "op-001");
                assert_eq!(status, OpStatus::Completed);
            }
            other => panic!("expected Event::OpFinished, got {other:?}"),
        }

        // ── Unregister ─────────────────────────────────────────────────────
        send_msg(&mut iw, &ClientMsg::Unregister).await.unwrap();

        let unreg: DaemonMsg = recv_msg(&mut sub_reader).await.unwrap();
        match unreg {
            DaemonMsg::InstanceUnregistered { id } => assert_eq!(id, instance_id),
            other => panic!("expected InstanceUnregistered, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_list_instances_query() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("list.sock");
        let sock2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(sock2).await;
        });
        wait_for_daemon(&sock).await;

        // Register one instance.
        let inst = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (_, mut iw) = tokio::io::split(inst);
        send_msg(
            &mut iw,
            &ClientMsg::Register {
                pid: 1234,
                mode: "http".to_string(),
                scope: "/project".to_string(),
                label: "HttpBridge".to_string(),
                client: None,
                session_id: None,
                client_pid: None,
            },
        )
        .await
        .unwrap();

        // Give the daemon time to process the registration.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // One-shot ListInstances query.
        let q = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (qr, mut qw) = tokio::io::split(q);
        let mut qrdr = BufReader::new(qr);
        send_msg(&mut qw, &ClientMsg::ListInstances).await.unwrap();

        let resp: DaemonMsg = recv_msg(&mut qrdr).await.unwrap();
        match resp {
            DaemonMsg::InstanceList { instances } => {
                assert_eq!(instances.len(), 1, "expected 1 registered instance");
                assert_eq!(instances[0].label, "HttpBridge");
                assert_eq!(instances[0].mode, "http");
            }
            other => panic!("expected InstanceList, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_stale_socket_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("stale.sock");

        // Create a stale socket file: bind a listener then immediately drop it.
        // The file remains but nothing is listening.
        {
            let _listener = tokio::net::UnixListener::bind(&sock).unwrap();
        }
        assert!(
            sock.exists(),
            "stale socket file should exist before daemon starts"
        );

        // The daemon should detect ECONNREFUSED on the stale socket,
        // remove the file, and bind successfully.
        let sock2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(sock2).await;
        });
        wait_for_daemon(&sock).await;

        // Verify a fresh connection works after cleanup.
        let conn = tokio::net::UnixStream::connect(&sock).await;
        assert!(
            conn.is_ok(),
            "daemon should be running after stale socket cleanup"
        );
    }

    #[tokio::test]
    async fn op_history_replays_started_and_finished_in_order() {
        let (hub, _rx) = DaemonHub::new(None);

        // op-1 runs to completion; op-2 is still running.
        hub.record_op_event(
            "i1",
            &DaemonEvent::OpStarted {
                id: "op-1".into(),
                tool_name: "cargo_build".into(),
                description: "build".into(),
                scope: "/w".into(),
                parent_id: None,
                started_epoch_ms: None,
                title: None,
                cwd: None,
                command: None,
                origin: None,
                partial: false,
                unsandboxed: false,
            },
        )
        .await;
        hub.record_op_event(
            "i1",
            &DaemonEvent::OpFinished {
                id: "op-1".into(),
                status: OpStatus::Completed,
                result_summary: Some("ok".into()),
                duration_ms: 10,
                ended_epoch_ms: None,
                exit_code: None,
                denial: None,
                interrupted: false,
            },
        )
        .await;
        hub.record_op_event(
            "i1",
            &DaemonEvent::OpStarted {
                id: "op-2".into(),
                tool_name: "cargo_test".into(),
                description: "test".into(),
                scope: "/w".into(),
                parent_id: None,
                started_epoch_ms: None,
                title: None,
                cwd: None,
                command: None,
                origin: None,
                partial: false,
                unsandboxed: false,
            },
        )
        .await;

        let replay = hub.replay_events().await;
        // op-1 started+finished (2) + op-2 started (1) = 3, ordered by record seq.
        assert_eq!(
            replay.len(),
            3,
            "replay should carry all retained op events"
        );
        assert!(
            matches!(
                &replay[0],
                DaemonMsg::Event { instance_id, payload: DaemonEvent::OpStarted { id, .. } }
                    if instance_id == "i1" && id == "op-1"
            ),
            "first replayed event is op-1 OpStarted"
        );
        assert!(
            matches!(
                &replay[1],
                DaemonMsg::Event { payload: DaemonEvent::OpFinished { id, .. }, .. } if id == "op-1"
            ),
            "op-1 OpFinished follows its OpStarted"
        );
        assert!(
            matches!(
                &replay[2],
                DaemonMsg::Event { payload: DaemonEvent::OpStarted { id, .. }, .. } if id == "op-2"
            ),
            "still-running op-2 is replayed as OpStarted only"
        );
    }

    #[tokio::test]
    async fn op_history_drops_unregistered_instance_and_ignores_output() {
        let (hub, _rx) = DaemonHub::new(None);

        // Streaming output is a live tail — never retained for replay.
        hub.record_op_event(
            "i1",
            &DaemonEvent::OpOutput {
                id: "op-1".into(),
                line: "compiling…".into(),
                is_stderr: false,
            },
        )
        .await;
        assert!(
            hub.replay_events().await.is_empty(),
            "OpOutput must not be replayed"
        );

        hub.record_op_event(
            "i1",
            &DaemonEvent::OpStarted {
                id: "op-1".into(),
                tool_name: "t".into(),
                description: "d".into(),
                scope: "/w".into(),
                parent_id: None,
                started_epoch_ms: None,
                title: None,
                cwd: None,
                command: None,
                origin: None,
                partial: false,
                unsandboxed: false,
            },
        )
        .await;
        assert_eq!(hub.replay_events().await.len(), 1);

        // When an instance unregisters its history is dropped (mirrors the
        // handle_connection cleanup path).
        hub.op_history.lock().await.remove("i1");
        assert!(hub.replay_events().await.is_empty());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Added coverage tests
    // ─────────────────────────────────────────────────────────────────────────

    /// Serializes env-var mutation across the env-dependent unit tests below.
    static ENV_MUTEX: std::sync::LazyLock<parking_lot::Mutex<()>> =
        std::sync::LazyLock::new(|| parking_lot::Mutex::new(()));

    // ── daemon_port / default_socket_path (env-driven) ────────────────────────

    #[test]
    fn daemon_port_default_and_override() {
        let _g = ENV_MUTEX.lock();
        let prev = std::env::var_os("AHMA_DAEMON_PORT");
        let prev_iso = std::env::var_os("AHMA_TEST_ISOLATION");
        let prev_nextest = std::env::var_os("NEXTEST");

        // Valid override is honored.
        unsafe { std::env::set_var("AHMA_DAEMON_PORT", "54321") };
        assert_eq!(daemon_port(), 54321);

        // Unparseable value falls back: under a test harness (R-ISO.1) to a
        // stable per-run private port in the ephemeral range, never the live
        // daemon port.
        unsafe { std::env::set_var("AHMA_DAEMON_PORT", "not-a-port") };
        unsafe { std::env::set_var("NEXTEST", "1") };
        let port = daemon_port();
        assert_ne!(
            port, WINDOWS_DAEMON_PORT,
            "a test-harness process must not fall back to the live daemon port"
        );
        assert!(
            (49152..65152).contains(&port),
            "per-run port must be in the ephemeral range, got {port}"
        );
        assert_eq!(daemon_port(), port, "per-run port must be stable");

        // Outside any test harness the platform default applies.
        unsafe { std::env::remove_var("NEXTEST") };
        unsafe { std::env::remove_var("AHMA_TEST_ISOLATION") };
        assert_eq!(
            daemon_port(),
            WINDOWS_DAEMON_PORT,
            "invalid AHMA_DAEMON_PORT must fall back to the default in production"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("AHMA_DAEMON_PORT", v) },
            None => unsafe { std::env::remove_var("AHMA_DAEMON_PORT") },
        }
        match prev_iso {
            Some(v) => unsafe { std::env::set_var("AHMA_TEST_ISOLATION", v) },
            None => unsafe { std::env::remove_var("AHMA_TEST_ISOLATION") },
        }
        match prev_nextest {
            Some(v) => unsafe { std::env::set_var("NEXTEST", v) },
            None => unsafe { std::env::remove_var("NEXTEST") },
        }
    }

    /// R-ISO.1: a test-harness process without explicit daemon-socket isolation
    /// must still resolve a private per-run socket, never the live daemon's.
    #[test]
    fn default_socket_path_private_under_test_harness() {
        let _g = ENV_MUTEX.lock();
        if SOCKET_PATH_OVERRIDE.get().is_some() {
            return;
        }
        let prev_sock = std::env::var_os("AHMA_DAEMON_SOCK");
        let prev_nextest = std::env::var_os("NEXTEST");
        unsafe { std::env::remove_var("AHMA_DAEMON_SOCK") };
        unsafe { std::env::set_var("NEXTEST", "1") };

        let path = default_socket_path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("ahma-test-daemon-"),
            "test-harness fallback must be a private per-run socket, got {}",
            path.display()
        );
        assert!(
            path.starts_with(std::env::temp_dir()),
            "private socket must live in the temp dir, got {}",
            path.display()
        );

        match prev_sock {
            Some(v) => unsafe { std::env::set_var("AHMA_DAEMON_SOCK", v) },
            None => unsafe { std::env::remove_var("AHMA_DAEMON_SOCK") },
        }
        match prev_nextest {
            Some(v) => unsafe { std::env::set_var("NEXTEST", v) },
            None => unsafe { std::env::remove_var("NEXTEST") },
        }
    }

    /// R-ISO.1: the MCP endpoint must be per-run private under a harness, and
    /// must key off the *same* discriminator as the hub socket — a test whose
    /// frontend and daemon disagree about which run they belong to rendezvouses
    /// on nothing (or, worse, on the developer's live endpoint).
    #[test]
    fn mcp_socket_path_is_private_under_test_harness_and_shares_the_hub_discriminator() {
        let _g = ENV_MUTEX.lock();
        let prev_nextest = std::env::var_os("NEXTEST");
        unsafe { std::env::set_var("NEXTEST", "1") };

        let mcp = PathBuf::from(mcp_socket_path(None));
        let name = mcp.file_name().unwrap().to_string_lossy().into_owned();
        let disc = crate::test_isolation::test_run_discriminator();
        assert_eq!(
            name,
            format!("ahma-test-mcp-{disc}.sock"),
            "harness fallback must be a private per-run MCP socket"
        );
        assert!(
            mcp.starts_with(std::env::temp_dir()),
            "private MCP socket must live in the temp dir, got {}",
            mcp.display()
        );
        assert!(
            name.contains(&disc),
            "MCP socket must carry the same run discriminator the hub socket uses"
        );

        match prev_nextest {
            Some(v) => unsafe { std::env::set_var("NEXTEST", v) },
            None => unsafe { std::env::remove_var("NEXTEST") },
        }
    }

    #[test]
    fn mcp_socket_path_explicit_override_wins() {
        let _g = ENV_MUTEX.lock();
        assert_eq!(
            mcp_socket_path(Some("/run/custom/ahma.sock")),
            "/run/custom/ahma.sock",
            "an explicit --unix-socket-path is used verbatim"
        );
        // An empty string is "unset" throughout AppConfig, not a path.
        assert_ne!(mcp_socket_path(Some("")), "");
    }

    /// The two rendezvous files live side by side, so one `runtime_dir` check
    /// covers both (SPEC R-DAEMON.2).
    #[cfg(unix)]
    #[test]
    fn mcp_socket_path_lives_beside_the_hub_socket() {
        let hub = platform_default_socket_path();
        let mcp = platform_mcp_socket_path();
        assert_eq!(
            hub.parent(),
            mcp.parent(),
            "hub and MCP sockets must share the per-user runtime directory"
        );
        assert_eq!(mcp.file_name().unwrap(), "mcp.sock");
        assert_ne!(
            mcp.to_string_lossy(),
            "/tmp/ahma.sock",
            "the machine-global socket is retired"
        );
    }

    /// A runtime directory another local user can read or write is refused:
    /// a 0600 socket inside a 0777 directory is still squattable.
    #[cfg(unix)]
    #[test]
    fn runtime_dir_rejects_group_or_other_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        std::fs::create_dir(&dir).unwrap();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        verify_runtime_dir_secure(&dir).expect("0700 owned by us is acceptable");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = verify_runtime_dir_secure(&dir)
            .expect_err("group/other-readable runtime dir must be refused")
            .to_string();
        assert!(err.contains("chmod 700"), "error must name the fix: {err}");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let _g = ENV_MUTEX.lock();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let dir = runtime_dir().expect("XDG_RUNTIME_DIR yields a runtime dir");
        assert_eq!(dir, tmp.path().join("ahma"));
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "runtime dir must be created 0700, got {mode:o}"
        );
        verify_runtime_dir_secure(&dir).expect("freshly created dir passes its own check");

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    #[test]
    fn default_socket_path_returns_env_var_path() {
        let _g = ENV_MUTEX.lock();
        // A CLI override (process-wide OnceLock) takes precedence over the env
        // var; if some other test installed one, this assertion does not apply.
        if SOCKET_PATH_OVERRIDE.get().is_some() {
            return;
        }
        let prev = std::env::var_os("AHMA_DAEMON_SOCK");
        let want = std::env::temp_dir().join("ahma_dsp_unit_test.sock");
        unsafe { std::env::set_var("AHMA_DAEMON_SOCK", &want) };

        assert_eq!(
            default_socket_path(),
            want,
            "AHMA_DAEMON_SOCK should be returned verbatim in test builds"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("AHMA_DAEMON_SOCK", v) },
            None => unsafe { std::env::remove_var("AHMA_DAEMON_SOCK") },
        }
    }

    // ── record_op_event eviction / no-op branches ─────────────────────────────

    #[tokio::test]
    async fn record_op_event_evicts_oldest_finished_when_over_cap() {
        let (hub, _rx) = DaemonHub::new(None);

        // Fill to the cap with fully-finished ops.
        for i in 0..MAX_OPS_PER_INSTANCE {
            let id = format!("op-{i}");
            hub.record_op_event(
                "i1",
                &DaemonEvent::OpStarted {
                    id: id.clone(),
                    tool_name: "t".into(),
                    description: "d".into(),
                    scope: "/w".into(),
                    parent_id: None,
                    started_epoch_ms: None,
                    title: None,
                    cwd: None,
                    command: None,
                    origin: None,
                    partial: false,
                    unsandboxed: false,
                },
            )
            .await;
            hub.record_op_event(
                "i1",
                &DaemonEvent::OpFinished {
                    id,
                    status: OpStatus::Completed,
                    result_summary: None,
                    duration_ms: 1,
                    ended_epoch_ms: None,
                    exit_code: None,
                    denial: None,
                    interrupted: false,
                },
            )
            .await;
        }
        assert_eq!(
            hub.op_history.lock().await.get("i1").unwrap().len(),
            MAX_OPS_PER_INSTANCE
        );

        // One more started op pushes over the cap → oldest finished (op-0) evicted.
        hub.record_op_event(
            "i1",
            &DaemonEvent::OpStarted {
                id: "op-new".into(),
                tool_name: "t".into(),
                description: "d".into(),
                scope: "/w".into(),
                parent_id: None,
                started_epoch_ms: None,
                title: None,
                cwd: None,
                command: None,
                origin: None,
                partial: false,
                unsandboxed: false,
            },
        )
        .await;

        let hist = hub.op_history.lock().await;
        let inst = hist.get("i1").unwrap();
        assert_eq!(
            inst.len(),
            MAX_OPS_PER_INSTANCE,
            "cap maintained by eviction"
        );
        assert!(!inst.contains_key("op-0"), "oldest finished op is evicted");
        assert!(
            inst.contains_key("op-new"),
            "the new running op is retained"
        );
    }

    #[tokio::test]
    async fn record_op_event_keeps_all_when_none_finished_to_evict() {
        let (hub, _rx) = DaemonHub::new(None);

        // Insert cap+1 *running* ops — none are eligible for eviction, so the
        // history is allowed to exceed the cap (running ops are never dropped).
        for i in 0..=MAX_OPS_PER_INSTANCE {
            hub.record_op_event(
                "i1",
                &DaemonEvent::OpStarted {
                    id: format!("op-{i}"),
                    tool_name: "t".into(),
                    description: "d".into(),
                    scope: "/w".into(),
                    parent_id: None,
                    started_epoch_ms: None,
                    title: None,
                    cwd: None,
                    command: None,
                    origin: None,
                    partial: false,
                    unsandboxed: false,
                },
            )
            .await;
        }

        assert_eq!(
            hub.op_history.lock().await.get("i1").unwrap().len(),
            MAX_OPS_PER_INSTANCE + 1,
            "running ops are never evicted even past the cap"
        );
    }

    /// Superseded contract. A terminal event for an op the hub never saw used
    /// to be dropped; it is now reconstructed (see
    /// `finished_without_started_is_retained_as_partial`), because the outcome
    /// is real even when the preamble is gone. What must still hold: the
    /// reconstruction is a *separate* row and never corrupts a live one.
    #[tokio::test]
    async fn a_finish_for_another_op_never_terminates_the_running_one() {
        let (hub, _rx) = DaemonHub::new(None);
        hub.record_op_event("i1", &started_ev("real")).await;
        hub.record_op_event("i1", &finished_ev("other", Some(now_epoch_ms())))
            .await;

        let replay = hub.replay_events().await;
        let real_finished = replay.iter().any(|m| {
            matches!(m, DaemonMsg::Event { payload: DaemonEvent::OpFinished { id, .. }, .. } if id == "real")
        });
        assert!(
            !real_finished,
            "the running op must stay running: {replay:?}"
        );
        let other_rows = replay
            .iter()
            .filter(|m| {
                matches!(m, DaemonMsg::Event { payload, .. }
                    if op_id_of(payload).as_deref() == Some("other"))
            })
            .count();
        assert_eq!(
            other_rows, 2,
            "the orphan gets its own started+finished pair"
        );
    }

    // ── resolve_target ────────────────────────────────────────────────────────

    fn instance_with_mode(id: &str, mode: &str) -> InstanceInfo {
        InstanceInfo {
            id: id.into(),
            pid: 1,
            mode: mode.into(),
            scope: "/w".into(),
            label: "L".into(),
            client: None,
            session_id: None,
            client_pid: None,
            ended_epoch_ms: None,
        }
    }

    /// An explicit target must name a live instance. Returning it verbatim let
    /// a decision be routed at an instance that had already gone — or, worse,
    /// at an id another session had since been given.
    #[tokio::test]
    async fn resolve_target_explicit_must_name_a_live_instance() {
        let (hub, _rx) = DaemonHub::new(None);
        assert_eq!(
            resolve_target(&hub, Some("ghost")).await,
            None,
            "an id nobody holds resolves to nothing"
        );

        hub.instances
            .lock()
            .await
            .insert("live".into(), instance_with_mode("live", "stdio"));
        assert_eq!(
            resolve_target(&hub, Some("live")).await,
            Some("live".to_string())
        );
    }

    /// With one attached client, picking "the first instance" was right by
    /// construction. With three Claude Code windows it sent one window's answer
    /// to another window's question; the hub now refuses to guess.
    #[tokio::test]
    async fn resolve_target_refuses_to_guess_among_several_instances() {
        let (hub, _rx) = DaemonHub::new(None);
        {
            let mut instances = hub.instances.lock().await;
            instances.insert("a".into(), instance_with_mode("a", "stdio"));
            instances.insert("b".into(), instance_with_mode("b", "stdio"));
        }
        assert_eq!(
            resolve_target(&hub, None).await,
            None,
            "an untargeted request among several sessions must not be guessed at"
        );
    }

    /// Hooks and the TUI are not candidates for an untargeted request: a hook
    /// has no agent loop to run a prompt, and the TUI is the thing asking. With
    /// them excluded, one real session is still unambiguous.
    #[tokio::test]
    async fn resolve_target_ignores_hook_and_tui_instances() {
        let (hub, _rx) = DaemonHub::new(None);
        {
            let mut instances = hub.instances.lock().await;
            instances.insert("hook".into(), instance_with_mode("hook", "hook"));
            instances.insert("tui".into(), instance_with_mode("tui", "tui"));
            instances.insert("session".into(), instance_with_mode("session", "stdio"));
        }
        assert_eq!(
            resolve_target(&hub, None).await,
            Some("session".to_string())
        );
    }

    /// An answer goes back to the session that asked, not to whichever instance
    /// happens to be registered.
    #[tokio::test]
    async fn a_decision_routes_to_the_instance_that_raised_it() {
        let (hub, _rx) = DaemonHub::new(None);
        {
            let mut instances = hub.instances.lock().await;
            instances.insert("asker".into(), instance_with_mode("asker", "stdio"));
            instances.insert("other".into(), instance_with_mode("other", "stdio"));
        }
        hub.pending_decisions
            .lock()
            .await
            .insert("decision-1".into(), "asker".into());

        assert_eq!(
            instance_for_decision(&hub, "decision-1").await,
            Some("asker".to_string())
        );
        assert_eq!(
            instance_for_decision(&hub, "never-asked").await,
            None,
            "an unknown decision id must not fall back to an arbitrary instance"
        );
    }

    // ── retention: output tails, ended instances, window and cap ──────────────

    /// Helper: a minimal started event for retention tests.
    fn started_ev(id: &str) -> DaemonEvent {
        DaemonEvent::OpStarted {
            id: id.into(),
            tool_name: "run_terminal_command".into(),
            description: "d".into(),
            scope: "/w".into(),
            parent_id: None,
            started_epoch_ms: None,
            title: Some(format!("cmd {id}")),
            cwd: None,
            command: None,
            origin: None,
            partial: false,
            unsandboxed: false,
        }
    }

    /// Helper: a terminal event that ended `ago_ms` milliseconds ago.
    fn finished_ev(id: &str, ended_epoch_ms: Option<u64>) -> DaemonEvent {
        DaemonEvent::OpFinished {
            id: id.into(),
            status: OpStatus::Completed,
            result_summary: Some("ok".into()),
            duration_ms: 5,
            ended_epoch_ms,
            exit_code: Some(0),
            denial: None,
            interrupted: false,
        }
    }

    /// The retained output window is bounded: it is what a late subscriber
    /// needs to see, not a log (SPEC R-DAEMON.7).
    #[tokio::test]
    async fn op_output_tail_is_bounded_to_max_tail_lines() {
        let (hub, _rx) = DaemonHub::new(None);
        hub.record_op_event("i1", &started_ev("op-1")).await;
        for n in 0..(MAX_TAIL_LINES * 2) {
            hub.record_op_event(
                "i1",
                &DaemonEvent::OpOutput {
                    id: "op-1".into(),
                    line: format!("line {n}"),
                    is_stderr: false,
                },
            )
            .await;
        }

        let lines: Vec<String> = hub
            .replay_events()
            .await
            .into_iter()
            .filter_map(|m| match m {
                DaemonMsg::Event {
                    payload: DaemonEvent::OpOutput { line, .. },
                    ..
                } => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), MAX_TAIL_LINES, "the window is capped");
        assert_eq!(
            lines.last().unwrap(),
            &format!("line {}", MAX_TAIL_LINES * 2 - 1),
            "the newest line survives"
        );
        assert_eq!(
            lines.first().unwrap(),
            &format!("line {}", MAX_TAIL_LINES),
            "the oldest lines are the ones dropped"
        );
    }

    /// Replay is the same shape as the live stream — started, then output, then
    /// finished — so a subscriber needs no second code path for history.
    #[tokio::test]
    async fn replay_emits_started_then_tail_then_finished_in_order() {
        let (hub, _rx) = DaemonHub::new(None);
        hub.record_op_event("i1", &started_ev("op-1")).await;
        hub.record_op_event(
            "i1",
            &DaemonEvent::OpOutput {
                id: "op-1".into(),
                line: "compiling".into(),
                is_stderr: false,
            },
        )
        .await;
        hub.record_op_event("i1", &finished_ev("op-1", Some(now_epoch_ms())))
            .await;

        let kinds: Vec<&'static str> = hub
            .replay_events()
            .await
            .iter()
            .map(|m| match m {
                DaemonMsg::Event { payload, .. } => match payload {
                    DaemonEvent::OpStarted { .. } => "started",
                    DaemonEvent::OpOutput { .. } => "output",
                    DaemonEvent::OpFinished { .. } => "finished",
                    DaemonEvent::LogLine { .. } => "log",
                },
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["started", "output", "finished"]);
    }

    /// A hook is an instance for the length of one command. Dropping its
    /// history when it disconnected made hooked work permanently invisible:
    /// by the time anyone looked, the instance had always already gone.
    #[tokio::test]
    async fn history_survives_unregister_and_the_instance_is_listed_as_ended() {
        let (hub, _rx) = DaemonHub::new(None);
        let info = InstanceInfo {
            id: "i1".into(),
            pid: 7,
            mode: "hook".into(),
            scope: "/w".into(),
            label: "hook".into(),
            client: Some("claude-code".into()),
            session_id: None,
            client_pid: None,
            ended_epoch_ms: None,
        };
        hub.instances.lock().await.insert("i1".into(), info.clone());
        hub.record_op_event("i1", &started_ev("op-1")).await;
        hub.record_op_event("i1", &finished_ev("op-1", Some(now_epoch_ms())))
            .await;

        // Simulate the teardown serve_instance performs.
        let mut departed = hub.instances.lock().await.remove("i1").unwrap();
        departed.ended_epoch_ms = Some(now_epoch_ms());
        hub.ended_instances
            .lock()
            .await
            .insert("i1".into(), departed);
        hub.prune_history(now_epoch_ms()).await;

        let listed = hub.instance_snapshot().await;
        assert_eq!(listed.len(), 1, "the ended instance is still listed");
        assert!(
            listed[0].ended_epoch_ms.is_some(),
            "and is marked as ended, which is how a reader tells it apart"
        );
        assert_eq!(
            hub.replay_events().await.len(),
            2,
            "its operations still replay"
        );
    }

    /// Finished work ages out of the window; work still running never does,
    /// however long it takes.
    #[tokio::test]
    async fn replay_window_evicts_finished_ops_older_than_the_window() {
        let (hub, _rx) = DaemonHub::new(None);
        let now = now_epoch_ms();
        let long_ago = now - HISTORY_REPLAY_WINDOW.as_millis() as u64 - 60_000;

        hub.record_op_event("i1", &started_ev("old")).await;
        hub.record_op_event("i1", &finished_ev("old", Some(long_ago)))
            .await;
        hub.record_op_event("i1", &started_ev("recent")).await;
        hub.record_op_event("i1", &finished_ev("recent", Some(now)))
            .await;
        hub.record_op_event("i1", &started_ev("still-running"))
            .await;

        hub.prune_history(now).await;

        let ids: Vec<String> = hub
            .replay_events()
            .await
            .iter()
            .filter_map(|m| match m {
                DaemonMsg::Event {
                    payload: DaemonEvent::OpStarted { id, .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert!(!ids.contains(&"old".to_string()), "aged out: {ids:?}");
        assert!(ids.contains(&"recent".to_string()), "kept: {ids:?}");
        assert!(
            ids.contains(&"still-running".to_string()),
            "a running op is never pruned by age: {ids:?}"
        );
    }

    /// The per-instance cap is unbounded in the number of instances, and a hook
    /// registers one per hooked command; the global ceiling is what actually
    /// bounds the daemon's memory.
    #[tokio::test]
    async fn global_retention_cap_evicts_oldest_finished_first() {
        let (hub, _rx) = DaemonHub::new(None);
        let now = now_epoch_ms();
        for i in 0..(MAX_RETAINED_OPS + 50) {
            let inst = format!("hook-{i}");
            let op = format!("op-{i}");
            hub.record_op_event(&inst, &started_ev(&op)).await;
            hub.record_op_event(&inst, &finished_ev(&op, Some(now)))
                .await;
        }
        hub.prune_history(now).await;

        let retained: usize = hub.op_history.lock().await.values().map(|o| o.len()).sum();
        assert!(
            retained <= MAX_RETAINED_OPS,
            "retention must be bounded across instances, kept {retained}"
        );
        let ids: Vec<String> = hub
            .replay_events()
            .await
            .iter()
            .filter_map(|m| match m {
                DaemonMsg::Event {
                    payload: DaemonEvent::OpStarted { id, .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert!(
            ids.contains(&format!("op-{}", MAX_RETAINED_OPS + 49)),
            "the newest work is what survives"
        );
        assert!(!ids.contains(&"op-0".to_string()), "the oldest is evicted");
    }

    /// An operation whose start the hub never saw still has a real outcome. It
    /// is reconstructed and flagged, rather than dropped — which used to lose
    /// exactly the work a user is most likely to ask about.
    #[tokio::test]
    async fn finished_without_started_is_retained_as_partial() {
        let (hub, _rx) = DaemonHub::new(None);
        let now = now_epoch_ms();
        hub.record_op_event("i1", &finished_ev("orphan", Some(now)))
            .await;

        let replay = hub.replay_events().await;
        let started = replay
            .iter()
            .find_map(|m| match m {
                DaemonMsg::Event {
                    payload: payload @ DaemonEvent::OpStarted { .. },
                    ..
                } => Some(payload.clone()),
                _ => None,
            })
            .expect("a reconstructed start record");
        match started {
            DaemonEvent::OpStarted {
                id,
                partial,
                title,
                started_epoch_ms,
                ..
            } => {
                assert_eq!(id, "orphan");
                assert!(partial, "the row must admit it was reconstructed");
                assert_eq!(title.as_deref(), Some("ok"), "outcome text is all we have");
                assert_eq!(
                    started_epoch_ms,
                    Some(now - 5),
                    "start derived from end minus duration"
                );
            }
            other => panic!("expected OpStarted, got {other:?}"),
        }
        assert!(
            replay.iter().any(|m| matches!(
                m,
                DaemonMsg::Event {
                    payload: DaemonEvent::OpFinished { .. },
                    ..
                }
            )),
            "and its outcome still replays"
        );
    }

    /// The daemon exits when idle, so without a file "what ran twenty minutes
    /// ago" is answerable only while the process that saw it happen is still
    /// alive. A fresh daemon must restore the window from disk — including the
    /// output tail, and including operations that were still running when the
    /// previous daemon went away (SPEC R-DAEMON.7).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fresh_hub_replays_the_last_hour_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let history = tmp.path().join("history.jsonl");

        // ── Daemon 1: does some work, then goes away mid-operation ───────────
        {
            let server = HubServer::bind_at(tmp.path().join("first.sock"))
                .await
                .expect("bind");
            let writer = server
                .attach_history(Some(history.clone()))
                .await
                .expect("history writer");
            let hub = server.hub.clone();
            hub.instances.lock().await.insert(
                "i1".into(),
                InstanceInfo {
                    id: "i1".into(),
                    pid: 7,
                    mode: "stdio".into(),
                    scope: "/ws".into(),
                    label: "ahma".into(),
                    client: Some("claude-code".into()),
                    session_id: Some("sess-1".into()),
                    client_pid: Some(11),
                    ended_epoch_ms: None,
                },
            );
            hub.record_op_event("i1", &started_ev("done")).await;
            hub.record_op_event(
                "i1",
                &DaemonEvent::OpOutput {
                    id: "done".into(),
                    line: "Compiling ahma_core".into(),
                    is_stderr: false,
                },
            )
            .await;
            hub.record_op_event("i1", &finished_ev("done", Some(now_epoch_ms())))
                .await;
            // ...and one that never finishes: the daemon dies under it.
            hub.record_op_event("i1", &started_ev("in-flight")).await;
            writer.flush().await;
        }

        // ── Daemon 2: a fresh process, nothing attached ──────────────────────
        let server = HubServer::bind_at(tmp.path().join("second.sock"))
            .await
            .expect("bind");
        server
            .attach_history(Some(history.clone()))
            .await
            .expect("history writer");

        let listed = server.hub.instance_snapshot().await;
        assert_eq!(listed.len(), 1, "the instance is restored: {listed:?}");
        assert_eq!(listed[0].client.as_deref(), Some("claude-code"));
        assert!(
            listed[0].ended_epoch_ms.is_some(),
            "restored instances are historic, not attached"
        );

        let replay = server.hub.replay_events().await;
        let output: Vec<String> = replay
            .iter()
            .filter_map(|m| match m {
                DaemonMsg::Event {
                    payload: DaemonEvent::OpOutput { line, .. },
                    ..
                } => Some(line.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            output,
            vec!["Compiling ahma_core".to_string()],
            "the output window survives the restart"
        );

        let interrupted: Vec<(String, bool)> = replay
            .iter()
            .filter_map(|m| match m {
                DaemonMsg::Event {
                    payload:
                        DaemonEvent::OpFinished {
                            id, interrupted, ..
                        },
                    ..
                } => Some((id.clone(), *interrupted)),
                _ => None,
            })
            .collect();
        assert!(
            interrupted.contains(&("done".to_string(), false)),
            "a completed op replays as completed: {interrupted:?}"
        );
        assert!(
            interrupted.contains(&("in-flight".to_string(), true)),
            "an op still running when the daemon died is closed as interrupted, \
             neither left spinning forever nor called failed: {interrupted:?}"
        );
    }

    /// A message a build does not understand is skipped, not fatal.
    ///
    /// This socket carries no version (R24.5), so a daemon left running across
    /// an upgrade is the reader that decides — and it used to decide by
    /// dropping the connection, which made every future message addition a hard
    /// incompatibility. Skipping is the property that has to ship *before* any
    /// new variant can.
    #[tokio::test]
    async fn an_unknown_message_type_is_skipped_and_the_next_one_still_arrives() {
        let wire = concat!(
            "{\"type\":\"SomethingFromTheFuture\",\"payload\":42}\n",
            "{\"type\":\"Subscribe\"}\n"
        );
        let mut reader = BufReader::new(wire.as_bytes());
        let msg: ClientMsg = recv_msg(&mut reader)
            .await
            .expect("the unknown line must not fail the connection");
        assert!(
            matches!(msg, ClientMsg::Subscribe),
            "the next understood message is delivered, got {msg:?}"
        );
    }

    /// Tolerance is for unknown *messages*, not for a broken stream: malformed
    /// JSON is still an error, or a desynchronised connection would spin
    /// forever pretending to make progress.
    #[tokio::test]
    async fn malformed_json_is_still_an_error() {
        let mut reader = BufReader::new(&b"{not json at all\n"[..]);
        assert!(
            recv_msg::<_, ClientMsg>(&mut reader).await.is_err(),
            "a broken line is a protocol error, not something to skip"
        );
    }

    /// A subscriber that falls far enough behind drops messages — possibly an
    /// `OpFinished`, which leaves a spinner that never resolves. The hub
    /// repairs the gap by re-sending the authoritative state.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_lagging_subscriber_is_resynchronised_rather_than_left_with_a_hole() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("lag.sock");
        let server = HubServer::bind_at(sock.clone()).await.expect("bind");
        let hub = server.hub.clone();
        tokio::spawn(server.serve());

        // An operation that ran and finished before anyone subscribed.
        hub.instances.lock().await.insert(
            "i1".into(),
            InstanceInfo {
                id: "i1".into(),
                pid: 7,
                mode: "stdio".into(),
                scope: "/ws".into(),
                label: "ahma".into(),
                client: None,
                session_id: None,
                client_pid: None,
                ended_epoch_ms: None,
            },
        );
        hub.record_op_event("i1", &started_ev("op-1")).await;
        hub.record_op_event("i1", &finished_ev("op-1", Some(now_epoch_ms())))
            .await;

        // A resync sends exactly what a fresh subscriber would get.
        let mut buf: Vec<u8> = Vec::new();
        resync_subscriber(&mut buf, &hub)
            .await
            .expect("resync writes");
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("\"type\":\"InstanceList\""),
            "resync re-sends the instance list: {text}"
        );
        assert!(
            text.contains("\"kind\":\"OpFinished\""),
            "and the terminal event the subscriber may have missed: {text}"
        );
    }

    // ── HubServer bind / already-running / stale ──────────────────────────────

    /// A live hub is never stolen: the loser of a startup race is told so and
    /// connects to the winner instead of unlinking a socket in use
    /// (SPEC R-DAEMON.1, R-ISO.2).
    #[cfg(unix)]
    #[tokio::test]
    async fn bind_hub_reports_already_running_when_a_live_hub_owns_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("embed.sock");
        let first = HubServer::bind_at(sock.clone())
            .await
            .expect("first bind should succeed on a fresh socket");
        let count = first.connection_count();
        tokio::spawn(first.serve());

        match HubServer::bind_at(sock.clone()).await {
            Err(HubBindError::AlreadyRunning) => {}
            Err(HubBindError::Failed(e)) => panic!("expected AlreadyRunning, got failure: {e}"),
            Ok(_) => panic!("second bind on a live socket must not succeed"),
        }
        assert!(
            sock.exists(),
            "the live hub's socket must survive a losing bind attempt"
        );
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "probe is not a connection"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_hub_removes_a_stale_socket_and_binds() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("stale_embed.sock");

        // Leave a socket file behind with nothing listening on it — what a
        // crash or a reboot leaves on a filesystem that is not tmpfs.
        {
            let _l = tokio::net::UnixListener::bind(&sock).unwrap();
        }
        assert!(sock.exists());

        let server = HubServer::bind_at(sock.clone())
            .await
            .expect("a stale socket is cleaned up and bound");
        assert_eq!(server.socket_path(), sock.as_path());
    }

    /// `Shutdown` must not shortcut a composed daemon's shutdown: with a hook
    /// installed the hub delegates instead of calling `process::exit`.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_message_invokes_exit_hook_instead_of_exiting() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("hook.sock");
        let server = HubServer::bind_at(sock.clone()).await.expect("bind");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        server.set_exit_hook(Arc::new(move |reason: &str| {
            let _ = tx.send(reason.to_string());
        }));
        tokio::spawn(server.serve());

        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        send_msg(&mut client, &ClientMsg::Shutdown).await.unwrap();

        let reason = tokio::time::timeout(crate::timeouts::TestTimeouts::scale_secs(5), rx.recv())
            .await
            .expect("exit hook must run instead of exiting the process")
            .expect("hook sends its reason");
        assert!(
            reason.contains("shutdown"),
            "hook is told why it was called: {reason}"
        );
        // The process is still alive, which is the whole point of the hook.
        assert!(sock.exists());
    }

    /// An instance re-registers whenever it learns its client's name or commits
    /// its sandbox scope. With a fresh id each time, a TUI saw one instance
    /// leave and a stranger arrive — losing the section's expansion state and
    /// reshuffling any view sorted by id. The session id keeps it the same
    /// instance (SPEC R-DAEMON.6).
    #[cfg(unix)]
    #[tokio::test]
    async fn reregister_with_the_same_session_keeps_the_instance_id() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stable.sock");
        let server = HubServer::bind_at(sock.clone()).await.expect("bind");
        tokio::spawn(server.serve());

        async fn register_once(sock: &std::path::Path, client: Option<&str>) -> String {
            let stream = tokio::net::UnixStream::connect(sock).await.unwrap();
            let (r, mut w) = tokio::io::split(stream);
            let reader = BufReader::new(r);
            send_msg(
                &mut w,
                &ClientMsg::Register {
                    pid: 11,
                    mode: "stdio".into(),
                    scope: "/ws".into(),
                    label: "ahma".into(),
                    client: client.map(str::to_string),
                    session_id: Some("mcp-session-7".into()),
                    client_pid: Some(4242),
                },
            )
            .await
            .unwrap();
            // Observe the registration from a subscriber's point of view.
            let sub = tokio::net::UnixStream::connect(sock).await.unwrap();
            let (sr, mut sw) = tokio::io::split(sub);
            let mut sub_reader = BufReader::new(sr);
            send_msg(&mut sw, &ClientMsg::ListInstances).await.unwrap();
            let id = match recv_msg::<_, DaemonMsg>(&mut sub_reader).await.unwrap() {
                DaemonMsg::InstanceList { instances } => {
                    let live: Vec<_> = instances
                        .iter()
                        .filter(|i| i.ended_epoch_ms.is_none())
                        .collect();
                    assert_eq!(live.len(), 1, "one session is one instance: {instances:?}");
                    assert_eq!(live[0].session_id.as_deref(), Some("mcp-session-7"));
                    assert_eq!(live[0].client_pid, Some(4242));
                    live[0].id.clone()
                }
                other => panic!("expected InstanceList, got {other:?}"),
            };
            // Drop the instance connection so the next registration is a
            // genuine reconnect-to-relabel.
            drop(w);
            drop(reader);
            id
        }

        let first = register_once(&sock, None).await;
        // Wait for the hub to finish tearing the first connection down.
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
            let (r, mut w) = tokio::io::split(stream);
            let mut reader = BufReader::new(r);
            send_msg(&mut w, &ClientMsg::ListInstances).await.unwrap();
            if let DaemonMsg::InstanceList { instances } =
                recv_msg::<_, DaemonMsg>(&mut reader).await.unwrap()
                && instances.iter().all(|i| i.ended_epoch_ms.is_some())
            {
                break;
            }
        }
        let second = register_once(&sock, Some("claude-code")).await;

        assert_eq!(
            first, second,
            "the same session must keep its instance id across a re-register"
        );
    }

    // ── serve_instance event forwarding ───────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_forwards_instance_events_to_subscriber() {
        use crate::scope_grant::{GrantReason, ScopeGrantRequest};

        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("events.sock");
        let s2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(s2).await;
        });
        wait_for_daemon(&sock).await;

        // Subscriber connects first.
        let sub = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (sr, mut sw) = tokio::io::split(sub);
        let mut srdr = BufReader::new(sr);
        send_msg(&mut sw, &ClientMsg::Subscribe).await.unwrap();
        assert!(matches!(
            recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap(),
            DaemonMsg::InstanceList { .. }
        ));

        // Instance registers.
        let inst = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (_ir, mut iw) = tokio::io::split(inst);
        send_msg(
            &mut iw,
            &ClientMsg::Register {
                pid: 9,
                mode: "stdio".into(),
                scope: "/w".into(),
                label: "L".into(),
                client: None,
                session_id: None,
                client_pid: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap(),
            DaemonMsg::InstanceRegistered { .. }
        ));

        // ChatToken → ChatToken.
        send_msg(
            &mut iw,
            &ClientMsg::Relay(HubRelay::ChatToken {
                token: "tok".into(),
            }),
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::ChatToken { token }) => assert_eq!(token, "tok"),
            other => panic!("expected ChatToken, got {other:?}"),
        }

        // ApprovalRequested → ApprovalRequested.
        send_msg(
            &mut iw,
            &ClientMsg::Relay(HubRelay::ApprovalRequested {
                id: "c1".into(),
                tool: "sh".into(),
                args: "ls".into(),
            }),
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::ApprovalRequested { id, tool, args }) => {
                assert_eq!(id, "c1");
                assert_eq!(tool, "sh");
                assert_eq!(args, "ls");
            }
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }

        // ScopeGrantRequested → ScopeGrantRequested.
        let req = ScopeGrantRequest {
            decision_id: "d9".into(),
            path: std::path::PathBuf::from("/opt/x"),
            access: crate::config::ScopeAccess::Rw,
            reason: GrantReason::StderrHeuristic,
            tool: Some("sccache".into()),
        };
        send_msg(
            &mut iw,
            &ClientMsg::Relay(HubRelay::ScopeGrantRequested { request: req }),
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::ScopeGrantRequested { request }) => {
                assert_eq!(request.decision_id, "d9")
            }
            other => panic!("expected ScopeGrantRequested, got {other:?}"),
        }

        // ScopeGrantResolved → ScopeGrantDismiss.
        send_msg(
            &mut iw,
            &ClientMsg::ScopeGrantResolved {
                decision_id: "d9".into(),
            },
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::ScopeGrantDismiss { decision_id } => assert_eq!(decision_id, "d9"),
            other => panic!("expected ScopeGrantDismiss, got {other:?}"),
        }

        // ToolCallStarted → ToolCallStarted.
        send_msg(
            &mut iw,
            &ClientMsg::Relay(HubRelay::ToolCallStarted {
                id: "t1".into(),
                name: "read_file".into(),
                args: "{}".into(),
            }),
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::ToolCallStarted { id, name, .. }) => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }

        // ToolCallFinished → ToolCallFinished.
        send_msg(
            &mut iw,
            &ClientMsg::Relay(HubRelay::ToolCallFinished {
                id: "t1".into(),
                result: "ok".into(),
                failed: false,
            }),
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::ToolCallFinished { id, failed, .. }) => {
                assert_eq!(id, "t1");
                assert!(!failed);
            }
            other => panic!("expected ToolCallFinished, got {other:?}"),
        }

        // Usage → Usage.
        send_msg(
            &mut iw,
            &ClientMsg::Relay(HubRelay::Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
            }),
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::Usage {
                prompt_tokens,
                total_tokens,
                ..
            }) => {
                assert_eq!(prompt_tokens, 100);
                assert_eq!(total_tokens, 120);
            }
            other => panic!("expected Usage, got {other:?}"),
        }

        // Pong produces NO broadcast; the next AgentDone proves it was swallowed.
        send_msg(&mut iw, &ClientMsg::Pong { seq: 5 })
            .await
            .unwrap();
        send_msg(&mut iw, &ClientMsg::Relay(HubRelay::AgentDone))
            .await
            .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::AgentDone) => {}
            other => panic!("expected AgentDone (Pong must not broadcast), got {other:?}"),
        }

        // AgentError → AgentError.
        send_msg(
            &mut iw,
            &ClientMsg::Relay(HubRelay::AgentError {
                error: "boom".into(),
            }),
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Relay(HubRelay::AgentError { error }) => assert_eq!(error, "boom"),
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    // ── handle_connection routing arms ────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_routes_tui_requests_to_instance() {
        use crate::scope_grant::GrantDecision;

        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("route.sock");
        let s2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(s2).await;
        });
        wait_for_daemon(&sock).await;

        // Instance registers and keeps its connection to receive routed messages.
        let inst = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (ir, mut iw) = tokio::io::split(inst);
        let mut irdr = BufReader::new(ir);
        send_msg(
            &mut iw,
            &ClientMsg::Register {
                pid: 7,
                mode: "stdio".into(),
                scope: "/w".into(),
                label: "L".into(),
                client: None,
                session_id: None,
                client_pid: None,
            },
        )
        .await
        .unwrap();
        // Let the registration land in instance_txs before routing to it.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // SubmitPrompt with no explicit target → routed (delivered) to the only instance.
        let mut tui = tokio::net::UnixStream::connect(&sock).await.unwrap();
        send_msg(
            &mut tui,
            &ClientMsg::SubmitPrompt {
                messages: vec![DaemonChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                system_prompt: Some("sys".into()),
                provider: Some("p".into()),
                model: Some("m".into()),
                target_instance_id: None,
            },
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut irdr).await.unwrap() {
            DaemonMsg::RunPrompt {
                messages,
                system_prompt,
                provider,
                model,
            } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(system_prompt.as_deref(), Some("sys"));
                assert_eq!(provider.as_deref(), Some("p"));
                assert_eq!(model.as_deref(), Some("m"));
            }
            other => panic!("expected RunPrompt, got {other:?}"),
        }

        // SubmitApproval routed.
        let mut tui2 = tokio::net::UnixStream::connect(&sock).await.unwrap();
        send_msg(
            &mut tui2,
            &ClientMsg::SubmitApproval {
                id: None,
                approved: true,
                target_instance_id: None,
            },
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut irdr).await.unwrap() {
            DaemonMsg::SubmitApproval { id: _, approved } => assert!(approved),
            other => panic!("expected SubmitApproval, got {other:?}"),
        }

        // SubmitScopeGrant routed.
        let mut tui3 = tokio::net::UnixStream::connect(&sock).await.unwrap();
        send_msg(
            &mut tui3,
            &ClientMsg::SubmitScopeGrant {
                decision_id: "d1".into(),
                decision: GrantDecision::GrantRo,
                target_instance_id: None,
            },
        )
        .await
        .unwrap();
        match recv_msg::<_, DaemonMsg>(&mut irdr).await.unwrap() {
            DaemonMsg::SubmitScopeGrant {
                decision_id,
                decision,
            } => {
                assert_eq!(decision_id, "d1");
                assert!(matches!(decision, GrantDecision::GrantRo));
            }
            other => panic!("expected SubmitScopeGrant, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_unexpected_first_message_closes_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("unexpected.sock");
        let s2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(s2).await;
        });
        wait_for_daemon(&sock).await;

        // Pong is not a valid first/role message → server hits the `_` arm and closes.
        let client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (cr, mut cw) = tokio::io::split(client);
        let mut crdr = BufReader::new(cr);
        send_msg(&mut cw, &ClientMsg::Pong { seq: 1 })
            .await
            .unwrap();

        let mut line = String::new();
        let n = tokio::time::timeout(TestTimeouts::scale_secs(2), crdr.read_line(&mut line))
            .await
            .expect("read should complete promptly")
            .unwrap();
        assert_eq!(
            n, 0,
            "server should close the connection after an unexpected first message"
        );
    }

    // ── serve_subscriber replay path ──────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_replays_history_to_late_subscriber() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("replay.sock");
        let s2 = sock.clone();
        tokio::spawn(async move {
            let _ = run_daemon_at(s2).await;
        });
        wait_for_daemon(&sock).await;

        // Instance registers and runs one op to completion BEFORE any subscriber.
        let inst = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (_ir, mut iw) = tokio::io::split(inst);
        send_msg(
            &mut iw,
            &ClientMsg::Register {
                pid: 3,
                mode: "stdio".into(),
                scope: "/w".into(),
                label: "L".into(),
                client: None,
                session_id: None,
                client_pid: None,
            },
        )
        .await
        .unwrap();
        send_msg(
            &mut iw,
            &ClientMsg::Event {
                payload: DaemonEvent::OpStarted {
                    id: "op-A".into(),
                    tool_name: "cargo_build".into(),
                    description: "b".into(),
                    scope: "/w".into(),
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
        send_msg(
            &mut iw,
            &ClientMsg::Event {
                payload: DaemonEvent::OpFinished {
                    id: "op-A".into(),
                    status: OpStatus::Completed,
                    result_summary: Some("ok".into()),
                    duration_ms: 5,
                    ended_epoch_ms: None,
                    exit_code: None,
                    denial: None,
                    interrupted: false,
                },
            },
        )
        .await
        .unwrap();
        // Allow the daemon to register the instance and record both events.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // A late subscriber must receive the instance list AND the replayed history.
        let sub = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (sr, mut sw) = tokio::io::split(sub);
        let mut srdr = BufReader::new(sr);
        send_msg(&mut sw, &ClientMsg::Subscribe).await.unwrap();

        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::InstanceList { instances } => assert_eq!(instances.len(), 1),
            other => panic!("expected InstanceList, got {other:?}"),
        }
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Event {
                payload: DaemonEvent::OpStarted { id, .. },
                ..
            } => assert_eq!(id, "op-A"),
            other => panic!("expected replayed OpStarted, got {other:?}"),
        }
        match recv_msg::<_, DaemonMsg>(&mut srdr).await.unwrap() {
            DaemonMsg::Event {
                payload: DaemonEvent::OpFinished { id, status, .. },
                ..
            } => {
                assert_eq!(id, "op-A");
                assert_eq!(status, OpStatus::Completed);
            }
            other => panic!("expected replayed OpFinished, got {other:?}"),
        }
    }
}

/// Golden wire bytes for the messages the hub relays verbatim.
///
/// The hub socket carries **no protocol version**: R24.5 lets it evolve by
/// adding fields, which is why a daemon and an instance from different builds
/// still understand each other. Restructuring the Rust types is therefore only
/// safe while the JSON stays identical, and "identical" is not something a
/// round-trip test can check — a round-trip passes just as happily after the
/// tag changes, because both ends changed together. These assertions pin the
/// actual bytes, so a refactor that would strand a running daemon fails here.
#[cfg(test)]
mod relay_wire_compat {
    use super::*;
    use serde_json::json;

    /// The same payload must appear on the wire whether it is travelling
    /// instance → hub ([`ClientMsg`]) or hub → TUI ([`DaemonMsg`]). The hub
    /// forwards these untouched, so any asymmetry would be a bug in itself.
    fn assert_same_bytes_both_directions(
        client: ClientMsg,
        daemon: DaemonMsg,
        expected: serde_json::Value,
    ) {
        assert_eq!(
            serde_json::to_value(&client).unwrap(),
            expected,
            "ClientMsg wire changed"
        );
        assert_eq!(
            serde_json::to_value(&daemon).unwrap(),
            expected,
            "DaemonMsg wire changed"
        );
        assert_one_tag(&serde_json::to_string(&client).unwrap());
        assert_one_tag(&serde_json::to_string(&daemon).unwrap());
    }

    /// Comparing `to_value` is not enough on its own.
    ///
    /// A nested envelope emits the tag twice — `{"type":"Relay","type":"…"}` —
    /// and `serde_json::Value` is a map, so the second key overwrites the first
    /// and the comparison above passes on a payload no other reader would
    /// accept. Only the string still has both. Confirmed by removing
    /// `#[serde(untagged)]` and watching this assertion be the one that fires.
    fn assert_one_tag(json: &str) {
        assert!(
            json.starts_with(r#"{"type":"#),
            "the tag must lead the object: {json}"
        );
        assert_eq!(
            json.matches(r#""type":"#).count(),
            1,
            "exactly one tag belongs on the wire: {json}"
        );
    }

    #[test]
    fn chat_tokens_keep_their_wire_bytes() {
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::ChatToken { token: "hi".into() }),
            DaemonMsg::Relay(HubRelay::ChatToken { token: "hi".into() }),
            json!({"type": "ChatToken", "token": "hi"}),
        );
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::ChatThinking { token: "mm".into() }),
            DaemonMsg::Relay(HubRelay::ChatThinking { token: "mm".into() }),
            json!({"type": "ChatThinking", "token": "mm"}),
        );
    }

    #[test]
    fn agent_lifecycle_keeps_its_wire_bytes() {
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::AgentDone),
            DaemonMsg::Relay(HubRelay::AgentDone),
            json!({"type": "AgentDone"}),
        );
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::AgentError {
                error: "boom".into(),
            }),
            DaemonMsg::Relay(HubRelay::AgentError {
                error: "boom".into(),
            }),
            json!({"type": "AgentError", "error": "boom"}),
        );
    }

    #[test]
    fn tool_calls_and_usage_keep_their_wire_bytes() {
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::ToolCallStarted {
                id: "c1".into(),
                name: "cargo".into(),
                args: "{}".into(),
            }),
            DaemonMsg::Relay(HubRelay::ToolCallStarted {
                id: "c1".into(),
                name: "cargo".into(),
                args: "{}".into(),
            }),
            json!({"type": "ToolCallStarted", "id": "c1", "name": "cargo", "args": "{}"}),
        );
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::ToolCallFinished {
                id: "c1".into(),
                result: "ok".into(),
                failed: false,
            }),
            DaemonMsg::Relay(HubRelay::ToolCallFinished {
                id: "c1".into(),
                result: "ok".into(),
                failed: false,
            }),
            json!({"type": "ToolCallFinished", "id": "c1", "result": "ok", "failed": false}),
        );
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
            }),
            DaemonMsg::Relay(HubRelay::Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
            }),
            json!({"type": "Usage", "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}),
        );
    }

    #[test]
    fn approval_prompts_keep_their_wire_bytes() {
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::ApprovalRequested {
                id: "a1".into(),
                tool: "rm".into(),
                args: "-rf".into(),
            }),
            DaemonMsg::Relay(HubRelay::ApprovalRequested {
                id: "a1".into(),
                tool: "rm".into(),
                args: "-rf".into(),
            }),
            json!({"type": "ApprovalRequested", "id": "a1", "tool": "rm", "args": "-rf"}),
        );

        let scope = crate::scope_grant::ScopeGrantRequest {
            decision_id: "d1".into(),
            path: std::path::PathBuf::from("/ext"),
            access: crate::config::ScopeAccess::Rw,
            reason: crate::scope_grant::GrantReason::StderrHeuristic,
            tool: Some("sccache".into()),
        };
        let expected_scope = serde_json::to_value(&scope).unwrap();
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::ScopeGrantRequested {
                request: scope.clone(),
            }),
            DaemonMsg::Relay(HubRelay::ScopeGrantRequested { request: scope }),
            json!({"type": "ScopeGrantRequested", "request": expected_scope}),
        );

        let web = crate::web_approval::WebApprovalRequest {
            decision_id: "w1".into(),
            domain: "example.com".into(),
            url: "https://example.com".into(),
            tool: None,
        };
        let expected_web = serde_json::to_value(&web).unwrap();
        assert_same_bytes_both_directions(
            ClientMsg::Relay(HubRelay::WebApprovalRequested {
                request: web.clone(),
            }),
            DaemonMsg::Relay(HubRelay::WebApprovalRequested { request: web }),
            json!({"type": "WebApprovalRequested", "request": expected_web}),
        );
    }

    /// The compatibility claim stated directly: a build from *before* the relay
    /// variants were collapsed must still read what this build writes, and this
    /// build must still read what it writes.
    ///
    /// `LegacyDaemonMsg` is that older reader — the flat shape, transcribed. It
    /// is deliberately a separate declaration rather than a reference to
    /// [`DaemonMsg`]: a test that reuses the live type cannot fail, because the
    /// live type moves with the code. The daemon left running across an upgrade
    /// does not.
    #[test]
    fn a_daemon_from_before_the_collapse_still_reads_and_writes_these() {
        #[derive(Debug, Serialize, Deserialize)]
        #[serde(tag = "type")]
        enum LegacyDaemonMsg {
            ChatToken {
                token: String,
            },
            AgentDone,
            ToolCallFinished {
                id: String,
                result: String,
                failed: bool,
            },
            /// A variant that was never part of the relay set, to show the two
            /// shapes still coexist in one stream.
            ScopeGrantDismiss {
                decision_id: String,
            },
        }

        // New writes → old reads.
        for (new, expect) in [
            (
                DaemonMsg::Relay(HubRelay::ChatToken { token: "hi".into() }),
                r#"ChatToken { token: "hi" }"#,
            ),
            (DaemonMsg::Relay(HubRelay::AgentDone), "AgentDone"),
            (
                DaemonMsg::ScopeGrantDismiss {
                    decision_id: "d1".into(),
                },
                r#"ScopeGrantDismiss { decision_id: "d1" }"#,
            ),
        ] {
            let json = serde_json::to_string(&new).unwrap();
            let old: LegacyDaemonMsg = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("a pre-collapse daemon could not read {json}: {e}"));
            assert_eq!(format!("{old:?}"), expect);
        }

        // Old writes → new reads.
        let legacy = serde_json::to_string(&LegacyDaemonMsg::ToolCallFinished {
            id: "c1".into(),
            result: "ok".into(),
            failed: true,
        })
        .unwrap();
        let now: DaemonMsg = serde_json::from_str(&legacy)
            .unwrap_or_else(|e| panic!("this build could not read a pre-collapse daemon: {e}"));
        assert!(matches!(
            now,
            DaemonMsg::Relay(HubRelay::ToolCallFinished { failed: true, .. })
        ));
    }

    /// Deserialization must also stay put: an old daemon's bytes have to land in
    /// the right variant, which is the half of compatibility that serialization
    /// tests cannot see.
    #[test]
    fn relayed_bytes_still_deserialize_into_the_right_variant() {
        let c: ClientMsg = serde_json::from_str(r#"{"type":"ChatToken","token":"hi"}"#).unwrap();
        assert!(matches!(c, ClientMsg::Relay(HubRelay::ChatToken { ref token }) if token == "hi"));
        let d: DaemonMsg = serde_json::from_str(r#"{"type":"AgentDone"}"#).unwrap();
        assert!(matches!(d, DaemonMsg::Relay(HubRelay::AgentDone)));
        let u: DaemonMsg = serde_json::from_str(
            r#"{"type":"Usage","prompt_tokens":1,"completion_tokens":2,"total_tokens":3}"#,
        )
        .unwrap();
        assert!(matches!(
            u,
            DaemonMsg::Relay(HubRelay::Usage {
                total_tokens: 3,
                ..
            })
        ));
    }
}
