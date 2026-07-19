//! # Ahma Core
//!
//! Ahma (Finnish for wolverine) is the foundational engine for building high-performance,
//! secure Model Context Protocol (MCP) servers. This crate provides the core library that
//! powers all Ahma interfaces, including the standard `ahma` binary (Stdio/CLI) and
//! the `ahma-http-bridge`.
//!
//! ## Foundational Philosophy
//!
//! Ahma is designed to bridge the gap between AI agents and the vast ecosystem of
//! command-line utilities. It treats CLI tools as first-class capabilities, wrapping them
//! in a secure, non-blocking concurrent execution environment.
//!
//! ## Core Architectural Pillars
//!
//! 1. **Kernel-Level Security**: Ahma is built on the principle that AI agents should never
//!    run unconstrained. It uses OS-native mechanisms (Landlock on Linux, Seatbelt on macOS)
//!    to enforce strict filesystem boundaries that are immutable once the session starts.
//!
//! 2. **Async-First Execution**: Long-running operations like builds or tests shouldn't
//!    block the agent's thought process. Ahma returns operation IDs immediately and
//!    pushes results back via notifications when complete.
//!
//! 3. **Persistent Shell Sessions**: Stateful PTY shell sessions
//!    ([`ShellSessionManager`](crate::shell_session::ShellSessionManager)) let agents keep
//!    environment state (cwd, exported variables) across commands when they opt in via
//!    `session_id`.
//!
//! ## Practical Integration Guide
//!
//! For developers building on top of this library, the two primary components are the
//! [`Adapter`] and the [`AhmaMcpService`].
//!
//! ### Initializing the Engine
//!
//! ```rust,no_run
//! use ahma_mcp::{Adapter, AhmaMcpService, config::ToolConfig};
//! use ahma_mcp::operation_monitor::{OperationMonitor, MonitorConfig};
//! use ahma_mcp::shell_pool::{ShellPoolManager, ShellPoolConfig};
//! use ahma_mcp::sandbox::{Sandbox, SandboxMode};
//! use std::sync::Arc;
//! use std::collections::HashMap;
//! use std::path::PathBuf;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // 1. Initialize core tracking and performance components
//!     let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(Duration::from_secs(300))));
//!     let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
//!     let sandbox = Arc::new(Sandbox::new(Vec::new(), SandboxMode::Strict, false, false, false)?);
//!
//!     // 2. Create the execution adapter (with default cargo mutex group)
//!     let mutex_groups = ahma_common::config::default_mutex_groups();
//!     let mutex_registry = Arc::new(Adapter::mutex_registry_from(&mutex_groups));
//!     let adapter = Arc::new(Adapter::new(monitor.clone(), shell_pool, sandbox, mutex_registry)?);
//!
//!     // 3. Initialize the MCP service with your tool configurations
//!     let configs = Arc::new(HashMap::<String, ToolConfig>::new());
//!     let service = AhmaMcpService::new(
//!         adapter,
//!         monitor,
//!         configs,
//!         Arc::new(None), // Guidance
//!         false, // force_synchronous
//!         false, // defer_sandbox
//!     ).await?;
//!
//!     // Now you can run the service over Stdio or an HTTP transport.
//!     Ok(())
//! }
//! ```
//!
//! ## Environment Variables (Retired)
//!
//! `AHMA_*` environment variables are **retired** as a configuration source
//! (SPEC R-CFG1.2). Setting one produces a startup `warn` naming it, and the
//! value is otherwise **ignored** — configuration comes from CLI flags and
//! `~/.ahma/settings.toml` instead (`--no-settings` skips the settings file).
//! Replacements for the previously documented variables:
//!
//! | Retired variable | Replacement |
//! |---|---|
//! | `AHMA_DISABLE_SANDBOX` | `--no-sandbox` (CLI-flag-only, R-CFG2.3) |
//! | `AHMA_SANDBOX_SCOPE` | `--sandbox-scope <PATH>` |
//! | `AHMA_SANDBOX_DEFER` | `--defer-sandbox` |
//! | `AHMA_WORKING_DIRS` | `--working-dir <PATH>` |
//! | `AHMA_TMP_ACCESS` | `--tmp` |
//! | `AHMA_DISABLE_TEMP` | `--disable-temp-files` |
//! | `AHMA_TOOLS_DIR` | `--tools-dir <PATH>` |
//! | `AHMA_TIMEOUT` / `AHMA_SYNC` / `AHMA_HOT_RELOAD` / `AHMA_SKIP_PROBES` | `--timeout <SECS>` / `--sync` / `--hot-reload` / `--skip-probes` |
//! | `AHMA_LOG_TARGET` | `--log-to-stderr` or `[logging] target` in settings.toml |
//! | `AHMA_LOG_MONITOR` / `AHMA_MONITOR_RATE_LIMIT` | `--log-monitor` / `--monitor-rate-limit <SECS>` |
//! | `AHMA_DISABLE_QUIC` / `AHMA_DISABLE_HTTP1_1` / `AHMA_HANDSHAKE_TIMEOUT` | CLI flags & settings keys |
//!
//! Platform/ecosystem-standard variables such as `RUST_LOG` remain honored
//! (R-CFG1.3 allowlist), and internal parent→child plumbing variables are
//! documented as `INTERNAL` in `docs/environment-variables.md`.
//!
//! See [`shell::cli::AppConfig`] for the resolved configuration structure, and the
//! project's `docs/environment-variables.md` for the full retirement table.
//!
//! ## Module Overview
//!
//! - **[`adapter`]**: The "heavy lifter" that coordinates shell processes and task monitors.
//! - **[`mcp_service`]**: The protocol layer implementing `rmcp` handlers for the MCP standard.
//! - **[`sandbox`]**: Platform-agnostic security enforcement using kernel features.
//! - **[`config`]**: Support for the Multi-Tool Definition Format (MTDF) JSON schema.
//! - **[`operation_monitor`]**: Real-time tracking and control (cancellation/status) of background tasks.
//! - **[`shell_pool`]**: The performance engine that keeps shells warm and ready.

// Public modules
/// Core adapter for tool execution.
pub mod adapter;
mod check_service_ext;
/// Client helpers for talking to Ahma.
pub mod client;
/// Client type helpers and compatibility flags.
pub mod client_type;
/// Tool configuration models and loaders.
pub mod config;
/// Constants used for guidance and tool hints.
pub mod constants;
/// Background reporter that registers this instance with the hub daemon.
pub mod daemon_reporter;
/// File operations provider.
pub mod file_ops;
/// External terminal hook management for supported AI tools.
pub mod hooks;
/// Live log monitoring pipeline (LLM-powered issue detection).
pub mod livelog;
/// LLM Completion Service provider.
pub mod llm_service;
/// Live log monitoring for streaming processes.
pub mod log_monitor;
/// MCP client connection manager and auto-discovery.
pub mod mcp_client;
/// MCP server implementation.
pub mod mcp_service;
/// Operation monitor for async tasks.
pub mod operation_monitor;
/// Path security checks for sandbox enforcement.
pub mod path_security;
/// Retry policies and helpers.
pub mod retry;
/// Sandbox configuration and enforcement.
pub mod sandbox;
/// JSON schema validation utilities.
pub mod schema_validation;
/// Unified service builder for all transport modes.
pub mod service_builder;
pub mod session_events;
/// Setup wizard for MCP, hooks, TLS, and skills.
pub mod setup;
/// CLI shell entry points.
pub mod shell;
/// Shell pooling and execution.
pub mod shell_pool;
/// Persistent stateful shell sessions (`session_id`).
pub mod shell_session;
/// Code complexity analysis and simplification tooling.
#[cfg(feature = "simplify")]
pub mod simplify;
/// Terminal output helpers for callbacks.
pub mod terminal_output;
/// Tool availability checks and guidance.
pub mod tool_availability;
/// Tool hint formatting.
pub mod tool_hints;
/// Transport patching for stdio MCP.
pub mod transport_patch;
/// Uninstall wizard (symmetric teardown of `ahma setup`).
pub mod uninstall;
/// Self-update: release downloads and Git branch installs.
pub mod update;
/// Shared utilities.
pub mod utils;
/// Tool configuration validation.
pub mod validation;

// ── New modules (roadmap milestones) ─────────────────────────────────────────
//
// The following milestone modules have been extracted into dedicated
// AGPL-3.0-or-later crates to allow this library to
// remain MIT OR Apache-2.0:
//
//   ahma_vault     — task vault, audit log, two-phase trash  (AGPL-3.0-or-later)
//   ahma_task_tree — recursive task-tree orchestration       (AGPL-3.0-or-later)
//   ahma_tui       — ratatui TUI control plane               (AGPL-3.0-or-later)
//   ahma_cluster   — local cluster scheduler (mDNS/QUIC)     (AGPL-3.0-or-later)
//
// These crates live in the same workspace and depend on this library;
// they must NOT be depended on from this crate.

/// Egress sandbox: per-task HTTP proxy with domain allowlist.
pub mod egress;

/// Bundle signing and supply-chain auditor.
pub mod bundle;

/// Token minimization and output optimization.
pub mod output_optimizer;

/// Harness guards and small-model adaptations.
pub mod harness_guard;

// Test utilities
/// Test helpers for integration and unit tests.
pub mod test_utils;

/// Task vault: audit logging and trash.
pub mod vault;

// Re-export main types for easier use
pub use adapter::Adapter;

pub use adapter::executor::{CommandExecutor, DefaultCommandExecutor};
pub use file_ops::{
    DefaultFileOpsProvider, DefaultWebPageFetcher, FileOpsProvider, WebPageFetcher,
};
pub use llm_service::{DefaultLlmCompletionService, LlmCompletionService};
pub use mcp_client::{
    McpClientHandler, McpConnectionManager, McpServerConfig, McpServerKind, ToolInfo,
};
pub use mcp_service::AhmaMcpService;
pub use mcp_service::{
    ActiveAgentSession, ExtensionToolHandler, PromptRunner, get_global_prompt_runner,
    register_global_extension_handler, register_global_prompt_runner,
};

static ACTIVE_SERVICE: std::sync::OnceLock<std::sync::Arc<AhmaMcpService>> =
    std::sync::OnceLock::new();

/// Register the active MCP service instance.
pub fn register_active_service(service: std::sync::Arc<AhmaMcpService>) {
    let _ = ACTIVE_SERVICE.set(service);
}

/// Retrieve the active MCP service instance.
pub fn get_active_service() -> Option<std::sync::Arc<AhmaMcpService>> {
    ACTIVE_SERVICE.get().cloned()
}
