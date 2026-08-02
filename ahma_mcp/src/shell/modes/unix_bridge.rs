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
use dunce;
use std::env;

/// Run in Unix domain socket bridge mode.
///
/// # Arguments
/// * `config` - Immutable application configuration.
///
/// # Errors
/// Returns an error if the bridge fails to start.
pub async fn run_unix_bridge_mode(config: AppConfig) -> Result<()> {
    let socket_path = if config.unix_socket_path.is_empty() {
        "/tmp/ahma.sock".to_string()
    } else {
        config.unix_socket_path.clone()
    };

    tracing::info!("Starting Unix socket bridge on {}", socket_path);
    tracing::info!("Session isolation: ENABLED (always-on)");

    let server_command = env::current_exe()
        .context("Failed to get current executable path")?
        .to_string_lossy()
        .to_string();

    let explicit_fallback_scope = if !config.sandbox_scopes.is_empty() {
        Some(
            dunce::canonicalize(&config.sandbox_scopes[0])
                .unwrap_or_else(|_| config.sandbox_scopes[0].clone()),
        )
    } else if config.use_scratch_dir {
        // When --sandbox is set (but no explicit --sandbox-scope), use ~/sandbox as the
        // fallback scope for clients that don't send roots/list (e.g. Antigravity).
        config
            .scratch_directory
            .as_ref()
            .and_then(|dir| ahma_common::config::ensure_sandbox_directory(dir).ok())
    } else {
        None
    };

    // Subprocess gets the `serve stdio` subcommand.
    let mut server_args = vec!["serve".to_string()];

    // Pass global options to child process
    if config.no_sandbox {
        server_args.push("--no-sandbox".to_string());
    }
    if config.use_scratch_dir {
        server_args.push("--sandbox".to_string());
    }
    if config.tmp_access {
        server_args.push("--tmp".to_string());
    }
    if config.log_monitor {
        server_args.push("--log-monitor".to_string());
    }
    server_args.push("--monitor-rate-limit".to_string());
    server_args.push(config.monitor_rate_limit_secs.to_string());
    server_args.push("--timeout".to_string());
    server_args.push(config.timeout_secs.to_string());
    if config.force_sync {
        server_args.push("--sync".to_string());
    }
    if config.no_temp_files {
        server_args.push("--disable-temp-files".to_string());
    }
    if config.hot_reload_tools {
        server_args.push("--hot-reload".to_string());
    }
    if config.skip_availability_probes {
        server_args.push("--skip-probes".to_string());
    }
    if let Some(ref otel_ep) = config.observability.endpoint {
        server_args.push("--opentelemetry".to_string());
        server_args.push(otel_ep.clone());
    }
    for scope in &config.sandbox_scopes {
        server_args.push("--sandbox-scope".to_string());
        server_args.push(scope.to_string_lossy().to_string());
    }
    for dir in &config.working_dirs {
        server_args.push("--working-dir".to_string());
        server_args.push(dir.to_string_lossy().to_string());
    }

    if config.explicit_tools_dir
        && let Some(ref tools_dir) = config.tools_dir
    {
        server_args.push("--tools-dir".to_string());
        server_args.push(tools_dir.to_string_lossy().to_string());
    }

    server_args.push("stdio".to_string());

    for bundle in &config.tool_bundles {
        server_args.push("--tools".to_string());
        server_args.push(bundle.clone());
    }

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
        cluster_shared_key: None,
        peer_factory: None,
        bound_port_tx: None,
    };

    start_bridge(bridge_config).await?;

    Ok(())
}
