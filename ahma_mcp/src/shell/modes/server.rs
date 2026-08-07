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

/// Start the guarded egress proxy and route every sandboxed subprocess through it
/// when `--restrict-network` (or `[network] restrict`) is on (SPEC R-NET). Returns
/// the proxy handle, which the caller must keep alive for the server's lifetime
/// (dropping it aborts the proxy). `None` when restriction is off.
///
/// Discloses the active restriction loudly (SPEC R7): the operator must be able to
/// see that egress is gated, which domains are reachable, and that the containment
/// is advisory (a tool that ignores `HTTP_PROXY` is not held by this alone —
/// kernel-level enforcement is a separate, platform-specific step).
///
/// `net_approval` carries the peer handle a subprocess connection to an
/// unlisted domain can raise an `elicitation/create` prompt against (SPEC
/// R-NET interactive approval): the caller passes the same peer handle the
/// MCP service populates on connect, so a prompt can reach whichever client
/// is attached by the time a subprocess actually tries to connect.
async fn maybe_start_egress_proxy(
    config: &AppConfig,
    sandbox: &sandbox::Sandbox,
    net_approval: crate::egress::NetApprovalContext,
) -> Option<crate::egress::EgressProxy> {
    if !config.restrict_network {
        return None;
    }
    let allowlist = crate::egress::EgressAllowlist::from_str(&config.network_allow.join("\n"));
    let proxy = match crate::egress::EgressProxy::start(crate::egress::EgressProxyConfig {
        allowlist,
        net_approval,
        ..Default::default()
    })
    .await
    {
        Ok(p) => p,
        Err(e) => {
            // Fail loud but non-fatal: the server still runs, just without the
            // network restriction. (We do not silently pretend it is enforced.)
            tracing::error!(
                "--restrict-network: failed to start the egress proxy ({e}); subprocess network \
                 egress is NOT restricted this session"
            );
            return None;
        }
    };
    sandbox::set_egress_proxy_env(proxy.env_vars());
    // macOS enforcement (R-NET): the Seatbelt profile denies all outbound IP
    // egress except this proxy address, so a subprocess that ignores HTTP_PROXY
    // still cannot reach the network directly. No-op on other platforms.
    sandbox.set_egress_proxy_addr(Some(proxy.local_addr));
    if config.network_allow.is_empty() {
        tracing::warn!(
            "NETWORK EGRESS RESTRICTED (--restrict-network): [network] allow is EMPTY, so ALL \
             subprocess network egress is denied. Add domains to [network] allow in \
             ~/.ahma/settings.toml. {enforcement}",
            enforcement = network_enforcement_note(),
        );
    } else {
        tracing::warn!(
            "NETWORK EGRESS RESTRICTED (--restrict-network): sandboxed subprocesses are routed \
             through a guarded proxy at {addr}; reachable domains: {allow:?}. Private/loopback/\
             cloud-metadata targets are refused. {enforcement}",
            addr = proxy.local_addr,
            allow = config.network_allow,
            enforcement = network_enforcement_note(),
        );
    }
    Some(proxy)
}

/// Disclose how strongly the network restriction is enforced on this platform.
/// On macOS the Seatbelt profile denies direct egress (only the proxy is
/// reachable), so it is kernel-enforced; elsewhere it is advisory (a tool that
/// ignores `HTTP_PROXY` or opens a raw socket is not contained — see the README).
fn network_enforcement_note() -> &'static str {
    if cfg!(target_os = "macos") {
        "Kernel-enforced (Seatbelt): direct egress that bypasses the proxy is blocked."
    } else if cfg!(target_os = "linux") {
        "Kernel-enforced where supported (Landlock, kernel 6.7+): outbound TCP is restricted to \
         the proxy port. Advisory on older kernels and for UDP (see the README network limits)."
    } else {
        "Advisory: a tool that ignores HTTP_PROXY or opens a raw socket is not contained \
         (see the README network limits)."
    }
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

/// Cancel every operation still listed as active in `final_summary`, logging the
/// outcome of each cancellation attempt. Self-contained tail-end of the shutdown
/// wait: only reached once the grace period has elapsed.
async fn cancel_remaining_operations(
    operation_monitor: &Arc<crate::operation_monitor::OperationMonitor>,
    final_summary: &crate::operation_monitor::ShutdownSummary,
    shutdown_reason: &str,
) {
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
        cancel_remaining_operations(operation_monitor, &final_summary, shutdown_reason).await;
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
// The functions below are the cross-crate chokepoint for bridge
// liveness/version/restart checks (see AGENTS.md): every surface that needs
// to know whether a bridge is up, what version it reports, or must ask it to
// restart — including `ahma_tui` — calls through here rather than
// reimplementing the raw HTTP/1.0-over-`UnixStream` and `reqwest` probes.
pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// A running bridge's self-reported version and (if any) the sandbox scope it
/// is actually configured for. `default_sandbox_scope` lets a caller decide
/// whether an already-healthy bridge is safe to reuse for a *different*
/// project, instead of assuming any healthy bridge is scoped correctly
/// (SPEC R7).
#[derive(Debug, PartialEq, Eq)]
pub struct BridgeHealth {
    pub version: String,
    pub default_sandbox_scope: Option<String>,
}

fn parse_health_body(body: &str) -> Option<BridgeHealth> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let version = parsed.get("version")?.as_str()?.to_string();
    let default_sandbox_scope = parsed
        .get("default_sandbox_scope")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(BridgeHealth {
        version,
        default_sandbox_scope,
    })
}

#[cfg(unix)]
pub async fn query_uds_health(path: &str) -> Option<BridgeHealth> {
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
    parse_health_body(body)
}

pub async fn query_tcp_health(url: &str) -> Option<BridgeHealth> {
    let health_url = format!("{}/health", url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap_or_default();
    let resp = client.get(&health_url).send().await.ok()?;
    if resp.status().is_success() {
        let body = resp.text().await.ok()?;
        return parse_health_body(&body);
    }
    None
}

/// Query the bridge's `/health` endpoint, preferring the Unix socket over TCP.
pub async fn query_bridge_health(
    socket_path: Option<&str>,
    http_url: Option<&str>,
) -> Option<BridgeHealth> {
    #[cfg(unix)]
    if let Some(path) = socket_path
        && let Some(health) = query_uds_health(path).await
    {
        return Some(health);
    }
    if let Some(url) = http_url
        && let Some(health) = query_tcp_health(url).await
    {
        return Some(health);
    }
    #[cfg(not(unix))]
    let _ = socket_path;
    None
}

pub async fn get_bridge_version(
    socket_path: Option<&str>,
    http_url: Option<&str>,
) -> Option<String> {
    query_bridge_health(socket_path, http_url)
        .await
        .map(|h| h.version)
}

#[cfg(unix)]
pub async fn trigger_uds_restart(path: &str) -> bool {
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

pub async fn trigger_tcp_restart(url: &str) -> bool {
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

/// Default TCP port of the shared bridge — the other machine-global endpoint.
pub const GLOBAL_HTTP_PORT: u16 = 3000;

/// True when `url` addresses the shared bridge's default port on loopback.
fn is_global_bridge_url(url: &str) -> bool {
    url.rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
        .is_some_and(|port| port == GLOBAL_HTTP_PORT)
}

pub async fn trigger_bridge_restart(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    // A test must never shut down a bridge it does not own. A test-spawned ahma is
    // a freshly built binary, so it carries a different BUILD_ID; pointed at the
    // machine-global endpoint it decides the running bridge is "stale" and restarts
    // whatever owns it — the developer's live MCP server, or another application's.
    //
    // Strip only the *global* endpoints, rather than refusing every restart: a test
    // driving its own mock bridge on a private socket/port is legitimate.
    let (socket_path, http_url) = if is_test_isolated() {
        let global_socket = socket_path.is_some_and(|p| p == GLOBAL_SOCKET_PATH);
        let global_http = http_url.is_some_and(is_global_bridge_url);
        if global_socket || global_http {
            tracing::warn!(
                "Test-isolated process may not restart the shared bridge; ignoring \
                 global endpoints (socket={socket_path:?}, http={http_url:?})"
            );
        }
        (
            socket_path.filter(|_| !global_socket),
            http_url.filter(|_| !global_http),
        )
    } else {
        (socket_path, http_url)
    };

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

/// How long [`restart_bridge_server`] waits for the old bridge to stop answering
/// health checks.
const BRIDGE_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// How often [`restart_bridge_server`] re-checks while waiting for that stop.
const BRIDGE_STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Poll `still_running` until it reports the bridge is gone, or until `timeout`
/// elapses. Returns `true` only when a check actually *observed* the bridge stop;
/// a timeout returns `false`.
///
/// The two outcomes are kept distinguishable on purpose. This wait previously
/// reported "Old bridge stopped." on both, which logged the one case an operator
/// needs to see — the old bridge outliving the wait, so a replacement is spawned
/// while it may still hold the socket or port — as a success.
async fn wait_for_bridge_to_stop(
    timeout: Duration,
    poll_interval: Duration,
    still_running: impl AsyncFn() -> bool,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !still_running().await {
            return true;
        }
        tokio::time::sleep(poll_interval).await;
    }
    false
}

/// Ask the running bridge to shut down, then wait (up to [`BRIDGE_STOP_TIMEOUT`])
/// for it to stop answering health checks so the caller can start a replacement.
///
/// Best-effort: neither a refused restart request nor a bridge that outlives the
/// wait is fatal — the caller spawns a fresh bridge either way. Both are logged at
/// `warn` so that a replacement which then fails to bind is explainable.
async fn restart_bridge_server(socket_path_opt: Option<&str>, http_url_opt: Option<&str>) {
    tracing::info!(
        "Client version is newer than running bridge version. Requesting bridge restart..."
    );
    if !trigger_bridge_restart(socket_path_opt, http_url_opt).await {
        tracing::warn!("Failed to request bridge restart. Attempting to start anyway.");
        return;
    }

    let stopped =
        wait_for_bridge_to_stop(BRIDGE_STOP_TIMEOUT, BRIDGE_STOP_POLL_INTERVAL, async || {
            check_bridge_running(socket_path_opt, http_url_opt).await
        })
        .await;

    if stopped {
        tracing::info!("Old bridge stopped. Starting new bridge...");
    } else {
        tracing::warn!(
            timeout_secs = BRIDGE_STOP_TIMEOUT.as_secs(),
            "Old bridge is still answering health checks after the shutdown wait; starting a \
             new bridge anyway. If the old process still holds the socket or port, the \
             replacement may fail to bind."
        );
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
    // No respawn hook here: this path has no AppConfig to spawn with, and a
    // stale same-version bridge already falls back to restart-and-respawn
    // below (AHMA_RESTARTED lineage guard).
    let proxy_result =
        crate::shell::modes::proxy_client::run_proxy_client(socket_path_opt, http_url_opt, None)
            .await;
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

    // Helpers so every forwarded flag stays on a single, auditable line.
    fn push(args: &mut Vec<String>, flag: &str) {
        args.push(flag.to_string());
    }
    fn push_val(args: &mut Vec<String>, flag: &str, val: impl Into<String>) {
        args.push(flag.to_string());
        args.push(val.into());
    }

    // Forward ONLY genuinely explicit sandbox scopes (from --sandbox-scope,
    // --working-dir, or task vault) so the bridge is not locked to a
    // provisional temp/CWD that the stdio parent derived at startup.
    // --sandbox and --tmp are forwarded as boolean flags below so the bridge
    // can derive ~/sandbox and temp access independently for each session.
    for scope in &config.sandbox_scopes {
        push_val(&mut args, "--sandbox-scope", scope.to_string_lossy());
    }
    for wd in &config.working_dirs {
        push_val(&mut args, "--working-dir", wd.to_string_lossy());
    }

    if config.explicit_tools_dir
        && let Some(ref tools_dir) = config.tools_dir
    {
        push_val(&mut args, "--tools-dir", tools_dir.to_string_lossy());
    }

    if let Some(ref task_vault) = config.task_vault {
        push_val(&mut args, "--task-vault", task_vault.to_string_lossy());
    }

    for bundle in &config.tool_bundles {
        push_val(&mut args, "--tools", bundle.clone());
    }

    let idle_timeout = config
        .idle_timeout_secs
        .unwrap_or(AUTO_SPAWNED_BRIDGE_IDLE_TIMEOUT_SECS);
    if idle_timeout > 0 {
        push_val(&mut args, "--idle-timeout", idle_timeout.to_string());
    }

    if !config.unix_socket_path.is_empty() {
        let socket = config.unix_socket_path.clone();
        push_val(&mut args, "--unix-socket-path", socket);
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
    if config.use_scratch_dir {
        push(&mut args, "--scratch");
    }
    if config.tmp_access {
        push(&mut args, "--tmp");
    }
    if config.no_temp_files {
        push(&mut args, "--disable-temp-files");
    }

    // Numeric flags — forwarded only when non-zero.
    if config.rate_limit_rps > 0 {
        let rps = config.rate_limit_rps.to_string();
        push_val(&mut args, "--rate-limit-rps", rps);
    }
    if config.rate_limit_burst > 0 {
        let burst = config.rate_limit_burst.to_string();
        push_val(&mut args, "--rate-limit-burst", burst);
    }
    if config.handshake_timeout_secs > 0 {
        let handshake = config.handshake_timeout_secs.to_string();
        push_val(&mut args, "--handshake-timeout", handshake);
    }

    // Auth flags.
    if let Some(ref token) = config.require_token {
        push_val(&mut args, "--require-token", token.clone());
    }
    if let Some(ref path) = config.require_token_path {
        push_val(&mut args, "--require-token-path", path.to_string_lossy());
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

/// Poll the bridge's health endpoint until it responds or `timeout` elapses.
/// Returns `true` as soon as a health check succeeds.
async fn wait_for_bridge_healthy(
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
    timeout: Duration,
) -> bool {
    let start_time = std::time::Instant::now();
    while start_time.elapsed() < timeout {
        if check_bridge_running(socket_path_opt, http_url_opt).await {
            return true;
        }
        tokio::time::sleep(ahma_common::timeouts::TestTimeouts::poll_interval()).await;
    }
    false
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

    // Intentionally detached (SPEC R-PROC.3): the background bridge must outlive
    // the process that spawned it, so it deliberately does NOT set kill_on_drop.
    // `process_group(0)` keeps it alive when our own process group goes away —
    // the opposite purpose it serves for an owned child (R-PROC.2).
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        cmd.creation_flags(crate::shell_pool::CREATE_NO_WINDOW);
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
    let timeout =
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::Quick);
    let healthy = wait_for_bridge_healthy(socket_path_opt, http_url_opt, timeout).await;
    if !healthy {
        return Err(bridge_unhealthy_error(&stderr_path, timeout));
    }
    tracing::info!(
        bridge_stdout = %stdout_path.display(),
        bridge_stderr = %stderr_path.display(),
        "Background bridge server started successfully and is healthy"
    );

    Ok(())
}

/// Build the error returned when the freshly spawned background bridge does not
/// become healthy in time, logging whatever stderr it managed to capture first.
fn bridge_unhealthy_error(stderr_path: &std::path::Path, timeout: Duration) -> anyhow::Error {
    let stderr_tail = read_log_tail(stderr_path, 8192);
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
    anyhow::anyhow!(
        "Background bridge server failed to become healthy within {timeout:?}. \
         Check {} and logs/ahma.log for details.",
        stderr_path.display()
    )
}

/// The machine-global bridge socket.
///
/// It is a well-known singleton *by design*: several MCP clients share one bridge
/// daemon. That sharing is exactly why a test must never resolve it — see
/// [`is_test_isolated`] and [`default_socket_path`].
pub const GLOBAL_SOCKET_PATH: &str = "/tmp/ahma.sock";

/// True when this process is running under the test harness.
///
/// `cfg!(test)` covers this crate's own unit tests;
/// [`ahma_common::test_isolation::spawned_under_test_harness`] covers the ahma
/// binaries that integration tests *spawn*, which `cfg!(test)` cannot see
/// because they are ordinary release/debug binaries. It detects both the
/// explicit `AHMA_TEST_ISOLATION` plumbing variable and the `NEXTEST` variable
/// that `cargo nextest` exports to every test process (and which children
/// inherit), so a spawn site that forgets the explicit variable can no longer
/// reach the machine-global endpoints (SPEC R-ISO.1).
///
/// A test-isolated process must never touch the machine-global endpoints: it gets
/// a private socket path, it never probes the running bridge's version, and it is
/// refused a bridge restart.
pub fn is_test_isolated() -> bool {
    cfg!(test) || ahma_common::test_isolation::spawned_under_test_harness()
}

/// Bridge socket to use when no explicit path is configured.
///
/// Under test isolation this is a per-process private path, so a test can never
/// reach — much less restart — a bridge it does not own.
fn default_socket_path() -> String {
    if is_test_isolated() {
        return std::env::temp_dir()
            .join(format!("ahma-test-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
    }
    GLOBAL_SOCKET_PATH.to_string()
}

/// Returns true when the process is running as a server-child subprocess.
/// In this mode we skip the background bridge spawn and run the service directly.
/// Detection is via the `--server-child` CLI flag or the `AHMA_SERVER_CHILD`
/// internal plumbing variable (set only by the parent ahma process).
///
/// Deliberately does NOT consider test isolation: this flag also disarms the
/// parent-death watchdog, and a test-spawned server must keep that watchdog
/// armed so it cannot outlive the test process. Test isolation is applied to the
/// version check separately — see [`is_test_isolated`].
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
        default_socket_path()
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

    // A test-isolated process gets a private socket, but the HTTP endpoint still
    // defaults to 127.0.0.1:3000 — a real bridge may be listening there. Skipping
    // the version probe entirely keeps a test from ever judging (and restarting) a
    // bridge it does not own.
    let skip_version_check = is_test || is_test_isolated();

    if let Some(()) =
        handle_version_checks(&config, skip_version_check, socket_path_opt, http_url_opt).await?
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

    // Scope-grant auto-detection: one shared coordinator drives both the adapter's
    // notifier (which emits requests) and the reporter (which resolves answers and
    // persists). The notifier delivers requests to the hub so a connected TUI can
    // show the "grant access?" modal; approval is persisted for the next start,
    // never applied to the live session (SPEC R5.4.7).
    let grant_coordinator = Arc::new(ahma_common::scope_grant::GrantCoordinator::new());
    let (grant_req_tx, grant_req_rx) = tokio::sync::mpsc::unbounded_channel();
    // The full question ladder (R-PERM.3), not a single hard-wired surface: ask the
    // MCP client that requested the work first (it is where the user is looking),
    // fall back to an attached TUI, and if neither can be asked, fail closed with a
    // command the user can paste. The hub channel below is rung 2.
    let permission_broker = Arc::new(crate::sandbox::PermissionBroker::new(
        grant_coordinator.clone(),
        Some(grant_req_tx),
    ));

    // Build the MCP service: monitor → pool → adapter → configs → service.
    let BuiltService {
        service,
        adapter,
        operation_monitor,
        shutdown_timeout,
        loaded_tools_count,
        configs: _configs,
    } = ServiceBuilder::new(&config, sandbox.clone())
        .with_permission_broker(permission_broker)
        .build()
        .await?;
    let service_handler = service;
    crate::register_active_service(Arc::new(service_handler.clone()));

    // Route sandboxed subprocesses through the guarded egress proxy when
    // `--restrict-network` is on (R-NET). Held for the server's lifetime — the
    // proxy's background task is aborted when this drops at function return.
    // Built after `service_handler` so an unlisted domain can raise an
    // interactive `elicitation/create` prompt at whichever MCP client attaches
    // (R-NET interactive approval): the proxy shares the service's own `peer`
    // handle, which the handshake populates once a client connects — the
    // sandbox does not lock (and no subprocess can spawn) until then, so
    // starting the proxy here does not weaken the existing enforcement timing.
    let net_approval = crate::egress::NetApprovalContext {
        coordinator: Arc::new(ahma_common::net_approval::NetApprovalCoordinator::new()),
        peer: service_handler.peer.clone(),
    };
    let _egress_proxy = maybe_start_egress_proxy(&config, &sandbox, net_approval).await;

    // Web-approval TUI surface (R-WEB.6): the service's own `WebApprovalCoordinator`
    // drives both the prompt delivery (via this sender) and the answer resolution
    // (the reporter shares the same coordinator), so a TUI approval takes effect for
    // the live session. Wired only in this daemon/server path.
    let (web_req_tx, web_req_rx) = tokio::sync::mpsc::unbounded_channel();
    service_handler.set_web_approval_sender(web_req_tx);
    let web_coordinator = service_handler.web_approval.clone();

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
            Some(crate::daemon_reporter::GrantReporting {
                coordinator: grant_coordinator,
                req_rx: grant_req_rx,
            }),
            Some(crate::daemon_reporter::WebApprovalReporting {
                coordinator: web_coordinator,
                req_rx: web_req_rx,
            }),
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
        return run_as_frontend_and_proxy(&config, socket_path_opt, http_url_opt).await;
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

/// Run this process as the IDE-facing frontend: it was spawned by an editor/agent
/// over a stdin/stdout pipe and proxies to the detached background bridge.
///
/// Arms the parent-death watchdog so we exit if that IDE dies without cleanly
/// closing our stdin (uncaught kill, inherited pipe fds, or a hang before the
/// proxy loop begins reading). This is the backstop that prevents orphaned
/// `ahma serve stdio` processes from accumulating across IDE sessions. The
/// detached bridge/daemon are deliberately NOT armed (they outlive their
/// spawner by design and self-terminate via idle-timeout).
async fn run_as_frontend_and_proxy(
    config: &AppConfig,
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
) -> Result<()> {
    crate::utils::parent_watchdog::spawn_parent_death_watchdog();

    spawn_background_bridge(config, socket_path_opt, http_url_opt).await?;

    // Respawn hook: if the bridge later dies or its socket vanishes (e.g. it
    // was killed out-of-band), the proxy's reconnect loop can bring a fresh
    // bridge up instead of re-dialing a gone endpoint until it gives up.
    let respawn_bridge: crate::shell::modes::proxy_client::BridgeRespawnFn = {
        let config = config.clone();
        let socket_path = socket_path_opt.map(str::to_string);
        let http_url = http_url_opt.map(str::to_string);
        Box::new(move || {
            let config = config.clone();
            let socket_path = socket_path.clone();
            let http_url = http_url.clone();
            Box::pin(async move {
                spawn_background_bridge(&config, socket_path.as_deref(), http_url.as_deref()).await
            })
        })
    };

    // Proceed with proxy setup — map Ok(bool) → Ok(()) since the caller only
    // cares about success/failure at this final stage.
    crate::shell::modes::proxy_client::run_proxy_client(
        socket_path_opt,
        http_url_opt,
        Some(respawn_bridge),
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};
    use tempfile::tempdir;

    /// Serializes tests that mutate process-global environment variables
    /// (e.g. `AHMA_SERVER_CHILD`). See AGENTS.md env-var test conventions.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn base_cfg() -> AppConfig {
        AppConfig {
            no_sandbox: true,
            restrict_network: false,
            network_allow: vec![],
            sandbox_scopes: vec![],
            use_scratch_dir: false,
            container_root: None,
            scratch_directory: None,
            tmp_access: false,
            defer_sandbox: false,
            working_dirs: vec![],
            persistent_scopes: vec![],
            explicit_tools_dir: false,
            tools_dir: None,
            tool_bundles: vec![],
            timeout_secs: 360,
            await_timeout_secs: 540,
            request_budget_override_secs: None,
            force_progress_notifications: false,
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
            settings_origin: crate::shell::cli::SettingsOriginCtx::default(),
        }
    }

    /// --sandbox is forwarded; empty sandbox_scopes do not produce --sandbox-scope.
    #[test]
    fn test_bridge_args_sandbox_flag_no_scope_forwarded() {
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            use_scratch_dir: true,
            scratch_directory: Some(tmp.path().to_path_buf()),
            ..base_cfg()
        };

        let args = build_background_bridge_args(&cfg);
        let has_sandbox_scope = args.windows(2).any(|w| w[0] == "--sandbox-scope");
        assert!(
            !has_sandbox_scope,
            "empty sandbox_scopes must not produce --sandbox-scope: {args:?}"
        );
        assert!(
            args.contains(&"--scratch".to_string()),
            "--scratch flag must be forwarded: {args:?}"
        );
    }

    /// Explicit --sandbox-scope is forwarded; --scratch not forwarded when not set.
    #[test]
    fn test_bridge_args_explicit_scope_forwarded_no_sandbox_flag() {
        let tmp = tempdir().unwrap();
        let scope = tmp.path().to_path_buf();
        let cfg = AppConfig {
            sandbox_scopes: vec![scope.clone()],
            use_scratch_dir: false,
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
            !args.contains(&"--scratch".to_string()),
            "--scratch must not appear when use_scratch_dir is false: {args:?}"
        );
    }

    /// Both --scratch and explicit --sandbox-scope coexist when both are set.
    #[test]
    fn test_bridge_args_both_sandbox_and_scope() {
        let tmp = tempdir().unwrap();
        let scope = tmp.path().to_path_buf();
        let cfg = AppConfig {
            sandbox_scopes: vec![scope.clone()],
            use_scratch_dir: true,
            scratch_directory: Some(tmp.path().to_path_buf()),
            ..base_cfg()
        };

        let args = build_background_bridge_args(&cfg);
        assert!(
            args.contains(&"--scratch".to_string()),
            "--scratch must be present: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "--sandbox-scope"),
            "--sandbox-scope must be present: {args:?}"
        );
    }

    // ------------------------------------------------------------------
    // parse_version
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_version_valid() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.0.0"), Some((0, 0, 0)));
    }

    #[test]
    fn test_parse_version_large_numbers() {
        assert_eq!(
            parse_version("4294967295.4294967295.4294967295"),
            Some((u32::MAX, u32::MAX, u32::MAX))
        );
    }

    #[test]
    fn test_parse_version_overflow_is_none() {
        // One past u32::MAX must fail to parse.
        assert_eq!(parse_version("4294967296.0.0"), None);
    }

    #[test]
    fn test_parse_version_too_few_parts() {
        assert_eq!(parse_version("1"), None);
        assert_eq!(parse_version("1.2"), None);
    }

    #[test]
    fn test_parse_version_non_numeric() {
        assert_eq!(parse_version("a.b.c"), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert_eq!(parse_version("1.x.3"), None);
    }

    #[test]
    fn test_parse_version_empty() {
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn test_parse_version_ignores_extra_parts() {
        // Only the first three dot-separated components are consumed.
        assert_eq!(parse_version("1.2.3.4"), Some((1, 2, 3)));
    }

    // ------------------------------------------------------------------
    // split_version_and_build_id
    // ------------------------------------------------------------------

    #[test]
    fn test_split_version_with_build_id() {
        assert_eq!(
            split_version_and_build_id("0.12.5+abc1234"),
            ("0.12.5", Some("abc1234"))
        );
    }

    #[test]
    fn test_split_version_without_build_id() {
        assert_eq!(split_version_and_build_id("0.12.5"), ("0.12.5", None));
    }

    #[test]
    fn test_split_version_empty() {
        assert_eq!(split_version_and_build_id(""), ("", None));
    }

    #[test]
    fn test_split_version_multiple_plus() {
        // Only the first '+' splits; the remainder (incl. further '+') is the build id.
        assert_eq!(
            split_version_and_build_id("1.0.0+a+b"),
            ("1.0.0", Some("a+b"))
        );
    }

    #[test]
    fn test_split_version_trailing_plus() {
        assert_eq!(split_version_and_build_id("1.0.0+"), ("1.0.0", Some("")));
    }

    // ------------------------------------------------------------------
    // sandbox_mode_name
    // ------------------------------------------------------------------

    fn make_test_sandbox(scope: &std::path::Path) -> sandbox::Sandbox {
        sandbox::Sandbox::new(
            vec![scope.to_path_buf()],
            sandbox::SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn test_sandbox_mode_name_test_mode() {
        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        assert_eq!(sandbox_mode_name(&sb), "DISABLED/TEST");
    }

    #[test]
    fn test_sandbox_mode_name_strict_current_platform() {
        let tmp = tempdir().unwrap();
        let sb = sandbox::Sandbox::new(
            vec![tmp.path().to_path_buf()],
            sandbox::SandboxMode::Strict,
            false,
            false,
            false,
        )
        .unwrap();
        let name = sandbox_mode_name(&sb);
        #[cfg(target_os = "linux")]
        assert_eq!(name, "LANDLOCK");
        #[cfg(target_os = "macos")]
        assert_eq!(name, "SEATBELT");
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(name, "UNSUPPORTED");
    }

    // ------------------------------------------------------------------
    // is_test_or_server_child
    // ------------------------------------------------------------------

    #[test]
    fn test_is_server_child_via_config_flag() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: test-only; ENV_MUTEX serializes env access in this module.
        let prev = std::env::var("AHMA_SERVER_CHILD").ok();
        unsafe { std::env::remove_var("AHMA_SERVER_CHILD") };

        let cfg = AppConfig {
            is_server_child: true,
            ..base_cfg()
        };
        assert!(
            is_test_or_server_child(&cfg),
            "config.is_server_child=true must be detected"
        );

        // SAFETY: test-only; ENV_MUTEX held — restore prior state.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_SERVER_CHILD", v),
                None => std::env::remove_var("AHMA_SERVER_CHILD"),
            }
        }
    }

    #[test]
    fn test_is_server_child_via_env_var() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let prev = std::env::var("AHMA_SERVER_CHILD").ok();
        // SAFETY: test-only; ENV_MUTEX held.
        unsafe { std::env::set_var("AHMA_SERVER_CHILD", "1") };

        let cfg = base_cfg();
        assert!(
            is_test_or_server_child(&cfg),
            "AHMA_SERVER_CHILD set must be detected even when config flag is false"
        );

        // SAFETY: test-only; ENV_MUTEX held — restore prior state.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_SERVER_CHILD", v),
                None => std::env::remove_var("AHMA_SERVER_CHILD"),
            }
        }
    }

    #[test]
    fn test_is_server_child_unset_and_flag_false() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let prev = std::env::var("AHMA_SERVER_CHILD").ok();
        // SAFETY: test-only; ENV_MUTEX held.
        unsafe { std::env::remove_var("AHMA_SERVER_CHILD") };

        let cfg = base_cfg();
        assert!(
            !is_test_or_server_child(&cfg),
            "neither env nor config flag set → false"
        );

        // SAFETY: test-only; ENV_MUTEX held — restore prior state.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AHMA_SERVER_CHILD", v),
                None => std::env::remove_var("AHMA_SERVER_CHILD"),
            }
        }
    }

    // ------------------------------------------------------------------
    // resolve_bridge_endpoints
    // ------------------------------------------------------------------

    /// A test process must NEVER resolve the machine-global bridge socket.
    ///
    /// Regression: a test-spawned ahma inherited `/tmp/ahma.sock`, saw a different
    /// `BUILD_ID` on the bridge that owned it, concluded the bridge was stale, and
    /// POSTed `/restart` — killing the developer's live MCP server (and, since the
    /// socket is shared by design, whatever other application was using it too).
    #[test]
    fn resolve_bridge_endpoints_never_returns_the_global_socket_under_test() {
        let cfg = AppConfig {
            unix_socket_path: String::new(),
            http_host: "127.0.0.1".to_string(),
            http_port: 3000,
            ..base_cfg()
        };
        let (socket, url) = resolve_bridge_endpoints(&cfg);

        assert!(
            is_test_isolated(),
            "precondition: unit tests are test-isolated"
        );
        assert_ne!(
            socket, GLOBAL_SOCKET_PATH,
            "a test must not resolve the shared bridge socket"
        );
        assert!(
            socket.contains(&format!("ahma-test-{}", std::process::id())),
            "expected a per-process private socket, got {socket}"
        );
        assert_eq!(url, "http://127.0.0.1:3000");
    }

    /// Defence in depth: even if a test somehow resolves the shared endpoints, it
    /// is refused the ability to shut that bridge down.
    #[tokio::test]
    async fn trigger_bridge_restart_refuses_the_global_endpoints_under_test_isolation() {
        assert!(
            !trigger_bridge_restart(
                Some(GLOBAL_SOCKET_PATH),
                Some(&format!("http://127.0.0.1:{GLOBAL_HTTP_PORT}"))
            )
            .await,
            "a test must never restart the shared bridge"
        );
    }

    /// ...but a test driving its OWN mock bridge on a private port is legitimate,
    /// so the guard must not be a blanket refusal.
    #[test]
    fn only_the_shared_bridge_port_counts_as_global() {
        assert!(is_global_bridge_url("http://127.0.0.1:3000"));
        assert!(is_global_bridge_url("http://127.0.0.1:3000/"));
        assert!(
            !is_global_bridge_url("http://127.0.0.1:54321"),
            "a mock bridge on a random port is the test's own, not the shared one"
        );
    }

    /// An explicit path still wins — tests that drive their own bridge are unaffected.
    #[test]
    fn resolve_bridge_endpoints_explicit_path_still_wins_under_isolation() {
        let cfg = AppConfig {
            unix_socket_path: "/run/custom/explicit.sock".to_string(),
            ..base_cfg()
        };
        let (socket, _) = resolve_bridge_endpoints(&cfg);
        assert_eq!(socket, "/run/custom/explicit.sock");
    }

    /// Test isolation must NOT imply server-child.
    ///
    /// `is_test_or_server_child` also disarms the parent-death watchdog, which is
    /// what stops a spawned `ahma serve` from outliving the process that started
    /// it. A test-spawned server must keep that watchdog armed, or CI accumulates
    /// orphans. Isolation is applied to the version check instead.
    #[test]
    fn test_isolation_does_not_disarm_the_parent_death_watchdog() {
        let cfg = base_cfg();
        assert!(is_test_isolated(), "precondition: unit tests are isolated");
        assert!(
            !is_test_or_server_child(&cfg),
            "isolation must not be mistaken for server-child: that would disarm \
             the parent-death watchdog and leak orphaned test servers"
        );
    }

    #[test]
    fn test_resolve_bridge_endpoints_custom_socket_and_host() {
        let cfg = AppConfig {
            unix_socket_path: "/run/custom/ahma.sock".to_string(),
            http_host: "0.0.0.0".to_string(),
            http_port: 8080,
            ..base_cfg()
        };
        let (socket, url) = resolve_bridge_endpoints(&cfg);
        assert_eq!(socket, "/run/custom/ahma.sock");
        assert_eq!(url, "http://0.0.0.0:8080");
    }

    // ------------------------------------------------------------------
    // open_capture_file
    // ------------------------------------------------------------------

    #[test]
    fn test_open_capture_file_writes_banner() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("capture.log");
        let banner = "# banner line\n";

        let file = open_capture_file(&path, banner);
        assert!(file.is_some(), "valid path must yield Some(file)");
        drop(file);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents, banner,
            "the banner must be written as the file's first content"
        );
    }

    #[test]
    fn test_open_capture_file_appends_after_banner() {
        use std::io::Write;
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("capture.log");
        let banner = "HEADER\n";

        let mut file = open_capture_file(&path, banner).expect("expected Some(file)");
        // The returned handle is opened in append mode; writes land after the banner.
        file.write_all(b"more\n").unwrap();
        drop(file);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "HEADER\nmore\n");
    }

    #[test]
    fn test_open_capture_file_missing_dir_returns_none() {
        let tmp = tempdir().unwrap();
        // Parent directory does not exist → std::fs::write fails → None.
        let path = tmp.path().join("no_such_dir").join("capture.log");
        assert!(
            open_capture_file(&path, "banner").is_none(),
            "path inside a non-existent directory must return None"
        );
    }

    // ------------------------------------------------------------------
    // bridge health / restart probes against absent or dead endpoints
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_bridge_version_none_endpoints() {
        assert_eq!(get_bridge_version(None, None).await, None);
    }

    #[tokio::test]
    async fn test_check_bridge_running_none_endpoints() {
        assert!(!check_bridge_running(None, None).await);
    }

    #[tokio::test]
    async fn test_trigger_bridge_restart_none_endpoints() {
        assert!(!trigger_bridge_restart(None, None).await);
    }

    #[tokio::test]
    async fn test_query_tcp_health_dead_url() {
        // Port 1 is reserved/unusable; connection fails fast (well under the 200ms cap).
        assert_eq!(query_tcp_health("http://127.0.0.1:1").await, None);
    }

    #[tokio::test]
    async fn test_get_bridge_version_dead_url() {
        assert_eq!(
            get_bridge_version(None, Some("http://127.0.0.1:1")).await,
            None
        );
    }

    #[tokio::test]
    async fn test_trigger_bridge_restart_dead_url() {
        assert!(!trigger_bridge_restart(None, Some("http://127.0.0.1:1")).await);
    }

    #[tokio::test]
    async fn test_get_bridge_version_missing_socket_and_dead_url() {
        // A unix socket path that does not exist fails to connect immediately;
        // combined with a dead URL the result is None.
        let tmp = tempdir().unwrap();
        let socket = tmp.path().join("missing.sock");
        let socket_str = socket.to_string_lossy();
        let result =
            get_bridge_version(Some(socket_str.as_ref()), Some("http://127.0.0.1:1")).await;
        assert_eq!(result, None);
    }

    // ------------------------------------------------------------------
    // build_background_bridge_args — additional branch coverage
    // ------------------------------------------------------------------

    /// Returns the value following `flag`, if present.
    fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(|s| s.as_str())
    }

    #[test]
    fn test_bridge_args_always_includes_serve_and_server_child() {
        let args = build_background_bridge_args(&base_cfg());
        assert_eq!(args[0], "serve");
        assert!(args.contains(&"--server-child".to_string()));
    }

    #[test]
    fn test_bridge_args_numeric_flags_omitted_when_zero() {
        let cfg = AppConfig {
            rate_limit_rps: 0,
            rate_limit_burst: 0,
            handshake_timeout_secs: 0,
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert!(!args.iter().any(|a| a == "--rate-limit-rps"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--rate-limit-burst"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--handshake-timeout"), "{args:?}");
    }

    #[test]
    fn test_bridge_args_numeric_flags_present_when_positive() {
        let cfg = AppConfig {
            rate_limit_rps: 5,
            rate_limit_burst: 7,
            handshake_timeout_secs: 30,
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert_eq!(arg_value(&args, "--rate-limit-rps"), Some("5"));
        assert_eq!(arg_value(&args, "--rate-limit-burst"), Some("7"));
        assert_eq!(arg_value(&args, "--handshake-timeout"), Some("30"));
    }

    #[test]
    fn test_bridge_args_boolean_flags_omitted_by_default() {
        // base_cfg has no_sandbox=true, so check the others which default false.
        let cfg = AppConfig {
            no_sandbox: false,
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        for flag in [
            "--no-sandbox",
            "--skip-probes",
            "--sync",
            "--hot-reload",
            "--defer-sandbox",
            "--scratch",
            "--tmp",
            "--disable-temp-files",
        ] {
            assert!(
                !args.iter().any(|a| a == flag),
                "{flag} must be absent by default: {args:?}"
            );
        }
    }

    #[test]
    fn test_bridge_args_all_boolean_flags_present_when_set() {
        let cfg = AppConfig {
            no_sandbox: true,
            skip_availability_probes: true,
            force_sync: true,
            hot_reload_tools: true,
            defer_sandbox: true,
            use_scratch_dir: true,
            tmp_access: true,
            no_temp_files: true,
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        for flag in [
            "--no-sandbox",
            "--skip-probes",
            "--sync",
            "--hot-reload",
            "--defer-sandbox",
            "--scratch",
            "--tmp",
            "--disable-temp-files",
        ] {
            assert!(
                args.iter().any(|a| a == flag),
                "{flag} must be present when set: {args:?}"
            );
        }
    }

    #[test]
    fn test_bridge_args_idle_timeout_default_when_none() {
        let cfg = AppConfig {
            idle_timeout_secs: None,
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        let expected_idle = AUTO_SPAWNED_BRIDGE_IDLE_TIMEOUT_SECS.to_string();
        assert_eq!(
            arg_value(&args, "--idle-timeout"),
            Some(expected_idle.as_str())
        );
    }

    #[test]
    fn test_bridge_args_idle_timeout_explicit_value() {
        let cfg = AppConfig {
            idle_timeout_secs: Some(123),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert_eq!(arg_value(&args, "--idle-timeout"), Some("123"));
    }

    #[test]
    fn test_bridge_args_idle_timeout_zero_omitted() {
        let cfg = AppConfig {
            idle_timeout_secs: Some(0),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert!(
            !args.iter().any(|a| a == "--idle-timeout"),
            "zero idle-timeout must be omitted: {args:?}"
        );
    }

    #[test]
    fn test_bridge_args_unix_socket_path_forwarded() {
        let cfg = AppConfig {
            unix_socket_path: "/run/ahma/x.sock".to_string(),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert_eq!(
            arg_value(&args, "--unix-socket-path"),
            Some("/run/ahma/x.sock")
        );
    }

    #[test]
    fn test_bridge_args_unix_socket_path_omitted_when_empty() {
        let cfg = AppConfig {
            unix_socket_path: String::new(),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert!(
            !args.iter().any(|a| a == "--unix-socket-path"),
            "empty socket path must be omitted: {args:?}"
        );
    }

    #[test]
    fn test_bridge_args_require_token_and_path() {
        let tmp = tempdir().unwrap();
        let token_path = tmp.path().join("token.txt");
        let cfg = AppConfig {
            require_token: Some("s3cret".to_string()),
            require_token_path: Some(token_path.clone()),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert_eq!(arg_value(&args, "--require-token"), Some("s3cret"));
        let expected_token_path = token_path.to_string_lossy().to_string();
        assert_eq!(
            arg_value(&args, "--require-token-path"),
            Some(expected_token_path.as_str())
        );
    }

    #[test]
    fn test_bridge_args_require_token_omitted_when_none() {
        let cfg = AppConfig {
            require_token: None,
            require_token_path: None,
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert!(!args.iter().any(|a| a == "--require-token"), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "--require-token-path"),
            "{args:?}"
        );
    }

    #[test]
    fn test_bridge_args_instance_label_forwarded() {
        let cfg = AppConfig {
            instance_label: "my-label".to_string(),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert_eq!(arg_value(&args, "--instance-label"), Some("my-label"));
    }

    #[test]
    fn test_bridge_args_instance_label_omitted_when_empty() {
        let cfg = AppConfig {
            instance_label: String::new(),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        assert!(
            !args.iter().any(|a| a == "--instance-label"),
            "empty instance_label must be omitted: {args:?}"
        );
    }

    #[test]
    fn test_bridge_args_task_vault_forwarded() {
        let tmp = tempdir().unwrap();
        let vault = tmp.path().join("vault");
        let cfg = AppConfig {
            task_vault: Some(vault.clone()),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        let expected_vault = vault.to_string_lossy().to_string();
        assert_eq!(
            arg_value(&args, "--task-vault"),
            Some(expected_vault.as_str())
        );
    }

    #[test]
    fn test_bridge_args_task_vault_omitted_when_none() {
        let args = build_background_bridge_args(&base_cfg());
        assert!(!args.iter().any(|a| a == "--task-vault"), "{args:?}");
    }

    #[test]
    fn test_bridge_args_tools_dir_only_when_explicit() {
        let tmp = tempdir().unwrap();
        let tools_dir = tmp.path().join("tools");

        // Set but NOT explicit → omitted.
        let cfg_implicit = AppConfig {
            explicit_tools_dir: false,
            tools_dir: Some(tools_dir.clone()),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg_implicit);
        assert!(
            !args.iter().any(|a| a == "--tools-dir"),
            "implicit tools_dir must not be forwarded: {args:?}"
        );

        // Explicit → forwarded.
        let cfg_explicit = AppConfig {
            explicit_tools_dir: true,
            tools_dir: Some(tools_dir.clone()),
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg_explicit);
        let expected_tools_dir = tools_dir.to_string_lossy().to_string();
        assert_eq!(
            arg_value(&args, "--tools-dir"),
            Some(expected_tools_dir.as_str())
        );
    }

    #[test]
    fn test_bridge_args_tool_bundles_forwarded_per_bundle() {
        let cfg = AppConfig {
            tool_bundles: vec!["cargo".to_string(), "git".to_string()],
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        let tools: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "--tools")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(tools, vec!["cargo", "git"]);
    }

    #[test]
    fn test_bridge_args_working_dirs_forwarded_per_dir() {
        let tmp = tempdir().unwrap();
        let d1 = tmp.path().join("a");
        let d2 = tmp.path().join("b");
        let cfg = AppConfig {
            working_dirs: vec![d1.clone(), d2.clone()],
            ..base_cfg()
        };
        let args = build_background_bridge_args(&cfg);
        let dirs: Vec<String> = args
            .windows(2)
            .filter(|w| w[0] == "--working-dir")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(
            dirs,
            vec![
                d1.to_string_lossy().to_string(),
                d2.to_string_lossy().to_string()
            ]
        );
    }

    // ------------------------------------------------------------------
    // network_enforcement_note
    // ------------------------------------------------------------------

    #[test]
    fn test_network_enforcement_note_matches_current_platform() {
        let note = network_enforcement_note();
        #[cfg(target_os = "macos")]
        assert!(
            note.contains("Kernel-enforced (Seatbelt)"),
            "macOS note must mention Seatbelt enforcement: {note}"
        );
        #[cfg(target_os = "linux")]
        assert!(
            note.contains("Kernel-enforced where supported (Landlock"),
            "Linux note must mention Landlock enforcement: {note}"
        );
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert!(
            note.contains("Advisory"),
            "other platforms must be disclosed as advisory-only: {note}"
        );
        // Regardless of platform, the note is a non-empty, static disclosure string.
        assert!(!note.is_empty());
    }

    // ------------------------------------------------------------------
    // maybe_start_egress_proxy
    // ------------------------------------------------------------------

    fn test_net_approval() -> crate::egress::NetApprovalContext {
        crate::egress::NetApprovalContext {
            coordinator: Arc::new(ahma_common::net_approval::NetApprovalCoordinator::new()),
            peer: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    #[tokio::test]
    async fn test_maybe_start_egress_proxy_off_returns_none() {
        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        let cfg = AppConfig {
            restrict_network: false,
            // Even with allow entries present, restrict_network=false must short-circuit
            // before any proxy is started.
            network_allow: vec!["example.com".to_string()],
            ..base_cfg()
        };

        let proxy = maybe_start_egress_proxy(&cfg, &sb, test_net_approval()).await;
        assert!(
            proxy.is_none(),
            "restrict_network=false must never start the egress proxy"
        );
    }

    #[tokio::test]
    async fn test_maybe_start_egress_proxy_on_empty_allow_starts_proxy() {
        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        let cfg = AppConfig {
            restrict_network: true,
            network_allow: vec![],
            ..base_cfg()
        };

        let proxy = maybe_start_egress_proxy(&cfg, &sb, test_net_approval()).await;
        let proxy = proxy.expect("restrict_network=true must start the egress proxy");
        assert!(
            proxy.local_addr.port() != 0,
            "proxy must bind to a real ephemeral port: {:?}",
            proxy.local_addr
        );
        assert!(proxy.local_addr.ip().is_loopback());
    }

    #[tokio::test]
    async fn test_maybe_start_egress_proxy_on_with_allowlist_starts_proxy() {
        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        let cfg = AppConfig {
            restrict_network: true,
            network_allow: vec!["example.com".to_string(), "*.example.org".to_string()],
            ..base_cfg()
        };

        let proxy = maybe_start_egress_proxy(&cfg, &sb, test_net_approval()).await;
        let proxy =
            proxy.expect("restrict_network=true with a non-empty allowlist must start the proxy");
        assert!(proxy.local_addr.port() != 0);
    }

    // ------------------------------------------------------------------
    // try_setup_mcp_client
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_setup_mcp_client_missing_file_is_ok() {
        let tmp = tempdir().unwrap();
        let cfg = AppConfig {
            mcp_config: tmp.path().join("does_not_exist.json"),
            ..base_cfg()
        };
        let result = try_setup_mcp_client(&cfg).await;
        assert!(
            result.is_ok(),
            "a missing mcp.json must be silently ignored: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_try_setup_mcp_client_invalid_json_is_ok() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, "{ this is not valid json").unwrap();
        let cfg = AppConfig {
            mcp_config: path,
            ..base_cfg()
        };
        let result = try_setup_mcp_client(&cfg).await;
        assert!(
            result.is_ok(),
            "a non-ahma / malformed mcp.json (e.g. Cursor/VSCode config) must not error: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_try_setup_mcp_client_valid_empty_servers_is_ok() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, r#"{"servers": {}}"#).unwrap();
        let cfg = AppConfig {
            mcp_config: path,
            ..base_cfg()
        };
        let result = try_setup_mcp_client(&cfg).await;
        assert!(
            result.is_ok(),
            "a valid ahma mcp.json with no servers must be a no-op: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_try_setup_mcp_client_non_http_server_is_ok() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"servers": {"local": {"type": "child_process", "command": "echo", "args": []}}}"#,
        )
        .unwrap();
        let cfg = AppConfig {
            mcp_config: path,
            ..base_cfg()
        };
        let result = try_setup_mcp_client(&cfg).await;
        assert!(
            result.is_ok(),
            "a child_process server entry must be skipped (only Http is wired), not error: {result:?}"
        );
    }

    /// Regression test: a bridge that outlives the shutdown wait must be reported as
    /// *not stopped*. `restart_bridge_server` used to log "Old bridge stopped." on
    /// this path too, so the timeout was indistinguishable from a clean stop.
    #[tokio::test]
    async fn wait_for_bridge_to_stop_reports_timeout_when_bridge_never_stops() {
        let polls = std::sync::atomic::AtomicUsize::new(0);

        let stopped = wait_for_bridge_to_stop(
            Duration::from_millis(60),
            Duration::from_millis(10),
            async || {
                polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true // still answering health checks, forever
            },
        )
        .await;

        assert!(
            !stopped,
            "a bridge still answering after the timeout must report not-stopped"
        );
        assert!(
            polls.load(std::sync::atomic::Ordering::Relaxed) > 1,
            "the wait must actually poll more than once before giving up"
        );
    }

    #[tokio::test]
    async fn wait_for_bridge_to_stop_reports_success_when_bridge_goes_away() {
        let polls = std::sync::atomic::AtomicUsize::new(0);

        let stopped = wait_for_bridge_to_stop(
            Duration::from_secs(30),
            Duration::from_millis(1),
            async || {
                // Answers twice, then the bridge is gone.
                polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 2
            },
        )
        .await;

        assert!(stopped, "an observed stop must report stopped");
        assert_eq!(
            polls.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "the wait must return on the first check that observes the stop"
        );
    }
}
