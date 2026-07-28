//! # HTTP Bridge Mode
//!
//! Runs the ahma_mcp server in HTTP bridge mode, which provides an HTTP interface
//! to the MCP server.

use crate::shell::cli::AppConfig;
use anyhow::{Context, Result};
use dunce;
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
    // SECURITY: only treat CLI/env as explicit fallback; do not silently use CWD.
    let explicit_fallback_scope = if !config.sandbox_scopes.is_empty() {
        Some(
            dunce::canonicalize(&config.sandbox_scopes[0])
                .unwrap_or_else(|_| config.sandbox_scopes[0].clone()),
        )
    } else if config.use_sandbox_dir {
        config
            .sandbox_directory
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
    if config.use_sandbox_dir {
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

    // Pass --tools-dir only if explicitly provided (otherwise subprocess auto-detects)
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

    // Pass through tool bundle selection
    for bundle in &config.tool_bundles {
        server_args.push("--tool".to_string());
        server_args.push(bundle.clone());
    }

    let enable_colored_output = true;
    tracing::info!(
        "HTTP bridge mode - colored terminal output enabled (v{})",
        env!("CARGO_PKG_VERSION")
    );
    match (&explicit_fallback_scope, config.use_sandbox_dir) {
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
        cluster_shared_key: None,
        peer_factory: None,
        bound_port_tx: None,
    };

    start_bridge(bridge_config).await?;

    Ok(())
}
