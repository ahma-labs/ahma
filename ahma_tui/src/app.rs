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
        event::{
            DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
            Event, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
            PushKeyboardEnhancementFlags,
        },
        execute,
        terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
            supports_keyboard_enhancement,
        },
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
    // Project root for the task-tree filter: the explicit path argument, else
    // the directory the TUI was started from. Canonicalized so it compares
    // against instance sandbox scopes (which are canonicalized at lock time).
    state.project_root = workspace_path
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .map(|p| {
            std::fs::canonicalize(&p)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        });
    state.mcp_http_base_url = http_base_url(connection);
    state.token_prefs = token_prefs;
    // Cache the effective minimize state (flag > env > settings) for the status
    // bar and the `/minimize` switch, so neither has to re-read settings.
    state.minimize_tokens = resolve_token_prefs(&state).0;

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
    // Bracketed paste makes the terminal deliver a paste as a single `Event::Paste`
    // (interior newlines included) instead of a stream of keystrokes. Without it, a
    // pasted trailing newline arrives as `Enter` and auto-submits, and a multi-line
    // paste fires one submission per line. See the `Event::Paste` handler below.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // Enable the Kitty keyboard protocol's escape-code disambiguation when the
    // terminal supports it. Without it, terminals collapse Shift+Enter into a
    // plain Enter, so it would submit instead of inserting a newline. We remember
    // whether the push succeeded so teardown only pops when we actually enabled it.
    let keyboard_enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if keyboard_enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
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
                            // Any keystroke disarms the startup auto-switch to
                            // the task view — the user has taken the wheel.
                            state.auto_view_pending = false;
                            if (key.code == crossterm::event::KeyCode::PageUp || key.code == crossterm::event::KeyCode::PageDown)
                                && !state.is_help_open()
                                && state.log_files_selected().is_none()
                            {
                                handle_page_up_down(key.code == crossterm::event::KeyCode::PageUp, &mut state);
                            } else if handle_settings_key(key, &mut state)
                                || handle_help_key(key, &mut state)
                                || handle_picker_key(key, &mut state)
                                || handle_scope_grant_key(key, &mut state)
                                || handle_web_approval_key(key, &mut state)
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
                                    // A click is user input: disarm the startup
                                    // auto-switch to the task view.
                                    state.auto_view_pending = false;
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
                        Some(Ok(Event::Paste(text))) => {
                            // Show the pasted text in the chat input but do NOT submit it.
                            // A trailing newline (e.g. pasting "somecommand\n") is dropped so
                            // it does not trigger a send; the user must press Enter themselves.
                            // Interior newlines are kept, so a multi-line paste appears as
                            // multiple lines in one input rather than many separate requests.
                            state.paste_into_chat_input(&text);
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
                    // While a turn is in flight but quiet, pulse the liveness
                    // spinner slowly so the user can tell it is still alive (and
                    // not silently timed out) — token arrivals drive it fast.
                    if chat_in_progress(&state) {
                        state.tick_waiting_spinner();
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
    if keyboard_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
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
            // Esc backs out one level: restore a zoomed pane first, then
            // return focus to the chat input.
            if state.zoomed.is_some() {
                state.zoomed = None;
            } else {
                state.focus = crate::state::Focus::Chat;
            }
        }
        Action::Enter if state.focus == crate::state::Focus::OpsDag => {
            // Drill in: open the full-screen detail view for an operation
            // (or fold an instance/session header).
            state.open_selected_tree_detail();
        }
        Action::ToggleNode if state.focus == crate::state::Focus::OpsDag => {
            // Space: inline accordion-expand the selected task into its
            // live/historic output view (or fold a header) without leaving
            // the tree.
            state.toggle_selected_tree_node();
        }
        Action::DetailClose => state.close_modal(),
        Action::ToggleProjectFilter if state.focus == crate::state::Focus::OpsDag => {
            state.show_all_projects = !state.show_all_projects;
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
            max_turns: ahma_common::config::AhmaSettings::load().tools.max_turns,
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
            // Maximise/restore the focused pane. Enter on the log pane and
            // `z` on any zoomable pane both land here.
            if state.zoomed.is_some() {
                state.zoomed = None;
            } else if state.focus.is_zoomable() {
                state.zoomed = Some(state.focus);
            }
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
    // The row is `name  base_url[  · num_ctx …]`; the base_url is the first
    // whitespace-delimited token of the remainder (URLs never contain spaces).
    let base_url = base_url_part
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
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

    // The full-screen operation detail overlay captures navigation while open.
    if matches!(state.modal, crate::state::ModalState::OperationDetail(_)) {
        return match action {
            Action::Up | Action::Down | Action::Top | Action::Bottom => {
                scroll_detail_overlay(action, state);
                true
            }
            _ => false,
        };
    }

    match action {
        Action::Up => scroll_focus_up(state),
        Action::Down => scroll_focus_down(state),
        Action::Top => move_focus_to_top(state),
        Action::Bottom => move_focus_to_bottom(state),
        _ => return false,
    }

    true
}

/// Scroll the operation-detail overlay; the max is computed at draw time.
#[cfg(feature = "tui")]
fn scroll_detail_overlay(action: &crate::keymap::Action, state: &mut crate::state::AppState) {
    use crate::keymap::Action;
    let max = state.detail_max_scroll.get();
    if let crate::state::ModalState::OperationDetail(d) = &mut state.modal {
        match action {
            Action::Up => d.scroll = d.scroll.saturating_sub(1),
            Action::Down => d.scroll = (d.scroll + 1).min(max),
            Action::Top => d.scroll = 0,
            Action::Bottom => d.scroll = max,
            _ => {}
        }
    }
}

#[cfg(feature = "tui")]
fn scroll_focus_up(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::OpsDag => state.ops_selected = state.ops_selected.saturating_sub(1),
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
        Focus::OpsDag if state.ops_row_count() > 0 => {
            state.ops_selected = (state.ops_selected + 1).min(state.ops_row_count() - 1);
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
        Focus::OpsDag => state.ops_selected = state.ops_row_count().saturating_sub(1),
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
        id: Some(gate.op_id.clone()),
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

    // Inside the detail overlay `c` cancels the operation being viewed, not
    // whatever the tree selection happens to be behind it.
    let id = if let crate::state::ModalState::OperationDetail(d) = &state.modal {
        d.op_id.clone()
    } else if let Some(op) = state.selected_op() {
        op.id.clone()
    } else {
        return;
    };
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
    if let Some(idx) = state.selected_op_index()
        && let Some(op) = state.operations.get_mut(idx)
    {
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
    state.focus = crate::state::Focus::OpsDag;
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

    if let Some(stripped_goal) = text.strip_prefix('#') {
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
        let client = LlmClient::new(base_url.to_string(), model.to_string(), None)
            .with_num_ctx(provider_num_ctx(base_url));
        if let Some(tx) = &state.bridge_tx {
            spawn_decompose_task(client, goal, tx.clone());
        }
        true
    } else {
        false
    }
}

fn maybe_run_cli_command(text: &str, state: &mut crate::state::AppState) -> bool {
    // `!` — UNSANDBOXED command. Runs the command locally at the user's full
    // privilege with NO sandbox, like a shell `!` escape. This is permitted ONLY
    // because a human explicitly typed `!` into the chat input. LLM/agent turns
    // submit work via `SubmitPrompt` and never write to this input box, so there
    // is no automated path to this branch — every invocation is a deliberate
    // human-in-the-loop action.
    if let Some(stripped) = text.strip_prefix('!') {
        let cmd_str = stripped.trim().to_string();
        if !cmd_str.is_empty() {
            run_unsandboxed_command(cmd_str, state);
        }
        return true;
    }
    false
}

/// Run a command OUTSIDE the sandbox, locally, at full user privilege.
///
/// SECURITY: reachable only via the human-typed `!` prefix (see
/// [`maybe_run_cli_command`]). Never call this from automated/agent code paths —
/// it deliberately bypasses the kernel sandbox and is gated on explicit human
/// intervention for every single command.
fn run_unsandboxed_command(cmd_str: String, state: &mut crate::state::AppState) {
    use crate::llm_bridge::spawn_window_cli_task;
    use crate::state::{LogEntry, LogLevel, TuiWindow};

    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level: LogLevel::Warn,
        message: format!("UNSANDBOXED command (human-authorized via !): {cmd_str}"),
    });

    let win_id = state.next_window_id;
    state.next_window_id = (state.next_window_id + 1) % 100;

    let working_dir = state.workspace.clone();
    let label = format!(
        "UNSANDBOXED: {} in {}",
        cmd_str,
        crate::ui::shorten_path(&working_dir, 20)
    );

    let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
    let w = TuiWindow {
        id: win_id,
        label,
        status: crate::state::WindowStatus::Running,
        content: vec![],
        collapsed: false,
        finished_at: None,
        duration_ms: None,
        last_output_at: None,
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
/// The pieces of a system prompt, assembled by a [`PromptComposer`]. Keeping
/// them separate lets a composer decide which to include for token economy.
#[cfg(feature = "tui")]
struct PromptParts {
    /// Optional per-profile prompt override.
    profile_prompt: String,
    /// The agentic base (user-editable agent prompt, or the concise fallback).
    base: String,
    /// `"Workspace: …\n"` or empty.
    workspace_line: String,
    /// `"Sandbox: …\n"` or empty.
    sandbox_line: String,
    /// Recent operations block (token-heavy).
    recent_ops: String,
    /// Recent failures block incl. stdout tails (token-heavy).
    recent_failures: String,
}

/// Strategy for assembling the agent system prompt. Swap implementations to
/// trade prompt richness for token economy — the seam behind `/minimize`.
#[cfg(feature = "tui")]
trait PromptComposer {
    fn compose(&self, parts: &PromptParts) -> String;
}

/// Join `profile` + `base` + the live-context `ctx` in the canonical layout.
#[cfg(feature = "tui")]
fn assemble_prompt(profile: &str, base: &str, ctx: &str) -> String {
    match (profile.is_empty(), ctx.is_empty()) {
        (true, true) => base.to_string(),
        (true, false) => format!("{base}\n\n--- Live context ---\n{ctx}"),
        (false, true) => format!("{profile}\n\n{base}"),
        (false, false) => format!("{profile}\n\n{base}\n\n--- Live context ---\n{ctx}"),
    }
}

/// The default composer: agentic base + profile + the full live-context block.
#[cfg(feature = "tui")]
struct FullComposer;

#[cfg(feature = "tui")]
impl PromptComposer for FullComposer {
    fn compose(&self, p: &PromptParts) -> String {
        let ctx = format!(
            "{}{}{}{}",
            p.workspace_line, p.sandbox_line, p.recent_ops, p.recent_failures
        );
        assemble_prompt(&p.profile_prompt, &p.base, &ctx)
    }
}

/// The lean composer used under `/minimize`: drops the token-heavy recent-ops
/// and recent-failures blocks, keeping the base, profile, workspace and sandbox.
#[cfg(feature = "tui")]
struct MinimalComposer;

#[cfg(feature = "tui")]
impl PromptComposer for MinimalComposer {
    fn compose(&self, p: &PromptParts) -> String {
        let ctx = format!("{}{}", p.workspace_line, p.sandbox_line);
        assemble_prompt(&p.profile_prompt, &p.base, &ctx)
    }
}

#[cfg(feature = "tui")]
fn build_system_prompt(state: &crate::state::AppState) -> String {
    let parts = PromptParts {
        profile_prompt: profile_field(state, |p| p.system_prompt, String::new()),
        base: if state.mcp_enabled {
            ahma_common::prompts::AhmaPrompts::load().agent_system_prompt()
        } else {
            "Provide concise, accurate answers.".to_string()
        },
        workspace_line: if state.workspace.is_empty() {
            String::new()
        } else {
            format!("Workspace: {}\n", state.workspace)
        },
        sandbox_line: if !state.sandbox_status.is_empty() && state.sandbox_status != "unknown" {
            format!("Sandbox: {}\n", state.sandbox_status)
        } else {
            String::new()
        },
        recent_ops: format_recent_ops(&state.operations),
        recent_failures: format_recent_failures(&state.operations),
    };

    // Under token minimization, drop the token-heavy live-context blocks.
    let composer: &dyn PromptComposer = if state.minimize_tokens {
        &MinimalComposer
    } else {
        &FullComposer
    };
    composer.compose(&parts)
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
    // Explicit --context-length wins; otherwise fall back to the selected
    // provider's declared window (`num_ctx` in ~/.ahma/config.toml) so
    // proactive compaction has a denominator without any flag (issue #484).
    let context_length = state.token_prefs.context_length.or_else(|| {
        let (base_url, _model) = parse_llm_selection(state);
        if base_url.is_empty() {
            None
        } else {
            provider_num_ctx(&base_url)
        }
    });
    (minimize_tokens, small_model_harness, context_length)
}

/// The configured context window (`num_ctx`) of the provider whose base URL
/// matches `base_url`, from `~/.ahma/config.toml`.
#[cfg(feature = "tui")]
fn provider_num_ctx(base_url: &str) -> Option<u32> {
    ahma_common::config::AhmaConfig::load().num_ctx_for_base_url(base_url)
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

    // Fall back to the configured global default when no profile pins a value.
    let settings_max_turns = ahma_common::config::AhmaSettings::load().tools.max_turns;
    let max_turns = profile_field(state, |p| p.max_turns, settings_max_turns);
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

/// Keys for the scope-grant modal. Three-valued and **Enter-safe**: Enter / Esc /
/// `n` deny (the default), `r` grants read-only, `y` grants read+write. Widening
/// always requires an explicit non-default key (SPEC R5.3.1).
#[cfg(feature = "tui")]
fn handle_scope_grant_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use ahma_common::scope_grant::GrantDecision;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.scope_grant.is_none() || state.text_entry_modal_open() || state.log_filter_active {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('y'), KeyModifiers::NONE) => {
            resolve_scope_grant(state, GrantDecision::GrantRw);
            true
        }
        (KeyCode::Char('r'), KeyModifiers::NONE) => {
            resolve_scope_grant(state, GrantDecision::GrantRo);
            true
        }
        (KeyCode::Char('n'), KeyModifiers::NONE) | (KeyCode::Esc, _) | (KeyCode::Enter, _) => {
            resolve_scope_grant(state, GrantDecision::Deny);
            true
        }
        _ => false,
    }
}

/// Resolve the pending scope-grant prompt: send the decision to the daemon (which
/// resolves + persists for the next start — never the live session) and log it.
#[cfg(feature = "tui")]
fn resolve_scope_grant(
    state: &mut crate::state::AppState,
    decision: ahma_common::scope_grant::GrantDecision,
) {
    use crate::state::{LogEntry, LogLevel};
    use ahma_common::scope_grant::GrantDecision;

    let Some(gate) = state.scope_grant.take() else {
        return;
    };

    send_daemon_msg(ahma_common::daemon_hub::ClientMsg::SubmitScopeGrant {
        decision_id: gate.decision_id,
        decision,
        target_instance_id: None,
    });

    let (level, message) = match decision {
        GrantDecision::Deny => (
            LogLevel::Warn,
            format!("Denied sandbox access to {}", gate.path),
        ),
        GrantDecision::GrantRo => (
            LogLevel::Info,
            format!(
                "Granted read-only access to {} — restart the bridge to apply now, else \
                 it takes effect on the next server start",
                gate.path
            ),
        ),
        GrantDecision::GrantRw => (
            LogLevel::Info,
            format!(
                "Granted read+write access to {} — restart the bridge to apply now, else \
                 it takes effect on the next server start",
                gate.path
            ),
        ),
    };
    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level,
        message,
    });
}

#[cfg(feature = "tui")]
fn handle_web_approval_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use ahma_common::web_approval::WebApprovalDecision;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.web_approval.is_none() || state.text_entry_modal_open() || state.log_filter_active {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('a'), KeyModifiers::NONE) => {
            resolve_web_approval(state, WebApprovalDecision::AllowAlways);
            true
        }
        (KeyCode::Char('s'), KeyModifiers::NONE) => {
            resolve_web_approval(state, WebApprovalDecision::AllowSession);
            true
        }
        (KeyCode::Char('n'), KeyModifiers::NONE) | (KeyCode::Esc, _) | (KeyCode::Enter, _) => {
            resolve_web_approval(state, WebApprovalDecision::Deny);
            true
        }
        _ => false,
    }
}

/// Resolve the pending web-approval prompt: send the decision to the daemon (which
/// applies it to the live session and, for `always`, persists it) and log it. The
/// request that raised the prompt was already denied, so the user retries it.
#[cfg(feature = "tui")]
fn resolve_web_approval(
    state: &mut crate::state::AppState,
    decision: ahma_common::web_approval::WebApprovalDecision,
) {
    use crate::state::{LogEntry, LogLevel};
    use ahma_common::web_approval::WebApprovalDecision;

    let Some(gate) = state.web_approval.take() else {
        return;
    };

    send_daemon_msg(ahma_common::daemon_hub::ClientMsg::SubmitWebApproval {
        decision_id: gate.decision_id,
        decision,
        target_instance_id: None,
    });

    let (level, message) = match decision {
        WebApprovalDecision::Deny => (
            LogLevel::Warn,
            format!("Denied web access to {}", gate.domain),
        ),
        WebApprovalDecision::AllowOnce => (
            LogLevel::Info,
            format!("Allowed web access to {} for this request", gate.domain),
        ),
        WebApprovalDecision::AllowSession => (
            LogLevel::Info,
            format!(
                "Allowed web access to {} for this session — retry the request",
                gate.domain
            ),
        ),
        WebApprovalDecision::AllowAlways => (
            LogLevel::Info,
            format!(
                "Allowed web access to {} and saved it to ~/.ahma/settings.toml — retry the request",
                gate.domain
            ),
        ),
    };
    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level,
        message,
    });
}

#[cfg(feature = "tui")]
fn handle_chat_input_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::state::Focus;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.focus != Focus::Chat
        || state.text_entry_modal_open()
        || state.log_filter_active
        || matches!(state.modal, crate::state::ModalState::OperationDetail(_))
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
        // Shift+Enter inserts a newline instead of submitting (requires the
        // terminal's keyboard-enhancement support enabled at startup).
        (KeyCode::Enter, KeyModifiers::SHIFT) => {
            state.chat_input.insert_newline();
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
        || handle_minimize_nav_command(cmd, state)
        || handle_agent_nav_command(cmd, state)
        || handle_export_nav_command(cmd, state)
        || handle_settings_nav_command(cmd, state)
        || handle_tools_nav_command(cmd, state)
        || handle_approval_nav_command(cmd, state)
        || handle_provider_admin_command(cmd, state)
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

/// `/minimize [on|off]` — toggle token minimization (concise prompting + output
/// compression for small models). With no argument it reports the current state.
/// The choice is applied live and persisted to `settings.tools.minimize_tokens`
/// so the daemon agent loop (which reads settings) and the next session both
/// honour it. Default is off.
#[cfg(feature = "tui")]
fn handle_minimize_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    let Some(rest) = cmd.strip_prefix("/minimize") else {
        return false;
    };
    let arg = rest.trim();

    let desired = match arg {
        "" => {
            let status = if state.minimize_tokens { "on" } else { "off" };
            push_assistant_message(
                state,
                format!(
                    "Token minimization is **{status}**. Use `/minimize on` or `/minimize off` to change it."
                ),
            );
            return true;
        }
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => {
            push_assistant_message(state, format!("Usage: /minimize [on|off] (got `{other}`)"));
            return true;
        }
    };

    set_minimize_tokens(state, desired);
    true
}

/// Apply and persist the token-minimization preference. Updates the live session
/// (`token_prefs` + the cached display flag) and writes `settings.toml`.
#[cfg(feature = "tui")]
fn set_minimize_tokens(state: &mut crate::state::AppState, desired: bool) {
    state.minimize_tokens = desired;
    state.token_prefs.minimize_tokens = Some(desired);

    let mut settings = ahma_common::config::AhmaSettings::load();
    settings.tools.minimize_tokens = desired;
    let status = if desired { "on" } else { "off" };
    match settings.save() {
        Ok(()) => push_assistant_message(
            state,
            format!("Token minimization turned **{status}** (saved to settings)."),
        ),
        Err(e) => push_assistant_message(
            state,
            format!(
                "Token minimization turned **{status}** for this session, but saving to settings failed: {e}"
            ),
        ),
    }
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
        // Focus the task tree — the pane the user came to see. (This used to
        // focus the removed AiActivity pane, so the first keystrokes landed
        // in a pane that was never drawn.)
        "/mode monitor" => set_mode_and_focus(
            state,
            crate::state::Mode::Monitor,
            crate::state::Focus::OpsDag,
        ),
        _ => return false,
    }

    true
}

/// Startup "it just works" behavior (SPEC R24.2): if the hub replay reveals
/// live work for this project — an MCP client (Claude Code, Cursor, …) already
/// running operations — switch straight to the monitor task tree so the user
/// sees what is being done on their behalf without pressing anything. Armed
/// only until the first keystroke, and only fires while still in chat mode.
#[cfg(feature = "tui")]
fn maybe_auto_open_task_view(state: &mut crate::state::AppState) {
    use crate::state::{Mode, OpStatus};

    if !state.auto_view_pending || state.mode == Mode::Monitor {
        return;
    }
    let project = state.project_root.as_deref();
    let has_live_project_work = state.operations.iter().any(|op| {
        op.instance_id.is_some()
            && matches!(
                op.status,
                OpStatus::Running | OpStatus::Pending | OpStatus::Waiting
            )
            && match (project, op.scope.as_deref()) {
                (Some(root), Some(scope)) => crate::task_tree::scope_matches_project(scope, root),
                _ => true,
            }
    });
    if has_live_project_work {
        state.auto_view_pending = false;
        set_mode_and_focus(state, Mode::Monitor, crate::state::Focus::OpsDag);
    }
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
            crate::state::ChatEntry::Thinking { content, .. } => {
                md.push_str("## Thinking\n\n");
                for line in content.lines() {
                    md.push_str("> ");
                    md.push_str(line);
                    md.push('\n');
                }
                md.push('\n');
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

/// Provider administration commands that persist to `~/.ahma/config.toml`:
///
/// - `/provider add <name> <base_url> <model> [num_ctx] [api_key]`
/// - `/provider numctx <tokens|off>` — set the context window for the *current*
///   provider (Ollama only; refused for providers that pin context).
#[cfg(feature = "tui")]
fn handle_provider_admin_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if let Some(rest) = cmd.strip_prefix("/provider add") {
        provider_add_command(rest.trim(), state);
        return true;
    }
    if let Some(rest) = cmd.strip_prefix("/provider numctx") {
        provider_numctx_command(rest.trim(), state);
        return true;
    }
    false
}

#[cfg(feature = "tui")]
fn provider_add_command(args: &str, state: &mut crate::state::AppState) {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() < 3 {
        push_assistant_message(
            state,
            "Usage: /provider add <name> <base_url> <model> [num_ctx] [api_key]\n\
             Example: /provider add together https://api.together.xyz/v1 moonshotai/Kimi-K2 - ${TOGETHER_API_KEY}\n\
             (num_ctx is honored only for Ollama endpoints; use `-` to skip it.)",
        );
        return;
    }
    let (name, base_url, model) = (parts[0], parts[1], parts[2]);
    // 4th token: num_ctx (or `-`/`off` to skip). 5th: api_key.
    let supports_ctx = ahma_common::config::endpoint_supports_num_ctx(
        base_url,
        ahma_common::config::ProviderKind::OpenAi,
    );
    let mut num_ctx = None;
    if let Some(tok) = parts.get(3)
        && !matches!(*tok, "-" | "off" | "_")
    {
        match tok.parse::<u32>() {
            Ok(n) if supports_ctx => num_ctx = Some(n),
            Ok(_) => push_assistant_message(
                state,
                format!(
                    "Note: '{base_url}' does not support num_ctx (not an Ollama endpoint); ignoring it."
                ),
            ),
            Err(_) => {
                push_assistant_message(
                    state,
                    format!("Invalid num_ctx '{tok}' (expected a number)."),
                );
                return;
            }
        }
    }
    let api_key = parts.get(4).map(|s| s.to_string());

    let entry = ahma_common::config::ProviderEntry {
        name: name.to_string(),
        kind: ahma_common::config::ProviderKind::OpenAi,
        base_url: base_url.to_string(),
        default_model: model.to_string(),
        api_key,
        num_ctx,
    };
    match ahma_common::config::AhmaConfig::add_provider(entry) {
        Ok(()) => {
            rebuild_available_providers(state);
            let ctx_note = match (supports_ctx, num_ctx) {
                (_, Some(n)) => format!(" (num_ctx={n})"),
                (true, None) => " (supports num_ctx; set with /provider numctx <n>)".to_string(),
                (false, None) => String::new(),
            };
            push_assistant_message(
                state,
                format!(
                    "Added provider '{name}' → {base_url}{ctx_note}. Select it with /provider."
                ),
            );
        }
        Err(e) => push_assistant_message(state, format!("Could not add provider: {e}")),
    }
}

#[cfg(feature = "tui")]
fn provider_numctx_command(arg: &str, state: &mut crate::state::AppState) {
    let Some(name) = current_provider_name(state) else {
        push_assistant_message(
            state,
            "No current provider selected. Pick one with /provider first.",
        );
        return;
    };
    if arg.is_empty() {
        push_assistant_message(state, "Usage: /provider numctx <tokens|off>");
        return;
    }
    let num_ctx = if matches!(arg, "off" | "0" | "-") {
        None
    } else {
        match arg.parse::<u32>() {
            Ok(n) => Some(n),
            Err(_) => {
                push_assistant_message(
                    state,
                    format!("Invalid num_ctx '{arg}' (expected a number or 'off')."),
                );
                return;
            }
        }
    };
    match ahma_common::config::AhmaConfig::set_provider_num_ctx(&name, num_ctx) {
        Ok(()) => {
            rebuild_available_providers(state);
            let msg = match num_ctx {
                Some(n) => format!(
                    "Set num_ctx={n} for provider '{name}'. It takes effect on the next prompt."
                ),
                None => format!("Cleared num_ctx for provider '{name}'."),
            };
            push_assistant_message(state, msg);
        }
        Err(e) => push_assistant_message(
            state,
            format!(
                "Could not set num_ctx: {e}. (Tip: add it to config first with /provider add, and note hosted clouds don't support it.)"
            ),
        ),
    }
}

/// The name of the currently-selected provider, derived from the `llm_label`
/// (`"Provider / Model"`); `None` if nothing is selected yet.
#[cfg(feature = "tui")]
fn current_provider_name(state: &crate::state::AppState) -> Option<String> {
    let label = provider_label(&state.llm_label);
    if label.is_empty() || label == "no LLM" {
        None
    } else {
        Some(label)
    }
}

#[cfg(feature = "tui")]
fn open_provider_picker(state: &mut crate::state::AppState) {
    use crate::state::PickerState;

    // Annotate each row with its num_ctx capability so the control reads as
    // "16384" / "settable" for Ollama, and a dim "num_ctx: n/a" for hosted
    // clouds that pin context to the model (greyed/not-supported).
    let cfg = ahma_common::config::AhmaConfig::load();
    let items: Vec<String> = state
        .available_providers
        .iter()
        .map(|p| {
            let supports = ahma_common::config::endpoint_supports_num_ctx(
                &p.base_url,
                ahma_common::config::ProviderKind::OpenAi,
            );
            let configured = cfg
                .providers
                .iter()
                .find(|c| c.name == p.name || c.base_url == p.base_url)
                .and_then(|c| c.num_ctx);
            let ctx = match (supports, configured) {
                (_, Some(n)) => format!("  · num_ctx {n}"),
                (true, None) => "  · num_ctx settable".to_string(),
                (false, None) => "  · num_ctx n/a".to_string(),
            };
            format!("{}  {}{}", p.name, p.base_url, ctx)
        })
        .collect();

    if items.is_empty() {
        push_assistant_message(
            state,
            "No providers discovered yet. Install Ollama, add one with `/provider add`, or use `ahma llm add`.",
        );
        return;
    }

    let mut picker = PickerState::new("Select provider", items);
    if let Some(current_url) = &state.current_provider_url
        && let Some(p) = state
            .available_providers
            .iter()
            .find(|p| p.base_url == *current_url)
    {
        // Rows carry a trailing ` · num_ctx …` annotation, so match by the
        // stable `name  base_url` prefix.
        picker.select_prefix(&format!("{}  {}", p.name, p.base_url));
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
        provider: provider.clone(),
        model: model.clone(),
        provider_url: state.current_provider_url.clone(),
        mcp_enabled: state.mcp_enabled,
        active_profile: state.active_profile.clone(),
    };

    if let Ok(cwd) = std::env::current_dir()
        && let Err(e) = cfg.save(&cwd)
    {
        debug!("Failed to save session config: {e}");
    }

    // Also record the selection globally so the MCP sub-agent (and the next
    // session, in any directory) can reuse the model the user last chose.
    persist_selected_model_to_settings(&provider, &model, &state.current_provider_url);
}

/// Persist the most-recently-selected provider/model to `~/.ahma/settings.toml`
/// (the `[agent]` section). Empty values clear the field. Best-effort: a save
/// failure is logged, never surfaced — the per-project session save is primary.
#[cfg(feature = "tui")]
fn persist_selected_model_to_settings(provider: &str, model: &str, provider_url: &Option<String>) {
    let mut settings = ahma_common::config::AhmaSettings::load();
    let to_opt = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    let next_provider = to_opt(provider);
    let next_model = to_opt(model);

    // Avoid a needless disk write when nothing changed.
    if settings.agent.provider == next_provider
        && settings.agent.model == next_model
        && &settings.agent.provider_url == provider_url
    {
        return;
    }
    settings.agent.provider = next_provider;
    settings.agent.model = next_model;
    settings.agent.provider_url = provider_url.clone();
    if let Err(e) = settings.save() {
        debug!("Failed to persist selected model to settings: {e}");
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
    // Merge user-configured providers from ~/.ahma/config.toml so manually-added
    // endpoints (e.g. an OpenAI-protocol cloud added via `/provider add`) are
    // selectable, not just auto-discovered local servers.
    let cfg = ahma_common::config::AhmaConfig::load();
    for p in &cfg.providers {
        if combined
            .iter()
            .any(|c| c.base_url == p.base_url || c.name == p.name)
        {
            continue;
        }
        combined.push(ahma_llm_monitor::LocalProvider {
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            models: vec![p.default_model.clone()],
        });
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
            status: crate::state::WindowStatus::Pending,
            content: vec![format!("Task: {}", step.task)],
            collapsed: false,
            finished_at: None,
            duration_ms: None,
            last_output_at: None,
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
            append_multiline_window_output(&mut w.content, &line);
        }
    }
}

/// Appends `line` to `content`, splitting on embedded newlines so that each
/// resulting segment becomes its own entry (continuing the last existing
/// entry rather than starting a fresh one for the first segment).
#[cfg(feature = "tui")]
fn append_multiline_window_output(content: &mut Vec<String>, line: &str) {
    if content.is_empty() {
        content.push(String::new());
    }
    let parts: Vec<&str> = line.split('\n').collect();
    if let Some(last) = content.last_mut() {
        last.push_str(parts[0]);
    }
    for part in parts.iter().skip(1) {
        content.push(part.to_string());
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
            crate::state::WindowStatus::Finished
        } else {
            crate::state::WindowStatus::Error
        };
        w.content.push(summary);
        w.finished_at = Some(std::time::Instant::now());
        if !success {
            current_failed = true;
        }
    }
    if current_failed {
        cancel_pending_windows(state);
    } else {
        run_next_pending_window(state);
    }
}

/// Marks every still-`Pending` window as `Cancelled`, used to cascade a
/// failure to windows that hadn't started running yet.
#[cfg(feature = "tui")]
fn cancel_pending_windows(state: &mut crate::state::AppState) {
    for w in &mut state.windows {
        if w.status == crate::state::WindowStatus::Pending {
            w.status = crate::state::WindowStatus::Cancelled;
            w.finished_at = Some(std::time::Instant::now());
        }
    }
}

/// Merge freshly discovered external MCP tools into the active tools list,
/// deduping by name and re-sorting, then notify the user in chat.
#[cfg(feature = "tui")]
fn handle_external_tools_refreshed(
    manager: crate::mcp_connections::McpConnectionManager,
    state: &mut crate::state::AppState,
) {
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

/// Build an `ApprovalGate` for a requested tool call and hand it to
/// `AppState::request_approval`. Shared by the daemon-hub (`SourceEvent`)
/// and in-process (`BridgeEvent`) approval-request paths, which differ only
/// in whether a responder channel is present.
#[cfg(feature = "tui")]
fn request_tool_approval(
    state: &mut crate::state::AppState,
    id: String,
    tool: String,
    args: String,
    responder: Option<tokio::sync::oneshot::Sender<bool>>,
) {
    let diff = if tool.contains("replace") || tool == "write_file" {
        serde_json::from_str::<serde_json::Value>(&args)
            .ok()
            .and_then(|val| serde_json::to_string_pretty(&val).ok())
    } else {
        None
    };
    let note = ahma_core::approvals::reask_note(std::path::Path::new(&state.workspace), &tool);
    state.request_approval(
        crate::state::ApprovalGate::new(id, tool.clone(), format!("Execute tool {tool}"))
            .with_note(note)
            .with_diff(diff),
        responder,
    );
}

#[cfg(feature = "tui")]
fn handle_bridge_event(event: crate::llm_bridge::BridgeEvent, state: &mut crate::state::AppState) {
    use crate::llm_bridge::BridgeEvent;
    use crate::state::ChatEntry;

    // Any server signal that the turn is alive and more is coming advances the
    // liveness panel in front of the `ahma` response line; the panel's motion
    // direction follows the turn state (thinking shimmers, streaming rains,
    // tool dispatch scrolls right). Done/Error clear it back to blanks below.
    match &event {
        BridgeEvent::Token(_) => state.mark_stream_activity(crate::state::LivenessState::Streaming),
        BridgeEvent::Thinking(_) => {
            state.mark_stream_activity(crate::state::LivenessState::Thinking)
        }
        BridgeEvent::Usage(_) => state.mark_stream_activity(state.liveness_state),
        BridgeEvent::ToolCallStarted { .. } => {
            state.mark_stream_activity(crate::state::LivenessState::ToolWait)
        }
        BridgeEvent::ToolCallFinished { .. } => {
            // The result is back; the model resumes processing it.
            state.mark_stream_activity(crate::state::LivenessState::Thinking)
        }
        _ => {}
    }

    match event {
        BridgeEvent::Token(token) => {
            state.chat.append_token(&token);
            state.chat_scroll = 0;
        }
        BridgeEvent::Thinking(token) => {
            state.chat.append_thinking(&token);
            state.chat_scroll = 0;
        }
        BridgeEvent::Usage(usage) => {
            state.token_usage.prompt_tokens += usage.prompt_tokens;
            state.token_usage.completion_tokens += usage.completion_tokens;
            state.token_usage.total_tokens += usage.total_tokens;
            if usage.prompt_tokens > 0 {
                state.last_prompt_tokens = usage.prompt_tokens;
            }
        }
        BridgeEvent::Done => {
            state.reset_liveness();
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
            state.reset_liveness();
            state.chat.finish_stream();
            state.chat.finish_user_timing();
            state.chat.push(ChatEntry::Assistant {
                content: format!("Error: {msg}"),
                streaming: false,
            });
            state.chat_scroll = 0;
        }
        BridgeEvent::Truncated { reason } => {
            // Never leave a cut-off response looking like a silent hang: a
            // visible note in the transcript itself, right where the user is
            // already looking, before the continuation's tokens start arriving.
            state.chat.push(ChatEntry::Assistant {
                content: format!("[{reason}]"),
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
            // Name the chat line the same way the monitor names its rows: by what
            // was actually run (SPEC R24.7). A column of identical
            // `run_terminal_command` lines tells the user nothing about their own
            // session — `cargo nextest run -p ahma_core` tells them everything.
            let title = serde_json::from_str::<serde_json::Value>(&args)
                .ok()
                .map(|v| ahma_common::op_identity::title_for_value(&name, Some(&v)))
                .unwrap_or_else(|| name.clone());
            state.chat.start_tool_call(id, title, args);
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
            handle_external_tools_refreshed(manager, state);
        }
        BridgeEvent::RequestApproval { id, tool, args, tx } => {
            request_tool_approval(state, id, tool, args, Some(tx));
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
fn format_friendly_start(op: &crate::state::Operation) -> String {
    let time_str = op.started_time.format("%H:%M:%S").to_string();
    // When the wire carried the real command it is already shown in the
    // window's `$` header — a bare timestamp is enough here. The parsed args
    // summary is only a fallback for pre-R24.7 servers.
    if op.command.is_some() {
        return format!("Started at {time_str}");
    }
    let display = op.display_name();
    // A display name other than the raw tool name already carries the parsed
    // command — repeating it as an args summary would print it twice.
    let args_summary = if display == op.tool_name {
        extract_args_summary(&op.tool_name, &op.description).unwrap_or_default()
    } else {
        String::new()
    };
    if args_summary.is_empty() {
        format!("Starting {display} at {time_str}")
    } else {
        format!("Starting {display} ({args_summary}) at {time_str}")
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
    content.push(format_friendly_start(op));

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
fn window_status_for(op: &crate::state::Operation) -> crate::state::WindowStatus {
    use crate::state::{OpStatus, WindowStatus};
    match op.status {
        OpStatus::Running => WindowStatus::Running,
        OpStatus::Pending | OpStatus::Waiting => WindowStatus::Pending,
        OpStatus::Succeeded => WindowStatus::Finished,
        OpStatus::Failed => WindowStatus::Error,
        OpStatus::Cancelled => WindowStatus::Cancelled,
    }
}

/// Whether cards from several distinct instances currently interleave. With a
/// single active instance the per-card instance suffix is pure repetition.
#[cfg(feature = "tui")]
fn has_multiple_instances(ops: &[crate::state::Operation]) -> bool {
    let distinct: std::collections::HashSet<&str> = ops
        .iter()
        .map(|o| o.instance_label.as_deref().unwrap_or("local"))
        .collect();
    distinct.len() > 1
}

/// The window label: the operation's human title (`display_name`, SPEC R24.7).
/// The owning instance is appended only when more than one instance is active —
/// in a single-instance session the suffix would repeat on every card.
#[cfg(feature = "tui")]
fn window_label_for(op: &crate::state::Operation, multi_instance: bool) -> String {
    let name = op.display_name();
    match (&op.instance_label, multi_instance) {
        (Some(instance), true) => format!("{name} ({instance})"),
        _ => name,
    }
}

#[cfg(feature = "tui")]
fn update_existing_window(
    w: &mut crate::state::TuiWindow,
    op: &crate::state::Operation,
    unicode: bool,
    multi_instance: bool,
) {
    w.status = window_status_for(op);

    if op.status != crate::state::OpStatus::Running
        && op.status != crate::state::OpStatus::Pending
        && op.status != crate::state::OpStatus::Waiting
        && w.finished_at.is_none()
    {
        w.finished_at = Some(std::time::Instant::now());
    }

    // The server-computed title/command can arrive on a later upsert than the
    // one that materialised the window — refresh identity, not just content.
    w.label = window_label_for(op, multi_instance);
    w.command = op.command.clone().unwrap_or_else(|| op.display_name());
    w.duration_ms = op.duration_ms;
    w.last_output_at = op.last_output_at;
    w.content = window_content_for(op, unicode);
}

#[cfg(feature = "tui")]
fn build_new_window(
    op: &crate::state::Operation,
    state: &mut crate::state::AppState,
    multi_instance: bool,
) -> crate::state::TuiWindow {
    let win_id = state.next_window_id;
    state.next_window_id = (state.next_window_id + 1) % 100;

    let label = window_label_for(op, multi_instance);

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
        duration_ms: op.duration_ms,
        last_output_at: op.last_output_at,
        is_cli: true,
        command: op.command.clone().unwrap_or_else(|| op.display_name()),
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

    let multi_instance = has_multiple_instances(&ops);

    for op in &ops {
        if let Some(w) = state
            .windows
            .iter_mut()
            .find(|w| w.op_id.as_deref() == Some(&op.id))
        {
            update_existing_window(w, op, state.unicode, multi_instance);
        } else if !state.window_suppressed_by_clear(op) {
            let w = build_new_window(op, state, multi_instance);
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
        // The cumulative counter drives the log title's rain panel: each
        // arriving line advances the animation one frame, so pour rate shows
        // arrival rate.
        state.log_lines_total = state
            .log_lines_total
            .wrapping_add(content.lines().count() as u64);
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
            maybe_auto_open_task_view(state);
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
            state.mark_stream_activity(crate::state::LivenessState::Streaming);
            state.chat_scroll = 0;
        }
        SourceEvent::ChatThinking { token } => {
            state.chat.append_thinking(&token);
            state.mark_stream_activity(crate::state::LivenessState::Thinking);
            state.chat_scroll = 0;
        }
        SourceEvent::ApprovalRequested { id, tool, args } => {
            request_tool_approval(state, id, tool, args, None);
        }
        SourceEvent::ScopeGrantRequested { request } => {
            // A grant prompt never overwrites a different pending one silently; the
            // newest replaces only if it is a fresh decision (dedup happens upstream
            // in the GrantCoordinator, so duplicates never reach here).
            state.scope_grant = Some(crate::state::ScopeGrantGate::from_request(request));
        }
        SourceEvent::ScopeGrantDismiss { decision_id } => {
            if state
                .scope_grant
                .as_ref()
                .is_some_and(|g| g.decision_id == decision_id)
            {
                state.scope_grant = None;
            }
        }
        SourceEvent::WebApprovalRequested { request } => {
            // Dedup happens upstream in the WebApprovalCoordinator, so duplicates
            // never reach here; the newest request becomes the pending prompt.
            state.web_approval = Some(crate::state::WebApprovalGate::from_request(request));
        }
        SourceEvent::WebApprovalDismiss { decision_id } => {
            if state
                .web_approval
                .as_ref()
                .is_some_and(|g| g.decision_id == decision_id)
            {
                state.web_approval = None;
            }
        }
        SourceEvent::AgentDone => {
            state.reset_liveness();
            state.chat.finish_stream();
            state.chat.finish_user_timing();
        }
        SourceEvent::AgentError { error } => {
            state.reset_liveness();
            state.chat.finish_stream();
            state.chat.push(crate::state::ChatEntry::Assistant {
                content: format!("Error: {error}"),
                streaming: false,
            });
        }
        SourceEvent::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        } => {
            // Mirror the in-process BridgeEvent::Usage accumulation so the
            // status-bar token counter populates on the daemon-hub path too.
            state.token_usage.prompt_tokens += prompt_tokens;
            state.token_usage.completion_tokens += completion_tokens;
            state.token_usage.total_tokens += total_tokens;
            if prompt_tokens > 0 {
                state.last_prompt_tokens = prompt_tokens;
            }
            state.mark_stream_activity(state.liveness_state);
        }
        SourceEvent::ToolCallStarted { id, name, args } => {
            state.mark_stream_activity(crate::state::LivenessState::ToolWait);
            state.chat.start_tool_call(id, name, args);
            state.chat_scroll = 0;
        }
        SourceEvent::ToolCallFinished { id, result, failed } => {
            state.mark_stream_activity(crate::state::LivenessState::Thinking);
            state.chat.finish_tool_call(&id, result, failed);
            state.chat_scroll = 0;
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
        // Output for an operation we have not seen yet. This happens when an
        // OpOutput line outruns the snapshot that materialises the op (e.g.
        // after a daemon-hub reconnect, which replays OpStarted/OpFinished but
        // not OpOutput). Buffer the line instead of dropping it; it is flushed
        // into the op's tail by `upsert_operation` once the op appears — so
        // fast commands like `!pwd` no longer lose their output.
        state.buffer_pending_output(op_id, line);
        return;
    };

    {
        let op = &mut state.operations[idx];
        if op.stdout_tail.len() >= crate::state::STDOUT_TAIL_CAP {
            op.stdout_tail.pop_front();
        }
        op.stdout_tail.push_back(line);
        op.last_output_at = Some(std::time::Instant::now());
    }

    let op = state.operations[idx].clone();
    let unicode = state.unicode;
    let multi_instance = has_multiple_instances(&state.operations);
    if let Some(w) = state
        .windows
        .iter_mut()
        .find(|w| w.op_id.as_deref() == Some(op_id))
    {
        update_existing_window(w, &op, unicode, multi_instance);
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
    if let Some(pos) = state
        .windows
        .iter()
        .position(|w| w.status == crate::state::WindowStatus::Pending)
    {
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
    w.status = crate::state::WindowStatus::Running;

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
            let num_ctx = provider_num_ctx(&base_url);
            spawn_window_llm_task(win_id, base_url, model, num_ctx, command, mcp, abort_rx, tx);
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
        w.status = crate::state::WindowStatus::Cancelled;
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

    let system_prompt = state.mcp_enabled.then(|| build_system_prompt(state));

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
        ClickTarget::TreeRow(row_idx) => {
            // Click = select + toggle: accordion-expand an operation into its
            // live/historic output view, or fold an instance/session header.
            state.ops_selected = row_idx;
            state.toggle_selected_tree_node();
            state.focus = crate::state::Focus::OpsDag;
        }
        ClickTarget::OpenOperationDetail(op_id) => {
            state.open_operation_detail(op_id);
        }
    }
}

/// Returns true when the click position falls on the close button area of a window.
/// Single-height windows use their entire right edge; taller windows require
/// the click to be on the title row.
#[cfg(feature = "tui")]
fn is_close_button_click(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    // Wide enough for the explicit "[x99]" close cell plus its margin.
    let in_close_zone = col >= rect.x + rect.width.saturating_sub(6);
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
        Detail(usize),
    }
    let hit = {
        let rects = state.window_rects.borrow();
        rects.iter().find_map(|&(win_id, rect)| {
            if !inside_rect(col, row, rect) {
                return None;
            }
            if is_close_button_click(col, row, rect) {
                Some(Hit::Close(win_id))
            } else if is_collapse_marker_click(col, row, rect) {
                Some(Hit::Toggle(win_id))
            } else {
                Some(Hit::Detail(win_id))
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
        Some(Hit::Detail(win_id)) => {
            // Click on the card body = drill into the operation's full-screen
            // detail view. Windows without a backing operation (UNSANDBOXED
            // CLI runs, LLM steps) keep the old expand/collapse behavior.
            let op_id = state
                .windows
                .iter()
                .find(|w| w.id == win_id)
                .and_then(|w| w.op_id.clone());
            match op_id {
                Some(id) => state.open_operation_detail(id),
                None => {
                    if let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id) {
                        w.collapsed = !w.collapsed;
                    }
                }
            }
            true
        }
        None => false,
    }
}

/// The `[+] N` / `[-] N` marker at the left edge of a card's title row —
/// clicking it toggles expand/collapse rather than drilling into the detail
/// view. Collapsed cards are one row; expanded cards count only their top row.
#[cfg(feature = "tui")]
fn is_collapse_marker_click(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    let on_title_row = if rect.height == 1 {
        true
    } else {
        row == rect.y
    };
    on_title_row && col < rect.x + 8
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
    // The operation-detail overlay covers the screen — wheel scrolls it.
    let detail_max = state.detail_max_scroll.get();
    if let crate::state::ModalState::OperationDetail(d) = &mut state.modal {
        d.scroll = if up {
            d.scroll.saturating_sub(1)
        } else {
            (d.scroll + 1).min(detail_max)
        };
        return;
    }

    let chat_area = state.chat_area.get();
    if inside_rect(col, row, chat_area) {
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
    if inside_rect(col, row, log_area) {
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
    // The operation-detail overlay captures paging while open.
    let detail_max = state.detail_max_scroll.get();
    if let crate::state::ModalState::OperationDetail(d) = &mut state.modal {
        let page = 10;
        d.scroll = if up {
            d.scroll.saturating_sub(page)
        } else {
            (d.scroll + page).min(detail_max)
        };
        return;
    }

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
    state.liveness_state != crate::state::LivenessState::Idle
}

#[cfg(test)]
mod tests {
    use super::parse_run_command;
    use serde_json::json;

    /// Regression for issue #484: the provider's declared `num_ctx` must be
    /// discoverable from its base URL so `resolve_token_prefs` can populate
    /// `context_length` without an explicit `--context-length` flag.
    #[test]
    fn provider_num_ctx_matches_base_url_ignoring_trailing_slash() {
        use ahma_common::config::{AhmaConfig, ProviderEntry, ProviderKind};
        let cfg = AhmaConfig {
            providers: vec![
                ProviderEntry {
                    name: "ollama-local".to_string(),
                    kind: ProviderKind::OpenAi,
                    base_url: "http://localhost:11434/v1/".to_string(),
                    default_model: "m".to_string(),
                    api_key: None,
                    num_ctx: Some(16_384),
                },
                ProviderEntry {
                    name: "no-ctx".to_string(),
                    kind: ProviderKind::OpenAi,
                    base_url: "http://localhost:9999/v1".to_string(),
                    default_model: "m".to_string(),
                    api_key: None,
                    num_ctx: None,
                },
            ],
            ..AhmaConfig::default()
        };
        assert_eq!(
            cfg.num_ctx_for_base_url("http://localhost:11434/v1"),
            Some(16_384),
            "trailing-slash difference must not hide the provider"
        );
        assert_eq!(cfg.num_ctx_for_base_url("http://localhost:9999/v1"), None);
        assert_eq!(cfg.num_ctx_for_base_url("http://elsewhere/v1"), None);
    }

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
    fn minimize_command_toggles_persists_and_reports() {
        use crate::state::AppState;
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: debug-only test seam; nextest isolates each test in its own process.
        unsafe {
            std::env::set_var("AHMA_TEST_HOME", dir.path());
        }

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        assert!(!state.minimize_tokens, "default off");

        // An unrecognised argument is still claimed (returns true) but changes nothing.
        assert!(super::handle_minimize_nav_command(
            "/minimize bogus",
            &mut state
        ));
        assert!(!state.minimize_tokens);

        // Turn on: live display flag, session override, and persisted setting.
        assert!(super::handle_minimize_nav_command(
            "/minimize on",
            &mut state
        ));
        assert!(state.minimize_tokens);
        assert_eq!(state.token_prefs.minimize_tokens, Some(true));
        assert!(
            ahma_common::config::AhmaSettings::load()
                .tools
                .minimize_tokens
        );

        // Turn off again, persisted.
        assert!(super::handle_minimize_nav_command(
            "/minimize off",
            &mut state
        ));
        assert!(!state.minimize_tokens);
        assert!(
            !ahma_common::config::AhmaSettings::load()
                .tools
                .minimize_tokens
        );

        // A non-minimize command is not claimed by this handler.
        assert!(!super::handle_minimize_nav_command("/help", &mut state));

        unsafe {
            std::env::remove_var("AHMA_TEST_HOME");
        }
    }

    #[test]
    fn persist_selected_model_writes_and_clears_agent_settings() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: debug-only test seam; nextest isolates each test in its own process.
        unsafe {
            std::env::set_var("AHMA_TEST_HOME", dir.path());
        }

        super::persist_selected_model_to_settings(
            "Ollama",
            "gemma3:27b",
            &Some("http://localhost:11434".to_string()),
        );
        let s = ahma_common::config::AhmaSettings::load();
        assert_eq!(s.agent.provider.as_deref(), Some("Ollama"));
        assert_eq!(s.agent.model.as_deref(), Some("gemma3:27b"));
        assert_eq!(
            s.agent.provider_url.as_deref(),
            Some("http://localhost:11434")
        );

        // Empty values clear the fields.
        super::persist_selected_model_to_settings("", "", &None);
        let s2 = ahma_common::config::AhmaSettings::load();
        assert_eq!(s2.agent.provider, None);
        assert_eq!(s2.agent.model, None);
        assert_eq!(s2.agent.provider_url, None);

        unsafe {
            std::env::remove_var("AHMA_TEST_HOME");
        }
    }

    #[test]
    fn minimal_composer_drops_heavy_live_context() {
        use super::{FullComposer, MinimalComposer, PromptComposer, PromptParts};
        let parts = PromptParts {
            profile_prompt: String::new(),
            base: "BASE".to_string(),
            workspace_line: "Workspace: /w\n".to_string(),
            sandbox_line: "Sandbox: on\n".to_string(),
            recent_ops: "OPS-HEAVY\n".to_string(),
            recent_failures: "FAILS-HEAVY\n".to_string(),
        };

        // The default composer includes the full live context.
        let full = FullComposer.compose(&parts);
        assert!(full.contains("OPS-HEAVY") && full.contains("FAILS-HEAVY"));
        assert!(full.contains("Workspace: /w") && full.contains("BASE"));

        // The minimal composer drops the token-heavy ops/failures, but keeps the
        // cheap workspace/sandbox lines and the agentic base.
        let minimal = MinimalComposer.compose(&parts);
        assert!(!minimal.contains("OPS-HEAVY") && !minimal.contains("FAILS-HEAVY"));
        assert!(minimal.contains("Workspace: /w") && minimal.contains("Sandbox: on"));
        assert!(minimal.contains("BASE"));
    }

    #[test]
    fn test_handle_window_nav_commands() {
        use crate::state::{AppState, TuiWindow};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        let w = TuiWindow {
            id: 3,
            label: "Test Window".to_string(),
            status: crate::state::WindowStatus::Running,
            content: vec![],
            collapsed: true,
            finished_at: None,
            duration_ms: None,
            last_output_at: None,
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
        assert_eq!(
            state.windows[0].status,
            crate::state::WindowStatus::Cancelled
        );

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

    /// Build a pending scope-grant gate for the key-handler tests.
    #[cfg(test)]
    fn test_scope_grant_gate() -> crate::state::ScopeGrantGate {
        use ahma_common::scope_grant::{GrantReason, ScopeGrantRequest};
        crate::state::ScopeGrantGate::from_request(ScopeGrantRequest {
            decision_id: "dec_test".to_string(),
            path: std::path::PathBuf::from("/some/external/dir"),
            access: ahma_common::config::ScopeAccess::Rw,
            reason: GrantReason::PreExecViolation,
            tool: Some("run_terminal_command".to_string()),
        })
    }

    /// `y` widens to read+write, clears the gate, and is consumed even with chat
    /// focus so it never lands in the input box.
    #[test]
    fn test_scope_grant_key_y_grants_rw() {
        use crate::state::{AppState, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;
        state.scope_grant = Some(test_scope_grant_gate());

        let handled = super::handle_scope_grant_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(handled, "y must be consumed by the scope-grant modal");
        assert!(state.scope_grant.is_none(), "gate should be cleared");
        assert!(
            state.chat_input_is_empty(),
            "y must not land in the input box"
        );
    }

    /// `r` grants read-only and clears the gate.
    #[test]
    fn test_scope_grant_key_r_grants_ro() {
        use crate::state::AppState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.scope_grant = Some(test_scope_grant_gate());

        let handled = super::handle_scope_grant_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(handled);
        assert!(state.scope_grant.is_none());
    }

    /// Enter / Esc / `n` all resolve to the safe default Deny — Enter must never
    /// widen the sandbox (SPEC R5.3.1).
    #[test]
    fn test_scope_grant_key_enter_esc_n_deny() {
        use crate::state::AppState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('n')] {
            let mut state = AppState::new("http://localhost:3000", "HTTP", true);
            state.scope_grant = Some(test_scope_grant_gate());

            let handled =
                super::handle_scope_grant_key(KeyEvent::new(code, KeyModifiers::NONE), &mut state);
            assert!(handled, "{code:?} must be consumed (deny)");
            assert!(state.scope_grant.is_none(), "{code:?} must clear the gate");
        }
    }

    /// With no pending gate the handler is inert so other handlers see the key.
    #[test]
    fn test_scope_grant_key_noop_when_no_gate() {
        use crate::state::AppState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        let handled = super::handle_scope_grant_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(!handled, "no gate → not handled, key falls through");
    }

    /// Build a pending web-approval gate for the key-handler tests.
    #[cfg(test)]
    fn test_web_approval_gate() -> crate::state::WebApprovalGate {
        use ahma_common::web_approval::WebApprovalRequest;
        crate::state::WebApprovalGate::from_request(WebApprovalRequest {
            decision_id: "web_test".to_string(),
            domain: "api.github.com".to_string(),
            url: "https://api.github.com/repos".to_string(),
            tool: Some("fetch_webpage".to_string()),
        })
    }

    /// `a`/`s` resolve to allow-always / allow-session, clear the gate, and are
    /// consumed even with chat focus so they never land in the input box.
    #[test]
    fn test_web_approval_key_allow_variants() {
        use crate::state::{AppState, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for code in [KeyCode::Char('a'), KeyCode::Char('s')] {
            let mut state = AppState::new("http://localhost:3000", "HTTP", true);
            state.focus = Focus::Chat;
            state.web_approval = Some(test_web_approval_gate());

            let handled =
                super::handle_web_approval_key(KeyEvent::new(code, KeyModifiers::NONE), &mut state);
            assert!(
                handled,
                "{code:?} must be consumed by the web-approval modal"
            );
            assert!(state.web_approval.is_none(), "{code:?} must clear the gate");
            assert!(
                state.chat_input_is_empty(),
                "{code:?} must not land in the input box"
            );
        }
    }

    /// Enter / Esc / `n` all resolve to the safe default Deny — Enter must never
    /// widen egress (SPEC R-WEB.6, mirrors R5.3.1).
    #[test]
    fn test_web_approval_key_enter_esc_n_deny() {
        use crate::state::AppState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('n')] {
            let mut state = AppState::new("http://localhost:3000", "HTTP", true);
            state.web_approval = Some(test_web_approval_gate());

            let handled =
                super::handle_web_approval_key(KeyEvent::new(code, KeyModifiers::NONE), &mut state);
            assert!(handled, "{code:?} must be consumed (deny)");
            assert!(state.web_approval.is_none(), "{code:?} must clear the gate");
        }
    }

    /// With no pending gate the handler is inert so other handlers see the key.
    #[test]
    fn test_web_approval_key_noop_when_no_gate() {
        use crate::state::AppState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        let handled = super::handle_web_approval_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(!handled, "no gate → not handled, key falls through");
    }

    /// Shift+Enter inserts a newline into the chat input instead of submitting,
    /// while plain Enter still submits.
    #[test]
    fn test_shift_enter_inserts_newline() {
        use crate::state::{AppState, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;
        state.chat_input.insert_str("first");

        let handled = super::handle_chat_input_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut state,
        );
        assert!(handled, "Shift+Enter must be consumed by the chat input");

        state.chat_input.insert_str("second");
        assert_eq!(
            state.chat_input.lines(),
            ["first", "second"],
            "Shift+Enter must add a line without submitting"
        );
        assert_eq!(
            state.chat_input.lines().len(),
            2,
            "buffer should still hold both lines (not submitted)"
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
            status: crate::state::WindowStatus::Running,
            content: vec![],
            collapsed: false,
            finished_at: None,
            duration_ms: None,
            last_output_at: None,
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
        assert_eq!(
            state.windows[0].status,
            crate::state::WindowStatus::Cancelled
        );

        // Restore window
        state.windows[0].visible = true;
        state.windows[0].status = crate::state::WindowStatus::Running;

        // Type "X26" in chat input
        state.chat_input.insert_str("X26");
        super::submit_chat_input(&mut state);

        assert!(!state.windows[0].visible);
        assert_eq!(
            state.windows[0].status,
            crate::state::WindowStatus::Cancelled
        );
    }

    /// `z`/Enter zoom toggles the focused pane full-screen; Esc restores the
    /// layout before it returns focus to chat.
    #[test]
    fn zoom_toggles_focused_pane_and_esc_restores() {
        use crate::keymap::Action;
        use crate::state::{AppState, Focus};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        state.focus = Focus::OpsDag;
        super::handle_action(Action::ToggleZoom, &mut state);
        assert_eq!(state.zoomed, Some(Focus::OpsDag));

        // Esc: first unzoom (focus stays), then focus chat.
        super::handle_action(Action::FocusChat, &mut state);
        assert_eq!(state.zoomed, None);
        assert_eq!(state.focus, Focus::OpsDag);
        super::handle_action(Action::FocusChat, &mut state);
        assert_eq!(state.focus, Focus::Chat);

        // Chat focus is not zoomable.
        super::handle_action(Action::ToggleZoom, &mut state);
        assert_eq!(state.zoomed, None);
    }

    /// `/mode monitor` must land focus on a pane that is actually drawn.
    #[test]
    fn mode_monitor_focuses_task_tree() {
        use crate::state::{AppState, Focus, Mode};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        assert!(super::handle_mode_nav_command("/mode monitor", &mut state));
        assert_eq!(state.mode, Mode::Monitor);
        assert_eq!(state.focus, Focus::OpsDag);
    }

    /// Clicking a card's body drills into the operation detail overlay;
    /// the `[+]` marker zone still toggles collapse; the `[x]` zone closes.
    #[tokio::test]
    async fn card_click_zones_route_detail_toggle_and_close() {
        use crate::state::{AppState, ModalState, OpStatus, Operation};
        use ratatui::layout::Rect;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        let mut op = Operation::new("op_9", "run_terminal_command", OpStatus::Running);
        op.title = Some("cargo build".into());
        state.operations.push(op);
        super::sync_operations_to_windows(&mut state);
        let win_id = state.windows[0].id;

        // Simulate the drawn frame: card occupies a 40x3 rect at origin.
        let rect = Rect::new(0, 0, 40, 3);
        state.window_rects.borrow_mut().push((win_id, rect));

        // Body click (below the title row) → detail overlay.
        assert!(super::handle_window_rect_click(20, 1, &mut state));
        match &state.modal {
            ModalState::OperationDetail(d) => assert_eq!(d.op_id, "op_9"),
            other => panic!("expected detail overlay, got {other:?}"),
        }
        state.modal = ModalState::None;

        // Collapse-marker click (title row, far left) → toggle, no overlay.
        let was_collapsed = state.windows[0].collapsed;
        assert!(super::handle_window_rect_click(2, 0, &mut state));
        assert_eq!(state.windows[0].collapsed, !was_collapsed);
        assert!(matches!(state.modal, ModalState::None));

        // Close-cell click (title row, far right) → hidden.
        assert!(super::handle_window_rect_click(38, 0, &mut state));
        assert!(!state.windows[0].visible);
    }

    /// The chat card must show the server-computed title and the real command
    /// (SPEC R24.7), not the raw tool name — and refresh them when identity
    /// arrives on a later upsert than the one that materialised the window.
    #[tokio::test]
    async fn window_cards_carry_op_title_and_command() {
        use crate::state::{AppState, OpStatus, Operation};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        let mut op = Operation::new("op_1", "run_terminal_command", OpStatus::Running);
        op.instance_label = Some("antigravity-client".into());
        state.operations.push(op);
        super::sync_operations_to_windows(&mut state);

        // No title yet, single instance → tool-name fallback, no instance suffix.
        assert_eq!(state.windows[0].label, "run_terminal_command");
        assert_eq!(state.windows[0].command, "run_terminal_command");

        // Title + command arrive later (e.g. post-reconnect snapshot).
        state.operations[0].title = Some("cargo nextest run".into());
        state.operations[0].command = Some("cargo nextest run --workspace".into());
        state.operations[0].status = OpStatus::Succeeded;
        state.operations[0].duration_ms = Some(2140);
        super::sync_operations_to_windows(&mut state);

        assert_eq!(state.windows[0].label, "cargo nextest run");
        assert_eq!(state.windows[0].command, "cargo nextest run --workspace");
        assert_eq!(state.windows[0].duration_ms, Some(2140));
        assert_eq!(
            state.windows[0].status,
            crate::state::WindowStatus::Finished
        );
    }

    /// With operations from several instances, each card names its instance.
    #[tokio::test]
    async fn window_label_names_instance_only_when_multiple() {
        use crate::state::{AppState, OpStatus, Operation};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        let mut a = Operation::new("op_a", "run_terminal_command", OpStatus::Running);
        a.title = Some("cargo build".into());
        a.instance_label = Some("cursor".into());
        let mut b = Operation::new("op_b", "run_terminal_command", OpStatus::Running);
        b.title = Some("git status".into());
        b.instance_label = Some("claude-code".into());
        state.operations.push(a);
        state.operations.push(b);
        super::sync_operations_to_windows(&mut state);

        assert_eq!(state.windows[0].label, "cargo build (cursor)");
        assert_eq!(state.windows[1].label, "git status (claude-code)");
    }

    /// With a wire command the start line is a bare timestamp — the `$` header
    /// already shows the command, so repeating it is noise.
    #[test]
    fn friendly_start_is_timestamp_only_when_command_known() {
        use crate::state::{OpStatus, Operation};
        let mut op = Operation::new("op_1", "run_terminal_command", OpStatus::Running);
        op.command = Some("cargo build".into());
        assert!(super::format_friendly_start(&op).starts_with("Started at "));

        op.command = None;
        op.title = Some("cargo build".into());
        assert_eq!(
            super::format_friendly_start(&op),
            format!(
                "Starting cargo build at {}",
                op.started_time.format("%H:%M:%S")
            )
        );
    }
}
