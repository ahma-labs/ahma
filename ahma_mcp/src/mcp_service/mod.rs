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
    ExtensionToolHandler, GuidanceConfig, LegacyGuidanceConfig, META_PARAMS, SequenceKind,
    register_global_extension_handler,
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
    /// Routes unified operation events to the MCP client as progress
    /// notifications (per-operation peer + progress token registration).
    pub progress_push: Arc<progress_push::ProgressPushRouter>,
    /// Operation ids whose `tool_call` was written to the vault audit log;
    /// the audit subscriber records the matching `tool_complete` on the
    /// terminal event and removes the id.
    pub vault_audited_ops: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl AhmaMcpService {
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
        if !self.adapter.sandbox().scopes().is_empty() {
            tracing::info!(
                "Sandbox scopes already configured via CLI/Env ({:?}), skipping roots/list request",
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
            harness_guard: Arc::new(tokio::sync::Mutex::new(
                crate::harness_guard::HarnessGuard::new(false),
            )),
            progress_push,
            vault_audited_ops: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
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
        if let Ok(mut guard) = self.harness_guard.try_lock() {
            guard.enabled = config.small_model_harness;
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

            let configs_lock = self.configs.read().unwrap();
            for config in configs_lock.values() {
                if !self.is_config_visible_to_client(config) {
                    continue;
                }
                tools.extend(self.create_tools_from_config(config));
            }
            drop(configs_lock);

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
        // 1. Tool name format healing
        let known = Self::HARDCODED_TOOLS.to_vec();
        let configs_lock = self.configs.read().unwrap();
        let config_names: Vec<String> = configs_lock.keys().cloned().collect();
        drop(configs_lock);
        let mut known_str: Vec<&str> = known.clone();
        for name in &config_names {
            known_str.push(name);
        }

        if let Some(healed_name) =
            crate::harness_guard::heal_tool_name(tool_name.as_ref(), &known_str)
            && healed_name.as_str() != &**tool_name
        {
            tracing::warn!("Healed tool name from '{}' to '{}'", tool_name, healed_name);
            *tool_name = std::borrow::Cow::Owned(healed_name);
        }

        // 2. Tool arguments format healing
        if let Some(map) = tool_args.as_mut() {
            crate::harness_guard::heal_tool_arguments(tool_name.as_ref(), map);
        }

        // 3. Loop detection — block the call if the same invocation has failed 3 times
        let args_str = tool_args
            .as_ref()
            .map(|v| serde_json::Value::Object(v.clone()).to_string())
            .unwrap_or_default();
        let is_loop = self
            .harness_guard
            .try_lock()
            .map(|g| g.loop_detector.is_loop(tool_name.as_ref(), &args_str))
            .unwrap_or(false);

        if is_loop {
            return Some(CallToolResult::error(vec![rmcp::model::Content::text(
                "LOOP_DETECTED: This exact call has failed 3 times. The approach is not working.\n\
                 Hint: Re-read the error messages above. Try a fundamentally different approach or read relevant documentation first.",
            )]));
        }

        None
    }

    /// Records the success or failure of a completed tool call into the loop
    /// detector so that repeated identical failures can be detected on future calls.
    fn record_result_in_loop_detector(
        &self,
        tool_name: &str,
        tool_args: &Option<serde_json::Map<String, Value>>,
        result: &Result<CallToolResult, McpError>,
    ) {
        let args_str = tool_args
            .as_ref()
            .map(|v| serde_json::Value::Object(v.clone()).to_string())
            .unwrap_or_default();
        let is_error = match result {
            Ok(res) => res.is_error.unwrap_or(false),
            Err(_) => true,
        };
        if let Ok(mut guard) = self.harness_guard.try_lock() {
            if is_error {
                guard.loop_detector.record_failure(tool_name, &args_str);
            } else {
                guard.loop_detector.record_success();
            }
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
    async fn handle_cancel_requires_id() {
        let service = make_service().await;
        let err = service
            .handle_cancel(serde_json::Map::new())
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("id parameter is required"));
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
}
