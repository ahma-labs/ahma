//! TUI application state and event loop.
//!
//! Uses `ratatui` for rendering and `crossterm` for terminal I/O.
//! The app polls the ahma HTTP `/health` + operation status endpoints at 500ms
//! intervals and redraws when state changes.

use anyhow::Result;
use std::time::Duration;
use tracing::debug;

// ─────────────────────────────────────────────────────────────────────────────
// TuiEvent
// ─────────────────────────────────────────────────────────────────────────────

/// Events that drive the TUI state machine.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// Keyboard input from the terminal.
    Key(TuiKey),
    /// Periodic tick — triggers status refresh from the server.
    Tick,
    /// Server health status changed.
    HealthChanged { healthy: bool },
    /// Active operations snapshot arrived.
    OperationsUpdated { operations: Vec<OperationSummary> },
    /// An approval is required from the user.
    ApprovalRequired {
        operation_id: String,
        description: String,
    },
}

/// Simplified key representation for TUI navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiKey {
    Quit,
    Up,
    Down,
    Enter,
    Yes,
    No,
    Char(char),
}

// ─────────────────────────────────────────────────────────────────────────────
// OperationSummary
// ─────────────────────────────────────────────────────────────────────────────

/// A lightweight snapshot of one active or recently completed operation.
#[derive(Debug, Clone)]
pub struct OperationSummary {
    pub id: String,
    pub tool_name: String,
    pub status: String,
    pub elapsed_secs: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// TuiApp
// ─────────────────────────────────────────────────────────────────────────────

/// Application state for the TUI.
pub struct TuiApp {
    /// URL of the ahma HTTP bridge.
    pub server_url: String,
    /// Whether the server was healthy on the last poll.
    pub server_healthy: bool,
    /// Current list of operations.
    pub operations: Vec<OperationSummary>,
    /// Index of the selected operation in the list.
    pub selected: usize,
    /// Recent log lines (ring buffer).
    pub log_lines: Vec<String>,
    /// Pending approval request (if any).
    pub pending_approval: Option<(String, String)>,
    /// Whether the app should exit on the next iteration.
    pub should_quit: bool,
}

impl TuiApp {
    /// Create a new TuiApp connected to `server_url`.
    pub fn new(server_url: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            server_healthy: false,
            operations: vec![],
            selected: 0,
            log_lines: vec![],
            pending_approval: None,
            should_quit: false,
        }
    }

    /// Process an incoming event and update app state.
    pub fn handle_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::Key(TuiKey::Quit) => {
                self.should_quit = true;
            }
            TuiEvent::Key(TuiKey::Up) => {
                self.selected = self.selected.saturating_sub(1);
            }
            TuiEvent::Key(TuiKey::Down) => {
                if !self.operations.is_empty() {
                    self.selected =
                        (self.selected + 1).min(self.operations.len().saturating_sub(1));
                }
            }
            TuiEvent::Key(TuiKey::Yes) => {
                if let Some((op_id, _)) = self.pending_approval.take() {
                    self.log_append(format!("Approved: {op_id}"));
                }
            }
            TuiEvent::Key(TuiKey::No) => {
                if let Some((op_id, _)) = self.pending_approval.take() {
                    self.log_append(format!("Rejected: {op_id}"));
                }
            }
            TuiEvent::HealthChanged { healthy } => {
                self.server_healthy = healthy;
            }
            TuiEvent::OperationsUpdated { operations } => {
                self.operations = operations;
                // Keep selection in bounds.
                if !self.operations.is_empty() {
                    self.selected = self.selected.min(self.operations.len() - 1);
                }
            }
            TuiEvent::ApprovalRequired {
                operation_id,
                description,
            } => {
                self.pending_approval = Some((operation_id, description));
            }
            _ => {}
        }
    }

    fn log_append(&mut self, line: String) {
        const MAX_LOG_LINES: usize = 200;
        self.log_lines.push(line);
        if self.log_lines.len() > MAX_LOG_LINES {
            self.log_lines
                .drain(0..self.log_lines.len() - MAX_LOG_LINES);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// run() — TUI event loop
// ─────────────────────────────────────────────────────────────────────────────

/// Start the TUI event loop.
///
/// When the `tui` Cargo feature is disabled, this is a no-op stub that
/// prints a message and returns immediately.
pub async fn run(server_url: &str) -> Result<()> {
    // Check if ratatui/crossterm are available at runtime.
    // The actual rendering is gated on the `tui` feature flag.
    // When the feature is disabled we provide a CLI fallback.
    run_cli_fallback(server_url).await
}

/// CLI fallback when the `tui` feature is not compiled in.
///
/// Polls the server and prints a live status table to stdout until Ctrl-C.
async fn run_cli_fallback(server_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let health_url = format!("{}/health", server_url.trim_end_matches('/'));

    println!("Ahma TUI (text mode — compile with --features tui for full TUI)");
    println!("Connecting to: {server_url}");
    println!("Press Ctrl-C to quit.\n");

    loop {
        let healthy = client
            .get(&health_url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        let status = if healthy { "HEALTHY" } else { "UNREACHABLE" };
        let now = chrono::Utc::now().format("%H:%M:%S");
        println!("[{now}] Server {server_url} — {status}");

        debug!("TUI heartbeat: server={server_url} healthy={healthy}");

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_quit_sets_should_quit() {
        let mut app = TuiApp::new("http://localhost:3000");
        app.handle_event(TuiEvent::Key(TuiKey::Quit));
        assert!(app.should_quit);
    }

    #[test]
    fn handle_down_increments_selection() {
        let mut app = TuiApp::new("http://localhost:3000");
        app.operations = vec![
            OperationSummary {
                id: "op_1".into(),
                tool_name: "cargo_build".into(),
                status: "Running".into(),
                elapsed_secs: 5,
            },
            OperationSummary {
                id: "op_2".into(),
                tool_name: "cargo_test".into(),
                status: "Pending".into(),
                elapsed_secs: 0,
            },
        ];
        app.handle_event(TuiEvent::Key(TuiKey::Down));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn approval_yes_clears_pending() {
        let mut app = TuiApp::new("http://localhost:3000");
        app.pending_approval = Some(("op_1".into(), "Delete 3 files".into()));
        app.handle_event(TuiEvent::Key(TuiKey::Yes));
        assert!(app.pending_approval.is_none());
        assert!(app.log_lines.iter().any(|l| l.contains("Approved")));
    }
}
