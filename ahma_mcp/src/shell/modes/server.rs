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
    // AppContainer isolation and the egress proxy cannot both be in effect
    // (SPEC R6.3.3.1a). An AppContainer blocks loopback unless the container is
    // registered with `CheckNetIsolation LoopbackExempt`, and this proxy binds
    // 127.0.0.1 — so a sandboxed child cannot reach it.
    //
    // Starting the proxy anyway would be the worst of both outcomes rather than a
    // partial one: a tool that honors HTTP_PROXY fails every request (it cannot
    // connect to the proxy at all), while a tool that ignores HTTP_PROXY reaches
    // the internet completely unrestricted through the container's internet-client
    // capability. The restriction would be broken *and* unenforced, and the
    // interactive-approval path (R-WEB.16.8) could never fire to explain why.
    //
    // The condition is whether AppContainer is *actually in the spawn path*, not
    // whether this is Windows. Those were the same thing when this check was
    // written and stopped being the same thing in #558, which disabled the
    // container without telling this function. For the whole interval a Windows
    // operator who asked for --restrict-network was refused, and told the reason
    // was an AppContainer that was no longer being created — so they had neither a
    // filesystem boundary nor an egress one, which is exactly the state SPEC R7
    // exists to make impossible. One predicate now answers for both sites.
    //
    // Non-fatal, matching the proxy-start failure below: the server still runs. What
    // it must never do is let the operator believe egress is gated when it is not.
    if crate::sandbox::windows::appcontainer_spawn_enabled() {
        tracing::error!(
            "--restrict-network is NOT in effect this session. Every command is launched into a \
             Windows AppContainer (SPEC R6.3.3), which blocks loopback — so the egress proxy \
             would be unreachable by the very subprocesses it exists to gate, and network egress \
             would be unrestricted for any tool that opens its own socket. Pick one: run without \
             --restrict-network (AppContainer filesystem isolation stays), or disable the sandbox \
             with --disable-sandbox to get proxy-based egress gating without it."
        );
        return None;
    }
    // The reachable set is the *union* of what the operator asked for and what
    // each enabled sandbox profile declares its toolchain needs — never one
    // replacing the other. Without the profile half, turning restriction on broke
    // `cargo build` on the first command, which is why almost nobody did.
    // See `egress::host_grants` for why the default nonetheless stays opt-in.
    let grants = crate::egress::EgressGrants::compute(crate::egress::EgressGrantSources {
        operator_allow: &config.network_allow,
        enabled_profiles: &config.sandbox_profiles,
        profile_hosts: config.network_profile_hosts,
        deny_profile_hosts: &config.network_deny_profile_hosts,
    });
    let allowlist = grants.allowlist();
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
    disclose_egress_restriction(&grants, proxy.local_addr);
    Some(proxy)
}

/// Announce the active egress restriction loudly (SPEC R7): what is reachable,
/// who granted it, how strongly it is enforced, and how to opt out. Pure
/// disclosure — no effect on the restriction itself.
fn disclose_egress_restriction(
    grants: &crate::egress::EgressGrants,
    proxy_addr: std::net::SocketAddr,
) {
    if grants.is_empty() {
        tracing::warn!(
            "NETWORK EGRESS RESTRICTED (--restrict-network): nothing is reachable, so ALL \
             subprocess network egress is denied. Add domains to [network] allow in \
             ~/.ahma/settings.toml, or re-enable sandbox profiles (`[sandbox] profiles`, \
             `[network] profile_hosts`) to let each toolchain contribute its own. \
             {enforcement}",
            enforcement = network_enforcement_note(),
        );
        return;
    }
    // Every host names the grant behind it (R-PERM.5.2). A merged anonymous
    // list would tell an operator *that* `proxy.golang.org` is reachable but
    // not that one line in their settings file removes it — a grant whose
    // origin is invisible cannot be refused.
    let opt_out = match grants.contributing_profiles().as_slice() {
        [] => String::new(),
        profiles => format!(
            " Profile-contributed hosts come from: {}; drop them with \
             `[network] deny_profile_hosts` (keeps the toolchain's file access) or all of \
             them with `[network] profile_hosts = false`.",
            profiles.join(", ")
        ),
    };
    tracing::warn!(
        "NETWORK EGRESS RESTRICTED (--restrict-network): sandboxed subprocesses are routed \
         through a guarded proxy at {addr}. Private/loopback/cloud-metadata targets are \
         refused. Reachable hosts, and who granted each:\n{disclosure}\n{enforcement}{opt_out}",
        addr = proxy_addr,
        disclosure = grants.disclosure(),
        enforcement = network_enforcement_note(),
    );
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
    let params = ahma_common::mcp_methods::SandboxTerminatedParams {
        reason: reason.to_string(),
    };
    if let Ok(notification) = serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": ahma_common::mcp_methods::SANDBOX_TERMINATED_METHOD,
        "params": params
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

/// The single graceful-shutdown choreography for every exit path of the stdio
/// server. Both the signal handler ([`run_shutdown_handler`]) and the
/// session-end path (`service.waiting()` returning) run exactly this sequence,
/// so the steps cannot drift apart between exits:
///
/// 1. With `grace` set, wait up to that long for in-flight operations to
///    finish, then cancel whatever remains — each cancellation reaps the
///    operation's full process tree, so no spawned work outlives the server.
///    `None` skips the wait: [`crate::adapter::Adapter::shutdown`] still
///    cancels every tracked operation immediately (used on session end, where
///    the client is gone and nothing will consume a late result).
/// 2. Disclose termination to the client (`notifications/sandbox/terminated`).
/// 3. Shut the adapter down: cancel stragglers, drain task handles, kill
///    persistent session shells.
///
/// The stdio server owns no Unix socket, so there is no socket file to remove
/// here. A shutdown path that *does* own one must remove it only after an
/// identity check (device+inode still ours — SPEC R-ISO.3); that lives with
/// the socket owner, `ahma_http_bridge`.
async fn graceful_shutdown(
    adapter: &Arc<crate::adapter::Adapter>,
    operation_monitor: &Arc<crate::operation_monitor::OperationMonitor>,
    grace: Option<Duration>,
    reason: &str,
) {
    if let Some(grace) = grace {
        let summary = operation_monitor.get_shutdown_summary().await;
        if summary.total_active > 0 {
            wait_for_active_operations(operation_monitor, grace, summary.total_active, reason)
                .await;
        } else {
            info!("OK No active operations - proceeding with immediate shutdown");
        }
    }
    emit_sandbox_terminated(reason);
    adapter.shutdown().await;
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
    graceful_shutdown(
        &adapter,
        &operation_monitor,
        Some(shutdown_timeout),
        shutdown_reason,
    )
    .await;

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

/// Shared client for the two 200 ms bridge probes below.
///
/// Both run in polling loops on the startup critical path — `wait_for_bridge_healthy`
/// and `wait_for_bridge_to_stop` each iterate dozens of times — and a per-probe
/// `Client` meant rebuilding a rustls `ClientConfig`, loading the root store and
/// standing up a fresh pool (and a QUIC endpoint, with http3 enabled) for a single
/// discarded request. Neither probe varies the configuration, so one client serves
/// both; cloning is a refcount bump.
static PROBE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap_or_default()
});

pub async fn query_tcp_health(url: &str) -> Option<BridgeHealth> {
    let health_url = format!("{}/health", url.trim_end_matches('/'));
    let resp = PROBE_CLIENT.get(&health_url).send().await.ok()?;
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
    trigger_uds_restart_with_mode(path, None).await
}

/// POST `/restart` over the Unix socket, optionally with a `mode` query.
///
/// `Some("drain")` asks a daemon to stop accepting new sessions and go once the
/// live ones end, which is how a version handoff avoids ending sessions that
/// belong to other windows (SPEC R-DAEMON.5).
pub async fn trigger_uds_restart_with_mode(path: &str, mode: Option<&str>) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = tokio::net::UnixStream::connect(path);
    let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_millis(200), connect).await else {
        return false;
    };

    let target = match mode {
        Some(mode) => format!("/restart?mode={mode}"),
        None => "/restart".to_string(),
    };
    let request = format!(
        "POST {target} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }

    let mut buf = Vec::with_capacity(512);
    let _ = tokio::time::timeout(Duration::from_millis(200), stream.read_to_end(&mut buf)).await;

    let response_str = std::str::from_utf8(&buf).unwrap_or("");
    response_str.starts_with("HTTP/1.") && response_str.contains(" 200 ")
}

pub async fn trigger_tcp_restart(url: &str) -> bool {
    trigger_tcp_restart_with_mode(url, None).await
}

/// TCP counterpart of [`trigger_uds_restart_with_mode`].
pub async fn trigger_tcp_restart_with_mode(url: &str, mode: Option<&str>) -> bool {
    let restart_url = match mode {
        Some(mode) => format!("{}/restart?mode={mode}", url.trim_end_matches('/')),
        None => format!("{}/restart", url.trim_end_matches('/')),
    };
    if let Ok(resp) = PROBE_CLIENT.post(&restart_url).send().await {
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

/// Drop the machine-global endpoints from a restart request made by a
/// test-isolated process.
///
/// A test must never shut down a bridge it does not own. A test-spawned ahma is
/// a freshly built binary, so it carries a different BUILD_ID; pointed at the
/// machine-global endpoint it decides the running bridge is "stale" and restarts
/// whatever owns it — the developer's live MCP server, or another application's.
///
/// Strips only the *global* endpoints, rather than refusing every restart: a test
/// driving its own mock bridge on a private socket/port is legitimate. Outside
/// test isolation both endpoints pass through untouched.
fn strip_global_endpoints_under_test<'a>(
    socket_path: Option<&'a str>,
    http_url: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>) {
    if !is_test_isolated() {
        return (socket_path, http_url);
    }
    let global_socket = socket_path.is_some_and(|p| p == global_mcp_socket_path());
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
}

pub async fn trigger_bridge_restart(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    trigger_bridge_restart_with_mode(socket_path, http_url, None).await
}

/// Ask the daemon to drain: finish the sessions it has, accept no new ones,
/// then exit (SPEC R-DAEMON.5).
pub async fn trigger_bridge_drain(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    trigger_bridge_restart_with_mode(socket_path, http_url, Some("drain")).await
}

async fn trigger_bridge_restart_with_mode(
    socket_path: Option<&str>,
    http_url: Option<&str>,
    mode: Option<&str>,
) -> bool {
    let (socket_path, http_url) = strip_global_endpoints_under_test(socket_path, http_url);

    #[cfg(unix)]
    if let Some(path) = socket_path
        && trigger_uds_restart_with_mode(path, mode).await
    {
        return true;
    }
    if let Some(url) = http_url
        && trigger_tcp_restart_with_mode(url, mode).await
    {
        return true;
    }
    #[cfg(not(unix))]
    let _ = socket_path;
    false
}

pub async fn check_bridge_running(socket_path: Option<&str>, http_url: Option<&str>) -> bool {
    get_bridge_version(socket_path, http_url).await.is_some()
}

pub fn re_exec_current_process() -> Result<()> {
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

/// Split a version string of the form `"semver+build_id"` into `(semver, Option<build_id>)`.
/// If there is no `+` separator, the build_id portion is `None`.
pub(crate) fn split_version_and_build_id(v: &str) -> (&str, Option<&str>) {
    if let Some(idx) = v.find('+') {
        (&v[..idx], Some(&v[idx + 1..]))
    } else {
        (v, None)
    }
}

/// Sync counterpart of [`open_capture_file`], kept for genuinely sync callers
/// (unit tests below run outside any async runtime).
#[cfg(test)]
fn open_capture_file_sync(path: &std::path::Path, banner: &str) -> Option<std::fs::File> {
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

/// The per-user MCP endpoint the daemon binds (SPEC R-DAEMON.2).
///
/// It is a per-user singleton *by design*: several MCP clients share one daemon.
/// That sharing is exactly why a test must never resolve it — see
/// [`is_test_isolated`] and [`default_socket_path`]. It used to be the
/// machine-global `/tmp/ahma.sock`, which every local user could see and, since
/// nothing owned the path, pre-create; it now lives beside the hub socket in the
/// 0700 per-user runtime directory.
pub fn global_mcp_socket_path() -> String {
    ahma_common::daemon_hub::platform_mcp_socket_path()
        .to_string_lossy()
        .into_owned()
}

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
    // One resolver for both rendezvous files (SPEC R-DAEMON.2, R-ISO.1): the
    // harness fallback keys off the test-run discriminator, not this process's
    // pid, so a test and the binaries it spawns agree on the same private path.
    ahma_common::daemon_hub::mcp_socket_path(None)
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

    // ── The frontend path: a pipe, and nothing else ──────────────────────────
    //
    // An editor spawned us over a stdin/stdout pipe. Everything that actually
    // serves MCP — the adapter, the shell pool, the sandbox — belongs to the
    // per-user daemon and its per-session workers (SPEC R-DAEMON.1), so this
    // process builds none of it and registers nothing with the hub. It used to
    // build a complete service it never served, and register it, which is why a
    // TUI showed a phantom instance that could not run anything.
    if !is_test {
        return run_as_frontend(&config, socket_path_opt, http_url_opt).await;
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
        configs: loaded_configs,
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

    // Register this worker with the hub so a TUI can see its work.
    //
    // The scope starts empty rather than `"."`: the real one is not known until
    // `roots/list` has been answered and the sandbox committed, at which point
    // `publish_committed_scope` re-registers with the truth. Advertising a
    // placeholder made every roots-driven instance invisible to a TUI filtering
    // by project (SPEC R24.3).
    {
        let scope_str = config
            .sandbox_scopes
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let label = config.instance_label.clone();
        crate::daemon_reporter::set_initial_identity(config.session_id.clone(), config.client_pid);
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

    log_startup_summary(&config, &sandbox, loaded_configs.len());

    serve_stdio_until_shutdown(
        service_handler,
        adapter,
        operation_monitor,
        shutdown_timeout,
    )
    .await
}

/// Disclose which sandbox is authoritative for this session, and what it is
/// scoped to, before any tool call can be served (SPEC R7). Pure logging.
fn log_startup_summary(config: &AppConfig, sandbox: &sandbox::Sandbox, loaded_tools: usize) {
    let sandbox_scopes = sandbox
        .scopes()
        .iter()
        .map(|scope| scope.display().to_string())
        .collect::<Vec<_>>();
    tracing::info!(
        "Startup summary: sandbox_mode={}, sandbox_scopes={:?}, disable_temp_files={}, tools_dir={}, loaded_tools={}",
        sandbox_mode_name(sandbox),
        sandbox_scopes,
        sandbox.is_no_temp_files(),
        config
            .tools_dir
            .as_ref()
            .map_or_else(|| "<none>".to_string(), |dir| dir.display().to_string()),
        loaded_tools,
    );
}

/// Serve the built MCP service on this process's stdio until the session ends or
/// a signal arrives, then run the single graceful-shutdown choreography.
///
/// The terminal phase of [`run_server_mode`] when this process *is* the server
/// (server-child or test), as opposed to the frontend that proxies to a detached
/// bridge. The signal handler is armed only after `serve` has taken stdio, so a
/// shutdown can never race the transport into existence.
async fn serve_stdio_until_shutdown(
    service_handler: crate::mcp_service::AhmaMcpService,
    adapter: Arc<crate::adapter::Adapter>,
    operation_monitor: Arc<crate::operation_monitor::OperationMonitor>,
    shutdown_timeout: Duration,
) -> Result<()> {
    let service = service_handler
        .serve(crate::transport_patch::PatchedStdioTransport::new_stdio())
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
    // No grace wait on session end: the client is gone, so nothing will
    // consume a late result — cancel and reap immediately.
    graceful_shutdown(&adapter, &operation_monitor, None, &reason).await;
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
async fn run_as_frontend(
    config: &AppConfig,
    socket_path_opt: Option<&str>,
    http_url_opt: Option<&str>,
) -> Result<()> {
    use crate::shell::modes::daemon_client;

    crate::utils::parent_watchdog::spawn_parent_death_watchdog();

    let outcome =
        daemon_client::ensure_daemon(socket_path_opt, http_url_opt, config.idle_timeout_secs)
            .await?;
    if let Some(notice) = daemon_client::disclosure(&outcome) {
        // R7: never let ahma's own state be something the user has to infer.
        tracing::warn!("{notice}");
    }

    // Respawn hook: if the daemon later dies or its socket vanishes (killed
    // out-of-band, or it exited on idle between our calls), the proxy's
    // reconnect loop brings one back instead of re-dialing a gone endpoint.
    let respawn_bridge: crate::shell::modes::proxy_client::BridgeRespawnFn = {
        let socket_path = socket_path_opt.map(str::to_string);
        let http_url = http_url_opt.map(str::to_string);
        let idle = config.idle_timeout_secs;
        Box::new(move || {
            let socket_path = socket_path.clone();
            let http_url = http_url.clone();
            Box::pin(async move {
                daemon_client::ensure_daemon(socket_path.as_deref(), http_url.as_deref(), idle)
                    .await
                    .map(|_| ())
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
    use parking_lot::Mutex;
    use std::sync::LazyLock;
    use tempfile::tempdir;

    /// Serializes tests that mutate process-global environment variables
    /// (e.g. `AHMA_SERVER_CHILD`). See AGENTS.md env-var test conventions.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn base_cfg() -> AppConfig {
        AppConfig {
            no_sandbox: true,
            restrict_network: false,
            network_allow: vec![],
            // Profile hosts off by default in these tests: each case states the
            // exact reachable set it means to exercise, and a test that silently
            // inherited the shipped toolchain hosts would stop testing its own
            // input. `profile_hosts_seed_the_allowlist` opts back in explicitly.
            network_profile_hosts: false,
            network_deny_profile_hosts: vec![],
            sandbox_profiles: vec![],
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
            daemon_idle_timeout_secs: 60,
            daemon_socket_explicit: false,
            session_id: None,
            client_pid: None,
        }
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
        let _guard = ENV_MUTEX.lock();
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
        let _guard = ENV_MUTEX.lock();
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
        let _guard = ENV_MUTEX.lock();
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
            socket,
            global_mcp_socket_path(),
            "a test must not resolve the shared daemon socket"
        );
        // Keyed by the test-run discriminator, not this pid: a test and the
        // binaries it spawns must agree on one private path (SPEC R-ISO.1).
        assert!(
            socket.contains(&format!(
                "ahma-test-mcp-{}",
                ahma_common::test_isolation::test_run_discriminator()
            )),
            "expected the per-run private socket, got {socket}"
        );
        assert_eq!(url, "http://127.0.0.1:3000");
    }

    /// Defence in depth: even if a test somehow resolves the shared endpoints, it
    /// is refused the ability to shut that bridge down.
    #[tokio::test]
    async fn trigger_bridge_restart_refuses_the_global_endpoints_under_test_isolation() {
        assert!(
            !trigger_bridge_restart(
                Some(&global_mcp_socket_path()),
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

        let file = open_capture_file_sync(&path, banner);
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

        let mut file = open_capture_file_sync(&path, banner).expect("expected Some(file)");
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
            open_capture_file_sync(&path, "banner").is_none(),
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
            peer: Arc::new(parking_lot::RwLock::new(None)),
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

    /// The egress proxy is refused when — and only when — AppContainer isolation
    /// is genuinely in the spawn path (SPEC R6.3.3.1a: an AppContainer blocks
    /// loopback, so the proxy would be unreachable by the very subprocesses it
    /// exists to gate).
    ///
    /// This asserts the *predicate*, not the platform. The previous version of
    /// this test asserted `#[cfg(windows)] => None`, which pinned the behaviour in
    /// place after #558 disabled the container: Windows kept refusing
    /// `--restrict-network` for a reason that had stopped being true, and the test
    /// certified it. Keying on `appcontainer_spawn_enabled()` means the day that
    /// flips to `true`, this expectation flips with it — and until then Windows
    /// gets the same egress gating as everywhere else.
    #[tokio::test]
    async fn test_egress_proxy_refusal_tracks_appcontainer_not_the_platform() {
        assert!(
            !crate::sandbox::windows::appcontainer_spawn_enabled(),
            "appcontainer_spawn_enabled() is true, so this test's sibling — the \
             proxy-does-start assertions below — no longer describes any platform. \
             Re-key them on the predicate before flipping it (SPEC R6.3.3.1a)."
        );

        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        let cfg = AppConfig {
            restrict_network: true,
            network_allow: vec!["example.com".to_string()],
            ..base_cfg()
        };

        let proxy = maybe_start_egress_proxy(&cfg, &sb, test_net_approval()).await;
        assert!(
            proxy.is_some(),
            "with AppContainer out of the spawn path there is nothing blocking loopback, \
             so --restrict-network must take effect rather than being refused (SPEC R7: \
             ahma never silently disables enforcement, and never refuses it for a reason \
             that does not apply)"
        );
    }

    // The four "proxy does start" tests below were `#[cfg(not(windows))]` for as
    // long as `maybe_start_egress_proxy` refused on Windows unconditionally. It
    // now refuses only when AppContainer is genuinely in the spawn path, so they
    // describe every platform and are gated on none. If AppContainer is ever
    // re-enabled, the assertion at the top of
    // `test_egress_proxy_refusal_tracks_appcontainer_not_the_platform` fires and
    // says so, rather than these four failing with no explanation.
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
        assert!(proxy.allows("example.com"));
        assert!(proxy.allows("api.example.org"));
        assert!(!proxy.allows("elsewhere.example"));
    }

    #[tokio::test]
    async fn profile_hosts_seed_the_allowlist() {
        // The wiring test for the whole feature: `--restrict-network` with *no*
        // hand-written `[network] allow` must still let cargo, npm and go reach
        // their registries, because the enabled sandbox profiles said so. Before
        // this, the same configuration denied everything, and the first command
        // an operator ran after enabling restriction failed — which is why
        // essentially nobody enabled it.
        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        let cfg = AppConfig {
            restrict_network: true,
            network_allow: vec![],
            network_profile_hosts: true,
            sandbox_profiles: crate::sandbox::profiles::default_profile_names(),
            ..base_cfg()
        };

        let proxy = maybe_start_egress_proxy(&cfg, &sb, test_net_approval())
            .await
            .expect("restriction on must start the proxy");
        for host in ["index.crates.io", "registry.npmjs.org", "proxy.golang.org"] {
            assert!(
                proxy.allows(host),
                "{host} must be reachable from the shipped profiles alone"
            );
        }
        assert!(
            !proxy.allows("evil.example"),
            "seeding the allowlist must not widen it to everything"
        );
    }

    #[tokio::test]
    async fn operator_allow_composes_with_profile_hosts_at_the_proxy() {
        // Guards the natural-but-wrong implementation: "if the operator wrote an
        // allowlist, use theirs". That would break every toolchain the moment an
        // operator added one internal host of their own.
        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        let cfg = AppConfig {
            restrict_network: true,
            network_allow: vec!["artifacts.internal.example".to_string()],
            network_profile_hosts: true,
            sandbox_profiles: vec!["rust".to_string()],
            ..base_cfg()
        };

        let proxy = maybe_start_egress_proxy(&cfg, &sb, test_net_approval())
            .await
            .expect("restriction on must start the proxy");
        assert!(
            proxy.allows("artifacts.internal.example"),
            "operator's host"
        );
        assert!(proxy.allows("index.crates.io"), "…and the rust profile's");
        assert!(
            !proxy.allows("registry.npmjs.org"),
            "…but only the profiles that are enabled"
        );
    }

    #[tokio::test]
    async fn withholding_profile_hosts_leaves_the_operators_own_list_intact() {
        // `[network] profile_hosts = false` is a hardening knob, not a kill
        // switch: it must not take the operator's own entries down with it.
        let tmp = tempdir().unwrap();
        let sb = make_test_sandbox(tmp.path());
        let cfg = AppConfig {
            restrict_network: true,
            network_allow: vec!["artifacts.internal.example".to_string()],
            network_profile_hosts: false,
            sandbox_profiles: crate::sandbox::profiles::default_profile_names(),
            ..base_cfg()
        };

        let proxy = maybe_start_egress_proxy(&cfg, &sb, test_net_approval())
            .await
            .expect("restriction on must start the proxy");
        assert!(proxy.allows("artifacts.internal.example"));
        assert!(!proxy.allows("index.crates.io"));
        assert_eq!(
            proxy.allowlist_entries(),
            vec!["artifacts.internal.example"]
        );
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
        assert!(result.is_ok(), "an empty servers map is fine: {result:?}");
    }
}
