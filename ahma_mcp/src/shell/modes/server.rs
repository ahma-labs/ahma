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
    let client_build_id = ahma_common::BUILD_ID;
    let bridge_version_opt = if is_test {
        None
    } else {
        get_bridge_version(socket_path_opt, http_url_opt).await
    };

    let Some(bridge_version_raw) = bridge_version_opt else {
        return Ok(None);
    };

    // Version strings from the health endpoint may carry a build-id suffix:
    // "0.12.5+abc1234".  Split them out for independent semver and build-id checks.
    let (bridge_semver, bridge_build_id) = split_version_and_build_id(&bridge_version_raw);
    let client_semver = client_version;

    tracing::info!(
        client_version = client_version,
        client_build_id = client_build_id,
        bridge_version = %bridge_version_raw,
        "Bridge version check: client={client_version}+{client_build_id} bridge={bridge_version_raw}"
    );

    let same_semver = bridge_semver == client_semver;
    // When the bridge exposes a build-id, check it too.  Differing build-ids on the
    // same semver mean a dev rebuild happened without bumping the version — treat the
    // running bridge as stale.
    let same_build = bridge_build_id.is_none_or(|bid| bid == client_build_id);

    if same_semver && same_build {
        return proxy_to_matching_bridge(socket_path_opt, http_url_opt, &bridge_version_raw).await;
    }

    let c_ver = parse_version(client_semver);
    let b_ver = parse_version(bridge_semver);
    let client_is_newer = match (c_ver, b_ver) {
        (Some(c), Some(b)) => c > b,
        _ => true,
    };

    // Same semver but different build-id: the bridge is a dev-rebuild peer on the same
    // version; treat it as stale (client binary is "newer" in intent).
    let client_is_newer = client_is_newer || (same_semver && !same_build);

    reconcile_version_mismatch(
        socket_path_opt,
        http_url_opt,
        client_version,
        &bridge_version_raw,
        client_is_newer,
    )
    .await
}

/// Bridge reports the same semver and build-id as the client: forward stdio to it as a
/// proxy client. If the proxy fails, or the bridge closed without forwarding any response,
/// the daemon is stale — restart it once and fall back to a fresh spawn (`Ok(None)`), or
/// error out if a restart was already attempted in this lineage.
async fn proxy_to_matching_bridge(
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
    bridge_version_raw: &str,
) -> Result<Option<()>> {
    tracing::info!(
        "Local bridge server is already running (v{bridge_version_raw}). Forwarding stdio as a proxy client."
    );
    let proxy_result =
        crate::shell::modes::proxy_client::run_proxy_client(socket_path_opt, http_url_opt).await;
    match proxy_result {
        // Bridge responded normally — this was a real MCP session that ended cleanly.
        Ok(true) => Ok(Some(())),
        result => {
            // Proxy failed (Err) OR the bridge closed the connection before sending any
            // response (Ok(false)).  Both indicate a stale / incompatible bridge daemon
            // that happens to report the same version string.
            if let Err(ref e) = result {
                tracing::warn!(
                    bridge_version = %bridge_version_raw,
                    error = %e,
                    "Proxy to same-version bridge failed; bridge may be stale"
                );
            } else {
                tracing::warn!(
                    bridge_version = %bridge_version_raw,
                    "Proxy to same-version bridge exited without forwarding any bridge \
                     response; bridge may be stale (same semver, incompatible binary)"
                );
            }
            if std::env::var("AHMA_RESTARTED").is_err() {
                tracing::info!(
                    "Triggering bridge restart and falling back to fresh bridge spawn..."
                );
                restart_bridge_server(socket_path_opt, http_url_opt).await;
                // Return Ok(None) so run_server_mode proceeds to spawn a fresh bridge
                // and connect to it.
                Ok(None)
            } else {
                Err(anyhow::anyhow!(
                    "Proxy to same-version bridge (v{bridge_version_raw}) failed after \
                     restart attempt. Please restart ahma manually: \
                     `pkill -f 'ahma serve'` then restart your IDE."
                ))
            }
        }
    }
}

/// Bridge semver/build differs from the client. When the client is newer, restart the
/// bridge (unless a restart already ran this lineage, which would risk a respawn storm).
/// When the client is older, re-exec into the matching binary, or error if we already did.
/// Returns `Ok(None)` so the caller proceeds to spawn/connect to a fresh bridge.
async fn reconcile_version_mismatch(
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
    client_version: &str,
    bridge_version_raw: &str,
    client_is_newer: bool,
) -> Result<Option<()>> {
    if client_is_newer {
        if std::env::var("AHMA_RESTARTED").is_ok() {
            // We already restarted once in this lineage. A *persistent*
            // version/build mismatch must not trigger another restart — that is
            // how a respawn storm starts when several ahma build-ids transiently
            // coexist (e.g. a dev rebuild while old `ahma serve` processes still
            // run). Proxy to whatever bridge is running instead; with a single
            // installed build-id the mismatch converges after one restart, so a
            // mismatch that *survives* a restart means restarting again is futile.
            // (Symmetric with the same-semver stale-bridge branch above.)
            tracing::warn!(
                client_version = client_version,
                bridge_version = %bridge_version_raw,
                "Bridge version/build mismatch persists after a restart; proxying without restarting again to avoid a respawn storm"
            );
        } else {
            tracing::info!(
                client_version = client_version,
                bridge_version = %bridge_version_raw,
                "Client is newer than bridge (or same version with different build); requesting bridge restart"
            );
            restart_bridge_server(socket_path_opt, http_url_opt).await;
        }
    } else if std::env::var("AHMA_RESTARTED").is_ok() {
        return Err(anyhow::anyhow!(
            "Version mismatch: Client version (v{}) is older than running bridge version (v{}). Please update the client binary.",
            client_version,
            bridge_version_raw
        ));
    } else {
        tracing::info!(
            "Client version (v{}) is older than running bridge version (v{}). Attempting self-restart (re-exec)...",
            client_version,
            bridge_version_raw
        );
        re_exec_current_process()?;
    }
    Ok(None)
}

/// Split a version string of the form `"semver+build_id"` into `(semver, Option<build_id>)`.
/// If there is no `+` separator, the build_id portion is `None`.
pub(crate) fn split_version_and_build_id(v: &str) -> (&str, Option<&str>) {
    if let Some(idx) = v.find('+') {
        (&v[..idx], Some(&v[idx + 1..]))
    } else {
        (v, None)
    }
}

pub(crate) fn build_background_bridge_args(config: &AppConfig) -> Vec<String> {
    let mut args = vec!["serve".to_string(), "--server-child".to_string()];

    // Helper closures to reduce push-pair verbosity.
    let push = |a: &mut Vec<String>, flag: &str| a.push(flag.to_string());
    let push_val = |a: &mut Vec<String>, flag: &str, val: String| {
        a.push(flag.to_string());
        a.push(val);
    };

    // Forward ONLY genuinely explicit sandbox scopes (from --sandbox-scope,
    // --working-dir, or task vault) so the bridge is not locked to a
    // provisional temp/CWD that the stdio parent derived at startup.
    // --sandbox and --tmp are forwarded as boolean flags below so the bridge
    // can derive ~/sandbox and temp access independently for each session.
    for scope in &config.sandbox_scopes {
        push_val(
            &mut args,
            "--sandbox-scope",
            scope.to_string_lossy().to_string(),
        );
    }
    for wd in &config.working_dirs {
        push_val(&mut args, "--working-dir", wd.to_string_lossy().to_string());
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
    if config.use_sandbox_dir {
        push(&mut args, "--sandbox");
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
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
) -> Result<()> {
    let server_command = std::env::current_exe()
        .context("Failed to get current executable path")?
        .to_string_lossy()
        .to_string();

    let server_args = build_background_bridge_args(config);

    let mut cmd = tokio::process::Command::new(&server_command);
    cmd.args(&server_args).env("AHMA_SERVER_CHILD", "1").env(
        ahma_common::process_guard::SPAWN_DEPTH_ENV,
        ahma_common::process_guard::child_spawn_depth(),
    );

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
    // Circuit breaker for self-respawn loops: if this `ahma serve` is nested far
    // deeper than the legitimate frontend→bridge→peer chain, refuse to start so
    // the chain stops growing instead of exhausting the OS process table.
    if let Err(msg) = ahma_common::process_guard::check_spawn_depth() {
        tracing::error!("{msg}");
        return Err(anyhow::anyhow!(msg));
    }

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
    crate::register_active_service(Arc::new(service_handler.clone()));

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
        // This is the IDE-facing frontend: it was spawned by an editor/agent
        // over a stdin/stdout pipe and proxies to the detached background
        // bridge. Arm the parent-death watchdog so we exit if that IDE dies
        // without cleanly closing our stdin (uncaught kill, inherited pipe
        // fds, or a hang before the proxy loop begins reading). This is the
        // backstop that prevents orphaned `ahma serve stdio` processes from
        // accumulating across IDE sessions. The detached bridge/daemon are
        // deliberately NOT armed (they outlive their spawner by design and
        // self-terminate via idle-timeout).
        crate::utils::parent_watchdog::spawn_parent_death_watchdog();

        let resolved_scopes: Vec<PathBuf> = sandbox.scopes().to_vec();
        let _ = resolved_scopes; // kept for startup log below; not forwarded to bridge
        spawn_background_bridge(&config, socket_path_opt, http_url_opt).await?;
        // Proceed with proxy setup — map Ok(bool) → Ok(()) since the caller only
        // cares about success/failure at this final stage.
        return crate::shell::modes::proxy_client::run_proxy_client(socket_path_opt, http_url_opt)
            .await
            .map(|_| ());
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn base_cfg() -> AppConfig {
        AppConfig {
            no_sandbox: true,
            sandbox_scopes: vec![],
            use_sandbox_dir: false,
            sandbox_directory: Some(std::path::PathBuf::from("~/sandbox")),
            tmp_access: false,
            defer_sandbox: false,
            working_dirs: vec![],
            persistent_scopes: vec![],
            explicit_tools_dir: false,
            tools_dir: None,
            tool_bundles: vec![],
            timeout_secs: 360,
            force_sync: false,
            hot_reload_tools: false,
            skip_availability_probes: false,
            no_temp_files: false,
            log_monitor: false,
            monitor_rate_limit_secs: 60,
            package_cache_write: true,
            http_host: "127.0.0.1".to_string(),
            http_port: 3000,
            no_quic: false,
            disable_http1_1: false,
            handshake_timeout_secs: 45,
            unix_socket_path: String::new(),
            list_server: None,
            mcp_config: std::path::PathBuf::from("mcp.json"),
            list_http: None,
            list_format: crate::shell::OutputFormat::Text,
            run_tool: None,
            run_tool_args: vec![],
            observability: ahma_common::observability::ObservabilityConfig::default(),
            task_vault: None,
            require_token: None,
            require_token_path: None,
            rate_limit_rps: 0,
            rate_limit_burst: 10,
            instance_label: "ahma".to_string(),
            idle_timeout_secs: None,
            max_sessions: 10,
            is_server_child: false,
            minimize_tokens: false,
            small_model_harness: false,
            mutex_groups: ahma_common::config::default_mutex_groups(),
            separate_cargo_target: false,
        }
    }

    /// --sandbox is forwarded; empty sandbox_scopes do not produce --sandbox-scope.
    #[test]
    fn test_bridge_args_sandbox_flag_no_scope_forwarded() {
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            use_sandbox_dir: true,
            sandbox_directory: Some(tmp.path().to_path_buf()),
            ..base_cfg()
        };

        let args = build_background_bridge_args(&cfg);
        let has_sandbox_scope = args.windows(2).any(|w| w[0] == "--sandbox-scope");
        assert!(
            !has_sandbox_scope,
            "empty sandbox_scopes must not produce --sandbox-scope: {args:?}"
        );
        assert!(
            args.contains(&"--sandbox".to_string()),
            "--sandbox flag must be forwarded: {args:?}"
        );
    }

    /// Explicit --sandbox-scope is forwarded; --sandbox not forwarded when not set.
    #[test]
    fn test_bridge_args_explicit_scope_forwarded_no_sandbox_flag() {
        let tmp = tempdir().unwrap();
        let scope = tmp.path().to_path_buf();
        let cfg = AppConfig {
            sandbox_scopes: vec![scope.clone()],
            use_sandbox_dir: false,
            ..base_cfg()
        };

        let args = build_background_bridge_args(&cfg);
        let scope_idx = args
            .iter()
            .position(|a| a == "--sandbox-scope")
            .expect("explicit scope must appear as --sandbox-scope");
        let expected = scope.to_string_lossy().into_owned();
        assert!(
            args[scope_idx + 1].contains(expected.as_str()),
            "scope value must follow --sandbox-scope: {args:?}"
        );
        assert!(
            !args.contains(&"--sandbox".to_string()),
            "--sandbox must not appear when use_sandbox_dir is false: {args:?}"
        );
    }

    /// Both --sandbox and explicit --sandbox-scope coexist when both are set.
    #[test]
    fn test_bridge_args_both_sandbox_and_scope() {
        let tmp = tempdir().unwrap();
        let scope = tmp.path().to_path_buf();
        let cfg = AppConfig {
            sandbox_scopes: vec![scope.clone()],
            use_sandbox_dir: true,
            sandbox_directory: Some(tmp.path().to_path_buf()),
            ..base_cfg()
        };

        let args = build_background_bridge_args(&cfg);
        assert!(
            args.contains(&"--sandbox".to_string()),
            "--sandbox must be present: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "--sandbox-scope"),
            "--sandbox-scope must be present: {args:?}"
        );
    }
}
