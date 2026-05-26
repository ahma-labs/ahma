//! # ahma_tui — Terminal dashboard
//!
//! A `ratatui`-based 4-pane terminal dashboard for monitoring and controlling
//! active ahma operations without leaving the terminal.
//!
//! Panels: AI Activity feed · Operations DAG · Detail · Log (filterable).
//! Non-modal approval banner · `:` command palette · context-sensitive footer.
//!
//! ## License
//!
//! This crate is licensed under **AGPL-3.0-or-later**.

pub mod app;
pub mod connection;
pub mod keymap;
pub mod mcp_source;
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

