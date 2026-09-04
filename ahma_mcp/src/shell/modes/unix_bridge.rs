//! # Unix Bridge Mode
//!
//! Runs the ahma_mcp server in HTTP-over-Unix-Socket bridge mode.
//! Clients connect via a Unix domain socket (UDS) using standard
//! MCP Streamable HTTP framing; no TCP port is opened.
//!
//! This mode is Unix-only (`#[cfg(unix)]`).

use crate::shell::cli::AppConfig;
use ahma_http_bridge::{BridgeConfig, ListenerKind, start_bridge};
use anyhow::{Context, Result};
use std::env;

/// Run in Unix domain socket bridge mode.
///
/// # Arguments
/// * `config` - Immutable application configuration.
///
/// # Errors
/// Returns an error if the bridge fails to start.
pub async fn run_unix_bridge_mode(config: AppConfig) -> Result<()> {
    // Per-user runtime dir, not the retired machine-global /tmp/ahma.sock
    // (SPEC R-DAEMON.2); an explicit --socket-path still wins.
    let socket_path = ahma_common::daemon_hub::mcp_socket_path(
        Some(config.unix_socket_path.as_str()).filter(|p| !p.is_empty()),
    );

    tracing::info!("Starting Unix socket bridge on {}", socket_path);
    tracing::info!("Session isolation: ENABLED (always-on)");

    let server_command = env::current_exe()
        .context("Failed to get current executable path")?
        .to_string_lossy()
        .to_string();

    let explicit_fallback_scope = super::resolve_explicit_fallback_scope(&config);

    let server_args = super::build_stdio_server_args(&config, "--tools", false);

    let enable_colored_output = true;

    match (&explicit_fallback_scope, config.use_scratch_dir) {
        (Some(scope), false) => tracing::info!(
            "Unix socket bridge mode - explicit fallback sandbox scope: {}",
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
            "Unix socket bridge mode - no --sandbox-scope given; falling back to the --sandbox \
             directory ({}) until a client's roots/list overrides it. Pass --sandbox-scope \
             explicitly if this bridge should be scoped to a specific project.",
            scope.display()
        ),
        (None, true) => tracing::warn!(
            "Unix socket bridge mode - --sandbox was set but no usable sandbox directory could \
             be resolved; sessions will rely entirely on client roots/list to lock their scope."
        ),
        (None, false) => tracing::info!(
            "Unix socket bridge mode - strict roots mode: no fallback scope configured"
        ),
    }

    // Derive a dummy bind_addr (unused when ListenerKind::Unix is set).
    let bind_addr = "127.0.0.1:0".parse().unwrap();

    let bridge_config = BridgeConfig {
        bind_addr,
        server_command,
        server_args,
        enable_colored_output,
        default_sandbox_scope: explicit_fallback_scope,
        handshake_timeout_secs: config.handshake_timeout_secs,
        request_timeout_secs: ahma_http_bridge::session::DEFAULT_REQUEST_TIMEOUT_SECS,
        tool_call_timeout_secs: ahma_http_bridge::session::DEFAULT_TOOL_CALL_TIMEOUT_SECS,
        // QUIC is UDP-based and incompatible with Unix sockets.
        enable_quic: false,
        disable_http1_1: false,
        listener_kind: ListenerKind::Unix(socket_path),
        require_token: None,
        require_token_path: None,
        rate_limit_rps: 0,
        rate_limit_burst: 10,
        active_sessions: None,
        idle_timeout_secs: config.idle_timeout_secs,
        max_sessions: config.max_sessions,
        peer_factory: None,
        bound_port_tx: None,
    };

    start_bridge(bridge_config).await?;

    Ok(())
}
