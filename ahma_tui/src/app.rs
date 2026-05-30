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
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    };
    use futures::StreamExt;
    use ratatui::{Terminal, backend::CrosstermBackend};
    use tokio::sync::mpsc;

    use crate::keymap::map_key;
    use crate::llm_bridge::{BridgeEvent, spawn_discovery_task};
    use crate::daemon_source::spawn_daemon_source;
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
    state.mcp_http_base_url = http_base_url(connection);

    let (mcp_tx, mut mcp_rx) = mpsc::channel::<SourceEvent>(256);
    spawn_mcp_source(connection.clone(), mcp_tx.clone());
    // Also subscribe to the hub daemon so stdio instances spawned by IDEs are visible.
    spawn_daemon_source(mcp_tx);

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
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let loop_result: Result<()> = async {
        loop {
            tokio::select! {
                biased;

                maybe = event_stream.next() => {
                    match maybe {
                        Some(Ok(Event::Key(key))) => {
                            if handle_help_key(key, &mut state)
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
                                );
                                handle_action(action, &mut state);
                            }
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

                Some(src_event) = mcp_rx.recv() => {
                    handle_source_event(src_event, &mut state);
                }

                Some(bridge_event) = bridge_rx.recv() => {
                    handle_bridge_event(bridge_event, &mut state);
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
    if handle_picker_action(&action, state)
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
        Action::ToggleDetail | Action::AwaitOp | Action::Unknown | Action::Enter => {}
        _ => {}
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
    let Some(model) = picker.selected_item() else {
        return;
    };

    let provider = provider_label(&state.llm_label);
    state.llm_label = format!("{provider} / {model}");
    save_session(state);
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
        Focus::Log => state.log_scroll = state.log_scroll.saturating_sub(1),
        Focus::Chat => {
            let max = state.chat.len().saturating_sub(1);
            state.chat_scroll = (state.chat_scroll + 1).min(max);
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
            let visible = state.filtered_log().len();
            state.log_scroll = (state.log_scroll + 1).min(visible.saturating_sub(1));
        }
        Focus::Chat => state.chat_scroll = state.chat_scroll.saturating_sub(1),
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn move_focus_to_top(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::OpsDag => state.ops_selected = 0,
        Focus::AiActivity => state.activity_scroll = 0,
        Focus::Log => state.log_scroll = 0,
        Focus::Chat => state.chat_scroll = state.chat.len().saturating_sub(1),
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
            let visible = state.filtered_log().len();
            state.log_scroll = visible.saturating_sub(1);
        }
        Focus::Chat => state.chat_scroll = 0,
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
    let tools = state.tools_list.clone();
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
        }
        Action::FilterChar(c) => {
            state.log_filter.push(*c);
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
    use crate::llm_bridge::{McpChatConfig, spawn_chat_task};
    use crate::state::ChatEntry;
    use ahma_llm_monitor::{ChatMessage, LlmClient};

    let text = state.chat_input_text().trim().to_string();
    if text.is_empty() {
        return;
    }
    state.clear_chat_input();

    let (base_url, model) = parse_llm_selection(state);
    if base_url.is_empty() {
        state.chat.push(ChatEntry::Assistant {
            content: "No LLM configured. Use /provider to select one.".to_string(),
            streaming: false,
        });
        return;
    }

    state.chat.push(ChatEntry::User(text.clone()));
    state.chat.push(ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

    let messages: Vec<ChatMessage> = state
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
        .collect();

    if let Some(tx) = &state.bridge_tx {
        let client = LlmClient::new(base_url, model, None);
        let system = state.mcp_enabled.then(|| {
            "Use ahma tools when they would materially improve the answer. Prefer direct answers when no tool is needed.".to_string()
        });
        let mcp =
            (state.mcp_enabled && !state.mcp_http_base_url.is_empty()).then(|| McpChatConfig {
                base_url: state.mcp_http_base_url.clone(),
                workspace_root: std::path::PathBuf::from(&state.workspace),
                session_id: state.session_id.clone(),
            });
        spawn_chat_task(client, messages, system, mcp, tx.clone());
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
    let tools = state.tools_list.clone();
    state.navigator.open(&tools);
}

#[cfg(feature = "tui")]
fn refresh_navigator_completions(state: &mut crate::state::AppState) {
    let tools = state.tools_list.clone();
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
    use crate::state::{Focus, Mode};
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.mode != Mode::Chat
        || state.focus != Focus::Chat
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
            let tools = state.tools_list.clone();
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

/// Dispatch a `/command` string from the navigator.
#[cfg(feature = "tui")]
fn dispatch_nav_command(cmd: &str, state: &mut crate::state::AppState) {
    let cmd = cmd.trim();
    if handle_basic_nav_command(cmd, state)
        || handle_mode_nav_command(cmd, state)
        || handle_mcp_nav_command(cmd, state)
        || handle_tools_nav_command(cmd, state)
        || handle_approval_nav_command(cmd, state)
        || handle_picker_nav_command(cmd, state)
        || handle_run_nav_command(cmd, state)
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
        "/help" => state.show_help = true,
        "/clear" => state.chat.clear(),
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
fn handle_mcp_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    match cmd {
        "/mcp on" => set_mcp_enabled(state, true),
        "/mcp off" => set_mcp_enabled(state, false),
        _ => return false,
    }

    true
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

    let message = if state.tools_list.is_empty() {
        "No tools discovered yet.".to_string()
    } else {
        let mut content = format!("{} tool(s) available:\n", state.tools_list.len());
        for tool in &state.tools_list {
            content.push_str(&format!("- {tool}\n"));
        }
        content.trim_end().to_string()
    };
    push_assistant_message(state, message);
    true
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
        .map(|(name, url)| format!("{name}  {url}"))
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
            .find(|(_, url)| url == current_url)
            .map(|(name, url)| format!("{name}  {url}"))
            .unwrap_or_default();
        picker.select_exact(&selected);
    }
    state.provider_picker = Some(picker);
}

#[cfg(feature = "tui")]
fn open_model_picker(state: &mut crate::state::AppState) {
    use crate::llm_bridge::spawn_model_refresh;
    use crate::state::PickerState;

    if state.available_models.is_empty() {
        let (base_url, _) = parse_llm_selection(state);
        if !base_url.is_empty()
            && let Some(tx) = &state.bridge_tx
        {
            spawn_model_refresh(base_url, tx.clone());
        }
        push_assistant_message(state, "Fetching model list…");
        return;
    }

    let mut picker = PickerState::new("Select model", state.available_models.clone());
    let selected_model = state.selected_model();
    if !selected_model.is_empty() {
        picker.select_exact(&selected_model);
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

    let Some(tx) = &state.bridge_tx else {
        return;
    };

    if state.mcp_http_base_url.is_empty() {
        push_assistant_message(
            state,
            "Cannot run tools because the ahma MCP bridge URL is unavailable.",
        );
        return;
    }

    if !state.tools_list.is_empty() && !state.tools_list.iter().any(|known| known == tool) {
        push_assistant_message(
            state,
            format!("Unknown tool `{tool}`. Use /tools to inspect the current tool list."),
        );
        return;
    }

    crate::llm_bridge::spawn_tool_call_task(
        tool.to_string(),
        arguments,
        crate::llm_bridge::McpChatConfig {
            base_url: state.mcp_http_base_url.clone(),
            workspace_root: std::path::PathBuf::from(&state.workspace),
            session_id: state.session_id.clone(),
        },
        tx.clone(),
    );
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
    if provider_name.starts_with("http") {
        return (provider_name, model);
    }
    let base_url = if provider_name.to_lowercase().contains("ollama") {
        "http://localhost:11434/v1".to_string()
    } else if provider_name.to_lowercase().contains("llama") {
        "http://localhost:8080/v1".to_string()
    } else {
        provider_name
    };
    (base_url, model)
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
    let first = providers.first().cloned();
    state.available_providers = providers
        .iter()
        .map(|p| (p.name.clone(), p.base_url.clone()))
        .collect();

    if state.llm_label == "no LLM" {
        if let Some(p) = first {
            let model = p.models.first().cloned().unwrap_or_default();
            state.available_models = p.models;
            state.current_provider_url = Some(p.base_url.clone());
            state.llm_label = format!("{} / {}", p.name, model);
            save_session(state);
        }
    } else if let Some(current_url) = &state.current_provider_url
        && let Some(provider) = providers
            .iter()
            .find(|provider| &provider.base_url == current_url)
    {
        let model = state.selected_model();
        state.available_models = provider.models.clone();
        if !model.is_empty() {
            state.llm_label = format!("{} / {}", provider.name, model);
        }
    }
}

#[cfg(feature = "tui")]
fn handle_models_refreshed(
    base_url: String,
    models: Vec<String>,
    state: &mut crate::state::AppState,
) {
    if state.current_provider_url.as_deref() == Some(base_url.as_str()) && !models.is_empty() {
        use crate::state::PickerState;
        state.available_models = models.clone();
        let mut picker = PickerState::new("Select model", models);
        let selected_model = state.selected_model();
        if !selected_model.is_empty() {
            picker.select_exact(&selected_model);
        }
        state.model_picker = Some(picker);
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
        BridgeEvent::Done => {
            state.chat.finish_stream();
        }
        BridgeEvent::Error(msg) => {
            state.chat.finish_stream();
            state.chat.push(ChatEntry::Assistant {
                content: format!("Error: {msg}"),
                streaming: false,
            });
            state.chat_scroll = 0;
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

fn http_base_url(connection: &ResolvedConnection) -> String {
    match &connection.transport {
        crate::connection::ResolvedTransport::Http(url)
        | crate::connection::ResolvedTransport::Http3(url) => url.clone(),
        #[cfg(unix)]
        crate::connection::ResolvedTransport::UnixSocket(_) => {
            std::env::var("AHMA_HTTP_URL").unwrap_or_else(|_| "http://localhost:3000".to_string())
        }
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
}
