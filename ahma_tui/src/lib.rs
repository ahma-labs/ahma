//! # ahma_tui — Terminal dashboard and chat interface
//!
//! A `ratatui`-based terminal UI providing a unified chat and dashboard interface:
//! multi-line input sends messages to a local LLM, `/` opens a command
//! navigator, and slash commands (e.g. `/tasks`, `/log`) open full-width sub-window views.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0**.

pub mod accordion;
pub mod agent_config;
pub mod app;
pub mod connection;
pub mod daemon_source;
pub mod keymap;
pub mod liveness;
pub mod llm_bridge;
pub mod mcp_connections;
pub mod mcp_source;
pub mod session_config;
pub mod settings_editor;
pub mod startup_notices;
pub mod state;
pub mod task_tree;
mod terminal_guard;
pub mod theme;
pub mod tui_reporter;
pub mod ui;
pub mod work_view;

pub use connection::{ResolvedConnection, ResolvedTransport};

use anyhow::Result;

/// Serializes tests that redirect the home directory through the
/// `AHMA_TEST_HOME` debug seam.
///
/// Environment variables are process-global. `cargo nextest` gives each test
/// its own process, so these tests are isolated there — but under plain
/// `cargo test` they share one process and race: two tests pointing the seam at
/// different temp directories make one of them save into the other's, and the
/// first to call `remove_var` unsets it for both. Taking this lock costs
/// nothing under nextest and makes the suite correct under either runner.
#[cfg(test)]
pub static HOME_SEAM_GUARD: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

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
    // Set by the previous process image when it re-exec'd itself to match a
    // newer running daemon (see `daemon_client::ensure_daemon`). The
    // user saw the screen flicker and was never told why; the restarted
    // process is the only one that can still say so.
    if std::env::var("AHMA_RESTARTED").is_ok() {
        startup_notices::push(
            startup_notices::Level::Warn,
            "This TUI restarted itself to match the newer ahma daemon already running.",
        );
    }

    // Pre-flight the directory this TUI will answer `roots/list` with, so a
    // typo or a `$HOME` launch is named here rather than surfacing later as a
    // per-session sandbox rejection (SPEC R5.2.4, R5.2.1.1).
    if let Err(e) = connection::preflight_scope(path.as_deref()) {
        startup_notices::push(startup_notices::Level::Warn, format!("{e:#}"));
    }

    if connect.is_none() {
        // Ensure the one per-user daemon exists. The TUI does **not** start a
        // server of its own, and above all does not start one scoped to its
        // launch directory: that used to lock every editor session that
        // arrived afterwards to whichever folder a terminal happened to be in
        // (SPEC R-DAEMON.9).
        let socket = ahma_common::daemon_hub::mcp_socket_path(None);
        match ahma_mcp::shell::modes::daemon_client::ensure_daemon(Some(&socket), None, None).await
        {
            Ok(outcome) => {
                if let Some(notice) = ahma_mcp::shell::modes::daemon_client::disclosure(&outcome) {
                    startup_notices::push(startup_notices::Level::Warn, notice);
                }
            }
            Err(e) => {
                // Not fatal — `resolve_connection` re-probes and may still find
                // one — but never silent.
                startup_notices::push(
                    startup_notices::Level::Warn,
                    format!("Could not reach or start the ahma daemon: {e:#}"),
                );
            }
        }
    }
    let connection = connection::resolve_connection(connect).await?;
    app::run(&connection, profile, path, token_prefs).await
}
