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
pub async fn run(
    connection: &ResolvedConnection,
    profile: Option<String>,
    path: Option<std::path::PathBuf>,
    token_prefs: crate::TokenPrefs,
) -> Result<()> {
    #[cfg(feature = "tui")]
    return run_ratatui(connection, profile, path, token_prefs).await;

    #[cfg(not(feature = "tui"))]
    {
        let _ = token_prefs;
        return run_text_stub(connection).await;
    }
}

// ─── Ratatui implementation (feature = "tui") ─────────────────────────────────

#[cfg(feature = "tui")]
async fn run_ratatui(
    connection: &ResolvedConnection,
    profile_override: Option<String>,
    workspace_path: Option<std::path::PathBuf>,
    token_prefs: crate::TokenPrefs,
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
    if let Some(ref path) = workspace_path {
        state.workspace = path.to_string_lossy().into_owned();
    }
    state.mcp_http_base_url = http_base_url(connection);
    state.token_prefs = token_prefs;

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
    state.mcp_source_tx = Some(spawn_mcp_source(
        connection.clone(),
        mcp_tx.clone(),
        workspace_path,
    ));
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
    // Populate the external MCP tools counter at startup (avoids needing `/mcp refresh`).
    crate::llm_bridge::spawn_external_tools_refresh(
        state.mcp_connections.clone(),
        bridge_tx.clone(),
    );
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
                                && !state.is_help_open()
                                && state.log_files_selected().is_none()
                            {
                                handle_page_up_down(key.code == crossterm::event::KeyCode::PageUp, &mut state);
                            } else if handle_settings_key(key, &mut state)
                                || handle_help_key(key, &mut state)
                                || handle_picker_key(key, &mut state)
                                || handle_approval_key(key, &mut state)
                                || handle_chat_input_key(key, &mut state)
                            {
                                // handled directly by an overlay/editor widget
                            } else {
                                let action = map_key(
                                    key,
                                    state.mode,
                                    state.focus,
                                    &state.modal,
                                    state.log_filter_active,
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
                } else if chat_in_progress(&state) {
                    Duration::from_millis(100)
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
        Action::ToggleHelp => state.toggle_help(),
        Action::FocusChat => {
            state.focus = crate::state::Focus::Chat;
        }
        Action::ToggleDetail | Action::AwaitOp | Action::Unknown | Action::Enter => {}
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn submit_log_switcher(state: &mut crate::state::AppState) {
    let Some(idx) = state.log_files_selected() else {
        return;
    };
    let new_file: Option<String> = if idx == 0 {
        None
    } else {
        let file_idx = idx - 1;
        if file_idx < state.log_files.len() {
            Some(state.log_files[file_idx].name.clone())
        } else {
            state.close_modal();
            return;
        }
    };
    state.active_log_file = new_file.clone();
    state.active_log_lines.clear();
    // A freshly opened/switched log tails from the bottom by default.
    state.log_follow = true;
    state.log_scroll = 0;
    state.sync_log_scroll_to_animation();
    if let Some(ref tx) = state.mcp_source_tx {
        let _ = tx.try_send(crate::mcp_source::McpSourceCommand::SetActiveFile(new_file));
    }
    state.close_modal();
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
        let (minimize_tokens, small_model_harness, context_length) = resolve_token_prefs(state);

        let mcp = crate::llm_bridge::McpChatConfig {
            base_url: state.server_url.clone(),
            workspace_root: std::path::PathBuf::from(&state.workspace),
            session_id: state.session_id.clone(),
            external_http_servers: std::collections::BTreeMap::new(),
            max_turns: 8,
            tool_approval: false,
            mcp_connections: state.mcp_connections.clone(),
            minimize_tokens,
            small_model_harness,
            context_length,
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
            state.open_log_files_modal(0);
            // Proactively request logs list refresh when modal is opened
            if let Some(ref tx) = state.mcp_source_tx {
                let _ = tx.try_send(crate::mcp_source::McpSourceCommand::RefreshLogs);
            }
            true
        }
        Action::CloseLogSwitcher => {
            if state.log_files_selected().is_some() {
                state.close_modal();
            }
            true
        }
        Action::SubmitLogSwitcher => {
            submit_log_switcher(state);
            true
        }
        Action::Up if state.log_files_selected().is_some() => {
            if let Some(sel) = state.log_files_selected()
                && sel > 0
            {
                state.set_log_files_selected(sel - 1);
            }
            true
        }
        Action::Down if state.log_files_selected().is_some() => {
            if let Some(sel) = state.log_files_selected()
                && sel < state.log_files.len()
            {
                state.set_log_files_selected(sel + 1);
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
    if let Some(picker) = state.take_provider_picker() {
        submit_provider_picker(picker, state);
        return;
    }

    if let Some(picker) = state.take_model_picker() {
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
    if matches!(
        state.modal,
        crate::state::ModalState::ProviderPicker(_) | crate::state::ModalState::ModelPicker(_)
    ) {
        state.close_modal();
    }
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
            state.detach_log_follow();
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
        // While following we are already pinned to the bottom — nothing to do
        // (falls through to the no-op arm below).
        Focus::Log if !state.log_follow => {
            let max = state.log_max_scroll.get();
            state.log_scroll = (state.log_scroll + 1).min(max);
            state.sync_log_scroll_to_animation();
            state.maybe_reengage_log_follow();
        }
        Focus::Chat => {
            // Clamp against max too: a resize can shrink the content, leaving a
            // stale chat_scroll above the new max that a lone decrement wouldn't fix.
            let max = state.chat_max_scroll.get();
            state.chat_scroll = state.chat_scroll.min(max).saturating_sub(1);
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
            state.log_follow = false;
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
            // Jump to the newest line and resume tracking new output.
            state.log_follow = true;
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
        Action::ApproveAlways => resolve_approval_always(state),
        Action::Reject => resolve_approval(state, false),
        _ => return false,
    }

    true
}

/// "Always allow": persist a grant for this tool+workspace (so it is never
/// re-prompted), then approve this call. Persistence lives outside the sandbox
/// in `~/.config/ahma/` — see [`ahma_core::approvals`].
#[cfg(feature = "tui")]
fn resolve_approval_always(state: &mut crate::state::AppState) {
    use crate::state::{LogEntry, LogLevel};

    if let Some(gate) = state.approval.as_ref() {
        let tool = gate.tool.clone();
        let workspace = std::path::PathBuf::from(&state.workspace);
        match ahma_core::approvals::remember_tool_approval(&workspace, &tool) {
            Ok(()) => state.push_log(LogEntry {
                timestamp: chrono::Local::now(),
                level: LogLevel::Info,
                message: format!("Always allowing tool '{tool}' in this workspace"),
            }),
            Err(e) => state.push_log(LogEntry {
                timestamp: chrono::Local::now(),
                level: LogLevel::Warn,
                message: format!("Could not persist always-allow for '{tool}': {e}"),
            }),
        }
    }

    resolve_approval(state, true);
}

#[cfg(feature = "tui")]
fn send_daemon_msg(msg: ahma_common::daemon_hub::ClientMsg) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Ok(mut stream) = ahma_common::daemon_hub::connect_to_daemon().await {
                let _ = ahma_common::daemon_hub::send_msg(&mut stream, &msg).await;
            }
        });
    } else {
        tracing::debug!(
            "send_daemon_msg: no active tokio runtime, skipping message: {:?}",
            msg
        );
    }
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

    send_daemon_msg(ahma_common::daemon_hub::ClientMsg::SubmitApproval {
        approved,
        target_instance_id: None,
    });

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
            if let Some(palette) = state.palette_mut() {
                palette.input.push(*c);
            }
            refresh_palette_completions(state);
        }
        Action::PaletteBackspace => {
            if let Some(palette) = state.palette_mut() {
                palette.input.pop();
            }
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
    state.modal = crate::state::ModalState::Palette(crate::state::PaletteState::default());
    refresh_palette_completions(state);
    state.focus = crate::state::Focus::Palette;
}

#[cfg(feature = "tui")]
fn close_palette(state: &mut crate::state::AppState) {
    if state.palette().is_some() {
        state.close_modal();
    }
    state.focus = crate::state::Focus::AiActivity;
}

#[cfg(feature = "tui")]
fn refresh_palette_completions(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    if let Some(palette) = state.palette_mut() {
        palette.update_completions(&tools);
    }
}

#[cfg(feature = "tui")]
fn apply_palette_completion(state: &mut crate::state::AppState) {
    let Some(palette) = state.palette_mut() else {
        return;
    };
    let n = palette.completions.len();
    if n == 0 {
        return;
    }

    palette.selected_completion = (palette.selected_completion + 1) % n;
    let idx = palette.selected_completion;
    if let Some(name) = palette.completions.get(idx).cloned() {
        palette.input = name;
    }
}

#[cfg(feature = "tui")]
fn advance_palette_selection(state: &mut crate::state::AppState) {
    if let Some(palette) = state.palette_mut() {
        let n = palette.completions.len();
        if n > 0 {
            palette.selected_completion = (palette.selected_completion + 1) % n;
        }
    }
}

#[cfg(feature = "tui")]
fn rewind_palette_selection(state: &mut crate::state::AppState) {
    if let Some(palette) = state.palette_mut() {
        let n = palette.completions.len();
        if n > 0 {
            palette.selected_completion =
                palette.selected_completion.checked_sub(1).unwrap_or(n - 1);
        }
    }
}

#[cfg(feature = "tui")]
fn submit_palette_command(state: &mut crate::state::AppState) {
    use crate::state::{LogEntry, LogLevel};

    let cmd = state.palette().map(|p| p.input.clone()).unwrap_or_default();
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

fn maybe_close_window_shortcut(text: &str, state: &mut crate::state::AppState) -> bool {
    let lower = text.to_lowercase();
    if lower.starts_with('x')
        && !lower[1..].is_empty()
        && lower[1..].chars().all(|c| c.is_ascii_digit())
        && let Ok(win_id) = lower[1..].parse::<usize>()
    {
        close_window_by_id(win_id, state);
        true
    } else {
        false
    }
}

fn maybe_decompose_goal(
    text: &str,
    base_url: &str,
    model: &str,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::llm_bridge::spawn_decompose_task;
    use crate::state::{LogEntry, LogLevel};
    use ahma_llm_monitor::client::LlmClient;

    if let Some(stripped_goal) = text.strip_prefix('!') {
        let goal = stripped_goal.trim().to_string();
        if goal.is_empty() {
            return true;
        }
        if base_url.is_empty() {
            push_assistant_message(state, "No LLM configured. Use /provider to select one.");
            return true;
        }
        state.push_log(LogEntry {
            timestamp: chrono::Local::now(),
            level: LogLevel::Info,
            message: format!("Decomposing goal: {}", goal),
        });
        let client = LlmClient::new(base_url.to_string(), model.to_string(), None);
        if let Some(tx) = &state.bridge_tx {
            spawn_decompose_task(client, goal, tx.clone());
        }
        true
    } else {
        false
    }
}

fn maybe_run_cli_command(text: &str, state: &mut crate::state::AppState) -> bool {
    use crate::llm_bridge::spawn_window_cli_task;
    use crate::state::TuiWindow;

    if let Some(stripped_cmd) = text
        .strip_prefix('%')
        .or_else(|| text.strip_prefix('$'))
        .or_else(|| text.strip_prefix('#'))
    {
        let cmd_str = stripped_cmd.trim().to_string();
        if cmd_str.is_empty() {
            return true;
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
        true
    } else {
        false
    }
}

#[cfg(feature = "tui")]
fn submit_chat_input(state: &mut crate::state::AppState) {
    use crate::state::ChatEntry;

    let text = state.chat_input_text().trim().to_string();
    if text.is_empty() {
        return;
    }
    state.clear_chat_input();

    if maybe_close_window_shortcut(&text, state) {
        return;
    }

    if text.starts_with('/') {
        dispatch_nav_command(&text, state);
        return;
    }

    let (base_url, model) = parse_llm_selection(state);

    if maybe_decompose_goal(&text, &base_url, &model, state) {
        return;
    }

    if maybe_run_cli_command(&text, state) {
        return;
    }

    if base_url.is_empty() {
        push_assistant_message(state, "No LLM configured. Use /provider to select one.");
        return;
    }

    state.chat.push(ChatEntry::User {
        text,
        started_at: Some(std::time::Instant::now()),
        duration_ms: None,
    });
    state.chat.push(ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

    let messages = collect_chat_history(state)
        .into_iter()
        .map(|msg| {
            let role = match msg.role {
                ahma_llm_monitor::ChatRole::System => "system",
                ahma_llm_monitor::ChatRole::User => "user",
                ahma_llm_monitor::ChatRole::Assistant => "assistant",
                ahma_llm_monitor::ChatRole::Tool => "tool",
            }
            .to_string();
            ahma_common::daemon_hub::DaemonChatMessage {
                role,
                content: msg.content,
            }
        })
        .collect();

    let system_prompt = Some(build_system_prompt(state));

    send_daemon_msg(ahma_common::daemon_hub::ClientMsg::SubmitPrompt {
        messages,
        system_prompt,
        provider: Some(base_url),
        model: Some(model),
        target_instance_id: None,
    });
}

#[cfg(feature = "tui")]
fn format_recent_ops(operations: &[crate::state::Operation]) -> String {
    use crate::state::OpStatus;
    let mut ctx = String::new();
    let recent_ops: Vec<&crate::state::Operation> = operations.iter().rev().take(5).collect();

    if !recent_ops.is_empty() {
        ctx.push_str("\nRecent operations:\n");
        for op in &recent_ops {
            let status_label = match &op.status {
                OpStatus::Running => "running",
                OpStatus::Pending => "pending",
                OpStatus::Succeeded => "ok",
                OpStatus::Failed => "FAILED",
                OpStatus::Cancelled => "cancelled",
                OpStatus::Waiting => "waiting",
            };
            let name = op.display_name();
            let elapsed = op.elapsed_display();
            let summary = op
                .result_summary
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!(" — {}", s.lines().next().unwrap_or(s)))
                .unwrap_or_default();
            ctx.push_str(&format!("  [{status_label}] {name} ({elapsed}){summary}\n"));
        }
    }
    ctx
}

#[cfg(feature = "tui")]
fn format_recent_failures(operations: &[crate::state::Operation]) -> String {
    use crate::state::OpStatus;
    let mut ctx = String::new();
    let failures: Vec<&crate::state::Operation> = operations
        .iter()
        .rev()
        .filter(|o| o.status == OpStatus::Failed)
        .take(2)
        .collect();

    if !failures.is_empty() {
        ctx.push_str("\nRecent failures (tail output):\n");
        for op in &failures {
            ctx.push_str(&format!(
                "  {} ({}):\n",
                op.display_name(),
                op.elapsed_display()
            ));
            let tail: Vec<&str> = op
                .stdout_tail
                .iter()
                .rev()
                .take(8)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            for line in tail {
                ctx.push_str(&format!("    {line}\n"));
            }
        }
    }
    ctx
}

/// Build the system prompt sent with every LLM request.
///
/// Combines (in priority order):
/// 1. The active agent profile's `system_prompt` (if any).
/// 2. A concise snapshot of live workspace context: scope, sandbox status,
///    recent operations with their status, and recent failures.
///
/// The context block is intentionally short (<500 tokens) so it does not eat
/// into the user's context window.
#[cfg(feature = "tui")]
fn build_system_prompt(state: &crate::state::AppState) -> String {
    // 1. Profile system prompt (may override defaults).
    let profile_prompt = profile_field(state, |p| p.system_prompt, String::new());

    // 2. Live context block.
    let mut ctx = String::new();

    // Workspace / scope.
    if !state.workspace.is_empty() {
        ctx.push_str(&format!("Workspace: {}\n", state.workspace));
    }

    // Sandbox / server status.
    if !state.sandbox_status.is_empty() && state.sandbox_status != "unknown" {
        ctx.push_str(&format!("Sandbox: {}\n", state.sandbox_status));
    }

    // Recent operations (last 5, most recent first).
    ctx.push_str(&format_recent_ops(&state.operations));

    // Recent failures — include a brief stdout tail to help with "why did it fail?" queries.
    ctx.push_str(&format_recent_failures(&state.operations));

    // 3. Assemble final prompt.
    let base = if state.mcp_enabled {
        "Use ahma tools when they would materially improve the answer. \
         Prefer direct answers when no tool is needed."
    } else {
        "Provide concise, accurate answers."
    };

    if profile_prompt.is_empty() && ctx.is_empty() {
        base.to_string()
    } else if profile_prompt.is_empty() {
        format!("{base}\n\n--- Live context ---\n{ctx}")
    } else if ctx.is_empty() {
        format!("{profile_prompt}\n\n{base}")
    } else {
        format!("{profile_prompt}\n\n{base}\n\n--- Live context ---\n{ctx}")
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
            ChatEntry::User { text, .. } => Some(ChatMessage::user(text.clone())),
            ChatEntry::Assistant {
                content,
                streaming: false,
            } => Some(ChatMessage::assistant(content.clone())),
            _ => None,
        })
        .collect()
}

/// Look up a single field from the active agent profile.
/// Returns `default` when there is no active profile or the profile cannot be loaded.
#[cfg(feature = "tui")]
fn profile_field<T, F>(state: &crate::state::AppState, extract: F, default: T) -> T
where
    F: FnOnce(crate::agent_config::AgentProfile) -> T,
{
    let Some(profile_name) = &state.active_profile else {
        return default;
    };
    let Ok(cwd) = std::env::current_dir() else {
        return default;
    };
    crate::agent_config::get_profile(&cwd, profile_name)
        .map(extract)
        .unwrap_or(default)
}

/// Resolve token/context preferences: CLI flag > deprecated env var > settings.
/// Returns `(minimize_tokens, small_model_harness, context_length)`.
#[cfg(feature = "tui")]
fn resolve_token_prefs(state: &crate::state::AppState) -> (bool, bool, Option<u32>) {
    let settings = ahma_common::config::AhmaSettings::load();

    fn env_bool(name: &str) -> Option<bool> {
        std::env::var(name).ok().map(|v| {
            tracing::warn!(
                "Deprecated: {name} environment variable is set. Use the corresponding CLI flag instead."
            );
            v == "1" || v.to_lowercase() == "true"
        })
    }

    let minimize_tokens = state
        .token_prefs
        .minimize_tokens
        .or_else(|| env_bool("AHMA_MINIMIZE_TOKENS"))
        .unwrap_or(settings.tools.minimize_tokens);
    let small_model_harness = state
        .token_prefs
        .small_model_harness
        .or_else(|| env_bool("AHMA_SMALL_MODEL_HARNESS"))
        .unwrap_or(settings.tools.small_model_harness);
    (
        minimize_tokens,
        small_model_harness,
        state.token_prefs.context_length,
    )
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

    let max_turns = profile_field(state, |p| p.max_turns, 8);
    let tool_approval = profile_field(state, |p| p.tool_approval, false);

    let (minimize_tokens, small_model_harness, context_length) = resolve_token_prefs(state);

    crate::llm_bridge::McpChatConfig {
        base_url: state.mcp_http_base_url.clone(),
        workspace_root: std::path::PathBuf::from(&state.workspace),
        session_id: state.session_id.clone(),
        external_http_servers,
        max_turns,
        tool_approval,
        mcp_connections: state.mcp_connections.clone(),
        minimize_tokens,
        small_model_harness,
        context_length,
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
        Action::NavEsc => {
            if state.navigator().is_some() {
                state.close_modal();
            }
        }
        Action::NavChar(c) => {
            if let Some(nav) = state.navigator_mut() {
                nav.input.push(*c);
            }
            refresh_navigator_completions(state);
        }
        Action::NavBackspace => {
            if let Some(nav) = state.navigator_mut() {
                nav.input.pop();
            }
            refresh_navigator_completions(state);
        }
        Action::NavComplete => {
            if let Some(nav) = state.navigator_mut() {
                nav.tab_complete();
            }
        }
        Action::NavUp => {
            if let Some(nav) = state.navigator_mut() {
                nav.select_prev();
            }
        }
        Action::NavDown => {
            if let Some(nav) = state.navigator_mut() {
                nav.select_next();
            }
        }
        Action::NavSubmit => submit_navigator_command(state),
        _ => return false,
    }

    true
}

#[cfg(feature = "tui")]
fn open_navigator(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    state.modal =
        crate::state::ModalState::Navigator(crate::state::CommandNavigator::opened(&tools));
}

#[cfg(feature = "tui")]
fn refresh_navigator_completions(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    if let Some(nav) = state.navigator_mut() {
        nav.refresh_completions(&tools);
    }
}

#[cfg(feature = "tui")]
fn submit_navigator_command(state: &mut crate::state::AppState) {
    let Some(cmd) = state.navigator().map(|n| n.selected_command()) else {
        return;
    };
    state.close_modal();
    dispatch_nav_command(&cmd, state);
}

#[cfg(feature = "tui")]
fn handle_settings_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crossterm::event::KeyCode;

    if !state.settings_editor.open {
        return false;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => state.settings_editor.close(),
        KeyCode::Up | KeyCode::Char('k') => state.settings_editor.item_up(),
        KeyCode::Down | KeyCode::Char('j') => state.settings_editor.item_down(),
        KeyCode::Left | KeyCode::Char('h') => state.settings_editor.category_up(),
        KeyCode::Right | KeyCode::Char('l') => state.settings_editor.category_down(),
        KeyCode::Char(' ') | KeyCode::Enter => state.settings_editor.toggle_current(),
        KeyCode::Char('r') => state.settings_editor.reset_current(),
        KeyCode::Char('s') => state.settings_editor.save(),
        KeyCode::Tab => state.settings_editor.category_down(),
        KeyCode::BackTab => state.settings_editor.category_up(),
        _ => {}
    }

    true
}

#[cfg(feature = "tui")]
fn handle_help_key(key: crossterm::event::KeyEvent, state: &mut crate::state::AppState) -> bool {
    use crossterm::event::KeyCode;

    if !state.is_help_open() {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) | (KeyCode::Char('?'), _) => {
            state.close_modal();
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
    match &mut state.modal {
        crate::state::ModalState::ProviderPicker(p) | crate::state::ModalState::ModelPicker(p) => {
            Some(p)
        }
        _ => None,
    }
}

#[cfg(feature = "tui")]
/// When an approval is pending, a bare `y` / `n` resolves it immediately — no
/// matter which panel has focus. Without this, the chat input box swallows the
/// keystroke as typed text (the bug where pressing "y" just sent "y" as a
/// message), forcing the user to Tab away before the global keymap saw it.
///
/// We deliberately bail when a text-entry overlay is active (navigator, palette,
/// pickers, log filter) so the user can still type a `y`/`n` there.
#[cfg(feature = "tui")]
fn handle_approval_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.approval.is_none() || state.text_entry_modal_open() || state.log_filter_active {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('y'), KeyModifiers::NONE) => {
            handle_action(crate::keymap::Action::Approve, state);
            true
        }
        (KeyCode::Char('a'), KeyModifiers::NONE) => {
            handle_action(crate::keymap::Action::ApproveAlways, state);
            true
        }
        (KeyCode::Char('n'), KeyModifiers::NONE) => {
            handle_action(crate::keymap::Action::Reject, state);
            true
        }
        _ => false,
    }
}

#[cfg(feature = "tui")]
fn handle_chat_input_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::state::Focus;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.focus != Focus::Chat || state.text_entry_modal_open() || state.log_filter_active {
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
                let mut nav = crate::state::CommandNavigator::opened(&tools);
                nav.input = "run ".to_string();
                nav.refresh_completions(&tools);
                state.modal = crate::state::ModalState::Navigator(nav);
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
            open_navigator(state);
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
    if cmd == "/exit" || cmd == "/quit" {
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
        || handle_settings_nav_command(cmd, state)
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
        "/help" | "/?" => state.modal = crate::state::ModalState::Help,
        "/clear" => state.clear_screen(),
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
fn handle_settings_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd != "/settings" {
        return false;
    }
    state.settings_editor.open();
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

fn handle_agent_list(cwd: &std::path::Path, state: &mut crate::state::AppState) {
    match crate::agent_config::load_profiles(cwd) {
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
}

fn handle_agent_save(cwd: &std::path::Path, name: &str, state: &mut crate::state::AppState) {
    let name = name.trim();
    if name.is_empty() {
        push_assistant_message(state, "Usage: /agent save <name>");
        return;
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
    match crate::agent_config::upsert_profile(cwd, profile) {
        Ok(()) => {
            state.active_profile = Some(name.to_string());
            push_assistant_message(state, format!("Saved profile `{name}`."));
        }
        Err(e) => push_assistant_message(state, format!("Failed to save profile: {e}")),
    }
}

fn handle_agent_load(cwd: &std::path::Path, name: &str, state: &mut crate::state::AppState) {
    let name = name.trim();
    if name.is_empty() {
        push_assistant_message(state, "Usage: /agent load <name>");
        return;
    }
    match crate::agent_config::get_profile(cwd, name) {
        Ok(profile) => {
            state.active_profile = Some(profile.name.clone());
            state.current_provider_url = Some(profile.provider_url);
            state.llm_label = format!("profile:{name} / {}", profile.model);
            push_assistant_message(state, format!("Loaded profile `{name}`."));
        }
        Err(e) => push_assistant_message(state, format!("Failed to load profile: {e}")),
    }
}

fn handle_agent_delete(cwd: &std::path::Path, name: &str, state: &mut crate::state::AppState) {
    let name = name.trim();
    if name.is_empty() {
        push_assistant_message(state, "Usage: /agent delete <name>");
        return;
    }
    match crate::agent_config::delete_profile(cwd, name) {
        Ok(true) => push_assistant_message(state, format!("Deleted profile `{name}`.")),
        Ok(false) => push_assistant_message(state, format!("Profile `{name}` not found.")),
        Err(e) => push_assistant_message(state, format!("Failed to delete profile: {e}")),
    }
}

#[cfg(feature = "tui")]
fn handle_agent_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        push_assistant_message(state, "Cannot resolve current working directory.");
        return true;
    };

    if cmd == "/agent list" {
        handle_agent_list(&cwd, state);
        return true;
    }

    if let Some(name) = cmd.strip_prefix("/agent save ") {
        handle_agent_save(&cwd, name, state);
        return true;
    }

    if let Some(name) = cmd.strip_prefix("/agent load ") {
        handle_agent_load(&cwd, name, state);
        return true;
    }

    if let Some(name) = cmd.strip_prefix("/agent delete ") {
        handle_agent_delete(&cwd, name, state);
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
            crate::state::ChatEntry::User { text, .. } => {
                md.push_str("## User\n\n");
                md.push_str(text);
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
    state.modal = crate::state::ModalState::ProviderPicker(picker);
}

#[cfg(feature = "tui")]
fn open_model_picker(state: &mut crate::state::AppState) {
    use crate::llm_bridge::spawn_model_refresh;
    use crate::state::PickerState;

    let mut items = Vec::new();
    for provider in &state.available_providers {
        for model in &provider.models {
            let item = format!("{} / {}", provider.name, model);
            if !items.contains(&item) {
                items.push(item);
            }
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
    state.modal = crate::state::ModalState::ModelPicker(picker);
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
fn virtual_provider_for_instance(label: &str) -> Option<ahma_llm_monitor::LocalProvider> {
    let normalized = label.to_lowercase();
    if normalized.contains("cursor") {
        Some(ahma_llm_monitor::LocalProvider {
            name: "Cursor (Sampling)".to_string(),
            base_url: "mcp://Cursor".to_string(),
            models: vec![
                "Claude 3.5 Sonnet (IDE subscription)".to_string(),
                "GPT-4o (IDE subscription)".to_string(),
                "Gemini 1.5 Pro (IDE subscription)".to_string(),
            ],
        })
    } else if normalized.contains("vscode")
        || normalized.contains("vs code")
        || normalized.contains("visual studio code")
    {
        Some(ahma_llm_monitor::LocalProvider {
            name: "VS Code (Sampling)".to_string(),
            base_url: "mcp://VS Code".to_string(),
            models: vec![
                "Claude 3.5 Sonnet (IDE subscription)".to_string(),
                "GPT-4o (IDE subscription)".to_string(),
                "Gemini 1.5 Pro (IDE subscription)".to_string(),
            ],
        })
    } else if normalized.contains("antigravity") {
        Some(ahma_llm_monitor::LocalProvider {
            name: "Antigravity (Sampling)".to_string(),
            base_url: "mcp://Antigravity".to_string(),
            models: vec![
                "Gemini 1.5 Pro (Google Cloud bill)".to_string(),
                "Gemini 1.5 Flash (Google Cloud bill)".to_string(),
            ],
        })
    } else if normalized.contains("claude") {
        Some(ahma_llm_monitor::LocalProvider {
            name: "Claude Code (Sampling)".to_string(),
            base_url: "mcp://Claude Code".to_string(),
            models: vec![
                "Claude 3.5 Sonnet (Anthropic API bill)".to_string(),
                "Claude 3 Opus (Anthropic API bill)".to_string(),
            ],
        })
    } else {
        Some(ahma_llm_monitor::LocalProvider {
            name: format!("{label} (Sampling)"),
            base_url: format!("mcp://{label}"),
            models: vec!["Default Model (IDE subscription)".to_string()],
        })
    }
}

#[cfg(feature = "tui")]
fn rebuild_available_providers(state: &mut crate::state::AppState) {
    let mut combined = state.discovered_providers.clone();
    for inst in &state.active_instances {
        if let Some(virtual_provider) = virtual_provider_for_instance(&inst.label)
            && !combined.iter().any(|p| p.name == virtual_provider.name)
        {
            combined.push(virtual_provider);
        }
    }
    state.available_providers = combined;
}

#[cfg(feature = "tui")]
fn handle_instances_updated(
    instances: Vec<ahma_common::daemon_hub::InstanceInfo>,
    state: &mut crate::state::AppState,
) {
    state.active_instances = instances;
    rebuild_available_providers(state);
}

#[cfg(feature = "tui")]
fn handle_providers_discovered(
    providers: Vec<ahma_llm_monitor::LocalProvider>,
    state: &mut crate::state::AppState,
) {
    state.discovered_providers = providers.clone();
    rebuild_available_providers(state);

    if state.llm_label == "no LLM" {
        if let Some(provider) = state.available_providers.first().cloned() {
            auto_select_first_provider(state, &provider);
        }
        return;
    }

    if let Some(current_url) = state.current_provider_url.clone() {
        refresh_current_provider_models(state, &current_url);
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
fn refresh_current_provider_models(state: &mut crate::state::AppState, current_url: &str) {
    let Some(provider) = state
        .available_providers
        .iter()
        .find(|p| p.base_url == current_url)
        .cloned()
    else {
        return;
    };
    state.available_models = provider.models;
    let model = state.selected_model();
    if !model.is_empty() {
        state.llm_label = format!("{} / {}", provider.name, model);
    }
}

#[cfg(feature = "tui")]
fn handle_model_refreshed(
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
            state.chat.finish_user_timing();
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
            state.chat.finish_user_timing();
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
            handle_model_refreshed(base_url, models, state);
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

            let note =
                ahma_core::approvals::reask_note(std::path::Path::new(&state.workspace), &tool);
            state.request_approval(
                crate::state::ApprovalGate::new(id, tool.clone(), format!("Execute tool {tool}"))
                    .with_note(note)
                    .with_diff(diff),
                Some(tx),
            );
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

fn extract_args_summary(tool_name: &str, description: &str) -> Option<String> {
    let start_idx = description.find('{')?;
    let end_idx = description.rfind('}')?;
    if start_idx >= end_idx {
        return None;
    }
    let val = serde_json::from_str::<serde_json::Value>(&description[start_idx..=end_idx]).ok()?;
    let obj = val.as_object()?;
    if tool_name == "run_terminal_command" {
        obj.get("command")
            .and_then(|v| v.as_str())
            .map(|cmd| format!("command: {cmd}"))
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
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

#[cfg(feature = "tui")]
fn format_friendly_start(
    tool_name: &str,
    description: &str,
    start_time: chrono::DateTime<chrono::Local>,
) -> String {
    let args_summary = extract_args_summary(tool_name, description).unwrap_or_default();
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

/// Build the full window content for an operation: friendly start line,
/// live output tail (streamed as the command runs), and — once terminal —
/// a separator plus a friendly result line.
#[cfg(feature = "tui")]
fn window_content_for(op: &crate::state::Operation, unicode: bool) -> Vec<String> {
    let is_live = matches!(
        op.status,
        crate::state::OpStatus::Running
            | crate::state::OpStatus::Pending
            | crate::state::OpStatus::Waiting
    );

    let mut content = Vec::with_capacity(op.stdout_tail.len() + 3);
    content.push(format_friendly_start(
        &op.tool_name,
        &op.description,
        op.started_time,
    ));

    // Live output tail — the command's stdout/stderr as it streams in.
    content.extend(op.stdout_tail.iter().cloned());

    if is_live {
        content.push("____".to_string());
    } else {
        let sep = if unicode {
            "────────────────────────────────────────".to_string()
        } else {
            "----------------------------------------".to_string()
        };
        content.push(sep);
        content.push(format_friendly_end(op));
    }
    content
}

#[cfg(feature = "tui")]
fn window_status_for(op: &crate::state::Operation) -> String {
    match op.status {
        crate::state::OpStatus::Running => "Running".to_string(),
        crate::state::OpStatus::Pending => "Pending".to_string(),
        crate::state::OpStatus::Succeeded => "Finished".to_string(),
        crate::state::OpStatus::Failed => "Error".to_string(),
        crate::state::OpStatus::Cancelled => "Cancelled".to_string(),
        crate::state::OpStatus::Waiting => "Pending".to_string(),
    }
}

#[cfg(feature = "tui")]
fn update_existing_window(
    w: &mut crate::state::TuiWindow,
    op: &crate::state::Operation,
    unicode: bool,
) {
    w.status = window_status_for(op);

    if op.status != crate::state::OpStatus::Running
        && op.status != crate::state::OpStatus::Pending
        && op.status != crate::state::OpStatus::Waiting
        && w.finished_at.is_none()
    {
        w.finished_at = Some(std::time::Instant::now());
    }

    w.content = window_content_for(op, unicode);
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

    let status = window_status_for(op);
    let content = window_content_for(op, state.unicode);

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
        } else if !state.window_suppressed_by_clear(op) {
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
        SourceEvent::DaemonHealthChanged { healthy } => {
            state.daemon_healthy = healthy;
            if let Some(ref tx) = state.mcp_source_tx {
                let _ = tx.try_send(crate::mcp_source::McpSourceCommand::SetDaemonHealthy(
                    healthy,
                ));
            }
        }
        SourceEvent::OperationsUpdated { ops } => {
            for op in ops {
                state.upsert_operation(op);
            }
            sync_operations_to_windows(state);
        }
        SourceEvent::OperationOutput {
            instance_id,
            op_id,
            line,
            is_stderr: _,
        } => {
            handle_operation_output(state, instance_id, &op_id, line);
        }
        SourceEvent::AiActivity(entry) => state.push_activity(entry),
        SourceEvent::LogLine(entry) => state.push_log(entry),
        SourceEvent::ToolsListUpdated { tools } => {
            handle_event_tools_list_updated(tools, state);
        }
        SourceEvent::SandboxStatus { status } => state.sandbox_status = status,
        SourceEvent::SessionId { id } => {
            state.session_id = if id.is_empty() { None } else { Some(id) };
        }
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
        SourceEvent::InstancesUpdated { instances } => {
            handle_instances_updated(instances, state);
        }
        SourceEvent::ChatToken { token } => {
            state.chat.append_token(&token);
        }
        SourceEvent::ApprovalRequested { id, tool, args } => {
            let diff = if tool.contains("replace") || tool == "write_file" {
                serde_json::from_str::<serde_json::Value>(&args)
                    .ok()
                    .and_then(|val| serde_json::to_string_pretty(&val).ok())
            } else {
                None
            };
            let note =
                ahma_core::approvals::reask_note(std::path::Path::new(&state.workspace), &tool);
            state.request_approval(
                crate::state::ApprovalGate::new(id, tool.clone(), format!("Execute tool {tool}"))
                    .with_note(note)
                    .with_diff(diff),
                None,
            );
        }
        SourceEvent::AgentDone => {
            state.chat.finish_stream();
            state.chat.finish_user_timing();
        }
        SourceEvent::AgentError { error } => {
            state.chat.finish_stream();
            state.chat.push(crate::state::ChatEntry::Assistant {
                content: format!("Error: {error}"),
                streaming: false,
            });
        }
    }
}

/// Append one live output line to an operation's tail buffer and refresh the
/// matching window incrementally — no full window rebuild, no polling delay.
#[cfg(feature = "tui")]
fn handle_operation_output(
    state: &mut crate::state::AppState,
    instance_id: Option<String>,
    op_id: &str,
    line: String,
) {
    // Prefer an exact (id, instance) match; fall back to id-only so output
    // still lands when the op was first seen via the poll path (instance None).
    let op_idx = state
        .operations
        .iter()
        .position(|o| o.id == op_id && o.instance_id == instance_id)
        .or_else(|| state.operations.iter().position(|o| o.id == op_id));

    let Some(idx) = op_idx else {
        // Output for an operation we have not seen yet — the OpStarted event
        // (or the next reconciliation poll) will create it; drop the line.
        return;
    };

    {
        let op = &mut state.operations[idx];
        if op.stdout_tail.len() >= crate::state::STDOUT_TAIL_CAP {
            op.stdout_tail.pop_front();
        }
        op.stdout_tail.push_back(line);
    }

    let op = state.operations[idx].clone();
    let unicode = state.unicode;
    if let Some(w) = state
        .windows
        .iter_mut()
        .find(|w| w.op_id.as_deref() == Some(op_id))
    {
        update_existing_window(w, &op, unicode);
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
            let mcp = if state.mcp_enabled {
                Some(mcp_chat_config(state))
            } else {
                None
            };
            spawn_window_llm_task(win_id, base_url, model, command, mcp, abort_rx, tx);
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

fn parse_monitor_path_and_prompt(rest: &str) -> (String, String) {
    if rest.starts_with('"') {
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
    }
}

#[cfg(feature = "tui")]
fn handle_monitor_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if !cmd.starts_with("/monitor file ") {
        return false;
    }

    let rest = cmd.strip_prefix("/monitor file ").unwrap().trim();
    let (path, prompt) = parse_monitor_path_and_prompt(rest);

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

    state.chat.push(crate::state::ChatEntry::User {
        text: format!("Analyze operation {}", id),
        started_at: Some(std::time::Instant::now()),
        duration_ms: None,
    });
    state.chat.push(crate::state::ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

    let (base_url, model) = parse_llm_selection(state);
    if base_url.is_empty() {
        push_assistant_message(state, "No LLM configured. Use /provider to select one.");
        return;
    }

    let mut history = collect_chat_history(state);
    if let Some(last_msg) = history.last_mut() {
        last_msg.content = prompt;
    }

    let messages = history
        .into_iter()
        .map(|msg| {
            let role = match msg.role {
                ahma_llm_monitor::ChatRole::System => "system",
                ahma_llm_monitor::ChatRole::User => "user",
                ahma_llm_monitor::ChatRole::Assistant => "assistant",
                ahma_llm_monitor::ChatRole::Tool => "tool",
            }
            .to_string();
            ahma_common::daemon_hub::DaemonChatMessage {
                role,
                content: msg.content,
            }
        })
        .collect();

    let system_prompt = state.mcp_enabled.then(|| {
        "Use ahma tools when they would materially improve the answer. Prefer direct answers when no tool is needed.".to_string()
    });

    send_daemon_msg(ahma_common::daemon_hub::ClientMsg::SubmitPrompt {
        messages,
        system_prompt,
        provider: Some(base_url),
        model: Some(model),
        target_instance_id: None,
    });
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

/// Returns true when the click position falls on the close button area of a window.
/// Single-height windows use their entire right edge; taller windows require
/// the click to be on the title row.
#[cfg(feature = "tui")]
fn is_close_button_click(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    let in_close_zone = col >= rect.x + rect.width.saturating_sub(5);
    if rect.height == 1 {
        in_close_zone
    } else {
        row == rect.y && in_close_zone
    }
}

#[cfg(feature = "tui")]
fn handle_window_rect_click(col: u16, row: u16, state: &mut crate::state::AppState) -> bool {
    // Collect the hit result first; acting on it requires a mutable borrow of `state`.
    enum Hit {
        Close(usize),
        Toggle(usize),
    }
    let hit = {
        let rects = state.window_rects.borrow();
        rects.iter().find_map(|&(win_id, rect)| {
            if !inside_rect(col, row, rect) {
                return None;
            }
            if is_close_button_click(col, row, rect) {
                Some(Hit::Close(win_id))
            } else {
                Some(Hit::Toggle(win_id))
            }
        })
    };

    match hit {
        Some(Hit::Close(win_id)) => {
            close_window_by_id(win_id, state);
            true
        }
        Some(Hit::Toggle(win_id)) => {
            if let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id) {
                w.collapsed = !w.collapsed;
            }
            true
        }
        None => false,
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
        let max = state.chat_max_scroll.get();
        if up {
            state.chat_scroll = (state.chat_scroll + 1).min(max);
        } else {
            state.chat_scroll = state.chat_scroll.min(max).saturating_sub(1);
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
            state.detach_log_follow();
            state.log_scroll = state.log_scroll.saturating_sub(1);
            state.sync_log_scroll_to_animation();
        } else if !state.log_follow {
            let max = state.log_max_scroll.get();
            state.log_scroll = (state.log_scroll + 1).min(max);
            state.sync_log_scroll_to_animation();
            state.maybe_reengage_log_follow();
        }
    }
}

fn determine_scrolled_panel(state: &crate::state::AppState) -> &'static str {
    if let Some((col, row)) = state.last_mouse_pos.get() {
        let chat_area = state.chat_area.get();
        let log_area = state.log_area.get();

        if col >= chat_area.x
            && col < chat_area.x + chat_area.width
            && row >= chat_area.y
            && row < chat_area.y + chat_area.height
        {
            return "chat";
        }
        if col >= log_area.x
            && col < log_area.x + log_area.width
            && row >= log_area.y
            && row < log_area.y + log_area.height
        {
            return "log";
        }
    }

    if state.mode == crate::state::Mode::Monitor && state.focus == crate::state::Focus::Log {
        "log"
    } else {
        "chat"
    }
}

/// Compute the page size for a panel given its rendered height.
#[cfg(feature = "tui")]
fn page_size_for_height(height: u16) -> f64 {
    (if height > 2 { height - 2 } else { 10 }) as f64
}

#[cfg(feature = "tui")]
fn handle_page_up_down(up: bool, state: &mut crate::state::AppState) {
    let panel = determine_scrolled_panel(state);

    if panel == "chat" {
        // Chat: up scrolls forward (higher offset), down scrolls back.
        let page_size = page_size_for_height(state.chat_area.get().height);
        let max_scroll = state.chat_max_scroll.get() as f64;
        let current = state.chat_scroll_target.get();
        let new_target = if up {
            (current + page_size).min(max_scroll)
        } else {
            (current - page_size).max(0.0)
        };
        state.chat_scroll_target.set(new_target);
    } else {
        // Log: up scrolls back (lower offset), down scrolls forward.
        let page_size = page_size_for_height(state.log_area.get().height);
        let max_scroll = state.log_max_scroll.get() as f64;
        if up {
            // Detach follow and page up from the current bottom.
            state.detach_log_follow();
            let current = state.log_scroll_target.get();
            state.log_scroll_target.set((current - page_size).max(0.0));
        } else if !state.log_follow {
            let current = state.log_scroll_target.get();
            let new_target = (current + page_size).min(max_scroll);
            state.log_scroll_target.set(new_target);
            // Paging down to the bottom re-engages tail-follow.
            if new_target >= max_scroll {
                state.log_follow = true;
            }
        }
    }
}

#[cfg(feature = "tui")]
fn update_scroll_animations(state: &mut crate::state::AppState) {
    // Clamp the animation against the live max so a resize that shrank the
    // content (recomputed in draw as chat_max_scroll) pulls a stale target/offset
    // back into range instead of stranding the view above the oldest line.
    let chat_max = state.chat_max_scroll.get() as f64;
    let chat_curr = state.chat_scroll_current.get().clamp(0.0, chat_max);
    let chat_tgt = state.chat_scroll_target.get().clamp(0.0, chat_max);
    state.chat_scroll_target.set(chat_tgt);
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

#[cfg(feature = "tui")]
fn chat_in_progress(state: &crate::state::AppState) -> bool {
    state.chat.entries().iter().any(|entry| {
        matches!(
            entry,
            crate::state::ChatEntry::Assistant {
                streaming: true,
                ..
            } | crate::state::ChatEntry::ToolCall { result: None, .. }
        )
    })
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

        // Test /quit (primary advertised command) to quit
        let mut state_quit = AppState::new("http://localhost:3000", "HTTP", true);
        let handled_quit = super::handle_window_nav_commands("/quit", &mut state_quit);
        assert!(handled_quit);
        assert!(state_quit.should_quit);

        // Test /exit (unadvertised alias) also quit
        let handled_exit = super::handle_window_nav_commands("/exit", &mut state);
        assert!(handled_exit);
        assert!(state.should_quit);
    }

    #[test]
    fn test_needs_approval_filtering() {
        use ahma_core::agent::needs_approval;
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
        state.request_approval(
            crate::state::ApprovalGate::new("op_test", "list_dir", "test"),
            Some(tx),
        );

        super::resolve_approval(&mut state, true);
        assert!(state.approval.is_none());
        assert!(state.approval_tx.is_none());

        let approved = rx.blocking_recv().unwrap();
        assert!(approved);
    }

    /// Regression: raising a second approval while one is pending must
    /// auto-reject (not silently drop) the first, so its waiter never hangs.
    #[test]
    fn test_superseded_approval_is_auto_rejected() {
        use crate::state::{AppState, ApprovalGate};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        let gate = |op: &str| ApprovalGate::new(op, "list_dir", "test");

        let (tx1, rx1) = tokio::sync::oneshot::channel();
        state.request_approval(gate("op_1"), Some(tx1));

        // A second gate supersedes the first.
        let (tx2, _rx2) = tokio::sync::oneshot::channel();
        state.request_approval(gate("op_2"), Some(tx2));

        // The first waiter is resolved with a rejection, never dropped.
        assert_eq!(rx1.blocking_recv().ok(), Some(false));
        assert_eq!(
            state.approval.as_ref().map(|g| g.op_id.as_str()),
            Some("op_2")
        );
    }

    /// Regression: a bare `y` while an approval is pending must resolve the
    /// approval even when the chat input box has focus, instead of being typed
    /// into the message as text.
    #[test]
    fn test_approval_key_resolves_while_chat_focused() {
        use crate::state::{AppState, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;

        let (tx, rx) = tokio::sync::oneshot::channel();
        state.request_approval(
            crate::state::ApprovalGate::new("op_test", "list_dir", "list_dir"),
            Some(tx),
        );

        let handled = super::handle_approval_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut state,
        );

        assert!(handled, "y must be consumed by the approval banner");
        assert!(state.approval.is_none(), "approval should be cleared");
        assert!(
            state.chat_input_is_empty(),
            "y must not land in the input box"
        );
        assert!(
            rx.blocking_recv().unwrap(),
            "decision sent should be approve"
        );
    }

    /// When no approval is pending, `y` must fall through to normal handling.
    #[test]
    fn test_approval_key_ignored_without_pending_gate() {
        use crate::state::{AppState, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;

        let handled = super::handle_approval_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(!handled, "y must pass through when no approval is pending");
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

        // Case 2: Mouse over Log Area -> Page keys scroll Log even when focused on Chat.
        state.last_mouse_pos.set(Some((10, 25))); // over log area

        // Following by default: PageDown is a no-op (already pinned to the bottom).
        assert!(state.log_follow);
        super::handle_page_up_down(false, &mut state);
        assert_eq!(state.log_scroll_target.get(), 0.0);
        assert!(
            state.log_follow,
            "PageDown while following stays at the bottom"
        );

        // PageUp detaches follow and pages up from the bottom (max=50): 50 - 8 = 42.
        super::handle_page_up_down(true, &mut state);
        assert!(!state.log_follow, "PageUp detaches follow");
        assert_eq!(state.log_scroll_target.get(), 42.0);

        // Paging back down to the bottom re-engages follow.
        super::handle_page_up_down(false, &mut state);
        assert_eq!(state.log_scroll_target.get(), 50.0);
        assert!(
            state.log_follow,
            "paging down to the bottom re-engages tail-follow"
        );
    }

    #[test]
    fn test_log_tail_follow_transitions() {
        use crate::state::{AppState, Focus};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Log;
        state.log_max_scroll.set(50);

        // Opens following by default.
        assert!(state.log_follow);

        // Scrolling up detaches follow and starts from the bottom (50 - 1 = 49).
        super::scroll_focus_up(&mut state);
        assert!(!state.log_follow, "scroll up detaches follow");
        assert_eq!(state.log_scroll, 49);

        // Scrolling back down to the bottom re-engages follow.
        super::scroll_focus_down(&mut state);
        assert_eq!(state.log_scroll, 50);
        assert!(state.log_follow, "scrolling back to the bottom re-follows");

        // While following, scroll down is a no-op (stays pinned).
        super::scroll_focus_down(&mut state);
        assert!(state.log_follow);

        // Home/Top detaches and jumps to the very top.
        super::move_focus_to_top(&mut state);
        assert!(!state.log_follow);
        assert_eq!(state.log_scroll, 0);

        // End/Bottom re-engages follow.
        super::move_focus_to_bottom(&mut state);
        assert!(state.log_follow);
    }

    #[test]
    fn test_submit_chat_input_closes_window() {
        use crate::state::{AppState, TuiWindow};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        let w = TuiWindow {
            id: 26,
            label: "Test Window".to_string(),
            status: "Running".to_string(),
            content: vec![],
            collapsed: false,
            finished_at: None,
            is_cli: true,
            command: "pwd".to_string(),
            working_dir: state.workspace.clone(),
            llm_model: None,
            visible: true,
            abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            op_id: None,
        };
        state.windows.push(w);

        // Type "x26" in chat input
        state.chat_input.insert_str("x26");
        super::submit_chat_input(&mut state);

        assert!(!state.windows[0].visible);
        assert_eq!(state.windows[0].status, "Cancelled");

        // Restore window
        state.windows[0].visible = true;
        state.windows[0].status = "Running".to_string();

        // Type "X26" in chat input
        state.chat_input.insert_str("X26");
        super::submit_chat_input(&mut state);

        assert!(!state.windows[0].visible);
        assert_eq!(state.windows[0].status, "Cancelled");
    }
}
