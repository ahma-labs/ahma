//! # ahma_tui — Terminal dashboard and chat interface
//!
//! A `ratatui`-based terminal UI with two modes:
//!
//! * **Chat mode** (default) — multi-line input sends messages to a local LLM;
//!   `/` opens a Claude Code-style command navigator for all ahma features.
//! * **Monitor mode** — 4-pane dashboard for watching active operations,
//!   viewing logs, and handling approval gates.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

pub mod agent_config;
pub mod app;
pub mod connection;
pub mod daemon_source;
pub mod keymap;
pub mod llm_bridge;
pub mod mcp_connections;
pub mod mcp_source;
pub mod session_config;
pub mod settings_editor;
pub mod state;
pub mod task_tree;
pub mod theme;
pub mod ui;

pub use connection::{ResolvedConnection, ResolvedTransport};

use anyhow::Result;

/// CLI-resolved token/context preferences for the local-LLM chat agent.
///
/// `Some(true)` / `Some(false)` are explicit on/off from the
/// `--minimize-tokens` / `--no-minimize-tokens` (and small-model-harness)
/// flags; `None` falls back to settings.toml, then the deprecated env vars.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenPrefs {
    pub minimize_tokens: Option<bool>,
    pub small_model_harness: Option<bool>,
    /// Model context window in tokens (`--context-length`).  Sizes the
    /// conversation and tool-result budgets for small local models.
    pub context_length: Option<u32>,
}

/// Run the TUI event loop, blocking until the user quits.
///
/// * `connect` — explicit `--connect` URL, or `None` to auto-probe local
///   transports (Unix socket first on Unix, then `http://localhost:3000`).
pub async fn run_tui(
    connect: Option<&str>,
    profile: Option<String>,
    path: Option<std::path::PathBuf>,
    token_prefs: TokenPrefs,
) -> Result<()> {
    if connect.is_none()
        && let Err(e) = connection::ensure_server_running(path.as_deref()).await
    {
        tracing::warn!("Could not ensure server is running: {}", e);
    }
    let connection = connection::resolve_connection(connect).await?;
    app::run(&connection, profile, path, token_prefs).await
}
