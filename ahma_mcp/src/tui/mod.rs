//! # TUI Control Plane
//!
//! A `ratatui`-based terminal dashboard for monitoring and controlling active
//! ahma tasks without leaving the terminal.  Works over SSH; requires no Electron
//! or graphical runtime.
//!
//! ## Panels
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │ AHMA  Task Control Plane     [q] quit     │
//! ├──────────────────┬───────────────────────┤
//! │ Active Tasks     │ Task Detail            │
//! │ ► op_001 [run]  │ tool: cargo_build      │
//! │   op_002 [wait] │ status: InProgress     │
//! │                  │ elapsed: 12s           │
//! ├──────────────────┴───────────────────────┤
//! │ Log Tail                                  │
//! │ 12:01:03  INFO  sandbox configured        │
//! │ 12:01:04  INFO  cargo_build started       │
//! └──────────────────────────────────────────┘
//! │ Approval: op_003 wants to delete 3 files [y/n] │
//! └────────────────────────────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! ```text
//! ahma tui                          # attach to a running ahma serve http instance
//! ahma tui --connect http://localhost:3000
//! ```
//!
//! ## Feature flag
//!
//! The TUI is compiled when the `tui` feature is enabled (default on non-Windows).
//! On Windows, the feature gate prevents pulling in `crossterm`'s Unix ioctl dependencies.

pub mod app;
pub mod ui;

pub use app::{TuiApp, TuiEvent};

use anyhow::Result;

/// Run the TUI event loop, blocking until the user quits.
///
/// `server_url` is the ahma HTTP bridge to connect to.
pub async fn run_tui(server_url: &str) -> Result<()> {
    app::run(server_url).await
}
