//! # Server Modes Module
//!
//! Contains the different operational modes for the ahma_mcp server.

pub mod cli;
pub mod daemon;
pub mod daemon_client;
pub mod http_bridge;
pub mod list_tools;
pub mod proxy_client;
pub mod server;
pub mod session_options;
#[cfg(unix)]
pub mod unix_bridge;

// Re-export mode functions for convenience
pub use cli::run_cli_mode;
pub use http_bridge::run_http_bridge_mode;
pub use list_tools::run_list_tools_mode;
pub use proxy_client::run_proxy_client;
pub use server::run_server_mode;
#[cfg(unix)]
pub use unix_bridge::run_unix_bridge_mode;

use crate::shell::cli::AppConfig;
use std::path::PathBuf;

/// Determine the explicit fallback sandbox scope for no-roots clients, shared
/// by both bridge modes.
///
/// SECURITY: only treat CLI/env as explicit fallback; do not silently use CWD.
/// When `--sandbox` is set (but no explicit `--sandbox-scope`), the scratch
/// directory (e.g. `~/sandbox`) is the fallback scope for clients that don't
/// send roots/list (e.g. Antigravity).
pub(crate) fn resolve_explicit_fallback_scope(config: &AppConfig) -> Option<PathBuf> {
    if !config.sandbox_scopes.is_empty() {
        Some(
            dunce::canonicalize(&config.sandbox_scopes[0])
                .unwrap_or_else(|_| config.sandbox_scopes[0].clone()),
        )
    } else if config.use_scratch_dir {
        config
            .scratch_directory
            .as_ref()
            .and_then(|dir| ahma_common::config::ensure_sandbox_directory(dir).ok())
    } else {
        None
    }
}

/// Build the argv for the `serve stdio` subprocess a bridge spawns, forwarding
/// the bridge's own global options.
///
/// The two bridge modes have deliberately divergent tails, made explicit as
/// parameters rather than re-duplicated: `bundle_flag` is `--tool` on the HTTP
/// bridge but `--tools` on the Unix bridge, and only the HTTP bridge forwards
/// `--task-vault` (`forward_task_vault`).
pub(crate) fn build_stdio_server_args(
    config: &AppConfig,
    bundle_flag: &str,
    forward_task_vault: bool,
) -> Vec<String> {
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

    if forward_task_vault && let Some(ref task_vault) = config.task_vault {
        server_args.push("--task-vault".to_string());
        server_args.push(task_vault.to_string_lossy().to_string());
    }

    server_args.push("stdio".to_string());

    // Pass through tool bundle selection
    for bundle in &config.tool_bundles {
        server_args.push(bundle_flag.to_string());
        server_args.push(bundle.clone());
    }

    server_args
}
