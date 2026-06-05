//! # Server Mode
//!
//! Runs the ahma_mcp server in stdio mode, which is the default mode for MCP integration.

use crate::shell::cli::AppConfig;
use crate::{
    config::ServerConfig as MpcServerConfig,
    sandbox,
    service_builder::{BuiltService, ServiceBuilder},
    utils::stdio::emit_stdout_notification,
};
use ahma_http_mcp_client::client::HttpMcpTransport;
use anyhow::{Context, Result};
use rmcp::ServiceExt;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, signal};
use tracing::info;

/// Try to wire up an HTTP MCP client proxy if `mcp.json` specifies one.
/// Missing or non-ahma configs (e.g. Cursor/VS Code) are silently ignored.
async fn try_setup_mcp_client(config: &AppConfig) -> Result<()> {
    if !fs::try_exists(&config.mcp_config).await.unwrap_or(false) {
        return Ok(());
    }
    match crate::config::load_mcp_config(&config.mcp_config).await {
        Ok(mcp_config) => {
            if let Some(server_config) = mcp_config.servers.values().next()
                && let MpcServerConfig::Http(http_config) = server_config
            {
                tracing::info!("Initializing HTTP MCP Client for: {}", http_config.url);
                let url =
                    url::Url::parse(&http_config.url).context("Failed to parse MCP server URL")?;
                let transport = HttpMcpTransport::new(
                    url,
                    http_config.atlassian_client_id.clone(),
                    http_config.atlassian_client_secret.clone(),
                )?;
                transport.ensure_authenticated().await?;
                tracing::info!("Successfully connected to HTTP MCP server");
                tracing::warn!(
                    "Remote tools are not yet proxied to the client - this is a partial integration"
                );
                // Keep the transport alive for the duration of the process
                Box::leak(Box::new(transport));
            }
        }
        Err(e) => {
            tracing::debug!(
                "Could not parse mcp.json as ahma_mcp config (this is OK if it's a Cursor/VSCode MCP config): {}",
                e
            );
        }
    }
    Ok(())
}

fn emit_sandbox_terminated(reason: &str) {
    if let Ok(notification) = serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/sandbox/terminated",
        "params": { "reason": reason }
    })) {
        let _ = emit_stdout_notification(&notification);
    }
}

fn sandbox_mode_name(sandbox: &sandbox::Sandbox) -> &'static str {
    if sandbox.is_test_mode() {
        "DISABLED/TEST"
    } else if cfg!(target_os = "linux") {
        "LANDLOCK"
    } else if cfg!(target_os = "macos") {
        "SEATBELT"
    } else {
        "UNSUPPORTED"
    }
}

async fn wait_for_active_operations(
    operation_monitor: &Arc<crate::operation_monitor::OperationMonitor>,
    shutdown_timeout: Duration,
    initial_count: usize,
    shutdown_reason: &str,
) {
    info!(
        "⏳ Waiting up to {:?} for {} active operation(s) to complete...",
        shutdown_timeout, initial_count
    );

    let shutdown_start = Instant::now();
    while shutdown_start.elapsed() < shutdown_timeout {
        let current = operation_monitor.get_shutdown_summary().await;
        if current.total_active == 0 {
            info!("OK All operations completed successfully");
            return;
        } else if current.total_active != initial_count {
            info!("📈 Progress: {} operations remaining", current.total_active);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let final_summary = operation_monitor.get_shutdown_summary().await;
    if final_summary.total_active > 0 {
        info!(
            "⏱️  Shutdown timeout reached - cancelling {} remaining operation(s) with reason: {}",
            final_summary.total_active, shutdown_reason
        );
        for op in final_summary.operations.iter() {
            tracing::debug!(
                "Attempting to cancel operation '{}' ({}) with reason: '{}'",
                op.id,
                op.tool_name,
                shutdown_reason
            );
            let cancelled = operation_monitor
                .cancel_operation_with_reason(&op.id, Some(shutdown_reason.to_string()))
                .await;
            if cancelled {
                info!("   OK Cancelled operation '{}' ({})", op.id, op.tool_name);
            } else {
                tracing::warn!(
                    "   WARNING Failed to cancel operation '{}' ({})",
                    op.id,
                    op.tool_name
                );
            }
        }
    }
}

// ============================================================================
// CRITICAL: Graceful Shutdown Implementation for Development Workflow
// ============================================================================
// PURPOSE: Solves graceful shutdown when cargo watch restarts the server.
// 1. Handles SIGTERM (cargo watch) and SIGINT (Ctrl+C) signals
// 2. Waits up to shutdown_timeout for in-flight operations to finish
// 3. Forces exit if the service doesn't stop within 5 additional seconds
// DO NOT REMOVE: Essential for development workflow integration.
// ============================================================================
async fn run_shutdown_handler(
    adapter: Arc<crate::adapter::Adapter>,
    operation_monitor: Arc<crate::operation_monitor::OperationMonitor>,
    shutdown_timeout: Duration,
) {
    let shutdown_reason = tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received SIGINT, initiating graceful shutdown...");
            "Cancelled due to SIGINT (Ctrl+C) - user interrupt"
        }
        _ = async {
            #[cfg(unix)]
            {
                let mut term_signal = signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("Failed to setup SIGTERM handler");
                term_signal.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        } => {
            info!("Received SIGTERM (likely from cargo watch), initiating graceful shutdown...");
            "Cancelled due to SIGTERM from cargo watch - source code reload"
        }
    };

    info!("🛑 Shutdown initiated - checking for active operations...");
    let shutdown_summary = operation_monitor.get_shutdown_summary().await;

    if shutdown_summary.total_active > 0 {
        wait_for_active_operations(
            &operation_monitor,
            shutdown_timeout,
            shutdown_summary.total_active,
            shutdown_reason,
        )
        .await;
    } else {
        info!("OK No active operations - proceeding with immediate shutdown");
    }

    info!("🔄 Shutting down adapter and shell pools...");
    emit_sandbox_terminated(shutdown_reason);
    adapter.shutdown().await;

    // Force process exit if service doesn't stop naturally
    tokio::time::sleep(Duration::from_secs(5)).await;
    info!("Service did not stop gracefully, forcing exit");
    std::process::exit(0);
}

/// Run in server mode (stdio MCP server).
///
/// # Arguments
/// * `config` - Immutable application configuration.
/// * `sandbox` - Sandbox configuration.
///
/// # Errors
/// Returns an error if the server fails to start or encounters a fatal error.
async fn check_bridge_running(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    #[cfg(unix)]
    if let Some(path) = socket_path
        && tokio::net::UnixStream::connect(path).await.is_ok()
    {
        return true;
    }
    if let Some(url) = http_url {
        let health_url = format!("{}/health", url.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap_or_default();
        if let Ok(resp) = client.get(&health_url).send().await
            && resp.status().is_success()
        {
            return true;
        }
    }
    false
}

/// Run in server mode (stdio MCP server).
///
/// # Arguments
/// * `config` - Immutable application configuration.
/// * `sandbox` - Sandbox configuration.
///
/// # Errors
/// Returns an error if the server fails to start or encounters a fatal error.
pub async fn run_server_mode(config: AppConfig, sandbox: Arc<sandbox::Sandbox>) -> Result<()> {
    let is_test = sandbox.is_test_mode()
        || std::env::var("NEXTEST").is_ok()
        || std::env::var("CARGO_MANIFEST_DIR").is_ok();

    let socket_path = if let Ok(path) = std::env::var("AHMA_UNIX_SOCKET") {
        path
    } else if config.unix_socket_path.is_empty() {
        "/tmp/ahma.sock".to_string()
    } else {
        config.unix_socket_path.clone()
    };
    let http_url = format!("http://{}:{}", config.http_host, config.http_port);

    let socket_path_opt = if cfg!(unix) {
        Some(socket_path.as_str())
    } else {
        None
    };
    let http_url_opt = Some(http_url.as_str());

    if !is_test && check_bridge_running(socket_path_opt, http_url_opt).await {
        tracing::info!(
            "Local bridge server is already running. Forwarding stdio as a proxy client."
        );
        return crate::shell::modes::proxy_client::run_proxy_client(socket_path_opt, http_url_opt)
            .await;
    }

    // Redirect stdout to stderr to prevent protocol stream corruption by standard prints
    if let Err(e) = crate::utils::stdio_redirect::redirect_stdout_to_stderr() {
        tracing::error!("Failed to redirect stdout to stderr: {}", e);
    }

    tracing::info!("Starting ahma_mcp v{}", env!("CARGO_PKG_VERSION"));
    if let Some(ref tools_dir) = config.tools_dir {
        tracing::info!("Tools directory: {:?}", tools_dir);
    } else {
        tracing::info!("No tools directory (using built-in internal tools only)");
    }
    tracing::info!("Command timeout: {}s", config.timeout_secs);

    // Try to wire up an HTTP MCP client proxy if mcp.json specifies one.
    try_setup_mcp_client(&config).await?;

    // Build the MCP service: monitor → pool → adapter → configs → service.
    let BuiltService {
        service,
        adapter,
        operation_monitor,
        shutdown_timeout,
        loaded_tools_count,
        configs: _configs,
    } = ServiceBuilder::new(&config, sandbox.clone())
        .build()
        .await?;
    let service_handler = service;

    // Register this stdio instance with the hub daemon so TUI can see it.
    {
        let scope_str = config
            .sandbox_scopes
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".to_string());
        let label = config.instance_label.clone();
        crate::daemon_reporter::spawn_reporter(
            operation_monitor.clone(),
            "stdio",
            scope_str,
            label,
        );
    }

    // Hot-reload is opt-in because runtime writes can change tool behavior mid-session.
    if config.hot_reload_tools {
        if let Some(tools_dir) = config.tools_dir.clone() {
            service_handler.start_config_watcher(tools_dir, config.clone());
        } else {
            tracing::warn!(
                "AHMA_HOT_RELOAD=1 but no tools directory is configured; hot-reload is disabled"
            );
        }
    }

    let sandbox_scopes = sandbox
        .scopes()
        .iter()
        .map(|scope| scope.display().to_string())
        .collect::<Vec<_>>();
    tracing::info!(
        "Startup summary: sandbox_mode={}, sandbox_scopes={:?}, disable_temp_files={}, tools_dir={}, loaded_tools={}",
        sandbox_mode_name(&sandbox),
        sandbox_scopes,
        sandbox.is_no_temp_files(),
        config
            .tools_dir
            .as_ref()
            .map_or_else(|| "<none>".to_string(), |dir| dir.display().to_string()),
        loaded_tools_count,
    );

    let active_sessions_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    if !is_test {
        // Start background bridge server
        let server_command = std::env::current_exe()
            .context("Failed to get current executable path")?
            .to_string_lossy()
            .to_string();

        let mut server_args = vec!["serve".to_string()];

        if config.explicit_tools_dir
            && let Some(ref tools_dir) = config.tools_dir
        {
            server_args.push("--tools-dir".to_string());
            server_args.push(tools_dir.to_string_lossy().to_string());
        }

        if let Some(ref task_vault) = config.task_vault {
            server_args.push("--task-vault".to_string());
            server_args.push(task_vault.to_string_lossy().to_string());
        }

        server_args.push("stdio".to_string());

        for bundle in &config.tool_bundles {
            server_args.push("--tool".to_string());
            server_args.push(bundle.clone());
        }

        let explicit_fallback_scope = if !config.sandbox_scopes.is_empty() {
            Some(
                dunce::canonicalize(&config.sandbox_scopes[0])
                    .unwrap_or_else(|_| config.sandbox_scopes[0].clone()),
            )
        } else {
            None
        };

        let bind_addr = format!("127.0.0.1:{}", config.http_port)
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:3000".parse().unwrap());

        #[cfg(unix)]
        let listener_kind = ahma_http_bridge::ListenerKind::Unix(socket_path.clone());
        #[cfg(not(unix))]
        let listener_kind = ahma_http_bridge::ListenerKind::Tcp(bind_addr);

        let bridge_config = ahma_http_bridge::BridgeConfig {
            bind_addr,
            server_command,
            server_args,
            enable_colored_output: true,
            default_sandbox_scope: explicit_fallback_scope,
            handshake_timeout_secs: config.handshake_timeout_secs,
            enable_quic: if cfg!(unix) { false } else { !config.no_quic },
            disable_http1_1: config.disable_http1_1,
            listener_kind,
            require_token: None,
            require_token_path: None,
            rate_limit_rps: 0,
            rate_limit_burst: 10,
            active_sessions: Some(active_sessions_counter.clone()),
        };

        tokio::spawn(async move {
            if let Err(e) = ahma_http_bridge::start_bridge(bridge_config).await {
                tracing::error!("Failed to start background bridge: {}", e);
            }
        });

        // Wait for the background bridge to be healthy/available
        let start_time = std::time::Instant::now();
        let mut healthy = false;
        while start_time.elapsed() < Duration::from_secs(2) {
            if check_bridge_running(socket_path_opt, http_url_opt).await {
                healthy = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !healthy {
            tracing::warn!("Background bridge server failed to become healthy within 2 seconds");
        } else {
            tracing::info!("Background bridge server started successfully and is healthy");
        }
    }

    use crate::transport_patch::PatchedStdioTransport;
    let service = service_handler
        .serve(PatchedStdioTransport::new_stdio())
        .await?;

    // Spawn graceful shutdown handler for SIGINT/SIGTERM.
    tokio::spawn(run_shutdown_handler(
        adapter.clone(),
        operation_monitor.clone(),
        shutdown_timeout,
    ));

    if is_test {
        let result = service.waiting().await;
        let reason = match &result {
            Ok(_) => "session_ended".to_string(),
            Err(e) => format!("session_error: {:#}", e),
        };
        emit_sandbox_terminated(&reason);
        adapter.shutdown().await;
        result?;
        return Ok(());
    }

    let stdio_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let stdio_active_clone = stdio_active.clone();

    let stdio_wait_task = tokio::spawn(async move {
        let res = service.waiting().await;
        stdio_active_clone.store(false, std::sync::atomic::Ordering::SeqCst);
        res
    });

    // Idle supervisor loop
    let active_sessions_clone = active_sessions_counter.clone();
    let adapter_clone = adapter.clone();
    let stdio_active_for_supervisor = stdio_active.clone();

    tokio::spawn(async move {
        let mut idle_duration = Duration::ZERO;
        let check_interval = Duration::from_millis(500);
        loop {
            tokio::time::sleep(check_interval).await;

            let stdio_on = stdio_active_for_supervisor.load(std::sync::atomic::Ordering::SeqCst);
            let active_count = active_sessions_clone.load(std::sync::atomic::Ordering::SeqCst);

            if !stdio_on && active_count == 0 {
                idle_duration += check_interval;
                if idle_duration >= Duration::from_secs(10) {
                    tracing::info!(
                        "No active clients (stdio disconnected and 0 bridge sessions) for 10 seconds. Shutting down."
                    );
                    emit_sandbox_terminated("idle_timeout");
                    adapter_clone.shutdown().await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    std::process::exit(0);
                }
            } else {
                idle_duration = Duration::ZERO;
            }
        }
    });

    let result = stdio_wait_task.await;
    if let Ok(res) = result {
        let reason = match &res {
            Ok(_) => "session_ended".to_string(),
            Err(e) => format!("session_error: {:#}", e),
        };
        emit_sandbox_terminated(&reason);
        res?;
    }

    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
