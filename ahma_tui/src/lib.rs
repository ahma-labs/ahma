//! # ahma_tui — Terminal dashboard and chat interface
//!
//! A `ratatui`-based terminal UI providing a unified chat and dashboard interface:
//! multi-line input sends messages to a local LLM, `/` opens a command
//! navigator, and slash commands (e.g. `/tasks`, `/log`) open full-width sub-window views.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

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
pub mod theme;
pub mod ui;

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
pub static HOME_SEAM_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    // newer running bridge (see connection::handle_existing_candidate). The
    // user saw the screen flicker and was never told why; the restarted
    // process is the only one that can still say so.
    if std::env::var("AHMA_RESTARTED").is_ok() {
        startup_notices::push(
            startup_notices::Level::Warn,
            "This TUI restarted itself to match the newer ahma server already running.",
        );
    }

    if connect.is_none()
        && let Err(e) = connection::ensure_server_running(path.as_deref()).await
    {
        // Not fatal — resolve_connection re-probes and may still find or start
        // a server. But it must not be silent: this arm catches the version
        // mismatch that says "please update TUI binary", the spawn timeout,
        // and every other reason the usual path did not work.
        startup_notices::push(
            startup_notices::Level::Warn,
            format!("Could not start or reuse a local ahma server: {e:#}"),
        );
    }
    let connection = connection::resolve_connection(connect).await?;
    app::run(&connection, profile, path, token_prefs).await
}
