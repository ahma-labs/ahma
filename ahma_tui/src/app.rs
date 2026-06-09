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
pub async fn run(connection: &ResolvedConnection, profile: Option<String>) -> Result<()> {
    #[cfg(feature = "tui")]
    return run_ratatui(connection, profile).await;

    #[cfg(not(feature = "tui"))]
    return run_text_stub(connection).await;
}

// ─── Ratatui implementation (feature = "tui") ─────────────────────────────────

#[cfg(feature = "tui")]
async fn run_ratatui(
    connection: &ResolvedConnection,
    profile_override: Option<String>,
) -> Result<()> {
    use std::io;
    use std::time::Duration;

    use crossterm::{
        event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    };
    use futures::StreamExt;
    use ratatui::{Terminal, backend::CrosstermBackend};
    use tokio::sync::mpsc;

    use crate::daemon_source::{spawn_daemon_source, spawn_embedded_hub_source};
    use crate::keymap::map_key;
    use crate::llm_bridge::{BridgeEvent, spawn_discovery_task};
    use crate::mcp_source::{SourceEvent, spawn_mcp_source};
    use crate::state::AppState;
    use crate::theme::Theme;
    use crate::ui;
    use ahma_common::daemon_hub::try_start_hub_server;

    let unicode = detect_unicode();
    let theme = Theme::new(unicode);
    let mut state = AppState::new(
        &connection.display_url,
        connection.transport_label(),
        unicode,
    );
    state.mcp_http_base_url = http_base_url(connection);

    if let Some(profile_name) = profile_override
        && let Ok(cwd) = std::env::current_dir()
    {
        if let Ok(profile) = crate::agent_config::get_profile(&cwd, &profile_name) {
            state.active_profile = Some(profile.name.clone());
            state.current_provider_url = Some(profile.provider_url);
            state.llm_label = format!("profile:{} / {}", profile.name, profile.model);
        } else {
            tracing::warn!("Failed to load profile override: {profile_name}");
        }
    }

    let (mcp_tx, mut mcp_rx) = mpsc::channel::<SourceEvent>(256);
    state.mcp_source_tx = Some(spawn_mcp_source(connection.clone(), mcp_tx.clone()));
    // Start the hub server inside this TUI process so its lifecycle matches the
    // TUI — no dangling socket if the TUI crashes. ahma instances connect via
    // Unix socket (macOS/Linux) or TCP loopback (Windows) using push messaging.
    // If another TUI or standalone daemon already owns the socket, we fall back
    // to subscriber mode so both TUI instances still receive events.
    let _hub = match try_start_hub_server().await {
        Ok(Some(hub)) => {
            spawn_embedded_hub_source(hub.subscribe(), mcp_tx.clone());
            Some(hub)
        }
        Ok(None) => {
            // Another server owns the socket — subscribe instead.
            debug!("hub: another server already running; connecting as subscriber");
            spawn_daemon_source(mcp_tx.clone());
            None
        }
        Err(e) => {
            debug!("hub: could not start embedded server ({e}); no multi-instance aggregation");
            None
        }
    };

    // Bridge channel carries both provider discovery results and LLM tokens.
    let (bridge_tx, mut bridge_rx) = mpsc::channel::<BridgeEvent>(512);
    spawn_discovery_task(bridge_tx.clone());
    // Store the sender so chat actions can spawn tasks later.
    state.bridge_tx = Some(bridge_tx);

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

    let loop_result: Result<()> = async {
        loop {
            tokio::select! {
                biased;

                maybe = event_stream.next() => {
                    match maybe {
                        Some(Ok(Event::Key(key))) => {
                            if (key.code == crossterm::event::KeyCode::PageUp || key.code == crossterm::event::KeyCode::PageDown)
                                && !state.show_help
                                && !state.log_files_modal_open
                            {
                                handle_page_up_down(key.code == crossterm::event::KeyCode::PageUp, &mut state);
                            } else if handle_help_key(key, &mut state)
                                || handle_picker_key(key, &mut state)
                                || handle_chat_input_key(key, &mut state)
                            {
                                // handled directly by an overlay/editor widget
                            } else {
                                let action = map_key(
                                    key,
                                    state.mode,
                                    state.focus,
                                    &state.palette,
                                    state.navigator.visible,
                                    state.log_filter_active,
                                    state.log_files_modal_open,
                                );
                                handle_action(action, &mut state);
                            }
                        }
                        Some(Ok(Event::Resize(..))) => {
                            terminal.autoresize()?;
                        }
                        Some(Ok(Event::Mouse(mouse_event))) => {
                            state.last_mouse_pos.set(Some((mouse_event.column, mouse_event.row)));
                            match mouse_event.kind {
                                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                                    handle_mouse_click(mouse_event.column, mouse_event.row, &mut state);
                                }
                                crossterm::event::MouseEventKind::ScrollUp => {
                                    handle_mouse_scroll(mouse_event.column, mouse_event.row, true, &mut state);
                                }
                                crossterm::event::MouseEventKind::ScrollDown => {
                                    handle_mouse_scroll(mouse_event.column, mouse_event.row, false, &mut state);
                                }
                                _ => {}
                            }
                        }
                        Some(Err(e)) => {
                            debug!("terminal event error: {e}");
                        }
                        None => break,
                        _ => {}
                    }
                }

                Some(src_event) = mcp_rx.recv() => {
                    handle_source_event(src_event, &mut state);
                }

                Some(bridge_event) = bridge_rx.recv() => {
                    handle_bridge_event(bridge_event, &mut state);
                }

                _ = tokio::time::sleep(if (state.chat_scroll_current.get() - state.chat_scroll_target.get()).abs() > 0.01
                    || (state.log_scroll_current.get() - state.log_scroll_target.get()).abs() > 0.01
                    || (state.chat_input_height_current.get() - state.chat_input_height_target.get()).abs() > 0.01
                {
                    Duration::from_millis(15)
                } else {
                    Duration::from_millis(250)
                }) => {
                    // Periodic redraw / animation update
                    let now = std::time::Instant::now();
                    for w in &mut state.windows {
                        if w.visible && w.finished_at.is_some_and(|t| now.duration_since(t) >= std::time::Duration::from_secs(300)) {
                            w.visible = false;
                        }
                    }
                    update_scroll_animations(&mut state);
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
    if handle_log_monitor_action(&action, state)
        || handle_picker_action(&action, state)
        || handle_navigation_action(&action, state)
        || handle_approval_action(&action, state)
        || handle_operation_action(&action, state)
        || handle_palette_action(&action, state)
        || handle_log_filter_action(&action, state)
        || handle_chat_action(&action, state)
        || handle_navigator_action(&action, state)
    {
        return;
    }

    match action {
        Action::Quit => state.should_quit = true,
        Action::Tab => state.focus = state.focus.cycle_next(),
        Action::BackTab => state.focus = state.focus.cycle_prev(),
        Action::ToggleHelp => state.show_help = !state.show_help,
        Action::FocusChat => {
            state.focus = crate::state::Focus::Chat;
        }
        Action::ToggleDetail | Action::AwaitOp | Action::Unknown | Action::Enter => {}
        _ => {}
    }
}

#[cfg(feature = "tui")]
#[cfg(feature = "tui")]
fn submit_log_switcher(state: &mut crate::state::AppState) {
    if state.log_files_modal_open {
        let idx = state.log_files_modal_selected;
        if idx == 0 {
            state.active_log_file = None;
            state.active_log_lines.clear();
            if let Some(ref tx) = state.mcp_source_tx {
                let _ = tx.try_send(crate::mcp_source::McpSourceCommand::SetActiveFile(None));
            }
        } else {
            let file_idx = idx - 1;
            if file_idx < state.log_files.len() {
                let file_name = state.log_files[file_idx].name.clone();
                state.active_log_file = Some(file_name.clone());
                state.active_log_lines.clear();
                if let Some(ref tx) = state.mcp_source_tx {
                    let _ = tx.try_send(crate::mcp_source::McpSourceCommand::SetActiveFile(Some(
                        file_name,
                    )));
                }
            }
        }
        state.log_files_modal_open = false;
    }
}

#[cfg(feature = "tui")]
fn approve_symlink(state: &mut crate::state::AppState) {
    if let Some(ref active_file) = state.active_log_file
        && let Some(info) = state.log_files.iter().find(|f| f.name == *active_file)
        && !info.is_approved
        && let Some(tx) = &state.bridge_tx
    {
        let tx = tx.clone();
        let file_to_approve = active_file.clone();
        let mcp = crate::llm_bridge::McpChatConfig {
            base_url: state.server_url.clone(),
            workspace_root: std::path::PathBuf::from(&state.workspace),
            session_id: state.session_id.clone(),
            external_http_servers: std::collections::BTreeMap::new(),
            max_turns: 8,
            tool_approval: false,
            mcp_connections: state.mcp_connections.clone(),
        };
        tokio::spawn(async move {
            crate::llm_bridge::spawn_tool_call_task(
                "logs_approve".to_string(),
                serde_json::json!({ "file": file_to_approve }),
                mcp,
                tx,
            );
        });
        // Optimistically set approved
        if let Some(pos) = state.log_files.iter().position(|f| f.name == *active_file) {
            state.log_files[pos].is_approved = true;
            if let Some(ref src_tx) = state.mcp_source_tx {
                let _ = src_tx.try_send(crate::mcp_source::McpSourceCommand::SetActiveFile(Some(
                    active_file.clone(),
                )));
            }
        }
    }
}

#[cfg(feature = "tui")]
fn handle_log_monitor_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;
    match action {
        Action::ToggleWrap => {
            state.log_wrap_enabled = !state.log_wrap_enabled;
            true
        }
        Action::ToggleZoom => {
            state.log_zoom_enabled = !state.log_zoom_enabled;
            true
        }
        Action::OpenLogSwitcher => {
            state.log_files_modal_open = true;
            state.log_files_modal_selected = 0;
            // Proactively request logs list refresh when modal is opened
            if let Some(ref tx) = state.mcp_source_tx {
                let _ = tx.try_send(crate::mcp_source::McpSourceCommand::RefreshLogs);
            }
            true
        }
        Action::CloseLogSwitcher => {
            state.log_files_modal_open = false;
            true
        }
        Action::SubmitLogSwitcher => {
            submit_log_switcher(state);
            true
        }
        Action::Up if state.log_files_modal_open => {
            if state.log_files_modal_selected > 0 {
                state.log_files_modal_selected -= 1;
            }
            true
        }
        Action::Down if state.log_files_modal_open => {
            if state.log_files_modal_selected < state.log_files.len() {
                state.log_files_modal_selected += 1;
            }
            true
        }
        Action::ApproveSymlink => {
            approve_symlink(state);
            true
        }
        _ => false,
    }
}

#[cfg(feature = "tui")]
fn handle_picker_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    if active_picker_mut(state).is_none() {
        return false;
    }

    match action {
        Action::Up | Action::NavUp => select_active_picker_prev(state),
        Action::Down | Action::NavDown => select_active_picker_next(state),
        Action::Enter | Action::NavSubmit | Action::InputSubmit => submit_active_picker(state),
        Action::NavEsc | Action::InputClear => close_active_pickers(state),
        Action::Quit => state.should_quit = true,
        _ => {}
    }

    true
}

#[cfg(feature = "tui")]
fn select_active_picker_prev(state: &mut crate::state::AppState) {
    if let Some(picker) = active_picker_mut(state) {
        picker.select_prev();
    }
}

#[cfg(feature = "tui")]
fn select_active_picker_next(state: &mut crate::state::AppState) {
    if let Some(picker) = active_picker_mut(state) {
        picker.select_next();
    }
}

#[cfg(feature = "tui")]
fn submit_active_picker(state: &mut crate::state::AppState) {
    if let Some(picker) = state.provider_picker.take() {
        submit_provider_picker(picker, state);
        return;
    }

    if let Some(picker) = state.model_picker.take() {
        submit_model_picker(picker, state);
    }
}

#[cfg(feature = "tui")]
fn submit_provider_picker(picker: crate::state::PickerState, state: &mut crate::state::AppState) {
    use crate::llm_bridge::spawn_model_refresh;

    let Some(item) = picker.selected_item() else {
        return;
    };

    let (name_part, base_url_part) = item.split_once("  ").unwrap_or((item, ""));
    let name = name_part.trim().to_string();
    let base_url = base_url_part.trim().to_string();
    let old_model = state.selected_model();
    state.current_provider_url = Some(base_url.clone());
    state.available_models.clear();
    state.llm_label = if old_model.is_empty() {
        name
    } else {
        format!("{name} / {old_model}")
    };

    if let Some(tx) = &state.bridge_tx {
        spawn_model_refresh(base_url, tx.clone());
    }

    save_session(state);
}

#[cfg(feature = "tui")]
fn submit_model_picker(picker: crate::state::PickerState, state: &mut crate::state::AppState) {
    let Some(item) = picker.selected_item() else {
        return;
    };

    if let Some((provider_name, model_name)) = item.split_once(" / ") {
        let provider_name = provider_name.trim();
        let model_name = model_name.trim();
        if let Some(provider) = state
            .available_providers
            .iter()
            .find(|p| p.name == provider_name)
        {
            state.current_provider_url = Some(provider.base_url.clone());
            state.available_models = provider.models.clone();
            state.llm_label = format!("{provider_name} / {model_name}");
            save_session(state);
        }
    } else {
        let provider = provider_label(&state.llm_label);
        state.llm_label = format!("{provider} / {item}");
        save_session(state);
    }
}

#[cfg(feature = "tui")]
fn close_active_pickers(state: &mut crate::state::AppState) {
    state.provider_picker = None;
    state.model_picker = None;
}

#[cfg(feature = "tui")]
fn handle_navigation_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    match action {
        Action::Up => scroll_focus_up(state),
        Action::Down => scroll_focus_down(state),
        Action::Top => move_focus_to_top(state),
        Action::Bottom => move_focus_to_bottom(state),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn scroll_focus_up(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::OpsDag => state.ops_selected = state.ops_selected.saturating_sub(1),
        Focus::AiActivity => state.activity_scroll = state.activity_scroll.saturating_sub(1),
        Focus::Log => {
            state.log_scroll = state.log_scroll.saturating_sub(1);
            state.sync_log_scroll_to_animation();
        }
        Focus::Chat => {
            let max = state.chat_max_scroll.get();
            state.chat_scroll = (state.chat_scroll + 1).min(max);
            state.sync_chat_scroll_to_animation();
        }
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn scroll_focus_down(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::OpsDag if !state.operations.is_empty() => {
            state.ops_selected =
                (state.ops_selected + 1).min(state.operations.len().saturating_sub(1));
        }
        Focus::AiActivity => {
            let max = state.ai_activity.len().saturating_sub(1);
            state.activity_scroll = (state.activity_scroll + 1).min(max);
        }
        Focus::Log => {
            let max = state.log_max_scroll.get();
            state.log_scroll = (state.log_scroll + 1).min(max);
            state.sync_log_scroll_to_animation();
        }
        Focus::Chat => {
            state.chat_scroll = state.chat_scroll.saturating_sub(1);
            state.sync_chat_scroll_to_animation();
        }
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn move_focus_to_top(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::OpsDag => state.ops_selected = 0,
        Focus::AiActivity => state.activity_scroll = 0,
        Focus::Log => {
            state.log_scroll = 0;
            state.sync_log_scroll_to_animation();
        }
        Focus::Chat => {
            state.chat_scroll = state.chat_max_scroll.get();
            state.sync_chat_scroll_to_animation();
        }
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn move_focus_to_bottom(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::OpsDag => state.ops_selected = state.operations.len().saturating_sub(1),
        Focus::AiActivity => state.activity_scroll = state.ai_activity.len().saturating_sub(1),
        Focus::Log => {
            state.log_scroll = state.log_max_scroll.get();
            state.sync_log_scroll_to_animation();
        }
        Focus::Chat => {
            state.chat_scroll = 0;
            state.sync_chat_scroll_to_animation();
        }
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn handle_approval_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    match action {
        Action::Approve => resolve_approval(state, true),
        Action::Reject => resolve_approval(state, false),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn resolve_approval(state: &mut crate::state::AppState, approved: bool) {
    use crate::state::{LogEntry, LogLevel};

    let Some(gate) = state.approval.take() else {
        return;
    };

    if let Some(tx) = state.approval_tx.take() {
        let _ = tx.send(approved);
    }

    let (level, verb) = if approved {
        (LogLevel::Info, "Approved")
    } else {
        (LogLevel::Warn, "Rejected")
    };

    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level,
        message: format!("{verb} gate: {}", gate.op_id),
    });
}

#[cfg(feature = "tui")]
fn handle_operation_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    match action {
        Action::CancelOp => request_cancel_selected_op(state),
        Action::PinOp => toggle_selected_op_pin(state),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn request_cancel_selected_op(state: &mut crate::state::AppState) {
    use crate::state::{LogEntry, LogLevel};

    let Some(op) = state.selected_op() else {
        return;
    };

    let id = op.id.clone();
    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level: LogLevel::Info,
        message: format!("Cancel requested: {id}"),
    });

    if let Some(tx) = &state.bridge_tx {
        let mcp_config = mcp_chat_config(state);
        crate::llm_bridge::spawn_tool_call_task(
            "cancel".to_string(),
            serde_json::json!({ "id": id }),
            mcp_config,
            tx.clone(),
        );
    }
}

#[cfg(feature = "tui")]
fn toggle_selected_op_pin(state: &mut crate::state::AppState) {
    if let Some(op) = state.operations.get_mut(state.ops_selected) {
        op.pinned = !op.pinned;
    }
}

#[cfg(feature = "tui")]
fn handle_palette_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    match action {
        Action::OpenPalette => open_palette(state),
        Action::PaletteEsc => close_palette(state),
        Action::PaletteChar(c) => {
            state.palette.input.push(*c);
            refresh_palette_completions(state);
        }
        Action::PaletteBackspace => {
            state.palette.input.pop();
            refresh_palette_completions(state);
        }
        Action::PaletteComplete => apply_palette_completion(state),
        Action::PaletteDown => advance_palette_selection(state),
        Action::PaletteUp => rewind_palette_selection(state),
        Action::PaletteSubmit => submit_palette_command(state),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn open_palette(state: &mut crate::state::AppState) {
    state.palette.open();
    refresh_palette_completions(state);
    state.focus = crate::state::Focus::Palette;
}

#[cfg(feature = "tui")]
fn close_palette(state: &mut crate::state::AppState) {
    state.palette.close();
    state.focus = crate::state::Focus::AiActivity;
}

#[cfg(feature = "tui")]
fn refresh_palette_completions(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    state.palette.update_completions(&tools);
}

#[cfg(feature = "tui")]
fn apply_palette_completion(state: &mut crate::state::AppState) {
    let n = state.palette.completions.len();
    if n == 0 {
        return;
    }

    state.palette.selected_completion = (state.palette.selected_completion + 1) % n;
    let idx = state.palette.selected_completion;
    if let Some(name) = state.palette.completions.get(idx).cloned() {
        state.palette.input = name;
    }
}

#[cfg(feature = "tui")]
fn advance_palette_selection(state: &mut crate::state::AppState) {
    let n = state.palette.completions.len();
    if n > 0 {
        state.palette.selected_completion = (state.palette.selected_completion + 1) % n;
    }
}

#[cfg(feature = "tui")]
fn rewind_palette_selection(state: &mut crate::state::AppState) {
    let n = state.palette.completions.len();
    if n > 0 {
        state.palette.selected_completion = state
            .palette
            .selected_completion
            .checked_sub(1)
            .unwrap_or(n - 1);
    }
}

#[cfg(feature = "tui")]
fn submit_palette_command(state: &mut crate::state::AppState) {
    use crate::state::{LogEntry, LogLevel};

    let cmd = state.palette.input.clone();
    close_palette(state);
    if cmd.is_empty() {
        return;
    }

    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level: LogLevel::Info,
        message: format!("Command: {cmd}"),
    });
}

#[cfg(feature = "tui")]
fn handle_log_filter_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    match action {
        Action::StartFilter => {
            state.log_filter_active = true;
            state.log_filter.clear();
            state.log_scroll = 0;
            state.sync_log_scroll_to_animation();
        }
        Action::FilterChar(c) => {
            state.log_filter.push(*c);
            state.log_scroll = 0;
            state.sync_log_scroll_to_animation();
        }
        Action::FilterBackspace => {
            state.log_filter.pop();
            state.log_scroll = 0;
            state.sync_log_scroll_to_animation();
        }
        Action::FilterEsc => {
            state.log_filter_active = false;
            state.log_filter.clear();
            state.log_scroll = 0;
            state.sync_log_scroll_to_animation();
        }
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn handle_chat_action(action: &crate::keymap::Action, state: &mut crate::state::AppState) -> bool {
    use crate::keymap::Action;

    match action {
        Action::InputChar(c) => insert_chat_character(*c, state),
        Action::InputBackspace => backspace_chat_input(state),
        Action::InputNewline => {
            state.chat_input.insert_str("\n");
        }
        Action::InputClear => state.clear_chat_input(),
        Action::InputSubmit => submit_chat_input(state),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn insert_chat_character(c: char, state: &mut crate::state::AppState) {
    if c == '/' && state.chat_input_is_empty() {
        open_navigator(state);
    } else {
        state.chat_input.insert_char(c);
    }
}

#[cfg(feature = "tui")]
fn backspace_chat_input(state: &mut crate::state::AppState) {
    state.chat_input.input(tui_textarea::Input {
        key: tui_textarea::Key::Backspace,
        ctrl: false,
        alt: false,
        shift: false,
    });
}

#[cfg(feature = "tui")]
fn submit_chat_input(state: &mut crate::state::AppState) {
    use crate::llm_bridge::{
        spawn_agent_task, spawn_chat_task, spawn_decompose_task, spawn_window_cli_task,
    };
    use crate::state::{ChatEntry, LogEntry, LogLevel, TuiWindow};
    use ahma_llm_monitor::client::LlmClient;

    let text = state.chat_input_text().trim().to_string();
    if text.is_empty() {
        return;
    }
    state.clear_chat_input();

    if text.starts_with('/') {
        dispatch_nav_command(&text, state);
        return;
    }

    let (base_url, model) = parse_llm_selection(state);

    if let Some(stripped_goal) = text.strip_prefix('!') {
        let goal = stripped_goal.trim().to_string();
        if goal.is_empty() {
            return;
        }
        if base_url.is_empty() {
            push_assistant_message(state, "No LLM configured. Use /provider to select one.");
            return;
        }
        state.push_log(LogEntry {
            timestamp: chrono::Local::now(),
            level: LogLevel::Info,
            message: format!("Decomposing goal: {}", goal),
        });
        let client = LlmClient::new(base_url, model, None);
        if let Some(tx) = &state.bridge_tx {
            spawn_decompose_task(client, goal, tx.clone());
        }
        return;
    }

    if let Some(stripped_cmd) = text
        .strip_prefix('%')
        .or_else(|| text.strip_prefix('$'))
        .or_else(|| text.strip_prefix('#'))
    {
        let cmd_str = stripped_cmd.trim().to_string();
        if cmd_str.is_empty() {
            return;
        }
        let win_id = state.next_window_id;
        state.next_window_id = (state.next_window_id + 1) % 100;

        let working_dir = state.workspace.clone();
        let label = format!(
            "Command: {} in {}",
            cmd_str,
            crate::ui::shorten_path(&working_dir, 20)
        );

        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
        let w = TuiWindow {
            id: win_id,
            label,
            status: "Running".to_string(),
            content: vec![],
            collapsed: false,
            finished_at: None,
            is_cli: true,
            command: cmd_str.clone(),
            working_dir: working_dir.clone(),
            llm_model: None,
            visible: true,
            abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(Some(abort_tx))),
            op_id: None,
        };

        state.windows.push(w);
        if state.windows.len() > 100 {
            state.windows.remove(0);
        }

        if let Some(tx) = &state.bridge_tx {
            spawn_window_cli_task(win_id, cmd_str, working_dir, abort_rx, tx.clone());
        }
        return;
    }

    if base_url.is_empty() {
        push_assistant_message(state, "No LLM configured. Use /provider to select one.");
        return;
    }

    state.chat.push(ChatEntry::User(text));
    state.chat.push(ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

    let Some(tx) = &state.bridge_tx else {
        return;
    };

    let client = LlmClient::new(base_url, model, None);
    let system = state.mcp_enabled.then(|| {
        "Use ahma tools when they would materially improve the answer. Prefer direct answers when no tool is needed.".to_string()
    });
    if state.mcp_enabled {
        spawn_agent_task(
            client,
            collect_chat_history(state),
            system,
            optional_mcp_chat_config(state),
            state.tools_list.clone(),
            tx.clone(),
        );
    } else {
        spawn_chat_task(
            client,
            collect_chat_history(state),
            system,
            optional_mcp_chat_config(state),
            tx.clone(),
        );
    }
}

#[cfg(feature = "tui")]
fn collect_chat_history(state: &crate::state::AppState) -> Vec<ahma_llm_monitor::ChatMessage> {
    use crate::state::ChatEntry;
    use ahma_llm_monitor::ChatMessage;

    state
        .chat
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            ChatEntry::User(content) => Some(ChatMessage::user(content.clone())),
            ChatEntry::Assistant {
                content,
                streaming: false,
            } => Some(ChatMessage::assistant(content.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(feature = "tui")]
fn optional_mcp_chat_config(
    state: &crate::state::AppState,
) -> Option<crate::llm_bridge::McpChatConfig> {
    (state.mcp_enabled && !state.mcp_http_base_url.is_empty()).then(|| mcp_chat_config(state))
}

#[cfg(feature = "tui")]
fn mcp_chat_config(state: &crate::state::AppState) -> crate::llm_bridge::McpChatConfig {
    let external_http_servers = state
        .mcp_connections
        .servers
        .iter()
        .filter_map(|s| match &s.kind {
            crate::mcp_connections::McpServerKind::Http { url } if s.enabled => {
                Some((s.name.clone(), url.clone()))
            }
            _ => None,
        })
        .collect();

    let max_turns = if let Some(profile_name) = &state.active_profile {
        if let Ok(cwd) = std::env::current_dir() {
            crate::agent_config::get_profile(&cwd, profile_name)
                .map(|p| p.max_turns)
                .unwrap_or(8)
        } else {
            8
        }
    } else {
        8
    };

    let tool_approval = if let Some(profile_name) = &state.active_profile {
        if let Ok(cwd) = std::env::current_dir() {
            crate::agent_config::get_profile(&cwd, profile_name)
                .map(|p| p.tool_approval)
                .unwrap_or(false)
        } else {
            false
        }
    } else {
        false
    };

    crate::llm_bridge::McpChatConfig {
        base_url: state.mcp_http_base_url.clone(),
        workspace_root: std::path::PathBuf::from(&state.workspace),
        session_id: state.session_id.clone(),
        external_http_servers,
        max_turns,
        tool_approval,
        mcp_connections: state.mcp_connections.clone(),
    }
}

#[cfg(feature = "tui")]
fn handle_navigator_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    match action {
        Action::OpenNavigator => open_navigator(state),
        Action::NavEsc => state.navigator.close(),
        Action::NavChar(c) => {
            state.navigator.input.push(*c);
            refresh_navigator_completions(state);
        }
        Action::NavBackspace => {
            state.navigator.input.pop();
            refresh_navigator_completions(state);
        }
        Action::NavComplete => state.navigator.tab_complete(),
        Action::NavUp => state.navigator.select_prev(),
        Action::NavDown => state.navigator.select_next(),
        Action::NavSubmit => submit_navigator_command(state),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn open_navigator(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    state.navigator.open(&tools);
}

#[cfg(feature = "tui")]
fn refresh_navigator_completions(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    state.navigator.refresh_completions(&tools);
}

#[cfg(feature = "tui")]
fn submit_navigator_command(state: &mut crate::state::AppState) {
    let cmd = state.navigator.selected_command();
    state.navigator.close();
    dispatch_nav_command(&cmd, state);
}

#[cfg(feature = "tui")]
fn handle_help_key(key: crossterm::event::KeyEvent, state: &mut crate::state::AppState) -> bool {
    use crossterm::event::KeyCode;

    if !state.show_help {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) | (KeyCode::Char('?'), _) => {
            state.show_help = false;
            true
        }
        _ => true,
    }
}

#[cfg(feature = "tui")]
fn handle_picker_key(key: crossterm::event::KeyEvent, state: &mut crate::state::AppState) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    let Some(picker) = active_picker_mut(state) else {
        return false;
    };

    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => {
            picker.select_prev();
            true
        }
        (KeyCode::Down, _) => {
            picker.select_next();
            true
        }
        (KeyCode::Enter, _) => {
            handle_action(crate::keymap::Action::Enter, state);
            true
        }
        (KeyCode::Esc, _) => {
            handle_action(crate::keymap::Action::NavEsc, state);
            true
        }
        (KeyCode::Backspace, _) => {
            picker.filter_pop();
            true
        }
        (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
            picker.filter_push(c);
            true
        }
        _ => false,
    }
}

#[cfg(feature = "tui")]
fn active_picker_mut(state: &mut crate::state::AppState) -> Option<&mut crate::state::PickerState> {
    if state.provider_picker.is_some() {
        state.provider_picker.as_mut()
    } else {
        state.model_picker.as_mut()
    }
}

#[cfg(feature = "tui")]
fn handle_chat_input_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::state::Focus;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.focus != Focus::Chat
        || state.navigator.visible
        || state.provider_picker.is_some()
        || state.model_picker.is_some()
        || state.palette.visible
        || state.log_filter_active
    {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            state.should_quit = true;
            true
        }
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            if state.chat_input_is_empty() {
                state.chat_input.insert_str("/run ");
                let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
                state.navigator.open(&tools);
                state.navigator.input = "run ".to_string();
                state.navigator.refresh_completions(&tools);
            }
            true
        }
        (KeyCode::Tab, KeyModifiers::NONE) => {
            handle_action(crate::keymap::Action::Tab, state);
            true
        }
        (KeyCode::BackTab, _) => {
            handle_action(crate::keymap::Action::BackTab, state);
            true
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            handle_action(crate::keymap::Action::InputSubmit, state);
            true
        }
        (KeyCode::Esc, _) => {
            state.clear_chat_input();
            true
        }
        (KeyCode::Char('/'), KeyModifiers::NONE) if state.chat_input_is_empty() => {
            let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
            state.navigator.open(&tools);
            true
        }
        _ => state.chat_input.input(textarea_input_from_key_event(key)),
    }
}

#[cfg(feature = "tui")]
fn textarea_input_from_key_event(key: crossterm::event::KeyEvent) -> tui_textarea::Input {
    use crossterm::event::{KeyCode, KeyModifiers};

    let mapped_key = match key.code {
        KeyCode::Backspace => tui_textarea::Key::Backspace,
        KeyCode::Enter => tui_textarea::Key::Enter,
        KeyCode::Left => tui_textarea::Key::Left,
        KeyCode::Right => tui_textarea::Key::Right,
        KeyCode::Up => tui_textarea::Key::Up,
        KeyCode::Down => tui_textarea::Key::Down,
        KeyCode::Tab => tui_textarea::Key::Tab,
        KeyCode::Delete => tui_textarea::Key::Delete,
        KeyCode::Home => tui_textarea::Key::Home,
        KeyCode::End => tui_textarea::Key::End,
        KeyCode::PageUp => tui_textarea::Key::PageUp,
        KeyCode::PageDown => tui_textarea::Key::PageDown,
        KeyCode::Esc => tui_textarea::Key::Esc,
        KeyCode::F(value) => tui_textarea::Key::F(value),
        KeyCode::Char(c) => tui_textarea::Key::Char(c),
        _ => tui_textarea::Key::Null,
    };

    tui_textarea::Input {
        key: mapped_key,
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    }
}

#[cfg(feature = "tui")]
fn handle_window_nav_commands(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd == "/exit" || cmd == "/q" || cmd == "/quit" {
        state.should_quit = true;
        return true;
    }
    if let Some(num_str) = cmd.strip_prefix("/x")
        && let Ok(id) = num_str.parse::<usize>()
    {
        close_window_by_id(id, state);
        return true;
    }
    if let Some(num_str) = cmd.strip_prefix('/')
        && let Ok(id) = num_str.parse::<usize>()
        && let Some(w) = state.windows.iter_mut().find(|w| w.id == id)
    {
        w.visible = true;
        w.collapsed = false;
        return true;
    }
    false
}

/// Dispatch a `/command` string from the navigator.
#[cfg(feature = "tui")]
fn dispatch_nav_command(cmd: &str, state: &mut crate::state::AppState) {
    let cmd = cmd.trim();
    if handle_window_nav_commands(cmd, state)
        || handle_basic_nav_command(cmd, state)
        || handle_mode_nav_command(cmd, state)
        || handle_mcp_nav_command(cmd, state)
        || handle_agent_nav_command(cmd, state)
        || handle_export_nav_command(cmd, state)
        || handle_tools_nav_command(cmd, state)
        || handle_approval_nav_command(cmd, state)
        || handle_picker_nav_command(cmd, state)
        || handle_run_nav_command(cmd, state)
        || handle_monitor_nav_command(cmd, state)
        || handle_analyze_nav_command(cmd, state)
    {
        return;
    }

    push_assistant_message(
        state,
        format!("Unknown command `{cmd}`. Use /help to see the available commands."),
    );
}

#[cfg(feature = "tui")]
fn handle_basic_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    match cmd {
        "/help" | "/?" => state.show_help = true,
        "/clear" => state.chat.clear(),
        "/compact" => {
            state.chat.compact(4);
            push_assistant_message(state, "Context window compacted (kept 4 latest turns).");
        }
        "/operations" => set_mode_and_focus(
            state,
            crate::state::Mode::Monitor,
            crate::state::Focus::OpsDag,
        ),
        "/logs" => set_mode_and_focus(state, crate::state::Mode::Monitor, crate::state::Focus::Log),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn handle_mode_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    match cmd {
        "/mode chat" => {
            set_mode_and_focus(state, crate::state::Mode::Chat, crate::state::Focus::Chat)
        }
        "/mode monitor" => set_mode_and_focus(
            state,
            crate::state::Mode::Monitor,
            crate::state::Focus::AiActivity,
        ),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn set_mode_and_focus(
    state: &mut crate::state::AppState,
    mode: crate::state::Mode,
    focus: crate::state::Focus,
) {
    state.mode = mode;
    state.focus = focus;
}

#[cfg(feature = "tui")]
fn handle_mcp_list(state: &mut crate::state::AppState) {
    let servers = state.mcp_connections.list_servers();
    if servers.is_empty() {
        push_assistant_message(state, "No MCP client servers configured.");
    } else {
        let mut msg = String::from("Configured MCP servers:\n");
        for s in servers {
            let kind = match &s.kind {
                crate::mcp_connections::McpServerKind::Http { url } => {
                    format!("http {url}")
                }
                crate::mcp_connections::McpServerKind::Stdio { command, args } => {
                    format!("stdio {} {}", command, args.join(" "))
                }
            };
            msg.push_str(&format!(
                "- {} [{}] {}\n",
                s.name,
                if s.enabled { "on" } else { "off" },
                kind
            ));
        }
        push_assistant_message(state, msg.trim_end());
    }
}

#[cfg(feature = "tui")]
fn handle_mcp_add_http(rest: &str, state: &mut crate::state::AppState) {
    let mut parts = rest.split_whitespace();
    let Some(url) = parts.next() else {
        push_assistant_message(state, "Usage: /mcp add http <url> [name]");
        return;
    };
    let name = parts.next().unwrap_or("external-http").to_string();
    state
        .mcp_connections
        .add_server(crate::mcp_connections::McpServerConfig {
            name: name.clone(),
            enabled: true,
            kind: crate::mcp_connections::McpServerKind::Http {
                url: url.to_string(),
            },
        });
    if let Ok(cwd) = std::env::current_dir() {
        let _ = state.mcp_connections.save(&cwd);
    }
    push_assistant_message(state, format!("Added HTTP MCP server `{name}` -> {url}"));
}

#[cfg(feature = "tui")]
fn handle_mcp_add_stdio(rest: &str, state: &mut crate::state::AppState) {
    let mut parts = rest.split_whitespace().peekable();
    let Some(command) = parts.next() else {
        push_assistant_message(
            state,
            "Usage: /mcp add stdio <command> [args...] [--name <name>]",
        );
        return;
    };
    let mut args: Vec<String> = Vec::new();
    let mut name_override: Option<String> = None;
    while let Some(part) = parts.next() {
        if part == "--name" {
            name_override = parts.next().map(String::from);
        } else {
            args.push(part.to_string());
        }
    }
    let name = name_override.unwrap_or_else(|| {
        std::path::Path::new(command)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(command)
            .to_string()
    });
    state
        .mcp_connections
        .add_server(crate::mcp_connections::McpServerConfig {
            name: name.clone(),
            enabled: true,
            kind: crate::mcp_connections::McpServerKind::Stdio {
                command: command.to_string(),
                args,
            },
        });
    if let Ok(cwd) = std::env::current_dir() {
        let _ = state.mcp_connections.save(&cwd);
    }
    push_assistant_message(
        state,
        format!("Added stdio MCP server `{name}` ({})", command),
    );
}

#[cfg(feature = "tui")]
fn handle_mcp_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd == "/mcp on" {
        set_mcp_enabled(state, true);
        return true;
    }
    if cmd == "/mcp off" {
        set_mcp_enabled(state, false);
        return true;
    }

    if cmd == "/mcp list" {
        handle_mcp_list(state);
        return true;
    }

    if cmd == "/mcp refresh" {
        if let Some(tx) = &state.bridge_tx {
            crate::llm_bridge::spawn_external_tools_refresh(
                state.mcp_connections.clone(),
                tx.clone(),
            );
            push_assistant_message(state, "Refreshing external MCP tools in the background...");
        } else {
            push_assistant_message(
                state,
                "Bridge is not available; cannot refresh external tools.",
            );
        }
        return true;
    }

    if let Some(rest) = cmd.strip_prefix("/mcp remove ") {
        let name = rest.trim();
        if name.is_empty() {
            push_assistant_message(state, "Usage: /mcp remove <name>");
            return true;
        }
        state.mcp_connections.remove_server(name);
        if let Ok(cwd) = std::env::current_dir() {
            let _ = state.mcp_connections.save(&cwd);
        }
        push_assistant_message(state, format!("Removed MCP server `{name}`."));
        return true;
    }

    if let Some(rest) = cmd.strip_prefix("/mcp add http ") {
        handle_mcp_add_http(rest, state);
        return true;
    }

    if let Some(rest) = cmd.strip_prefix("/mcp add stdio ") {
        handle_mcp_add_stdio(rest, state);
        return true;
    }

    false
}

#[cfg(feature = "tui")]
fn set_mcp_enabled(state: &mut crate::state::AppState, enabled: bool) {
    state.mcp_enabled = enabled;
    let message = if enabled {
        "ahma MCP bridge enabled. The LLM can now call ahma tools."
    } else {
        "ahma MCP bridge disabled."
    };
    push_assistant_message(state, message);
    save_session(state);
}

#[cfg(feature = "tui")]
fn handle_tools_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd != "/tools" {
        return false;
    }

    push_assistant_message(state, format_tools_list_message(&state.tools_list));
    true
}

#[cfg(feature = "tui")]
fn handle_agent_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        push_assistant_message(state, "Cannot resolve current working directory.");
        return true;
    };

    if cmd == "/agent list" {
        match crate::agent_config::load_profiles(&cwd) {
            Ok(file) => {
                if file.profiles.is_empty() {
                    push_assistant_message(state, "No saved agent profiles.");
                } else {
                    let mut msg = String::from("Saved agent profiles:\n");
                    for name in file.profiles.keys() {
                        msg.push_str(&format!("- {name}\n"));
                    }
                    push_assistant_message(state, msg.trim_end());
                }
            }
            Err(e) => push_assistant_message(state, format!("Failed to load profiles: {e}")),
        }
        return true;
    }

    if let Some(name) = cmd.strip_prefix("/agent save ") {
        let name = name.trim();
        if name.is_empty() {
            push_assistant_message(state, "Usage: /agent save <name>");
            return true;
        }
        let profile = crate::agent_config::AgentProfile {
            name: name.to_string(),
            model: state.selected_model(),
            provider_url: state.current_provider_url.clone().unwrap_or_default(),
            system_prompt: "Use tools when needed, prefer concise reasoning.".to_string(),
            tool_approval: false,
            max_turns: 8,
            mcp_servers: state
                .mcp_connections
                .servers
                .iter()
                .map(|s| s.name.clone())
                .collect(),
        };
        match crate::agent_config::upsert_profile(&cwd, profile) {
            Ok(()) => {
                state.active_profile = Some(name.to_string());
                push_assistant_message(state, format!("Saved profile `{name}`."));
            }
            Err(e) => push_assistant_message(state, format!("Failed to save profile: {e}")),
        }
        return true;
    }

    if let Some(name) = cmd.strip_prefix("/agent load ") {
        let name = name.trim();
        if name.is_empty() {
            push_assistant_message(state, "Usage: /agent load <name>");
            return true;
        }
        match crate::agent_config::get_profile(&cwd, name) {
            Ok(profile) => {
                state.active_profile = Some(profile.name.clone());
                state.current_provider_url = Some(profile.provider_url);
                state.llm_label = format!("profile:{name} / {}", profile.model);
                push_assistant_message(state, format!("Loaded profile `{name}`."));
            }
            Err(e) => push_assistant_message(state, format!("Failed to load profile: {e}")),
        }
        return true;
    }

    if let Some(name) = cmd.strip_prefix("/agent delete ") {
        let name = name.trim();
        if name.is_empty() {
            push_assistant_message(state, "Usage: /agent delete <name>");
            return true;
        }
        match crate::agent_config::delete_profile(&cwd, name) {
            Ok(true) => push_assistant_message(state, format!("Deleted profile `{name}`.")),
            Ok(false) => push_assistant_message(state, format!("Profile `{name}` not found.")),
            Err(e) => push_assistant_message(state, format!("Failed to delete profile: {e}")),
        }
        return true;
    }

    false
}

#[cfg(feature = "tui")]
fn handle_export_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd != "/export markdown" {
        return false;
    }

    let Ok(cwd) = std::env::current_dir() else {
        push_assistant_message(state, "Cannot resolve current working directory.");
        return true;
    };

    let out_dir = cwd.join(".ahma").join("exports");
    let _ = std::fs::create_dir_all(&out_dir);
    let file = out_dir.join(format!(
        "chat-{}.md",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));

    let mut md = String::from("# ahma chat export\n\n");
    for entry in state.chat.entries() {
        match entry {
            crate::state::ChatEntry::User(content) => {
                md.push_str("## User\n\n");
                md.push_str(content);
                md.push_str("\n\n");
            }
            crate::state::ChatEntry::Assistant { content, .. } => {
                md.push_str("## Assistant\n\n");
                md.push_str(content);
                md.push_str("\n\n");
            }
            crate::state::ChatEntry::ToolCall {
                name, args, result, ..
            } => {
                md.push_str(&format!("## Tool `{name}`\n\n"));
                md.push_str(&format!("Args: `{args}`\n\n"));
                if let Some(result) = result {
                    md.push_str(&format!("Result:\n\n```\n{}\n```\n\n", result));
                }
            }
        }
    }

    match std::fs::write(&file, md) {
        Ok(_) => push_assistant_message(state, format!("Exported chat to `{}`", file.display())),
        Err(e) => push_assistant_message(state, format!("Export failed: {e}")),
    }
    true
}

#[cfg(feature = "tui")]
fn format_tools_list_message(tools: &[crate::mcp_connections::ToolInfo]) -> String {
    if tools.is_empty() {
        return "No tools discovered yet.".to_string();
    }
    let mut content = format!("{} tool(s) available:\n", tools.len());
    for tool in tools {
        if let Some(desc) = &tool.description {
            content.push_str(&format!("- **{}**: {}\n", tool.name, desc));
        } else {
            content.push_str(&format!("- {}\n", tool.name));
        }
    }
    content.trim_end().to_string()
}

#[cfg(feature = "tui")]
fn handle_approval_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    match cmd {
        "/approve" => resolve_approval(state, true),
        "/reject" => resolve_approval(state, false),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn handle_picker_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    match cmd {
        "/provider" => open_provider_picker(state),
        "/model" => open_model_picker(state),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn open_provider_picker(state: &mut crate::state::AppState) {
    use crate::state::PickerState;

    let items: Vec<String> = state
        .available_providers
        .iter()
        .map(|p| format!("{}  {}", p.name, p.base_url))
        .collect();

    if items.is_empty() {
        push_assistant_message(
            state,
            "No providers discovered yet. Install Ollama or use `ahma llm add`.",
        );
        return;
    }

    let mut picker = PickerState::new("Select provider", items);
    if let Some(current_url) = &state.current_provider_url {
        let selected = state
            .available_providers
            .iter()
            .find(|p| p.base_url == *current_url)
            .map(|p| format!("{}  {}", p.name, p.base_url))
            .unwrap_or_default();
        picker.select_exact(&selected);
    }
    state.provider_picker = Some(picker);
}

#[cfg(feature = "tui")]
fn open_model_picker(state: &mut crate::state::AppState) {
    use crate::llm_bridge::spawn_model_refresh;
    use crate::state::PickerState;

    let mut items = Vec::new();
    for provider in &state.available_providers {
        for model in &provider.models {
            items.push(format!("{} / {}", provider.name, model));
        }
    }

    if items.is_empty() {
        let (base_url, _) = parse_llm_selection(state);
        if !base_url.is_empty()
            && let Some(tx) = &state.bridge_tx
        {
            spawn_model_refresh(base_url, tx.clone());
        }
        push_assistant_message(state, "Fetching model list…");
        return;
    }

    let mut picker = PickerState::new("Select model", items);
    let selected_model = state.selected_model();
    let selected_provider = provider_label(&state.llm_label);
    if !selected_model.is_empty() && !selected_provider.is_empty() {
        let exact = format!("{selected_provider} / {selected_model}");
        picker.select_exact(&exact);
    }
    state.model_picker = Some(picker);
}

#[cfg(feature = "tui")]
fn handle_run_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if !cmd.starts_with("/run ") {
        return false;
    }

    run_nav_tool(cmd.trim_start_matches("/run ").trim(), state);
    true
}

#[cfg(feature = "tui")]
fn run_nav_tool(rest: &str, state: &mut crate::state::AppState) {
    let (tool, arguments) = match parse_run_command(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            push_assistant_message(state, message);
            return;
        }
    };

    if let Err(message) = validate_nav_tool_run(tool, state) {
        push_assistant_message(state, message);
        return;
    }

    let Some(tx) = &state.bridge_tx else {
        return;
    };

    if tool.contains("::") {
        let tool_name = tool.to_string();
        let args_clone = arguments.clone();
        let tx_clone = tx.clone();
        let manager = state.mcp_connections.clone();
        tokio::spawn(async move {
            let id = format!(
                "call_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
            );
            let _ = tx_clone
                .send(crate::llm_bridge::BridgeEvent::ToolCallStarted {
                    id: id.clone(),
                    name: tool_name.clone(),
                    args: serde_json::to_string(&args_clone).unwrap_or_default(),
                })
                .await;

            let outcome = manager.call_tool(&tool_name, args_clone).await;
            match outcome {
                Ok((result, failed)) => {
                    let _ = tx_clone
                        .send(crate::llm_bridge::BridgeEvent::ToolCallFinished {
                            id,
                            result,
                            failed,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx_clone
                        .send(crate::llm_bridge::BridgeEvent::ToolCallFinished {
                            id,
                            result: format!("Error: {e}"),
                            failed: true,
                        })
                        .await;
                }
            }
        });
        return;
    }

    crate::llm_bridge::spawn_tool_call_task(
        tool.to_string(),
        arguments,
        mcp_chat_config(state),
        tx.clone(),
    );
}

#[cfg(feature = "tui")]
fn validate_nav_tool_run(tool: &str, state: &crate::state::AppState) -> Result<(), String> {
    if state.mcp_http_base_url.is_empty() {
        return Err("Cannot run tools because the ahma MCP bridge URL is unavailable.".to_string());
    }
    if !state.tools_list.is_empty() && !state.tools_list.iter().any(|known| known.name == tool) {
        return Err(format!(
            "Unknown tool `{tool}`. Use /tools to inspect the current tool list."
        ));
    }
    Ok(())
}

#[cfg(feature = "tui")]
fn push_assistant_message(state: &mut crate::state::AppState, content: impl Into<String>) {
    state.chat.push(crate::state::ChatEntry::Assistant {
        content: content.into(),
        streaming: false,
    });
}

fn parse_run_command(rest: &str) -> Result<(&str, serde_json::Value), String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err("Usage: /run <tool> {json}\nExample: /run status {}".to_string());
    }

    let split_at = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let tool = rest[..split_at].trim();
    let args_raw = rest[split_at..].trim();

    if tool.is_empty() {
        return Err("Usage: /run <tool> {json}".to_string());
    }

    if args_raw.is_empty() {
        return Ok((tool, serde_json::json!({})));
    }

    let value = serde_json::from_str::<serde_json::Value>(args_raw).map_err(|error| {
        format!(
            "Invalid JSON arguments for /run: {error}\nUsage: /run <tool> {{\"key\":\"value\"}}"
        )
    })?;

    if !value.is_object() {
        return Err(
            "/run expects a JSON object for arguments, for example: /run status {}".to_string(),
        );
    }

    Ok((tool, value))
}

fn parse_llm_selection(state: &crate::state::AppState) -> (String, String) {
    if state.llm_label == "no LLM" || state.llm_label.is_empty() {
        return (String::new(), String::new());
    }

    let model = state.selected_model();
    if let Some(base_url) = &state.current_provider_url {
        return (base_url.clone(), model);
    }

    let provider_name = provider_label(&state.llm_label);
    let base_url = if provider_name.starts_with("http") {
        provider_name
    } else {
        default_provider_base_url(&provider_name)
    };
    (base_url, model)
}

fn default_provider_base_url(provider_name: &str) -> String {
    let lower = provider_name.to_lowercase();
    if lower.contains("ollama") {
        "http://localhost:11434/v1".to_string()
    } else if lower.contains("llama") {
        "http://localhost:8080/v1".to_string()
    } else {
        provider_name.to_string()
    }
}

fn provider_label(label: &str) -> String {
    label
        .rsplit_once(" / ")
        .map(|(provider, _)| provider.trim().to_string())
        .unwrap_or_else(|| label.trim().to_string())
}

/// Persist the current session config to `.ahma/session.toml`.
fn save_session(state: &crate::state::AppState) {
    use crate::session_config::TuiSessionConfig;

    let (provider, model) = {
        let label = &state.llm_label;
        if let Some(pos) = label.rfind(" / ") {
            (
                label[..pos].trim().to_string(),
                label[pos + 3..].trim().to_string(),
            )
        } else {
            (label.clone(), String::new())
        }
    };

    let cfg = TuiSessionConfig {
        provider,
        model,
        provider_url: state.current_provider_url.clone(),
        mcp_enabled: state.mcp_enabled,
        active_profile: state.active_profile.clone(),
    };

    if let Ok(cwd) = std::env::current_dir()
        && let Err(e) = cfg.save(&cwd)
    {
        debug!("Failed to save session config: {e}");
    }
}

// ─── Bridge event handler ─────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn handle_providers_discovered(
    providers: Vec<ahma_llm_monitor::LocalProvider>,
    state: &mut crate::state::AppState,
) {
    state.available_providers = providers.clone();

    if state.llm_label == "no LLM" {
        if let Some(provider) = providers.first() {
            auto_select_first_provider(state, provider);
        }
        return;
    }

    if let Some(current_url) = state.current_provider_url.clone() {
        refresh_current_provider_models(state, &providers, &current_url);
    }
}

#[cfg(feature = "tui")]
fn auto_select_first_provider(
    state: &mut crate::state::AppState,
    provider: &ahma_llm_monitor::LocalProvider,
) {
    let model = provider.models.first().cloned().unwrap_or_default();
    state.available_models = provider.models.clone();
    state.current_provider_url = Some(provider.base_url.clone());
    state.llm_label = format!("{} / {}", provider.name, model);
    save_session(state);
}

#[cfg(feature = "tui")]
fn refresh_current_provider_models(
    state: &mut crate::state::AppState,
    providers: &[ahma_llm_monitor::LocalProvider],
    current_url: &str,
) {
    let Some(provider) = providers.iter().find(|p| p.base_url == current_url) else {
        return;
    };
    state.available_models = provider.models.clone();
    let model = state.selected_model();
    if !model.is_empty() {
        state.llm_label = format!("{} / {}", provider.name, model);
    }
}

#[cfg(feature = "tui")]
fn handle_models_refreshed(
    base_url: String,
    models: Vec<String>,
    state: &mut crate::state::AppState,
) {
    if let Some(provider) = state
        .available_providers
        .iter_mut()
        .find(|p| p.base_url == base_url)
    {
        provider.models = models.clone();
    }

    if state.current_provider_url.as_deref() == Some(base_url.as_str()) && !models.is_empty() {
        state.available_models = models.clone();
        open_model_picker(state);
    }
}

#[cfg(feature = "tui")]
fn handle_decomposed_event(
    steps: Vec<ahma_task_tree::parser::ParsedStep>,
    state: &mut crate::state::AppState,
) {
    for step in steps {
        let win_id = state.next_window_id;
        state.next_window_id = (state.next_window_id + 1) % 100;

        let is_cli = step.r#type.as_str() == "shell_command";

        let label = if is_cli {
            format!(
                "Command: {} in {}",
                step.command.as_deref().unwrap_or(&step.task),
                crate::ui::shorten_path(&state.workspace, 20)
            )
        } else {
            format!("LLM Call (model: {})", state.selected_model())
        };

        let command = if is_cli {
            step.command.clone().unwrap_or(step.task.clone())
        } else {
            step.instructions.clone().unwrap_or(step.task.clone())
        };

        let w = crate::state::TuiWindow {
            id: win_id,
            label,
            status: "Pending".to_string(),
            content: vec![format!("Task: {}", step.task)],
            collapsed: false,
            finished_at: None,
            is_cli,
            command,
            working_dir: state.workspace.clone(),
            llm_model: if is_cli {
                None
            } else {
                Some(state.selected_model())
            },
            visible: true,
            abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            op_id: None,
        };

        state.windows.push(w);
        if state.windows.len() > 100 {
            state.windows.remove(0);
        }
    }

    run_next_pending_window(state);
}

#[cfg(feature = "tui")]
fn handle_window_output_event(window_id: usize, line: String, state: &mut crate::state::AppState) {
    if let Some(w) = state.windows.iter_mut().find(|w| w.id == window_id) {
        if w.is_cli {
            w.content.push(line);
        } else {
            if w.content.is_empty() {
                w.content.push(String::new());
            }
            let parts: Vec<&str> = line.split('\n').collect();
            if let Some(last) = w.content.last_mut() {
                last.push_str(parts[0]);
            }
            for part in parts.iter().skip(1) {
                w.content.push(part.to_string());
            }
        }
    }
}

#[cfg(feature = "tui")]
fn handle_window_finished_event(
    window_id: usize,
    success: bool,
    summary: String,
    state: &mut crate::state::AppState,
) {
    let mut current_failed = false;
    if let Some(w) = state.windows.iter_mut().find(|w| w.id == window_id) {
        w.status = if success {
            "Finished".to_string()
        } else {
            "Error".to_string()
        };
        w.content.push(summary);
        w.finished_at = Some(std::time::Instant::now());
        if !success {
            current_failed = true;
        }
    }
    if current_failed {
        for w in &mut state.windows {
            if w.status == "Pending" {
                w.status = "Cancelled".to_string();
                w.finished_at = Some(std::time::Instant::now());
            }
        }
    } else {
        run_next_pending_window(state);
    }
}

#[cfg(feature = "tui")]
fn handle_bridge_event(event: crate::llm_bridge::BridgeEvent, state: &mut crate::state::AppState) {
    use crate::llm_bridge::BridgeEvent;
    use crate::state::ChatEntry;

    match event {
        BridgeEvent::Token(token) => {
            state.chat.append_token(&token);
            state.chat_scroll = 0;
        }
        BridgeEvent::Usage(usage) => {
            state.token_usage.prompt_tokens += usage.prompt_tokens;
            state.token_usage.completion_tokens += usage.completion_tokens;
            state.token_usage.total_tokens += usage.total_tokens;
        }
        BridgeEvent::Done => {
            state.chat.finish_stream();
            if let Some(profile) = &state.active_profile
                && let Ok(cwd) = std::env::current_dir()
            {
                let payload = serde_json::json!({
                    "timestamp": chrono::Local::now().to_rfc3339(),
                    "chat_entries": state.chat.entries().len(),
                    "model": state.selected_model(),
                });
                let _ = crate::agent_config::append_transcript_entry(
                    &cwd,
                    profile,
                    &payload.to_string(),
                );
            }
        }
        BridgeEvent::Error(msg) => {
            state.chat.finish_stream();
            state.chat.push(ChatEntry::Assistant {
                content: format!("Error: {msg}"),
                streaming: false,
            });
            state.chat_scroll = 0;
        }
        BridgeEvent::Decomposed { steps } => {
            handle_decomposed_event(steps, state);
        }
        BridgeEvent::WindowOutput { window_id, line } => {
            handle_window_output_event(window_id, line, state);
        }
        BridgeEvent::WindowFinished {
            window_id,
            success,
            summary,
        } => {
            handle_window_finished_event(window_id, success, summary, state);
        }
        BridgeEvent::ToolCallStarted { id, name, args } => {
            state.chat.start_tool_call(id, name, args);
            state.chat_scroll = 0;
        }
        BridgeEvent::ToolCallFinished { id, result, failed } => {
            state.chat.finish_tool_call(&id, result, failed);
            state.chat_scroll = 0;
        }
        BridgeEvent::ProvidersDiscovered(providers) => {
            handle_providers_discovered(providers, state);
        }
        BridgeEvent::ModelsRefreshed { base_url, models } => {
            handle_models_refreshed(base_url, models, state);
        }
        BridgeEvent::ExternalToolsRefreshed { manager } => {
            state.mcp_connections = manager;
            let mut merged = state.tools_list.clone();
            for tool in state.mcp_connections.aggregate_tools() {
                if !merged.iter().any(|existing| existing.name == tool.name) {
                    merged.push(tool);
                }
            }
            merged.sort_by(|a, b| a.name.cmp(&b.name));
            state.tools_list = merged;
            push_assistant_message(state, "External MCP tools refreshed.");
        }
        BridgeEvent::RequestApproval { id, tool, args, tx } => {
            let diff = if tool.contains("replace") || tool == "write_file" {
                serde_json::from_str::<serde_json::Value>(&args)
                    .ok()
                    .and_then(|val| serde_json::to_string_pretty(&val).ok())
            } else {
                None
            };

            state.approval = Some(crate::state::ApprovalGate {
                op_id: id,
                description: format!("Execute tool {tool}"),
                deadline: None,
                diff,
            });
            state.approval_tx = Some(tx);
        }
    }
}

#[cfg(feature = "tui")]
fn clean_up_summary(summary: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(summary) {
        if let Some(msg) = val.get("message").and_then(|v| v.as_str()) {
            return msg.to_string();
        }
        if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
            return err.to_string();
        }
    }
    if summary.starts_with('"') && summary.ends_with('"') && summary.len() >= 2 {
        return summary[1..summary.len() - 1].to_string();
    }
    summary.to_string()
}

#[cfg(feature = "tui")]
fn format_friendly_start(
    tool_name: &str,
    description: &str,
    start_time: chrono::DateTime<chrono::Local>,
) -> String {
    let mut args_summary = String::new();
    if let Some(start_idx) = description.find('{')
        && let Some(end_idx) = description.rfind('}')
        && start_idx < end_idx
        && let Ok(val) =
            serde_json::from_str::<serde_json::Value>(&description[start_idx..=end_idx])
        && let Some(obj) = val.as_object()
    {
        if tool_name == "run_terminal_command" {
            if let Some(cmd) = obj.get("command").and_then(|v| v.as_str()) {
                args_summary = format!("command: {cmd}");
            }
        } else {
            let parts: Vec<String> = obj
                .iter()
                .filter(|(k, _)| {
                    *k != "working_directory" && *k != "working_dir" && *k != "synchronous"
                })
                .map(|(k, v)| {
                    let val_str = match v {
                        serde_json::Value::String(s) => s.clone(),
                        _ => v.to_string(),
                    };
                    format!("{k}={val_str}")
                })
                .collect();
            if !parts.is_empty() {
                args_summary = parts.join(", ");
            }
        }
    }

    let time_str = start_time.format("%H:%M:%S").to_string();
    if args_summary.is_empty() {
        format!("Starting {tool_name} at {time_str}")
    } else {
        format!("Starting {tool_name} ({args_summary}) at {time_str}")
    }
}

#[cfg(feature = "tui")]
fn format_friendly_end(op: &crate::state::Operation) -> String {
    let status_str = match op.status {
        crate::state::OpStatus::Succeeded => "Finished successfully",
        crate::state::OpStatus::Failed => "Failed",
        crate::state::OpStatus::Cancelled => "Cancelled",
        _ => "Finished",
    };

    let duration_str = if let Some(ms) = op.duration_ms {
        if ms < 1000 {
            format!("{ms}ms")
        } else {
            format!("{:.2}s", ms as f64 / 1000.0)
        }
    } else {
        op.elapsed_display()
    };

    let mut end_text = format!("{status_str} in {duration_str}");

    if let Some(summary) = &op.result_summary {
        let clean_summary = clean_up_summary(summary);
        if !clean_summary.is_empty() {
            end_text.push_str(&format!(": {clean_summary}"));
        }
    }

    end_text
}

#[cfg(feature = "tui")]
fn update_existing_window(
    w: &mut crate::state::TuiWindow,
    op: &crate::state::Operation,
    unicode: bool,
) {
    w.status = match op.status {
        crate::state::OpStatus::Running => "Running".to_string(),
        crate::state::OpStatus::Pending => "Pending".to_string(),
        crate::state::OpStatus::Succeeded => "Finished".to_string(),
        crate::state::OpStatus::Failed => "Error".to_string(),
        crate::state::OpStatus::Cancelled => "Cancelled".to_string(),
        crate::state::OpStatus::Waiting => "Pending".to_string(),
    };

    if op.status != crate::state::OpStatus::Running
        && op.status != crate::state::OpStatus::Pending
        && op.status != crate::state::OpStatus::Waiting
        && w.finished_at.is_none()
    {
        w.finished_at = Some(std::time::Instant::now());
    }

    let mut content = vec![];
    let start_text = format_friendly_start(&op.tool_name, &op.description, op.started_time);
    content.push(start_text);

    if op.status != crate::state::OpStatus::Running
        && op.status != crate::state::OpStatus::Pending
        && op.status != crate::state::OpStatus::Waiting
    {
        let sep = if unicode {
            "────────────────────────────────────────".to_string()
        } else {
            "----------------------------------------".to_string()
        };
        content.push(sep);
        content.push(format_friendly_end(op));
    } else {
        content.push("____".to_string());
    }

    w.content = content;
}

#[cfg(feature = "tui")]
fn build_new_window(
    op: &crate::state::Operation,
    state: &mut crate::state::AppState,
) -> crate::state::TuiWindow {
    let win_id = state.next_window_id;
    state.next_window_id = (state.next_window_id + 1) % 100;

    let label = format!(
        "Operation: {} (instance: {})",
        op.tool_name,
        op.instance_label.as_deref().unwrap_or("local")
    );

    let status = match op.status {
        crate::state::OpStatus::Running => "Running".to_string(),
        crate::state::OpStatus::Pending => "Pending".to_string(),
        crate::state::OpStatus::Succeeded => "Finished".to_string(),
        crate::state::OpStatus::Failed => "Error".to_string(),
        crate::state::OpStatus::Cancelled => "Cancelled".to_string(),
        crate::state::OpStatus::Waiting => "Pending".to_string(),
    };

    let mut content = vec![];
    let start_text = format_friendly_start(&op.tool_name, &op.description, op.started_time);
    content.push(start_text);

    if op.status != crate::state::OpStatus::Running
        && op.status != crate::state::OpStatus::Pending
        && op.status != crate::state::OpStatus::Waiting
    {
        let sep = if state.unicode {
            "────────────────────────────────────────".to_string()
        } else {
            "----------------------------------------".to_string()
        };
        content.push(sep);
        content.push(format_friendly_end(op));
    } else {
        content.push("____".to_string());
    }

    let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
    let op_id = op.id.clone();
    let bridge_tx = state.bridge_tx.clone();
    let mcp_config = mcp_chat_config(state);

    tokio::spawn(async move {
        if let Ok(()) = abort_rx.await
            && let Some(tx) = bridge_tx
        {
            crate::llm_bridge::spawn_tool_call_task(
                "cancel".to_string(),
                serde_json::json!({ "id": op_id }),
                mcp_config,
                tx,
            );
        }
    });

    crate::state::TuiWindow {
        id: win_id,
        label,
        status,
        content,
        collapsed: false,
        finished_at: if op.status != crate::state::OpStatus::Running
            && op.status != crate::state::OpStatus::Pending
            && op.status != crate::state::OpStatus::Waiting
        {
            Some(std::time::Instant::now())
        } else {
            None
        },
        is_cli: true,
        command: op.tool_name.clone(),
        working_dir: op.cwd.clone().unwrap_or_else(|| state.workspace.clone()),
        llm_model: None,
        visible: true,
        abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(Some(abort_tx))),
        op_id: Some(op.id.clone()),
    }
}

#[cfg(feature = "tui")]
fn sync_operations_to_windows(state: &mut crate::state::AppState) {
    let mut to_add = Vec::new();
    let ops = state.operations.clone();

    for op in &ops {
        if let Some(w) = state
            .windows
            .iter_mut()
            .find(|w| w.op_id.as_deref() == Some(&op.id))
        {
            update_existing_window(w, op, state.unicode);
        } else {
            let w = build_new_window(op, state);
            to_add.push(w);
        }
    }

    for w in to_add {
        state.windows.push(w);
        if state.windows.len() > 100 {
            state.windows.remove(0);
        }
    }
}

// ─── Source event handler ────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn handle_event_tools_list_updated(
    tools: Vec<crate::mcp_connections::ToolInfo>,
    state: &mut crate::state::AppState,
) {
    let mut merged = tools;
    for t in state.mcp_connections.aggregate_tools() {
        if !merged.iter().any(|existing| existing.name == t.name) {
            merged.push(t);
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    state.tools_list = merged;
}

#[cfg(feature = "tui")]
fn handle_event_log_files_updated(
    files: Vec<crate::state::LogFileInfo>,
    state: &mut crate::state::AppState,
) {
    state.log_files = files;
    if state.active_log_file.is_none() && !state.log_files.is_empty() {
        // Default to first file
        let first = state.log_files[0].name.clone();
        state.active_log_file = Some(first.clone());
        if let Some(ref tx) = state.mcp_source_tx {
            let _ = tx.try_send(crate::mcp_source::McpSourceCommand::SetActiveFile(Some(
                first,
            )));
        }
    }
}

#[cfg(feature = "tui")]
fn handle_event_log_lines_updated(
    file: String,
    content: String,
    append: bool,
    state: &mut crate::state::AppState,
) {
    if Some(&file) == state.active_log_file.as_ref() {
        if append {
            for line in content.lines() {
                state.active_log_lines.push(line.to_string());
            }
            if state.active_log_lines.len() > 2000 {
                let drain_len = state.active_log_lines.len() - 2000;
                state.active_log_lines.drain(0..drain_len);
            }
        } else {
            state.active_log_lines = content.lines().map(String::from).collect();
        }
    }
}

#[cfg(feature = "tui")]
fn handle_source_event(event: crate::mcp_source::SourceEvent, state: &mut crate::state::AppState) {
    use crate::mcp_source::SourceEvent;
    match event {
        SourceEvent::HealthChanged { healthy } => state.server_healthy = healthy,
        SourceEvent::DaemonHealthChanged { healthy } => state.daemon_healthy = healthy,
        SourceEvent::OperationsUpdated { ops } => {
            for op in ops {
                state.upsert_operation(op);
            }
            sync_operations_to_windows(state);
        }
        SourceEvent::AiActivity(entry) => state.push_activity(entry),
        SourceEvent::LogLine(entry) => state.push_log(entry),
        SourceEvent::ToolsListUpdated { tools } => {
            handle_event_tools_list_updated(tools, state);
        }
        SourceEvent::SandboxStatus { status } => state.sandbox_status = status,
        SourceEvent::SessionId { id } => state.session_id = Some(id),
        SourceEvent::LogFilesUpdated { files } => {
            handle_event_log_files_updated(files, state);
        }
        SourceEvent::LogLinesUpdated {
            file,
            content,
            append,
        } => {
            handle_event_log_lines_updated(file, content, append, state);
        }
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

fn http_base_url(connection: &ResolvedConnection) -> String {
    match &connection.transport {
        crate::connection::ResolvedTransport::Http(url)
        | crate::connection::ResolvedTransport::Http3(url) => url.clone(),
        #[cfg(unix)]
        crate::connection::ResolvedTransport::UnixSocket(path) => {
            std::env::var("AHMA_HTTP_URL").unwrap_or_else(|_| format!("unix://{}", path))
        }
    }
}

#[cfg(feature = "tui")]
fn run_next_pending_window(state: &mut crate::state::AppState) {
    if let Some(pos) = state.windows.iter().position(|w| w.status == "Pending") {
        let win_id = state.windows[pos].id;
        start_window_execution(win_id, state);
    }
}

#[cfg(feature = "tui")]
fn start_window_execution(win_id: usize, state: &mut crate::state::AppState) {
    use crate::llm_bridge::{spawn_window_cli_task, spawn_window_llm_task};

    let (base_url, model) = parse_llm_selection(state);

    let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id) else {
        return;
    };
    w.status = "Running".to_string();

    let is_cli = w.is_cli;
    let command = w.command.clone();
    let working_dir = w.working_dir.clone();
    let bridge_tx = state.bridge_tx.clone();

    let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
    if let Ok(mut guard) = w.abort_tx.try_lock() {
        *guard = Some(abort_tx);
    }

    if is_cli {
        if let Some(tx) = bridge_tx {
            spawn_window_cli_task(win_id, command, working_dir, abort_rx, tx);
        }
    } else {
        if let Some(tx) = bridge_tx {
            spawn_window_llm_task(win_id, base_url, model, command, abort_rx, tx);
        }
    }
}

#[cfg(feature = "tui")]
fn close_window_by_id(win_id: usize, state: &mut crate::state::AppState) {
    if let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id) {
        if let Ok(mut guard) = w.abort_tx.try_lock()
            && let Some(abort_tx) = guard.take()
        {
            let _ = abort_tx.send(());
        }
        w.visible = false;
        w.status = "Cancelled".to_string();
        w.finished_at = Some(std::time::Instant::now());
    }
}

#[cfg(feature = "tui")]
fn handle_monitor_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if !cmd.starts_with("/monitor file ") {
        return false;
    }

    let rest = cmd.strip_prefix("/monitor file ").unwrap().trim();
    let (path, prompt) = if rest.starts_with('"') {
        let mut chars = rest.chars().skip(1);
        let mut path_str = String::new();
        let mut closed = false;
        for c in chars.by_ref() {
            if c == '"' {
                closed = true;
                break;
            }
            path_str.push(c);
        }
        if closed {
            (path_str, chars.collect::<String>().trim().to_string())
        } else {
            (rest.to_string(), String::new())
        }
    } else {
        let parts: Vec<&str> = rest.splitn(2, |c: char| c.is_whitespace()).collect();
        let path = parts[0].to_string();
        let prompt = parts.get(1).map(|&s| s.to_string()).unwrap_or_default();
        (path, prompt)
    };

    if path.is_empty() {
        push_assistant_message(state, "Usage: /monitor file <path> [prompt]");
        return true;
    }

    let Some(tx) = &state.bridge_tx else {
        push_assistant_message(state, "Bridge not available.");
        return true;
    };

    let (base_url, model) = parse_llm_selection(state);
    let mut args = serde_json::json!({
        "file_path": path,
    });
    if !prompt.is_empty() {
        args["detection_prompt"] = serde_json::Value::String(prompt);
    }
    if !base_url.is_empty() {
        args["llm_base_url"] = serde_json::Value::String(base_url);
        args["llm_model"] = serde_json::Value::String(model);
    }

    crate::llm_bridge::spawn_tool_call_task(
        "log_monitor".to_string(),
        args,
        mcp_chat_config(state),
        tx.clone(),
    );
    push_assistant_message(
        state,
        format!("Started log monitor on `{}` in the background.", path),
    );
    true
}

#[cfg(feature = "tui")]
fn handle_analyze_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd == "/analyze" {
        let op_id = state.selected_op().map(|op| op.id.clone());
        if let Some(id) = op_id {
            analyze_operation(state, &id);
        } else {
            push_assistant_message(
                state,
                "No operation selected for analysis. Usage: /analyze [op_id]",
            );
        }
        return true;
    }

    if let Some(op_id) = cmd.strip_prefix("/analyze ") {
        let op_id = op_id.trim();
        if op_id.is_empty() {
            push_assistant_message(state, "Usage: /analyze [op_id]");
            return true;
        }
        analyze_operation(state, op_id);
        return true;
    }

    false
}

#[cfg(feature = "tui")]
fn analyze_operation(state: &mut crate::state::AppState, op_id: &str) {
    let extracted = {
        let Some(op) = state.operations.iter().find(|o| o.id == op_id) else {
            push_assistant_message(state, format!("Operation `{op_id}` not found."));
            return;
        };
        let stdout_str = op
            .stdout_tail
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let alerts_str = op.alerts.join("\n");
        Some((
            op.id.clone(),
            op.tool_name.clone(),
            op.status.clone(),
            op.args.clone(),
            alerts_str,
            stdout_str,
        ))
    };

    let Some((id, tool_name, status, args, alerts_str, stdout_str)) = extracted else {
        return;
    };

    let prompt = format!(
        "Analyze the following operation:\n\
         - ID: {}\n\
         - Tool: {}\n\
         - Status: {:?}\n\
         - Command/Args: {:?}\n\
         - Alerts:\n{}\n\
         - Stdout tail:\n{}",
        id, tool_name, status, args, alerts_str, stdout_str
    );

    state.chat.push(crate::state::ChatEntry::User(format!(
        "Analyze operation {}",
        id
    )));
    state.chat.push(crate::state::ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

    let Some(tx) = &state.bridge_tx else {
        return;
    };

    let (base_url, model) = parse_llm_selection(state);
    if base_url.is_empty() {
        push_assistant_message(state, "No LLM configured. Use /provider to select one.");
        return;
    }

    use ahma_llm_monitor::client::LlmClient;
    let client = LlmClient::new(base_url, model, None);
    let system = state.mcp_enabled.then(|| {
        "Use ahma tools when they would materially improve the answer. Prefer direct answers when no tool is needed.".to_string()
    });

    let mut history = collect_chat_history(state);
    if let Some(last_msg) = history.last_mut() {
        last_msg.content = prompt;
    }

    if state.mcp_enabled {
        crate::llm_bridge::spawn_agent_task(
            client,
            history,
            system,
            optional_mcp_chat_config(state),
            state.tools_list.clone(),
            tx.clone(),
        );
    } else {
        crate::llm_bridge::spawn_chat_task(
            client,
            history,
            system,
            optional_mcp_chat_config(state),
            tx.clone(),
        );
    }
}

#[cfg(feature = "tui")]
#[cfg(feature = "tui")]
#[inline]
fn inside_rect(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

#[cfg(feature = "tui")]
fn handle_click_target(target: crate::state::ClickTarget, state: &mut crate::state::AppState) {
    use crate::state::ClickTarget;
    match target {
        ClickTarget::CancelOperation(op_id) => {
            let id = op_id.clone();
            state.push_log(crate::state::LogEntry {
                timestamp: chrono::Local::now(),
                level: crate::state::LogLevel::Info,
                message: format!("Cancel requested: {id}"),
            });
            if let Some(tx) = &state.bridge_tx {
                let mcp_config = mcp_chat_config(state);
                crate::llm_bridge::spawn_tool_call_task(
                    "cancel".to_string(),
                    serde_json::json!({ "id": id }),
                    mcp_config,
                    tx.clone(),
                );
            }
            state.focus = crate::state::Focus::OpsDag;
        }
        ClickTarget::PinOperation(op_id) => {
            if let Some(op) = state.operations.iter_mut().find(|o| o.id == op_id) {
                op.pinned = !op.pinned;
            }
            state.focus = crate::state::Focus::OpsDag;
        }
        ClickTarget::AnalyzeOperation(op_id) => {
            analyze_operation(state, &op_id);
            state.focus = crate::state::Focus::OpsDag;
        }
        ClickTarget::SelectOperation(op_idx) => {
            state.ops_selected = op_idx;
            state.focus = crate::state::Focus::OpsDag;
        }
        ClickTarget::CloseWindow(win_id) => {
            close_window_by_id(win_id, state);
        }
        ClickTarget::ToggleWindow(win_id) => {
            if let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id) {
                w.collapsed = !w.collapsed;
            }
        }
    }
}

#[cfg(feature = "tui")]
fn handle_window_rect_click(col: u16, row: u16, state: &mut crate::state::AppState) -> bool {
    let mut clicked_close = None;
    let mut clicked_toggle = None;

    let rects = state.window_rects.borrow().clone();
    for &(win_id, rect) in &rects {
        if inside_rect(col, row, rect) {
            let is_close_click = if rect.height == 1 {
                col >= rect.x + rect.width.saturating_sub(5)
            } else {
                row == rect.y && col >= rect.x + rect.width.saturating_sub(5)
            };

            if is_close_click {
                clicked_close = Some(win_id);
            } else {
                clicked_toggle = Some(win_id);
            }
            break;
        }
    }

    if let Some(win_id) = clicked_close {
        close_window_by_id(win_id, state);
        true
    } else if let Some(win_id) = clicked_toggle
        && let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id)
    {
        w.collapsed = !w.collapsed;
        true
    } else {
        false
    }
}

#[cfg(feature = "tui")]
fn handle_mouse_click(col: u16, row: u16, state: &mut crate::state::AppState) {
    let click_targets = state.click_targets.borrow().clone();
    for (target, rect) in click_targets {
        if inside_rect(col, row, rect) {
            handle_click_target(target, state);
            return;
        }
    }

    if handle_window_rect_click(col, row, state) {
        return;
    }

    if inside_rect(col, row, state.chat_input_area.get()) {
        state.focus = crate::state::Focus::Chat;
        return;
    }
    if inside_rect(col, row, state.chat_area.get()) {
        state.focus = crate::state::Focus::Chat;
        return;
    }
    if inside_rect(col, row, state.log_area.get()) {
        state.focus = crate::state::Focus::Log;
        return;
    }
    if inside_rect(col, row, state.ops_area.get()) {
        state.focus = crate::state::Focus::OpsDag;
        return;
    }
    if inside_rect(col, row, state.detail_area.get()) {
        state.focus = crate::state::Focus::OpsDag;
    }
}

#[cfg(feature = "tui")]
fn handle_mouse_scroll(col: u16, row: u16, up: bool, state: &mut crate::state::AppState) {
    let chat_area = state.chat_area.get();
    if col >= chat_area.x
        && col < chat_area.x + chat_area.width
        && row >= chat_area.y
        && row < chat_area.y + chat_area.height
    {
        if up {
            let max = state.chat_max_scroll.get();
            state.chat_scroll = (state.chat_scroll + 1).min(max);
        } else {
            state.chat_scroll = state.chat_scroll.saturating_sub(1);
        }
        state.sync_chat_scroll_to_animation();
        return;
    }

    let log_area = state.log_area.get();
    if col >= log_area.x
        && col < log_area.x + log_area.width
        && row >= log_area.y
        && row < log_area.y + log_area.height
    {
        if up {
            state.log_scroll = state.log_scroll.saturating_sub(1);
        } else {
            let max = state.log_max_scroll.get();
            state.log_scroll = (state.log_scroll + 1).min(max);
        }
        state.sync_log_scroll_to_animation();
    }
}

#[cfg(feature = "tui")]
fn handle_page_up_down(up: bool, state: &mut crate::state::AppState) {
    let mut scrolled_panel = None;

    if let Some((col, row)) = state.last_mouse_pos.get() {
        let chat_area = state.chat_area.get();
        let log_area = state.log_area.get();

        if col >= chat_area.x
            && col < chat_area.x + chat_area.width
            && row >= chat_area.y
            && row < chat_area.y + chat_area.height
        {
            scrolled_panel = Some("chat");
        } else if col >= log_area.x
            && col < log_area.x + log_area.width
            && row >= log_area.y
            && row < log_area.y + log_area.height
        {
            scrolled_panel = Some("log");
        }
    }

    let panel = scrolled_panel.unwrap_or_else(|| {
        if state.mode == crate::state::Mode::Monitor && state.focus == crate::state::Focus::Log {
            "log"
        } else {
            "chat"
        }
    });

    if panel == "chat" {
        let height = state.chat_area.get().height;
        let page_size = if height > 2 { height - 2 } else { 10 } as f64;
        let max_scroll = state.chat_max_scroll.get() as f64;
        let current_target = state.chat_scroll_target.get();
        let new_target = if up {
            (current_target + page_size).min(max_scroll)
        } else {
            (current_target - page_size).max(0.0)
        };
        state.chat_scroll_target.set(new_target);
    } else {
        let height = state.log_area.get().height;
        let page_size = if height > 2 { height - 2 } else { 10 } as f64;
        let max_scroll = state.log_max_scroll.get() as f64;
        let current_target = state.log_scroll_target.get();
        let new_target = if up {
            (current_target - page_size).max(0.0)
        } else {
            (current_target + page_size).min(max_scroll)
        };
        state.log_scroll_target.set(new_target);
    }
}

#[cfg(feature = "tui")]
fn update_scroll_animations(state: &mut crate::state::AppState) {
    let chat_curr = state.chat_scroll_current.get();
    let chat_tgt = state.chat_scroll_target.get();
    if (chat_curr - chat_tgt).abs() > 0.01 {
        let next = chat_curr + (chat_tgt - chat_curr) * 0.25;
        state.chat_scroll_current.set(next);
        state.chat_scroll = next.round() as usize;
    } else {
        state.chat_scroll_current.set(chat_tgt);
        state.chat_scroll = chat_tgt.round() as usize;
    }

    let log_curr = state.log_scroll_current.get();
    let log_tgt = state.log_scroll_target.get();
    if (log_curr - log_tgt).abs() > 0.01 {
        let next = log_curr + (log_tgt - log_curr) * 0.25;
        state.log_scroll_current.set(next);
        state.log_scroll = next.round() as usize;
    } else {
        state.log_scroll_current.set(log_tgt);
        state.log_scroll = log_tgt.round() as usize;
    }

    let input_curr = state.chat_input_height_current.get();
    let input_tgt = state.chat_input_height_target.get();
    if (input_curr - input_tgt).abs() > 0.01 {
        let next = input_curr + (input_tgt - input_curr) * 0.25;
        state.chat_input_height_current.set(next);
    } else {
        state.chat_input_height_current.set(input_tgt);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_run_command;
    use serde_json::json;

    #[test]
    fn parse_run_command_defaults_to_empty_object() {
        let (tool, args) = parse_run_command("status").unwrap();
        assert_eq!(tool, "status");
        assert_eq!(args, json!({}));
    }

    #[test]
    fn parse_run_command_accepts_json_object() {
        let (tool, args) = parse_run_command("await {\"id\":\"op_123\"}").unwrap();
        assert_eq!(tool, "await");
        assert_eq!(args, json!({"id": "op_123"}));
    }

    #[test]
    fn parse_run_command_rejects_invalid_json() {
        let error = parse_run_command("status {not-json}").unwrap_err();
        assert!(error.contains("Invalid JSON arguments"));
    }

    #[test]
    fn parse_run_command_rejects_non_object_json() {
        let error = parse_run_command("status []").unwrap_err();
        assert!(error.contains("expects a JSON object"));
    }

    #[test]
    fn test_handle_window_nav_commands() {
        use crate::state::{AppState, TuiWindow};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        let w = TuiWindow {
            id: 3,
            label: "Test Window".to_string(),
            status: "Running".to_string(),
            content: vec![],
            collapsed: true,
            finished_at: None,
            is_cli: true,
            command: "echo test".to_string(),
            working_dir: state.workspace.clone(),
            llm_model: None,
            visible: false,
            abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            op_id: None,
        };
        state.windows.push(w);

        // Test /3 to restore and expand
        let handled = super::handle_window_nav_commands("/3", &mut state);
        assert!(handled);
        assert!(state.windows[0].visible);
        assert!(!state.windows[0].collapsed);

        // Test /x3 to close
        let handled_close = super::handle_window_nav_commands("/x3", &mut state);
        assert!(handled_close);
        assert!(!state.windows[0].visible);
        assert_eq!(state.windows[0].status, "Cancelled");

        // Test /exit to quit
        let handled_exit = super::handle_window_nav_commands("/exit", &mut state);
        assert!(handled_exit);
        assert!(state.should_quit);

        // Test /q and /quit to quit
        let mut state_q = AppState::new("http://localhost:3000", "HTTP", true);
        let handled_q = super::handle_window_nav_commands("/q", &mut state_q);
        assert!(handled_q);
        assert!(state_q.should_quit);

        let mut state_quit = AppState::new("http://localhost:3000", "HTTP", true);
        let handled_quit = super::handle_window_nav_commands("/quit", &mut state_quit);
        assert!(handled_quit);
        assert!(state_quit.should_quit);
    }

    #[test]
    fn test_needs_approval_filtering() {
        use crate::llm_bridge::needs_approval;
        // Gating write_file and replace_in_file by default
        assert!(needs_approval("write_file", false));
        assert!(needs_approval("replace_in_file", false));
        assert!(needs_approval("srv::write_file", false));
        assert!(needs_approval("srv::replace_in_file", false));
        // Not gating read_file by default
        assert!(!needs_approval("read_file", false));
        assert!(!needs_approval("srv::read_file", false));

        // When tool_approval is enabled, all tools need approval
        assert!(needs_approval("read_file", true));
        assert!(needs_approval("srv::list_dir", true));
    }

    #[test]
    fn test_resolve_approval_signaling() {
        use crate::state::AppState;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        let (tx, rx) = tokio::sync::oneshot::channel();
        state.approval = Some(crate::state::ApprovalGate {
            op_id: "op_test".to_string(),
            description: "test".to_string(),
            deadline: None,
            diff: None,
        });
        state.approval_tx = Some(tx);

        super::resolve_approval(&mut state, true);
        assert!(state.approval.is_none());
        assert!(state.approval_tx.is_none());

        let approved = rx.blocking_recv().unwrap();
        assert!(approved);
    }

    #[test]
    fn test_handle_mouse_click_focus_change() {
        use crate::state::{AppState, Focus};
        use ratatui::layout::Rect;

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.chat_input_area.set(Rect::new(10, 10, 20, 5));
        state.chat_area.set(Rect::new(10, 0, 20, 10));
        state.log_area.set(Rect::new(30, 0, 20, 15));

        // Start with Focus::OpsDag
        state.focus = Focus::OpsDag;

        // Click on Chat Input Area -> should change focus to Chat
        super::handle_mouse_click(15, 12, &mut state);
        assert_eq!(state.focus, Focus::Chat);

        // Click on Log Area -> should change focus to Log
        super::handle_mouse_click(35, 5, &mut state);
        assert_eq!(state.focus, Focus::Log);

        // Click on Chat Area -> should change focus to Chat
        super::handle_mouse_click(15, 5, &mut state);
        assert_eq!(state.focus, Focus::Chat);
    }

    #[test]
    fn test_handle_page_up_down_scrolling() {
        use crate::state::{AppState, Focus};
        use ratatui::layout::Rect;

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.chat_area.set(Rect::new(0, 0, 80, 20)); // height = 20, page_size = 18
        state.log_area.set(Rect::new(0, 20, 80, 10)); // height = 10, page_size = 8

        state.chat_max_scroll.set(100);
        state.log_max_scroll.set(50);

        // Case 1: Mouse not over TUI, focus is Chat -> PageUp should scroll Chat
        state.focus = Focus::Chat;
        state.last_mouse_pos.set(None);
        super::handle_page_up_down(true, &mut state);
        assert_eq!(state.chat_scroll_target.get(), 18.0);

        // PageDown Chat
        super::handle_page_up_down(false, &mut state);
        assert_eq!(state.chat_scroll_target.get(), 0.0);

        // Case 2: Mouse over Log Area -> PageUp/PageDown should scroll Log, even if focused on Chat
        state.last_mouse_pos.set(Some((10, 25))); // over log area
        super::handle_page_up_down(false, &mut state); // PageDown log -> target increases (shows newer logs)
        assert_eq!(state.log_scroll_target.get(), 8.0);

        super::handle_page_up_down(true, &mut state); // PageUp log -> target decreases
        assert_eq!(state.log_scroll_target.get(), 0.0);
    }
}
