//! # Ahma MCP Service: The Protocol Layer
//!
//! This module implements the "brain" of the Ahma server. The [`AhmaMcpService`]
//! is responsible for managing the full lifecycle of the Model Context Protocol (MCP),
//! from initial handshake and tool discovery to execution routing and session isolation.
//!
//! ## Protocol Lifecycle
//!
//! The service coordinates several critical phases of an MCP session:
//!
//! 1. **Handshake (`initialize`)**: Establishes the connection and identifies client
//!    capabilities.
//! 2. **Sandbox Anchoring (`roots/list`)**: In HTTP bridge or session-isolated modes,
//!    the server queries the client for workspace roots to dynamically configure the
//!    security sandbox for that specific session.
//! 3. **Tool Discovery (`list_tools`)**: Dynamically transforms MTDF JSON configurations
//!    and bundled capability flags (like `--tools git`) into a rich set of
//!    tools that the AI can understand and call.
//! 4. **Execution Routing (`call_tool`)**: Validates incoming arguments against the
//!    tool's JSON schema and routes the execution request to the [`Adapter`].
//!
//! ## Async-First Philosophy
//!
//! Ahma is designed for agents performing complex, multi-threaded work. Most tool calls
//! follow an **Async-Result-Push** pattern:
//! - **Immediate Response**: The server returns an operation ID (e.g., `op_123`)
//!   instantly, allowing the agent to continue working or call other tools.
//! - **Background Execution**: The task runs in the background, governed by the
//!   [`OperationMonitor`](crate::operation_monitor::OperationMonitor).
//! - **Progressive Feedback**: Status updates and final results are pushed back to the
//!   client via standard MCP notifications as they happen.
//!
//! ## Built-in Core Tools
//!
//! The service always exposes a set of "Internal Tools" (`await`, `status`,
//! `cancel`, and `run_terminal_command`) that provide essential primitives for managing
//! background tasks and executing arbitrary logic within the sandbox.

pub mod bundle_registry;
pub mod handlers;
pub mod progress_push;
/// Sandbox configuration from client roots + the one-shot tool-config loads that
/// hang off it. (Formerly `config_watcher`; the tools-directory file watcher was
/// removed — see the module docs.)
mod sandbox_config;
pub mod schema;
mod sequence;
mod subcommand;
mod types;

pub use types::{
    ActiveAgentSession, CallWait, ExtensionToolHandler, GuidanceConfig, META_PARAMS, PromptRunner,
    SequenceKind, get_global_prompt_runner, register_global_extension_handler,
    register_global_prompt_runner,
};

use chrono::Utc;
use parking_lot::RwLock;
use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, ErrorData as McpError,
        Implementation, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ServerCapabilities, ServerInfo, Tool, ToolsCapability,
    },
    service::{NotificationContext, Peer, RequestContext, RoleServer},
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tracing;
use tracing::Instrument as _;

use crate::builtin_tool::BuiltinTool;
use crate::{
    adapter::Adapter,
    client_type::McpClientType,
    config::ToolConfig,
    file_ops::{DefaultFileOpsProvider, DefaultWebPageFetcher, FileOpsProvider, WebPageFetcher},
    llm_service::DefaultLlmCompletionService,
    operation_monitor::{Operation, OperationStatus},
};
use serde_json::Value;

pub(crate) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// How long a `tools/call` waits for an in-flight sandbox configuration to
/// settle before proceeding anyway (SPEC R5.1.2).
///
/// The `roots/list` round-trip is normally sub-millisecond; this only matters
/// for a client that answers slowly, and it is capped well under any client's
/// single-request budget (R2.6.5) so waiting here can never itself be the thing
/// that times a request out.
const SANDBOX_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// `AhmaMcpService` is the server handler for the MCP service.
#[derive(Clone)]
pub struct AhmaMcpService {
    pub adapter: Arc<Adapter>,
    pub operation_monitor: Arc<crate::operation_monitor::OperationMonitor>,
    /// Tool configurations, keyed by tool name. Writers MUST invalidate
    /// `Self::invalidate_config_tools_cache` after mutating (as
    /// [`Self::update_tools`] does), or `tools/list` serves stale entries.
    pub configs: Arc<RwLock<HashMap<String, ToolConfig>>>,
    /// Lazily-built builtin `Tool` list. The builtin set and every builtin
    /// schema are static for the life of the service, but each `Tool` (schema
    /// map included) used to be rebuilt on every `tools/list` request; built
    /// once here and cheaply cloned instead (`Tool` holds `Cow`/`Arc` fields).
    builtin_tools_cache: Arc<std::sync::OnceLock<Vec<Tool>>>,
    /// Cache of the constructed config-backed `Tool` list (visible entries
    /// only). Schema generation per config is the expensive part; the cache is
    /// dropped whenever `configs` changes ([`Self::invalidate_config_tools_cache`]).
    config_tools_cache: Arc<RwLock<Option<Arc<Vec<Tool>>>>>,
    /// Cache of the configured tool *names*, used by the harness guard's
    /// name-healing pass on every `tools/call`. Rebuilding it there allocated one
    /// `String` per configured tool per call, though the set only changes when
    /// `configs` does — so it is dropped by the same
    /// [`Self::invalidate_config_tools_cache`] that drops the `Tool` list.
    config_names_cache: Arc<RwLock<Option<Arc<Vec<String>>>>>,
    pub guidance: Arc<Option<GuidanceConfig>>,
    /// When true, forces all operations to run synchronously (overrides async-by-default).
    /// This is set when the --sync CLI flag is used.
    pub force_synchronous: bool,
    /// When true, sandbox initialization is deferred until roots/list_changed notification.
    /// This is used in HTTP bridge mode where SSE must connect before server→client requests.
    pub defer_sandbox: bool,
    /// The peer handle for sending notifications to the client.
    /// This is populated by capturing it from the first request context.
    pub peer: Arc<RwLock<Option<Peer<RoleServer>>>>,
    /// Minimum seconds between successive log monitoring alerts (default: 60).
    pub monitor_rate_limit_seconds: u64,
    /// Optional snapshot of the AppConfig used to construct this service.
    /// Stored so that runtime events (e.g. `roots/list` arrival) can rediscover
    /// a per-client `.ahma/` directory and reload tool configs against it
    /// without having to thread the AppConfig through every callsite.
    /// `None` for tests that don't need per-client tool discovery.
    pub app_config: Arc<RwLock<Option<Arc<crate::shell::cli::AppConfig>>>>,
    /// Path of the tools directory whose configs are currently loaded.
    /// Used by `configure_sandbox_from_roots` to detect when the per-client
    /// `.ahma/` differs from the currently-loaded one and reload is needed.
    pub current_tools_dir: Arc<RwLock<Option<std::path::PathBuf>>>,
    /// Registered handlers for extension tool types (e.g. decompose)
    pub extension_handlers:
        Arc<parking_lot::RwLock<HashMap<String, Arc<dyn ExtensionToolHandler>>>>,
    /// Custom file operations backend.
    pub file_ops_provider: Arc<dyn FileOpsProvider>,
    /// What each file looked like when this session last read or wrote it, so
    /// an edit is refused if the file was never read or has changed since
    /// (see `handlers::harness_tools`).
    pub(crate) file_stamps:
        Arc<parking_lot::Mutex<HashMap<PathBuf, handlers::harness_tools::FileStamp>>>,
    /// Custom web page fetcher.
    pub web_page_fetcher: Arc<dyn WebPageFetcher>,
    /// LLM completion service used by the livelog pipeline.
    pub llm_service: Arc<DefaultLlmCompletionService>,
    /// Last received timestamp for keep-alive optimization.
    pub last_received_signal: Arc<std::sync::atomic::AtomicU64>,
    /// True if the connected peer is an Ahma node.
    pub is_ahma_peer: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the bridge has told this subprocess it currently has a live
    /// push channel to the real client (an open SSE stream). Defaults to
    /// `false` — the safe assumption for direct stdio (no bridge in front)
    /// and for any session that never opens one (e.g. a configured default
    /// sandbox scope, which lets a client skip SSE entirely). A future
    /// liveness probe (SPEC R2.6.5 redesign) must not attempt a
    /// server-initiated request mid-`await` unless this is `true` — a
    /// subprocess-initiated message with no SSE subscriber is silently
    /// dropped by the bridge, so probing without this signal would
    /// misreport a healthy client as unresponsive.
    pub push_channel_open: Arc<std::sync::atomic::AtomicBool>,
    /// Safety harness guard context.
    pub harness_guard: Arc<tokio::sync::Mutex<crate::harness_guard::HarnessGuard>>,
    /// The agent's current task plan (the `todo_write` checklist). One list per
    /// service instance — adequate for the single-user TUI; per-session
    /// isolation is a future refinement.
    pub todo_list: Arc<tokio::sync::Mutex<Vec<handlers::todo_tool::TodoItem>>>,
    /// Routes unified operation events to the MCP client as progress
    /// notifications (per-operation peer + progress token registration).
    pub progress_push: Arc<progress_push::ProgressPushRouter>,
    /// Operation ids whose `tool_call` was written to the vault audit log;
    /// the audit subscriber records the matching `tool_complete` on the
    /// terminal event and removes the id.
    pub vault_audited_ops: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
    /// All external MCP servers (HTTP and stdio) for agent tool routing.
    pub mcp_connections: Arc<tokio::sync::RwLock<crate::mcp_client::McpConnectionManager>>,
    /// Session-scoped web-egress approvals (R-WEB.5). Holds the domains granted or
    /// denied for this session and coordinates in-flight approval prompts. Its
    /// grant/deny snapshots are threaded into the `[web]` policy decision on every
    /// `fetch_webpage`, so a session approval takes effect without a restart.
    pub web_approval: Arc<ahma_common::web_approval::WebApprovalCoordinator>,
    /// Optional sink for delivering a web-approval prompt to a connected TUI over
    /// the daemon hub (R-WEB.6), set in daemon/server mode. When a `fetch_webpage`
    /// hits an unknown domain and the MCP client cannot do interactive
    /// `elicitation/create`, the request is sent here; the daemon reporter forwards
    /// it as `ClientMsg::Relay(HubRelay::WebApprovalRequested)` and routes
    /// the TUI's answer back into `web_approval`. `None` ⇒ no TUI surface wired.
    pub web_approval_tx: Arc<
        parking_lot::Mutex<
            Option<
                tokio::sync::mpsc::UnboundedSender<ahma_common::web_approval::WebApprovalRequest>,
            >,
        >,
    >,
    /// The session's grant coordinator, when a permission broker is wired
    /// (server mode). Read by the keep-alive path to disclose the number of
    /// grants awaiting a human decision in the heartbeat payload (#485).
    pub grant_coordinator: Arc<RwLock<Option<Arc<ahma_common::scope_grant::GrantCoordinator>>>>,
    /// `true` while a sandbox configuration (the `roots/list` round-trip) is in
    /// flight (SPEC R5.1.2). Two readers: `spawn_sandbox_configuration` uses it
    /// to keep a burst of `roots/list_changed` notifications from starting
    /// concurrent queries, and `guard_sandbox_ready_for_tool_calls` waits on it
    /// so a `tools/call` never runs against a scope that is still being decided.
    pub sandbox_config_in_flight: Arc<tokio::sync::watch::Sender<bool>>,
}

/// Project an rmcp [`Tool`] into the `ToolInfo` shape the agent loop consumes.
fn tool_info_from_tool(tool: Tool) -> crate::mcp_client::ToolInfo {
    crate::mcp_client::ToolInfo {
        name: tool.name.to_string(),
        description: tool.description.map(|d| d.to_string()),
        input_schema: serde_json::Value::Object(tool.input_schema.as_ref().clone()),
    }
}

/// Build a `Tool` whose title is its own name.
///
/// Every tool ahma advertises does this — the built-ins below, the
/// configured tools, the flattened subcommands, and the external MCP tools
/// it re-exports. MCP treats `title` as a display name that falls back to
/// `name` when absent, so "title == name" and "no title" render the same;
/// what the field buys is that a client never has to know about the
/// fallback. Naming that intent once stops the pair drifting, which the
/// twenty hand-written `.with_title("<the name again>")` calls this
/// replaced could not.
fn self_titled_tool<N, D, S>(name: N, description: D, input_schema: S) -> Tool
where
    N: Into<std::borrow::Cow<'static, str>>,
    D: Into<std::borrow::Cow<'static, str>>,
    S: Into<std::sync::Arc<rmcp::model::JsonObject>>,
{
    let name = name.into();
    let title = name.to_string();
    Tool::new(name, description, input_schema).with_title(title)
}

/// Whether progress notifications should be sent to a client with the given
/// `force` override and client-type heuristic. Shared by
/// [`AhmaMcpService::effective_supports_progress`] and
/// `sequence::register_progress_target`, which can't reach `self` and so
/// takes `force` as an already-resolved parameter.
fn progress_enabled(force: bool, client_type: crate::client_type::McpClientType) -> bool {
    force || client_type.supports_progress()
}

impl AhmaMcpService {
    /// The server's sync/async policy (SPEC R2.1): `tools.execution_mode`, as
    /// resolved from `--sync`/`--async` and the settings files. A service with
    /// no `AppConfig` — an embedding that never set one — keeps async, the
    /// library's historical behaviour; the `ahma` binary always sets one.
    pub fn execution_policy(&self) -> ahma_common::config::ExecutionPolicy {
        self.app_config
            .read()
            .as_ref()
            .map(|c| c.execution_mode)
            .unwrap_or(ahma_common::config::ExecutionPolicy::Async)
    }

    /// How long this call waits: sync mode waits until done unless the call
    /// (MTDF `synchronous: false`, `blocking: false`) explicitly asks not to.
    pub(crate) fn call_wait(&self, opted_out_of_waiting: bool) -> CallWait {
        match self.execution_policy() {
            ahma_common::config::ExecutionPolicy::Sync if !opted_out_of_waiting => {
                CallWait::UntilDone
            }
            _ => CallWait::Adaptive,
        }
    }

    /// A sequence's share of [`Self::call_wait`]: in sync mode, the steps are
    /// waited for within the window a single call would get.
    fn sequence_wait(
        &self,
        context: &RequestContext<RoleServer>,
        opted_out_of_waiting: bool,
    ) -> Option<sequence::SequenceWait<'_>> {
        if self.call_wait(opted_out_of_waiting) != CallWait::UntilDone {
            return None;
        }
        let (window, _) = self.sync_call_wait(
            McpClientType::from_peer(&context.peer),
            Some(context.peer.clone()),
        );
        Some(sequence::SequenceWait {
            monitor: &self.operation_monitor,
            window,
        })
    }

    /// Whether the bridge in front of this subprocess (if any) currently has
    /// a live push channel open to the real client. See the
    /// [`push_channel_open`](Self::push_channel_open) field docs for why a
    /// future liveness probe must check this before attempting a
    /// server-initiated request mid-`await`.
    pub(crate) fn push_channel_open(&self) -> bool {
        self.push_channel_open
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The raw `tools.force_progress_notifications` / `--force-progress-notifications`
    /// override, before combining it with any client's default heuristic. Free
    /// functions that can't reach `self` (e.g. `sequence::handle_sequence_tool`)
    /// take this resolved bool as a parameter instead of computing it themselves.
    pub(crate) fn force_progress_notifications_override(&self) -> bool {
        self.app_config
            .read()
            .as_ref()
            .map(|c| c.force_progress_notifications)
            .unwrap_or(false)
    }

    /// Whether progress notifications should be sent to this client: the
    /// operator's `tools.force_progress_notifications` /
    /// `--force-progress-notifications` override if set, otherwise
    /// `client_type.supports_progress()`'s default heuristic (which
    /// suppresses them for Cursor — an asserted, not measured, client quirk).
    pub(crate) fn effective_supports_progress(
        &self,
        client_type: crate::client_type::McpClientType,
    ) -> bool {
        progress_enabled(self.force_progress_notifications_override(), client_type)
    }

    /// The pure dispatch logic behind [`ServerHandler::on_custom_notification`],
    /// factored out so it is testable without constructing a real
    /// `NotificationContext` (its `Peer` cannot be built outside rmcp itself).
    fn apply_custom_notification(&self, method: &str, params: Option<&serde_json::Value>) {
        if method == ahma_common::mcp_methods::HEARTBEAT_METHOD {
            self.last_received_signal.store(
                ahma_common::keepalive::current_timestamp_ms(),
                std::sync::atomic::Ordering::Relaxed,
            );
        } else if method == ahma_common::mcp_methods::PUSH_CHANNEL_CHANGED_METHOD {
            // Lenient by design (SPEC R2.6.5.3): missing or malformed params
            // fall back to `connected: false`, the safe "no live channel"
            // assumption — never an error.
            let connected =
                ahma_common::mcp_methods::PushChannelChangedParams::from_params(params).connected;
            self.push_channel_open
                .store(connected, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Every tool available to this session, in `ToolInfo` form: the built-ins
    /// from `Self::builtin_tools`, then per-client config tools, then
    /// external MCP tools.
    ///
    /// This is the toolset handed to ahma's own agent loop, so it withholds
    /// the built-ins [`BuiltinTool::is_denied_in_agent_loop`] names. Everything
    /// else is shared with `list_tools` by construction rather than by hand.
    pub async fn get_all_available_tools(&self) -> Vec<crate::mcp_client::ToolInfo> {
        let mut tools: Vec<crate::mcp_client::ToolInfo> = self
            .builtin_tools()
            .into_iter()
            .filter(|t| {
                !BuiltinTool::from_name(&t.name).is_some_and(BuiltinTool::is_denied_in_agent_loop)
            })
            .map(tool_info_from_tool)
            .collect();

        tools.extend(
            self.visible_config_tools()
                .into_iter()
                .map(tool_info_from_tool),
        );

        let external_mgr = self.mcp_connections.read().await;
        for ext_tool in external_mgr.aggregate_tools() {
            tools.push(ext_tool.clone());
        }

        tools
    }

    /// The `Tool` entries for every configured tool that is visible to the
    /// client. Shared by `get_all_available_tools` and `list_tools` so their
    /// filtering cannot drift apart.
    ///
    /// Schema construction per config is expensive and the result only changes
    /// when `configs` does, so the built list is cached until
    /// [`Self::invalidate_config_tools_cache`] drops it (every `configs`
    /// writer must call it — [`Self::update_tools`] does).
    fn visible_config_tools(&self) -> Vec<Tool> {
        if let Some(cached) = self.config_tools_cache.read().as_ref() {
            return cached.as_ref().clone();
        }
        let tools: Vec<Tool> = {
            let configs_lock = self.configs.read();
            configs_lock
                .values()
                .filter(|config| self.is_config_visible_to_client(config))
                .flat_map(|config| self.create_tools_from_config(config))
                .collect()
        };
        *self.config_tools_cache.write() = Some(Arc::new(tools.clone()));
        tools
    }

    /// Drop the cached config-backed `Tool` list so the next `tools/list`
    /// rebuilds it from the current `configs`. Must accompany every mutation
    /// of [`Self::configs`].
    fn invalidate_config_tools_cache(&self) {
        *self.config_tools_cache.write() = None;
        *self.config_names_cache.write() = None;
    }

    /// The configured tool names, cached alongside the `Tool` list.
    fn config_tool_names(&self) -> Arc<Vec<String>> {
        if let Some(cached) = self.config_names_cache.read().as_ref() {
            return cached.clone();
        }
        let names = Arc::new(self.configs.read().keys().cloned().collect::<Vec<String>>());
        *self.config_names_cache.write() = Some(names.clone());
        names
    }

    /// The built-in tools every session exposes, before per-client config
    /// tools and external MCP tools are appended.
    ///
    /// Single source of truth for both `list_tools` (what MCP clients see) and
    /// [`Self::get_all_available_tools`] (what ahma's own agent loop sees).
    /// Those were two separately maintained literals and they had drifted: the
    /// agent-facing one was three tools and two descriptions behind.
    ///
    /// The set (schemas included) is static per service, so it is built once
    /// and cloned per call — `Tool` is cheap to clone (`Cow` strings, `Arc`
    /// schema map), while *building* one means constructing a full JSON schema.
    fn builtin_tools(&self) -> Vec<Tool> {
        self.builtin_tools_cache
            .get_or_init(|| self.build_builtin_tools())
            .clone()
    }

    /// Construct the builtin `Tool` values. Only called once per service, via
    /// the [`Self::builtin_tools`] cache.
    fn build_builtin_tools(&self) -> Vec<Tool> {
        BuiltinTool::ALL
            .iter()
            .map(|tool| self.build_builtin_tool(*tool))
            .collect()
    }

    /// The description and input schema for one built-in.
    ///
    /// Exhaustive on [`BuiltinTool`], which is the point: a tool added to the
    /// enum without an entry here is a compile error, where the hand-written
    /// `vec![...]` this replaced could silently omit one.
    fn build_builtin_tool(&self, tool: BuiltinTool) -> Tool {
        let (description, input_schema) = match tool {
            BuiltinTool::Await => (
                "Block until a started operation completes and return its final result. Operations notify automatically when they finish, so prefer doing other useful work first; reach for `await` only when the next step truly depends on the result.",
                self.generate_input_schema_for_wait(),
            ),
            BuiltinTool::Status => (
                "Return a snapshot of active and completed operations without blocking. Completion is pushed via notifications, so this is for ad-hoc inspection rather than polling.",
                self.generate_input_schema_for_status(),
            ),
            BuiltinTool::RunTerminalCommand => (
                "Run a shell command inside a kernel-level filesystem sandbox (Landlock on Linux, Seatbelt on macOS, Job Objects on Windows). Returns an operation_id immediately; use `status`, `await`, or `cancel` to manage long-running work. Supports pipes, redirects, environment variables, and full shell syntax. Set `monitor_level` to stream error/warning alerts from stdout or stderr.",
                self.generate_input_schema_for_run_terminal_command(),
            ),
            BuiltinTool::LogsList => (
                "List all log files in the project log directory (`.ahma/logs/`). Returns file names, sizes, modification times, and symlink targets. Use this to discover which log files are available before calling logs_read or logs_search.",
                handlers::log_tools::logs_list_schema(),
            ),
            BuiltinTool::LogsApprove => (
                "Approve a blocked out-of-scope log symlink target to allow AI read access.",
                handlers::log_tools::logs_approve_schema(),
            ),
            BuiltinTool::LogsRead => (
                "Read lines from a project log file with optional pagination. Sensitive values (tokens, passwords, API keys) are redacted by default. Use `raw: true` only when debugging credential issues.",
                handlers::log_tools::logs_read_schema(),
            ),
            BuiltinTool::LogsSearch => (
                "Search a project log file for lines matching a pattern (case-insensitive substring match by default). Returns matching lines with line numbers. Sensitive values are redacted by default.",
                handlers::log_tools::logs_search_schema(),
            ),
            BuiltinTool::Restart => (
                "Force stop and restart the background bridge server, disconnecting all active sessions (including TUI and other IDEs) to apply updates or recover from a bad state.",
                handlers::restart_tool::restart_schema(),
            ),
            BuiltinTool::Cancel => (
                "Cancel a running background operation by `id`, or cancel EVERY in-flight operation with `all: true`. Each cancellation reaps the operation's full process tree (cargo/rustc/sccache) — the clean way to stop wedged work without killing and restarting the server.",
                handlers::cancel_tool::cancel_schema(),
            ),
            BuiltinTool::SandboxGrant => (
                "Propose adding an out-of-scope path as a persistent sandbox root in ~/.ahma/settings.toml. Call this when a command fails with a `sandbox_denial` error. WITHOUT `confirm: true` it only PREVIEWS — it returns the full settings-file path, the exact line it would add, and a risk assessment so you can show the human and get approval first. Catastrophic paths (filesystem root, $HOME, credential dirs, system dirs, workspace parents) are REFUSED even with confirmation. On `confirm: true` it writes the grant; run `restart` to apply, then re-run the blocked command.",
                handlers::sandbox_grant_tool::sandbox_grant_schema(),
            ),
            BuiltinTool::ReadFile => (
                "Read a text file as numbered lines (`  N<tab>text`; the number is not part of the file). Returns up to 2000 lines from start_line and says how to continue; long lines are cut at 2000 characters; binary files are refused. Read a file before editing or overwriting it.",
                handlers::harness_tools::read_file_schema(),
            ),
            BuiltinTool::ListDir => (
                "List entries in a scoped directory with basic metadata.",
                handlers::harness_tools::list_dir_schema(),
            ),
            BuiltinTool::FileSearch => (
                "Find files by glob pattern (e.g. `**/*.rs`), most recently modified first. Respects .gitignore and skips hidden files, like ripgrep.",
                handlers::harness_tools::file_search_schema(),
            ),
            BuiltinTool::GrepSearch => (
                "Search file contents by plain text or regex. Respects .gitignore, skips binary files. output_mode: content (matching lines, optional context), files (paths only), count (matches per file).",
                handlers::harness_tools::grep_search_schema(),
            ),
            BuiltinTool::FetchWebpage => (
                "Fetch an HTTP/HTTPS page and return its readable text (at most 50,000 characters; pass `query` to keep only the lines that mention it).",
                handlers::harness_tools::fetch_webpage_schema(),
            ),
            BuiltinTool::WriteFile => (
                "Create a file, or overwrite one — an existing file must have been read in this session and not changed since. For changes to part of a file prefer replace_in_file, multi_edit or apply_patch.",
                handlers::harness_tools::write_file_schema(),
            ),
            BuiltinTool::ReplaceInFile => (
                "Replace one exact piece of a file: old_str must occur exactly once (add surrounding lines to make it unique) unless replace_all. Read the file first. Returns the edited lines. On a miss, says what is there instead.",
                handlers::harness_tools::replace_in_file_schema(),
            ),
            BuiltinTool::MultiEdit => (
                "Several exact replacements in one file, applied in order; if any fails, none is applied. Same rules as replace_in_file for each edit.",
                handlers::harness_tools::multi_edit_schema(),
            ),
            BuiltinTool::ApplyPatch => (
                "Apply a patch that adds, deletes, updates or moves files (the `*** Begin Patch` format). Nothing is written unless every file operation applies. Files updated or deleted must have been read first.",
                handlers::harness_tools::apply_patch_schema(),
            ),
            BuiltinTool::Agent => (
                "Delegate a self-contained task to ahma's own agent loop as a sub-agent. ahma runs its full tool-using loop (read/edit files, run commands in the sandbox, search) with the model the user last selected in `ahma tui`, and returns the final answer. Use this to offload a focused sub-task — investigating code, producing a file or report, or answering a question grounded in the workspace — without doing the steps yourself.",
                handlers::agent_tool::agent_schema(),
            ),
            BuiltinTool::TodoWrite => (
                "Record or update your task plan as a checklist. Pass the FULL list of steps each time — it replaces the current plan. Use this at the start of any multi-step task, then call it again to mark a step in_progress before you work on it and completed when it's done. Keeps you (and the user) oriented across turns.",
                handlers::todo_tool::todo_write_schema(),
            ),
            BuiltinTool::LogMonitor => (
                "Start a real-time log monitoring session on a file inside the sandbox. Reads new lines as they are written, runs them through the AI for issue detection, and sends alerts.",
                schema::object_input_schema(
                    {
                        let mut props = serde_json::Map::new();
                        props.insert(
                            "file_path".to_string(),
                            schema::string_property(
                                "Path of the log file to monitor (within sandbox scope)",
                            ),
                        );
                        props.insert(
                            "detection_prompt".to_string(),
                            schema::string_property("Optional prompt guiding AI issue detection"),
                        );
                        props.insert(
                            "llm_base_url".to_string(),
                            schema::string_property("Optional custom LLM base URL"),
                        );
                        props.insert(
                            "llm_model".to_string(),
                            schema::string_property("Optional custom LLM model"),
                        );
                        props.insert(
                            "llm_api_key".to_string(),
                            schema::string_property("Optional custom LLM API key"),
                        );
                        props
                    },
                    &["file_path"],
                ),
            ),
        };
        self_titled_tool(tool.name(), description, input_schema)
    }

    fn task_vault_root(&self) -> Option<PathBuf> {
        let cfg_root = self
            .app_config
            .read()
            .as_ref()
            .and_then(|c| c.task_vault.clone());

        if cfg_root.is_some() {
            return cfg_root;
        }

        let scope = self.adapter.sandbox().scopes().first()?.clone();
        let is_workdir = scope
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == "workdir")
            .unwrap_or(false);
        if is_workdir {
            scope.parent().map(|p| p.to_path_buf())
        } else {
            None
        }
    }

    fn task_vault_audit_log_path(&self) -> Option<PathBuf> {
        self.task_vault_root().map(|root| root.join("audit.jsonl"))
    }

    fn store_peer_handle_if_unset(&self, peer: &Peer<RoleServer>) {
        let mut peer_guard = self.peer.write();
        if peer_guard.is_none() {
            *peer_guard = Some(peer.clone());
            tracing::info!("Successfully captured MCP peer handle for async notifications.");
        }
    }

    fn should_skip_client_roots_sandbox_setup(&self) -> bool {
        // SPEC R5.5: only EXPLICIT user-provided scopes (--sandbox-scope,
        // --working-directories, task vault) suppress the roots/list request.
        // Implicitly-derived scopes (the CWD fallback, the --tmp temp scope) are
        // provisional: we still ask the client for its workspace roots and prefer
        // them. This is what lets shared-process clients like Cursor — whose
        // subprocess CWD is unrelated to the open workspace (often the system
        // temp dir) — get sandboxed to the correct workspace root instead of
        // being locked to a bogus implicit scope. If the client never answers
        // (e.g. Antigravity), `configure_sandbox_from_roots` falls back to the
        // provisional scopes, preserving prior behavior.
        if self.adapter.sandbox().has_explicit_scopes()
            && !self.adapter.sandbox().scopes().is_empty()
        {
            tracing::info!(
                "Sandbox scopes explicitly configured ({:?}), skipping roots/list request (SPEC R5.5)",
                self.adapter.sandbox().scopes()
            );
            return true;
        }

        if self.adapter.sandbox().is_test_mode() {
            tracing::debug!(
                "Sandbox in test/disabled mode: skipping roots/list request. \
                 Path validation bypassed for all paths."
            );
            return true;
        }

        false
    }

    fn is_sync_meta_tool_for_protocol_cancel(tool_name: &str) -> bool {
        BuiltinTool::from_name(tool_name)
            .is_some_and(BuiltinTool::is_sync_meta_tool_for_protocol_cancel)
    }

    fn task_vault_trash_dir(&self) -> Option<PathBuf> {
        self.task_vault_root().map(|root| root.join("trash"))
    }

    fn summarize_arguments(arguments: &serde_json::Map<String, serde_json::Value>) -> String {
        serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string())
    }

    async fn append_audit_line(
        audit_log_path: &Path,
        payload: serde_json::Value,
    ) -> Result<(), anyhow::Error> {
        use tokio::fs::OpenOptions;
        use tokio::io::AsyncWriteExt as _;

        if let Some(parent) = audit_log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut line = serde_json::to_string(&payload)?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(audit_log_path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    async fn append_tool_call_event(
        audit_log_path: &Path,
        operation_id: &str,
        tool_name: &str,
        args_summary: &str,
    ) -> Result<(), anyhow::Error> {
        Self::append_audit_line(
            audit_log_path,
            serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "type": "tool_call",
                "operation_id": operation_id,
                "tool_name": tool_name,
                "args_summary": args_summary,
            }),
        )
        .await
    }

    async fn append_tool_complete_event(
        audit_log_path: &Path,
        operation_id: &str,
        success: bool,
        duration_ms: u64,
    ) -> Result<(), anyhow::Error> {
        Self::append_audit_line(
            audit_log_path,
            serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "type": "tool_complete",
                "operation_id": operation_id,
                "success": success,
                "duration_ms": duration_ms,
            }),
        )
        .await
    }

    async fn emit_vault_tool_call(&self, operation_id: &str, tool_name: &str, args_summary: &str) {
        let Some(audit_log_path) = self.task_vault_audit_log_path() else {
            return;
        };
        // Track the id so the vault audit subscriber records the matching
        // tool_complete when the operation's terminal event arrives.  Sync
        // paths call emit_vault_tool_complete directly, which removes the id.
        // Scoped, not a bare `let`: this guard must be dropped before the
        // `.await` below. A `parking_lot` guard is `!Send` (as std's is), so
        // holding one across an await point makes the whole future `!Send` and
        // the service stops compiling — which is the compiler catching the
        // deadlock risk rather than the risk reaching production.
        {
            self.vault_audited_ops
                .lock()
                .insert(operation_id.to_string());
        }
        if let Err(e) =
            Self::append_tool_call_event(&audit_log_path, operation_id, tool_name, args_summary)
                .await
        {
            tracing::warn!("Failed to append vault tool_call audit event: {}", e);
        }
    }

    async fn emit_vault_tool_complete(&self, operation_id: &str, success: bool, duration_ms: u64) {
        let Some(audit_log_path) = self.task_vault_audit_log_path() else {
            return;
        };
        // Sync paths complete directly — drop any pending subscriber tracking.
        // Scoped so the guard is released before the `.await`: a `parking_lot`
        // guard is `!Send`, so holding one across an await point would make this
        // whole future `!Send`.
        {
            self.vault_audited_ops.lock().remove(operation_id);
        }
        if let Err(e) =
            Self::append_tool_complete_event(&audit_log_path, operation_id, success, duration_ms)
                .await
        {
            tracing::warn!("Failed to append vault tool_complete audit event: {}", e);
        }
    }

    async fn emit_vault_file_staged(&self, original_path: &str, trash_path: &str) {
        let Some(audit_log_path) = self.task_vault_audit_log_path() else {
            return;
        };
        if let Err(e) = Self::append_audit_line(
            &audit_log_path,
            serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(),
                "type": "file_staged",
                "original_path": original_path,
                "trash_path": trash_path,
            }),
        )
        .await
        {
            tracing::warn!("Failed to append vault file_staged audit event: {}", e);
        }
    }

    async fn maybe_stage_configured_delete(
        &self,
        base_command: &str,
        working_directory: &str,
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<CallToolResult>, McpError> {
        use crate::vault::rm_interceptor::RmInterceptor;

        if !RmInterceptor::looks_like_rm_command(base_command) || self.task_vault_root().is_none() {
            return Ok(None);
        }

        let targets = RmInterceptor::extract_rm_targets_from_arguments(arguments);
        if targets.is_empty() {
            return Ok(None);
        }

        let trash_dir = self.task_vault_trash_dir().ok_or_else(|| {
            handlers::common::mcp_internal("Task vault trash directory not configured")
        })?;

        let staged =
            RmInterceptor::stage_paths_into_vault_trash(&trash_dir, working_directory, &targets)
                .map_err(|e| {
                    handlers::common::mcp_internal(format!(
                        "Failed to stage deletion to vault trash: {}",
                        e
                    ))
                })?;

        for (original_path, trash_path) in &staged {
            self.emit_vault_file_staged(original_path, trash_path).await;
        }

        Ok(Some(handlers::common::text_result(format!(
            "Staged {} path(s) into vault trash instead of permanent delete.",
            staged.len()
        ))))
    }

    /// Creates a new `AhmaMcpService` instance.
    ///
    /// This service implements the `rmcp::ServerHandler` trait and manages tool execution
    /// via the provided `Adapter`.
    ///
    /// # Arguments
    ///
    /// * `adapter` - The tool execution engine.
    /// * `operation_monitor` - Monitor for tracking background task progress.
    /// * `configs` - Map of loaded tool configurations.
    /// * `guidance` - Optional guidance configuration for AI usage hints.
    /// * `force_synchronous` - If true, overrides async defaults (e.g., for debugging).
    /// * `defer_sandbox` - If true, delays sandbox initialization (for HTTP bridge scenarios).
    pub async fn new(
        adapter: Arc<Adapter>,
        operation_monitor: Arc<crate::operation_monitor::OperationMonitor>,
        configs: Arc<HashMap<String, ToolConfig>>,
        guidance: Arc<Option<GuidanceConfig>>,
        force_synchronous: bool,
        defer_sandbox: bool,
    ) -> Result<Self, anyhow::Error> {
        // Start the background monitor for operation timeouts
        crate::operation_monitor::OperationMonitor::start_background_monitor(
            operation_monitor.clone(),
        );

        let progress_push = progress_push::ProgressPushRouter::new(operation_monitor.clone());
        progress_push.spawn_forwarder(&operation_monitor);

        // A fresh `Sandbox` starts with `roots_received == false`; nothing to
        // reset here. (This used to undo a `true` the constructor seeded, which
        // left every non-MCP construction reporting `roots/list` provenance for
        // a scope that never saw roots.)
        let service = Self {
            adapter,
            operation_monitor,
            configs: Arc::new(RwLock::new((*configs).clone())),
            builtin_tools_cache: Arc::new(std::sync::OnceLock::new()),
            config_tools_cache: Arc::new(RwLock::new(None)),
            config_names_cache: Arc::new(RwLock::new(None)),
            guidance,
            force_synchronous,
            defer_sandbox,
            sandbox_config_in_flight: Arc::new(tokio::sync::watch::Sender::new(false)),
            peer: Arc::new(RwLock::new(None)),
            monitor_rate_limit_seconds: crate::log_monitor::DEFAULT_RATE_LIMIT_SECONDS,
            app_config: Arc::new(RwLock::new(None)),
            current_tools_dir: Arc::new(RwLock::new(None)),
            extension_handlers: Arc::new(parking_lot::RwLock::new(
                types::get_global_extension_handlers().read().clone(),
            )),
            file_ops_provider: Arc::new(DefaultFileOpsProvider),
            file_stamps: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            web_page_fetcher: Arc::new(DefaultWebPageFetcher),
            llm_service: Arc::new(DefaultLlmCompletionService),
            last_received_signal: Arc::new(std::sync::atomic::AtomicU64::new(
                ahma_common::keepalive::current_timestamp_ms(),
            )),
            is_ahma_peer: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            push_channel_open: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // Self-correction (tool-name/argument healing + failure-loop
            // detection) is on by default — see `set_app_config` for why.
            harness_guard: Arc::new(tokio::sync::Mutex::new(
                crate::harness_guard::HarnessGuard::new(true),
            )),
            todo_list: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            progress_push,
            vault_audited_ops: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            mcp_connections: Arc::new(tokio::sync::RwLock::new(
                crate::mcp_client::McpConnectionManager::default(),
            )),
            web_approval: Arc::new(ahma_common::web_approval::WebApprovalCoordinator::new()),
            web_approval_tx: Arc::new(parking_lot::Mutex::new(None)),
            grant_coordinator: Arc::new(RwLock::new(None)),
        };
        service.spawn_vault_audit_subscriber();
        Ok(service)
    }

    /// Subscribe to the unified event stream and append a `tool_complete`
    /// vault audit record when an audited operation reaches a terminal state.
    fn spawn_vault_audit_subscriber(&self) {
        let service = self.clone();
        let mut rx = self.operation_monitor.subscribe_events();
        tokio::spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("vault audit subscriber lagged {n} events");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if !event.is_terminal() {
                    continue;
                }
                let op_id = event.operation_id().to_string();
                let audited = service.vault_audited_ops.lock().remove(&op_id);
                if !audited {
                    continue;
                }
                let (success, duration_ms) = Self::vault_audit_outcome(event.as_ref());
                service
                    .emit_vault_tool_complete(&op_id, success, duration_ms)
                    .await;
            }
        });
    }

    /// Classifies a terminal operation event for the vault audit log as
    /// `(success, duration_ms)`. Non-terminal variants never reach here; they
    /// map to the conservative `(false, 0)`.
    fn vault_audit_outcome(event: &ahma_common::event_dispatcher::OperationEvent) -> (bool, u64) {
        use ahma_common::event_dispatcher::OperationEvent;
        match event {
            OperationEvent::Completed { duration_ms, .. } => (true, *duration_ms),
            OperationEvent::Failed { duration_ms, .. }
            | OperationEvent::Cancelled { duration_ms, .. }
            | OperationEvent::TimedOut { duration_ms, .. } => (false, *duration_ms),
            _ => (false, 0),
        }
    }

    /// Sets a custom file operations provider.
    pub fn with_file_ops_provider(mut self, provider: Arc<dyn FileOpsProvider>) -> Self {
        self.file_ops_provider = provider;
        self
    }

    /// Sets a custom web page fetcher.
    pub fn with_web_page_fetcher(mut self, fetcher: Arc<dyn WebPageFetcher>) -> Self {
        self.web_page_fetcher = fetcher;
        self
    }

    /// Wire the daemon-hub sink that delivers web-approval prompts to a connected
    /// TUI (R-WEB.6). Takes `&self` so it can be called after construction on the
    /// already-shared service; the sink is stored behind the service's shared
    /// `Arc`, so every clone sees it.
    pub fn set_web_approval_sender(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<ahma_common::web_approval::WebApprovalRequest>,
    ) {
        *self.web_approval_tx.lock() = Some(tx);
    }

    /// Store the AppConfig that constructed this service so runtime events
    /// (such as `roots/list` arrival) can rediscover per-client `.ahma/` dirs.
    pub fn set_app_config(&self, config: Arc<crate::shell::cli::AppConfig>) {
        if let Some(dir) = config.tools_dir.clone() {
            *self.current_tools_dir.write() = Some(dir);
        }
        // Self-correction (tool-name/argument healing + failure-loop detection)
        // is universally safe and is exactly what stops a model from burning its
        // turn budget re-issuing the same broken call, so it stays on regardless
        // of `small_model_harness`. That flag now governs only the verbose
        // coaching hints injected in the agent loop, not self-correction.
        if let Ok(mut guard) = self.harness_guard.try_lock() {
            guard.enabled = true;
        }
        *self.app_config.write() = Some(config);
    }

    /// Register an extension handler for custom tool routing.
    pub fn register_extension_handler(&self, name: String, handler: Arc<dyn ExtensionToolHandler>) {
        self.extension_handlers.write().insert(name, handler);
    }

    /// Tool names — bare builtin names, or the flattened `tool_subcommand`
    /// names [`Self::get_all_available_tools`] advertises — whose effective
    /// `mutates` resolves to `false`.
    ///
    /// Read once per agent run and handed to the caller (`ahma_core`'s
    /// `needs_approval`) rather than looked up per tool call, so that
    /// function stays a pure, synchronous, easily tested check. Everything
    /// **not** in this set is treated as mutating, including a name this
    /// service has never heard of — the default is fail-closed, matching
    /// [`crate::config::ToolConfig::mutates`]'s own documented default: an
    /// author who never set the flag is assumed capable of doing harm until
    /// they say otherwise, not the reverse.
    pub fn non_mutating_tool_names(&self) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();

        for tool in BuiltinTool::ALL {
            if !tool.is_mutating() {
                names.insert(tool.name().to_string());
            }
        }

        // Same flattening `create_tools_from_config` uses, so the names
        // computed here are exactly the names `tools/call` will receive.
        let configs = self.configs.read();
        for tool_config in configs.values() {
            let leaves = Self::leaf_subcommands(tool_config);
            if Self::creates_single_tool(&leaves) {
                if !tool_config.mutates.unwrap_or(true) {
                    names.insert(tool_config.name.clone());
                }
                continue;
            }
            for (sub_path, subcommand_config) in leaves {
                let effective = subcommand_config
                    .mutates
                    .or(tool_config.mutates)
                    .unwrap_or(true);
                if !effective {
                    names.insert(format!("{}_{}", tool_config.name, sub_path));
                }
            }
        }

        names
    }

    fn leaf_subcommands(
        tool_config: &ToolConfig,
    ) -> Vec<(String, &crate::config::SubcommandConfig)> {
        let mut leaf_subcommands = Vec::new();
        if let Some(subcommands) = &tool_config.subcommand {
            schema::collect_leaf_subcommands(subcommands, "", &mut leaf_subcommands);
        }
        leaf_subcommands
    }

    fn creates_single_tool(
        leaf_subcommands: &[(String, &crate::config::SubcommandConfig)],
    ) -> bool {
        match leaf_subcommands {
            [] => true,
            [(name, _)] => name == "default",
            _ => false,
        }
    }

    fn build_single_tool_from_config(&self, tool_config: &ToolConfig) -> Tool {
        let base_name = &tool_config.name;
        let description = self.tool_description(tool_config, base_name);
        let input_schema = schema::generate_schema_for_tool_config(tool_config);
        self_titled_tool(base_name.clone(), description, input_schema)
    }

    fn flattened_subcommand_description(
        tool_config: &ToolConfig,
        subcommand_config: &crate::config::SubcommandConfig,
    ) -> String {
        if subcommand_config.description.is_empty() {
            tool_config.description.clone()
        } else {
            subcommand_config.description.clone()
        }
    }

    fn build_flattened_tool_from_config(
        &self,
        tool_config: &ToolConfig,
        sub_path: &str,
        subcommand_config: &crate::config::SubcommandConfig,
    ) -> Tool {
        let base_name = &tool_config.name;
        let flat_name = format!("{}_{}", base_name, sub_path);
        let sub_description =
            Self::flattened_subcommand_description(tool_config, subcommand_config);
        let description = self.tool_description_text(tool_config, &flat_name, &sub_description);
        let input_schema = Arc::new(schema::generate_single_command_schema(
            tool_config,
            &(sub_path.to_string(), subcommand_config),
        ));
        self_titled_tool(flat_name, description, input_schema)
    }

    /// Creates MCP Tools from a ToolConfig.
    ///
    /// If the tool has subcommands, returns one flattened Tool per leaf subcommand
    /// (e.g., `"file-tools_hello"`, `"file-tools_world"`). If there are no
    /// subcommands (or only a single `"default"` one), returns a single Tool
    /// with the original config name.
    fn create_tools_from_config(&self, tool_config: &ToolConfig) -> Vec<Tool> {
        let leaf_subcommands = Self::leaf_subcommands(tool_config);

        if Self::creates_single_tool(&leaf_subcommands) {
            return vec![self.build_single_tool_from_config(tool_config)];
        }

        // Multiple subcommands → flatten into one Tool per leaf
        leaf_subcommands
            .into_iter()
            .map(|(sub_path, subcommand_config)| {
                self.build_flattened_tool_from_config(tool_config, &sub_path, subcommand_config)
            })
            .collect()
    }

    /// Prepends the resolved guidance block (if any) to `base`. Shared by
    /// [`Self::tool_description`] and [`Self::tool_description_text`], which
    /// differ only in where `base` comes from.
    fn apply_guidance(&self, tool_config: &ToolConfig, key: &str, base: &str) -> String {
        let mut description = base.to_string();
        if let Some(guidance_config) = self.guidance.as_ref() {
            let default_key = key.to_string();
            let gk = tool_config.guidance_key.as_ref().unwrap_or(&default_key);
            if let Some(guidance_text) = guidance_config.guidance_blocks.get(gk) {
                description = format!("{}\n\n{}", guidance_text, description);
            }
        }
        description
    }

    /// Resolves guidance-augmented description for a tool config by key.
    fn tool_description(&self, tool_config: &ToolConfig, key: &str) -> String {
        self.apply_guidance(tool_config, key, &tool_config.description)
    }

    /// Builds a guidance-augmented description from explicit text.
    fn tool_description_text(&self, tool_config: &ToolConfig, key: &str, base: &str) -> String {
        self.apply_guidance(tool_config, key, base)
    }

    /// Resolves a flattened tool name (e.g., `"file-tools_hello"`) to a parent
    /// config and the subcommand path portion. Tries every possible split position
    /// of `_` from left to right so that tool names containing underscores still
    /// work correctly (the config key match is authoritative).
    fn resolve_flattened_tool<'a>(
        tool_name: &str,
        configs: &'a HashMap<String, ToolConfig>,
    ) -> Option<(&'a ToolConfig, String)> {
        // Try splitting at each '_' from left to right
        for (idx, _) in tool_name.match_indices('_') {
            let parent = &tool_name[..idx];
            let sub_path = &tool_name[idx + 1..];
            if !sub_path.is_empty()
                && let Some(config) = configs.get(parent).filter(|c| c.subcommand.is_some())
            {
                return Some((config, sub_path.to_string()));
            }
        }
        None
    }

    /// Returns true if a configured tool should be exposed to the client
    /// given the current disclosure state. Centralises the filter so
    /// `list_tools()` and `list_tool_names()` cannot drift apart.
    fn is_config_visible_to_client(&self, config: &ToolConfig) -> bool {
        if BuiltinTool::from_name(&config.name).is_some() {
            return false;
        }
        if !config.enabled {
            tracing::debug!("Skipping disabled tool '{}'", config.name);
            return false;
        }
        true
    }

    /// Returns true if a hard-coded harness tool should be exposed to `client_type`.
    /// Only the tools [`BuiltinTool::is_harness_file_tool`] names are gated;
    /// everything else (the shell/
    /// operation tools, `agent`, `fetch_webpage`, `sandbox_grant`, `log_monitor`,
    /// …) has no native equivalent in any harness and stays visible everywhere.
    fn is_harness_tool_visible_to_client(name: &str, client_type: McpClientType) -> bool {
        if !BuiltinTool::from_name(name).is_some_and(BuiltinTool::is_harness_file_tool) {
            return true;
        }
        !client_type.has_native_file_tools()
    }

    /// Resolves a `tools/call` tool name to its config, returning the
    /// owned config and (for flattened subcommand names) the resolved
    /// subcommand path.
    fn find_tool_config(&self, tool_name: &str) -> Option<(ToolConfig, Option<String>)> {
        let configs_lock = self.configs.read();
        if let Some(config) = configs_lock.get(tool_name) {
            Some((config.clone(), None))
        } else {
            Self::resolve_flattened_tool(tool_name, &configs_lock)
                .map(|(parent, sub_path)| (parent.clone(), Some(sub_path)))
        }
    }

    #[allow(deprecated)]
    fn sync_override_from_config(
        subcommand_config: &crate::config::SubcommandConfig,
        tool_config: &ToolConfig,
    ) -> Option<bool> {
        subcommand_config.synchronous.or(tool_config.synchronous)
    }

    fn determine_execution_mode(
        &self,
        subcommand_config: &crate::config::SubcommandConfig,
        tool_config: &ToolConfig,
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> crate::adapter::ExecutionMode {
        let dynamic_blocking = arguments
            .get("blocking")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        #[allow(deprecated)]
        let sync_override = Self::sync_override_from_config(subcommand_config, tool_config);

        if self.force_synchronous || dynamic_blocking || sync_override == Some(true) {
            return crate::adapter::ExecutionMode::Synchronous;
        }
        if sync_override == Some(false) {
            return crate::adapter::ExecutionMode::AsyncResultPush;
        }
        let explicit_mode_str = arguments.get("execution_mode").and_then(|v| v.as_str());
        if explicit_mode_str == Some("Synchronous") {
            return crate::adapter::ExecutionMode::Synchronous;
        }
        crate::adapter::ExecutionMode::AsyncResultPush
    }

    fn sync_tool_progress_description(base_command: &str, working_directory: &str) -> String {
        format!("Execute {} in {}", base_command, working_directory)
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_sync_tool(
        &self,
        id: String,
        base_command: &str,
        working_directory: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        timeout: Option<u64>,
        subcommand_config: &crate::config::SubcommandConfig,
        progress_token: Option<rmcp::model::ProgressToken>,
        client_type: McpClientType,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let started_at = std::time::Instant::now();
        self.emit_vault_tool_call(&id, base_command, &Self::summarize_arguments(&arguments))
            .await;

        // Sync operations never enter the OperationMonitor, so the event
        // forwarder cannot see them — push start/final progress directly.
        let push_token = progress_token.filter(|_| self.effective_supports_progress(client_type));
        self.push_sync_start_progress(push_token.clone(), &peer, base_command, working_directory)
            .await;

        let result = self
            .adapter
            .execute_sync_in_dir(
                base_command,
                Some(arguments),
                working_directory,
                timeout,
                Some(subcommand_config),
            )
            .await;

        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.emit_vault_tool_complete(&id, result.is_ok(), duration_ms)
            .await;

        self.push_sync_final_progress(
            push_token,
            &peer,
            &id,
            base_command,
            working_directory,
            duration_ms,
            &result,
        )
        .await;

        match result {
            Ok(output) => Ok(handlers::common::text_result(output)),
            Err(e) => {
                let error_message = format!("Synchronous execution failed: {}", e);
                tracing::error!("{}", error_message);
                Err(handlers::common::mcp_internal(error_message))
            }
        }
    }

    /// Pushes the initial 0% progress notification for a sync tool call.
    /// `progress_token` must already be filtered by the client's progress
    /// support (see the `push_token` computation in callers); `None` is a
    /// no-op.
    async fn push_sync_start_progress(
        &self,
        progress_token: Option<rmcp::model::ProgressToken>,
        peer: &Peer<RoleServer>,
        base_command: &str,
        working_directory: &str,
    ) {
        let Some(token) = progress_token else {
            return;
        };
        progress_push::push_progress(
            peer,
            token,
            0.0,
            format!(
                "{base_command}: {}",
                Self::sync_tool_progress_description(base_command, working_directory)
            ),
            false,
        )
        .await;
    }

    /// Pushes the terminal 100% progress notification for a sync tool call,
    /// under the same gating as [`Self::push_sync_start_progress`]. No-op
    /// otherwise.
    #[allow(clippy::too_many_arguments)]
    async fn push_sync_final_progress(
        &self,
        progress_token: Option<rmcp::model::ProgressToken>,
        peer: &Peer<RoleServer>,
        id: &str,
        base_command: &str,
        working_directory: &str,
        duration_ms: u64,
        result: &Result<String, anyhow::Error>,
    ) {
        let Some(token) = progress_token else {
            return;
        };
        let (success, full_output) = match result {
            Ok(output) => (true, output.clone()),
            Err(e) => (false, format!("Error: {}", e)),
        };
        let message = progress_push::sync_final_message(
            id,
            base_command,
            &Self::sync_tool_progress_description(base_command, working_directory),
            working_directory,
            success,
            duration_ms,
            &full_output,
        );
        progress_push::push_progress(peer, token, 100.0, message, true).await;
    }

    /// Registers `op_id` as a progress-push destination when the client
    /// supplied a progress token. Owns the token + capability resolution so
    /// the five start paths (configured async tools, `run_terminal_command`
    /// sync-special/async, `log_monitor`, livelog) cannot drift apart.
    pub(crate) async fn register_progress_if_requested(
        &self,
        op_id: &str,
        peer: Peer<RoleServer>,
        progress_token: Option<rmcp::model::ProgressToken>,
        client_type: McpClientType,
    ) {
        if let Some(token) = progress_token {
            let progress_enabled = self.effective_supports_progress(client_type);
            self.progress_push
                .register(op_id, peer, token, client_type, progress_enabled)
                .await;
        }
    }

    /// Derives the per-operation log monitor configuration from a tool config's
    /// `monitor_level`/`monitor_stream` fields.
    fn log_monitor_config_from_tool(
        &self,
        config: &ToolConfig,
    ) -> Option<crate::log_monitor::LogMonitorConfig> {
        config.monitor_level.as_deref().map(|level_str| {
            let level = level_str
                .parse::<crate::log_monitor::LogLevel>()
                .unwrap_or(crate::log_monitor::LogLevel::Error);
            let stream = config
                .monitor_stream
                .as_deref()
                .and_then(|s| s.parse::<crate::log_monitor::MonitorStream>().ok())
                .unwrap_or(crate::log_monitor::MonitorStream::Stderr);
            crate::log_monitor::LogMonitorConfig {
                monitor_level: level,
                monitor_stream: stream,
                rate_limit_seconds: self.monitor_rate_limit_seconds,
            }
        })
    }

    /// Starts an async operation and reports the result: vault telemetry,
    /// progress registration, adapter start, inline completion window, and
    /// cleanup on a failed start. Shared by configured async tools and
    /// `run_terminal_command`'s async path; `vault_args_summary` and
    /// `map_start_error` carry the per-caller differences (argument summary
    /// shape and start-failure error mapping).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn call_async_tool(
        &self,
        tool_name: &str,
        id: String,
        base_command: &str,
        working_directory: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        timeout: Option<u64>,
        subcommand_config: &crate::config::SubcommandConfig,
        log_monitor_config: Option<crate::log_monitor::LogMonitorConfig>,
        vault_args_summary: &str,
        progress_token: Option<rmcp::model::ProgressToken>,
        client_type: McpClientType,
        peer: Peer<RoleServer>,
        wait: CallWait,
        map_start_error: impl FnOnce(&anyhow::Error) -> McpError,
    ) -> Result<CallToolResult, McpError> {
        self.emit_vault_tool_call(&id, tool_name, vault_args_summary)
            .await;
        let wait_peer = peer.clone();

        self.register_progress_if_requested(&id, peer, progress_token, client_type)
            .await;

        let job_id = self
            .adapter
            .execute_async_in_dir_with_options(
                tool_name,
                base_command,
                working_directory,
                crate::adapter::AsyncExecOptions {
                    id: Some(id.clone()),
                    args: Some(arguments),
                    timeout,
                    subcommand_config: Some(subcommand_config),
                    log_monitor_config,
                },
            )
            .await;

        match job_id {
            Ok(id) => {
                let (finished, still_running) = match wait {
                    // Async: wait out the inline window (SPEC R2.6.1) so fast
                    // commands answer without an `await` round-trip.
                    CallWait::Adaptive => {
                        let finished = handlers::common::try_automatic_async_completion(
                            &self.operation_monitor,
                            &id,
                            self.effective_request_budget(client_type),
                        )
                        .await;
                        (finished, String::new())
                    }
                    // Sync: wait for the result, as long as a default `await`
                    // on it could. Progress keeps flowing to the caller's token
                    // meanwhile, registered above.
                    CallWait::UntilDone => {
                        let (window, clamp_note) =
                            self.sync_call_wait(client_type, Some(wait_peer));
                        let finished = handlers::common::wait_for_completion(
                            &self.operation_monitor,
                            &id,
                            window,
                        )
                        .await;
                        let note = format!(
                            "\n\nStill running after {}s — the longest this client can \
                             hold one request open. It keeps running: call `await` with \
                             id `{id}` to collect the result.{}",
                            window.as_secs(),
                            clamp_note.unwrap_or_default()
                        );
                        (finished, note)
                    }
                };
                if let Some(result) = finished {
                    return Ok(result);
                }
                let hint = crate::tool_hints::preview(&id, tool_name);
                Ok(handlers::common::text_result(format!(
                    "AHMA ID: {id}{still_running}{hint}"
                )))
            }
            Err(e) => {
                // The operation never started, so no terminal event will
                // arrive to clean up the push registration.
                self.progress_push.unregister(&id).await;
                self.emit_vault_tool_complete(&id, false, 0).await;
                Err(map_start_error(&e))
            }
        }
    }
}

#[async_trait::async_trait]
impl ServerHandler for AhmaMcpService {
    fn get_info(&self) -> ServerInfo {
        let instructions =
            ahma_common::mcp_methods::server_instructions(self.execution_policy()).to_string();

        let mut tools_capability = ToolsCapability::default();
        tools_capability.list_changed = Some(true);
        let capabilities = ServerCapabilities::builder()
            .enable_tools_with(tools_capability)
            .build();

        let server_info = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .with_title(env!("CARGO_PKG_NAME"));

        ServerInfo::new(capabilities)
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(server_info)
            .with_instructions(instructions)
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.last_received_signal.store(
            ahma_common::keepalive::current_timestamp_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
        async move {
            tracing::info!("Client connected: {context:?}");

            // Detect and log client type for debugging
            let client_type = McpClientType::from_peer(&context.peer);
            tracing::info!(
                "Detected MCP client type: {} (progress notifications: {})",
                client_type.display_name(),
                if self.effective_supports_progress(client_type) {
                    "enabled"
                } else {
                    "disabled"
                }
            );

            // Publish the raw client identity so the daemon reporter can
            // re-register this instance with the hub under the client's name
            // (the TUI task tree groups work by who is driving it).
            if let Some(info) = context.peer.peer_info() {
                crate::daemon_reporter::set_client_identity(
                    info.client_info.name.clone(),
                    info.capabilities.sampling.is_some(),
                );
            }

            let peer = &context.peer;
            self.store_peer_handle_if_unset(peer);

            // Configure keepalive behavior based on peer type
            if matches!(client_type, McpClientType::Ahma) {
                self.is_ahma_peer
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }

            // Start keepalive heartbeat task
            ahma_common::keepalive::spawn_keepalive_task(
                Arc::new(self.clone()),
                self.last_received_signal.clone(),
                env!("CARGO_PKG_VERSION").to_string(),
                ahma_common::BUILD_ID.to_string(),
            );

            if self.defer_sandbox {
                tracing::info!("Sandbox deferred - waiting for roots/list_changed notification");
                return;
            }

            // Query client for workspace roots and configure sandbox (see
            // `should_skip_client_roots_sandbox_setup` for defer/CLI/test cases).
            if self.should_skip_client_roots_sandbox_setup() {
                return;
            }

            // Off the message loop, for the same reason as the
            // `roots/list_changed` path (SPEC R5.1.2): this queries the client
            // and waits for its reply, and anything the client pipelined behind
            // `initialized` would wait with it.
            self.spawn_sandbox_configuration(peer.clone());
        }
    }

    fn on_roots_list_changed(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.last_received_signal.store(
            ahma_common::keepalive::current_timestamp_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
        async move {
            tracing::info!("Received roots/list_changed notification");

            // This notification is sent by the HTTP bridge when SSE connects.
            // It signals that we can now safely call roots/list.
            self.spawn_sandbox_configuration(context.peer.clone());
        }
    }

    fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.last_received_signal.store(
            ahma_common::keepalive::current_timestamp_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
        async move {
            let request_id = format!("{:?}", notification.request_id);
            let reason = notification
                .reason
                .as_deref()
                .unwrap_or("Client-initiated cancellation");

            tracing::info!(
                "MCP protocol cancellation received: request_id={}, reason='{}'",
                request_id,
                reason
            );

            // CRITICAL FIX: Only cancel background operations, not synchronous MCP calls
            // This prevents the rmcp library from generating "Canceled: Canceled" messages
            // that get incorrectly processed as process cancellations.

            let active_ops = self.operation_monitor.get_all_active_operations().await;

            if active_ops.is_empty() {
                tracing::info!(
                    "No active operations found during MCP protocol cancellation (request_id: {})",
                    request_id
                );
                return;
            }

            let background_ops: Vec<_> = active_ops
                .iter()
                .filter(|op| {
                    if Self::is_sync_meta_tool_for_protocol_cancel(&op.tool_name) {
                        tracing::debug!(
                            "on_cancelled: skipping sync/meta tool '{}' (op {})",
                            op.tool_name,
                            op.id
                        );
                        return false;
                    }
                    true
                })
                .collect();

            if background_ops.is_empty() {
                tracing::info!(
                    "Found {} operations during MCP cancellation, but none are background processes. No cancellation needed.",
                    active_ops.len()
                );
                return;
            }

            tracing::info!(
                "Found {} background operations during MCP cancellation. Cancelling most recent background operation...",
                background_ops.len()
            );

            self.cancel_most_recent_background_op(&background_ops, &request_id, reason)
                .await;
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        self.last_received_signal.store(
            ahma_common::keepalive::current_timestamp_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
        async move {
            let client_type = McpClientType::from_peer(&context.peer);
            let mut tools = self.builtin_tools();
            tools.retain(|tool| Self::is_harness_tool_visible_to_client(&tool.name, client_type));
            tools.extend(self.visible_config_tools());

            let external_mgr = self.mcp_connections.read().await;
            append_external_mcp_tools(&mut tools, &external_mgr);

            Ok(ListToolsResult {
                meta: None,
                tools,
                next_cursor: None,
            })
        }
    }

    fn call_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        self.last_received_signal.store(
            ahma_common::keepalive::current_timestamp_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
        let span = tracing::info_span!("call_tool", tool = &*params.name);
        async move {
            let is_guard_active = self
                .harness_guard
                .try_lock()
                .map(|g| g.enabled)
                .unwrap_or(false);

            let mut tool_name = params.name;
            let mut tool_args = params.arguments;

            if is_guard_active
                && let Some(early) = self.harness_guard_preprocess(&mut tool_name, &mut tool_args)
            {
                return Ok(early);
            }

            // The loop detector is the only consumer of the (possibly healed)
            // arguments after dispatch, so only it pays for a clone.
            let loop_detector_args = if is_guard_active {
                tool_args.clone()
            } else {
                None
            };

            let mut run_params = CallToolRequestParams::new(tool_name.clone());
            run_params.arguments = tool_args;
            run_params.meta = params.meta;
            run_params.task = params.task;

            // Every tool call gates on the scope being settled (SPEC R5.1.2) —
            // here, once, rather than in each handler. The built-in file tools
            // (`write_file`, `read_file`, `list_dir`, …) validate paths against
            // the scope, so they need this exactly as much as the configured
            // tools and `run_terminal_command` did; only those two had it.
            // `tools/list` deliberately does NOT wait: a client must be able to
            // discover tools while the scope is still being decided.
            let builtin = BuiltinTool::from_name(tool_name.as_ref());
            if !builtin.is_some_and(BuiltinTool::is_sandbox_exempt) {
                self.guard_sandbox_ready_for_tool_calls().await?;
            }

            let result = self
                .dispatch_tool_call(tool_name.as_ref(), run_params, context)
                .await;

            if is_guard_active {
                self.record_result_in_loop_detector(
                    tool_name.as_ref(),
                    loop_detector_args.as_ref(),
                    &result,
                );
            }

            result
        }
        .instrument(span)
    }

    fn ping(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        self.last_received_signal.store(
            ahma_common::keepalive::current_timestamp_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
        std::future::ready(Ok(()))
    }

    fn on_custom_notification(
        &self,
        notification: rmcp::model::CustomNotification,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.apply_custom_notification(&notification.method, notification.params.as_ref());
        std::future::ready(())
    }
}

fn append_external_mcp_tools(
    tools: &mut Vec<Tool>,
    external_mgr: &crate::mcp_client::McpConnectionManager,
) {
    for ext_tool in external_mgr.aggregate_tools() {
        let description = ext_tool.description.clone();
        let input_schema = match ext_tool.input_schema.clone() {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let schema_arc = Arc::new(input_schema);
        tools.push(self_titled_tool(
            ext_tool.name.clone(),
            description.unwrap_or_default(),
            schema_arc,
        ));
    }
}

impl AhmaMcpService {
    async fn dispatch_tool_call(
        &self,
        tool_name: &str,
        run_params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let Some(builtin) = BuiltinTool::from_name(tool_name) else {
            return self.dispatch_configured_tool(run_params, context).await;
        };
        match builtin {
            BuiltinTool::Status => {
                self.handle_status(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::Await => {
                let mut caller = handlers::await_tool::AwaitCaller::from_context(&context);
                caller.push_channel_open = self.push_channel_open();
                self.handle_await_for_caller(run_params, caller).await
            }
            BuiltinTool::RunTerminalCommand => {
                self.handle_run_terminal_command(run_params, context).await
            }
            BuiltinTool::Cancel => {
                self.handle_cancel(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::SandboxGrant => {
                let client_type = McpClientType::from_peer(&context.peer);
                self.handle_sandbox_grant(run_params.arguments.unwrap_or_default(), client_type)
                    .await
            }
            BuiltinTool::LogsList => {
                self.handle_logs_list(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::LogsApprove => {
                self.handle_logs_approve(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::LogsRead => {
                self.handle_logs_read(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::LogsSearch => {
                self.handle_logs_search(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::Restart => {
                self.handle_restart(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::ReadFile => {
                self.handle_read_file(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::MultiEdit => {
                self.handle_multi_edit(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::ApplyPatch => {
                self.handle_apply_patch(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::ListDir => {
                self.handle_list_dir(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::FileSearch => {
                self.handle_file_search(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::GrepSearch => {
                self.handle_grep_search(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::FetchWebpage => {
                self.handle_fetch_webpage(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::WriteFile => {
                self.handle_write_file(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::ReplaceInFile => {
                self.handle_replace_in_file(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::Agent => {
                self.handle_agent(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::TodoWrite => {
                self.handle_todo_write(run_params.arguments.unwrap_or_default())
                    .await
            }
            BuiltinTool::LogMonitor => {
                self.handle_log_monitor(run_params.arguments.unwrap_or_default(), context)
                    .await
            }
        }
    }
    /// Heals the tool name and arguments via the harness guard, and checks for
    /// repeated identical failures (loop detection). Returns `Some(result)` when a
    /// loop is detected — the caller should return that result immediately.
    /// Returns `None` to proceed normally. Mutates `tool_name` and `tool_args` in
    /// place when healing is applied.
    fn harness_guard_preprocess(
        &self,
        tool_name: &mut std::borrow::Cow<'static, str>,
        tool_args: &mut Option<serde_json::Map<String, Value>>,
    ) -> Option<CallToolResult> {
        use crate::harness_guard::{GuardContext, GuardOutcome};

        // Assemble the known-tool set (hard-coded + configured) for name healing.
        // The names come from a cache keyed to `configs`, so the lock is released
        // before the guard pipeline runs and no per-call allocation is needed;
        // `known_tools` then borrows from `config_names`.
        let config_names = self.config_tool_names();
        let mut known_tools: Vec<&str> = BuiltinTool::names().collect();
        known_tools.extend(config_names.iter().map(String::as_str));

        // Run the guard pipeline over a mutable copy, then write any healing back.
        let had_args = tool_args.is_some();
        let mut name = tool_name.to_string();
        let mut args = tool_args.take().unwrap_or_default();
        let ctx = GuardContext {
            known_tools: &known_tools,
        };
        let outcome = match self.harness_guard.try_lock() {
            Ok(guard) => guard.inspect(&ctx, &mut name, &mut args),
            Err(_) => GuardOutcome::Proceed,
        };
        if name.as_str() != &**tool_name {
            *tool_name = std::borrow::Cow::Owned(name);
        }
        // Preserve a `None` payload: handlers distinguish "no arguments" from an
        // empty object. Only
        // restore args if they existed originally or the pipeline added some.
        if had_args || !args.is_empty() {
            *tool_args = Some(args);
        }

        match outcome {
            GuardOutcome::Block(msg) => Some(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(msg),
            ])),
            GuardOutcome::Proceed => None,
        }
    }

    /// Notify the guard pipeline of a completed tool call so stateful guards
    /// (loop detection) can track repeated identical failures.
    fn record_result_in_loop_detector(
        &self,
        tool_name: &str,
        tool_args: Option<&serde_json::Map<String, Value>>,
        result: &Result<CallToolResult, McpError>,
    ) {
        let failed = match result {
            Ok(res) => res.is_error.unwrap_or(false),
            Err(_) => true,
        };
        let empty = serde_json::Map::new();
        let args = tool_args.unwrap_or(&empty);
        if let Ok(guard) = self.harness_guard.try_lock() {
            guard.observe(tool_name, args, failed);
        }
    }

    /// Gate a `tools/call` on the sandbox scope being settled (SPEC R5.1.2, R5.2).
    ///
    /// Waits out an in-flight configuration first — scope is decided off the
    /// message loop, so "not ready yet" is a matter of milliseconds in the
    /// normal case and this turns what would be a spurious denial into a
    /// correct execution. Only if no scope exists at all after that does the
    /// call get the retryable error.
    async fn guard_sandbox_ready_for_tool_calls(&self) -> Result<(), McpError> {
        self.wait_for_sandbox_configuration(SANDBOX_SETTLE_WAIT)
            .await;
        if self.adapter.sandbox().is_ready_for_tool_calls() {
            return Ok(());
        }
        // Two distinct refusals share the -32001 code:
        //
        // * Provisional scope, commit still pending (R5.1.2.1): the negotiation
        //   is running but did not settle inside the wait budget. Retrying is
        //   the whole remediation. Executing anyway is not an option — the one
        //   provisional source that matters, the container root, is *wider*
        //   than the scope about to be committed.
        //
        // * No scope to be had (R5.2.3): refuse *with the remediation*. The old
        //   message said only "retry after roots/list completes", which is a lie
        //   to the client that most needs this error — one that already answered
        //   `roots/list` with `{"roots": []}` (R5.2.7) and will never send
        //   another. It waits, retries, and eventually leaves for an unsandboxed
        //   terminal.
        let scopes_pending = !self.adapter.sandbox().scopes().is_empty();
        let error_message = if scopes_pending {
            "ahma's sandbox scope is still being negotiated (the commit has not settled \
             yet), so it will not run anything at this instant. Retry shortly — this \
             resolves as soon as the scope is committed."
                .to_string()
        } else {
            "ahma has no sandbox scope, so it will not run anything yet. \
             If your editor is still opening a workspace this resolves by itself in a moment — \
             retry once. If your client reports no workspace roots (Antigravity and LM Studio \
             answer `roots/list` with an empty list), it never will, and one of these is \
             needed: open a workspace folder in the client; or set `[sandbox] container_root = \
             \"~/github\"` in ~/.ahma/settings.toml, naming the directory that holds your \
             projects; or start ahma with `--sandbox-scope <project-dir>`. ahma does not pick \
             a directory for you — running in one nobody chose is what this refusal exists to \
             prevent."
                .to_string()
        };
        tracing::warn!("{}", error_message);
        Err(McpError::new(
            rmcp::model::ErrorCode(-32001),
            error_message,
            Some(serde_json::json!({
                "kind": if scopes_pending { "sandbox_scope_uncommitted" }
                        else { "sandbox_scope_missing" },
                "lock_state": ahma_common::state_machine::FsmState::name(
                    &self.adapter.sandbox().lock_state()),
                "roots_received": self.adapter.sandbox().roots_received(),
                "remediation": if scopes_pending {
                    "Retry shortly; the scope commit is in flight."
                } else {
                    "Open a workspace folder in the client, set `[sandbox] container_root` \
                     in ~/.ahma/settings.toml, or start ahma with `--sandbox-scope \
                     <project-dir>`."
                },
            })),
        ))
    }

    fn parse_llm_provider(
        &self,
        arguments: &serde_json::Map<String, Value>,
    ) -> crate::config::LlmProviderConfig {
        let llm_base_url = handlers::common::opt_str(arguments, "llm_base_url");
        let llm_model = handlers::common::opt_str(arguments, "llm_model");
        let llm_api_key = handlers::common::opt_str(arguments, "llm_api_key");

        if let Some(base_url) = llm_base_url
            && let Some(model) = llm_model
        {
            return crate::config::LlmProviderConfig {
                base_url,
                model,
                api_key: llm_api_key,
            };
        }

        self.fallback_llm_provider()
    }

    fn fallback_llm_provider(&self) -> crate::config::LlmProviderConfig {
        let configs_lock = self.configs.read();
        let mut found_provider = None;
        for config in configs_lock.values() {
            if let Some(livelog) = &config.livelog {
                found_provider = Some(livelog.llm_provider.clone());
                break;
            }
        }
        drop(configs_lock);

        found_provider.unwrap_or_else(|| crate::config::LlmProviderConfig {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "llama3.2".to_string(),
            api_key: None,
        })
    }

    pub async fn handle_log_monitor(
        &self,
        arguments: serde_json::Map<String, Value>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        use std::sync::atomic::Ordering;

        let file_path_str = handlers::common::require_str(
            &arguments,
            "file_path",
            "file_path parameter is required",
        )?;

        let detection_prompt = handlers::common::opt_str(&arguments, "detection_prompt")
            .unwrap_or_else(|| "Identify errors or warnings".to_string());

        let path = std::path::Path::new(&file_path_str);
        let safe_path =
            self.adapter.sandbox().validate_path(path).map_err(|e| {
                handlers::common::mcp_invalid_params(format!("Invalid file path: {e}"))
            })?;

        let llm_provider = self.parse_llm_provider(&arguments);

        static NEXT_LOG_MON_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let op_id = format!("logmon_{}", NEXT_LOG_MON_ID.fetch_add(1, Ordering::SeqCst));
        let operation = Operation::new_with_timeout(
            op_id.clone(),
            "log_monitor".to_string(),
            format!("Monitoring log: {}", file_path_str),
            None,
            None,
        );
        self.operation_monitor.add_operation(operation).await;

        let monitor = self.operation_monitor.clone();
        let cancellation_token = monitor
            .get_operation(&op_id)
            .await
            .unwrap()
            .cancellation_token;

        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);
        self.register_progress_if_requested(
            &op_id,
            context.peer.clone(),
            progress_token,
            client_type,
        )
        .await;

        let op_id_clone = op_id.clone();
        let monitor_clone = monitor.clone();
        let llm_service_clone = self.llm_service.clone();
        tokio::spawn(async move {
            monitor_clone
                .update_status(&op_id_clone, OperationStatus::InProgress, None)
                .await;

            crate::livelog::run_file_monitor_pipeline(
                &op_id_clone,
                safe_path,
                detection_prompt,
                llm_provider,
                cancellation_token,
                monitor_clone.clone(),
                llm_service_clone,
            )
            .await;

            monitor_clone
                .update_status(&op_id_clone, OperationStatus::Completed, None)
                .await;
        });

        Ok(handlers::common::text_result(format!(
            "Log monitor started on '{}'. Operation ID: {}",
            file_path_str, op_id
        )))
    }

    fn resolve_configured_tool(
        &self,
        tool_name: &str,
    ) -> Result<(ToolConfig, Option<String>), McpError> {
        let (config, flattened_subcommand) = match self.find_tool_config(tool_name) {
            Some(pair) => pair,
            None => {
                let error_message = format!("Tool '{}' not found.", tool_name);
                tracing::error!("{}", error_message);
                return Err(McpError::invalid_params(
                    error_message,
                    Some(serde_json::json!({ "tool_name": tool_name })),
                ));
            }
        };

        if !config.enabled {
            let error_message = format!(
                "Tool '{}' is unavailable because its runtime availability probe failed",
                tool_name
            );
            tracing::error!("{}", error_message);
            return Err(McpError::invalid_request(error_message, None));
        }

        Ok((config, flattened_subcommand))
    }

    fn get_extension_key(&self, config: &ToolConfig) -> Option<String> {
        if config.tool_type != Some(crate::config::ToolType::Extension) {
            return None;
        }
        let handlers = self.extension_handlers.read();
        for key in config.extra.keys() {
            if handlers.contains_key(key) {
                return Some(key.clone());
            }
        }
        None
    }

    async fn dispatch_resolved_configured_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
        config: ToolConfig,
        flattened_subcommand: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let handler_opt = self
            .get_extension_key(&config)
            .and_then(|key| self.extension_handlers.read().get(&key).cloned());
        if let Some(handler) = handler_opt {
            return handler
                .call(
                    params,
                    context,
                    config,
                    self.adapter.clone(),
                    self.operation_monitor.clone(),
                )
                .await;
        }

        if config.sequence.is_some() {
            #[allow(deprecated)]
            let opted_out = config.synchronous == Some(false)
                || params
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("blocking"))
                    .and_then(|v| v.as_bool())
                    == Some(false);
            let wait = self.sequence_wait(&context, opted_out);
            return sequence::handle_sequence_tool(
                &self.adapter,
                &self.progress_push,
                &self.configs,
                &config,
                params,
                context,
                self.force_progress_notifications_override(),
                wait,
            )
            .await;
        }

        if config.tool_type == Some(crate::config::ToolType::Livelog) {
            return self.handle_livelog_call(&config, &params, &context).await;
        }

        self.dispatch_subcommand_tool(params, context, config, flattened_subcommand)
            .await
    }

    /// Routes a `tools/call` for a configured (non-built-in) tool. Resolves
    /// the tool config and dispatches by tool type (sequence / livelog /
    /// subcommand). The sandbox gate has already run at the single dispatch
    /// point (SPEC R5.1.2.2) — the per-handler gate this method used to carry
    /// was the pre-R5.1.2.2 leftover that let the built-in file tools go
    /// ungated while double-gating this path.
    async fn dispatch_configured_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.name.contains("::")
            && let Some(result) = self.try_dispatch_external_mcp_tool(&params).await
        {
            return result;
        }

        let (config, flattened_subcommand) = self.resolve_configured_tool(&params.name)?;
        self.dispatch_resolved_configured_tool(params, context, config, flattened_subcommand)
            .await
    }

    /// Attempts to route a `"server::tool"`-style call to a connected external
    /// MCP server. Returns `None` when the name doesn't resolve to a known,
    /// connected server, so the caller falls through to local tool resolution.
    async fn try_dispatch_external_mcp_tool(
        &self,
        params: &CallToolRequestParams,
    ) -> Option<Result<CallToolResult, McpError>> {
        let mgr = {
            let guard = self.mcp_connections.read().await;
            guard.clone()
        };
        let (server_name, _) = mgr.resolve_tool_name(&params.name)?;
        if !mgr.servers.iter().any(|s| s.name == server_name) {
            return None;
        }

        let args_val = serde_json::Value::Object(params.arguments.clone().unwrap_or_default());
        Some(match mgr.call_tool(&params.name, args_val).await {
            Ok((output, is_error)) => {
                let content = vec![rmcp::model::ContentBlock::text(output)];
                Ok(if is_error {
                    CallToolResult::error(content)
                } else {
                    CallToolResult::success(content)
                })
            }
            Err(e) => Err(handlers::common::mcp_internal(format!(
                "External tool call failed: {e}"
            ))),
        })
    }

    fn resolve_subcommand<'a>(
        &self,
        config: &'a ToolConfig,
        tool_name: &str,
        arguments: &mut serde_json::Map<String, serde_json::Value>,
        flattened_subcommand: Option<String>,
    ) -> Result<(&'a crate::config::SubcommandConfig, Vec<String>), McpError> {
        let subcommand_name = flattened_subcommand.or_else(|| {
            arguments
                .remove("subcommand")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        });

        match subcommand::find_subcommand_config_from_args(config, subcommand_name.clone()) {
            Some(result) => Ok(result),
            None => Err(Self::subcommand_not_found_error(
                tool_name,
                config,
                subcommand_name,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_subcommand_command(
        &self,
        tool_name: &str,
        base_command: &str,
        working_directory: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        timeout: Option<u64>,
        subcommand_config: &crate::config::SubcommandConfig,
        config: &ToolConfig,
        context: RequestContext<RoleServer>,
        execution_mode: crate::adapter::ExecutionMode,
    ) -> Result<CallToolResult, McpError> {
        let counter_val = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let id =
            crate::utils::operation::generate_id_with_details(counter_val, tool_name, base_command);
        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);

        match execution_mode {
            crate::adapter::ExecutionMode::Synchronous => {
                self.call_sync_tool(
                    id,
                    base_command,
                    working_directory,
                    arguments,
                    timeout,
                    subcommand_config,
                    progress_token,
                    client_type,
                    context.peer.clone(),
                )
                .await
            }
            crate::adapter::ExecutionMode::AsyncResultPush => {
                let vault_args_summary = Self::summarize_arguments(&arguments);
                // In sync mode a tool can still ask to return early: MTDF
                // `synchronous: false`, or `blocking: false` on the call.
                #[allow(deprecated)]
                let opted_out = Self::sync_override_from_config(subcommand_config, config)
                    == Some(false)
                    || arguments.get("blocking").and_then(|v| v.as_bool()) == Some(false);
                let wait = self.call_wait(opted_out);
                self.call_async_tool(
                    tool_name,
                    id,
                    base_command,
                    working_directory,
                    arguments,
                    timeout,
                    subcommand_config,
                    self.log_monitor_config_from_tool(config),
                    &vault_args_summary,
                    progress_token,
                    client_type,
                    context.peer.clone(),
                    wait,
                    |e| {
                        let error_message =
                            format!("Failed to start asynchronous operation: {}", e);
                        tracing::error!("{}", error_message);
                        handlers::common::mcp_internal(error_message)
                    },
                )
                .await
            }
        }
    }

    /// Resolves the subcommand from arguments, then dispatches either as a
    /// subcommand sequence or a regular sync/async execution.
    async fn dispatch_subcommand_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
        config: ToolConfig,
        flattened_subcommand: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let tool_name = params.name.to_string();
        let mut arguments = params.arguments.clone().unwrap_or_default();

        let (subcommand_config, command_parts) = match self.resolve_subcommand(
            &config,
            &tool_name,
            &mut arguments,
            flattened_subcommand,
        ) {
            Ok(res) => res,
            Err(e) => return Err(e),
        };

        if subcommand_config.sequence.is_some() {
            #[allow(deprecated)]
            let opted_out = Self::sync_override_from_config(subcommand_config, &config)
                == Some(false)
                || arguments.get("blocking").and_then(|v| v.as_bool()) == Some(false);
            let wait = self.sequence_wait(&context, opted_out);
            return sequence::handle_subcommand_sequence(
                &self.adapter,
                &self.progress_push,
                &config,
                subcommand_config,
                params,
                context,
                self.force_progress_notifications_override(),
                wait,
            )
            .await;
        }

        let base_command = command_parts.join(" ");
        // Where the command runs, and who decided that (SPEC R5.2.8 / R5.4).
        // Shared with `run_terminal_command`: substituting a directory is a scope
        // decision, and the rule binds every surface that runs a command — this
        // one used to substitute silently while the shell handler disclosed.
        let working_directory =
            handlers::working_directory::resolve(self.adapter.sandbox(), &tool_name, &arguments)?;

        if let Some(staged_result) = self
            .maybe_stage_configured_delete(&base_command, &working_directory.path, &arguments)
            .await?
        {
            return Ok(working_directory.disclose(staged_result));
        }

        let timeout = arguments.get("timeout_seconds").and_then(|v| v.as_u64());
        let execution_mode = self.determine_execution_mode(subcommand_config, &config, &arguments);

        match self
            .execute_subcommand_command(
                &tool_name,
                &base_command,
                &working_directory.path,
                arguments,
                timeout,
                subcommand_config,
                &config,
                context,
                execution_mode,
            )
            .await
        {
            Ok(result) => Ok(working_directory.disclose(result)),
            Err(e) => Err(working_directory.disclose_error(e)),
        }
    }

    async fn handle_livelog_call(
        &self,
        config: &ToolConfig,
        params: &CallToolRequestParams,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let params_map = params.arguments.clone().unwrap_or_default();
        let op_id = format!("livelog_{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);
        self.register_progress_if_requested(
            &op_id,
            context.peer.clone(),
            progress_token,
            client_type,
        )
        .await;
        match handlers::livelog_tool::handle_livelog_start(
            op_id.clone(),
            config,
            &params_map,
            self.operation_monitor.clone(),
            self.adapter.sandbox_arc(),
            self.llm_service.clone(),
        )
        .await
        {
            Ok(started_id) => Ok(handlers::common::text_result(format!(
                "Live log monitoring started. Operation ID: {started_id}\n\
                 Use `status` or `await` to check progress, `cancel` to stop."
            ))),
            Err(e) => {
                self.progress_push.unregister(&op_id).await;
                let msg = format!("Failed to start livelog '{}': {}", config.name, e);
                tracing::error!("{}", msg);
                Err(handlers::common::mcp_internal(msg))
            }
        }
    }

    fn subcommand_not_found_error(
        tool_name: &str,
        config: &ToolConfig,
        subcommand_name: Option<String>,
    ) -> McpError {
        let has_subcommands = config.subcommand.is_some();
        let num_subcommands = config.subcommand.as_ref().map(|s| s.len()).unwrap_or(0);
        let subcommand_names: Vec<String> = config
            .subcommand
            .as_ref()
            .map(|subs| {
                subs.iter()
                    .map(|s| format!("{} (enabled={})", s.name, s.enabled))
                    .collect()
            })
            .unwrap_or_default();
        let error_message = format!(
            "Subcommand '{:?}' for tool '{}' not found or invalid. Tool enabled={}, has_subcommands={}, num_subcommands={}, available_subcommands={:?}",
            subcommand_name,
            tool_name,
            config.enabled,
            has_subcommands,
            num_subcommands,
            subcommand_names
        );
        tracing::error!("{}", error_message);
        McpError::invalid_params(
            error_message,
            Some(serde_json::json!({ "tool_name": tool_name, "subcommand": subcommand_name })),
        )
    }

    async fn cancel_most_recent_background_op(
        &self,
        background_ops: &[&crate::operation_monitor::Operation],
        request_id: &str,
        reason: &str,
    ) {
        let Some(most_recent_bg_op) = background_ops.last() else {
            return;
        };
        let enhanced_reason = format!(
            "MCP protocol cancellation (request_id: {}, reason: '{}')",
            request_id, reason
        );
        let cancelled = self
            .operation_monitor
            .cancel_operation_with_reason(&most_recent_bg_op.id, Some(enhanced_reason.clone()))
            .await;
        if cancelled {
            tracing::info!(
                "Successfully cancelled background operation '{}' due to MCP protocol cancellation: {}",
                most_recent_bg_op.id,
                enhanced_reason
            );
        } else {
            tracing::warn!(
                "Failed to cancel background operation '{}' for MCP protocol cancellation",
                most_recent_bg_op.id
            );
        }
    }

    /// Returns the list of tool names that `list_tools()` would return,
    /// without requiring a `RequestContext`.
    ///
    /// This is useful for testing and introspection.
    pub fn list_tool_names(&self) -> Vec<String> {
        // Reading the names off the enum is far cheaper than rebuilding every
        // builtin's JSON schema just to list them, and cannot disagree with
        // what `builtin_tools()` constructs — both are driven by the same
        // `BuiltinTool::ALL`.
        let mut names: Vec<String> = BuiltinTool::names().map(str::to_string).collect();

        let configs_lock = self.configs.read();
        for config in configs_lock.values() {
            if self.is_config_visible_to_client(config) {
                names.push(config.name.clone());
            }
        }
        names
    }
}

impl AhmaMcpService {
    /// Fill the session-health fields of an outgoing heartbeat (R8.8.4):
    /// `pending_grants` from the session's grant coordinator, so a slow-polling
    /// ahma peer converges even if it missed the `grant_pending` event.
    /// (`reconnects` is owned by the stdio proxy, which overlays it in transit.)
    fn enrich_heartbeat(&self, payload: &mut ahma_common::keepalive::HeartbeatPayload) {
        if let Some(coordinator) = self.grant_coordinator.read().as_ref() {
            payload.pending_grants = coordinator.pending_count() as u32;
        }
    }
}

impl ahma_common::keepalive::KeepAlive for AhmaMcpService {
    async fn send_standard_ping(&self) -> anyhow::Result<()> {
        let peer_opt = self.peer.read().clone();
        if let Some(peer) = peer_opt {
            peer.send_request(rmcp::model::ServerRequest::PingRequest(Default::default()))
                .await?;
        }
        Ok(())
    }

    async fn send_enhanced_heartbeat(
        &self,
        mut payload: ahma_common::keepalive::HeartbeatPayload,
    ) -> anyhow::Result<()> {
        self.enrich_heartbeat(&mut payload);
        let peer_opt = self.peer.read().clone();
        if let Some(peer) = peer_opt {
            let params = serde_json::to_value(payload)?;

            peer.send_notification(rmcp::model::ServerNotification::CustomNotification(
                rmcp::model::CustomNotification::new(
                    ahma_common::mcp_methods::HEARTBEAT_METHOD,
                    Some(params),
                ),
            ))
            .await?;
        }
        Ok(())
    }

    fn time_since_last_received(&self) -> std::time::Duration {
        let last = self
            .last_received_signal
            .load(std::sync::atomic::Ordering::Relaxed);
        let now = ahma_common::keepalive::current_timestamp_ms();
        std::time::Duration::from_millis(now.saturating_sub(last))
    }

    fn heartbeat_timeout(&self) -> std::time::Duration {
        ahma_common::timeouts::TestTimeouts::scale(std::time::Duration::from_secs(60))
    }

    fn is_ahma_peer(&self) -> bool {
        self.is_ahma_peer.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn on_timeout(&self) {
        tracing::warn!(
            "AhmaMcpService keepalive timeout! Exiting process to allow bridge to cleanup."
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    // ==================== force_synchronous inheritance tests ====================

    use super::*;
    use crate::config::{SubcommandConfig, ToolConfig, ToolHints};
    use crate::operation_monitor::{MonitorConfig, Operation, OperationMonitor, OperationStatus};
    use ahma_common::timeouts::TestTimeouts;

    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    async fn make_service_with_monitor(
        monitor: Arc<OperationMonitor>,
        guidance: Arc<Option<GuidanceConfig>>,
    ) -> AhmaMcpService {
        // Adapter is required by the service but not used by these unit tests.
        let adapter =
            crate::test_utils::client::create_test_config(Path::new(".")).expect("adapter");
        let configs: Arc<HashMap<String, ToolConfig>> = Arc::new(HashMap::new());
        AhmaMcpService::new(adapter, monitor, configs, guidance, false, false)
            .await
            .expect("service")
    }

    async fn make_service() -> AhmaMcpService {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        make_service_with_monitor(monitor, Arc::new(None)).await
    }

    fn call_tool_params(name: &str, args: serde_json::Value) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = args.as_object().cloned() {
            params = params.with_arguments(arguments);
        }
        params
    }

    fn first_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn handle_status_empty_shows_zero_counts() {
        let service = make_service().await;
        let result = service
            .handle_status(serde_json::Map::new())
            .await
            .expect("status result");
        let text = first_text(&result);
        assert!(text.contains("Operations status:"));
        assert!(text.contains("0 active"));
        assert!(text.contains("0 completed"));
    }

    #[tokio::test]
    async fn handle_status_filters_by_tools_and_id() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        // Active operation
        let op_active = Operation::new(
            "op_active".to_string(),
            "alpha_tool".to_string(),
            "desc".to_string(),
            None,
        );
        monitor.add_operation(op_active).await;

        // Completed operation
        let op_completed = Operation::new(
            "op_completed".to_string(),
            "beta_tool".to_string(),
            "desc".to_string(),
            None,
        );
        monitor.add_operation(op_completed).await;
        monitor
            .update_status(
                "op_completed",
                OperationStatus::Completed,
                Some(json!({"ok": true})),
            )
            .await;

        // Filter by tool prefix
        let args = json!({"tools": "alpha"}).as_object().unwrap().clone();
        let result = service.handle_status(args).await.expect("status");
        let text = first_text(&result);
        assert!(text.contains("Operations status for 'alpha': 1 active, 0 completed"));
        assert!(
            result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .any(|t| t.text.contains("=== ACTIVE OPERATIONS ==="))
        );

        // Filter by specific operation id
        let args = json!({"id": "op_active"}).as_object().unwrap().clone();
        let result = service.handle_status(args).await.expect("status");
        let text = first_text(&result);
        assert!(text.contains("Operation 'op_active' found"));
    }

    #[tokio::test]
    async fn handle_cancel_requires_id_or_all() {
        let service = make_service().await;
        let err = service
            .handle_cancel(serde_json::Map::new())
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("either `id` or `all: true` is required"));
    }

    #[tokio::test]
    async fn handle_cancel_rejects_non_string_id() {
        let service = make_service().await;
        let args = json!({"id": 123}).as_object().unwrap().clone();
        let err = service.handle_cancel(args).await.unwrap_err();
        assert!(format!("{err:?}").contains("id must be a string"));
    }

    #[tokio::test]
    async fn handle_cancel_success_includes_hint_block() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let op = Operation::new(
            "op_to_cancel".to_string(),
            "alpha_tool".to_string(),
            "desc".to_string(),
            None,
        );
        monitor.add_operation(op).await;

        let args = json!({"id": "op_to_cancel", "reason": "because"})
            .as_object()
            .unwrap()
            .clone();
        let result = service.handle_cancel(args).await.expect("cancel");
        let text = first_text(&result);
        assert!(text.contains("has been cancelled successfully"));
        assert!(text.contains("reason='because'"));
        assert!(
            result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .any(|t| t.text.contains("\"tool_hint\""))
        );
    }

    #[tokio::test]
    async fn handle_cancel_all_cancels_every_in_flight_operation() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        for id in ["op_x", "op_y"] {
            let mut op = Operation::new(id.to_string(), "t".to_string(), "d".to_string(), None);
            op.state = OperationStatus::InProgress;
            monitor.add_operation(op).await;
        }

        let args = json!({"all": true, "reason": "stop everything"})
            .as_object()
            .unwrap()
            .clone();
        let result = service.handle_cancel(args).await.expect("cancel all");
        let text = first_text(&result);
        assert!(
            text.contains("Cancelled 2 in-flight operation"),
            "got: {text}"
        );
        assert!(text.contains("reason='stop everything'"));
        assert!(
            monitor.get_all_active_operations().await.is_empty(),
            "cancel-all must leave no active operations"
        );
    }

    #[tokio::test]
    async fn handle_cancel_all_with_nothing_running_is_graceful() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let args = json!({"all": true}).as_object().unwrap().clone();
        let result = service.handle_cancel(args).await.expect("cancel all");
        assert!(first_text(&result).contains("No in-flight operations to cancel"));
    }

    #[tokio::test]
    async fn handle_cancel_terminal_operation_reports_already_terminal() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let mut op = Operation::new(
            "op_terminal".to_string(),
            "alpha_tool".to_string(),
            "desc".to_string(),
            None,
        );
        op.state = OperationStatus::Completed;
        monitor.add_operation(op).await;

        let args = json!({"id": "op_terminal"}).as_object().unwrap().clone();
        let result = service.handle_cancel(args).await.expect("cancel");
        let text = first_text(&result);
        assert!(text.contains("already completed"));
    }

    #[tokio::test]
    async fn handle_cancel_operation_not_found() {
        let service = make_service().await;
        let args = json!({"id": "op_nonexistent_xyz"})
            .as_object()
            .unwrap()
            .clone();
        let result = service.handle_cancel(args).await.expect("cancel");
        let text = first_text(&result);
        assert!(text.contains("not found"));
        assert!(text.contains("op_nonexistent_xyz"));
    }

    #[tokio::test]
    async fn handle_cancel_success_without_reason_uses_default_message() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let op = Operation::new(
            "op_no_reason".to_string(),
            "test_tool".to_string(),
            "desc".to_string(),
            None,
        );
        monitor.add_operation(op).await;

        let args = json!({"id": "op_no_reason"}).as_object().unwrap().clone();
        let result = service.handle_cancel(args).await.expect("cancel");
        let text = first_text(&result);
        assert!(text.contains("has been cancelled successfully"));
        assert!(text.contains("No reason provided (default: user-initiated)"));
    }

    #[tokio::test]
    async fn handle_cancel_already_failed_reports_failed() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let mut op = Operation::new(
            "op_failed".to_string(),
            "test_tool".to_string(),
            "desc".to_string(),
            None,
        );
        op.state = OperationStatus::Failed;
        monitor.add_operation(op).await;

        let args = json!({"id": "op_failed"}).as_object().unwrap().clone();
        let result = service.handle_cancel(args).await.expect("cancel");
        let text = first_text(&result);
        assert!(text.contains("already failed"));
    }

    #[tokio::test]
    async fn handle_cancel_already_cancelled_reports_cancelled() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let mut op = Operation::new(
            "op_cancelled".to_string(),
            "test_tool".to_string(),
            "desc".to_string(),
            None,
        );
        op.state = OperationStatus::Cancelled;
        monitor.add_operation(op).await;

        let args = json!({"id": "op_cancelled"}).as_object().unwrap().clone();
        let result = service.handle_cancel(args).await.expect("cancel");
        let text = first_text(&result);
        assert!(text.contains("already cancelled"));
    }

    #[tokio::test]
    async fn handle_cancel_already_timed_out_reports_timed_out() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let mut op = Operation::new(
            "op_timed_out".to_string(),
            "test_tool".to_string(),
            "desc".to_string(),
            None,
        );
        op.state = OperationStatus::TimedOut;
        monitor.add_operation(op).await;

        let args = json!({"id": "op_timed_out"}).as_object().unwrap().clone();
        let result = service.handle_cancel(args).await.expect("cancel");
        let text = first_text(&result);
        assert!(text.contains("timed out"));
    }

    #[tokio::test]
    async fn handle_await_id_not_found_reports_not_found() {
        let service = make_service().await;
        let params = call_tool_params("await", json!({"id": "op_missing"}));
        let result = service.handle_await(params).await.expect("await result");
        assert!(first_text(&result).contains("Operation op_missing not found"));
    }

    #[tokio::test]
    async fn handle_await_id_in_history_reports_already_completed() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let op_id = "op_done".to_string();
        let op = Operation::new(
            op_id.clone(),
            "demo_tool".to_string(),
            "desc".to_string(),
            None,
        );
        monitor.add_operation(op).await;
        monitor
            .update_status(
                &op_id,
                OperationStatus::Completed,
                Some(json!({"ok": true})),
            )
            .await;

        let params = call_tool_params("await", json!({"id": op_id}));
        let result = service.handle_await(params).await.expect("await result");
        assert!(first_text(&result).contains("already completed"));
        // Completed op details should be included as a JSON block in content.
        assert!(
            result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .any(|t| t.text.contains("\"tool_name\": \"demo_tool\""))
        );
    }

    #[tokio::test]
    async fn handle_await_no_pending_operations_returns_fast_message() {
        let service = make_service().await;
        let params = call_tool_params("await", json!({}));
        let result = service.handle_await(params).await.expect("await result");
        assert_eq!(first_text(&result), "No pending operations to await for.");
    }

    #[tokio::test]
    async fn handle_await_filtered_no_pending_but_recently_completed_lists_history() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let op_id = "op_recent".to_string();
        let op = Operation::new(
            op_id.clone(),
            "alpha_tool".to_string(),
            "desc".to_string(),
            None,
        );
        monitor.add_operation(op).await;
        monitor
            .update_status(
                &op_id,
                OperationStatus::Completed,
                Some(json!({"ok": true})),
            )
            .await;

        let params = call_tool_params("await", json!({"tools": "alpha"}));
        let result = service.handle_await(params).await.expect("await result");
        let text = first_text(&result);
        assert!(text.contains("No pending operations for tools: alpha"));
        assert!(
            result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .any(|t| t.text.contains("\"id\": \"op_recent\""))
        );
    }

    #[tokio::test]
    async fn calculate_intelligent_timeout_uses_max_of_default_and_ops() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let mut op = Operation::new(
            "op_long".to_string(),
            "beta_tool".to_string(),
            "desc".to_string(),
            None,
        );
        op.timeout_duration = Some(Duration::from_secs(600));
        monitor.add_operation(op).await;

        let t_any = service.calculate_intelligent_timeout(&[], 600.0).await;
        assert!(t_any >= 600.0);

        let t_filtered_miss = service
            .calculate_intelligent_timeout(&["nope".to_string()], 240.0)
            .await;
        assert!(t_filtered_miss >= 240.0);

        let t_filtered_hit = service
            .calculate_intelligent_timeout(&["beta".to_string()], 240.0)
            .await;
        assert!(t_filtered_hit >= 600.0);
    }

    #[tokio::test]
    async fn create_tool_from_config_prepends_guidance_block() {
        let mut guidance_blocks = std::collections::HashMap::new();
        guidance_blocks.insert("my_tool".to_string(), "GUIDE".to_string());
        let guidance = GuidanceConfig { guidance_blocks };

        let service = make_service_with_monitor(
            Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
                TestTimeouts::scale_secs(30),
            ))),
            Arc::new(Some(guidance)),
        )
        .await;

        let tool_config = ToolConfig {
            name: "my_tool".to_string(),
            description: "DESC".to_string(),
            command: "echo".to_string(),
            subcommand: Some(vec![SubcommandConfig {
                extra: Default::default(),
                mutates: None,
                name: "default".to_string(),
                description: "d".to_string(),
                enabled: true,
                ..Default::default()
            }]),
            input_schema: None,
            timeout_seconds: None,
            synchronous: None,
            hints: ToolHints::default(),
            enabled: true,
            guidance_key: None,
            sequence: None,
            step_delay_ms: None,
            availability_check: None,
            install_instructions: None,
            monitor_level: None,
            monitor_stream: None,
            tool_type: None,
            livelog: None,
            ..Default::default()
        };

        let tools = service.create_tools_from_config(&tool_config);
        assert_eq!(tools.len(), 1);
        let desc = tools[0].description.clone().unwrap_or_default();
        assert!(desc.starts_with("GUIDE\n\nDESC"));
    }

    #[tokio::test]
    async fn schemas_for_await_and_status_have_expected_properties() {
        let service = make_service().await;
        let await_schema = service.generate_input_schema_for_wait();
        let status_schema = service.generate_input_schema_for_status();

        let await_props = await_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("await properties");
        assert!(await_props.contains_key("tools"));
        assert!(await_props.contains_key("id"));

        let status_props = status_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("status properties");
        assert!(status_props.contains_key("tools"));
        assert!(status_props.contains_key("id"));
    }

    #[test]
    fn test_force_synchronous_inheritance_subcommand_overrides_tool() {
        // When subcommand has force_synchronous set, it should override tool level
        let subcommand_sync = Some(true);
        let tool_sync = Some(false);

        // Subcommand wins
        let effective = subcommand_sync.or(tool_sync);
        assert_eq!(effective, Some(true));
    }

    #[test]
    fn test_force_synchronous_inheritance_subcommand_none_inherits_tool() {
        // When subcommand has no force_synchronous, it should inherit from tool
        let subcommand_sync: Option<bool> = None;
        let tool_sync = Some(true);

        // Tool wins when subcommand is None
        let effective = subcommand_sync.or(tool_sync);
        assert_eq!(effective, Some(true));
    }

    #[test]
    fn test_force_synchronous_inheritance_both_none() {
        // When both are None, effective is None (default behavior)
        let subcommand_sync: Option<bool> = None;
        let tool_sync: Option<bool> = None;

        let effective = subcommand_sync.or(tool_sync);
        assert_eq!(effective, None);
    }

    #[test]
    fn test_force_synchronous_subcommand_explicit_false_overrides_tool_true() {
        // Subcommand can explicitly set false to override tool's true
        let subcommand_sync = Some(false);
        let tool_sync = Some(true);

        let effective = subcommand_sync.or(tool_sync);
        assert_eq!(effective, Some(false));
    }

    // ============= resolve_flattened_tool tests =============

    #[test]
    fn test_resolve_flattened_tool_found() {
        let mut configs = HashMap::new();
        configs.insert(
            "file-tools".to_string(),
            ToolConfig {
                name: "file-tools".to_string(),
                description: "File tools".to_string(),
                command: "ls".to_string(),
                subcommand: Some(vec![SubcommandConfig {
                    name: "read".to_string(),
                    ..SubcommandConfig::default()
                }]),
                ..serde_json::from_value(json!({
                    "name": "file-tools",
                    "description": "File tools",
                    "command": "ls",
                }))
                .unwrap()
            },
        );
        let result = AhmaMcpService::resolve_flattened_tool("file-tools_read", &configs);
        assert!(result.is_some());
        let (config, sub_path) = result.unwrap();
        assert_eq!(config.name, "file-tools");
        assert_eq!(sub_path, "read");
    }

    #[test]
    fn test_resolve_flattened_tool_not_found() {
        let configs = HashMap::new();
        let result = AhmaMcpService::resolve_flattened_tool("nonexistent_tool", &configs);
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_flattened_tool_no_subcommands() {
        let mut configs = HashMap::new();
        configs.insert(
            "simple".to_string(),
            ToolConfig {
                name: "simple".to_string(),
                description: "Simple tool".to_string(),
                command: "echo".to_string(),
                subcommand: None,
                ..serde_json::from_value(json!({
                    "name": "simple",
                    "description": "Simple tool",
                    "command": "echo",
                }))
                .unwrap()
            },
        );
        // Won't resolve because config has no subcommands
        let result = AhmaMcpService::resolve_flattened_tool("simple_sub", &configs);
        assert!(result.is_none());
    }

    // ============= get_info tests =============

    #[tokio::test]
    async fn test_get_info_returns_server_info() {
        let service = make_service().await;
        let info = service.get_info();
        assert_eq!(info.server_info.name, env!("CARGO_PKG_NAME"));
    }

    use ahma_common::keepalive::{HeartbeatPayload, KeepAlive};
    use std::borrow::Cow;
    use tempfile::tempdir;

    // ==================== shared helpers (new) ====================

    async fn make_service_with_adapter(adapter: Arc<Adapter>) -> AhmaMcpService {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        AhmaMcpService::new(
            adapter,
            monitor,
            Arc::new(HashMap::new()),
            Arc::new(None),
            false,
            false,
        )
        .await
        .expect("service")
    }

    async fn make_service_force_sync() -> AhmaMcpService {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let adapter =
            crate::test_utils::client::create_test_config(Path::new(".")).expect("adapter");
        AhmaMcpService::new(
            adapter,
            monitor,
            Arc::new(HashMap::new()),
            Arc::new(None),
            true,
            false,
        )
        .await
        .expect("service")
    }

    fn cfg_from(v: serde_json::Value) -> ToolConfig {
        serde_json::from_value(v).expect("tool config")
    }

    fn sub_from(v: serde_json::Value) -> SubcommandConfig {
        serde_json::from_value(v).expect("subcommand config")
    }

    fn insert_config(service: &AhmaMcpService, config: ToolConfig) {
        service.configs.write().insert(config.name.clone(), config);
        // Direct `configs` writers must invalidate the tools/list cache,
        // exactly as `update_tools` does.
        service.invalidate_config_tools_cache();
    }

    fn obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    // ==================== pure static helpers ====================

    #[test]
    fn is_sync_meta_tool_for_protocol_cancel_matches_meta_tools() {
        for name in [
            "await",
            "status",
            "cancel",
            "logs_list",
            "logs_approve",
            "logs_read",
            "logs_search",
            "restart",
        ] {
            assert!(
                AhmaMcpService::is_sync_meta_tool_for_protocol_cancel(name),
                "{name} should be a sync/meta tool"
            );
        }
        assert!(!AhmaMcpService::is_sync_meta_tool_for_protocol_cancel(
            "run_terminal_command"
        ));
        assert!(!AhmaMcpService::is_sync_meta_tool_for_protocol_cancel(
            "cargo_build"
        ));
    }

    #[test]
    fn summarize_arguments_serializes_map() {
        let empty = serde_json::Map::new();
        assert_eq!(AhmaMcpService::summarize_arguments(&empty), "{}");

        let args = obj(json!({"a": 1}));
        assert_eq!(AhmaMcpService::summarize_arguments(&args), "{\"a\":1}");
    }

    #[test]
    fn sync_tool_progress_description_formats_command_and_dir() {
        let s = AhmaMcpService::sync_tool_progress_description("cargo", "/work");
        assert_eq!(s, "Execute cargo in /work");
    }

    #[test]
    fn flattened_subcommand_description_falls_back_to_tool_description() {
        let cfg = cfg_from(json!({
            "name": "t", "description": "TOOLDESC", "command": "c"
        }));
        let empty_sub = sub_from(json!({"name": "x", "description": ""}));
        assert_eq!(
            AhmaMcpService::flattened_subcommand_description(&cfg, &empty_sub),
            "TOOLDESC"
        );

        let described_sub = sub_from(json!({"name": "x", "description": "SUBDESC"}));
        assert_eq!(
            AhmaMcpService::flattened_subcommand_description(&cfg, &described_sub),
            "SUBDESC"
        );
    }

    #[test]
    fn leaf_subcommands_and_creates_single_tool() {
        // No subcommands -> empty leaves -> single tool.
        let single = cfg_from(json!({"name": "s", "description": "d", "command": "c"}));
        let leaves = AhmaMcpService::leaf_subcommands(&single);
        assert!(leaves.is_empty());
        assert!(AhmaMcpService::creates_single_tool(&leaves));

        // A lone "default" subcommand -> still a single tool.
        let default_only = cfg_from(json!({
            "name": "s", "description": "d", "command": "c",
            "subcommand": [{"name": "default", "description": "d"}]
        }));
        let leaves = AhmaMcpService::leaf_subcommands(&default_only);
        assert_eq!(leaves.len(), 1);
        assert!(AhmaMcpService::creates_single_tool(&leaves));

        // Multiple named subcommands -> NOT a single tool.
        let multi = cfg_from(json!({
            "name": "s", "description": "d", "command": "c",
            "subcommand": [
                {"name": "hello", "description": "h"},
                {"name": "world", "description": "w"}
            ]
        }));
        let leaves = AhmaMcpService::leaf_subcommands(&multi);
        assert_eq!(leaves.len(), 2);
        assert!(!AhmaMcpService::creates_single_tool(&leaves));
    }

    #[test]
    fn subcommand_not_found_error_reports_available_subcommands() {
        let cfg = cfg_from(json!({
            "name": "mytool", "description": "d", "command": "c",
            "subcommand": [{"name": "build", "description": "b"}]
        }));
        let err =
            AhmaMcpService::subcommand_not_found_error("mytool", &cfg, Some("nope".to_string()));
        let dbg = format!("{err:?}");
        assert!(dbg.contains("not found or invalid"), "got: {dbg}");
        assert!(dbg.contains("mytool"));
        assert!(dbg.contains("build (enabled=true)"));
    }

    // ==================== tool building / descriptions ====================

    #[tokio::test]
    async fn create_tools_from_config_flattens_multiple_subcommands() {
        let service = make_service().await;
        let cfg = cfg_from(json!({
            "name": "mytool", "description": "d", "command": "echo",
            "subcommand": [
                {"name": "hello", "description": "h"},
                {"name": "world", "description": "w"}
            ]
        }));
        let tools = service.create_tools_from_config(&cfg);
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"mytool_hello".to_string()));
        assert!(names.contains(&"mytool_world".to_string()));
    }

    #[tokio::test]
    async fn non_mutating_tool_names_covers_builtins_and_configured_tools() {
        let service = make_service().await;

        // A single-tool config with no subcommands: mutates: false exempts it,
        // mutates omitted (default true) does not.
        let read_only = cfg_from(json!({
            "name": "status_check", "description": "d", "command": "echo",
            "mutates": false
        }));
        service
            .configs
            .write()
            .insert(read_only.name.clone(), read_only);
        let undeclared = cfg_from(json!({
            "name": "npm_publish", "description": "d", "command": "npm"
        }));
        service
            .configs
            .write()
            .insert(undeclared.name.clone(), undeclared);

        // A flattened tool: the subcommand's own mutates overrides the
        // tool-level default, in both directions.
        let git = cfg_from(json!({
            "name": "git", "description": "d", "command": "git",
            "mutates": true,
            "subcommand": [
                {"name": "status", "description": "s", "mutates": false},
                {"name": "commit", "description": "c"}
            ]
        }));
        service.configs.write().insert(git.name.clone(), git);

        let names = service.non_mutating_tool_names();

        // Builtins: BuiltinTool::is_mutating() drives this directly.
        assert!(names.contains(BuiltinTool::ReadFile.name()));
        assert!(!names.contains(BuiltinTool::WriteFile.name()));
        assert!(!names.contains(BuiltinTool::RunTerminalCommand.name()));

        // Configured tools.
        assert!(names.contains("status_check"), "mutates: false must exempt");
        assert!(
            !names.contains("npm_publish"),
            "an undeclared custom tool must default to mutating (fail closed)"
        );
        assert!(
            names.contains("git_status"),
            "subcommand mutates: false must exempt the flattened name"
        );
        assert!(
            !names.contains("git_commit"),
            "a subcommand with no override inherits the tool-level mutates: true"
        );
    }

    #[tokio::test]
    async fn build_single_and_flattened_tool_names() {
        let service = make_service().await;
        let cfg = cfg_from(json!({
            "name": "mytool", "description": "d", "command": "echo",
            "subcommand": [{"name": "hello", "description": "h"}]
        }));
        let single = service.build_single_tool_from_config(&cfg);
        assert_eq!(&*single.name, "mytool");

        let sub = &cfg.subcommand.as_ref().unwrap()[0];
        let flat = service.build_flattened_tool_from_config(&cfg, "hello", sub);
        assert_eq!(&*flat.name, "mytool_hello");
    }

    #[tokio::test]
    async fn tool_description_prepends_guidance_by_key_override() {
        let mut guidance_blocks = std::collections::HashMap::new();
        guidance_blocks.insert("gk".to_string(), "GUIDE".to_string());
        let guidance = GuidanceConfig { guidance_blocks };
        let service = make_service_with_monitor(
            Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
                TestTimeouts::scale_secs(30),
            ))),
            Arc::new(Some(guidance)),
        )
        .await;

        let cfg = cfg_from(json!({
            "name": "mytool", "description": "DESC", "command": "echo",
            "guidance_key": "gk"
        }));
        // guidance_key overrides the supplied key, so even a bogus key resolves "gk".
        let d = service.tool_description(&cfg, "unused_key");
        assert_eq!(d, "GUIDE\n\nDESC");

        let dt = service.tool_description_text(&cfg, "unused_key", "BASE");
        assert_eq!(dt, "GUIDE\n\nBASE");
    }

    #[tokio::test]
    async fn tool_description_without_guidance_returns_base() {
        let service = make_service().await;
        let cfg = cfg_from(json!({"name": "t", "description": "DESC", "command": "c"}));
        assert_eq!(service.tool_description(&cfg, "t"), "DESC");
        assert_eq!(service.tool_description_text(&cfg, "t", "BASE"), "BASE");
    }

    // ==================== config visibility / resolution ====================

    #[tokio::test]
    async fn is_config_visible_to_client_filters_hardcoded_and_disabled() {
        let service = make_service().await;

        let hardcoded = cfg_from(json!({"name": "await", "description": "d", "command": "c"}));
        assert!(!service.is_config_visible_to_client(&hardcoded));

        let disabled = cfg_from(json!({
            "name": "dt", "description": "d", "command": "c", "enabled": false
        }));
        assert!(!service.is_config_visible_to_client(&disabled));

        let normal = cfg_from(json!({"name": "ok", "description": "d", "command": "c"}));
        assert!(service.is_config_visible_to_client(&normal));
    }

    /// Every built-in is advertised, exactly once, titled with its own name.
    ///
    /// The name half of this test used to compare `builtin_tools()` against a
    /// separately hand-maintained `HARDCODED_TOOLS` list, because the two had
    /// drifted twice. Both are now driven by `BuiltinTool::ALL` and the
    /// construction match is exhaustive, so the compiler enforces that half and
    /// only the observable shape is worth asserting here.
    #[tokio::test]
    async fn every_builtin_is_advertised_once_and_titled_with_its_name() {
        let service = make_service().await;
        let advertised: Vec<Tool> = service.builtin_tools();

        assert_eq!(
            advertised.len(),
            BuiltinTool::ALL.len(),
            "one advertised tool per variant"
        );
        for (tool, builtin) in advertised.iter().zip(BuiltinTool::ALL) {
            assert_eq!(
                &*tool.name,
                builtin.name(),
                "advertised tools follow BuiltinTool::ALL order"
            );
            assert_eq!(
                tool.title.as_deref(),
                Some(builtin.name()),
                "builtin `{}` must be titled with its own name",
                builtin.name()
            );
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "builtin `{}` must carry a description",
                builtin.name()
            );
        }
    }

    #[tokio::test]
    async fn find_tool_config_direct_flattened_and_missing() {
        let service = make_service().await;
        insert_config(
            &service,
            cfg_from(json!({"name": "mytool", "description": "d", "command": "echo"})),
        );
        insert_config(
            &service,
            cfg_from(json!({
                "name": "file-tools", "description": "d", "command": "ls",
                "subcommand": [{"name": "hello", "description": "h"}]
            })),
        );

        let (cfg, sub) = service.find_tool_config("mytool").expect("direct");
        assert_eq!(cfg.name, "mytool");
        assert!(sub.is_none());

        let (cfg, sub) = service.find_tool_config("file-tools_hello").expect("flat");
        assert_eq!(cfg.name, "file-tools");
        assert_eq!(sub.as_deref(), Some("hello"));

        assert!(service.find_tool_config("does_not_exist").is_none());
    }

    #[tokio::test]
    async fn resolve_configured_tool_not_found_and_disabled() {
        let service = make_service().await;

        let err = service.resolve_configured_tool("ghost").unwrap_err();
        assert!(format!("{err:?}").contains("not found"));

        insert_config(
            &service,
            cfg_from(json!({
                "name": "dt", "description": "d", "command": "c", "enabled": false
            })),
        );
        let err = service.resolve_configured_tool("dt").unwrap_err();
        assert!(format!("{err:?}").contains("availability probe failed"));

        insert_config(
            &service,
            cfg_from(json!({"name": "ok", "description": "d", "command": "c"})),
        );
        let (cfg, _) = service.resolve_configured_tool("ok").expect("ok");
        assert_eq!(cfg.name, "ok");
    }

    // ==================== execution mode ====================

    #[tokio::test]
    async fn determine_execution_mode_variants() {
        use crate::adapter::ExecutionMode;
        let service = make_service().await;
        let cfg = cfg_from(json!({"name": "t", "description": "d", "command": "c"}));
        let sub = sub_from(json!({"name": "default", "description": "d"}));

        // Default -> async.
        assert_eq!(
            service.determine_execution_mode(&sub, &cfg, &serde_json::Map::new()),
            ExecutionMode::AsyncResultPush
        );
        // Dynamic blocking arg -> sync.
        assert_eq!(
            service.determine_execution_mode(&sub, &cfg, &obj(json!({"blocking": true}))),
            ExecutionMode::Synchronous
        );
        // Explicit execution_mode string -> sync.
        assert_eq!(
            service.determine_execution_mode(
                &sub,
                &cfg,
                &obj(json!({"execution_mode": "Synchronous"}))
            ),
            ExecutionMode::Synchronous
        );

        // Subcommand-level synchronous override true / false.
        let sync_sub =
            sub_from(json!({"name": "default", "description": "d", "synchronous": true}));
        assert_eq!(
            service.determine_execution_mode(&sync_sub, &cfg, &serde_json::Map::new()),
            ExecutionMode::Synchronous
        );
        let async_sub =
            sub_from(json!({"name": "default", "description": "d", "synchronous": false}));
        assert_eq!(
            service.determine_execution_mode(&async_sub, &cfg, &serde_json::Map::new()),
            ExecutionMode::AsyncResultPush
        );
    }

    #[tokio::test]
    async fn determine_execution_mode_force_synchronous_service() {
        use crate::adapter::ExecutionMode;
        let service = make_service_force_sync().await;
        let cfg = cfg_from(json!({"name": "t", "description": "d", "command": "c"}));
        let sub = sub_from(json!({"name": "default", "description": "d"}));
        assert_eq!(
            service.determine_execution_mode(&sub, &cfg, &serde_json::Map::new()),
            ExecutionMode::Synchronous
        );
    }

    // ==================== subcommand resolution ====================

    #[tokio::test]
    async fn resolve_subcommand_default_explicit_and_missing() {
        let service = make_service().await;
        let cfg = cfg_from(json!({
            "name": "mytool", "description": "d", "command": "echo",
            "subcommand": [
                {"name": "default", "description": "dd"},
                {"name": "build", "description": "b"}
            ]
        }));

        // No subcommand argument -> "default".
        let mut args = serde_json::Map::new();
        let (sub, _parts) = service
            .resolve_subcommand(&cfg, "mytool", &mut args, None)
            .expect("default");
        assert_eq!(sub.name, "default");

        // "subcommand" arg is consumed and resolved.
        let mut args = obj(json!({"subcommand": "build"}));
        let (sub, _parts) = service
            .resolve_subcommand(&cfg, "mytool", &mut args, None)
            .expect("build");
        assert_eq!(sub.name, "build");
        assert!(!args.contains_key("subcommand"), "subcommand key consumed");

        // Unknown flattened subcommand -> error.
        let mut args = serde_json::Map::new();
        let err = service
            .resolve_subcommand(&cfg, "mytool", &mut args, Some("nope".to_string()))
            .unwrap_err();
        assert!(format!("{err:?}").contains("not found or invalid"));
    }

    // ==================== working directory / sandbox guards ====================

    /// The MTDF surface resolves its working directory through the same shared
    /// path as `run_terminal_command` (SPEC R5.2.8), so an omitted directory is
    /// disclosed rather than silently substituted.
    #[tokio::test]
    async fn resolve_working_directory_explicit_and_scope() {
        let service = make_service().await;
        let sandbox = service.adapter.sandbox();

        let explicit = obj(json!({"working_directory": "/explicit/path"}));
        let wd = handlers::working_directory::resolve(sandbox, "cargo", &explicit).unwrap();
        assert_eq!(wd.path, "/explicit/path");

        // No arg + a client-reported scope -> substitute the first scope, and say so.
        sandbox.set_roots_received(true);
        let wd = handlers::working_directory::resolve(sandbox, "cargo", &serde_json::Map::new())
            .unwrap();
        let scope = sandbox
            .scopes()
            .first()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap();
        assert_eq!(wd.path, scope);
        let disclosed = wd.disclose(handlers::common::text_result("out"));
        assert!(first_text(&disclosed).contains("no `working_directory` was given"));
    }

    #[tokio::test]
    async fn guard_and_skip_roots_defaults() {
        let service = make_service().await;
        // A rooted but *uncommitted* scope is not enough: the gate keys on the
        // commit latch (R5.1.2.1), because the one provisional source that
        // matters — a container root — is wider than the scope it commits to.
        let err = service
            .guard_sandbox_ready_for_tool_calls()
            .await
            .expect_err("uncommitted scope must be refused");
        assert_eq!(err.code, rmcp::model::ErrorCode(-32001));
        // Once committed, the same scope passes the gate.
        let _ = service.adapter.sandbox().commit_existing_scopes();
        assert!(service.guard_sandbox_ready_for_tool_calls().await.is_ok());
        // Not explicit, not test mode -> we still ask the client for roots.
        assert!(!service.should_skip_client_roots_sandbox_setup());
    }

    // ==================== extension key / registration ====================

    #[tokio::test]
    async fn get_extension_key_none_without_registered_handler() {
        let service = make_service().await;

        // A plain command tool is never an extension.
        let plain = cfg_from(json!({"name": "t", "description": "d", "command": "c"}));
        assert_eq!(service.get_extension_key(&plain), None);

        // An extension tool whose key has no registered handler resolves to None
        // (there are no hardcoded built-in extension keys).
        let ext = cfg_from(json!({
            "name": "t", "description": "d", "command": "c",
            "tool_type": "ext", "some_ext": {}
        }));
        assert_eq!(service.get_extension_key(&ext), None);
    }

    struct DummyExtHandler;
    #[async_trait::async_trait]
    impl ExtensionToolHandler for DummyExtHandler {
        async fn call(
            &self,
            _params: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
            _config: ToolConfig,
            _adapter: Arc<crate::adapter::Adapter>,
            _operation_monitor: Arc<crate::operation_monitor::OperationMonitor>,
        ) -> Result<CallToolResult, McpError> {
            Ok(handlers::common::text_result("dummy"))
        }
    }

    #[tokio::test]
    async fn register_extension_handler_enables_custom_key_lookup() {
        let service = make_service().await;
        service.register_extension_handler("myext".to_string(), Arc::new(DummyExtHandler));

        // Extension config whose extra map carries the registered key.
        let cfg = cfg_from(json!({
            "name": "x", "description": "d", "command": "c",
            "tool_type": "customext", "myext": {"foo": 1}
        }));
        assert_eq!(service.get_extension_key(&cfg).as_deref(), Some("myext"));
    }

    // ==================== llm provider resolution ====================

    #[tokio::test]
    async fn parse_llm_provider_uses_explicit_args_when_complete() {
        let service = make_service().await;
        let args = obj(json!({
            "llm_base_url": "http://x/v1", "llm_model": "m", "llm_api_key": "secret"
        }));
        let p = service.parse_llm_provider(&args);
        assert_eq!(p.base_url, "http://x/v1");
        assert_eq!(p.model, "m");
        assert_eq!(p.api_key.as_deref(), Some("secret"));
    }

    #[tokio::test]
    async fn parse_llm_provider_falls_back_when_incomplete() {
        let service = make_service().await;
        // base_url present but model missing -> fallback (no livelog configs -> default).
        let args = obj(json!({"llm_base_url": "http://x/v1"}));
        let p = service.parse_llm_provider(&args);
        assert_eq!(p.base_url, "http://localhost:11434/v1");
        assert_eq!(p.model, "llama3.2");
        assert!(p.api_key.is_none());
    }

    #[tokio::test]
    async fn fallback_llm_provider_prefers_livelog_config() {
        let service = make_service().await;
        insert_config(
            &service,
            cfg_from(json!({
                "name": "l", "description": "d", "command": "x",
                "tool_type": "livelog",
                "livelog": {
                    "source_command": "tail",
                    "detection_prompt": "p",
                    "llm_provider": {"base_url": "http://found/v1", "model": "foundmodel"}
                }
            })),
        );
        let p = service.fallback_llm_provider();
        assert_eq!(p.base_url, "http://found/v1");
        assert_eq!(p.model, "foundmodel");
    }

    // ==================== tool listing ====================

    #[tokio::test]
    async fn list_tool_names_includes_visible_excludes_hidden() {
        let service = make_service().await;
        insert_config(
            &service,
            cfg_from(json!({"name": "mytool", "description": "d", "command": "c"})),
        );
        insert_config(
            &service,
            cfg_from(json!({
                "name": "dt", "description": "d", "command": "c", "enabled": false
            })),
        );
        insert_config(
            &service,
            cfg_from(json!({"name": "await", "description": "d", "command": "c"})),
        );

        let names = service.list_tool_names();
        assert!(names.contains(&"status".to_string()));
        assert!(names.contains(&"run_terminal_command".to_string()));
        assert!(names.contains(&"mytool".to_string()));
        assert!(!names.contains(&"dt".to_string()));
        // "await" appears only as the hard-wired builtin, not via the config.
        assert_eq!(names.iter().filter(|n| *n == "await").count(), 1);
    }

    #[tokio::test]
    async fn get_all_available_tools_lists_builtins_and_configs() {
        let service = make_service().await;
        insert_config(
            &service,
            cfg_from(json!({"name": "mytool", "description": "d", "command": "c"})),
        );

        let tools = service.get_all_available_tools().await;
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        for expected in [
            "await",
            "status",
            "run_terminal_command",
            "logs_list",
            "read_file",
            "log_monitor",
            "mytool",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
    }

    /// The agent loop's toolset must be the client toolset minus exactly the
    /// denied built-ins.
    ///
    /// Regression: `get_all_available_tools` and `list_tools` used to carry two
    /// hand-maintained copies of the built-in list. They drifted — the agent-facing
    /// copy was missing `cancel` and `sandbox_grant`, so ahma's own agent could
    /// start async operations it had no way to cancel, and could not act on a
    /// `sandbox_denial`. Both now derive from `builtin_tools`, and this asserts the
    /// derivation instead of re-listing the names.
    #[tokio::test]
    async fn agent_toolset_is_client_toolset_minus_denied_builtins() {
        let service = make_service().await;

        let client_builtins: Vec<String> = service
            .builtin_tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        let agent_tools: Vec<String> = service
            .get_all_available_tools()
            .await
            .into_iter()
            .map(|t| t.name)
            .collect();

        for name in &client_builtins {
            let denied =
                BuiltinTool::from_name(name).is_some_and(BuiltinTool::is_denied_in_agent_loop);
            assert_eq!(
                agent_tools.contains(name),
                !denied,
                "builtin {name}: denied={denied}, but present in agent toolset={}",
                agent_tools.contains(name)
            );
        }

        // The two tools the drift had silently dropped.
        for expected in ["cancel", "sandbox_grant"] {
            assert!(
                agent_tools.iter().any(|n| n == expected),
                "agent toolset is missing {expected}"
            );
        }
        // ...and the one that is withheld on purpose, to stop the agent loop
        // recursing into itself.
        assert!(
            !agent_tools.iter().any(|n| n == "agent"),
            "the agent loop must not be handed the `agent` tool"
        );
        assert!(
            client_builtins.iter().any(|n| n == "agent"),
            "MCP clients should still see the `agent` tool"
        );
    }

    // ==================== vault audit append helpers ====================

    #[tokio::test]
    async fn append_audit_line_creates_parents_and_appends() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("audit.jsonl");

        AhmaMcpService::append_audit_line(&path, json!({"k": "v"}))
            .await
            .unwrap();
        AhmaMcpService::append_audit_line(&path, json!({"k": "v2"}))
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"k\":\"v\""));
        assert!(lines[1].contains("\"k\":\"v2\""));
    }

    #[tokio::test]
    async fn append_tool_call_and_complete_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

        AhmaMcpService::append_tool_call_event(&path, "op1", "mytool", "argsum")
            .await
            .unwrap();
        AhmaMcpService::append_tool_complete_event(&path, "op1", true, 42)
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("\"type\":\"tool_call\""));
        assert!(content.contains("\"operation_id\":\"op1\""));
        assert!(content.contains("\"tool_name\":\"mytool\""));
        assert!(content.contains("\"type\":\"tool_complete\""));
        assert!(content.contains("\"success\":true"));
        assert!(content.contains("\"duration_ms\":42"));
    }

    // ==================== vault emit (task_vault wired via AppConfig) ====================

    fn app_config_with_vault(vault: std::path::PathBuf) -> crate::shell::cli::AppConfig {
        crate::shell::cli::AppConfig {
            task_vault: Some(vault),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn task_vault_paths_none_by_default() {
        let service = make_service().await;
        assert!(service.task_vault_root().is_none());
        assert!(service.task_vault_audit_log_path().is_none());
        assert!(service.task_vault_trash_dir().is_none());
    }

    #[tokio::test]
    async fn task_vault_paths_from_app_config() {
        let dir = tempdir().unwrap();
        let vault = dir.path().join("vault");
        let service = make_service().await;
        service.set_app_config(Arc::new(app_config_with_vault(vault.clone())));

        assert_eq!(service.task_vault_root(), Some(vault.clone()));
        assert_eq!(
            service.task_vault_audit_log_path(),
            Some(vault.join("audit.jsonl"))
        );
        assert_eq!(service.task_vault_trash_dir(), Some(vault.join("trash")));
    }

    #[tokio::test]
    async fn task_vault_root_derived_from_workdir_scope() {
        let dir = tempdir().unwrap();
        let workdir = dir.path().join("workdir");
        std::fs::create_dir_all(&workdir).unwrap();
        let adapter = crate::test_utils::client::create_test_config(&workdir).expect("adapter");
        let service = make_service_with_adapter(adapter).await;

        let root = service.task_vault_root().expect("root from workdir");
        let scope = service.adapter.sandbox().scopes().first().unwrap().clone();
        assert_eq!(Some(root.as_path()), scope.parent());
    }

    #[tokio::test]
    async fn emit_vault_tool_call_is_noop_without_vault() {
        let service = make_service().await;
        service.emit_vault_tool_call("op1", "tool", "args").await;
        assert!(service.vault_audited_ops.lock().is_empty());
    }

    #[tokio::test]
    async fn emit_vault_tool_call_then_complete_writes_audit_and_tracks_ops() {
        let dir = tempdir().unwrap();
        let vault = dir.path().join("vault");
        let service = make_service().await;
        service.set_app_config(Arc::new(app_config_with_vault(vault.clone())));

        service.emit_vault_tool_call("op1", "tool", "argsum").await;
        assert!(service.vault_audited_ops.lock().contains("op1"));

        service.emit_vault_tool_complete("op1", true, 10).await;
        assert!(!service.vault_audited_ops.lock().contains("op1"));

        let content = tokio::fs::read_to_string(vault.join("audit.jsonl"))
            .await
            .unwrap();
        assert!(content.contains("\"type\":\"tool_call\""));
        assert!(content.contains("\"type\":\"tool_complete\""));
    }

    #[tokio::test]
    async fn emit_vault_file_staged_writes_event() {
        let dir = tempdir().unwrap();
        let vault = dir.path().join("vault");
        let service = make_service().await;
        service.set_app_config(Arc::new(app_config_with_vault(vault.clone())));

        service
            .emit_vault_file_staged("/orig/path", "/trash/path")
            .await;

        let content = tokio::fs::read_to_string(vault.join("audit.jsonl"))
            .await
            .unwrap();
        assert!(content.contains("\"type\":\"file_staged\""));
        assert!(content.contains("/orig/path"));
        assert!(content.contains("/trash/path"));
    }

    // ==================== rm staging interception ====================

    #[tokio::test]
    async fn maybe_stage_configured_delete_skips_non_rm() {
        let service = make_service().await;
        let args = obj(json!({"path": "x"}));
        let r = service
            .maybe_stage_configured_delete("ls", ".", &args)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn maybe_stage_configured_delete_noop_without_vault() {
        let service = make_service().await;
        let args = obj(json!({"path": "x"}));
        let r = service
            .maybe_stage_configured_delete("rm", ".", &args)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn maybe_stage_configured_delete_empty_targets_returns_none() {
        let dir = tempdir().unwrap();
        let vault = dir.path().join("vault");
        let service = make_service().await;
        service.set_app_config(Arc::new(app_config_with_vault(vault)));

        // Only a meta argument -> no deletion targets.
        let args = obj(json!({"timeout_seconds": 5}));
        let r = service
            .maybe_stage_configured_delete("rm", ".", &args)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn maybe_stage_configured_delete_moves_file_into_trash() {
        let dir = tempdir().unwrap();
        let vault = dir.path().join("vault");
        let workdir = dir.path().join("work");
        std::fs::create_dir_all(&workdir).unwrap();
        let victim = workdir.join("victim.txt");
        std::fs::write(&victim, b"data").unwrap();

        let service = make_service().await;
        service.set_app_config(Arc::new(app_config_with_vault(vault.clone())));

        let args = obj(json!({"victim": "victim.txt"}));
        let result = service
            .maybe_stage_configured_delete("rm", workdir.to_str().unwrap(), &args)
            .await
            .unwrap()
            .expect("staged result");
        assert!(first_text(&result).contains("Staged 1 path"));
        assert!(
            !victim.exists(),
            "victim should be moved out of working dir"
        );
        assert!(vault.join("trash").exists());
    }

    // ==================== set_app_config side effects ====================

    #[tokio::test]
    async fn set_app_config_propagates_flags_and_tools_dir() {
        let dir = tempdir().unwrap();
        let tools_dir = dir.path().join("tools");
        let service = make_service().await;

        let cfg = crate::shell::cli::AppConfig {
            tools_dir: Some(tools_dir.clone()),
            minimize_tokens: true,
            small_model_harness: true,
            ..Default::default()
        };
        service.set_app_config(Arc::new(cfg));

        assert_eq!(*service.current_tools_dir.read(), Some(tools_dir));
        assert!(service.harness_guard.lock().await.enabled);
    }

    /// Self-correction (tool-name/argument healing + failure-loop detection) is
    /// on by default and stays on even when `small_model_harness` is off — it is
    /// what stops a model from burning its turn budget re-issuing a broken call.
    #[tokio::test]
    async fn self_correction_enabled_by_default_independent_of_small_model_harness() {
        let service = make_service().await;
        // Enabled before any AppConfig is applied.
        assert!(service.harness_guard.lock().await.enabled);

        let cfg = crate::shell::cli::AppConfig {
            small_model_harness: false,
            ..Default::default()
        };
        service.set_app_config(Arc::new(cfg));
        assert!(
            service.harness_guard.lock().await.enabled,
            "self-correction must stay on with small_model_harness disabled"
        );
    }

    // ==================== cancel most-recent background op ====================

    #[tokio::test]
    async fn cancel_most_recent_background_op_empty_is_noop() {
        let service = make_service().await;
        // No panic, nothing to cancel.
        service
            .cancel_most_recent_background_op(&[], "req", "reason")
            .await;
    }

    #[tokio::test]
    async fn cancel_most_recent_background_op_cancels_latest() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            TestTimeouts::scale_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let mut op = Operation::new(
            "bg1".to_string(),
            "cargo_build".to_string(),
            "d".to_string(),
            None,
        );
        op.state = OperationStatus::InProgress;
        monitor.add_operation(op).await;

        let active = monitor.get_all_active_operations().await;
        let refs: Vec<&Operation> = active.iter().collect();
        service
            .cancel_most_recent_background_op(&refs, "req1", "user-stop")
            .await;

        // Cancellation removes the op from the active map and moves it to history.
        assert!(
            monitor.get_operation("bg1").await.is_none(),
            "cancelled op is no longer active"
        );
        let completed = monitor.get_completed_operations().await;
        let op = completed
            .iter()
            .find(|o| o.id == "bg1")
            .expect("cancelled op moved to completion history");
        assert_eq!(op.state, OperationStatus::Cancelled);
    }

    // ==================== agent sub-agent tool ====================

    #[tokio::test]
    async fn handle_agent_requires_a_prompt() {
        let service = make_service().await;
        // Missing prompt → invalid-params protocol error, before any runner call.
        assert!(service.handle_agent(serde_json::Map::new()).await.is_err());
        // Blank prompt is also rejected.
        let mut blank = serde_json::Map::new();
        blank.insert("prompt".into(), json!("   "));
        assert!(service.handle_agent(blank).await.is_err());
    }

    #[tokio::test]
    async fn handle_agent_delegates_to_the_registered_runner() {
        use ahma_common::daemon_hub::{ClientMsg, DaemonChatMessage};

        struct MockRunner;
        #[async_trait::async_trait]
        impl PromptRunner for MockRunner {
            async fn run_prompt(
                &self,
                _messages: Vec<DaemonChatMessage>,
                _system_prompt: Option<String>,
                _provider: Option<String>,
                _model: Option<String>,
                _hub_tx: tokio::sync::mpsc::Sender<ClientMsg>,
                _session: Arc<tokio::sync::Mutex<ActiveAgentSession>>,
            ) -> Result<(), String> {
                Ok(())
            }
            async fn run_prompt_to_completion(
                &self,
                messages: Vec<DaemonChatMessage>,
                _system_prompt: Option<String>,
                _provider: Option<String>,
                _model: Option<String>,
                max_turns: Option<u32>,
            ) -> Result<String, String> {
                Ok(format!(
                    "handled '{}' (max_turns={:?})",
                    messages[0].content, max_turns
                ))
            }
        }

        // nextest runs each test in its own process, so this global set is local.
        register_global_prompt_runner(Arc::new(MockRunner));

        let service = make_service().await;
        let mut args = serde_json::Map::new();
        args.insert("prompt".into(), json!("summarise the build"));
        args.insert("max_turns".into(), json!(3));

        let result = service.handle_agent(args).await.expect("tool call ok");
        assert_ne!(result.is_error, Some(true), "delegation should succeed");
        let text = first_text(&result);
        assert!(
            text.contains("handled 'summarise the build'") && text.contains("max_turns=Some(3)"),
            "unexpected sub-agent result: {text}"
        );
    }

    // ==================== todo_write plan tool ====================

    #[tokio::test]
    async fn handle_todo_write_stores_and_renders_plan() {
        let service = make_service().await;
        let args: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "todos": [
                {"content": "read SPEC", "status": "completed"},
                {"content": "add field", "status": "in_progress"},
                {"content": "wire + test"}
            ]
        }))
        .unwrap();

        let result = service.handle_todo_write(args).await.expect("tool ok");
        let text = first_text(&result);
        assert!(text.contains("Plan (1/3 done):"), "got: {text}");
        assert!(text.contains("[x] read SPEC"));
        assert!(text.contains("[~] add field"));
        assert!(text.contains("[ ] wire + test"));

        // The plan is persisted on the service for the TUI/next turn.
        assert_eq!(service.todo_list.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn handle_todo_write_rejects_bad_input() {
        let service = make_service().await;
        // Missing `todos`.
        assert!(
            service
                .handle_todo_write(serde_json::Map::new())
                .await
                .is_err()
        );
        // Blank content.
        let blank: serde_json::Map<String, Value> =
            serde_json::from_value(json!({"todos": [{"content": "  "}]})).unwrap();
        assert!(service.handle_todo_write(blank).await.is_err());
        // Unknown status.
        let bad_status: serde_json::Map<String, Value> =
            serde_json::from_value(json!({"todos": [{"content": "x", "status": "nope"}]})).unwrap();
        assert!(service.handle_todo_write(bad_status).await.is_err());
    }

    // ==================== harness guard preprocessing ====================

    #[tokio::test]
    async fn harness_guard_preprocess_passthrough_returns_none() {
        let service = make_service().await;
        let mut name: Cow<'static, str> = Cow::Owned("status".to_string());
        let mut args: Option<serde_json::Map<String, Value>> = None;
        assert!(
            service
                .harness_guard_preprocess(&mut name, &mut args)
                .is_none()
        );
        assert_eq!(&*name, "status");
        // Regression: a None payload must stay None so handlers can still detect
        // "missing arguments" (the pipeline must not materialise an empty {}).
        assert!(args.is_none(), "None args must be preserved");
    }

    #[tokio::test]
    async fn harness_guard_preprocess_heals_name_and_args() {
        let service = make_service().await;
        insert_config(
            &service,
            cfg_from(json!({"name": "mytool", "description": "d", "command": "c"})),
        );

        // Typo within edit distance 2 of a hard-coded tool.
        let mut name: Cow<'static, str> = Cow::Owned("run_terminal_commnd".to_string());
        let mut args = Some(obj(json!({"args": "echo hi"})));
        let early = service.harness_guard_preprocess(&mut name, &mut args);
        assert!(early.is_none());
        assert_eq!(&*name, "run_terminal_command");
        // String "args" healed into a singleton array.
        assert_eq!(args.unwrap().get("args").unwrap(), &json!(["echo hi"]));
    }

    #[tokio::test]
    async fn harness_guard_preprocess_detects_loop() {
        let service = make_service().await;
        {
            let g = service.harness_guard.lock().await;
            let empty = serde_json::Map::new();
            for _ in 0..3 {
                g.observe("status", &empty, true);
            }
        }
        let mut name: Cow<'static, str> = Cow::Owned("status".to_string());
        let mut args: Option<serde_json::Map<String, Value>> = None;
        let early = service
            .harness_guard_preprocess(&mut name, &mut args)
            .expect("loop detected");
        assert!(first_text(&early).contains("LOOP_DETECTED"));
        assert_eq!(early.is_error, Some(true));
    }

    #[tokio::test]
    async fn record_result_in_loop_detector_tracks_failures() {
        let service = make_service().await;
        let args = Some(obj(json!({"x": 1})));

        let err_result: Result<CallToolResult, McpError> = Ok(CallToolResult::error(vec![
            rmcp::model::ContentBlock::text("boom"),
        ]));
        for _ in 0..3 {
            service.record_result_in_loop_detector("mytool", args.as_ref(), &err_result);
        }
        // After three identical failures the same call is blocked by the pipeline.
        let mut name: Cow<'static, str> = Cow::Owned("mytool".to_string());
        let mut a = args.clone();
        assert!(
            service
                .harness_guard_preprocess(&mut name, &mut a)
                .is_some(),
            "loop should be detected after 3 failures"
        );

        // A success clears the detector, so the call proceeds again.
        let ok_result: Result<CallToolResult, McpError> = Ok(handlers::common::text_result("done"));
        service.record_result_in_loop_detector("mytool", args.as_ref(), &ok_result);
        let mut name2: Cow<'static, str> = Cow::Owned("mytool".to_string());
        let mut a2 = args.clone();
        assert!(
            service
                .harness_guard_preprocess(&mut name2, &mut a2)
                .is_none(),
            "success should clear the loop"
        );
    }

    // ==================== KeepAlive trait impl ====================

    #[tokio::test]
    async fn keepalive_basic_accessors() {
        let service = make_service().await;
        assert!(!service.is_ahma_peer());
        // heartbeat_timeout() scales the 60s base by the platform/coverage
        // multiplier (×4 on Windows, ×2 under coverage), so compare against the
        // same scaled value — never the raw 60s, which only holds at ×1.
        assert_eq!(
            service.heartbeat_timeout(),
            ahma_common::timeouts::TestTimeouts::scale(Duration::from_secs(60)),
            "heartbeat timeout should be the platform-scaled 60s base"
        );
        // last_received_signal was set at construction -> small elapsed time.
        assert!(service.time_since_last_received() < Duration::from_secs(60));
    }

    #[tokio::test]
    async fn keepalive_send_without_peer_is_ok() {
        let service = make_service().await;
        // No peer captured yet -> both sends are graceful no-ops.
        assert!(service.send_standard_ping().await.is_ok());
        let payload = HeartbeatPayload {
            version: "0.0.0".to_string(),
            hash: "abc".to_string(),
            timestamp: 1,
            ..Default::default()
        };
        assert!(service.send_enhanced_heartbeat(payload).await.is_ok());
    }

    #[tokio::test]
    async fn effective_supports_progress_defaults_to_the_client_heuristic() {
        // SPEC R2.2.1: with no override set, the resolved answer must match
        // McpClientType::supports_progress() exactly — Cursor suppressed,
        // everyone else enabled.
        let service = make_service().await;
        assert!(!service.effective_supports_progress(crate::client_type::McpClientType::Cursor));
        assert!(service.effective_supports_progress(crate::client_type::McpClientType::VSCode));
    }

    #[tokio::test]
    async fn force_progress_notifications_overrides_the_cursor_suppression() {
        // SPEC R2.2.1: an operator who knows a given Cursor version fixed the
        // client-side logging issue can turn progress back on for it.
        let service = make_service().await;
        service.set_app_config(Arc::new(crate::shell::cli::AppConfig {
            force_progress_notifications: true,
            ..Default::default()
        }));
        assert!(service.effective_supports_progress(crate::client_type::McpClientType::Cursor));
        // The override is uniform — it does not stop applying to clients that
        // were already enabled.
        assert!(service.effective_supports_progress(crate::client_type::McpClientType::VSCode));
    }

    #[tokio::test]
    async fn push_channel_open_defaults_false_and_reflects_the_stored_flag() {
        // The safe default: a subprocess that never hears from the bridge (direct
        // stdio, or a session that never opens SSE) must assume there is no live
        // push channel — see the field doc on `AhmaMcpService::push_channel_open`.
        let service = make_service().await;
        assert!(!service.push_channel_open());

        service
            .push_channel_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(service.push_channel_open());

        service
            .push_channel_open
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!service.push_channel_open());
    }

    #[tokio::test]
    async fn on_custom_notification_push_channel_changed_updates_the_flag() {
        // Exercises the actual dispatch path a bridge notification arrives
        // through (minus the NotificationContext, which cannot be constructed
        // outside rmcp) — proves the method-name match and the
        // `params.connected` extraction are wired correctly end to end.
        let service = make_service().await;
        assert!(!service.push_channel_open());

        service.apply_custom_notification(
            "notifications/ahma/pushChannelChanged",
            Some(&json!({"connected": true})),
        );
        assert!(service.push_channel_open());

        service.apply_custom_notification(
            "notifications/ahma/pushChannelChanged",
            Some(&json!({"connected": false})),
        );
        assert!(!service.push_channel_open());

        // Missing/malformed params must not panic, and must not flip the flag.
        service
            .push_channel_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        service.apply_custom_notification("notifications/ahma/pushChannelChanged", None);
        assert!(
            !service.push_channel_open(),
            "missing params must fall back to the safe default (false), same as unset"
        );

        // A malformed `connected` (wrong type) must also land on the safe
        // default — lenient parsing, pinned so the typed path can never
        // become stricter than the old `.get("connected").as_bool()` chain.
        service
            .push_channel_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        service.apply_custom_notification(
            "notifications/ahma/pushChannelChanged",
            Some(&json!({"connected": "yes"})),
        );
        assert!(
            !service.push_channel_open(),
            "malformed connected must fall back to the safe default (false)"
        );
    }

    #[tokio::test]
    async fn heartbeat_reports_pending_grants_from_the_coordinator() {
        // R8.8.4: the heartbeat's pending_grants mirrors the coordinator's
        // in-flight decision count, and stays 0 when no coordinator is wired.
        let service = make_service().await;
        let mut payload = HeartbeatPayload::default();
        service.enrich_heartbeat(&mut payload);
        assert_eq!(payload.pending_grants, 0, "no coordinator wired → 0");

        let coordinator = Arc::new(ahma_common::scope_grant::GrantCoordinator::new());
        *service.grant_coordinator.write() = Some(coordinator.clone());
        let tmp = tempfile::tempdir().unwrap();
        coordinator
            .begin(
                tmp.path(),
                ahma_common::config::ScopeAccess::Rw,
                ahma_common::scope_grant::GrantReason::PreExecViolation,
                None,
            )
            .expect("fresh path begins a decision");

        service.enrich_heartbeat(&mut payload);
        assert_eq!(payload.pending_grants, 1);
    }
}
