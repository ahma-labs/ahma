//! # HTTP Bridge Mode
//!
//! Runs the ahma_mcp server in HTTP bridge mode, which provides an HTTP interface
//! to the MCP server.

use crate::shell::cli::AppConfig;
use anyhow::{Context, Result};
use std::env;

/// Run in HTTP bridge mode.
///
/// # Arguments
/// * `config` - Immutable application configuration.
///
/// # Errors
/// Returns an error if the bridge fails to start.
pub async fn run_http_bridge_mode(config: AppConfig) -> Result<()> {
    use ahma_http_bridge::{BridgeConfig, start_bridge};

    let bind_addr = format!("{}:{}", config.http_host, config.http_port)
        .parse()
        .context("Invalid HTTP host/port")?;

    tracing::info!("Starting HTTP bridge on {}", bind_addr);
    tracing::info!("Session isolation: ENABLED (always-on)");

    // Build the command to run the stdio MCP server subprocess.
    let server_command = env::current_exe()
        .context("Failed to get current executable path")?
        .to_string_lossy()
        .to_string();

    // Determine explicit fallback scope for no-roots clients.
    let explicit_fallback_scope = super::resolve_explicit_fallback_scope(&config);

    let server_args = super::build_stdio_server_args(&config, "--tool", true);

    let enable_colored_output = true;
    tracing::info!(
        "HTTP bridge mode - colored terminal output enabled (v{})",
        env!("CARGO_PKG_VERSION")
    );
    match (&explicit_fallback_scope, config.use_scratch_dir) {
        (Some(scope), false) => tracing::info!(
            "HTTP explicit fallback sandbox scope configured for no-roots clients: {}",
            scope.display()
        ),
        // `--sandbox` with no explicit `--sandbox-scope`: every session on
        // this bridge auto-locks to this fallback directory until its own
        // roots/list overrides it. Never let this substitution happen
        // silently (SPEC R7) — a client that reuses an already-running
        // bridge without checking `/health`'s `default_sandbox_scope` first
        // would otherwise silently execute against the wrong project.
        (Some(scope), true) => tracing::warn!(
            fallback_scope = %scope.display(),
            "HTTP bridge - no --sandbox-scope given; falling back to the --sandbox directory \
             ({}) until a client's roots/list overrides it. Pass --sandbox-scope explicitly if \
             this bridge should be scoped to a specific project.",
            scope.display()
        ),
        (None, true) => tracing::warn!(
            "HTTP bridge - --sandbox was set but no usable sandbox directory could be resolved; \
             sessions will rely entirely on client roots/list to lock their scope."
        ),
        (None, false) => tracing::info!(
            "HTTP strict roots mode: no fallback scope configured; clients must provide roots/list"
        ),
    }

    let bridge_config = BridgeConfig {
        bind_addr,
        server_command,
        server_args,
        enable_colored_output,
        default_sandbox_scope: explicit_fallback_scope,
        handshake_timeout_secs: config.handshake_timeout_secs,
        request_timeout_secs: ahma_http_bridge::session::DEFAULT_REQUEST_TIMEOUT_SECS,
        tool_call_timeout_secs: ahma_http_bridge::session::DEFAULT_TOOL_CALL_TIMEOUT_SECS,
        enable_quic: !config.no_quic,
        disable_http1_1: config.disable_http1_1,
        listener_kind: ahma_http_bridge::ListenerKind::Tcp(bind_addr),
        // Configured auth + rate limiting passed down from AppConfig.
        require_token: config.require_token.clone(),
        require_token_path: config.require_token_path.clone(),
        rate_limit_rps: config.rate_limit_rps,
        rate_limit_burst: config.rate_limit_burst,
        active_sessions: None,
        idle_timeout_secs: config.idle_timeout_secs,
        max_sessions: config.max_sessions,
        peer_factory: None,
        bound_port_tx: None,
        // Explicitly started bridge: it owns its process (SPEC R-DAEMON.1).
        exit: None,
    };

    start_bridge(bridge_config).await?;

    Ok(())
}
