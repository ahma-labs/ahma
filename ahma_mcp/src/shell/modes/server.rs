//! # Server Mode
//!
//! Runs the ahma_mcp server in stdio mode, which is the default mode for MCP integration.

use crate::shell::cli::AppConfig;
use crate::{
    config::ServerConfig as MpcServerConfig,
    sandbox,
    service_builder::{BuiltService, ServiceBuilder},
    utils::logging::{BRIDGE_CAPTURE_HEADER, prepare_bridge_capture_files, read_log_tail},
    utils::stdio::emit_stdout_notification,
};
use ahma_common::timeouts::AUTO_SPAWNED_BRIDGE_IDLE_TIMEOUT_SECS;
use ahma_http_mcp_client::client::HttpMcpTransport;
use anyhow::{Context, Result};
use rmcp::ServiceExt;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, signal};
use tracing::info;

/// Try to wire up an HTTP MCP client proxy if `mcp.json` specifies one.
/// Missing or non-ahma configs (e.g. Cursor/VS Code) are silently ignored.
///
/// The transport is stored in a process-lifetime `OnceLock` so the connection
/// stays alive without resorting to `Box::leak`.
async fn try_setup_mcp_client(config: &AppConfig) -> Result<()> {
    static MCP_TRANSPORT: std::sync::OnceLock<HttpMcpTransport> = std::sync::OnceLock::new();

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
                let _ = MCP_TRANSPORT.set(transport);
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
fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(unix)]
async fn query_uds_health(path: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = tokio::net::UnixStream::connect(path);
    let mut stream = tokio::time::timeout(Duration::from_millis(200), connect)
        .await
        .ok()?
        .ok()?;

    let request = b"GET /health HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return None;
    }

    let mut buf = Vec::with_capacity(512);
    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read_to_end(&mut buf)).await;

    let response_str = std::str::from_utf8(&buf).ok()?;
    let body = response_str.split("\r\n\r\n").nth(1)?;
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed.get("version")?.as_str().map(String::from)
}

async fn query_tcp_health(url: &str) -> Option<String> {
    let health_url = format!("{}/health", url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap_or_default();
    let resp = client.get(&health_url).send().await.ok()?;
    if resp.status().is_success() {
        let parsed: serde_json::Value = resp.json().await.ok()?;
        return parsed.get("version")?.as_str().map(String::from);
    }
    None
}

pub async fn get_bridge_version(
    socket_path: Option<&str>,
    http_url: Option<&str>,
) -> Option<String> {
    #[cfg(unix)]
    if let Some(path) = socket_path
        && let Some(ver) = query_uds_health(path).await
    {
        return Some(ver);
    }
    if let Some(url) = http_url
        && let Some(ver) = query_tcp_health(url).await
    {
        return Some(ver);
    }
    #[cfg(not(unix))]
    let _ = socket_path;
    None
}

#[cfg(unix)]
async fn trigger_uds_restart(path: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = tokio::net::UnixStream::connect(path);
    let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_millis(200), connect).await else {
        return false;
    };

    let request = b"POST /restart HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return false;
    }

    let mut buf = Vec::with_capacity(512);
    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read_to_end(&mut buf)).await;

    let response_str = std::str::from_utf8(&buf).unwrap_or("");
    response_str.starts_with("HTTP/1.") && response_str.contains(" 200 ")
}

async fn trigger_tcp_restart(url: &str) -> bool {
    let restart_url = format!("{}/restart", url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap_or_default();
    if let Ok(resp) = client.post(&restart_url).send().await {
        return resp.status().is_success();
    }
    false
}

pub async fn trigger_bridge_restart(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    #[cfg(unix)]
    if let Some(path) = socket_path
        && trigger_uds_restart(path).await
    {
        return true;
    }
    if let Some(url) = http_url
        && trigger_tcp_restart(url).await
    {
        return true;
    }
    #[cfg(not(unix))]
    let _ = socket_path;
    false
}

async fn check_bridge_running(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    get_bridge_version(socket_path, http_url).await.is_some()
}

/// Run in server mode (stdio MCP server).
///
/// # Arguments
/// * `config` - Immutable application configuration.
/// * `sandbox` - Sandbox configuration.
///
/// # Errors
/// Returns an error if the server fails to start or encounters a fatal error.
async fn restart_bridge_server(socket_path_opt: Option<&str>, http_url_opt: Option<&str>) {
    tracing::info!(
        "Client version is newer than running bridge version. Requesting bridge restart..."
    );
    if trigger_bridge_restart(socket_path_opt, http_url_opt).await {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if !check_bridge_running(socket_path_opt, http_url_opt).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tracing::info!("Old bridge stopped. Starting new bridge...");
    } else {
        tracing::warn!("Failed to request bridge restart. Attempting to start anyway.");
    }
}

fn re_exec_current_process() -> Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&args);
    cmd.env("AHMA_RESTARTED", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(anyhow::anyhow!("Failed to re-exec client process: {}", err))
    }
    #[cfg(not(unix))]
    {
        let mut child = cmd
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;
        let status = child.wait()?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

async fn handle_version_checks(
    _config: &AppConfig,
    is_test: bool,
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
) -> Result<Option<()>> {
    let client_version = env!("CARGO_PKG_VERSION");
    let bridge_version_opt = if is_test {
        None
    } else {
        get_bridge_version(socket_path_opt, http_url_opt).await
    };

    let Some(bridge_version) = bridge_version_opt else {
        return Ok(None);
    };

    tracing::info!(
        client_version = client_version,
        bridge_version = %bridge_version,
        "Bridge version check: client={client_version} bridge={bridge_version}"
    );

    if bridge_version == client_version {
        tracing::info!(
            "Local bridge server is already running (v{bridge_version}). Forwarding stdio as a proxy client."
        );
        crate::shell::modes::proxy_client::run_proxy_client(socket_path_opt, http_url_opt).await?;
        return Ok(Some(()));
    }

    let c_ver = parse_version(client_version);
    let b_ver = parse_version(&bridge_version);
    let client_is_newer = match (c_ver, b_ver) {
        (Some(c), Some(b)) => c > b,
        _ => true,
    };

    if client_is_newer {
        tracing::info!(
            client_version = client_version,
            bridge_version = %bridge_version,
            "Client is newer than bridge; requesting bridge restart"
        );
        restart_bridge_server(socket_path_opt, http_url_opt).await;
    } else if std::env::var("AHMA_RESTARTED").is_ok() {
        return Err(anyhow::anyhow!(
            "Version mismatch: Client version (v{}) is older than running bridge version (v{}). Please update the client binary.",
            client_version,
            bridge_version
        ));
    } else {
        tracing::info!(
            "Client version (v{}) is older than running bridge version (v{}). Attempting self-restart (re-exec)...",
            client_version,
            bridge_version
        );
        re_exec_current_process()?;
    }
    Ok(None)
}

fn build_background_bridge_args(config: &AppConfig, resolved_scopes: &[PathBuf]) -> Vec<String> {
    let mut args = vec!["serve".to_string(), "--server-child".to_string()];

    // Helper closures to reduce push-pair verbosity.
    let push = |a: &mut Vec<String>, flag: &str| a.push(flag.to_string());
    let push_val = |a: &mut Vec<String>, flag: &str, val: String| {
        a.push(flag.to_string());
        a.push(val);
    };

    // Forward the resolved sandbox scope(s) so the bridge can use them as a fallback
    // for clients that don't send roots/list (e.g. Antigravity) and so the bridge
    // can pass them on to per-session subprocesses.
    for scope in resolved_scopes {
        push_val(
            &mut args,
            "--sandbox-scope",
            scope.to_string_lossy().to_string(),
        );
    }

    if config.explicit_tools_dir
        && let Some(ref tools_dir) = config.tools_dir
    {
        push_val(
            &mut args,
            "--tools-dir",
            tools_dir.to_string_lossy().to_string(),
        );
    }

    if let Some(ref task_vault) = config.task_vault {
        push_val(
            &mut args,
            "--task-vault",
            task_vault.to_string_lossy().to_string(),
        );
    }

    for bundle in &config.tool_bundles {
        push_val(&mut args, "--tools", bundle.clone());
    }

    let timeout = config
        .idle_timeout_secs
        .or(Some(AUTO_SPAWNED_BRIDGE_IDLE_TIMEOUT_SECS));
    if let Some(t) = timeout
        && t > 0
    {
        push_val(&mut args, "--idle-timeout", t.to_string());
    }

    if !config.unix_socket_path.is_empty() {
        push_val(
            &mut args,
            "--unix-socket-path",
            config.unix_socket_path.clone(),
        );
    }

    // Boolean flags — each enabled only when the config field is set.
    if config.no_sandbox {
        push(&mut args, "--no-sandbox");
    }
    if config.skip_availability_probes {
        push(&mut args, "--skip-probes");
    }
    if config.force_sync {
        push(&mut args, "--sync");
    }
    if config.hot_reload_tools {
        push(&mut args, "--hot-reload");
    }
    if config.defer_sandbox {
        push(&mut args, "--defer-sandbox");
    }
    if config.tmp_access {
        push(&mut args, "--tmp");
    }
    if config.no_temp_files {
        push(&mut args, "--disable-temp-files");
    }

    // Numeric flags — forwarded only when non-zero.
    if config.rate_limit_rps > 0 {
        push_val(
            &mut args,
            "--rate-limit-rps",
            config.rate_limit_rps.to_string(),
        );
    }
    if config.rate_limit_burst > 0 {
        push_val(
            &mut args,
            "--rate-limit-burst",
            config.rate_limit_burst.to_string(),
        );
    }
    if config.handshake_timeout_secs > 0 {
        push_val(
            &mut args,
            "--handshake-timeout",
            config.handshake_timeout_secs.to_string(),
        );
    }

    // Auth flags.
    if let Some(ref token) = config.require_token {
        push_val(&mut args, "--require-token", token.clone());
    }
    if let Some(ref path) = config.require_token_path {
        push_val(
            &mut args,
            "--require-token-path",
            path.to_string_lossy().to_string(),
        );
    }

    if !config.instance_label.is_empty() {
        push_val(&mut args, "--instance-label", config.instance_label.clone());
    }

    args
}

/// Open a capture file for bridge output, writing `banner` as its first line.
/// Returns `None` (and logs a warning) if the file can't be created or opened.
fn open_capture_file(path: &std::path::Path, banner: &str) -> Option<std::fs::File> {
    match std::fs::write(path, banner)
        .and_then(|_| std::fs::OpenOptions::new().append(true).open(path))
    {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("Failed to create bridge capture at {}: {e}", path.display());
            None
        }
    }
}

async fn spawn_background_bridge(
    config: &AppConfig,
    resolved_scopes: &[PathBuf],
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
) -> Result<()> {
    let server_command = std::env::current_exe()
        .context("Failed to get current executable path")?
        .to_string_lossy()
        .to_string();

    let server_args = build_background_bridge_args(config, resolved_scopes);

    let mut cmd = tokio::process::Command::new(&server_command);
    cmd.args(&server_args).env("AHMA_SERVER_CHILD", "1");

    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let (stdout_path, stderr_path) = match prepare_bridge_capture_files() {
        Ok(paths) => paths,
        Err(e) => {
            tracing::error!("Failed to prepare bridge capture files in logs/: {e:#}");
            return Err(e);
        }
    };

    let spawn_banner = format!(
        "{BRIDGE_CAPTURE_HEADER}# bridge spawn parent_pid={}\n",
        std::process::id()
    );

    match open_capture_file(&stdout_path, &spawn_banner) {
        Some(f) => cmd.stdout(f),
        None => cmd.stdout(std::process::Stdio::null()),
    };
    match open_capture_file(&stderr_path, &spawn_banner) {
        Some(f) => cmd.stderr(f),
        None => cmd.stderr(std::process::Stdio::null()),
    };
    cmd.stdin(std::process::Stdio::null());

    match cmd.spawn() {
        Ok(_) => tracing::info!("Spawned background bridge server successfully"),
        Err(e) => tracing::error!("Failed to spawn background bridge server: {}", e),
    }

    // Wait for the background bridge to be healthy/available
    let start_time = std::time::Instant::now();
    let mut healthy = false;
    let timeout =
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick);
    while start_time.elapsed() < timeout {
        if check_bridge_running(socket_path_opt, http_url_opt).await {
            healthy = true;
            break;
        }
        tokio::time::sleep(ahma_common::timeouts::TestTimeouts::poll_interval()).await;
    }
    if !healthy {
        let stderr_tail = read_log_tail(&stderr_path, 8192);
        if stderr_tail.is_empty() {
            tracing::error!(
                bridge_stderr = %stderr_path.display(),
                "Background bridge failed health check within {timeout:?}; bridge stderr capture is empty"
            );
        } else {
            tracing::error!(
                bridge_stderr = %stderr_path.display(),
                bridge_stderr_tail = %stderr_tail,
                "Background bridge failed health check within {timeout:?}"
            );
        }
        return Err(anyhow::anyhow!(
            "Background bridge server failed to become healthy within {timeout:?}. \
             Check {} and logs/ahma.log for details.",
            stderr_path.display()
        ));
    }
    tracing::info!(
        bridge_stdout = %stdout_path.display(),
        bridge_stderr = %stderr_path.display(),
        "Background bridge server started successfully and is healthy"
    );

    Ok(())
}

/// Returns true when the process is running as a server-child subprocess.
/// In this mode we skip the background bridge spawn and run the service directly.
/// Detection is via the `--server-child` CLI flag or the `AHMA_SERVER_CHILD`
/// internal plumbing variable (set only by the parent ahma process).
fn is_test_or_server_child(config: &AppConfig) -> bool {
    std::env::var("AHMA_SERVER_CHILD").is_ok() || config.is_server_child
}

/// Resolve the Unix socket path and HTTP URL used to communicate with the background bridge.
/// Returns `(socket_path_string, http_url_string)`.
fn resolve_bridge_endpoints(config: &AppConfig) -> (String, String) {
    // AHMA_UNIX_SOCKET is retired per R-CFG1.2. Use --unix-socket-path CLI flag or
    // settings.toml instead. The value comes from AppConfig.unix_socket_path which
    // was already resolved at startup.
    let socket_path = if config.unix_socket_path.is_empty() {
        "/tmp/ahma.sock".to_string()
    } else {
        config.unix_socket_path.clone()
    };
    let http_url = format!("http://{}:{}", config.http_host, config.http_port);
    (socket_path, http_url)
}

pub async fn run_server_mode(config: AppConfig, sandbox: Arc<sandbox::Sandbox>) -> Result<()> {
    let is_test = is_test_or_server_child(&config);

    let (socket_path, http_url) = resolve_bridge_endpoints(&config);

    let socket_path_opt = if cfg!(unix) {
        Some(socket_path.as_str())
    } else {
        None
    };
    let http_url_opt = Some(http_url.as_str());

    if let Some(()) = handle_version_checks(&config, is_test, socket_path_opt, http_url_opt).await?
    {
        return Ok(());
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
        match config.tools_dir.clone() {
            Some(tools_dir) => service_handler.start_config_watcher(tools_dir, config.clone()),
            None => tracing::warn!(
                "--hot-reload is set but no tools directory is configured; hot-reload is disabled"
            ),
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

    if !is_test {
        let resolved_scopes: Vec<PathBuf> = sandbox.scopes().to_vec();
        spawn_background_bridge(&config, &resolved_scopes, socket_path_opt, http_url_opt).await?;
        // Proceed with proxy setup
        return crate::shell::modes::proxy_client::run_proxy_client(socket_path_opt, http_url_opt)
            .await;
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

    let result = service.waiting().await;
    let reason = match &result {
        Ok(_) => "session_ended".to_string(),
        Err(e) => format!("session_error: {:#}", e),
    };
    emit_sandbox_terminated(&reason);
    adapter.shutdown().await;
    result?;

    Ok(())
}
