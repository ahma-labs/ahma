//! Ratatui event loop and action dispatcher.
//!
//! With `--features tui` (the default), [`run`] launches the full ratatui
//! terminal UI.  Without it, a minimal text stub prints a single status line
//! and exits — no heartbeat spam.

use anyhow::Result;
use tracing::debug;

use crate::connection::ResolvedConnection;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Launch the TUI.  Restores the terminal on exit (even on error) when the
/// full ratatui UI is compiled in.
pub async fn run(connection: &ResolvedConnection) -> Result<()> {
    #[cfg(feature = "tui")]
    return run_ratatui(connection).await;

    #[cfg(not(feature = "tui"))]
    return run_text_stub(connection).await;
}

// ─── Ratatui implementation (feature = "tui") ─────────────────────────────────

#[cfg(feature = "tui")]
async fn run_ratatui(connection: &ResolvedConnection) -> Result<()> {
    use std::io;
    use std::time::Duration;

    use crossterm::{
        event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
        execute,
        terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        },
    };
    use futures::StreamExt;
    use ratatui::{Terminal, backend::CrosstermBackend};
    use tokio::sync::mpsc;

    use crate::keymap::map_key;
    use crate::mcp_source::{SourceEvent, spawn_mcp_source};
    use crate::state::AppState;
    use crate::theme::Theme;
    use crate::ui;

    let unicode = detect_unicode();
    let theme = Theme::new(unicode);
    let mut state = AppState::new(
        &connection.display_url,
        connection.transport_label(),
        unicode,
    );

    let (tx, mut rx) = mpsc::channel::<SourceEvent>(256);
    spawn_mcp_source(connection.clone(), tx);

    // ── Terminal setup ───────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal.draw(|f| ui::draw(f, &state, &theme))?;

    // ── Event loop ───────────────────────────────────────────────────────────
    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let loop_result: Result<()> = async {
        loop {
            tokio::select! {
                biased;

                maybe = event_stream.next() => {
                    match maybe {
                        Some(Ok(Event::Key(key))) => {
                            let action = map_key(
                                key,
                                state.focus,
                                &state.palette,
                                state.log_filter_active,
                            );
                            handle_action(action, &mut state);
                        }
                        Some(Ok(Event::Resize(..))) => {
                            terminal.autoresize()?;
                        }
                        Some(Err(e)) => {
                            debug!("terminal event error: {e}");
                        }
                        None => break,
                        _ => {}
                    }
                }

                Some(src_event) = rx.recv() => {
                    handle_source_event(src_event, &mut state);
                }

                _ = tick.tick() => {
                    // Periodic redraw keeps elapsed timers and approval
                    // countdown ticking even with no key events.
                }
            }

            terminal.draw(|f| ui::draw(f, &state, &theme))?;

            if state.should_quit {
                break;
            }
        }
        Ok(())
    }
    .await;

    // ── Always restore terminal ───────────────────────────────────────────────
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
    );
    let _ = terminal.show_cursor();

    loop_result
}

// ─── Action handler ───────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn handle_action(action: crate::keymap::Action, state: &mut crate::state::AppState) {
    use crate::keymap::Action;
    use crate::state::{Focus, LogEntry, LogLevel};

    match action {
        Action::Quit => {
            state.should_quit = true;
        }

        // ── Navigation ──────────────────────────────────────────────────────
        Action::Up => match state.focus {
            Focus::OpsDag => {
                state.ops_selected = state.ops_selected.saturating_sub(1);
            }
            Focus::AiActivity => {
                state.activity_scroll = state.activity_scroll.saturating_sub(1);
            }
            Focus::Log => {
                state.log_scroll = state.log_scroll.saturating_sub(1);
            }
            _ => {}
        },

        Action::Down => match state.focus {
            Focus::OpsDag if !state.operations.is_empty() => {
                state.ops_selected =
                    (state.ops_selected + 1).min(state.operations.len().saturating_sub(1));
            }
            Focus::AiActivity => {
                let max = state.ai_activity.len().saturating_sub(1);
                state.activity_scroll = (state.activity_scroll + 1).min(max);
            }
            Focus::Log => {
                let visible = state.filtered_log().len();
                state.log_scroll = (state.log_scroll + 1).min(visible.saturating_sub(1));
            }
            _ => {}
        },

        Action::Top => match state.focus {
            Focus::OpsDag => state.ops_selected = 0,
            Focus::AiActivity => state.activity_scroll = 0,
            Focus::Log => state.log_scroll = 0,
            _ => {}
        },

        Action::Bottom => match state.focus {
            Focus::OpsDag => {
                state.ops_selected = state.operations.len().saturating_sub(1);
            }
            Focus::AiActivity => {
                state.activity_scroll = state.ai_activity.len().saturating_sub(1);
            }
            Focus::Log => {
                let visible = state.filtered_log().len();
                state.log_scroll = visible.saturating_sub(1);
            }
            _ => {}
        },

        Action::Tab => state.focus = state.focus.cycle_next(),
        Action::BackTab => state.focus = state.focus.cycle_prev(),

        // ── Toggles ─────────────────────────────────────────────────────────
        Action::ToggleHelp => state.show_help = !state.show_help,
        Action::ToggleDetail => {}

        // ── Approval ────────────────────────────────────────────────────────
        Action::Approve => {
            if let Some(gate) = state.approval.take() {
                state.push_log(LogEntry {
                    timestamp: chrono::Local::now(),
                    level: LogLevel::Info,
                    message: format!("Approved gate: {}", gate.op_id),
                });
                // TODO Phase C: POST approval to MCP renewal endpoint
            }
        }
        Action::Reject => {
            if let Some(gate) = state.approval.take() {
                state.push_log(LogEntry {
                    timestamp: chrono::Local::now(),
                    level: LogLevel::Warn,
                    message: format!("Rejected gate: {}", gate.op_id),
                });
                // TODO Phase C: POST rejection to MCP renewal endpoint
            }
        }

        // ── Operation actions ────────────────────────────────────────────────
        Action::CancelOp => {
            if let Some(op) = state.selected_op() {
                let id = op.id.clone();
                state.push_log(LogEntry {
                    timestamp: chrono::Local::now(),
                    level: LogLevel::Info,
                    message: format!("Cancel requested: {id}"),
                });
                // TODO Phase D: call cancel tool via MCP
            }
        }
        Action::PinOp => {
            if let Some(op) = state.operations.get_mut(state.ops_selected) {
                op.pinned = !op.pinned;
            }
        }
        Action::AwaitOp => {
            // TODO Phase D: call await tool via MCP
        }

        // ── Command palette ──────────────────────────────────────────────────
        Action::OpenPalette => {
            let tools = state.tools_list.clone();
            state.palette.open();
            state.palette.update_completions(&tools);
            state.focus = Focus::Palette;
        }
        Action::PaletteEsc => {
            state.palette.close();
            state.focus = Focus::AiActivity;
        }
        Action::PaletteChar(c) => {
            state.palette.input.push(c);
            let tools = state.tools_list.clone();
            state.palette.update_completions(&tools);
        }
        Action::PaletteBackspace => {
            state.palette.input.pop();
            let tools = state.tools_list.clone();
            state.palette.update_completions(&tools);
        }
        Action::PaletteComplete => {
            let n = state.palette.completions.len();
            if n > 0 {
                state.palette.selected_completion =
                    (state.palette.selected_completion + 1) % n;
                let idx = state.palette.selected_completion;
                if let Some(name) = state.palette.completions.get(idx).cloned() {
                    state.palette.input = name;
                }
            }
        }
        Action::PaletteDown => {
            let n = state.palette.completions.len();
            if n > 0 {
                state.palette.selected_completion =
                    (state.palette.selected_completion + 1) % n;
            }
        }
        Action::PaletteUp => {
            let n = state.palette.completions.len();
            if n > 0 {
                state.palette.selected_completion = state
                    .palette
                    .selected_completion
                    .checked_sub(1)
                    .unwrap_or(n - 1);
            }
        }
        Action::PaletteSubmit => {
            let cmd = state.palette.input.clone();
            state.palette.close();
            state.focus = Focus::AiActivity;
            if !cmd.is_empty() {
                state.push_log(LogEntry {
                    timestamp: chrono::Local::now(),
                    level: LogLevel::Info,
                    message: format!("Command: {cmd}"),
                });
                // TODO Phase D: dispatch cmd as tools/call via MCP
            }
        }

        // ── Log filter ───────────────────────────────────────────────────────
        Action::StartFilter => {
            state.log_filter_active = true;
            state.log_filter.clear();
            state.log_scroll = 0;
        }
        Action::FilterChar(c) => {
            state.log_filter.push(c);
            state.log_scroll = 0;
        }
        Action::FilterBackspace => {
            state.log_filter.pop();
            state.log_scroll = 0;
        }
        Action::FilterEsc => {
            state.log_filter_active = false;
            state.log_filter.clear();
            state.log_scroll = 0;
        }

        Action::Unknown | Action::Enter => {}
    }
}

// ─── Source event handler ────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn handle_source_event(event: crate::mcp_source::SourceEvent, state: &mut crate::state::AppState) {
    use crate::mcp_source::SourceEvent;
    match event {
        SourceEvent::HealthChanged { healthy } => state.server_healthy = healthy,
        SourceEvent::OperationsUpdated { ops } => {
            for op in ops {
                state.upsert_operation(op);
            }
        }
        SourceEvent::AiActivity(entry) => state.push_activity(entry),
        SourceEvent::LogLine(entry) => state.push_log(entry),
        SourceEvent::ToolsListUpdated { tools } => state.tools_list = tools,
        SourceEvent::SandboxStatus { status } => state.sandbox_status = status,
        SourceEvent::SessionId { id } => state.session_id = Some(id),
    }
}

// ─── Text stub (no-tui builds) ────────────────────────────────────────────────

#[cfg(not(feature = "tui"))]
async fn run_text_stub(connection: &ResolvedConnection) -> Result<()> {
    eprintln!(
        "ahma tui: full TUI requires the `tui` feature (rebuild with --features tui).\n\
         Server: {} [{}]",
        connection.display_url,
        connection.transport_label()
    );
    Ok(())
}

// ─── Utility ─────────────────────────────────────────────────────────────────

fn detect_unicode() -> bool {
    std::env::var("NO_COLOR").is_err()
        && !std::env::var("TERM")
            .unwrap_or_default()
            .to_lowercase()
            .contains("dumb")
}




