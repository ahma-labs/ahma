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
//!    and bundled capability flags (like `--rust` or `--git`) into a rich set of
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
mod config_watcher;
pub mod handlers;
pub mod progress_push;
pub mod schema;
mod sequence;
mod subcommand;
mod types;
mod utils;

pub use types::{
    ActiveAgentSession, ExtensionToolHandler, GuidanceConfig, LegacyGuidanceConfig, META_PARAMS,
    PromptRunner, SequenceKind, get_global_prompt_runner, register_global_extension_handler,
    register_global_prompt_runner,
};

use chrono::Utc;
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
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};
use tracing;
use tracing::Instrument as _;

use crate::{
    adapter::Adapter,
    client_type::McpClientType,
    config::ToolConfig,
    file_ops::{DefaultFileOpsProvider, DefaultWebPageFetcher, FileOpsProvider, WebPageFetcher},
    llm_service::{DefaultLlmCompletionService, LlmCompletionService},
    operation_monitor::{Operation, OperationStatus},
};
use serde_json::Value;

pub(crate) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// `AhmaMcpService` is the server handler for the MCP service.
#[derive(Clone)]
pub struct AhmaMcpService {
    pub adapter: Arc<Adapter>,
    pub operation_monitor: Arc<crate::operation_monitor::OperationMonitor>,
    pub configs: Arc<RwLock<HashMap<String, ToolConfig>>>,
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
    /// Registered handlers for extension tool types (e.g. task_tree, decompose)
    pub extension_handlers: Arc<std::sync::RwLock<HashMap<String, Arc<dyn ExtensionToolHandler>>>>,
    /// Custom file operations backend.
    pub file_ops_provider: Arc<dyn FileOpsProvider>,
    /// Custom web page fetcher.
    pub web_page_fetcher: Arc<dyn WebPageFetcher>,
    /// Custom LLM completion service.
    pub llm_service: Arc<dyn LlmCompletionService>,
    /// Last received timestamp for keep-alive optimization.
    pub last_received_signal: Arc<std::sync::atomic::AtomicU64>,
    /// True if the connected peer is an Ahma node.
    pub is_ahma_peer: Arc<std::sync::atomic::AtomicBool>,
    /// Token minimization and output optimizer context.
    pub output_optimizer: Arc<tokio::sync::Mutex<crate::output_optimizer::OutputOptimizer>>,
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
    pub vault_audited_ops: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// All external MCP servers (HTTP and stdio) for agent tool routing.
    pub mcp_connections: Arc<tokio::sync::RwLock<crate::mcp_client::McpConnectionManager>>,
    /// Session-scoped web-egress approvals (R-WEB.5). Holds the domains granted or
    /// denied for this session and coordinates in-flight approval prompts. Its
    /// grant/deny snapshots are threaded into the `[web]` policy decision on every
    /// `fetch_webpage`, so a session approval takes effect without a restart.
    pub web_approval: Arc<ahma_common::web_approval::WebApprovalCoordinator>,
}

impl AhmaMcpService {
    /// Retrieve a list of all locally and externally registered tools in ToolInfo format.
    pub async fn get_all_available_tools(&self) -> Vec<crate::mcp_client::ToolInfo> {
        let mut tools = vec![
            crate::mcp_client::ToolInfo {
                name: "await".to_string(),
                description: Some("Block until a started operation completes and return its final result. Operations notify automatically when they finish, so prefer doing other useful work first; reach for `await` only when the next step truly depends on the result.".to_string()),
                input_schema: serde_json::Value::Object(self.generate_input_schema_for_wait().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "status".to_string(),
                description: Some("Return a snapshot of active and completed operations without blocking. Completion is pushed via notifications, so this is for ad-hoc inspection rather than polling.".to_string()),
                input_schema: serde_json::Value::Object(self.generate_input_schema_for_status().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "run_terminal_command".to_string(),
                description: Some("Run a shell command inside a kernel-level filesystem sandbox (Landlock on Linux, Seatbelt on macOS, Job Objects on Windows). Returns an operation_id immediately; use `status`, `await`, or `cancel` to manage long-running work. Supports pipes, redirects, environment variables, and full shell syntax. Set `monitor_level` to stream error/warning alerts from stdout or stderr.".to_string()),
                input_schema: serde_json::Value::Object(self.generate_input_schema_for_run_terminal_command().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "logs_list".to_string(),
                description: Some("List all log files in the project log directory (`./logs/`). Returns file names, sizes, modification times, and symlink targets. Use this to discover which log files are available before calling logs_read or logs_search.".to_string()),
                input_schema: serde_json::Value::Object(handlers::log_tools::logs_list_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "logs_approve".to_string(),
                description: Some("Approve a blocked out-of-scope log symlink target to allow AI read access.".to_string()),
                input_schema: serde_json::Value::Object(handlers::log_tools::logs_approve_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "logs_read".to_string(),
                description: Some("Read lines from a project log file with optional pagination. Sensitive values (tokens, passwords, API keys) are redacted by default. Use `raw: true` only when debugging credential issues.".to_string()),
                input_schema: serde_json::Value::Object(handlers::log_tools::logs_read_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "logs_search".to_string(),
                description: Some("Search a project log file for lines matching a pattern (case-insensitive substring match by default). Returns matching lines with line numbers. Sensitive values are redacted by default.".to_string()),
                input_schema: serde_json::Value::Object(handlers::log_tools::logs_search_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "restart".to_string(),
                description: Some("Force stop and restart the background bridge server, disconnecting all active sessions (including TUI and other IDEs) to apply updates or recover from a bad state.".to_string()),
                input_schema: serde_json::Value::Object(handlers::restart_tool::restart_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "read_file".to_string(),
                description: Some("Read UTF-8 text from a scoped file, with optional line slicing.".to_string()),
                input_schema: serde_json::Value::Object(handlers::harness_tools::read_file_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "list_dir".to_string(),
                description: Some("List entries in a scoped directory with basic metadata.".to_string()),
                input_schema: serde_json::Value::Object(handlers::harness_tools::list_dir_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "file_search".to_string(),
                description: Some("Find files by glob pattern inside the sandbox scope.".to_string()),
                input_schema: serde_json::Value::Object(handlers::harness_tools::file_search_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "grep_search".to_string(),
                description: Some("Search file contents by plain text or regex.".to_string()),
                input_schema: serde_json::Value::Object(handlers::harness_tools::grep_search_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "fetch_webpage".to_string(),
                description: Some("Fetch and extract readable text from an HTTP/HTTPS webpage.".to_string()),
                input_schema: serde_json::Value::Object(handlers::harness_tools::fetch_webpage_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "write_file".to_string(),
                description: Some("Write UTF-8 content to a scoped file (create or overwrite).".to_string()),
                input_schema: serde_json::Value::Object(handlers::harness_tools::write_file_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "replace_in_file".to_string(),
                description: Some("Replace exact string occurrences in a scoped UTF-8 file.".to_string()),
                input_schema: serde_json::Value::Object(handlers::harness_tools::replace_in_file_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "todo_write".to_string(),
                description: Some("Record or update your task plan as a checklist (pass the full list each time; it replaces the current plan). Use for any multi-step task: list the steps, mark one in_progress, mark finished steps completed.".to_string()),
                input_schema: serde_json::Value::Object(handlers::todo_tool::todo_write_schema().as_ref().clone()),
            },
            crate::mcp_client::ToolInfo {
                name: "log_monitor".to_string(),
                description: Some("Start a real-time log monitoring session on a file inside the sandbox. Reads new lines as they are written, runs them through the AI for issue detection, and sends alerts.".to_string()),
                input_schema: serde_json::Value::Object(
                    schema::object_input_schema(
                        {
                            let mut props = serde_json::Map::new();
                            props.insert("file_path".to_string(), schema::string_property("Path of the log file to monitor (within sandbox scope)"));
                            props.insert("detection_prompt".to_string(), schema::string_property("Optional prompt guiding AI issue detection"));
                            props.insert("llm_base_url".to_string(), schema::string_property("Optional custom LLM base URL"));
                            props.insert("llm_model".to_string(), schema::string_property("Optional custom LLM model"));
                            props.insert("llm_api_key".to_string(), schema::string_property("Optional custom LLM API key"));
                            props
                        },
                        &["file_path"],
                    ).as_ref().clone()
                ),
            },
        ];

        {
            let configs_lock = self.configs.read().unwrap();
            for config in configs_lock.values() {
                if !self.is_config_visible_to_client(config) {
                    continue;
                }
                for t in self.create_tools_from_config(config) {
                    tools.push(crate::mcp_client::ToolInfo {
                        name: t.name.to_string(),
                        description: t.description.map(|d| d.to_string()),
                        input_schema: serde_json::Value::Object(t.input_schema.as_ref().clone()),
                    });
                }
            }
        }

        let external_mgr = self.mcp_connections.read().await;
        for ext_tool in external_mgr.aggregate_tools() {
            tools.push(ext_tool.clone());
        }

        tools
    }

    fn task_vault_root(&self) -> Option<PathBuf> {
        let cfg_root = self
            .app_config
            .read()
            .ok()
            .and_then(|cfg| cfg.as_ref().and_then(|c| c.task_vault.clone()));

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
        let mut peer_guard = self.peer.write().unwrap();
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
        matches!(
            tool_name,
            "await"
                | "status"
                | "cancel"
                | "logs_list"
                | "logs_approve"
                | "logs_read"
                | "logs_search"
                | "restart"
        )
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
        if let Ok(mut set) = self.vault_audited_ops.lock() {
            set.insert(operation_id.to_string());
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
        if let Ok(mut set) = self.vault_audited_ops.lock() {
            set.remove(operation_id);
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
    /// * `progressive_disclosure` - If true, only built-in + activate_tools shown initially.
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

        let progress_push = progress_push::ProgressPushRouter::new();
        progress_push.spawn_forwarder(&operation_monitor);

        // Reset roots_received to false so that client roots/list negotiation
        // or no-roots auto-scoping can occur for this service session.
        adapter.sandbox().set_roots_received(false);

        let service = Self {
            adapter,
            operation_monitor,
            configs: Arc::new(RwLock::new((*configs).clone())),
            guidance,
            force_synchronous,
            defer_sandbox,
            peer: Arc::new(RwLock::new(None)),
            monitor_rate_limit_seconds: crate::log_monitor::DEFAULT_RATE_LIMIT_SECONDS,
            app_config: Arc::new(RwLock::new(None)),
            current_tools_dir: Arc::new(RwLock::new(None)),
            extension_handlers: Arc::new(std::sync::RwLock::new(
                types::get_global_extension_handlers()
                    .read()
                    .unwrap()
                    .clone(),
            )),
            file_ops_provider: Arc::new(DefaultFileOpsProvider),
            web_page_fetcher: Arc::new(DefaultWebPageFetcher),
            llm_service: Arc::new(DefaultLlmCompletionService),
            last_received_signal: Arc::new(std::sync::atomic::AtomicU64::new(
                ahma_common::keepalive::current_timestamp_ms(),
            )),
            is_ahma_peer: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            output_optimizer: Arc::new(tokio::sync::Mutex::new(
                crate::output_optimizer::OutputOptimizer::new(false, None),
            )),
            // Self-correction (tool-name/argument healing + failure-loop
            // detection) is on by default — see `set_app_config` for why.
            harness_guard: Arc::new(tokio::sync::Mutex::new(
                crate::harness_guard::HarnessGuard::new(true),
            )),
            todo_list: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            progress_push,
            vault_audited_ops: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            mcp_connections: Arc::new(tokio::sync::RwLock::new(
                crate::mcp_client::McpConnectionManager::default(),
            )),
            web_approval: Arc::new(ahma_common::web_approval::WebApprovalCoordinator::new()),
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
                let audited = service
                    .vault_audited_ops
                    .lock()
                    .map(|mut set| set.remove(&op_id))
                    .unwrap_or(false);
                if !audited {
                    continue;
                }
                use ahma_common::event_dispatcher::OperationEvent;
                let (success, duration_ms) = match event.as_ref() {
                    OperationEvent::Completed { duration_ms, .. } => (true, *duration_ms),
                    OperationEvent::Failed { duration_ms, .. }
                    | OperationEvent::Cancelled { duration_ms, .. }
                    | OperationEvent::TimedOut { duration_ms, .. } => (false, *duration_ms),
                    _ => (false, 0),
                };
                service
                    .emit_vault_tool_complete(&op_id, success, duration_ms)
                    .await;
            }
        });
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

    /// Sets a custom LLM completion service.
    pub fn with_llm_service(mut self, service: Arc<dyn LlmCompletionService>) -> Self {
        self.llm_service = service;
        self
    }

    /// Store the AppConfig that constructed this service so runtime events
    /// (such as `roots/list` arrival) can rediscover per-client `.ahma/` dirs.
    pub fn set_app_config(&self, config: Arc<crate::shell::cli::AppConfig>) {
        if let Some(dir) = config.tools_dir.clone() {
            *self.current_tools_dir.write().unwrap() = Some(dir);
        }
        if let Ok(mut opt) = self.output_optimizer.try_lock() {
            opt.enabled = config.minimize_tokens;
        }
        // Self-correction (tool-name/argument healing + failure-loop detection)
        // is universally safe and is exactly what stops a model from burning its
        // turn budget re-issuing the same broken call, so it stays on regardless
        // of `small_model_harness`. That flag now governs only the verbose
        // coaching hints injected in the agent loop, not self-correction.
        if let Ok(mut guard) = self.harness_guard.try_lock() {
            guard.enabled = true;
        }
        *self.app_config.write().unwrap() = Some(config);
    }

    /// Register an extension handler for custom tool routing.
    pub fn register_extension_handler(&self, name: String, handler: Arc<dyn ExtensionToolHandler>) {
        self.extension_handlers
            .write()
            .unwrap()
            .insert(name, handler);
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
        let input_schema =
            schema::generate_schema_for_tool_config(tool_config, self.guidance.as_ref());
        Tool::new(base_name.clone(), description, input_schema).with_title(base_name.clone())
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
        let input_schema = Arc::new(schema::generate_single_command_schema_pub(
            tool_config,
            &(sub_path.to_string(), subcommand_config),
        ));
        Tool::new(flat_name.clone(), description, input_schema).with_title(flat_name)
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

    /// Resolves guidance-augmented description for a tool config by key.
    fn tool_description(&self, tool_config: &ToolConfig, key: &str) -> String {
        let mut description = tool_config.description.clone();
        if let Some(guidance_config) = self.guidance.as_ref() {
            let default_key = key.to_string();
            let gk = tool_config.guidance_key.as_ref().unwrap_or(&default_key);
            if let Some(guidance_text) = guidance_config.guidance_blocks.get(gk) {
                description = format!("{}\n\n{}", guidance_text, description);
            }
        }
        description
    }

    /// Builds a guidance-augmented description from explicit text.
    fn tool_description_text(&self, tool_config: &ToolConfig, key: &str, base: &str) -> String {
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

    /// Names that are always hard-wired in the protocol layer and must not
    /// appear in user/bundled configs (we skip duplicates here).
    const HARDCODED_TOOLS: &'static [&'static str] = &[
        "await",
        "status",
        "run_terminal_command",
        "cancel",
        "logs_list",
        "logs_approve",
        "logs_read",
        "logs_search",
        "restart",
        "read_file",
        "list_dir",
        "file_search",
        "grep_search",
        "fetch_webpage",
        "write_file",
        "replace_in_file",
        "agent",
        "todo_write",
    ];

    /// Returns true if a configured tool should be exposed to the client
    /// given the current disclosure state. Centralises the filter so
    /// `list_tools()` and `list_tool_names()` cannot drift apart.
    fn is_config_visible_to_client(&self, config: &ToolConfig) -> bool {
        if Self::HARDCODED_TOOLS.contains(&config.name.as_str()) {
            return false;
        }
        if !config.enabled {
            tracing::debug!("Skipping disabled tool '{}'", config.name);
            return false;
        }
        true
    }

    /// Resolves a `tools/call` tool name to its config, returning the
    /// owned config and (for flattened subcommand names) the resolved
    /// subcommand path.
    fn find_tool_config(&self, tool_name: &str) -> Option<(ToolConfig, Option<String>)> {
        let configs_lock = self.configs.read().unwrap();
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
        let push_enabled = progress_token.is_some() && client_type.supports_progress();
        if let Some(token) = progress_token.clone()
            && push_enabled
        {
            progress_push::push_progress(
                &peer,
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

        if let Some(token) = progress_token
            && push_enabled
        {
            let (success, full_output) = match &result {
                Ok(output) => (true, output.clone()),
                Err(e) => (false, format!("Error: {}", e)),
            };
            let message = progress_push::sync_final_message(
                &id,
                base_command,
                &Self::sync_tool_progress_description(base_command, working_directory),
                working_directory,
                success,
                duration_ms,
                &full_output,
            );
            progress_push::push_progress(&peer, token, 100.0, message, true).await;
        }

        match result {
            Ok(output) => Ok(handlers::common::text_result(output)),
            Err(e) => {
                let error_message = format!("Synchronous execution failed: {}", e);
                tracing::error!("{}", error_message);
                Err(handlers::common::mcp_internal(error_message))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_async_tool(
        &self,
        tool_name: &str,
        id: String,
        base_command: &str,
        working_directory: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        timeout: Option<u64>,
        subcommand_config: &crate::config::SubcommandConfig,
        config: &ToolConfig,
        progress_token: Option<rmcp::model::ProgressToken>,
        client_type: McpClientType,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.emit_vault_tool_call(&id, tool_name, &Self::summarize_arguments(&arguments))
            .await;

        if let Some(token) = progress_token {
            self.progress_push
                .register(&id, peer, token, client_type)
                .await;
        }

        let log_monitor_config = config.monitor_level.as_deref().map(|level_str| {
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
        });

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
                if let Some(result) =
                    handlers::common::try_automatic_async_completion(&self.operation_monitor, &id)
                        .await
                {
                    return Ok(result);
                }
                let hint = crate::tool_hints::preview(&id, tool_name);
                Ok(handlers::common::text_result(format!(
                    "AHMA ID: {}{}",
                    id, hint
                )))
            }
            Err(e) => {
                // The operation never started, so no terminal event will
                // arrive to clean up the push registration.
                self.progress_push.unregister(&id).await;
                self.emit_vault_tool_complete(&id, false, 0).await;
                let error_message = format!("Failed to start asynchronous operation: {}", e);
                tracing::error!("{}", error_message);
                Err(handlers::common::mcp_internal(error_message))
            }
        }
    }
}

#[async_trait::async_trait]
impl ServerHandler for AhmaMcpService {
    fn get_info(&self) -> ServerInfo {
        let instructions = "Ahma exposes shell, build, test, and log-monitoring tools that run inside a \
                  kernel-enforced workspace sandbox (Landlock on Linux, Seatbelt on macOS, \
                  Job Objects on Windows). Prefer `run_terminal_command` over the native terminal when: \
                  (1) the command writes to disk — the sandbox guarantees the write stays inside the workspace; \
                  (2) the command is long-running — `run_terminal_command` returns an operation_id immediately \
                  and you can `status`, `await`, or `cancel` it without blocking; \
                  (3) the command's output should be watched for errors — set `monitor_level` and ahma \
                  streams alerts when matching lines appear; \
                  (4) multiple commands should run concurrently — each call gets its own operation_id. \
                  Workflow: start operations, do other useful work, then `await` the ids you need — \
                  completion is also pushed via notifications, so avoid polling `status` in a loop. \
                  Results include a bounded stdout/stderr window plus an `output_file` path holding the \
                  COMPLETE output of the operation; when the inline output is marked truncated, read or \
                  grep that file instead of re-running the command. \
                  For read-only file inspection (read, grep, glob, replace) keep using the IDE's native \
                  file tools — that is what they are for.".to_string();

        let capabilities = ServerCapabilities::builder()
            .enable_tools_with(ToolsCapability {
                list_changed: Some(true),
            })
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
                if client_type.supports_progress() {
                    "enabled"
                } else {
                    "disabled"
                }
            );

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
                "todo-hash".to_string(), // TODO: inject build hash
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

            // Run synchronously per R19.3 - sandbox configuration is a lifecycle
            // operation that should complete before we're "ready"
            self.configure_sandbox_from_roots(peer).await;
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
            let peer = &context.peer;

            // Run synchronously per R19.3 - sandbox configuration must complete
            // before we can safely process tools/call requests. Initial handshake
            // timing is not super critical, but correctness is.
            self.configure_sandbox_from_roots(peer).await;
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
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        self.last_received_signal.store(
            ahma_common::keepalive::current_timestamp_ms(),
            std::sync::atomic::Ordering::Relaxed,
        );
        async move {
            let mut tools = vec![
                // Hard-wired await command - always available
                Tool::new(
                    "await",
                    "Block until a started operation completes and return its final result. Operations notify automatically when they finish, so prefer doing other useful work first; reach for `await` only when the next step truly depends on the result.",
                    self.generate_input_schema_for_wait(),
                )
                .with_title("await"),
                // Hard-wired status command - always available
                Tool::new(
                    "status",
                    "Return a snapshot of active and completed operations without blocking. Completion is pushed via notifications, so this is for ad-hoc inspection rather than polling.",
                    self.generate_input_schema_for_status(),
                )
                .with_title("status"),
                // Hard-wired run_terminal_command command - always available
                Tool::new(
                    "run_terminal_command",
                    "Run a shell command inside a kernel-level filesystem sandbox (Landlock on Linux, Seatbelt on macOS, Job Objects on Windows). Returns an operation_id immediately; use `status`, `await`, or `cancel` to manage long-running work. Supports pipes, redirects, environment variables, and full shell syntax. Set `monitor_level` to stream error/warning alerts from stdout or stderr.",
                    self.generate_input_schema_for_run_terminal_command(),
                )
                .with_title("run_terminal_command"),
                // Hard-wired log inspection tools — always available
                Tool::new(
                    "logs_list",
                    "List all log files in the project log directory (`./logs/`). Returns file names, sizes, modification times, and symlink targets. Use this to discover which log files are available before calling logs_read or logs_search.",
                    handlers::log_tools::logs_list_schema(),
                )
                .with_title("logs_list"),
                Tool::new(
                    "logs_approve",
                    "Approve a blocked out-of-scope log symlink target to allow AI read access.",
                    handlers::log_tools::logs_approve_schema(),
                )
                .with_title("logs_approve"),
                Tool::new(
                    "logs_read",
                    "Read lines from a project log file with optional pagination. Sensitive values (tokens, passwords, API keys) are redacted by default. Use `raw: true` only when debugging credential issues.",
                    handlers::log_tools::logs_read_schema(),
                )
                .with_title("logs_read"),
                Tool::new(
                    "logs_search",
                    "Search a project log file for lines matching a pattern (case-insensitive substring match by default). Returns matching lines with line numbers. Sensitive values are redacted by default.",
                    handlers::log_tools::logs_search_schema(),
                )
                .with_title("logs_search"),
                Tool::new(
                    "restart",
                    "Force stop and restart the background bridge server, disconnecting all active sessions (including TUI and other IDEs) to apply updates or recover from a bad state.",
                    handlers::restart_tool::restart_schema(),
                )
                .with_title("restart"),
                Tool::new(
                    "cancel",
                    "Cancel a running background operation by `id`, or cancel EVERY in-flight operation with `all: true`. Each cancellation reaps the operation's full process tree (cargo/rustc/sccache) — the clean way to stop wedged work without killing and restarting the server.",
                    handlers::cancel_tool::cancel_schema(),
                )
                .with_title("cancel"),
                Tool::new(
                    "sandbox_grant",
                    "Propose adding an out-of-scope path as a persistent sandbox root in ~/.ahma/settings.toml. Call this when a command fails with a `sandbox_denial` error. WITHOUT `confirm: true` it only PREVIEWS — it returns the full settings-file path, the exact line it would add, and a risk assessment so you can show the human and get approval first. Catastrophic paths (filesystem root, $HOME, credential dirs, system dirs, workspace parents) are REFUSED even with confirmation. On `confirm: true` it writes the grant; run `restart` to apply, then re-run the blocked command.",
                    handlers::sandbox_grant_tool::sandbox_grant_schema(),
                )
                .with_title("sandbox_grant"),
                Tool::new(
                    "read_file",
                    "Read UTF-8 text from a scoped file, with optional line slicing.",
                    handlers::harness_tools::read_file_schema(),
                )
                .with_title("read_file"),
                Tool::new(
                    "list_dir",
                    "List entries in a scoped directory with basic metadata.",
                    handlers::harness_tools::list_dir_schema(),
                )
                .with_title("list_dir"),
                Tool::new(
                    "file_search",
                    "Find files by glob pattern inside the sandbox scope.",
                    handlers::harness_tools::file_search_schema(),
                )
                .with_title("file_search"),
                Tool::new(
                    "grep_search",
                    "Search file contents by plain text or regex.",
                    handlers::harness_tools::grep_search_schema(),
                )
                .with_title("grep_search"),
                Tool::new(
                    "fetch_webpage",
                    "Fetch and extract readable text from an HTTP/HTTPS webpage.",
                    handlers::harness_tools::fetch_webpage_schema(),
                )
                .with_title("fetch_webpage"),
                Tool::new(
                    "write_file",
                    "Write UTF-8 content to a scoped file (create or overwrite).",
                    handlers::harness_tools::write_file_schema(),
                )
                .with_title("write_file"),
                Tool::new(
                    "replace_in_file",
                    "Replace exact string occurrences in a scoped UTF-8 file.",
                    handlers::harness_tools::replace_in_file_schema(),
                )
                .with_title("replace_in_file"),
                Tool::new(
                    "agent",
                    "Delegate a self-contained task to ahma's own agent loop as a sub-agent. ahma runs its full tool-using loop (read/edit files, run commands in the sandbox, search) with the model the user last selected in `ahma tui`, and returns the final answer. Use this to offload a focused sub-task — investigating code, producing a file or report, or answering a question grounded in the workspace — without doing the steps yourself.",
                    handlers::agent_tool::agent_schema(),
                )
                .with_title("agent"),
                Tool::new(
                    "todo_write",
                    "Record or update your task plan as a checklist. Pass the FULL list of steps each time — it replaces the current plan. Use this at the start of any multi-step task, then call it again to mark a step in_progress before you work on it and completed when it's done. Keeps you (and the user) oriented across turns.",
                    handlers::todo_tool::todo_write_schema(),
                )
                .with_title("todo_write"),
                Tool::new(
                    "log_monitor",
                    "Start a real-time log monitoring session on a file inside the sandbox. Reads new lines as they are written, runs them through the AI for issue detection, and sends alerts.",
                    schema::object_input_schema(
                        {
                            let mut props = serde_json::Map::new();
                            props.insert("file_path".to_string(), schema::string_property("Path of the log file to monitor (within sandbox scope)"));
                            props.insert("detection_prompt".to_string(), schema::string_property("Optional prompt guiding AI issue detection"));
                            props.insert("llm_base_url".to_string(), schema::string_property("Optional custom LLM base URL"));
                            props.insert("llm_model".to_string(), schema::string_property("Optional custom LLM model"));
                            props.insert("llm_api_key".to_string(), schema::string_property("Optional custom LLM API key"));
                            props
                        },
                        &["file_path"],
                    ),
                )
                .with_title("log_monitor"),
            ];

            {
                let configs_lock = self.configs.read().unwrap();
                for config in configs_lock.values() {
                    if !self.is_config_visible_to_client(config) {
                        continue;
                    }
                    tools.extend(self.create_tools_from_config(config));
                }
            }

            // Add external MCP tools from McpConnectionManager
            let external_mgr = self.mcp_connections.read().await;
            for ext_tool in external_mgr.aggregate_tools() {
                let description = ext_tool.description.clone();
                let input_schema = match ext_tool.input_schema.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                let schema_arc = Arc::new(input_schema);
                tools.push(
                    Tool::new(
                        ext_tool.name.clone(),
                        description.unwrap_or_default(),
                        schema_arc,
                    )
                    .with_title(ext_tool.name.clone()),
                );
            }

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

            let mut tool_name = params.name.clone();
            let mut tool_args = params.arguments.clone();

            if is_guard_active
                && let Some(early) = self.harness_guard_preprocess(&mut tool_name, &mut tool_args)
            {
                return Ok(early);
            }

            let mut run_params = CallToolRequestParams::new(tool_name.clone());
            run_params.arguments = tool_args.clone();
            run_params.meta = params.meta.clone();
            run_params.task = params.task.clone();

            let result = match tool_name.as_ref() {
                "status" => {
                    self.handle_status(run_params.arguments.unwrap_or_default())
                        .await
                }
                "await" => self.handle_await(run_params).await,
                "run_terminal_command" => {
                    self.handle_run_terminal_command(run_params, context).await
                }
                "cancel" => {
                    self.handle_cancel(run_params.arguments.unwrap_or_default())
                        .await
                }
                "sandbox_grant" => {
                    let client_type = McpClientType::from_peer(&context.peer);
                    self.handle_sandbox_grant(run_params.arguments.unwrap_or_default(), client_type)
                        .await
                }
                "logs_list" => {
                    self.handle_logs_list(run_params.arguments.unwrap_or_default())
                        .await
                }
                "logs_approve" => {
                    self.handle_logs_approve(run_params.arguments.unwrap_or_default())
                        .await
                }
                "logs_read" => {
                    self.handle_logs_read(run_params.arguments.unwrap_or_default())
                        .await
                }
                "logs_search" => {
                    self.handle_logs_search(run_params.arguments.unwrap_or_default())
                        .await
                }
                "restart" => {
                    self.handle_restart(run_params.arguments.unwrap_or_default())
                        .await
                }
                "read_file" => {
                    self.handle_read_file(run_params.arguments.unwrap_or_default())
                        .await
                }
                "list_dir" => {
                    self.handle_list_dir(run_params.arguments.unwrap_or_default())
                        .await
                }
                "file_search" => {
                    self.handle_file_search(run_params.arguments.unwrap_or_default())
                        .await
                }
                "grep_search" => {
                    self.handle_grep_search(run_params.arguments.unwrap_or_default())
                        .await
                }
                "fetch_webpage" => {
                    self.handle_fetch_webpage(run_params.arguments.unwrap_or_default())
                        .await
                }
                "write_file" => {
                    self.handle_write_file(run_params.arguments.unwrap_or_default())
                        .await
                }
                "replace_in_file" => {
                    self.handle_replace_in_file(run_params.arguments.unwrap_or_default())
                        .await
                }
                "agent" => {
                    self.handle_agent(run_params.arguments.unwrap_or_default())
                        .await
                }
                "todo_write" => {
                    self.handle_todo_write(run_params.arguments.unwrap_or_default())
                        .await
                }
                "log_monitor" => {
                    self.handle_log_monitor(run_params.arguments.unwrap_or_default(), context)
                        .await
                }
                _ => self.dispatch_configured_tool(run_params, context).await,
            };

            if is_guard_active {
                self.record_result_in_loop_detector(tool_name.as_ref(), &tool_args, &result);
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
        if notification.method == "notifications/ahma/heartbeat" {
            self.last_received_signal.store(
                ahma_common::keepalive::current_timestamp_ms(),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        std::future::ready(())
    }
}

impl AhmaMcpService {
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
        let known = Self::HARDCODED_TOOLS.to_vec();
        let configs_lock = self.configs.read().unwrap();
        let config_names: Vec<String> = configs_lock.keys().cloned().collect();
        drop(configs_lock);
        let mut known_str: Vec<&str> = known;
        for name in &config_names {
            known_str.push(name);
        }

        // Run the guard pipeline over a mutable copy, then write any healing back.
        let had_args = tool_args.is_some();
        let mut name = tool_name.to_string();
        let mut args = tool_args.take().unwrap_or_default();
        let ctx = GuardContext {
            known_tools: &known_str,
        };
        let outcome = match self.harness_guard.try_lock() {
            Ok(guard) => guard.inspect(&ctx, &mut name, &mut args),
            Err(_) => GuardOutcome::Proceed,
        };
        if name.as_str() != &**tool_name {
            *tool_name = std::borrow::Cow::Owned(name);
        }
        // Preserve a `None` payload: handlers distinguish "no arguments" from an
        // empty object (e.g. task_tree's "Missing arguments payload"). Only
        // restore args if they existed originally or the pipeline added some.
        if had_args || !args.is_empty() {
            *tool_args = Some(args);
        }

        match outcome {
            GuardOutcome::Block(msg) => {
                Some(CallToolResult::error(vec![rmcp::model::Content::text(msg)]))
            }
            GuardOutcome::Proceed => None,
        }
    }

    /// Notify the guard pipeline of a completed tool call so stateful guards
    /// (loop detection) can track repeated identical failures.
    fn record_result_in_loop_detector(
        &self,
        tool_name: &str,
        tool_args: &Option<serde_json::Map<String, Value>>,
        result: &Result<CallToolResult, McpError>,
    ) {
        let failed = match result {
            Ok(res) => res.is_error.unwrap_or(false),
            Err(_) => true,
        };
        let args = tool_args.clone().unwrap_or_default();
        if let Ok(guard) = self.harness_guard.try_lock() {
            guard.observe(tool_name, &args, failed);
        }
    }

    fn guard_sandbox_ready_for_tool_calls(&self) -> Result<(), McpError> {
        if self.adapter.sandbox().is_ready_for_tool_calls() {
            return Ok(());
        }
        let error_message =
            "Sandbox initializing from client roots - retry tools/call after roots/list completes"
                .to_string();
        tracing::warn!("{}", error_message);
        Err(handlers::common::mcp_internal(error_message))
    }

    fn parse_llm_provider(
        &self,
        arguments: &serde_json::Map<String, Value>,
    ) -> crate::config::LlmProviderConfig {
        let llm_base_url = arguments
            .get("llm_base_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let llm_model = arguments
            .get("llm_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let llm_api_key = arguments
            .get("llm_api_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

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
        let configs_lock = self.configs.read().unwrap();
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

        let file_path_str = arguments
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                McpError::invalid_params("file_path parameter is required".to_string(), None)
            })?;

        let detection_prompt = arguments
            .get("detection_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("Identify errors or warnings")
            .to_string();

        let path = std::path::Path::new(file_path_str);
        let safe_path = self
            .adapter
            .sandbox()
            .validate_path(path)
            .map_err(|e| McpError::invalid_params(format!("Invalid file path: {}", e), None))?;

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
        if let Some(token) = progress_token {
            self.progress_push
                .register(&op_id, context.peer.clone(), token, client_type)
                .await;
        }

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
        if config.task_tree.is_some() {
            return Some("task_tree".to_string());
        }
        if config.decompose.is_some() {
            return Some("decompose".to_string());
        }
        if config.worker.is_some() {
            return Some("worker".to_string());
        }
        let handlers = self.extension_handlers.read().unwrap();
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
            .and_then(|key| self.extension_handlers.read().unwrap().get(&key).cloned());
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
            return sequence::handle_sequence_tool(
                &self.adapter,
                &self.operation_monitor,
                &self.progress_push,
                &self.configs,
                &config,
                params,
                context,
            )
            .await;
        }

        if config.tool_type == Some(crate::config::ToolType::Livelog) {
            return self.handle_livelog_call(&config, &params, &context).await;
        }

        self.dispatch_subcommand_tool(params, context, config, flattened_subcommand)
            .await
    }

    /// Routes a `tools/call` for a configured (non-built-in) tool. Validates
    /// the sandbox is locked, resolves the tool config, and dispatches by
    /// tool type (sequence / livelog / subcommand).
    async fn dispatch_configured_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.guard_sandbox_ready_for_tool_calls()?;

        if params.name.contains("::") {
            let mgr = {
                let guard = self.mcp_connections.read().await;
                guard.clone()
            };
            if let Some((server_name, _)) = mgr.resolve_tool_name(&params.name)
                && mgr.servers.iter().any(|s| s.name == server_name)
            {
                let arguments = params.arguments.unwrap_or_default();
                let args_val = serde_json::Value::Object(arguments);
                match mgr.call_tool(&params.name, args_val).await {
                    Ok((output, is_error)) => {
                        if is_error {
                            return Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                                output,
                            )]));
                        } else {
                            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                                output,
                            )]));
                        }
                    }
                    Err(e) => {
                        return Err(McpError::internal_error(
                            format!("External tool call failed: {e}"),
                            None,
                        ));
                    }
                }
            }
        }

        let (config, flattened_subcommand) = self.resolve_configured_tool(&params.name)?;
        self.dispatch_resolved_configured_tool(params, context, config, flattened_subcommand)
            .await
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
                self.call_async_tool(
                    tool_name,
                    id,
                    base_command,
                    working_directory,
                    arguments,
                    timeout,
                    subcommand_config,
                    config,
                    progress_token,
                    client_type,
                    context.peer.clone(),
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
            return sequence::handle_subcommand_sequence(
                &self.adapter,
                &self.progress_push,
                &config,
                subcommand_config,
                params,
                context,
            )
            .await;
        }

        let base_command = command_parts.join(" ");
        let working_directory = self.resolve_working_directory(&arguments);

        if let Some(staged_result) = self
            .maybe_stage_configured_delete(&base_command, &working_directory, &arguments)
            .await?
        {
            return Ok(staged_result);
        }

        let timeout = arguments.get("timeout_seconds").and_then(|v| v.as_u64());
        let execution_mode = self.determine_execution_mode(subcommand_config, &config, &arguments);

        self.execute_subcommand_command(
            &tool_name,
            &base_command,
            &working_directory,
            arguments,
            timeout,
            subcommand_config,
            &config,
            context,
            execution_mode,
        )
        .await
    }

    /// Picks the working directory for a tool call: explicit argument first,
    /// then the first sandbox scope (skipped in test mode), then ".".
    fn resolve_working_directory(
        &self,
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> String {
        if let Some(path) = arguments.get("working_directory").and_then(|v| v.as_str()) {
            return path.to_string();
        }
        if self.adapter.sandbox().is_test_mode() {
            return ".".to_string();
        }
        self.adapter
            .sandbox()
            .scopes()
            .first()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string())
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
        if let Some(token) = progress_token {
            self.progress_push
                .register(&op_id, context.peer.clone(), token, client_type)
                .await;
        }
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
        let mut names: Vec<String> = vec![
            "await".into(),
            "status".into(),
            "run_terminal_command".into(),
        ];

        let configs_lock = self.configs.read().unwrap();
        for config in configs_lock.values() {
            if self.is_config_visible_to_client(config) {
                names.push(config.name.clone());
            }
        }
        names
    }
}

impl ahma_common::keepalive::KeepAlive for AhmaMcpService {
    async fn send_standard_ping(&self) -> anyhow::Result<()> {
        let peer_opt = self.peer.read().unwrap().clone();
        if let Some(peer) = peer_opt {
            peer.send_request(rmcp::model::ServerRequest::PingRequest(Default::default()))
                .await?;
        }
        Ok(())
    }

    async fn send_enhanced_heartbeat(
        &self,
        payload: ahma_common::keepalive::HeartbeatPayload,
    ) -> anyhow::Result<()> {
        let peer_opt = self.peer.read().unwrap().clone();
        if let Some(peer) = peer_opt {
            let params = serde_json::to_value(payload)?;

            peer.send_notification(rmcp::model::ServerNotification::CustomNotification(
                rmcp::model::CustomNotification::new("notifications/ahma/heartbeat", Some(params)),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            monitor.get_active_operations().await.is_empty(),
            "cancel-all must leave no active operations"
        );
    }

    #[tokio::test]
    async fn handle_cancel_all_with_nothing_running_is_graceful() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            Duration::from_secs(30),
        )));
        let service = make_service_with_monitor(monitor.clone(), Arc::new(None)).await;

        let args = json!({"all": true}).as_object().unwrap().clone();
        let result = service.handle_cancel(args).await.expect("cancel all");
        assert!(first_text(&result).contains("No in-flight operations to cancel"));
    }

    #[tokio::test]
    async fn handle_cancel_terminal_operation_reports_already_terminal() {
        let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn parse_file_uri_to_path_accepts_localhost_and_decodes() {
        let p = AhmaMcpService::parse_file_uri_to_path(
            "file://localhost/Users/test/My%20Project/file.txt?x=1#frag",
        )
        .expect("path");
        assert_eq!(p.to_string_lossy(), "/Users/test/My Project/file.txt");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn parse_file_uri_to_path_rejects_non_file_scheme_and_relative() {
        assert!(AhmaMcpService::parse_file_uri_to_path("http://example.com/a").is_none());
        assert!(AhmaMcpService::parse_file_uri_to_path("file://not-abs").is_none());
        assert!(AhmaMcpService::parse_file_uri_to_path("file://localhostnotabs").is_none());
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn parse_file_uri_to_path_accepts_absolute_without_localhost() {
        let p = AhmaMcpService::parse_file_uri_to_path("file:///home/user/file.txt").expect("path");
        assert_eq!(p.to_string_lossy(), "/home/user/file.txt");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn parse_file_uri_to_path_strips_query_only() {
        let p =
            AhmaMcpService::parse_file_uri_to_path("file:///path/to/file?query=1").expect("path");
        assert_eq!(p.to_string_lossy(), "/path/to/file");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn parse_file_uri_to_path_strips_fragment_only() {
        let p =
            AhmaMcpService::parse_file_uri_to_path("file:///path/to/file#section").expect("path");
        assert_eq!(p.to_string_lossy(), "/path/to/file");
    }

    // ── parse_file_uri_to_path (Windows equivalents) ────────────────────────
    // Windows uses drive-letter URIs (file:///C:/...) instead of Unix absolute paths.

    #[test]
    #[cfg(target_os = "windows")]
    fn parse_file_uri_to_path_accepts_absolute_without_localhost() {
        let p =
            AhmaMcpService::parse_file_uri_to_path("file:///C:/home/user/file.txt").expect("path");
        assert_eq!(p.to_string_lossy(), "C:/home/user/file.txt");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn parse_file_uri_to_path_accepts_localhost_and_decodes() {
        let p = AhmaMcpService::parse_file_uri_to_path(
            "file://localhost/C:/Users/test/My%20Project/file.txt?x=1#frag",
        )
        .expect("path");
        assert_eq!(p.to_string_lossy(), "C:/Users/test/My Project/file.txt");
    }

    #[test]
    fn percent_decode_utf8_rejects_invalid_hex() {
        assert!(AhmaMcpService::percent_decode_utf8("/a%ZZ").is_none());
        assert!(AhmaMcpService::percent_decode_utf8("/a%2").is_none());
    }

    #[test]
    fn percent_decode_utf8_decodes_space() {
        let decoded = AhmaMcpService::percent_decode_utf8("/path%20to%20file").expect("decode");
        assert_eq!(decoded, "/path to file");
    }

    #[test]
    fn percent_decode_utf8_preserves_plain_text() {
        let decoded = AhmaMcpService::percent_decode_utf8("/path/to/file").expect("decode");
        assert_eq!(decoded, "/path/to/file");
    }

    #[test]
    fn percent_decode_utf8_uppercase_hex() {
        let decoded = AhmaMcpService::percent_decode_utf8("path%2Ffile").expect("decode");
        assert_eq!(decoded, "path/file");
    }

    #[test]
    fn percent_decode_utf8_truncated_percent_at_end() {
        assert!(AhmaMcpService::percent_decode_utf8("/path%").is_none());
    }

    #[test]
    fn percent_decode_utf8_invalid_utf8_returns_none() {
        // %FF decodes to byte 0xFF which is invalid as standalone UTF-8
        assert!(AhmaMcpService::percent_decode_utf8("%FF").is_none());
    }

    #[test]
    fn percent_decode_utf8_empty_string() {
        let decoded = AhmaMcpService::percent_decode_utf8("").expect("decode");
        assert_eq!(decoded, "");
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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

        let t_any = service.calculate_intelligent_timeout(&[]).await;
        assert!(t_any >= 600.0);

        let t_filtered_miss = service
            .calculate_intelligent_timeout(&["nope".to_string()])
            .await;
        assert!(t_filtered_miss >= 240.0);

        let t_filtered_hit = service
            .calculate_intelligent_timeout(&["beta".to_string()])
            .await;
        assert!(t_filtered_hit >= 600.0);
    }

    #[tokio::test]
    async fn create_tool_from_config_prepends_guidance_block() {
        let mut guidance_blocks = std::collections::HashMap::new();
        guidance_blocks.insert("my_tool".to_string(), "GUIDE".to_string());
        let guidance = GuidanceConfig {
            guidance_blocks,
            templates: std::collections::HashMap::new(),
            legacy_guidance: None,
        };

        let service = make_service_with_monitor(
            Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
                Duration::from_secs(30),
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
            Duration::from_secs(30),
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
            Duration::from_secs(30),
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
        service
            .configs
            .write()
            .unwrap()
            .insert(config.name.clone(), config);
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
        let guidance = GuidanceConfig {
            guidance_blocks,
            templates: std::collections::HashMap::new(),
            legacy_guidance: None,
        };
        let service = make_service_with_monitor(
            Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
                Duration::from_secs(30),
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

    #[tokio::test]
    async fn resolve_working_directory_explicit_and_scope() {
        let service = make_service().await;

        let explicit = obj(json!({"working_directory": "/explicit/path"}));
        assert_eq!(
            service.resolve_working_directory(&explicit),
            "/explicit/path"
        );

        // No arg -> first sandbox scope (strict test adapter has one rooted scope).
        let wd = service.resolve_working_directory(&serde_json::Map::new());
        let scope = service
            .adapter
            .sandbox()
            .scopes()
            .first()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap();
        assert_eq!(wd, scope);
    }

    #[tokio::test]
    async fn guard_and_skip_roots_defaults() {
        let service = make_service().await;
        // Strict test adapter has a rooted scope -> ready.
        assert!(service.guard_sandbox_ready_for_tool_calls().is_ok());
        // Not explicit, not test mode -> we still ask the client for roots.
        assert!(!service.should_skip_client_roots_sandbox_setup());
    }

    // ==================== extension key / registration ====================

    #[tokio::test]
    async fn get_extension_key_for_builtin_extension_types() {
        let service = make_service().await;

        let plain = cfg_from(json!({"name": "t", "description": "d", "command": "c"}));
        assert_eq!(service.get_extension_key(&plain), None);

        let tt = cfg_from(json!({
            "name": "t", "description": "d", "command": "c",
            "tool_type": "ext", "task_tree": {}
        }));
        assert_eq!(service.get_extension_key(&tt).as_deref(), Some("task_tree"));

        let dc = cfg_from(json!({
            "name": "t", "description": "d", "command": "c",
            "tool_type": "ext", "decompose": {}
        }));
        assert_eq!(service.get_extension_key(&dc).as_deref(), Some("decompose"));

        let wk = cfg_from(json!({
            "name": "t", "description": "d", "command": "c",
            "tool_type": "ext", "worker": {}
        }));
        assert_eq!(service.get_extension_key(&wk).as_deref(), Some("worker"));
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
        assert!(service.vault_audited_ops.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn emit_vault_tool_call_then_complete_writes_audit_and_tracks_ops() {
        let dir = tempdir().unwrap();
        let vault = dir.path().join("vault");
        let service = make_service().await;
        service.set_app_config(Arc::new(app_config_with_vault(vault.clone())));

        service.emit_vault_tool_call("op1", "tool", "argsum").await;
        assert!(service.vault_audited_ops.lock().unwrap().contains("op1"));

        service.emit_vault_tool_complete("op1", true, 10).await;
        assert!(!service.vault_audited_ops.lock().unwrap().contains("op1"));

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

        assert_eq!(*service.current_tools_dir.read().unwrap(), Some(tools_dir));
        assert!(service.output_optimizer.lock().await.enabled);
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
            Duration::from_secs(30),
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

        let err_result: Result<CallToolResult, McpError> =
            Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                "boom",
            )]));
        for _ in 0..3 {
            service.record_result_in_loop_detector("mytool", &args, &err_result);
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
        service.record_result_in_loop_detector("mytool", &args, &ok_result);
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
        };
        assert!(service.send_enhanced_heartbeat(payload).await.is_ok());
    }
}
