//! # Service Builder
//!
//! Provides a builder for initializing [`AhmaMcpService`] with all required
//! infrastructure components, eliminating the duplicated startup code that
//! previously existed across transport modes (stdio server, CLI, etc.).
//!
//! ## The shared initialization sequence
//!
//! All transport modes share the same init chain:
//! ```text
//! MonitorConfig → OperationMonitor
//!   → ShellPoolConfig → ShellPoolManager
//!     → Adapter
//!       → load_tool_configs
//!         → evaluate_tool_availability
//!           → AhmaMcpService
//! ```
//!
//! `ServiceBuilder` encapsulates that chain so each mode only handles what is
//! unique to it — transport binding, signal handling, output formatting, etc.

use crate::{
    adapter::Adapter,
    config::{ToolConfig, load_tool_configs},
    mcp_service::{AhmaMcpService, GuidanceConfig},
    operation_monitor::{MonitorConfig, OperationMonitor},
    sandbox::Sandbox,
    shell::cli::AppConfig,
    shell_pool::{ShellPoolConfig, ShellPoolManager},
    tool_availability::{AvailabilitySummary, evaluate_tool_availability, format_install_guidance},
};
use anyhow::{Context, Result};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// The result of a successful [`ServiceBuilder::build`] call.
///
/// Contains the fully initialised MCP service together with the infrastructure
/// components that the caller may need for lifecycle management (e.g. signal
/// handlers, graceful shutdown).
pub struct BuiltService {
    /// The ready-to-serve MCP service.
    pub service: AhmaMcpService,
    /// Shared adapter for execution and resource cleanup.
    pub adapter: Arc<Adapter>,
    /// Shared operation monitor for in-flight task tracking.
    pub operation_monitor: Arc<OperationMonitor>,
    /// How long to wait for in-flight operations during graceful shutdown.
    pub shutdown_timeout: Duration,
    /// Number of tool configurations that passed availability checks.
    pub loaded_tools_count: usize,
    /// The final tool configurations after availability filtering.
    ///
    /// Provided for callers (e.g. CLI mode) that need direct config access
    /// before going through the MCP service layer.
    pub configs: Arc<HashMap<String, ToolConfig>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Builder
// ─────────────────────────────────────────────────────────────────────────────

/// Builds an [`AhmaMcpService`] from the shared initialization sequence.
///
/// Defaults are pre-populated from the provided [`AppConfig`] so callers only
/// need to override values that differ from the config (e.g. forcing
/// `force_synchronous = true` for CLI single-shot mode).
pub struct ServiceBuilder<'a> {
    config: &'a AppConfig,
    sandbox: Arc<Sandbox>,
    guidance: Option<GuidanceConfig>,
    skip_availability_probes: bool,
    force_synchronous: bool,
    defer_sandbox: bool,
    monitor_rate_limit: u64,
    scope_grant_notifier: Option<Arc<dyn crate::sandbox::ScopeGrantNotifier>>,
    /// The same object as `scope_grant_notifier` when the caller installed a
    /// broker, kept concretely so `build` can hand it the peer once one exists.
    permission_broker: Option<Arc<crate::sandbox::PermissionBroker>>,
}

impl<'a> ServiceBuilder<'a> {
    /// Create a new builder, pre-populating options from `config`.
    pub fn new(config: &'a AppConfig, sandbox: Arc<Sandbox>) -> Self {
        Self {
            config,
            sandbox,
            guidance: Some(GuidanceConfig::default()),
            skip_availability_probes: config.skip_availability_probes,
            force_synchronous: config.force_sync,
            defer_sandbox: config.defer_sandbox,
            monitor_rate_limit: config.monitor_rate_limit_secs,
            scope_grant_notifier: None,
            permission_broker: None,
        }
    }

    /// Override the scope-grant notifier the adapter routes auto-detected
    /// violations to. When unset, a logging notifier is installed (violations are
    /// reported to the log). The server installs a hub-delivering notifier so a
    /// connected TUI can show the "grant access?" modal.
    pub fn with_scope_grant_notifier(
        mut self,
        notifier: Arc<dyn crate::sandbox::ScopeGrantNotifier>,
    ) -> Self {
        self.scope_grant_notifier = Some(notifier);
        self
    }

    /// Install the [`PermissionBroker`](crate::sandbox::PermissionBroker) as the
    /// violation surface — the full question ladder (harness → TUI → fail closed)
    /// rather than a single hard-wired surface.
    ///
    /// Takes the broker concretely, not as `dyn ScopeGrantNotifier`, because
    /// [`build`](Self::build) must hand it the MCP peer once the service exists.
    /// That ordering is forced: the adapter needs the notifier, the notifier is
    /// built before the service, and the peer only appears when a client connects.
    pub fn with_permission_broker(mut self, broker: Arc<crate::sandbox::PermissionBroker>) -> Self {
        self.scope_grant_notifier = Some(broker.clone());
        self.permission_broker = Some(broker);
        self
    }

    /// Override the guidance configuration.
    pub fn with_guidance(mut self, guidance: GuidanceConfig) -> Self {
        self.guidance = Some(guidance);
        self
    }

    /// Override whether to skip tool availability probes.
    pub fn skip_availability_probes(mut self, skip: bool) -> Self {
        self.skip_availability_probes = skip;
        self
    }

    /// Override whether to force synchronous execution for all tools.
    pub fn force_synchronous(mut self, force: bool) -> Self {
        self.force_synchronous = force;
        self
    }

    /// Override whether to defer sandbox initialisation until roots arrive.
    pub fn defer_sandbox(mut self, defer: bool) -> Self {
        self.defer_sandbox = defer;
        self
    }

    /// Override the rate-limit (in seconds) for log-monitor alert suppression.
    pub fn monitor_rate_limit(mut self, secs: u64) -> Self {
        self.monitor_rate_limit = secs;
        self
    }

    /// Run all initialization steps and return a [`BuiltService`].
    ///
    /// # Errors
    ///
    /// Returns an error if any step fails: sandbox creation, pool startup, tool
    /// config loading, availability probing, or `AhmaMcpService` construction.
    pub async fn build(self) -> Result<BuiltService> {
        let config = self.config;
        let sandbox = &self.sandbox;

        // Enable the idle-output watchdog in production: an async operation that
        // goes completely silent for this long is timed out as *stalled*, well
        // before any (much longer) total-runtime budget. Five minutes of total
        // silence is a strong wedge signal even for a quiet link/codegen step,
        // and a system suspend is forgiven separately (see `note_monitor_tick`),
        // so this will not false-fire across laptop sleep.
        let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(config.timeout_secs))
            .with_idle_timeout(Some(Duration::from_secs(300)));
        let shutdown_timeout = monitor_config.shutdown_timeout;
        let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));

        let shell_pool_config = ShellPoolConfig {
            command_timeout: Duration::from_secs(config.timeout_secs),
        };
        let shell_pool_manager = Arc::new(ShellPoolManager::new(shell_pool_config));

        let mutex_registry = Arc::new(crate::adapter::CommandMutexRegistry::from_config(
            &config.mutex_groups,
        ));

        // Auto-detect sandbox scope violations and surface a "grant access?" prompt.
        // The caller (server mode) installs a hub-delivering notifier so a connected
        // TUI can show the modal; without one we fall back to a logging notifier so
        // violations remain observable. Either way the notifier only *persists* an
        // approved grant for the next start — it never widens the live session (R5).
        let grant_notifier: Arc<dyn crate::sandbox::ScopeGrantNotifier> =
            self.scope_grant_notifier.unwrap_or_else(|| {
                Arc::new(crate::sandbox::LoggingGrantNotifier::new(Arc::new(
                    ahma_common::scope_grant::GrantCoordinator::new(),
                )))
            });
        let adapter = Arc::new(
            Adapter::new_with_registry(
                operation_monitor.clone(),
                shell_pool_manager,
                sandbox.clone(),
                mutex_registry,
            )?
            .with_scope_grant_notifier(grant_notifier),
        );

        let raw_configs = load_tool_configs(config, config.tools_dir.as_deref())
            .await
            .context("Failed to load tool configurations")?;

        let configs = if self.skip_availability_probes {
            tracing::info!("Skipping tool availability probes (AHMA_SKIP_PROBES)");
            Arc::new(raw_configs)
        } else {
            let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let availability_summary =
                evaluate_tool_availability(raw_configs, working_dir.as_path(), sandbox.as_ref())
                    .await?;

            log_availability_warnings(&availability_summary);
            Arc::new(availability_summary.filtered_configs)
        };

        log_loaded_tools(&configs, config.tools_dir.as_deref());

        let loaded_tools_count = configs.len();
        let configs_for_output = configs.clone();

        let mut service = AhmaMcpService::new(
            adapter.clone(),
            operation_monitor.clone(),
            configs,
            Arc::new(self.guidance),
            self.force_synchronous,
            self.defer_sandbox,
        )
        .await?;

        service.monitor_rate_limit_seconds = self.monitor_rate_limit;
        service.set_app_config(std::sync::Arc::new(config.clone()));

        // Install rung 1 of the question ladder (R-PERM.3). The broker is built
        // *before* the service (the adapter needs it), so it cannot capture the
        // peer at construction — the peer only exists once a client connects. It
        // shares the service's peer slot instead, and starts asking the harness the
        // moment one attaches.
        if let Some(broker) = &self.permission_broker {
            broker.set_elicitation_surface(Arc::new(crate::sandbox::PeerElicitationSurface::new(
                service.peer.clone(),
            )));
            // Session-health disclosure (#485): the broker emits
            // grant_pending/grant_decided events over the same peer slot, and
            // the keep-alive path reads the coordinator's pending count into
            // the heartbeat payload.
            broker.set_session_events(Arc::new(crate::session_events::SessionEventSender::new(
                service.peer.clone(),
            )));
            *service.grant_coordinator.write().unwrap() = Some(broker.coordinator().clone());
        }

        Ok(BuiltService {
            service,
            adapter,
            operation_monitor,
            shutdown_timeout,
            loaded_tools_count,
            configs: configs_for_output,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Private helpers
// ─────────────────────────────────────────────────────────────────────────────

fn log_availability_warnings(summary: &AvailabilitySummary) {
    if !summary.disabled_tools.is_empty() {
        let current_path = std::env::var("PATH").unwrap_or_else(|_| "<not set>".to_string());
        tracing::warn!(
            "{} tool(s) disabled by availability probes. PATH={}",
            summary.disabled_tools.len(),
            current_path
        );
        for disabled in &summary.disabled_tools {
            tracing::warn!(
                "Tool '{}' disabled at startup. {}",
                disabled.name,
                disabled.message
            );
            if let Some(instructions) = &disabled.install_instructions {
                tracing::info!(
                    "Install instructions for '{}': {}",
                    disabled.name,
                    instructions
                );
            }
        }
    }

    if !summary.disabled_subcommands.is_empty() {
        for disabled in &summary.disabled_subcommands {
            tracing::warn!(
                "Tool subcommand '{}::{}' disabled at startup. {}",
                disabled.tool,
                disabled.subcommand_path,
                disabled.message
            );
            if let Some(instructions) = &disabled.install_instructions {
                tracing::info!(
                    "Install instructions for '{}::{}': {}",
                    disabled.tool,
                    disabled.subcommand_path,
                    instructions
                );
            }
        }
    }

    if !summary.disabled_tools.is_empty() || !summary.disabled_subcommands.is_empty() {
        let install_guidance = format_install_guidance(summary);
        tracing::warn!(
            "Startup tool guidance (share with users who need to install prerequisites):\n{}",
            install_guidance
        );
    }
}

fn log_loaded_tools(configs: &HashMap<String, ToolConfig>, tools_dir: Option<&std::path::Path>) {
    if configs.is_empty() {
        tracing::error!("No valid tool configurations available after availability checks");
        if let Some(dir) = tools_dir {
            tracing::error!("Tools directory: {:?}", dir);
        } else {
            tracing::error!("No tools directory specified (using built-in internal tools only)");
        }
        // Not fatal – the service can still serve built-in (hard-wired) tools.
    } else {
        let tool_names: Vec<String> = configs.keys().cloned().collect();
        tracing::info!(
            "Loaded {} tool configurations: {}",
            configs.len(),
            tool_names.join(", ")
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::load_tool_configs_sync,
        sandbox::{LoggingGrantNotifier, Sandbox, SandboxMode},
        shell::cli::AppConfig,
        tool_availability::{AvailabilitySummary, DisabledSubcommand, DisabledTool},
    };
    use ahma_common::scope_grant::GrantCoordinator;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;

    // ─── Test helpers ────────────────────────────────────────────────────────

    /// Create a `SandboxMode::Test` sandbox scoped to `scope`.
    fn make_test_sandbox(scope: std::path::PathBuf) -> Arc<Sandbox> {
        Arc::new(
            Sandbox::new(vec![scope], SandboxMode::Test, false, false, false)
                .expect("test sandbox creation should never fail"),
        )
    }

    /// Minimal `AppConfig` for tests: probes skipped, sensible defaults.
    fn make_test_config() -> AppConfig {
        AppConfig {
            skip_availability_probes: true,
            ..AppConfig::default()
        }
    }

    // ─── ServiceBuilder::new ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_service_builder_new_builds_with_default_config() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let result = ServiceBuilder::new(&config, sandbox).build().await;

        assert!(result.is_ok(), "build should succeed: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_service_builder_new_inherits_force_sync_from_config() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = AppConfig {
            skip_availability_probes: true,
            force_sync: true,
            ..AppConfig::default()
        };

        // Builder picks up force_sync from config; build should still succeed.
        let result = ServiceBuilder::new(&config, sandbox).build().await;

        assert!(result.is_ok(), "build with force_sync=true should succeed");
    }

    #[tokio::test]
    async fn test_service_builder_new_inherits_monitor_rate_limit_from_config() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = AppConfig {
            skip_availability_probes: true,
            monitor_rate_limit_secs: 120,
            ..AppConfig::default()
        };

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        assert_eq!(built.service.monitor_rate_limit_seconds, 120);
    }

    // ─── with_guidance ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_with_guidance_default_builds_successfully() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let result = ServiceBuilder::new(&config, sandbox)
            .with_guidance(GuidanceConfig::default())
            .build()
            .await;

        assert!(
            result.is_ok(),
            "with_guidance should succeed: {:?}",
            result.err()
        );
    }

    // ─── with_scope_grant_notifier ───────────────────────────────────────────

    #[tokio::test]
    async fn test_with_scope_grant_notifier_custom_builds_successfully() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let notifier: Arc<dyn crate::sandbox::ScopeGrantNotifier> =
            Arc::new(LoggingGrantNotifier::new(Arc::new(GrantCoordinator::new())));

        let result = ServiceBuilder::new(&config, sandbox)
            .with_scope_grant_notifier(notifier)
            .build()
            .await;

        assert!(result.is_ok(), "with_scope_grant_notifier should succeed");
    }

    #[tokio::test]
    async fn test_without_scope_grant_notifier_uses_default_logging_notifier() {
        // When no notifier is provided, build falls back to LoggingGrantNotifier
        // internally. Verify the service still builds successfully.
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let result = ServiceBuilder::new(&config, sandbox).build().await;

        assert!(result.is_ok());
    }

    // ─── skip_availability_probes ────────────────────────────────────────────

    #[tokio::test]
    async fn test_skip_availability_probes_true_bypasses_probe_phase() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = AppConfig {
            skip_availability_probes: false, // start as false
            ..AppConfig::default()
        };

        // Override to skip probes explicitly.
        let result = ServiceBuilder::new(&config, sandbox)
            .skip_availability_probes(true)
            .build()
            .await;

        assert!(
            result.is_ok(),
            "skipping probes should succeed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_skip_availability_probes_false_runs_probe_phase() {
        // With the default config (no custom tools, only synthetic run_terminal_command
        // which has no availability_check), running probes is a fast no-op.
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = AppConfig {
            skip_availability_probes: true,
            ..AppConfig::default()
        };

        let result = ServiceBuilder::new(&config, sandbox)
            .skip_availability_probes(false)
            .build()
            .await;

        assert!(
            result.is_ok(),
            "probes with no-check tools should succeed: {:?}",
            result.err()
        );
    }

    // ─── force_synchronous ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_force_synchronous_true_builds_successfully() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let result = ServiceBuilder::new(&config, sandbox)
            .force_synchronous(true)
            .build()
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_force_synchronous_false_builds_successfully() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let result = ServiceBuilder::new(&config, sandbox)
            .force_synchronous(false)
            .build()
            .await;

        assert!(result.is_ok());
    }

    // ─── defer_sandbox ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_defer_sandbox_true_builds_successfully() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let result = ServiceBuilder::new(&config, sandbox)
            .defer_sandbox(true)
            .build()
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_defer_sandbox_false_builds_successfully() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let result = ServiceBuilder::new(&config, sandbox)
            .defer_sandbox(false)
            .build()
            .await;

        assert!(result.is_ok());
    }

    // ─── monitor_rate_limit ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_monitor_rate_limit_override_reflected_in_service() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox)
            .monitor_rate_limit(300)
            .build()
            .await
            .unwrap();

        assert_eq!(built.service.monitor_rate_limit_seconds, 300);
    }

    #[tokio::test]
    async fn test_monitor_rate_limit_zero_is_accepted() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox)
            .monitor_rate_limit(0)
            .build()
            .await
            .unwrap();

        assert_eq!(built.service.monitor_rate_limit_seconds, 0);
    }

    #[tokio::test]
    async fn test_monitor_rate_limit_large_value_is_accepted() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox)
            .monitor_rate_limit(u64::MAX)
            .build()
            .await
            .unwrap();

        assert_eq!(built.service.monitor_rate_limit_seconds, u64::MAX);
    }

    // ─── BuiltService fields ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_built_service_loaded_tools_count_matches_configs_len() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        assert_eq!(built.loaded_tools_count, built.configs.len());
    }

    #[tokio::test]
    async fn test_built_service_configs_contains_run_terminal_command() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        assert!(
            built.configs.contains_key("run_terminal_command"),
            "configs must include the synthetic run_terminal_command tool"
        );
    }

    #[tokio::test]
    async fn test_built_service_shutdown_timeout_is_positive() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        assert!(
            built.shutdown_timeout > Duration::ZERO,
            "shutdown_timeout must be positive, got {:?}",
            built.shutdown_timeout
        );
    }

    #[tokio::test]
    async fn test_built_service_adapter_sandbox_is_accessible() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        // Verify the adapter's sandbox is reachable (no panic).
        let _sandbox_ref = built.adapter.sandbox();
    }

    #[tokio::test]
    async fn test_built_service_arcs_have_positive_strong_count() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        assert!(Arc::strong_count(&built.operation_monitor) >= 1);
        assert!(Arc::strong_count(&built.configs) >= 1);
        assert!(Arc::strong_count(&built.adapter) >= 1);
    }

    // ─── tools_dir loading ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_build_with_tools_dir_loads_custom_tool() {
        let temp = tempdir().unwrap();
        let tools_dir = temp.path().join(".ahma");
        std::fs::create_dir_all(&tools_dir).unwrap();
        // NOTE: CommandOption uses #[serde(rename = "type")] so the JSON key must be "type".
        std::fs::write(
            tools_dir.join("echo.json"),
            r#"{
                "name": "echo",
                "description": "Echo a message",
                "command": "echo",
                "timeout_seconds": 10,
                "enabled": true,
                "subcommand": [
                    {
                        "name": "default",
                        "description": "echo the message",
                        "positional_args": [
                            {
                                "name": "message",
                                "type": "string",
                                "description": "message to echo",
                                "required": true
                            }
                        ]
                    }
                ]
            }"#,
        )
        .unwrap();

        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = AppConfig {
            skip_availability_probes: true,
            tools_dir: Some(tools_dir),
            ..AppConfig::default()
        };

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        assert!(
            built.configs.contains_key("echo"),
            "custom echo tool should be loaded from tools_dir"
        );
        // At minimum: echo + run_terminal_command
        assert!(built.loaded_tools_count >= 2);
    }

    #[tokio::test]
    async fn test_build_with_empty_tools_dir_yields_only_synthetic_tools() {
        let temp = tempdir().unwrap();
        let tools_dir = temp.path().join(".ahma");
        std::fs::create_dir_all(&tools_dir).unwrap(); // empty dir

        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = AppConfig {
            skip_availability_probes: true,
            tools_dir: Some(tools_dir),
            ..AppConfig::default()
        };

        let built = ServiceBuilder::new(&config, sandbox).build().await.unwrap();

        // Only run_terminal_command (synthetic) should be present.
        assert_eq!(built.configs.len(), 1);
        assert!(built.configs.contains_key("run_terminal_command"));
    }

    #[tokio::test]
    async fn test_build_with_tools_dir_and_probes_enabled() {
        // echo has no availability_check, so no actual shell probes are run.
        let temp = tempdir().unwrap();
        let tools_dir = temp.path().join(".ahma");
        std::fs::create_dir_all(&tools_dir).unwrap();
        // NOTE: CommandOption uses #[serde(rename = "type")] so JSON key must be "type".
        std::fs::write(
            tools_dir.join("echo.json"),
            r#"{
                "name": "echo",
                "description": "Echo a message",
                "command": "echo",
                "timeout_seconds": 10,
                "enabled": true,
                "subcommand": [
                    {
                        "name": "default",
                        "description": "echo the message",
                        "positional_args": [
                            {
                                "name": "message",
                                "type": "string",
                                "description": "message to echo",
                                "required": true
                            }
                        ]
                    }
                ]
            }"#,
        )
        .unwrap();

        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = AppConfig {
            skip_availability_probes: false,
            tools_dir: Some(tools_dir),
            ..AppConfig::default()
        };

        let result = ServiceBuilder::new(&config, sandbox)
            .skip_availability_probes(false)
            .build()
            .await;

        assert!(
            result.is_ok(),
            "build with probes and echo tool should succeed: {:?}",
            result.err()
        );
    }

    // ─── Chained builder ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_service_builder_all_methods_chained_builds_successfully() {
        let temp = tempdir().unwrap();
        let sandbox = make_test_sandbox(temp.path().to_path_buf());
        let config = make_test_config();

        let notifier: Arc<dyn crate::sandbox::ScopeGrantNotifier> =
            Arc::new(LoggingGrantNotifier::new(Arc::new(GrantCoordinator::new())));

        let result = ServiceBuilder::new(&config, sandbox)
            .with_scope_grant_notifier(notifier)
            .with_guidance(GuidanceConfig::default())
            .skip_availability_probes(true)
            .force_synchronous(true)
            .defer_sandbox(false)
            .monitor_rate_limit(42)
            .build()
            .await;

        assert!(result.is_ok(), "all-methods-chained build should succeed");
        let built = result.unwrap();
        assert_eq!(built.service.monitor_rate_limit_seconds, 42);
    }

    // ─── Private helper: log_availability_warnings ───────────────────────────

    #[test]
    fn test_log_availability_warnings_empty_summary_is_noop() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![],
            disabled_subcommands: vec![],
        };
        // Must not panic.
        log_availability_warnings(&summary);
    }

    #[test]
    fn test_log_availability_warnings_disabled_tool_without_instructions() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![DisabledTool {
                name: "cargo".to_string(),
                message: "command not found".to_string(),
                install_instructions: None,
            }],
            disabled_subcommands: vec![],
        };
        log_availability_warnings(&summary);
    }

    #[test]
    fn test_log_availability_warnings_disabled_tool_with_instructions() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![DisabledTool {
                name: "rustfmt".to_string(),
                message: "not installed".to_string(),
                install_instructions: Some("rustup component add rustfmt".to_string()),
            }],
            disabled_subcommands: vec![],
        };
        log_availability_warnings(&summary);
    }

    #[test]
    fn test_log_availability_warnings_disabled_subcommand_without_instructions() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![],
            disabled_subcommands: vec![DisabledSubcommand {
                tool: "git".to_string(),
                subcommand_path: "commit".to_string(),
                message: "git not found".to_string(),
                install_instructions: None,
            }],
        };
        log_availability_warnings(&summary);
    }

    #[test]
    fn test_log_availability_warnings_disabled_subcommand_with_instructions() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![],
            disabled_subcommands: vec![DisabledSubcommand {
                tool: "gh".to_string(),
                subcommand_path: "pr create".to_string(),
                message: "gh not installed".to_string(),
                install_instructions: Some("brew install gh".to_string()),
            }],
        };
        log_availability_warnings(&summary);
    }

    #[test]
    fn test_log_availability_warnings_both_disabled_tools_and_subcommands() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![DisabledTool {
                name: "cargo".to_string(),
                message: "not found".to_string(),
                install_instructions: Some("Install Rust via rustup".to_string()),
            }],
            disabled_subcommands: vec![DisabledSubcommand {
                tool: "cargo".to_string(),
                subcommand_path: "fmt".to_string(),
                message: "rustfmt missing".to_string(),
                install_instructions: Some("rustup component add rustfmt".to_string()),
            }],
        };
        log_availability_warnings(&summary);
    }

    #[test]
    fn test_log_availability_warnings_multiple_disabled_tools() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![
                DisabledTool {
                    name: "tool_a".to_string(),
                    message: "missing".to_string(),
                    install_instructions: None,
                },
                DisabledTool {
                    name: "tool_b".to_string(),
                    message: "also missing".to_string(),
                    install_instructions: Some("install_b".to_string()),
                },
            ],
            disabled_subcommands: vec![],
        };
        log_availability_warnings(&summary);
    }

    #[test]
    fn test_log_availability_warnings_multiple_disabled_subcommands() {
        let summary = AvailabilitySummary {
            filtered_configs: HashMap::new(),
            disabled_tools: vec![],
            disabled_subcommands: vec![
                DisabledSubcommand {
                    tool: "tool_x".to_string(),
                    subcommand_path: "sub1".to_string(),
                    message: "sub1 missing".to_string(),
                    install_instructions: None,
                },
                DisabledSubcommand {
                    tool: "tool_x".to_string(),
                    subcommand_path: "sub2".to_string(),
                    message: "sub2 missing".to_string(),
                    install_instructions: Some("install instructions".to_string()),
                },
            ],
        };
        log_availability_warnings(&summary);
    }

    // ─── Private helper: log_loaded_tools ────────────────────────────────────

    #[test]
    fn test_log_loaded_tools_empty_configs_no_dir_logs_error() {
        let configs: HashMap<String, ToolConfig> = HashMap::new();
        // Must not panic; logs an error-level message.
        log_loaded_tools(&configs, None);
    }

    #[test]
    fn test_log_loaded_tools_empty_configs_with_dir_logs_error_and_dir() {
        let temp = tempdir().unwrap();
        let configs: HashMap<String, ToolConfig> = HashMap::new();
        log_loaded_tools(&configs, Some(temp.path()));
    }

    #[test]
    fn test_log_loaded_tools_nonempty_configs_logs_info() {
        let configs = load_tool_configs_sync(&AppConfig::default(), None)
            .expect("load_tool_configs_sync should succeed with default config");
        assert!(
            !configs.is_empty(),
            "should include at least run_terminal_command"
        );
        log_loaded_tools(&configs, None);
    }

    #[test]
    fn test_log_loaded_tools_nonempty_configs_with_dir() {
        let temp = tempdir().unwrap();
        let configs = load_tool_configs_sync(&AppConfig::default(), None).unwrap();
        log_loaded_tools(&configs, Some(temp.path()));
    }
}
