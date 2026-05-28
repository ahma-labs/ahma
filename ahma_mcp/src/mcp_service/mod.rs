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
pub mod schema;
mod sequence;
mod subcommand;
mod types;
mod utils;

pub use types::{GuidanceConfig, LegacyGuidanceConfig, META_PARAMS, SequenceKind};

use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, ErrorData as McpError,
        Implementation, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ServerCapabilities, ServerInfo, Tool, ToolsCapability,
    },
    service::{NotificationContext, Peer, RequestContext, RoleServer},
};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};
use tracing;
use tracing::Instrument as _;

use crate::{
    adapter::Adapter, callback_system::CallbackSender, client_type::McpClientType,
    config::ToolConfig, mcp_callback::McpCallbackSender,
};

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
    /// When true, only built-in tools and `activate_tools` are shown initially.
    /// Bundled tools are revealed on demand via `activate_tools reveal <bundle>`.
    pub progressive_disclosure: bool,
    /// Set of bundle names whose tools have been disclosed to the client.
    pub disclosed_bundles: Arc<RwLock<HashSet<String>>>,
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
}

impl AhmaMcpService {
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
        progressive_disclosure: bool,
    ) -> Result<Self, anyhow::Error> {
        // Start the background monitor for operation timeouts
        crate::operation_monitor::OperationMonitor::start_background_monitor(
            operation_monitor.clone(),
        );

        Ok(Self {
            adapter,
            operation_monitor,
            configs: Arc::new(RwLock::new((*configs).clone())),
            guidance,
            force_synchronous,
            defer_sandbox,
            peer: Arc::new(RwLock::new(None)),
            monitor_rate_limit_seconds: crate::log_monitor::DEFAULT_RATE_LIMIT_SECONDS,
            progressive_disclosure,
            disclosed_bundles: Arc::new(RwLock::new(HashSet::new())),
            app_config: Arc::new(RwLock::new(None)),
            current_tools_dir: Arc::new(RwLock::new(None)),
        })
    }

    /// Store the AppConfig that constructed this service so runtime events
    /// (such as `roots/list` arrival) can rediscover per-client `.ahma/` dirs.
    pub fn set_app_config(&self, config: Arc<crate::shell::cli::AppConfig>) {
        if let Some(dir) = config.tools_dir.clone() {
            *self.current_tools_dir.write().unwrap() = Some(dir);
        }
        *self.app_config.write().unwrap() = Some(config);
    }

    /// Pre-discloses the given bundle names so their tools appear in the first
    /// `tools/list` response without requiring an `activate_tools reveal` call.
    ///
    /// Used for bundles explicitly requested via CLI flags (e.g. `--rust`).
    pub fn pre_disclose(&self, bundles: &std::collections::HashSet<String>) {
        if bundles.is_empty() {
            return;
        }
        let mut disclosed = self.disclosed_bundles.write().unwrap();
        for name in bundles {
            disclosed.insert(name.clone());
        }
        tracing::info!(
            "Auto-revealed CLI-flagged bundles: {}",
            bundles.iter().cloned().collect::<Vec<_>>().join(", ")
        );
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

    fn current_peer(&self) -> Option<Peer<RoleServer>> {
        self.peer.read().unwrap().clone()
    }

    /// Sends a `notifications/tools/list_changed` notification to the connected client.
    ///
    /// Called after bundle disclosure state changes (e.g., via `activate_tools reveal`).
    pub async fn notify_tools_changed(&self) {
        let Some(peer) = self.current_peer() else {
            tracing::debug!("No peer connected, skipping tools/list_changed notification");
            return;
        };

        if let Err(e) = peer.notify_tool_list_changed().await {
            tracing::error!("Failed to send tools/list_changed notification: {}", e);
        } else {
            tracing::info!("Sent tools/list_changed notification after bundle reveal");
        }
    }

    /// Returns true if the given tool config name belongs to a known bundle.
    fn is_bundle_tool(&self, config_name: &str) -> bool {
        bundle_registry::BUNDLES
            .iter()
            .any(|b| b.config_tool_name == config_name)
    }

    /// Returns true if the tool's parent bundle has been disclosed.
    fn is_tool_disclosed(&self, config_name: &str, disclosed: &HashSet<String>) -> bool {
        bundle_registry::BUNDLES
            .iter()
            .find(|b| b.config_tool_name == config_name)
            .is_some_and(|b| disclosed.contains(b.name))
    }

    /// Names that are always hard-wired in the protocol layer and must not
    /// appear in user/bundled configs (we skip duplicates here).
    const HARDCODED_TOOLS: &'static [&'static str] = &[
        "await",
        "status",
        "run_terminal_command",
        "cancel",
        "activate_tools",
        "logs_list",
        "logs_read",
        "logs_search",
    ];

    /// Returns a snapshot of the disclosed-bundle set when progressive
    /// disclosure is on, or `None` when every loaded tool is visible.
    fn disclosed_snapshot(&self) -> Option<HashSet<String>> {
        if self.progressive_disclosure {
            Some(self.disclosed_bundles.read().unwrap().clone())
        } else {
            None
        }
    }

    /// Returns true if a configured tool should be exposed to the client
    /// given the current disclosure state. Centralises the filter so
    /// `list_tools()` and `list_tool_names()` cannot drift apart.
    fn is_config_visible_to_client(
        &self,
        config: &ToolConfig,
        disclosed: &Option<HashSet<String>>,
    ) -> bool {
        if Self::HARDCODED_TOOLS.contains(&config.name.as_str()) {
            return false;
        }
        if !config.enabled {
            tracing::debug!("Skipping disabled tool '{}'", config.name);
            return false;
        }
        if let Some(set) = disclosed
            && self.is_bundle_tool(&config.name)
            && !self.is_tool_disclosed(&config.name, set)
        {
            tracing::debug!("Skipping undisclosed bundle tool '{}'", config.name);
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

    /// Generates a rich, action-oriented description for the `activate_tools` meta-tool.
    ///
    /// The description dynamically lists all loaded bundles with their `ai_hint` text,
    /// giving the AI immediate awareness of what capabilities are available and when
    /// to activate each one.
    fn generate_activate_tools_description(&self) -> String {
        let config_keys: std::collections::HashSet<String> = {
            let configs_lock = self.configs.read().unwrap();
            configs_lock.keys().cloned().collect()
        };

        let loaded = bundle_registry::loaded_bundle_names(&config_keys);

        if loaded.is_empty() {
            return "Discover and activate tool bundles. Call with action 'list' to see available bundles and their status, or 'reveal' with a bundle name to activate its tools. Bundles are revealed progressively to minimize context usage.".to_string();
        }

        let mut parts = Vec::new();
        parts.push(
            "Activate additional tool bundles to extend available capabilities. Available bundles:"
                .to_string(),
        );

        for bundle in &loaded {
            parts.push(format!("- '{}': {}", bundle.name, bundle.ai_hint));
        }

        parts.push(
            "Call with action 'list' for details, or 'reveal' with a bundle name to activate."
                .to_string(),
        );

        parts.join("\n")
    }

    fn sync_override_from_config(
        subcommand_config: &crate::config::SubcommandConfig,
        tool_config: &ToolConfig,
    ) -> Option<bool> {
        subcommand_config.synchronous.or(tool_config.synchronous)
    }

    fn execution_mode_from_preferences(
        sync_override: Option<bool>,
        force_synchronous: bool,
        explicit_mode_str: Option<&str>,
    ) -> crate::adapter::ExecutionMode {
        match sync_override {
            Some(true) => crate::adapter::ExecutionMode::Synchronous,
            Some(false) => crate::adapter::ExecutionMode::AsyncResultPush,
            None if force_synchronous || explicit_mode_str == Some("Synchronous") => {
                crate::adapter::ExecutionMode::Synchronous
            }
            None => crate::adapter::ExecutionMode::AsyncResultPush,
        }
    }

    fn determine_execution_mode(
        &self,
        subcommand_config: &crate::config::SubcommandConfig,
        tool_config: &ToolConfig,
        explicit_mode_str: Option<&str>,
    ) -> crate::adapter::ExecutionMode {
        let sync_override = Self::sync_override_from_config(subcommand_config, tool_config);
        Self::execution_mode_from_preferences(
            sync_override,
            self.force_synchronous,
            explicit_mode_str,
        )
    }

    fn sync_tool_progress_description(base_command: &str, working_directory: &str) -> String {
        format!("Execute {} in {}", base_command, working_directory)
    }

    fn sync_started_progress_update(
        id: &str,
        base_command: &str,
        working_directory: &str,
    ) -> crate::callback_system::ProgressUpdate {
        crate::callback_system::ProgressUpdate::Started {
            id: id.to_string(),
            command: base_command.to_string(),
            description: Self::sync_tool_progress_description(base_command, working_directory),
        }
    }

    fn sync_final_progress_update<E: std::fmt::Display>(
        id: &str,
        base_command: &str,
        working_directory: &str,
        result: &Result<String, E>,
    ) -> crate::callback_system::ProgressUpdate {
        let description = Self::sync_tool_progress_description(base_command, working_directory);
        let working_directory = working_directory.to_string();

        match result {
            Ok(output) => crate::callback_system::ProgressUpdate::FinalResult {
                id: id.to_string(),
                command: base_command.to_string(),
                description,
                working_directory,
                success: true,
                duration_ms: 0,
                full_output: output.clone(),
            },
            Err(e) => crate::callback_system::ProgressUpdate::FinalResult {
                id: id.to_string(),
                command: base_command.to_string(),
                description,
                working_directory,
                success: false,
                duration_ms: 0,
                full_output: format!("Error: {}", e),
            },
        }
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
        if let Some(token) = progress_token.clone() {
            let callback =
                McpCallbackSender::new(peer.clone(), id.clone(), Some(token), client_type);
            let _ = callback
                .send_progress(Self::sync_started_progress_update(
                    &id,
                    base_command,
                    working_directory,
                ))
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

        if let Some(token) = progress_token {
            let callback = McpCallbackSender::new(peer, id.clone(), Some(token), client_type);
            let final_update =
                Self::sync_final_progress_update(&id, base_command, working_directory, &result);
            let _ = callback.send_progress(final_update).await;
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
        let callback: Option<Box<dyn CallbackSender>> = progress_token.map(|token| {
            Box::new(McpCallbackSender::new(
                peer,
                id.clone(),
                Some(token),
                client_type,
            )) as Box<dyn CallbackSender>
        });

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
                    id: Some(id),
                    args: Some(arguments),
                    timeout,
                    callback,
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
                let error_message = format!("Failed to start asynchronous operation: {}", e);
                tracing::error!("{}", error_message);
                Err(handlers::common::mcp_internal(error_message))
            }
        }
    }
}

#[async_trait::async_trait]
#[expect(
    clippy::manual_async_fn,
    reason = "async-trait desugars to manual Future returns; required by rmcp ServerHandler trait contract"
)]
impl ServerHandler for AhmaMcpService {
    fn get_info(&self) -> ServerInfo {
        let base_instructions = "Ahma exposes shell, build, test, and log-monitoring tools that run inside a \
                  kernel-enforced workspace sandbox (Landlock on Linux, Seatbelt on macOS, \
                  Job Objects on Windows). Prefer `run_terminal_command` over the native terminal when: \
                  (1) the command writes to disk — the sandbox guarantees the write stays inside the workspace; \
                  (2) the command is long-running — `run_terminal_command` returns an operation_id immediately \
                  and you can `status`, `await`, or `cancel` it without blocking; \
                  (3) the command's output should be watched for errors — set `monitor_level` and ahma \
                  streams alerts when matching lines appear; \
                  (4) multiple commands should run concurrently — each call gets its own operation_id. \
                  For read-only file inspection (read, grep, glob, replace) keep using the IDE's native \
                  file tools — that is what they are for.";

        let instructions = if self.progressive_disclosure {
            format!(
                "{base_instructions} \
                  Bundles for cargo, git, python, kotlin, github, fileutils, and simplify are revealed \
                  on demand via `activate_tools` (action `list` then `reveal`)."
            )
        } else {
            base_instructions.to_string()
        };

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

            // Get the peer from the context
            let peer = &context.peer;
            if self.peer.read().unwrap().is_none() {
                let mut peer_guard = self.peer.write().unwrap();
                if peer_guard.is_none() {
                    *peer_guard = Some(peer.clone());
                    tracing::info!(
                        "Successfully captured MCP peer handle for async notifications."
                    );
                }
            }

            // Query client for workspace roots and configure sandbox
            // Per MCP spec, server sends roots/list request to client
            // IMPORTANT: Only do this if sandbox is NOT deferred.
            // In HTTP bridge mode with --defer-sandbox, we wait for roots/list_changed
            // notification which is sent by the bridge when SSE connects.
            if !self.defer_sandbox {
                // IF scopes are already configured (e.g. via CLI --sandbox-scope), respect them
                // and do not ask client for roots (which would overwrite CLI scopes).
                // This also prevents hangs when testing with clients that don't support roots/list.
                if !self.adapter.sandbox().scopes().is_empty() {
                    tracing::info!(
                        "Sandbox scopes already configured via CLI/Env ({:?}), skipping roots/list request",
                        self.adapter.sandbox().scopes()
                    );
                } else if self.adapter.sandbox().is_test_mode() {
                    // In test/disabled sandbox mode, skip the roots/list exchange.
                    // Calling configure_sandbox_from_roots in test mode causes two issues:
                    // 1) It blocks the event loop waiting for roots/list from a client that
                    //    may not implement the handler (causing a 60s handshake timeout).
                    // 2) On failure it calls emit_stdout_notification(), which writes raw
                    //    bytes to the rmcp stdio pipe, corrupting the JSON-RPC framing
                    //    and causing the client to hang on subsequent tool calls.
                    // In test mode path validation is bypassed anyway, so this is safe.
                    tracing::debug!(
                        "Sandbox in test/disabled mode: skipping roots/list request. \
                         Path validation bypassed for all paths."
                    );
                } else {
                    // Run synchronously per R19.3 - sandbox configuration is a lifecycle
                    // operation that should complete before we're "ready"
                    self.configure_sandbox_from_roots(peer).await;
                }
            } else {
                tracing::info!("Sandbox deferred - waiting for roots/list_changed notification");
            }
        }
    }

    fn on_roots_list_changed(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
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

            // Filter for operations that are actually background processes
            // vs. synchronous MCP tools like 'await' that don't have processes
            let background_ops: Vec<_> = active_ops
                .iter()
                .filter(|op| {
                    // Only cancel operations that represent actual background processes.
                    // Exclude synchronous / meta tools that never create OperationMonitor
                    // entries — cancelling them would incorrectly kill the most-recent
                    // background process instead.
                    let is_sync_meta = matches!(
                        op.tool_name.as_str(),
                        "await"
                            | "status"
                            | "cancel"
                            | "activate_tools"
                            | "logs_list"
                            | "logs_read"
                            | "logs_search"
                    );
                    if is_sync_meta {
                        tracing::debug!(
                            "on_cancelled: skipping sync/meta tool '{}' (op {})",
                            op.tool_name,
                            op.id
                        );
                    }
                    !is_sync_meta
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
                    "List all log files in the project log directory (`./log/`). Returns file names, sizes, modification times, and symlink targets. Use this to discover which log files are available before calling logs_read or logs_search.",
                    handlers::log_tools::logs_list_schema(),
                )
                .with_title("logs_list"),
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
            ];

            // When progressive disclosure is enabled, expose the activate_tools meta-tool
            // with a dynamically generated description listing all loaded bundles
            if self.progressive_disclosure {
                let description = self.generate_activate_tools_description();
                tools.push(
                    Tool::new(
                        "activate_tools",
                        description,
                        self.generate_input_schema_for_discover_tools(),
                    )
                    .with_title("activate_tools"),
                );
            }

            let disclosed = self.disclosed_snapshot();
            let configs_lock = self.configs.read().unwrap();
            for config in configs_lock.values() {
                if !self.is_config_visible_to_client(config, &disclosed) {
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
        let span = tracing::info_span!("call_tool", tool = params.name.as_ref());
        async move {
            match params.name.as_ref() {
                "status" => {
                    self.handle_status(params.arguments.unwrap_or_default())
                        .await
                }
                "await" => self.handle_await(params).await,
                "run_terminal_command" => self.handle_run_terminal_command(params, context).await,
                "cancel" => {
                    self.handle_cancel(params.arguments.unwrap_or_default())
                        .await
                }
                "activate_tools" => {
                    self.handle_discover_tools(params.arguments.unwrap_or_default())
                        .await
                }
                "logs_list" => {
                    self.handle_logs_list(params.arguments.unwrap_or_default())
                        .await
                }
                "logs_read" => {
                    self.handle_logs_read(params.arguments.unwrap_or_default())
                        .await
                }
                "logs_search" => {
                    self.handle_logs_search(params.arguments.unwrap_or_default())
                        .await
                }
                _ => self.dispatch_configured_tool(params, context).await,
            }
        }
        .instrument(span)
    }
}

impl AhmaMcpService {
    /// Routes a `tools/call` for a configured (non-built-in) tool. Validates
    /// the sandbox is locked, resolves the tool config, and dispatches by
    /// tool type (sequence / livelog / subcommand).
    async fn dispatch_configured_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool_name = params.name.as_ref();

        // Delay tool execution until sandbox is initialized from roots/list.
        // This is critical in HTTP bridge mode with deferred sandbox initialization.
        if !self.adapter.sandbox().is_ready_for_tool_calls() {
            let error_message = "Sandbox initializing from client roots - retry tools/call after roots/list completes".to_string();
            tracing::warn!("{}", error_message);
            return Err(handlers::common::mcp_internal(error_message));
        }

        // Resolve the tool name (handling flattened `parent_sub` subcommand names).
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

        if config.sequence.is_some() {
            return sequence::handle_sequence_tool(
                &self.adapter,
                &self.operation_monitor,
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

    /// Resolves the subcommand from arguments, then dispatches either as a
    /// subcommand sequence or a regular sync/async execution.
    async fn dispatch_subcommand_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
        config: ToolConfig,
        flattened_subcommand: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let tool_name = params.name.as_ref().to_string();
        let mut arguments = params.arguments.clone().unwrap_or_default();
        let subcommand_name = flattened_subcommand.or_else(|| {
            arguments
                .remove("subcommand")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        });

        let (subcommand_config, command_parts) =
            match subcommand::find_subcommand_config_from_args(&config, subcommand_name.clone()) {
                Some(result) => result,
                None => {
                    return Err(Self::subcommand_not_found_error(
                        &tool_name,
                        &config,
                        subcommand_name,
                    ));
                }
            };

        if subcommand_config.sequence.is_some() {
            return sequence::handle_subcommand_sequence(
                &self.adapter,
                &config,
                subcommand_config,
                params,
                context,
            )
            .await;
        }

        let base_command = command_parts.join(" ");
        let working_directory = self.resolve_working_directory(&arguments);
        let timeout = arguments.get("timeout_seconds").and_then(|v| v.as_u64());
        let execution_mode = self.determine_execution_mode(
            subcommand_config,
            &config,
            arguments.get("execution_mode").and_then(|v| v.as_str()),
        );

        let id = format!("op_{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
        let progress_token = context.meta.get_progress_token();
        let client_type = McpClientType::from_peer(&context.peer);

        match execution_mode {
            crate::adapter::ExecutionMode::Synchronous => {
                self.call_sync_tool(
                    id,
                    &base_command,
                    &working_directory,
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
                    &tool_name,
                    id,
                    &base_command,
                    &working_directory,
                    arguments,
                    timeout,
                    subcommand_config,
                    &config,
                    progress_token,
                    client_type,
                    context.peer.clone(),
                )
                .await
            }
        }
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
        let callback: Option<Box<dyn CallbackSender>> = progress_token.map(|token| {
            Box::new(McpCallbackSender::new(
                context.peer.clone(),
                op_id.clone(),
                Some(token),
                client_type,
            )) as Box<dyn CallbackSender>
        });
        match handlers::livelog_tool::handle_livelog_start(
            op_id.clone(),
            config,
            &params_map,
            self.operation_monitor.clone(),
            self.adapter.sandbox_arc(),
            callback,
        )
        .await
        {
            Ok(started_id) => Ok(handlers::common::text_result(format!(
                "Live log monitoring started. Operation ID: {started_id}\n\
                 Use `status` or `await` to check progress, `cancel` to stop."
            ))),
            Err(e) => {
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

        if self.progressive_disclosure {
            names.push("activate_tools".into());
        }

        let disclosed = self.disclosed_snapshot();
        let configs_lock = self.configs.read().unwrap();
        for config in configs_lock.values() {
            if self.is_config_visible_to_client(config, &disclosed) {
                names.push(config.name.clone());
            }
        }
        names
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
        AhmaMcpService::new(adapter, monitor, configs, guidance, false, false, false)
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
                name: "default".to_string(),
                description: "d".to_string(),
                subcommand: None,
                options: None,
                positional_args: None,
                positional_args_first: None,
                timeout_seconds: None,
                synchronous: None,
                enabled: true,
                guidance_key: None,
                sequence: None,
                step_delay_ms: None,
                availability_check: None,
                install_instructions: None,
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

    // ============= is_bundle_tool / is_tool_disclosed tests =============

    #[tokio::test]
    async fn test_is_bundle_tool_known() {
        let service = make_service().await;
        // "cargo" is a known bundle config_tool_name
        assert!(service.is_bundle_tool("cargo"));
    }

    #[tokio::test]
    async fn test_is_bundle_tool_unknown() {
        let service = make_service().await;
        assert!(!service.is_bundle_tool("nonexistent_tool"));
    }

    #[tokio::test]
    async fn test_is_tool_disclosed_not_disclosed() {
        let service = make_service().await;
        let disclosed = HashSet::new();
        assert!(!service.is_tool_disclosed("cargo", &disclosed));
    }

    #[tokio::test]
    async fn test_is_tool_disclosed_after_disclosure() {
        let service = make_service().await;
        let mut disclosed = HashSet::new();
        // Bundle name for "cargo" config_tool_name is "rust"
        disclosed.insert("rust".to_string());
        assert!(service.is_tool_disclosed("cargo", &disclosed));
    }

    // ============= pre_disclose tests =============

    #[tokio::test]
    async fn test_pre_disclose_empty() {
        let service = make_service().await;
        let empty = HashSet::new();
        service.pre_disclose(&empty);
        // Should not panic and disclosed set should remain empty
        let disclosed = service.disclosed_bundles.read().unwrap();
        assert!(disclosed.is_empty());
    }

    #[tokio::test]
    async fn test_pre_disclose_adds_bundles() {
        let service = make_service().await;
        let mut bundles = HashSet::new();
        bundles.insert("cargo".to_string());
        bundles.insert("git".to_string());
        service.pre_disclose(&bundles);
        let disclosed = service.disclosed_bundles.read().unwrap();
        assert!(disclosed.contains("cargo"));
        assert!(disclosed.contains("git"));
    }

    // ============= list_tool_names tests =============

    #[tokio::test]
    async fn test_list_tool_names_includes_hardcoded() {
        let service = make_service().await;
        let names = service.list_tool_names();
        assert!(names.contains(&"await".to_string()));
        assert!(names.contains(&"status".to_string()));
        assert!(names.contains(&"run_terminal_command".to_string()));
    }

    // ============= generate_activate_tools_description tests =============

    #[tokio::test]
    async fn test_generate_activate_tools_description_no_bundles_loaded() {
        let service = make_service().await;
        let desc = service.generate_activate_tools_description();
        // With empty configs, no bundles are loaded; should return default description
        assert!(desc.contains("Discover and activate"));
    }

    // ============= get_info tests =============

    #[tokio::test]
    async fn test_get_info_returns_server_info() {
        let service = make_service().await;
        let info = service.get_info();
        assert_eq!(info.server_info.name, env!("CARGO_PKG_NAME"));
    }
}
