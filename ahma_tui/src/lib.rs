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

pub mod app;
pub mod connection;
pub mod keymap;
pub mod llm_bridge;
pub mod mcp_client;
pub mod mcp_source;
pub mod session_config;
pub mod state;
pub mod theme;
pub mod ui;

pub use connection::{ResolvedConnection, ResolvedTransport};

use anyhow::Result;

/// Run the TUI event loop, blocking until the user quits.
///
/// * `connect` — explicit `--connect` URL, or `None` to auto-probe local
///   transports (Unix socket first on Unix, then `http://localhost:3000`).
pub async fn run_tui(connect: Option<&str>) -> Result<()> {
    let connection = connection::resolve_connection(connect).await?;
    app::run(&connection).await
}
