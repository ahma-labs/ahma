//! # ahma_tui — Terminal dashboard
//!
//! A `ratatui`-based terminal dashboard for monitoring and controlling active
//! ahma tasks without leaving the terminal.  Works over SSH; requires no Electron
//! or graphical runtime.
//!
//! ## License
//!
//! This crate is licensed under **GPL-3.0-or-later**.

pub mod app;
pub mod ui;

pub use app::{TuiApp, TuiEvent};

use anyhow::Result;

/// Run the TUI event loop, blocking until the user quits.
pub async fn run_tui(server_url: &str) -> Result<()> {
    app::run(server_url).await
}
