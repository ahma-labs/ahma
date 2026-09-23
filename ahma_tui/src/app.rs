//! Ratatui event loop and action dispatcher.
//!
//! [`run`] launches the ratatui terminal UI and restores the terminal on the
//! way out, including on panic.

use anyhow::Result;
use tracing::debug;

use crate::connection::ResolvedConnection;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Launch the TUI. Restores the terminal on exit, even on error.
pub async fn run(
    connection: &ResolvedConnection,
    profile_override: Option<String>,
    workspace_path: Option<std::path::PathBuf>,
    token_prefs: crate::TokenPrefs,
) -> Result<()> {
    use std::io;
    use std::time::Duration;

    // The teardown counterparts of these (LeaveAlternateScreen, DisableMouseCapture,
    // DisableBracketedPaste, PopKeyboardEnhancementFlags, disable_raw_mode) are
    // deliberately not imported here: they belong to `terminal_guard`, which owns
    // the single restore path. Undoing the setup from two places is how the panic
    // path came to be missed.
    use crossterm::{
        event::{
            EnableBracketedPaste, EnableMouseCapture, Event, EventStream, KeyboardEnhancementFlags,
            PushKeyboardEnhancementFlags,
        },
        execute,
        terminal::{EnterAlternateScreen, enable_raw_mode, supports_keyboard_enhancement},
    };
    use futures::StreamExt;
    use ratatui::{Terminal, backend::CrosstermBackend};
    use tokio::sync::mpsc;

    use crate::daemon_source::spawn_daemon_source;
    use crate::keymap::map_key;
    use crate::llm_bridge::{BridgeEvent, spawn_discovery_task};
    use crate::mcp_source::{SourceEvent, spawn_mcp_source};
    use crate::state::AppState;
    use crate::theme::Theme;
    use crate::ui;

    let unicode = detect_unicode();
    let theme = Theme::with_color(unicode, !no_color());
    let mut state = AppState::new(
        &connection.display_url,
        connection.transport_label(),
        unicode,
    );
    if let Some(ref path) = workspace_path {
        // Canonicalized like `project_root` below: this string becomes the chat
        // session's `McpChatConfig.workspace_root` and hence its `roots/list`
        // answer, so a relative or symlinked spelling here would hand the server
        // a different-looking scope than the one the bridge locked.
        state.workspace = dunce::canonicalize(path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .into_owned();
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
            state.llm_selection = Some(crate::state::LlmSelection::profile(
                profile.name.clone(),
                profile.model,
            ));
        } else {
            // The user asked for a specific profile by name and did not get it;
            // opening as if they had never passed the flag is a surprise.
            crate::startup_notices::push(
                crate::startup_notices::Level::Warn,
                format!(
                    "--profile {profile_name} could not be loaded; continuing without it. \
                     Use /agent list to see the profiles saved for this project."
                ),
            );
        }
    }

    // Everything that happened before the first frame (bridge restarts, a self
    // re-exec, a refused reuse, this profile failure) surfaces here rather than
    // in a log file the user was never told about.
    drain_startup_notices(&mut state);
    load_granted_scopes(&mut state);
    offer_folder_trust(&mut state);

    let (mcp_tx, mut mcp_rx) = mpsc::channel::<SourceEvent>(256);
    state.mcp_source_tx = Some(spawn_mcp_source(
        connection.clone(),
        mcp_tx.clone(),
        workspace_path,
    ));
    // The TUI is always a subscriber, never the hub (SPEC R-DAEMON.9). It used
    // to bind the hub socket itself when it started first, which made the
    // observability of every other client depend on this window staying open:
    // quitting the TUI unlinked the socket and every attached instance lost its
    // event stream until it reconnected. The per-user daemon owns the hub; this
    // process only watches it, and quitting sends nothing but EOF.
    spawn_daemon_source(mcp_tx.clone());
    // ...and a second, outgoing connection, because the TUI is also a place
    // work happens: a `!` command runs here, unsandboxed, and used to be the
    // one kind of work the unified view could not see (SPEC R-DAEMON.9).
    state.tui_reporter = Some(crate::tui_reporter::spawn_tui_reporter(
        state.workspace.clone(),
    ));

    // Bridge channel carries both provider discovery results and LLM tokens.
    let (bridge_tx, mut bridge_rx) = mpsc::channel::<BridgeEvent>(512);
    spawn_discovery_task(bridge_tx.clone());
    // Local model servers come and go (Ollama started after the TUI, LM Studio
    // quit): look again now and then while idle, so the picker and the
    // fallback work from what is actually running rather than a startup
    // snapshot.
    let mut last_discovery = std::time::Instant::now();
    // Populate the external MCP tools counter at startup (avoids needing `/mcp refresh`).
    crate::llm_bridge::spawn_external_tools_refresh(
        state.mcp_connections.clone(),
        bridge_tx.clone(),
    );
    // Store the sender so chat actions can spawn tasks later.
    state.bridge_tx = Some(bridge_tx);

    // ── Terminal setup ───────────────────────────────────────────────────────
    enable_raw_mode()?;
    // Arm the restore *immediately*, before the rest of the setup. From here on
    // every exit path — normal, `?`, or panic — puts the terminal back; see
    // `terminal_guard`. Armed before `execute!` rather than after, because a
    // failure partway through the setup used to return `Err` with raw mode
    // already on.
    let terminal_guard = crate::terminal_guard::TerminalGuard::arm();
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
        // Tell the guard only once the push has actually succeeded: popping a
        // flag that was never pushed unbalances crossterm's stack.
        terminal_guard.set_keyboard_enhanced(true);
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal.draw(|f| ui::draw(f, &state, &theme))?;

    // ── Event loop ───────────────────────────────────────────────────────────
    let mut event_stream = EventStream::new();

    // Upper bound on how many queued source/bridge events one loop iteration
    // drains before drawing. Batching turns a firehose of per-line output
    // events (e.g. a chatty build) into one draw per pass instead of one full
    // widget-tree rebuild per event, while the bound keeps input and tick
    // handling responsive during a flood.
    const MAX_EVENT_DRAIN_PER_PASS: usize = 256;

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
                            if key.code == crossterm::event::KeyCode::Char('c')
                                && key.modifiers == crossterm::event::KeyModifiers::CONTROL
                            {
                                // One meaning everywhere, ahead of every
                                // overlay: cancel the turn, else quit.
                                interrupt(&mut state);
                            } else if (key.code == crossterm::event::KeyCode::PageUp || key.code == crossterm::event::KeyCode::PageDown)
                                && !state.is_help_open()
                                && state.log_files_selected().is_none()
                            {
                                handle_page_up_down(key.code == crossterm::event::KeyCode::PageUp, &mut state);
                            } else if handle_settings_key(key, &mut state)
                                || handle_help_key(key, &mut state)
                                || handle_picker_key(key, &mut state)
                                || handle_trust_key(key, &mut state)
                                || handle_scope_grant_key(key, &mut state)
                                || handle_web_approval_key(key, &mut state)
                                || handle_approval_key(key, &mut state)
                                || handle_chat_input_key(key, &mut state)
                            {
                                // handled directly by an overlay/editor widget
                            } else {
                                let action = map_key(
                                    key,
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
                            state.handle_paste(&text);
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
                    || state.accordion.as_ref().is_some_and(|a| a.is_active(crate::ui::wall_ms()))
                {
                    Duration::from_millis(15)
                } else if chat_in_progress(&state) {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(250)
                }) => {
                    // Periodic redraw / animation update
                    let now = std::time::Instant::now();
                    if !chat_in_progress(&state)
                        && now.duration_since(last_discovery) >= PROVIDER_REDISCOVERY_EVERY
                    {
                        last_discovery = now;
                        if let Some(tx) = state.bridge_tx.clone() {
                            spawn_discovery_task(tx);
                        }
                    }
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

            // Drain the ready backlog from the event channels before drawing,
            // so bursts of events cost one draw rather than one draw each.
            let mut drained = 0usize;
            while drained < MAX_EVENT_DRAIN_PER_PASS
                && let Ok(src_event) = mcp_rx.try_recv()
            {
                handle_source_event(src_event, &mut state);
                drained += 1;
            }
            while drained < MAX_EVENT_DRAIN_PER_PASS
                && let Ok(bridge_event) = bridge_rx.try_recv()
            {
                handle_bridge_event(bridge_event, &mut state);
                drained += 1;
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
    // One restore path, shared with the guard's `Drop` and its panic hook, so a
    // crash cannot leave the user in raw mode on the alternate screen with the
    // panic message written somewhere they cannot see (ahma_tui/SPEC.md §3).
    // Explicit here rather than left to the drop at end of scope so the terminal
    // is back to normal before anything else this function might print.
    terminal_guard.restore();

    loop_result
}

// ─── Action handler ───────────────────────────────────────────────────────────

fn handle_action(action: crate::keymap::Action, state: &mut crate::state::AppState) {
    use crate::keymap::Action;
    if handle_log_monitor_action(&action, state)
        || handle_picker_action(&action, state)
        || handle_navigation_action(&action, state)
        || handle_approval_action(&action, state)
        || handle_operation_action(&action, state)
        || handle_log_filter_action(&action, state)
        || handle_chat_action(&action, state)
        || handle_navigator_action(&action, state)
    {
        return;
    }

    match action {
        Action::Quit => request_quit(state),
        Action::Tab => {
            state.focus = state
                .focus
                .cycle_next_active(state.chat_open, state.log_window_open)
        }
        Action::BackTab => {
            state.focus = state
                .focus
                .cycle_prev_active(state.chat_open, state.log_window_open)
        }
        Action::ToggleHelp => state.toggle_help(),
        Action::ToggleChat => toggle_chat_pane(state),
        Action::FocusChat => {
            // Esc backs out one level: restore a zoomed pane first, then
            // return to the work view — which is where the TUI lives, and is
            // somewhere that always exists (the chat pane may be closed).
            if state.zoomed.is_some() {
                state.zoomed = None;
            } else if state.focus == crate::state::Focus::Work && state.chat_open {
                state.focus = crate::state::Focus::Chat;
            } else {
                state.focus = crate::state::Focus::Work;
            }
        }
        Action::Enter if state.focus == crate::state::Focus::Work => {
            if let Some(key) = selected_instance_header_key(state) {
                activate_window_chat(&key, state);
            } else {
                // Drill in: open the full-screen detail view for an operation
                // (or open the accordion section for an instance/session header).
                state.open_selected_tree_detail(crate::ui::wall_ms());
            }
        }
        Action::ToggleNode if state.focus == crate::state::Focus::Work => {
            // Space: inline accordion-expand the selected task into its
            // live/historic output view (or fold a header) without leaving
            // the tree.
            state.toggle_selected_tree_node(crate::ui::wall_ms());
        }
        Action::DetailClose => state.close_modal(),
        Action::ToggleProjectFilter if state.focus == crate::state::Focus::Work => {
            state.show_all_projects = !state.show_all_projects;
        }
        Action::Unknown | Action::Enter => {}
        _ => {}
    }
}

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
            // This auto-triggered logs_approve call bypasses needs_approval
            // entirely (spawn_tool_call_task below, not the agent loop's
            // resolve_tool_approval), so this field is never consulted here.
            non_mutating_tool_names: std::sync::Arc::new(
                ahma_core::agent::builtin_non_mutating_tool_names(),
            ),
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

/// Maximise/restore the focused pane. Enter on the log pane and `z` on any
/// zoomable pane both land here.
fn toggle_focused_pane_zoom(state: &mut crate::state::AppState) {
    if state.zoomed.is_some() {
        state.zoomed = None;
    } else if state.focus.is_zoomable() {
        state.zoomed = Some(state.focus);
    }
}

fn open_log_switcher(state: &mut crate::state::AppState) {
    state.open_log_files_modal(0);
    // Proactively request logs list refresh when modal is opened
    if let Some(ref tx) = state.mcp_source_tx {
        let _ = tx.try_send(crate::mcp_source::McpSourceCommand::RefreshLogs);
    }
}

fn close_log_switcher(state: &mut crate::state::AppState) {
    if state.log_files_selected().is_some() {
        state.close_modal();
    }
}

/// Move the log-switcher selection one row up or down, clamped to the modal's
/// range. Index `log_files.len()` is a valid selection (the trailing row), so
/// the forward bound is the length itself.
fn move_log_files_selection(state: &mut crate::state::AppState, forward: bool) {
    let Some(sel) = state.log_files_selected() else {
        return;
    };
    if forward {
        if sel < state.log_files.len() {
            state.set_log_files_selected(sel + 1);
        }
    } else if sel > 0 {
        state.set_log_files_selected(sel - 1);
    }
}

fn handle_log_monitor_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;
    // Thin dispatch table: every arm delegates, so adding an action never
    // deepens this function.
    match action {
        Action::ToggleWrap => {
            state.log_wrap_enabled = !state.log_wrap_enabled;
        }
        Action::ToggleZoom => toggle_focused_pane_zoom(state),
        Action::OpenLogSwitcher => open_log_switcher(state),
        Action::CloseLogSwitcher => close_log_switcher(state),
        Action::SubmitLogSwitcher => submit_log_switcher(state),
        // The guards matter: without them Up/Down would be swallowed here
        // instead of falling through to the other panes' handlers.
        Action::Up if state.log_files_selected().is_some() => {
            move_log_files_selection(state, false)
        }
        Action::Down if state.log_files_selected().is_some() => {
            move_log_files_selection(state, true)
        }
        Action::ApproveSymlink => approve_symlink(state),
        _ => return false,
    }
    true
}

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
        Action::Quit => request_quit(state),
        _ => {}
    }

    true
}

fn select_active_picker_prev(state: &mut crate::state::AppState) {
    if let Some(picker) = active_picker_mut(state) {
        picker.select_prev();
    }
}

fn select_active_picker_next(state: &mut crate::state::AppState) {
    if let Some(picker) = active_picker_mut(state) {
        picker.select_next();
    }
}

fn submit_active_picker(state: &mut crate::state::AppState) {
    if let Some(picker) = state.take_provider_picker() {
        submit_provider_picker(picker, state);
        return;
    }

    if let Some(picker) = state.take_model_picker() {
        submit_model_picker(picker, state);
        return;
    }

    if let Some(picker) = state.take_llm_setup_provider() {
        submit_llm_setup_provider(picker, state);
        return;
    }

    if let Some((picker, provider_name, base_url)) = state.take_llm_setup_model() {
        submit_llm_setup_model(picker, provider_name, base_url, state);
    }
}

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
    state.llm_selection = Some(crate::state::LlmSelection::named(name, old_model));

    if let Some(tx) = &state.bridge_tx {
        spawn_model_refresh(base_url, tx.clone());
        state.model_picker_requested = true;
    }

    save_session(state);
}

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
            state.llm_selection =
                Some(crate::state::LlmSelection::named(provider_name, model_name));
            save_session(state);
        }
    } else {
        // Keep whatever provider is already selected and swap only the model.
        if let Some(sel) = &mut state.llm_selection {
            sel.model = item.to_string();
        }
        save_session(state);
    }
}

fn close_active_pickers(state: &mut crate::state::AppState) {
    if matches!(
        state.modal,
        crate::state::ModalState::ProviderPicker(_)
            | crate::state::ModalState::ModelPicker(_)
            | crate::state::ModalState::LlmSetupProvider(_)
            | crate::state::ModalState::LlmSetupModel { .. }
    ) {
        state.close_modal();
    }
}

fn selected_instance_header_key(state: &crate::state::AppState) -> Option<String> {
    use crate::task_tree::RowKind;
    let rows = state.task_rows.borrow();
    rows.get(state.ops_selected).and_then(|r| match &r.kind {
        RowKind::Instance { group_id, .. } => Some(group_id.clone()),
        _ => None,
    })
}

fn activate_window_chat(key: &str, state: &mut crate::state::AppState) {
    let now = crate::ui::wall_ms();
    // A running turn streams into the transcript on screen; changing window
    // under it would pour one window's answer into another's conversation.
    if state.turn.is_some() && state.active_target_instance.as_deref() != Some(key) {
        set_footer_hint(
            state,
            "A reply is still coming — Esc cancels it, then switch windows",
        );
        return;
    }
    if state.open_section.as_deref() == Some(key)
        && (state.focus == crate::state::Focus::Chat || state.llm_selection.is_none())
    {
        state.toggle_section(key, now);
        state.chat_open = false;
        state.focus = crate::state::Focus::Work;
        state.active_target_instance = None;
        state.switch_transcript("");
        return;
    }

    state.open_section(key, now);
    state.active_target_instance = Some(key.to_string());
    state.switch_transcript(key);

    // Restore or assign default LLM for this window
    if let Some(win_cfg) = state.get_window_llm(key).cloned() {
        state.current_provider_url = win_cfg.provider_url;
        state.llm_selection = Some(crate::state::LlmSelection::named(
            win_cfg.provider,
            win_cfg.model,
        ));
    } else if let Some(sel) = &state.llm_selection {
        state.set_window_llm(
            key,
            crate::session_config::WindowLlmConfig {
                provider: sel.persistable_provider().to_string(),
                model: sel.model.clone(),
                provider_url: state.current_provider_url.clone(),
            },
        );
        save_session(state);
    }

    state.focus = crate::state::Focus::Work;
    // If still no LLM configured anywhere, start the setup wizard!
    if state.llm_selection.is_none() {
        start_llm_setup_wizard(state);
    } else {
        state.chat_open = true;
        state.focus = crate::state::Focus::Chat;
    }
}

const SETUP_OLLAMA: &str = "Ollama — local, free, private";
const SETUP_LM_STUDIO: &str = "LM Studio — local";
const SETUP_ANTHROPIC: &str = "Anthropic Claude — key from $ANTHROPIC_API_KEY";
const SETUP_OPENAI: &str = "OpenAI — key from $OPENAI_API_KEY";
const SETUP_OTHER: &str = "Another OpenAI-compatible server — see /provider add";
const SETUP_SAMPLING: &str = "Use the client's own model: ";

/// Step 1 of `/setup`: choose where the model runs.
///
/// Every choice is checked before it is saved: the provider's model list is
/// fetched (that *is* the connection test) and the picker in step 2 offers
/// what it actually has, rather than a hard-coded list of names that may not
/// exist there.
fn start_llm_setup_wizard(state: &mut crate::state::AppState) {
    use crate::state::PickerState;
    let mut ollama = SETUP_OLLAMA.to_string();
    if state
        .available_providers
        .iter()
        .any(|p| p.name.to_lowercase().contains("ollama"))
    {
        ollama.push_str(" · running");
    }
    let mut items = vec![
        ollama,
        SETUP_LM_STUDIO.to_string(),
        SETUP_ANTHROPIC.to_string(),
        SETUP_OPENAI.to_string(),
    ];
    for inst in &state.active_instances {
        if let Some(p) = virtual_provider_for_instance(inst) {
            items.push(format!("{SETUP_SAMPLING}{}", p.name));
        }
    }
    items.push(SETUP_OTHER.to_string());

    let picker = PickerState::new("Connect an LLM · step 1/2 — where does it run?", items);
    state.modal = crate::state::ModalState::LlmSetupProvider(picker);
}

fn submit_llm_setup_provider(
    picker: crate::state::PickerState,
    state: &mut crate::state::AppState,
) {
    use crate::state::{CloudSetup, SetupPending};
    use ahma_common::config::ProviderKind;
    let Some(item) = picker.selected_item().map(str::to_string) else {
        return;
    };
    let local = |name: &str, url: &str| SetupPending {
        provider_name: name.to_string(),
        base_url: url.to_string(),
        cloud: None,
        connected: false,
    };
    if item.starts_with(SETUP_OLLAMA) {
        // `/v1`: the client posts to `{base}/chat/completions`, which Ollama
        // serves under `/v1` only. The bare port 404'd every request.
        begin_setup_connect(state, local("Ollama", "http://localhost:11434/v1"), None);
    } else if item.starts_with(SETUP_LM_STUDIO) {
        begin_setup_connect(state, local("LM Studio", "http://localhost:1234/v1"), None);
    } else if item.starts_with(SETUP_ANTHROPIC) {
        let cloud = CloudSetup {
            config_name: "anthropic",
            kind: ProviderKind::Anthropic,
            key_env: "ANTHROPIC_API_KEY",
        };
        begin_cloud_setup(state, "Anthropic", "https://api.anthropic.com/v1", cloud);
    } else if item.starts_with(SETUP_OPENAI) {
        let cloud = CloudSetup {
            config_name: "openai",
            kind: ProviderKind::OpenAi,
            key_env: "OPENAI_API_KEY",
        };
        begin_cloud_setup(state, "OpenAI", "https://api.openai.com/v1", cloud);
    } else if item.starts_with(SETUP_OTHER) {
        push_assistant_message(
            state,
            "Register any OpenAI-compatible server with\n\
             `/provider add <name> <base_url> <model> - ${YOUR_KEY_VAR}`\n\
             (e.g. `/provider add groq https://api.groq.com/openai/v1 llama-3.3-70b - ${GROQ_API_KEY}`), \
             then choose it with /provider.",
        );
    } else if let Some(name) = item.strip_prefix(SETUP_SAMPLING) {
        let chosen = state
            .active_instances
            .iter()
            .find_map(|inst| virtual_provider_for_instance(inst).filter(|p| p.name == name));
        if let Some(p) = chosen {
            let model = p.models.first().cloned().unwrap_or_default();
            finish_llm_setup(p.name, model, Some(p.base_url), state);
        }
    }
}

/// A cloud provider needs its key in the environment first. The key is never
/// typed into the TUI or written anywhere: the provider is registered with a
/// `${VAR}` reference that ahma resolves when it calls the API.
fn begin_cloud_setup(
    state: &mut crate::state::AppState,
    name: &str,
    base_url: &str,
    cloud: crate::state::CloudSetup,
) {
    let key = std::env::var(cloud.key_env)
        .ok()
        .filter(|k| !k.trim().is_empty());
    let Some(key) = key else {
        push_assistant_message(
            state,
            format!(
                "{name} needs an API key in the environment: set `{var}` (e.g. \
                 `export {var}=…` in your shell profile), restart `ahma tui` from that \
                 shell, and run /setup again. The key is never typed here or stored — \
                 ahma reads it from `${var}` when it calls {name}.",
                var = cloud.key_env
            ),
        );
        return;
    };
    let pending = crate::state::SetupPending {
        provider_name: name.to_string(),
        base_url: base_url.to_string(),
        cloud: Some(cloud),
        connected: false,
    };
    begin_setup_connect(state, pending, Some(key));
}

/// Fetch the provider's model list; its arrival (or its absence) is the
/// connection test. See [`handle_setup_models`].
fn begin_setup_connect(
    state: &mut crate::state::AppState,
    pending: crate::state::SetupPending,
    api_key: Option<String>,
) {
    push_assistant_message(
        state,
        format!(
            "Connecting to {} at {}…",
            pending.provider_name, pending.base_url
        ),
    );
    if let Some(tx) = &state.bridge_tx {
        crate::llm_bridge::spawn_model_refresh_with_key(
            pending.base_url.clone(),
            api_key,
            tx.clone(),
        );
    }
    state.setup_pending = Some(pending);
}

/// The wizard's connection result: a model list opens step 2; nothing means
/// the provider is not reachable (or the key is wrong), and says what to fix.
fn handle_setup_models(models: Vec<String>, state: &mut crate::state::AppState) {
    let Some(mut pending) = state.setup_pending.take() else {
        return;
    };
    if models.is_empty() {
        let advice = match (&pending.cloud, pending.provider_name.as_str()) {
            (Some(cloud), name) => format!(
                "{name} did not return a model list. Check that `${}` holds a valid key, \
                 then /setup.",
                cloud.key_env
            ),
            (None, "Ollama") => "No models from Ollama. Is it running (`ollama serve`) with a \
                 model pulled (e.g. `ollama pull qwen2.5-coder`)? Then /setup."
                .to_string(),
            (None, name) => format!(
                "No models from {name} at {}. Start its server with a model loaded, then /setup.",
                pending.base_url
            ),
        };
        push_assistant_message(state, advice);
        return;
    }
    let picker = crate::state::PickerState::new(
        format!(
            "Connect an LLM · step 2/2 — {} model",
            pending.provider_name
        ),
        models,
    );
    state.modal = crate::state::ModalState::LlmSetupModel {
        picker,
        provider_name: pending.provider_name.clone(),
        base_url: pending.base_url.clone(),
    };
    pending.connected = true;
    state.setup_pending = Some(pending);
}

fn submit_llm_setup_model(
    picker: crate::state::PickerState,
    provider_name: String,
    base_url: String,
    state: &mut crate::state::AppState,
) {
    let Some(model) = picker.selected_item().map(|s| s.to_string()) else {
        return;
    };
    let pending = state.setup_pending.take();
    if let Some(cloud) = pending.and_then(|p| p.cloud).filter(|_| {
        ahma_common::config::AhmaConfig::load()
            .provider_for_base_url(&base_url)
            .is_none()
    }) {
        let entry = ahma_common::config::ProviderEntry {
            name: cloud.config_name.to_string(),
            kind: cloud.kind,
            base_url: base_url.clone(),
            default_model: model.clone(),
            api_key: Some(format!("${{{}}}", cloud.key_env)),
            num_ctx: None,
        };
        if let Err(e) = ahma_common::config::AhmaConfig::add_provider(entry) {
            push_assistant_message(
                state,
                format!("Could not register {provider_name} in ~/.ahma/config.toml: {e}"),
            );
        }
    }
    finish_llm_setup(provider_name, model, Some(base_url), state);
}

fn finish_llm_setup(
    provider_name: String,
    model: String,
    base_url: Option<String>,
    state: &mut crate::state::AppState,
) {
    state.current_provider_url = base_url.clone();
    let selection = crate::state::LlmSelection::named(provider_name.clone(), model.clone());
    state.llm_selection = Some(selection);

    if let Some(target) = state.active_target_instance.clone() {
        state.set_window_llm(
            &target,
            crate::session_config::WindowLlmConfig {
                provider: provider_name,
                model,
                provider_url: base_url,
            },
        );
    }
    save_session(state);
    state.close_modal();
    state.chat_open = true;
    state.focus = crate::state::Focus::Chat;
}

fn handle_navigation_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    // The full-screen detail overlays capture navigation while open.
    if matches!(
        state.modal,
        crate::state::ModalState::OperationDetail(_) | crate::state::ModalState::LogLineDetail(_)
    ) {
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

/// Scroll whichever full-screen detail overlay is open; the max is computed at
/// draw time and published through `detail_max_scroll`, which both overlays
/// share (only one can be open at a time).
fn scroll_detail_overlay(action: &crate::keymap::Action, state: &mut crate::state::AppState) {
    use crate::keymap::Action;
    use crate::state::ModalState;
    let max = state.detail_max_scroll.get();
    let scroll = match &mut state.modal {
        ModalState::OperationDetail(d) => &mut d.scroll,
        ModalState::LogLineDetail(d) => &mut d.scroll,
        _ => return,
    };
    match action {
        Action::Up => *scroll = scroll.saturating_sub(1),
        Action::Down => *scroll = (*scroll + 1).min(max),
        Action::Top => *scroll = 0,
        Action::Bottom => *scroll = max,
        _ => {}
    }
}

fn scroll_focus_up(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::Work => {
            state.ops_selected = state.ops_selected.saturating_sub(1);
            // A keyboard move should pull the view to the cursor; a wheel
            // scroll should not (see `scroll_work`).
            state.work_follow_selection.set(true);
        }
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
    }
}

fn scroll_focus_down(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::Work if state.ops_row_count() > 0 => {
            state.ops_selected = (state.ops_selected + 1).min(state.ops_row_count() - 1);
            state.work_follow_selection.set(true);
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

fn move_focus_to_top(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::Work => state.ops_selected = 0,
        Focus::Log => {
            state.log_follow = false;
            state.log_scroll = 0;
            state.sync_log_scroll_to_animation();
        }
        Focus::Chat => {
            state.chat_scroll = state.chat_max_scroll.get();
            state.sync_chat_scroll_to_animation();
        }
    }
}

fn move_focus_to_bottom(state: &mut crate::state::AppState) {
    use crate::state::Focus;

    match state.focus {
        Focus::Work => state.ops_selected = state.ops_row_count().saturating_sub(1),
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
    }
}

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
/// re-prompted), then approve this call. The grant is keyed by the workspace
/// the asking agent checks ([`crate::state::ApprovalGate::workspace`]), and
/// lives outside the sandbox in `~/.ahma/settings.toml` — see
/// [`ahma_core::approvals`].
fn resolve_approval_always(state: &mut crate::state::AppState) {
    use crate::state::{LogEntry, LogLevel};

    if let Some(gate) = state.approval.as_ref() {
        let tool = gate.tool.clone();
        let workspace = if gate.workspace.is_empty() {
            std::path::PathBuf::from(&state.workspace)
        } else {
            std::path::PathBuf::from(&gate.workspace)
        };
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

/// Ask again for the path a selected denied operation was refused (SPEC
/// R-PERM.7.1). This is the escape hatch a denial never had: the row that
/// records the refusal is the place you answer it from.
///
/// The question is re-raised *through the instance's own broker* rather than
/// faked locally, so the modal the user answers is the same one the automatic
/// flow raises, resolves through the same coordinator, and persists through the
/// same preview-and-approve path.
fn reraise_grant_for_selected_op(state: &mut crate::state::AppState) {
    use crate::state::{LogEntry, LogLevel, OpStatus};

    // Inside the detail overlay, act on the operation being viewed rather than
    // whatever the tree selection happens to be behind it — same rule as `c`.
    let op = match state.detail_op_key() {
        Some(key) => state.find_op(&key).cloned(),
        None => state.selected_op().cloned(),
    };
    let Some(op) = op else { return };

    let Some((path, access)) = op.denial.clone().filter(|_| op.status == OpStatus::Denied) else {
        state.push_log(LogEntry {
            timestamp: chrono::Local::now(),
            level: LogLevel::Info,
            message: "[a] asks for sandbox access — select a denied operation first.".to_string(),
        });
        return;
    };

    send_daemon_msg(ahma_common::daemon_hub::ClientMsg::ReRaiseScopeGrant {
        path: path.clone(),
        access,
        target_instance_id: op.instance_id.clone(),
    });
    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level: LogLevel::Info,
        message: format!("Asking again for {} access to {path}…", access.short()),
    });
}

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

fn resolve_approval(state: &mut crate::state::AppState, approved: bool) {
    use crate::state::{LogEntry, LogLevel};

    let Some(gate) = state.approval.take() else {
        return;
    };
    if let Some(turn) = state.turn.as_mut() {
        turn.enter(if approved {
            crate::state::TurnPhase::Tool {
                name: gate.tool.clone(),
            }
        } else {
            crate::state::TurnPhase::Reading
        });
    }

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

fn handle_operation_action(
    action: &crate::keymap::Action,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::keymap::Action;

    match action {
        Action::CancelOp => request_cancel_selected_op(state),
        Action::PinOp => toggle_selected_op_pin(state),
        Action::ReRaiseGrant => reraise_grant_for_selected_op(state),
        _ => return false,
    }

    true
}

fn request_cancel_selected_op(state: &mut crate::state::AppState) {
    // Inside the detail overlay `c` cancels the operation being viewed, not
    // whatever the tree selection happens to be behind it.
    let key = match state.detail_op_key() {
        Some(key) => key,
        None => match state.selected_op() {
            Some(op) => crate::state::OpKey::of(op),
            None => return,
        },
    };
    cancel_op(state, key);
}

/// Cancel one operation where it lives. An operation from another client is
/// cancelled on that client's instance through the daemon; the TUI's own MCP
/// session cannot see it, so the `cancel` tool there would miss. Without a
/// known instance the TUI's own session is the only place to ask.
fn cancel_op(state: &mut crate::state::AppState, key: crate::state::OpKey) {
    use crate::state::{LogEntry, LogLevel};
    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level: LogLevel::Info,
        message: format!("Cancel requested: {}", key.id),
    });
    match key.instance_id {
        Some(instance) => send_daemon_msg(ahma_common::daemon_hub::ClientMsg::CancelOperation {
            op_id: key.id,
            target_instance_id: Some(instance),
        }),
        None => {
            if let Some(tx) = &state.bridge_tx {
                let mcp_config = mcp_chat_config(state);
                crate::llm_bridge::spawn_tool_call_task(
                    "cancel".to_string(),
                    serde_json::json!({ "id": key.id }),
                    mcp_config,
                    tx.clone(),
                );
            }
        }
    }
}

fn toggle_selected_op_pin(state: &mut crate::state::AppState) {
    if let Some(idx) = state.selected_op_index()
        && let Some(op) = state.operations.get_mut(idx)
    {
        op.pinned = !op.pinned;
    }
}

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

fn insert_chat_character(c: char, state: &mut crate::state::AppState) {
    if c == '/' && state.chat_input_is_empty() {
        open_navigator(state);
    } else {
        state.chat_input.insert_char(c);
    }
}

fn backspace_chat_input(state: &mut crate::state::AppState) {
    state.chat_input.input(tui_textarea::Input {
        key: tui_textarea::Key::Backspace,
        ctrl: false,
        alt: false,
        shift: false,
    });
}

fn maybe_decompose_goal(
    text: &str,
    base_url: &str,
    model: &str,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::llm_bridge::spawn_decompose_task;
    use crate::state::{LogEntry, LogLevel};

    if let Some(stripped_goal) = text.strip_prefix('#') {
        let goal = stripped_goal.trim().to_string();
        if goal.is_empty() {
            return true;
        }
        if base_url.is_empty() {
            push_assistant_message(
                state,
                "No LLM configured. /setup connects one step by step (or /provider picks a known one).",
            );
            return true;
        }
        state.push_log(LogEntry {
            timestamp: chrono::Local::now(),
            level: LogLevel::Info,
            message: format!("Decomposing goal: {}", goal),
        });
        let client = crate::llm_bridge::build_configured_client(base_url, model);
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

    // The command is about to run outside the sandbox; say so to the daemon
    // before it starts, so the row exists in every view for as long as the
    // command does (SPEC R-DAEMON.9).
    let report = state.bang_report();
    if let Some(r) = &report {
        r.reporter.report(crate::tui_reporter::bang_started(
            &r.op_id,
            &cmd_str,
            &working_dir,
        ));
    }

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
        op_id: report.as_ref().map(|r| r.op_id.clone()),
    };

    state.windows.push(w);
    if state.windows.len() > 100 {
        state.windows.remove(0);
    }

    if let Some(tx) = &state.bridge_tx {
        spawn_window_cli_task(win_id, cmd_str, working_dir, abort_rx, tx.clone(), report);
    }
}

fn submit_chat_input(state: &mut crate::state::AppState) {
    use crate::state::ChatEntry;

    let text = state.chat_input_text().trim().to_string();
    if text.is_empty() {
        return;
    }
    state.clear_chat_input();
    state.remember_input(&text);

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
        push_assistant_message(
            state,
            "No LLM configured. /setup connects one step by step (or /provider picks a known one).",
        );
        return;
    }

    state.turn_retries = 0;
    state.chat.push(ChatEntry::User {
        text,
        payload: None,
        started_at: Some(std::time::Instant::now()),
        duration_ms: None,
    });
    state.chat.push(ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

    send_chat_turn(state, base_url, model);
}

/// Collect the current conversation and submit it to the daemon LLM loop.
fn send_chat_turn(state: &mut crate::state::AppState, base_url: String, model: String) {
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

    let target_instance_id = state
        .active_target_instance
        .as_ref()
        .filter(|key| state.active_instances.iter().any(|i| &i.id == *key))
        .cloned();

    state.turn = Some(crate::state::ChatTurn::new(target_instance_id.clone()));
    send_turn_msg(
        state,
        ahma_common::daemon_hub::ClientMsg::SubmitPrompt {
            messages,
            system_prompt,
            provider: Some(base_url),
            model: Some(model),
            target_instance_id,
        },
    );
}

/// Send a message a chat turn depends on. Unlike [`send_daemon_msg`], a
/// failure is not swallowed: the turn is ended with the reason, because a
/// prompt that never reached the daemon otherwise spins forever.
fn send_turn_msg(state: &crate::state::AppState, msg: ahma_common::daemon_hub::ClientMsg) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("send_turn_msg: no active tokio runtime, skipping message: {msg:?}");
        return;
    };
    let failed_tx = state.bridge_tx.clone();
    handle.spawn(async move {
        let sent = match ahma_common::daemon_hub::connect_to_daemon().await {
            Ok(mut stream) => ahma_common::daemon_hub::send_msg(&mut stream, &msg).await,
            Err(e) => Err(e),
        };
        if let Err(e) = sent
            && let Some(tx) = failed_tx
        {
            let _ = tx
                .send(crate::llm_bridge::BridgeEvent::TurnSendFailed(format!(
                    "Could not reach the ahma daemon: {e}"
                )))
                .await;
        }
    });
}

/// The turn finished normally: stop every live indicator.
fn end_turn(state: &mut crate::state::AppState) {
    state.turn = None;
    state.reset_liveness();
    state.chat.finish_stream();
    state.chat.finish_user_timing();
}

fn end_turn_with_error(state: &mut crate::state::AppState, error: &str) {
    let transient = is_transient_turn_error(error);
    // Retry by ourselves only when it is safe to: the failure looks like the
    // connection, not the request, and no answer text has arrived yet (a
    // half-written reply would otherwise be sent back to the model as if it
    // had said it). Once per message — a second failure is reported.
    let retry = transient
        && state.turn_retries == 0
        && state.turn.as_ref().is_some_and(|t| t.streamed_chars == 0);
    end_turn(state);
    state.chat_scroll = 0;
    if retry {
        let (base_url, model) = parse_llm_selection(state);
        if !base_url.is_empty() {
            state.turn_retries += 1;
            state.chat.push(crate::state::ChatEntry::Notice {
                text: format!(
                    "{} dropped the request ({}) — trying again now",
                    crate::ui::shorten_llm_label(&state.llm_label()),
                    first_line(error)
                ),
            });
            state.chat.push(crate::state::ChatEntry::Assistant {
                content: String::new(),
                streaming: true,
            });
            send_chat_turn(state, base_url, model);
            return;
        }
    }
    push_assistant_message(state, format!("Error: {error}"));
    if transient {
        state.chat.push(crate::state::ChatEntry::Notice {
            text: "The model could not be reached. Your message is still here: send it again \
                   when ready, or /model to pick another."
                .to_string(),
        });
    }
}

/// A dim one-line account of a slow turn — how long, how much the model read,
/// how fast it wrote — so choosing a different model or a smaller context is
/// an informed decision. Quick turns get none: the numbers only matter when
/// the wait did.
fn turn_summary(state: &crate::state::AppState) -> Option<String> {
    let turn = state.turn.as_ref()?;
    let now = std::time::Instant::now();
    let took = now.duration_since(turn.started);
    if took < std::time::Duration::from_secs(10) {
        return None;
    }
    let label = state.llm_label();
    let mut parts = vec![
        crate::ui::shorten_llm_label(&label),
        format!("took {}", crate::ui::format_elapsed_short(took)),
    ];
    let read = state
        .window_usage
        .get(&crate::state::AppState::usage_key(
            turn.target_instance.as_deref(),
        ))
        .map_or(0, |u| u.last_prompt_tokens);
    if read > 0 {
        let rate = state
            .read_rates
            .get(&label)
            .map(|r| format!(" at {} tok/s", r.round()))
            .unwrap_or_default();
        parts.push(format!("read {}k tokens{rate}", read.div_ceil(1000)));
    }
    if let Some(rate) = turn.tokens_per_sec(now) {
        parts.push(format!("wrote {rate} tok/s"));
    }
    Some(parts.join(" · "))
}

/// Whether a turn error is the connection's fault (worth one quiet retry)
/// rather than the request's (a 4xx, a refused tool, a cancel).
fn is_transient_turn_error(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    if e.contains("cancel") {
        return false;
    }
    [
        "timed out",
        "timeout",
        "connection",
        "error sending request",
        "broken pipe",
        "unexpected eof",
        "reset by peer",
        "502",
        "503",
        "504",
        "overloaded",
    ]
    .iter()
    .any(|needle| e.contains(needle))
}

/// The first line of a (possibly multi-line) error, for a one-line notice.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// Stop the running turn, if any. The turn ends here at once rather than on
/// the instance's reply, so Esc works even when the daemon is gone; the
/// instance's own "cancelled" error then finds no turn and is ignored.
fn cancel_turn(state: &mut crate::state::AppState) -> bool {
    let Some(turn) = state.turn.take() else {
        return false;
    };
    send_daemon_msg(ahma_common::daemon_hub::ClientMsg::CancelPrompt {
        target_instance_id: turn.target_instance,
    });
    end_turn(state);
    push_assistant_message(state, "Cancelled.");
    state.chat_scroll = 0;
    true
}

/// Operations and `!` windows still running in this TUI's view.
fn running_work_count(state: &crate::state::AppState) -> usize {
    use crate::state::{OpStatus, WindowStatus};
    let ops = state
        .operations
        .iter()
        .filter(|op| matches!(op.status, OpStatus::Running | OpStatus::Pending))
        .count();
    let windows = state
        .windows
        .iter()
        .filter(|w| matches!(w.status, WindowStatus::Running | WindowStatus::Pending))
        .count();
    ops + windows
}

fn set_footer_hint(state: &mut crate::state::AppState, hint: impl Into<String>) {
    state.footer_hint = Some((hint.into(), std::time::Instant::now()));
}

fn quit_is_armed(state: &crate::state::AppState) -> bool {
    state
        .quit_armed_at
        .is_some_and(|t| t.elapsed() < crate::state::CTRL_C_QUIT_WINDOW)
}

/// `q` / `/quit`: quit at once when nothing is running, otherwise warn and
/// quit on a second request inside [`crate::state::CTRL_C_QUIT_WINDOW`].
fn request_quit(state: &mut crate::state::AppState) {
    let busy = running_work_count(state) + usize::from(state.turn.is_some());
    if busy == 0 || quit_is_armed(state) {
        state.should_quit = true;
        return;
    }
    state.quit_armed_at = Some(std::time::Instant::now());
    set_footer_hint(state, format!("{busy} still running — press again to quit"));
}

/// Ctrl-C: the first press cancels a running turn; otherwise, or pressed
/// again, it quits (asking first while work is running, like `q`).
fn interrupt(state: &mut crate::state::AppState) {
    if cancel_turn(state) {
        state.quit_armed_at = Some(std::time::Instant::now());
        set_footer_hint(state, "Turn cancelled — Ctrl-C again to quit");
        return;
    }
    request_quit(state);
}

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
                OpStatus::Denied => "DENIED (sandbox)",
                OpStatus::Interrupted => "interrupted (outcome unknown)",
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
/// Summary of discovered skills for system prompt awareness.
fn format_skills_summary(workspace: &str) -> String {
    if workspace.is_empty() {
        return String::new();
    }
    let set = ahma_common::skills::discover_skills(std::path::Path::new(workspace));
    let user_invocable: Vec<_> = set.skills.iter().filter(|s| s.user_invocable).collect();
    if user_invocable.is_empty() {
        return String::new();
    }
    let mut out = String::from("Available Agent Skills (invoke with /<name> [args]):\n");
    for s in user_invocable {
        out.push_str(&format!("  - /{}: {}\n", s.name, s.description));
    }
    out
}

/// Active Agent Skills context in effect for multi-turn sessions (SPEC R-SK8.4).
fn format_active_skills(skills: &[ahma_common::skills::Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nActive Agent Skills in effect:\n");
    for s in skills {
        out.push_str(&format!(
            "<active_skill name=\"{}\" path=\"{}\" directory=\"{}\">\n{}\n</active_skill>\n",
            s.name,
            s.path.display(),
            s.root_dir().display(),
            s.body.trim()
        ));
    }
    out
}

/// The pieces of a system prompt, assembled by a [`PromptComposer`]. Keeping
/// them separate lets a composer decide which to include for token economy.
struct PromptParts {
    /// Optional per-profile prompt override.
    profile_prompt: String,
    /// The agentic base (user-editable agent prompt, or the concise fallback).
    base: String,
    /// `"Workspace: …\n"` or empty.
    workspace_line: String,
    /// `"Sandbox: …\n"` or empty.
    sandbox_line: String,
    /// Discovered Agent Skills summary.
    skills_summary: String,
    /// Active Agent Skills in effect across turns (SPEC R-SK8.4).
    active_skills: String,
    /// Recent operations block (token-heavy).
    recent_ops: String,
    /// Recent failures block incl. stdout tails (token-heavy).
    recent_failures: String,
}

/// Strategy for assembling the agent system prompt. Swap implementations to
/// trade prompt richness for token economy — the seam behind `/minimize`.
trait PromptComposer {
    fn compose(&self, parts: &PromptParts) -> String;
}

/// Join `profile` + `base` + the live-context `ctx` in the canonical layout.
fn assemble_prompt(profile: &str, base: &str, ctx: &str) -> String {
    match (profile.is_empty(), ctx.is_empty()) {
        (true, true) => base.to_string(),
        (true, false) => format!("{base}\n\n--- Live context ---\n{ctx}"),
        (false, true) => format!("{profile}\n\n{base}"),
        (false, false) => format!("{profile}\n\n{base}\n\n--- Live context ---\n{ctx}"),
    }
}

/// The default composer: agentic base + profile + the full live-context block.
struct FullComposer;

impl PromptComposer for FullComposer {
    fn compose(&self, p: &PromptParts) -> String {
        let ctx = format!(
            "{}{}{}{}{}{}",
            p.workspace_line,
            p.sandbox_line,
            p.skills_summary,
            p.active_skills,
            p.recent_ops,
            p.recent_failures
        );
        assemble_prompt(&p.profile_prompt, &p.base, &ctx)
    }
}

/// The lean composer used under `/minimize`: drops the token-heavy recent-ops
/// and recent-failures blocks, keeping the base, profile, workspace, sandbox, and skills.
struct MinimalComposer;

impl PromptComposer for MinimalComposer {
    fn compose(&self, p: &PromptParts) -> String {
        let ctx = format!(
            "{}{}{}{}",
            p.workspace_line, p.sandbox_line, p.skills_summary, p.active_skills
        );
        assemble_prompt(&p.profile_prompt, &p.base, &ctx)
    }
}

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
        sandbox_line: if state.sandbox_status.is_known() {
            format!("Sandbox: {}\n", state.sandbox_status.label())
        } else {
            String::new()
        },
        skills_summary: format_skills_summary(&state.workspace),
        active_skills: format_active_skills(&state.active_skills),
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

fn collect_chat_history(state: &crate::state::AppState) -> Vec<ahma_llm_monitor::ChatMessage> {
    use crate::state::ChatEntry;
    use ahma_llm_monitor::ChatMessage;

    state
        .chat
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            // A `/skill` invocation displays the typed command but sends the
            // full skill instructions (SPEC R-SK8): prefer the payload.
            ChatEntry::User { text, payload, .. } => Some(ChatMessage::user(
                payload.clone().unwrap_or_else(|| text.clone()),
            )),
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

/// Resolve token/context preferences: CLI flag > settings.
/// Returns `(minimize_tokens, small_model_harness, context_length)`.
///
/// `AHMA_MINIMIZE_TOKENS` and `AHMA_SMALL_MODEL_HARNESS` are **retired** (R-CFG1.2)
/// and are warn-and-ignored, not read. `ahma_mcp`'s CLI already warned-and-ignored
/// them while this function still honored them, so the same variable meant two
/// different things in two binaries of the same product: setting it changed the TUI's
/// behaviour but not the server's. One variable, one verdict — the replacements are
/// `--minimize-tokens` / `--small-model-harness` (both already reflected in
/// `state.token_prefs`) and the matching `[tools]` settings keys.
fn resolve_token_prefs(state: &crate::state::AppState) -> (bool, bool, Option<u32>) {
    let settings = ahma_common::config::AhmaSettings::load();

    ahma_mcp::warn_retired_env("AHMA_MINIMIZE_TOKENS");
    ahma_mcp::warn_retired_env("AHMA_SMALL_MODEL_HARNESS");

    let minimize_tokens = state
        .token_prefs
        .minimize_tokens
        .unwrap_or(settings.tools.minimize_tokens);
    let small_model_harness = state
        .token_prefs
        .small_model_harness
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
fn provider_num_ctx(base_url: &str) -> Option<u32> {
    ahma_common::config::AhmaConfig::load().num_ctx_for_base_url(base_url)
}

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
        // The TUI connects to the bridge over HTTP with no in-process
        // AhmaMcpService, so it cannot resolve a remote custom tool's MTDF
        // `mutates` field — only the static builtin classification is known
        // here. An unrecognized configured tool therefore falls back to the
        // fail-closed default (mutating) exactly as intended, not a bug.
        non_mutating_tool_names: std::sync::Arc::new(
            ahma_core::agent::builtin_non_mutating_tool_names(),
        ),
    }
}

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
        Action::NavSubmit => submit_navigator_command(state),
        Action::NavChar(c) => mutate_navigator(state, |nav| nav.input.push(*c), true),
        Action::NavBackspace => mutate_navigator(
            state,
            |nav| {
                nav.input.pop();
            },
            true,
        ),
        Action::NavComplete => mutate_navigator(state, |nav| nav.tab_complete(), false),
        Action::NavUp => mutate_navigator(state, |nav| nav.select_prev(), false),
        Action::NavDown => mutate_navigator(state, |nav| nav.select_next(), false),
        _ => return false,
    }

    true
}

fn mutate_navigator(
    state: &mut crate::state::AppState,
    f: impl FnOnce(&mut crate::state::CommandNavigator),
    refresh_completions: bool,
) {
    if let Some(nav) = state.navigator_mut() {
        f(nav);
    }
    if refresh_completions {
        refresh_navigator_completions(state);
    }
}

fn open_navigator(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    let skills = skill_nav_commands(state);
    state.modal =
        crate::state::ModalState::Navigator(crate::state::CommandNavigator::opened(&tools, skills));
}

fn refresh_navigator_completions(state: &mut crate::state::AppState) {
    let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
    if let Some(nav) = state.navigator_mut() {
        nav.refresh_completions(&tools);
    }
}

fn submit_navigator_command(state: &mut crate::state::AppState) {
    let Some(cmd) = state.navigator().map(|n| n.selected_command()) else {
        return;
    };
    state.close_modal();
    dispatch_nav_command(&cmd, state);
}

fn handle_settings_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crossterm::event::KeyCode;

    if !state.settings_editor.open {
        return false;
    }

    // Ctrl-C quits from everywhere, including here. This handler runs first in
    // the dispatch chain and used to return `true` for every key, so the
    // keymap's "Ctrl-C always quits" was false while the panel was open.
    if key.code == KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
    {
        interrupt(state);
        return true;
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

fn handle_help_key(key: crossterm::event::KeyEvent, state: &mut crate::state::AppState) -> bool {
    use crossterm::event::KeyCode;

    if !state.is_help_open() {
        return false;
    }

    match (key.code, key.modifiers) {
        // Ctrl-C quits from the help overlay too — it swallowed every key,
        // which made the documented "Ctrl-C always quits" untrue exactly where
        // a stuck user is most likely to try it.
        (KeyCode::Char('c'), m) if m.contains(crossterm::event::KeyModifiers::CONTROL) => {
            interrupt(state);
            true
        }
        // `q` closes help, matching every other overlay in the app.
        (KeyCode::Esc, _) | (KeyCode::Char('?'), _) | (KeyCode::Char('q'), _) => {
            state.help_scroll = 0;
            state.close_modal();
            true
        }
        // The overlay is taller than the screen it is capped to, so it scrolls.
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            state.help_scroll = state.help_scroll.saturating_add(1);
            true
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            state.help_scroll = state.help_scroll.saturating_sub(1);
            true
        }
        (KeyCode::PageDown, _) | (KeyCode::Char(' '), _) => {
            state.help_scroll = state.help_scroll.saturating_add(10);
            true
        }
        (KeyCode::PageUp, _) => {
            state.help_scroll = state.help_scroll.saturating_sub(10);
            true
        }
        (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
            state.help_scroll = 0;
            true
        }
        _ => true,
    }
}

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

fn active_picker_mut(state: &mut crate::state::AppState) -> Option<&mut crate::state::PickerState> {
    match &mut state.modal {
        crate::state::ModalState::ProviderPicker(p)
        | crate::state::ModalState::ModelPicker(p)
        | crate::state::ModalState::LlmSetupProvider(p)
        | crate::state::ModalState::LlmSetupModel { picker: p, .. } => Some(p),
        _ => None,
    }
}

/// When an approval is pending, a bare `y` / `n` resolves it immediately — no
/// matter which panel has focus. Without this, the chat input box swallows the
/// keystroke as typed text (the bug where pressing "y" just sent "y" as a
/// message), forcing the user to Tab away before the global keymap saw it.
///
/// We deliberately bail when a text-entry overlay is active (navigator,
/// pickers, log filter) so the user can still type a `y`/`n` there.
fn handle_approval_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.approval.is_none()
        || state.text_entry_modal_open()
        || state.log_filter_active
        || state.typing_in_chat()
    {
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

/// Raise the one-time "trust this folder?" question (SPEC R-PERM.1.3) when the
/// workspace is new to ahma. Not offered for a folder that is already trusted,
/// or for one too broad ever to trust (home, a root — see
/// [`ahma_core::approvals::trust_allowed`]); there the per-tool questions stay.
fn offer_folder_trust(state: &mut crate::state::AppState) {
    let workspace = std::path::Path::new(&state.workspace);
    if state.workspace.is_empty()
        || ahma_core::approvals::is_workspace_trusted(workspace)
        || !ahma_core::approvals::trust_allowed(
            workspace,
            ahma_common::config::ahma_home_dir().as_deref(),
        )
    {
        return;
    }
    state.trust_prompt = Some(state.workspace.clone());
}

/// Keys for the trust question. **Enter-safe** like every gate: only `y`
/// trusts; `n`, Enter and Esc keep asking per tool.
fn handle_trust_key(key: crossterm::event::KeyEvent, state: &mut crate::state::AppState) -> bool {
    use crate::state::{LogEntry, LogLevel};
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.trust_prompt.is_none()
        || state.text_entry_modal_open()
        || state.log_filter_active
        || state.typing_in_chat()
    {
        return false;
    }
    let trust = match (key.code, key.modifiers) {
        (KeyCode::Char('y'), KeyModifiers::NONE) => true,
        (KeyCode::Char('n'), KeyModifiers::NONE) | (KeyCode::Esc, _) | (KeyCode::Enter, _) => false,
        _ => return false,
    };
    let Some(folder) = state.trust_prompt.take() else {
        return false;
    };
    let (level, message) = if !trust {
        (
            LogLevel::Info,
            format!("Not trusting {folder}: ahma will ask before each tool that changes things"),
        )
    } else {
        match ahma_core::approvals::trust_workspace(std::path::Path::new(&folder)) {
            Ok(()) => (
                LogLevel::Info,
                format!(
                    "Trusted {folder}: tools run here without asking. Anything outside it, \
                     network access and settings changes still ask. \
                     Undo: ahma permissions revoke tool '*' --workspace {folder}"
                ),
            ),
            Err(e) => (LogLevel::Warn, format!("Could not trust {folder}: {e}")),
        }
    };
    set_footer_hint(state, message.clone());
    state.push_log(LogEntry {
        timestamp: chrono::Local::now(),
        level,
        message,
    });
    true
}

/// Keys for the scope-grant modal. Three-valued and **Enter-safe**: Enter / Esc /
/// `n` deny (the default), `r` grants read-only, `y` grants read+write. Widening
/// always requires an explicit non-default key (SPEC R5.3.1).
fn handle_scope_grant_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use ahma_common::scope_grant::GrantDecision;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.scope_grant.is_none()
        || state.text_entry_modal_open()
        || state.log_filter_active
        || state.typing_in_chat()
    {
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

fn handle_web_approval_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use ahma_common::web_approval::WebApprovalDecision;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.web_approval.is_none()
        || state.text_entry_modal_open()
        || state.log_filter_active
        || state.typing_in_chat()
    {
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
        // SPEC R-WEB.5 defines three tiers. `AllowOnce` existed in the decision
        // enum and had a log branch here, but no key ever produced it — one of
        // the three specified tiers was unreachable, so "I'm not sure yet" had
        // no answer short of allowing the whole session.
        (KeyCode::Char('o'), KeyModifiers::NONE) => {
            resolve_web_approval(state, WebApprovalDecision::AllowOnce);
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

fn handle_chat_input_key(
    key: crossterm::event::KeyEvent,
    state: &mut crate::state::AppState,
) -> bool {
    use crate::state::Focus;
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.focus != Focus::Chat
        || state.text_entry_modal_open()
        || state.log_filter_active
        || matches!(
            state.modal,
            crate::state::ModalState::OperationDetail(_)
                | crate::state::ModalState::LogLineDetail(_)
        )
    {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            if state.chat_input_is_empty() {
                state.chat_input.insert_str("/run ");
                let tools: Vec<String> = state.tools_list.iter().map(|t| t.name.clone()).collect();
                let skills = skill_nav_commands(state);
                let mut nav = crate::state::CommandNavigator::opened(&tools, skills);
                nav.input = "run ".to_string();
                nav.refresh_completions(&tools);
                state.modal = crate::state::ModalState::Navigator(nav);
            }
            true
        }
        // Recall earlier input from the first line, later from the last;
        // anywhere else the arrows move the cursor.
        (KeyCode::Up, KeyModifiers::NONE)
            if state.chat_input.cursor().0 == 0 && state.recall_previous_input() =>
        {
            true
        }
        (KeyCode::Down, KeyModifiers::NONE)
            if state.chat_input.cursor().0 + 1 >= state.chat_input.lines().len()
                && state.recall_next_input() =>
        {
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
            if !state.chat_input_is_empty() {
                state.clear_chat_input();
            } else if !cancel_turn(state) {
                state.focus = crate::state::Focus::Work;
            }
            true
        }
        // `?` on an empty input opens help, the same way `/` opens the
        // navigator. Without this the help overlay was unreachable from the
        // default screen state: both sub-windows default closed, so focus is
        // Chat and the global `?` binding never fires — pressing `?` just
        // typed a literal question mark while the help overlay's own first
        // line claimed "? Toggle this help".
        (KeyCode::Char('?'), KeyModifiers::NONE | KeyModifiers::SHIFT)
            if state.chat_input_is_empty() =>
        {
            state.modal = crate::state::ModalState::Help;
            true
        }
        (KeyCode::Char('/'), KeyModifiers::NONE) if state.chat_input_is_empty() => {
            open_navigator(state);
            true
        }
        _ => state.chat_input.input(textarea_input_from_key_event(key)),
    }
}

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

fn handle_window_nav_commands(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd == "/exit" || cmd == "/quit" {
        request_quit(state);
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
fn dispatch_nav_command(cmd: &str, state: &mut crate::state::AppState) {
    let cmd = cmd.trim();
    if handle_window_nav_commands(cmd, state)
        || handle_basic_nav_command(cmd, state)
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
        || handle_log_file_nav_command(cmd, state)
        || handle_analyze_nav_command(cmd, state)
        || handle_skills_nav_command(cmd, state)
    {
        return;
    }

    push_assistant_message(
        state,
        format!(
            "Unknown command `{cmd}`. Use /help for the built-in commands or /skills for the Agent Skills you can invoke with /<name>."
        ),
    );
}

/// `/skills` (and bare `/skill`) lists discovered Agent Skills; `/skill <name>
/// [args]` or plain `/<name> [args]` invokes one (SPEC R-SK8). Runs after
/// every built-in handler so built-in commands always shadow same-named
/// skills.
fn handle_skills_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    let (head, rest) = split_first_token(cmd.trim_start_matches('/'));

    match head {
        "skills" => {
            list_skills(state);
            true
        }
        "skill" => {
            handle_explicit_skill_command(rest, cmd, state);
            true
        }
        name => handle_implicit_skill_command(name, rest, cmd, state),
    }
}

/// `/skill [name] [args]`: a missing skill is reported, not treated as an
/// unknown command. Split out of `handle_skills_nav_command` so that arm
/// reads as one call.
fn handle_explicit_skill_command(rest: &str, cmd: &str, state: &mut crate::state::AppState) {
    let (name, args) = split_first_token(rest);
    if name.is_empty() {
        list_skills(state);
    } else if let Some(skill) = find_user_skill(state, name) {
        invoke_skill(state, &skill, args, cmd);
    } else {
        push_assistant_message(
            state,
            format!("No Agent Skill named `{name}`. Use /skills to list what is available."),
        );
    }
}

/// Implicit form `/<name> [args]`: only names that are valid per the Agent
/// Skills spec and actually resolve to a skill are consumed; everything else
/// falls through to the unknown-command message (`false`). Split out of
/// `handle_skills_nav_command` so that arm reads as one call.
fn handle_implicit_skill_command(
    name: &str,
    rest: &str,
    cmd: &str,
    state: &mut crate::state::AppState,
) -> bool {
    if ahma_common::skills::validate_name(name).is_err() {
        return false;
    }
    let workspace = std::path::Path::new(&state.workspace);
    let set = ahma_common::skills::discover_skills(workspace);
    if let Some(err) = set.invalid.iter().find(|e| {
        e.path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == name)
    }) {
        push_assistant_message(
            state,
            format!("Skill `{name}` could not be loaded: {}", err.reason),
        );
        return true;
    }
    let Some(skill) = find_user_skill(state, name) else {
        return false;
    };
    invoke_skill(state, &skill, rest, cmd);
    true
}

/// Split `s` into its first whitespace-delimited token and the trimmed rest.
fn split_first_token(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (s, ""),
    }
}

/// Look up a user-invocable skill by name from the standard discovery roots.
fn find_user_skill(
    state: &crate::state::AppState,
    name: &str,
) -> Option<ahma_common::skills::Skill> {
    ahma_common::skills::discover_skills(std::path::Path::new(&state.workspace))
        .get(name)
        .filter(|s| s.user_invocable)
        .cloned()
}

/// Post the `/skills` listing into the chat, disclosing skipped skill
/// directories rather than hiding them.
fn list_skills(state: &mut crate::state::AppState) {
    let workspace = std::path::Path::new(&state.workspace);
    let set = ahma_common::skills::discover_skills(workspace);

    let mut msg = if set.skills.is_empty() {
        format_searched_roots(workspace)
    } else {
        format_discovered_skills(&set.skills)
    };
    for e in &set.invalid {
        msg.push_str(&format!("\n⚠ Skipped {}: {}", e.path.display(), e.reason));
    }

    push_assistant_message(state, msg.trim_end().to_string());
}

/// The "nothing found" body: name every directory that was searched, so an
/// empty listing is diagnosable rather than mysterious.
fn format_searched_roots(workspace: &std::path::Path) -> String {
    let mut msg = String::from("No Agent Skills found. Searched:\n");
    for root in ahma_common::skills::skill_roots(workspace) {
        msg.push_str(&format!("- {}\n", root.display()));
    }
    msg
}

fn format_discovered_skills(skills: &[ahma_common::skills::Skill]) -> String {
    let mut msg = String::from("Available Agent Skills — invoke with `/<name> [args]`:\n");
    for s in skills {
        let note = if s.user_invocable {
            ""
        } else {
            " (not user-invocable)"
        };
        msg.push_str(&format!("- **/{}**{note} — {}\n", s.name, s.description));
    }
    msg
}

/// Inject the skill's SKILL.md instructions as the LLM payload for this turn
/// while the pane displays the typed command (SPEC R-SK8).
fn invoke_skill(
    state: &mut crate::state::AppState,
    skill: &ahma_common::skills::Skill,
    args: &str,
    typed: &str,
) {
    let (base_url, model) = parse_llm_selection(state);
    if base_url.is_empty() {
        push_assistant_message(
            state,
            "No LLM configured. /setup connects one step by step (or /provider picks a known one).",
        );
        return;
    }

    if !state.active_skills.iter().any(|s| s.name == skill.name) {
        state.active_skills.push(skill.clone());
    }

    state.push_log(crate::state::LogEntry {
        timestamp: chrono::Local::now(),
        level: crate::state::LogLevel::Info,
        message: format!(
            "Skill '{}' loaded from {} ({} lines)",
            skill.name,
            skill.path.display(),
            skill.body.lines().count()
        ),
    });

    state.chat.push(crate::state::ChatEntry::User {
        text: typed.to_string(),
        payload: Some(compose_skill_prompt(skill, args)),
        started_at: Some(std::time::Instant::now()),
        duration_ms: None,
    });
    state.chat.push(crate::state::ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

    send_chat_turn(state, base_url, model);
}

/// The message the LLM receives for a skill invocation: the full SKILL.md
/// instruction body plus file path metadata and the user's arguments.
fn compose_skill_prompt(skill: &ahma_common::skills::Skill, args: &str) -> String {
    let name = &skill.name;
    let body = skill.body.trim();
    let path = skill.path.display();
    let dir = skill.root_dir().display();
    let mut prompt = format!(
        "<skill name=\"{name}\" path=\"{path}\" directory=\"{dir}\">\n{body}\n</skill>\n\nThe user invoked the \"{name}\" Agent Skill"
    );
    if args.is_empty() {
        prompt.push_str(". Follow the skill instructions above.");
    } else {
        prompt.push_str(&format!(
            " with arguments: {args}\nFollow the skill instructions above, applying them to these arguments."
        ));
    }
    prompt
}

/// `/name` navigator entries for the user-invocable Agent Skills discovered
/// from the standard roots.
fn skill_nav_commands(state: &crate::state::AppState) -> Vec<crate::state::NavCommand> {
    ahma_common::skills::discover_skills(std::path::Path::new(&state.workspace))
        .skills
        .iter()
        .filter(|s| s.user_invocable)
        .map(|s| crate::state::NavCommand {
            command: format!("/{}", s.name),
            description: format!("Agent Skill — {}", s.description),
        })
        .collect()
}

/// `/minimize [on|off]` — toggle token minimization (concise prompting + output
/// compression for small models). With no argument it reports the current state.
/// The choice is applied live and persisted to `settings.tools.minimize_tokens`
/// so the daemon agent loop (which reads settings) and the next session both
/// honour it. Default is off.
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
fn set_minimize_tokens(state: &mut crate::state::AppState, desired: bool) {
    state.minimize_tokens = desired;
    state.token_prefs.minimize_tokens = Some(desired);

    let saved = ahma_common::config::AhmaSettings::update(|s| s.tools.minimize_tokens = desired);
    let status = if desired { "on" } else { "off" };
    match saved {
        Ok(_) => push_assistant_message(
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

/// Switch `tools.execution_mode` and persist it to `~/.ahma/settings.toml`
/// (re-read first, so an edit made elsewhere is not clobbered).
///
/// A running ahma worker read its mode when it started, so the message says
/// plainly which sessions the change reaches rather than implying it applied
/// everywhere at once.
fn set_execution_mode(
    state: &mut crate::state::AppState,
    mode: ahma_common::config::ExecutionPolicy,
) {
    match ahma_common::config::AhmaSettings::update(|s| s.tools.execution_mode = mode) {
        Ok(_) => {
            state.settings_editor.note_execution_mode(mode);
            let what = match mode {
                ahma_common::config::ExecutionPolicy::Sync => "calls wait for their result",
                ahma_common::config::ExecutionPolicy::Async => {
                    "calls return an operation id; results are collected with `await`"
                }
            };
            push_assistant_message(
                state,
                format!(
                    "Execution mode is now **{mode}** ({what}), saved to ~/.ahma/settings.toml. \
                     New ahma sessions use it; sessions already running keep their mode \
                     until restarted (ask the agent to run the `restart` tool)."
                ),
            );
        }
        Err(e) => push_assistant_message(
            state,
            format!("Could not save execution mode **{mode}** to settings: {e}"),
        ),
    }
}

fn handle_basic_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    match cmd {
        "/help" | "/?" => state.modal = crate::state::ModalState::Help,
        // The guided setup was reachable only by selecting a window with no
        // LLM configured — i.e. never, once anything was picked.
        "/setup" | "/connect" => start_llm_setup_wizard(state),
        "/resume" => {
            if let Some(home) = ahma_common::config::ahma_home_dir() {
                resume_transcript(state, &home);
            }
        }
        "/sync" => set_execution_mode(state, ahma_common::config::ExecutionPolicy::Sync),
        "/async" => set_execution_mode(state, ahma_common::config::ExecutionPolicy::Async),
        "/clear" => state.clear_screen(),
        "/compact" => {
            const KEEP_TURNS: usize = 4;
            let dropped = state.chat.compact(KEEP_TURNS);
            let msg = if dropped == 0 {
                format!("Nothing to compact: {KEEP_TURNS} turns or fewer.")
            } else {
                format!(
                    "Dropped {dropped} older turn(s); the model now sees the last {KEEP_TURNS}."
                )
            };
            // A footer note, not a transcript entry: a transcript entry would
            // itself be sent to the model as an assistant message.
            set_footer_hint(state, msg);
        }
        // The work view is always on screen now, so `/tasks` focuses it. Kept
        // because it is in a lot of muscle memory.
        "/tasks" => state.focus = crate::state::Focus::Work,
        "/chat" => toggle_chat_pane(state),
        "/log" => {
            if !state.log_window_open {
                state.log_window_open = true;
                state.focus = crate::state::Focus::Log;
            } else if state.focus == crate::state::Focus::Log {
                state.log_window_open = false;
                state.focus = crate::state::Focus::Chat;
            } else {
                state.focus = crate::state::Focus::Log;
            }
        }
        // Informational sub-window (SPEC R5.4(b): the persistent TUI scope
        // panel); not focusable, so plain toggle without a focus change.
        "/scope" => state.scope_window_open = !state.scope_window_open,
        _ => return false,
    }

    true
}

fn handle_settings_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd != "/settings" {
        return false;
    }
    state.settings_editor.open();
    true
}

/// Startup "it just works" behavior (SPEC R24.2): if the hub replay reveals
/// live work for this project — an MCP client (Claude Code, Cursor, …) already
/// running operations — switch straight to the monitor task tree so the user
/// sees what is being done on their behalf without pressing anything. Armed
/// only until the first keystroke, and only fires while still in chat mode.
fn maybe_auto_open_task_view(state: &mut crate::state::AppState) {
    use crate::state::OpStatus;

    if !state.auto_view_pending {
        return;
    }
    let project = state.project_root.as_deref();
    let live = state.operations.iter().find(|op| {
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
    let Some(live) = live else { return };

    // Open the section that work belongs to, rather than merely switching to a
    // view of everything: a user who opens the TUI mid-build wants that build's
    // output, not a list they then have to click into (SPEC R24.2).
    state.auto_view_pending = false;
    state.focus = crate::state::Focus::Work;
    let now = crate::ui::wall_ms();
    state.rebuild_work_view(now);
    let live_instance = live.instance_id.clone();
    let key = state
        .work_sections
        .borrow()
        .iter()
        .find(|s| live_instance.as_deref() == Some(s.key.as_str()))
        .map(|s| s.key.clone());
    if let Some(key) = key
        && state.open_section.as_deref() != Some(key.as_str())
    {
        state.toggle_section(&key, now);
    }
}

/// Open or close the chat pane (SPEC R24.9): it is a thing you choose to do,
/// not the screen the window is for.
fn toggle_chat_pane(state: &mut crate::state::AppState) {
    state.chat_open = !state.chat_open;
    state.focus = if state.chat_open {
        crate::state::Focus::Chat
    } else {
        crate::state::Focus::Work
    };
}

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
        handle_mcp_refresh(state);
        return true;
    }

    if let Some(rest) = cmd.strip_prefix("/mcp remove ") {
        handle_mcp_remove(rest, state);
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

fn handle_mcp_refresh(state: &mut crate::state::AppState) {
    if let Some(tx) = &state.bridge_tx {
        crate::llm_bridge::spawn_external_tools_refresh(state.mcp_connections.clone(), tx.clone());
        push_assistant_message(state, "Refreshing external MCP tools in the background...");
    } else {
        push_assistant_message(
            state,
            "Bridge is not available; cannot refresh external tools.",
        );
    }
}

fn handle_mcp_remove(rest: &str, state: &mut crate::state::AppState) {
    let name = rest.trim();
    if name.is_empty() {
        push_assistant_message(state, "Usage: /mcp remove <name>");
        return;
    }
    state.mcp_connections.remove_server(name);
    if let Ok(cwd) = std::env::current_dir() {
        let _ = state.mcp_connections.save(&cwd);
    }
    push_assistant_message(state, format!("Removed MCP server `{name}`."));
}

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
            state.llm_selection = Some(crate::state::LlmSelection::profile(name, profile.model));
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

/// Append one chat entry's markdown rendering to `md`.
fn push_entry_markdown(md: &mut String, entry: &crate::state::ChatEntry) {
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
        crate::state::ChatEntry::Notice { text } => {
            md.push_str(&format!("_{text}_\n\n"));
        }
    }
}

/// Render the whole transcript as a markdown document.
fn chat_to_markdown(chat: &crate::state::ChatHistory) -> String {
    let mut md = String::from("# ahma chat export\n\n");
    for entry in chat.entries() {
        push_entry_markdown(&mut md, entry);
    }
    md
}

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

    let md = chat_to_markdown(&state.chat);
    match std::fs::write(&file, md) {
        Ok(_) => push_assistant_message(state, format!("Exported chat to `{}`", file.display())),
        Err(e) => push_assistant_message(state, format!("Export failed: {e}")),
    }
    true
}

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

fn handle_approval_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    match cmd {
        "/approve" => resolve_approval(state, true),
        "/reject" => resolve_approval(state, false),
        _ => return false,
    }

    true
}

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
fn current_provider_name(state: &crate::state::AppState) -> Option<String> {
    // `provider_name()` is `None` for a profile, so a profile alias can never
    // reach a caller that wants a registry provider.
    state
        .llm_selection
        .as_ref()
        .and_then(|s| s.provider_name())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

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

/// Kick off an async model-list refresh and let the user know it's in
/// flight. Split out of `open_model_picker` so the empty-items branch reads
/// as one call instead of a nested `if let`.
fn request_model_refresh(state: &mut crate::state::AppState) {
    let (base_url, _) = parse_llm_selection(state);
    if !base_url.is_empty()
        && let Some(tx) = &state.bridge_tx
    {
        crate::llm_bridge::spawn_model_refresh(base_url, tx.clone());
        state.model_picker_requested = true;
    }
    push_assistant_message(state, "Fetching model list…");
}

/// The `provider / model` prefix used to pre-select the current choice in
/// the model picker.
fn selected_provider_label(state: &crate::state::AppState) -> String {
    state
        .llm_selection
        .as_ref()
        .map(|s| match &s.provider {
            crate::state::ProviderRef::Named(n) => n.clone(),
            crate::state::ProviderRef::Profile(a) => format!("profile:{a}"),
        })
        .unwrap_or_default()
}

fn open_model_picker(state: &mut crate::state::AppState) {
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

    // The current provider may not be a discovered one (configured by hand or
    // by the setup wizard): its refreshed models live only in
    // `available_models`. Without this the refresh result opened an empty
    // picker, which asked for another refresh, forever.
    // Bare names: submitting one keeps the current provider and swaps only
    // the model (a `provider / model` row needs a discovered provider).
    if items.is_empty() {
        items = state.available_models.clone();
    }

    if items.is_empty() {
        request_model_refresh(state);
        return;
    }

    let mut picker = PickerState::new("Select model", items);
    let selected_model = state.selected_model();
    let selected_provider = selected_provider_label(state);
    if !selected_model.is_empty() && !selected_provider.is_empty() {
        let exact = format!("{selected_provider} / {selected_model}");
        picker.select_exact(&exact);
    }
    state.modal = crate::state::ModalState::ModelPicker(picker);
}

fn handle_run_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if !cmd.starts_with("/run ") {
        return false;
    }

    run_nav_tool(cmd.trim_start_matches("/run ").trim(), state);
    true
}

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
            let id = format!("call_{}", ahma_common::keepalive::current_timestamp_ms());
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

/// Read the user's persistent scope grants so the `/scope` panel can show
/// which roots exist because the user granted them. Read-only: the write path
/// stays with `ahma sandbox grant/revoke`, which gates every write behind the
/// preview-and-approve exchange and the catastrophic-path denylist (SPEC
/// R5.4.5) — machinery the TUI must not duplicate half-way.
fn load_granted_scopes(state: &mut crate::state::AppState) {
    let settings = ahma_common::config::AhmaSettings::load();
    state.granted_scopes = settings
        .sandbox
        .persistent_scopes
        .iter()
        .map(|s| {
            (
                s.path.display().to_string(),
                match s.access {
                    ahma_common::config::ScopeAccess::Ro => "ro".to_string(),
                    ahma_common::config::ScopeAccess::Rw => "rw".to_string(),
                },
            )
        })
        .collect();
}

/// Move the pre-UI startup notices onto the screen: all of them into the log
/// pane, and the warnings additionally into the chat transcript, which is the
/// pane that is open by default. A warning the user has to run `/log` to
/// discover is not much better than one in a file.
fn drain_startup_notices(state: &mut crate::state::AppState) {
    use crate::startup_notices::Level;

    for notice in crate::startup_notices::drain() {
        state.push_log(crate::state::LogEntry {
            timestamp: chrono::Local::now(),
            level: match notice.level {
                Level::Info => crate::state::LogLevel::Info,
                Level::Warn => crate::state::LogLevel::Warn,
            },
            message: notice.message.clone(),
        });
        if notice.level == Level::Warn {
            push_assistant_message(state, format!("Note: {}", notice.message));
        }
    }
}

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
    let Some(selection) = state.llm_selection.as_ref() else {
        return (String::new(), String::new());
    };

    let model = selection.model.clone();
    if let Some(base_url) = &state.current_provider_url {
        return (base_url.clone(), model);
    }

    // No URL and no registry name (a profile always carries a URL, so this is
    // the named case) — fall back to the provider's default endpoint.
    let provider_name = selection.provider_name().unwrap_or_default().to_string();
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

/// Persist the current session config to `.ahma/session.toml`.
fn save_session(state: &mut crate::state::AppState) {
    use crate::session_config::{TuiSessionConfig, WindowLlmConfig, push_recent};

    // Straight off the typed selection. Splitting the *display* label here is
    // what wrote `provider = "no LLM"` and `provider = "profile:<alias>"` into
    // the user's global settings.
    let (provider, model) = match state.llm_selection.as_ref() {
        Some(sel) => (sel.persistable_provider().to_string(), sel.model.clone()),
        None => (String::new(), String::new()),
    };
    push_recent(
        &mut state.recent_llms,
        WindowLlmConfig {
            provider: provider.clone(),
            model: model.clone(),
            provider_url: state.current_provider_url.clone(),
        },
    );
    if cfg!(test) {
        return;
    }

    let cfg = TuiSessionConfig {
        provider: provider.clone(),
        model: model.clone(),
        provider_url: state.current_provider_url.clone(),
        mcp_enabled: state.mcp_enabled,
        active_profile: state.active_profile.clone(),
        window_llms: state.window_llms.clone(),
        recent: state.recent_llms.clone(),
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
fn persist_selected_model_to_settings(provider: &str, model: &str, provider_url: &Option<String>) {
    let current = ahma_common::config::AhmaSettings::load();
    let to_opt = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    let next_provider = to_opt(provider);
    let next_model = to_opt(model);

    // Avoid a needless disk write when nothing changed.
    if current.agent.provider == next_provider
        && current.agent.model == next_model
        && &current.agent.provider_url == provider_url
    {
        return;
    }
    let saved = ahma_common::config::AhmaSettings::update(|s| {
        s.agent.provider = next_provider;
        s.agent.model = next_model;
        s.agent.provider_url = provider_url.clone();
    });
    if let Err(e) = saved {
        debug!("Failed to persist selected model to settings: {e}");
    }
}

// ─── Bridge event handler ─────────────────────────────────────────────────────

/// An `mcp://` provider for a window whose MCP client can answer prompts with
/// its own model (MCP sampling).
///
/// Only a client that declared the `sampling` capability gets one. This used to
/// return a provider for *every* instance — hooks and this TUI included — with
/// invented model names ("Claude 3.5 Sonnet (IDE subscription)"): entries that
/// could never answer, and because one always existed, an LLM was auto-selected
/// and the setup wizard never ran. With sampling the client picks the model;
/// ahma can only state a preference, so the entry says so instead of naming
/// models it cannot promise.
fn virtual_provider_for_instance(
    inst: &ahma_common::daemon_hub::InstanceInfo,
) -> Option<ahma_llm_monitor::LocalProvider> {
    if !inst.sampling || inst.mode == "hook" || inst.mode == "tui" {
        return None;
    }
    let who = inst.client.as_deref().unwrap_or(&inst.label);
    Some(ahma_llm_monitor::LocalProvider {
        name: format!("{who} (its own model)"),
        base_url: format!("mcp://{}", inst.label),
        models: vec!["client's choice".to_string()],
    })
}

fn rebuild_available_providers(state: &mut crate::state::AppState) {
    let mut combined = state.discovered_providers.clone();
    for inst in &state.active_instances {
        if let Some(virtual_provider) = virtual_provider_for_instance(inst)
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

fn handle_instances_updated(
    instances: Vec<ahma_common::daemon_hub::InstanceInfo>,
    state: &mut crate::state::AppState,
) {
    state.active_instances = instances;
    rebuild_available_providers(state);
    follow_client_model_availability(state);
}

/// Keep chat on a model that exists (see [`crate::session_config::pick_fallback`]).
///
/// An MCP client's own model is only there while that client is connected.
/// When it goes, chat moves to the most recent model ahma runs itself and says
/// so; when the same client comes back and the user has not picked anything
/// else meanwhile, chat moves back — also said. Nothing here ever happens
/// silently: a model change the user did not make is always announced.
fn follow_client_model_availability(state: &mut crate::state::AppState) {
    use crate::session_config::is_client_model_url;
    let available = |state: &crate::state::AppState, url: &str| {
        state.available_providers.iter().any(|p| p.base_url == url)
    };

    // The displaced client model is back, and we are still on our stand-in.
    if let Some((displaced_sel, displaced_url, stand_in)) = state.displaced_client_model.clone()
        && available(state, &displaced_url)
    {
        if state.llm_selection.as_ref() == Some(&stand_in) {
            let back_to = crate::ui::shorten_llm_label(&displaced_sel.display_label());
            state.llm_selection = Some(displaced_sel);
            state.current_provider_url = Some(displaced_url);
            state.chat.push(crate::state::ChatEntry::Notice {
                text: format!("{back_to} is back — chat switched back to it"),
            });
            save_session(state);
        }
        state.displaced_client_model = None;
        return;
    }

    let Some(url) = state.current_provider_url.clone() else {
        return;
    };
    if !is_client_model_url(&url) || available(state, &url) {
        return;
    }
    let gone = state.llm_label();
    let fallback =
        crate::session_config::pick_fallback(&state.recent_llms, &state.available_providers)
            .cloned();
    let text = match fallback {
        Some(next) => {
            let previous = state.llm_selection.clone();
            let stand_in =
                crate::state::LlmSelection::named(next.provider.clone(), next.model.clone());
            state.llm_selection = Some(stand_in.clone());
            state.current_provider_url = next.provider_url.clone();
            if let Some(previous) = previous {
                state.displaced_client_model = Some((previous, url, stand_in));
            }
            save_session(state);
            format!(
                "{} disconnected — chat switched to {}",
                crate::ui::shorten_llm_label(&gone),
                crate::ui::shorten_llm_label(&state.llm_label())
            )
        }
        None => {
            state.llm_selection = None;
            state.current_provider_url = None;
            format!(
                "{} disconnected and no other model is set up — /setup connects one",
                crate::ui::shorten_llm_label(&gone)
            )
        }
    };
    state.chat.push(crate::state::ChatEntry::Notice { text });
}

/// How often the idle TUI looks for local model servers again.
const PROVIDER_REDISCOVERY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

fn handle_providers_discovered(
    providers: Vec<ahma_llm_monitor::LocalProvider>,
    state: &mut crate::state::AppState,
) {
    // Periodic re-discovery reports the same servers most of the time; only a
    // real change is worth a rebuild (which re-reads config.toml).
    if providers == state.discovered_providers && !state.available_providers.is_empty() {
        return;
    }
    state.discovered_providers = providers.clone();
    rebuild_available_providers(state);
    follow_client_model_availability(state);

    if state.llm_selection.is_none() {
        if let Some(provider) = state.available_providers.first().cloned() {
            auto_select_first_provider(state, &provider);
        }
        return;
    }

    if let Some(current_url) = state.current_provider_url.clone() {
        refresh_current_provider_models(state, &current_url);
    }
}

fn auto_select_first_provider(
    state: &mut crate::state::AppState,
    provider: &ahma_llm_monitor::LocalProvider,
) {
    let model = provider.models.first().cloned().unwrap_or_default();
    state.available_models = provider.models.clone();
    state.current_provider_url = Some(provider.base_url.clone());
    state.llm_selection = Some(crate::state::LlmSelection::named(
        provider.name.clone(),
        model,
    ));
    save_session(state);
}

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
        state.llm_selection = Some(crate::state::LlmSelection::named(
            provider.name.clone(),
            model,
        ));
    }
}

fn handle_model_refreshed(
    base_url: String,
    models: Vec<String>,
    state: &mut crate::state::AppState,
) {
    if state
        .setup_pending
        .as_ref()
        .is_some_and(|p| !p.connected && p.base_url == base_url)
    {
        handle_setup_models(models, state);
        return;
    }
    if let Some(provider) = state
        .available_providers
        .iter_mut()
        .find(|p| p.base_url == base_url)
    {
        provider.models = models.clone();
    }

    if state.current_provider_url.as_deref() == Some(base_url.as_str()) && !models.is_empty() {
        state.available_models = models.clone();
        // Only a refresh the user asked for (picking a provider, opening an
        // empty model list) opens the picker; a background one must not
        // steal focus from whatever they are doing now.
        if std::mem::take(&mut state.model_picker_requested) {
            open_model_picker(state);
        }
    }
}

/// Build the pending window that will execute one decomposed step. A step is
/// either a shell command or an LLM call, and that single flag decides the
/// label, the payload and whether a model is recorded.
fn window_from_step(
    win_id: usize,
    step: &crate::llm_bridge::ParsedStep,
    state: &crate::state::AppState,
) -> crate::state::TuiWindow {
    let is_cli = step.r#type.as_str() == "shell_command";

    let (label, command, llm_model) = if is_cli {
        (
            format!(
                "Command: {} in {}",
                step.command.as_deref().unwrap_or(&step.task),
                crate::ui::shorten_path(&state.workspace, 20)
            ),
            step.command.clone().unwrap_or_else(|| step.task.clone()),
            None,
        )
    } else {
        (
            format!("LLM Call (model: {})", state.selected_model()),
            step.instructions
                .clone()
                .unwrap_or_else(|| step.task.clone()),
            Some(state.selected_model()),
        )
    };

    crate::state::TuiWindow {
        id: win_id,
        label,
        status: crate::state::WindowStatus::Pending,
        content: vec![crate::state::WindowLine::start(format!(
            "Task: {}",
            step.task
        ))],
        collapsed: false,
        finished_at: None,
        duration_ms: None,
        last_output_at: None,
        is_cli,
        command,
        working_dir: state.workspace.clone(),
        llm_model,
        visible: true,
        abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        op_id: None,
    }
}

fn handle_decomposed_event(
    steps: Vec<crate::llm_bridge::ParsedStep>,
    state: &mut crate::state::AppState,
) {
    for step in steps {
        let win_id = state.next_window_id;
        state.next_window_id = (state.next_window_id + 1) % 100;

        let w = window_from_step(win_id, &step, state);
        state.windows.push(w);
        if state.windows.len() > 100 {
            state.windows.remove(0);
        }
    }

    run_next_pending_window(state);
}

fn handle_window_output_event(window_id: usize, line: String, state: &mut crate::state::AppState) {
    if let Some(w) = state.windows.iter_mut().find(|w| w.id == window_id) {
        if w.is_cli {
            w.content.push(crate::state::WindowLine::output(line));
        } else {
            append_multiline_window_output(&mut w.content, &line);
        }
    }
}

/// Appends `line` to `content`, splitting on embedded newlines so that each
/// resulting segment becomes its own entry (continuing the last existing
/// entry rather than starting a fresh one for the first segment).
fn append_multiline_window_output(content: &mut Vec<crate::state::WindowLine>, line: &str) {
    use crate::state::LineKind;
    // Continue the previous line only when it is actually output; appending a
    // stdout fragment onto the "Starting …" header would corrupt both.
    if !matches!(content.last().map(|l| &l.kind), Some(LineKind::Output)) {
        content.push(crate::state::WindowLine::output(String::new()));
    }
    let parts: Vec<&str> = line.split('\n').collect();
    if let Some(last) = content.last_mut() {
        last.text.push_str(parts[0]);
    }
    for part in parts.iter().skip(1) {
        content.push(crate::state::WindowLine::output(*part));
    }
}

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
        w.content
            .push(crate::state::WindowLine::end(w.status, summary));
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
fn request_tool_approval(
    state: &mut crate::state::AppState,
    id: String,
    tool: String,
    args: String,
    workspace: Option<String>,
    responder: Option<tokio::sync::oneshot::Sender<bool>>,
) {
    // Both of these ask the shared approvals mechanism. The preview used to be
    // decided here by a substring guess on the tool name, which could not work
    // for tools ahma does not define (MTDF custom tools) and defaulted to
    // hiding the arguments — the unsafe direction for a prompt whose whole job
    // is to let the operator see what they are authorising.
    let diff = ahma_core::approvals::argument_preview(&tool, &args);
    if let Some(turn) = state.turn.as_mut() {
        turn.enter(crate::state::TurnPhase::AwaitingYou);
    }
    // Grants are keyed by the workspace the *agent* checks; fall back to ours
    // only for a peer too old to say which that is.
    let workspace = workspace.unwrap_or_else(|| state.workspace.clone());
    let note = ahma_core::approvals::reask_note(std::path::Path::new(&workspace), &tool);
    state.request_approval(
        crate::state::ApprovalGate::new(id, tool.clone(), format!("Execute tool {tool}"))
            .with_workspace(workspace)
            .with_note(note)
            .with_diff(diff),
        responder,
    );
}

fn handle_bridge_event(event: crate::llm_bridge::BridgeEvent, state: &mut crate::state::AppState) {
    use crate::llm_bridge::BridgeEvent;
    use crate::state::ChatEntry;

    // Any server signal that the turn is alive and more is coming advances the
    // liveness panel in front of the `ahma` response line; the panel's motion
    // direction follows the turn state (thinking shimmers, streaming rains,
    // tool dispatch scrolls right). Done/Error clear it back to blanks below.
    match &event {
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
        BridgeEvent::Error(msg) => {
            state.chat.push(ChatEntry::Assistant {
                content: format!("Error: {msg}"),
                streaming: false,
            });
            state.chat_scroll = 0;
        }
        BridgeEvent::TurnSendFailed(msg) => end_turn_with_error(state, &msg),
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
        BridgeEvent::SessionEstablished { session_id } => {
            state.session_id = Some(session_id);
        }
    }
}

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
        format_generic_args_summary(obj)
    }
}

/// Render every argument except the plumbing-only ones (`working_directory`,
/// `working_dir`, `synchronous`) as `key=value`, joined by `, `. Split out of
/// `extract_args_summary` so its non-`run_terminal_command` branch reads as
/// one call.
fn format_generic_args_summary(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let parts: Vec<String> = obj
        .iter()
        .filter(|(k, _)| *k != "working_directory" && *k != "working_dir" && *k != "synchronous")
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

fn format_friendly_end(op: &crate::state::Operation) -> String {
    // Via the shared identity mechanism (SPEC R24.7) rather than a local match:
    // the local one had a `_` arm that swallowed `Denied` and announced a
    // refused write as "Finished".
    let status_str = op.identity().outcome.friendly_phrase();

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
fn window_content_for(
    op: &crate::state::Operation,
    unicode: bool,
) -> Vec<crate::state::WindowLine> {
    // The complement of `is_terminal`, not a third list of variants: an eighth
    // OpStatus would otherwise have to be added here, in ui.rs, and in
    // `is_terminal`, with nothing catching a disagreement.
    let is_live = !op.status.is_terminal();

    let mut content = Vec::with_capacity(op.stdout_tail.len() + 3);
    content.push(crate::state::WindowLine::start(format_friendly_start(op)));

    // Live output tail — the command's stdout/stderr as it streams in.
    content.extend(op.stdout_tail.iter().map(crate::state::WindowLine::output));

    if is_live {
        content.push(crate::state::WindowLine::live_edge());
    } else {
        let sep = if unicode {
            "────────────────────────────────────────"
        } else {
            "----------------------------------------"
        };
        content.push(crate::state::WindowLine::separator(sep));
        content.push(crate::state::WindowLine::end(
            window_status_for(op),
            format_friendly_end(op),
        ));
    }
    content
}

fn window_status_for(op: &crate::state::Operation) -> crate::state::WindowStatus {
    use crate::state::{OpStatus, WindowStatus};
    match op.status {
        OpStatus::Running => WindowStatus::Running,
        OpStatus::Pending | OpStatus::Waiting => WindowStatus::Pending,
        OpStatus::Succeeded => WindowStatus::Finished,
        OpStatus::Failed | OpStatus::Denied => WindowStatus::Error,
        // Cancelled, not Error: an interrupted operation was not observed to
        // fail, and colouring it red would assert something we do not know.
        OpStatus::Cancelled | OpStatus::Interrupted => WindowStatus::Cancelled,
    }
}

/// Whether cards from several distinct instances currently interleave. With a
/// single active instance the per-card instance suffix is pure repetition.
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
fn window_label_for(op: &crate::state::Operation, multi_instance: bool) -> String {
    let name = op.display_name();
    match (&op.instance_label, multi_instance) {
        (Some(instance), true) => format!("{name} ({instance})"),
        _ => name,
    }
}

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

fn sync_operations_to_windows(state: &mut crate::state::AppState) {
    let mut to_add = Vec::new();
    // Moved out and put back rather than cloned: this runs on every operation
    // state change, and each `Operation` carries a stdout tail of up to
    // STDOUT_TAIL_CAP lines. None of the helpers below read `state.operations`,
    // so the borrow split costs nothing.
    let ops = std::mem::take(&mut state.operations);

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

    state.operations = ops;

    for w in to_add {
        state.windows.push(w);
        if state.windows.len() > 100 {
            state.windows.remove(0);
        }
    }
}

// ─── Source event handler ────────────────────────────────────────────────────

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

fn handle_event_log_lines_updated(
    file: String,
    content: String,
    append: bool,
    state: &mut crate::state::AppState,
) {
    if Some(&file) != state.active_log_file.as_ref() {
        return;
    }

    // The cumulative counter drives the log title's rain panel: each
    // arriving line advances the animation one frame, so pour rate shows
    // arrival rate.
    state.log_lines_total = state
        .log_lines_total
        .wrapping_add(content.lines().count() as u64);
    if append {
        append_active_log_lines(&mut state.active_log_lines, &content);
    } else {
        state.active_log_lines = content.lines().map(String::from).collect();
    }
}

/// Append `content`'s lines to the active log tail, then drop the oldest
/// lines past the 2000-line cap. Split out of `handle_event_log_lines_updated`
/// so its `append` branch reads as one call instead of a nested loop-plus-if.
fn append_active_log_lines(active_log_lines: &mut Vec<String>, content: &str) {
    for line in content.lines() {
        active_log_lines.push(line.to_string());
    }
    if active_log_lines.len() > 2000 {
        let drain_len = active_log_lines.len() - 2000;
        active_log_lines.drain(0..drain_len);
    }
}

fn handle_source_event(event: crate::mcp_source::SourceEvent, state: &mut crate::state::AppState) {
    use crate::mcp_source::SourceEvent;
    match event {
        SourceEvent::HealthChanged { healthy } => state.set_server_healthy(healthy),
        SourceEvent::DaemonHealthChanged { healthy } => {
            state.set_daemon_healthy(healthy);
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
        SourceEvent::SandboxStatus { .. }
        | SourceEvent::SandboxScope { .. }
        | SourceEvent::SandboxFailed { .. } => {
            handle_source_sandbox_event(event, state);
        }
        SourceEvent::ApprovalRequested { .. }
        | SourceEvent::ScopeGrantRequested { .. }
        | SourceEvent::ScopeGrantDismiss { .. }
        | SourceEvent::WebApprovalRequested { .. }
        | SourceEvent::WebApprovalDismiss { .. } => {
            handle_source_gate_event(event, state);
        }
        SourceEvent::ChatToken { .. }
        | SourceEvent::ChatThinking { .. }
        | SourceEvent::AgentDone
        | SourceEvent::AgentError { .. }
        | SourceEvent::Usage { .. }
        | SourceEvent::ToolCallStarted { .. }
        | SourceEvent::ToolCallFinished { .. }
        | SourceEvent::Truncated { .. } => {
            handle_source_chat_event(event, state);
        }
    }
}

fn handle_source_sandbox_event(
    event: crate::mcp_source::SourceEvent,
    state: &mut crate::state::AppState,
) {
    use crate::mcp_source::SourceEvent;
    match event {
        SourceEvent::SandboxStatus { status } => state.sandbox_status = status,
        SourceEvent::SandboxScope { scope } => {
            state.sandbox_failed_reason = None;
            state.sandbox_scope = Some(scope);
        }
        SourceEvent::SandboxFailed { error } => {
            state.sandbox_status = crate::state::SandboxAuthority::Failed;
            state.sandbox_failed_reason = Some(error.clone());
            state.push_log(crate::state::LogEntry {
                timestamp: chrono::Local::now(),
                level: crate::state::LogLevel::Error,
                message: format!(
                    "Sandbox configuration FAILED: {error} — tool calls are refused until a \
                     scope locks. See /scope for details; fix the scope source (open a \
                     workspace folder, pass --sandbox-scope, or configure [sandbox] \
                     container_root) and restart."
                ),
            });
        }
        _ => {}
    }
}

fn handle_source_gate_event(
    event: crate::mcp_source::SourceEvent,
    state: &mut crate::state::AppState,
) {
    use crate::mcp_source::SourceEvent;
    match event {
        SourceEvent::ApprovalRequested {
            id,
            tool,
            args,
            workspace,
        } => {
            request_tool_approval(state, id, tool, args, workspace, None);
        }
        SourceEvent::ScopeGrantRequested { request } => {
            state.scope_grant = Some(crate::state::ScopeGrantGate::from_request(request));
        }
        SourceEvent::ScopeGrantDismiss { decision_id }
            if state
                .scope_grant
                .as_ref()
                .is_some_and(|g| g.decision_id == decision_id) =>
        {
            state.scope_grant = None;
        }
        SourceEvent::WebApprovalRequested { request } => {
            state.web_approval = Some(crate::state::WebApprovalGate::from_request(request));
        }
        SourceEvent::WebApprovalDismiss { decision_id }
            if state
                .web_approval
                .as_ref()
                .is_some_and(|g| g.decision_id == decision_id) =>
        {
            state.web_approval = None;
        }
        _ => {}
    }
}

/// Where this window's transcript is saved: keyed by the window's durable
/// identity (client and workspace), not the per-session instance id, so the
/// next session of the same client in the same workspace finds it.
fn transcript_path(state: &crate::state::AppState, home: &std::path::Path) -> std::path::PathBuf {
    // This terminal's own chat is per workspace, like every other window.
    let key = if state.chat_key.is_empty() {
        format!("ahma-tui@{}", state.workspace)
    } else {
        state.window_llm_key(&state.chat_key)
    };
    crate::transcripts::path_for(&crate::transcripts::dir_for(home), &key)
}

/// Save the current window's transcript after a finished turn. Best-effort,
/// on the blocking pool.
fn save_transcript(state: &crate::state::AppState) {
    if cfg!(test) {
        return;
    }
    let Some(home) = ahma_common::config::ahma_home_dir() else {
        return;
    };
    let path = transcript_path(state, &home);
    let entries = crate::transcripts::to_saved(&state.chat);
    if entries.is_empty() {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn_blocking(move || {
            if let Err(e) = crate::transcripts::save(&path, &entries) {
                debug!("Failed to save transcript {}: {e}", path.display());
            }
        });
    }
}

/// `/resume`: bring back this window's saved conversation.
fn resume_transcript(state: &mut crate::state::AppState, home: &std::path::Path) {
    if state.turn.is_some() {
        set_footer_hint(state, "A reply is still coming — Esc cancels it first");
        return;
    }
    let path = transcript_path(state, home);
    match crate::transcripts::load(&path) {
        Ok(Some(entries)) => {
            let n = entries.len();
            state.chat = crate::transcripts::from_saved(entries);
            *state.chat_rows_cache.borrow_mut() = crate::state::ChatRowsCache::default();
            state.chat_scroll = 0;
            state.chat_open = true;
            set_footer_hint(
                state,
                format!("Resumed {n} messages from {}", path.display()),
            );
        }
        Ok(None) => push_assistant_message(
            state,
            format!(
                "No saved conversation for this window yet ({}).",
                path.display()
            ),
        ),
        Err(e) => push_assistant_message(state, format!("Could not read {}: {e}", path.display())),
    }
}

/// Append one line per finished turn to the active agent profile's transcript
/// log, when a profile is active. Best-effort, on the blocking pool (no
/// blocking I/O on the event loop).
fn append_profile_transcript(state: &crate::state::AppState) {
    if let Some(profile) = &state.active_profile
        && let Ok(cwd) = std::env::current_dir()
    {
        let payload = serde_json::json!({
            "timestamp": chrono::Local::now().to_rfc3339(),
            "chat_entries": state.chat.entries().len(),
            "model": state.selected_model(),
        });
        // Blocking file append must not run on the async event loop
        // (repo rule: no blocking I/O in async context) — hand it to
        // the blocking pool; best-effort, as before.
        let profile = profile.clone();
        tokio::task::spawn_blocking(move || {
            let _ =
                crate::agent_config::append_transcript_entry(&cwd, &profile, &payload.to_string());
        });
    }
}

fn handle_source_chat_event(
    event: crate::mcp_source::SourceEvent,
    state: &mut crate::state::AppState,
) {
    use crate::mcp_source::SourceEvent;
    // Stream events belong to the turn this TUI started. With none in flight
    // they are another TUI's turn (the hub broadcasts to every subscriber) or
    // late output from a turn just cancelled here.
    let Some(turn) = state.turn.as_mut() else {
        tracing::debug!("chat event with no turn in flight, ignored: {event:?}");
        return;
    };
    turn.last_event = std::time::Instant::now();
    match event {
        SourceEvent::ChatToken { token } => {
            if let Some(turn) = state.turn.as_mut() {
                turn.note_token(&token);
            }
            state.chat.append_token(&token);
            state.mark_stream_activity(crate::state::LivenessState::Streaming);
            state.chat_scroll = 0;
        }
        SourceEvent::ChatThinking { token } => {
            if let Some(turn) = state.turn.as_mut() {
                turn.enter(crate::state::TurnPhase::Thinking);
            }
            state.chat.append_thinking(&token);
            state.mark_stream_activity(crate::state::LivenessState::Thinking);
            state.chat_scroll = 0;
        }
        SourceEvent::AgentDone => {
            state.turn_retries = 0;
            if let Some(text) = turn_summary(state) {
                state.chat.push(crate::state::ChatEntry::Notice { text });
            }
            end_turn(state);
            append_profile_transcript(state);
            save_transcript(state);
        }
        SourceEvent::AgentError { error } => end_turn_with_error(state, &error),
        SourceEvent::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        } => {
            state.token_usage.prompt_tokens += prompt_tokens;
            state.token_usage.completion_tokens += completion_tokens;
            state.token_usage.total_tokens += total_tokens;
            if prompt_tokens > 0 {
                state.last_prompt_tokens = prompt_tokens;
            }
            record_read_rate(state, prompt_tokens);
            let key = crate::state::AppState::usage_key(
                state
                    .turn
                    .as_ref()
                    .and_then(|t| t.target_instance.as_deref()),
            );
            let usage = state.window_usage.entry(key).or_default();
            usage.prompt_tokens += prompt_tokens;
            usage.completion_tokens += completion_tokens;
            if prompt_tokens > 0 {
                usage.last_prompt_tokens = prompt_tokens;
            }
            state.mark_stream_activity(state.liveness_state);
        }
        SourceEvent::ToolCallStarted { id, name, args } => {
            if let Some(turn) = state.turn.as_mut() {
                turn.enter(crate::state::TurnPhase::Tool { name: name.clone() });
            }
            state.mark_stream_activity(crate::state::LivenessState::ToolWait);
            state.chat.start_tool_call(id, name, args);
            state.chat_scroll = 0;
        }
        SourceEvent::ToolCallFinished { id, result, failed } => {
            // The result goes back to the model, which reads it all again.
            if let Some(turn) = state.turn.as_mut() {
                turn.enter(crate::state::TurnPhase::Reading);
            }
            state.mark_stream_activity(crate::state::LivenessState::Thinking);
            state.chat.finish_tool_call(&id, result, failed);
            state.chat_scroll = 0;
        }
        SourceEvent::Truncated { reason } => {
            // Same rendering as the in-process BridgeEvent::Truncated: a
            // visible note in the transcript, not silently folded into the
            // model's own output.
            state.chat.push(crate::state::ChatEntry::Assistant {
                content: format!("[{reason}]"),
                streaming: false,
            });
            state.chat_scroll = 0;
        }
        _ => {}
    }
}

/// Turn the model's last reading time and the prompt size its provider just
/// reported into a reading speed for that model, so the next wait can say how
/// long it will likely take instead of just how long it has been.
fn record_read_rate(state: &mut crate::state::AppState, prompt_tokens: u32) {
    let Some(prefill) = state.turn.as_mut().and_then(|t| t.last_prefill.take()) else {
        return;
    };
    let secs = prefill.as_secs_f64();
    if prompt_tokens == 0 || secs < 0.05 {
        return;
    }
    let model = state.llm_label();
    state
        .read_rates
        .insert(model, f64::from(prompt_tokens) / secs);
}

/// Append one live output line to an operation's tail buffer and refresh the
/// matching window incrementally — no full window rebuild, no polling delay.
fn handle_operation_output(
    state: &mut crate::state::AppState,
    instance_id: Option<String>,
    op_id: &str,
    line: String,
) {
    // Prefer an exact (id, instance) match; fall back to an op first seen via
    // the poll path, which carries no instance. Never to another instance's
    // op with the same id: that would splice one client's output into
    // another's.
    let op_idx = state
        .operations
        .iter()
        .position(|o| o.id == op_id && o.instance_id == instance_id)
        .or_else(|| {
            state
                .operations
                .iter()
                .position(|o| o.id == op_id && o.instance_id.is_none())
        });

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

// ─── Utility ─────────────────────────────────────────────────────────────────

/// Whether to draw box-drawing and status glyphs rather than ASCII fallbacks.
///
/// Keyed on the terminal's own capability signal only. `NO_COLOR` used to gate
/// this too, which inverted the standard's meaning: a user asking for no colour
/// got ASCII glyphs *and* a fully coloured UI, since nothing here ever consulted
/// it when choosing styles. Colour is [`no_color`]'s business; glyphs are this
/// function's. See <https://no-color.org>.
fn detect_unicode() -> bool {
    !std::env::var("TERM")
        .unwrap_or_default()
        .to_lowercase()
        .contains("dumb")
}

/// Honour the `NO_COLOR` convention: any non-empty value means "do not emit
/// colour". The standard is about colour specifically, so it must not also
/// change which characters are drawn.
pub fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

fn http_base_url(connection: &ResolvedConnection) -> String {
    match &connection.transport {
        crate::connection::ResolvedTransport::Http(url)
        | crate::connection::ResolvedTransport::Http3(url) => url.clone(),
        #[cfg(unix)]
        crate::connection::ResolvedTransport::UnixSocket(path) => {
            // `AHMA_HTTP_URL` is retired (R-CFG1.2) and warn-and-ignored: it was
            // an undocumented second way to redirect the endpoint, invisible to
            // the retired-env drift test precisely because it was undocumented.
            // `ahma tui --connect <URL>` does the same job at the invocation
            // site, where it can be seen.
            ahma_common::config::warn_retired_env("AHMA_HTTP_URL");
            format!("unix://{}", path)
        }
    }
}

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

fn start_window_execution(win_id: usize, state: &mut crate::state::AppState) {
    use crate::llm_bridge::spawn_window_llm_task;

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

    // Without a bridge there is nowhere to send the task's output, so both
    // branches below would be no-ops. Bail once instead of twice.
    let Some(tx) = bridge_tx else {
        return;
    };

    if is_cli {
        start_cli_window(win_id, command, working_dir, abort_rx, tx, state);
    } else {
        let mcp = if state.mcp_enabled {
            Some(mcp_chat_config(state))
        } else {
            None
        };
        spawn_window_llm_task(win_id, base_url, model, command, mcp, abort_rx, tx);
    }
}

/// Report the re-run to the active bang reporter (if any) and hand the
/// command off to the CLI task. Split out of `start_window_execution` so the
/// `is_cli` branch there reads as one call.
fn start_cli_window(
    win_id: usize,
    command: String,
    working_dir: String,
    abort_rx: tokio::sync::oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::Sender<crate::llm_bridge::BridgeEvent>,
    state: &mut crate::state::AppState,
) {
    use crate::llm_bridge::spawn_window_cli_task;

    // A re-run is a second command, not the first one resuming, so it
    // reports under an id of its own — window ids wrap and get reused.
    let report = state.bang_report();
    if let Some(r) = &report {
        r.reporter.report(crate::tui_reporter::bang_started(
            &r.op_id,
            &command,
            &working_dir,
        ));
        if let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id) {
            w.op_id = Some(r.op_id.clone());
        }
    }
    spawn_window_cli_task(win_id, command, working_dir, abort_rx, tx, report);
}

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

fn handle_log_file_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if !cmd.starts_with("/log file ") {
        return false;
    }

    let rest = cmd.strip_prefix("/log file ").unwrap().trim();
    let (path, prompt) = parse_monitor_path_and_prompt(rest);

    if path.is_empty() {
        push_assistant_message(state, "Usage: /log file <path> [prompt]");
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

fn handle_analyze_nav_command(cmd: &str, state: &mut crate::state::AppState) -> bool {
    if cmd == "/analyze" {
        let key = state.selected_op().map(crate::state::OpKey::of);
        if let Some(key) = key {
            analyze_operation(state, &key);
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
        analyze_operation(state, &crate::state::OpKey::bare(op_id));
        return true;
    }

    false
}

fn analyze_operation(state: &mut crate::state::AppState, key: &crate::state::OpKey) {
    let extracted = {
        let Some(op) = state.find_op(key) else {
            push_assistant_message(state, format!("Operation `{}` not found.", key.id));
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

    let (base_url, model) = parse_llm_selection(state);
    if base_url.is_empty() {
        push_assistant_message(
            state,
            "No LLM configured. /setup connects one step by step (or /provider picks a known one).",
        );
        return;
    }

    state.chat.push(crate::state::ChatEntry::User {
        text: format!("Analyze operation {}", id),
        payload: None,
        started_at: Some(std::time::Instant::now()),
        duration_ms: None,
    });
    state.chat.push(crate::state::ChatEntry::Assistant {
        content: String::new(),
        streaming: true,
    });
    state.chat_scroll = 0;

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

    state.turn = Some(crate::state::ChatTurn::new(None));
    send_turn_msg(
        state,
        ahma_common::daemon_hub::ClientMsg::SubmitPrompt {
            messages,
            system_prompt,
            provider: Some(base_url),
            model: Some(model),
            target_instance_id: None,
        },
    );
}

#[inline]
fn inside_rect(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

fn handle_click_target(target: crate::state::ClickTarget, state: &mut crate::state::AppState) {
    use crate::state::ClickTarget;
    match target {
        ClickTarget::CancelOperation(key) => {
            cancel_op(state, key);
            state.focus = crate::state::Focus::Work;
        }
        ClickTarget::PinOperation(key) => {
            if let Some(op) = state.find_op_mut(&key) {
                op.pinned = !op.pinned;
            }
            state.focus = crate::state::Focus::Work;
        }
        ClickTarget::AnalyzeOperation(key) => {
            analyze_operation(state, &key);
            // The analysis streams into the chat pane. When Analyze is clicked
            // from the full-screen detail overlay, that overlay covers chat — so
            // leaving it up made a working click look like it did nothing. Close
            // it (a no-op when analysing from the side panel) and focus chat, so
            // the answer is visible as it arrives.
            state.close_modal();
            state.focus = crate::state::Focus::Chat;
        }
        ClickTarget::SelectOperation(op_idx) => {
            state.ops_selected = op_idx;
            state.focus = crate::state::Focus::Work;
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
            // live/historic output view, or fold a session group.
            state.ops_selected = row_idx;
            state.work_follow_selection.set(true);
            state.toggle_selected_tree_node(crate::ui::wall_ms());
            state.focus = crate::state::Focus::Work;
        }
        ClickTarget::SectionHeader(key) => {
            activate_window_chat(&key, state);
        }
        ClickTarget::OpenOperationDetail(key) => {
            state.open_operation_detail(key);
        }
        ClickTarget::OpenLogLine(text) => {
            // Clicking the log pane also focuses it, so Esc lands the user back
            // on the pane they were reading rather than somewhere else.
            state.focus = crate::state::Focus::Log;
            state.open_log_line_detail(text);
        }
    }
}

enum WindowHit {
    Close(usize),
    Toggle(usize),
    Detail(usize),
}

fn find_window_rect_hit(
    col: u16,
    row: u16,
    rects: &[(usize, ratatui::layout::Rect)],
) -> Option<WindowHit> {
    rects.iter().find_map(|&(win_id, rect)| {
        if !inside_rect(col, row, rect) {
            return None;
        }
        if is_close_button_click(col, row, rect) {
            Some(WindowHit::Close(win_id))
        } else if is_collapse_marker_click(col, row, rect) {
            Some(WindowHit::Toggle(win_id))
        } else {
            Some(WindowHit::Detail(win_id))
        }
    })
}

fn apply_window_hit(hit: Option<WindowHit>, state: &mut crate::state::AppState) -> bool {
    match hit {
        Some(WindowHit::Close(win_id)) => {
            close_window_by_id(win_id, state);
            true
        }
        Some(WindowHit::Toggle(win_id)) => {
            if let Some(w) = state.windows.iter_mut().find(|w| w.id == win_id) {
                w.collapsed = !w.collapsed;
            }
            true
        }
        Some(WindowHit::Detail(win_id)) => {
            let op_id = state
                .windows
                .iter()
                .find(|w| w.id == win_id)
                .and_then(|w| w.op_id.clone());
            match op_id {
                Some(id) => state.open_operation_detail(crate::state::OpKey::bare(id)),
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

fn handle_window_rect_click(col: u16, row: u16, state: &mut crate::state::AppState) -> bool {
    let hit = find_window_rect_hit(col, row, &state.window_rects.borrow());
    apply_window_hit(hit, state)
}

/// The `[+] N` / `[-] N` marker at the left edge of a card's title row —
/// clicking it toggles expand/collapse rather than drilling into the detail
/// view. Collapsed cards are one row; expanded cards count only their top row.
fn is_collapse_marker_click(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    let on_title_row = if rect.height == 1 {
        true
    } else {
        row == rect.y
    };
    on_title_row && col < rect.x + 8
}

/// Returns true when the click position falls on the close button area of a window.
/// Single-height windows use their entire right edge; taller windows require
/// the click to be on the title row.
fn is_close_button_click(col: u16, row: u16, rect: ratatui::layout::Rect) -> bool {
    // Wide enough for the explicit "[x99]" close cell plus its margin.
    let in_close_zone = col >= rect.x + rect.width.saturating_sub(6);
    if rect.height == 1 {
        in_close_zone
    } else {
        row == rect.y && in_close_zone
    }
}

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
        // A tool-call line is clipped to one row; clicking it opens the whole
        // call — arguments and result — full-screen (SPEC R24.8.4).
        if let Some(detail) =
            crate::ui::chat_entry_at(state, col, row).and_then(|idx| state.tool_call_detail(idx))
        {
            state.open_log_line_detail(detail);
        }
        return;
    }
    if inside_rect(col, row, state.log_area.get()) {
        state.focus = crate::state::Focus::Log;
        return;
    }
    if inside_rect(col, row, state.work_area.get()) {
        state.focus = crate::state::Focus::Work;
    }
}

fn scroll_overlay(up: bool, state: &mut crate::state::AppState) -> bool {
    let detail_max = state.detail_max_scroll.get();
    let overlay_scroll = match &mut state.modal {
        crate::state::ModalState::OperationDetail(d) => Some(&mut d.scroll),
        crate::state::ModalState::LogLineDetail(d) => Some(&mut d.scroll),
        _ => None,
    };
    if let Some(scroll) = overlay_scroll {
        *scroll = if up {
            scroll.saturating_sub(1)
        } else {
            (*scroll + 1).min(detail_max)
        };
        true
    } else {
        false
    }
}

fn scroll_chat(col: u16, row: u16, up: bool, state: &mut crate::state::AppState) -> bool {
    let chat_area = state.chat_area.get();
    if inside_rect(col, row, chat_area) {
        let max = state.chat_max_scroll.get();
        if up {
            state.chat_scroll = (state.chat_scroll + 1).min(max);
        } else {
            state.chat_scroll = state.chat_scroll.min(max).saturating_sub(1);
        }
        state.sync_chat_scroll_to_animation();
        true
    } else {
        false
    }
}

fn scroll_log(col: u16, row: u16, up: bool, state: &mut crate::state::AppState) {
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

fn handle_mouse_scroll(col: u16, row: u16, up: bool, state: &mut crate::state::AppState) {
    if scroll_overlay(up, state)
        || scroll_work(col, row, up, state)
        || scroll_chat(col, row, up, state)
    {
        return;
    }
    scroll_log(col, row, up, state);
}

/// Wheel over the work view scrolls it.
///
/// It also detaches selection-follow: without that the next frame pulls the
/// view back to wherever the cursor is, and the wheel appears not to work.
fn scroll_work(col: u16, row: u16, up: bool, state: &mut crate::state::AppState) -> bool {
    let area = state.work_area.get();
    if area.width == 0 || area.height == 0 || !inside_rect(col, row, area) {
        return false;
    }
    let visible = area.height as usize;
    let total = state.work_total_rows.get();
    let max_scroll = total.saturating_sub(visible);
    let current = state.ops_scroll.get();
    let next = if up {
        current.saturating_sub(3)
    } else {
        (current + 3).min(max_scroll)
    };
    state.ops_scroll.set(next);
    state.work_follow_selection.set(false);
    true
}

fn determine_scrolled_panel(state: &crate::state::AppState) -> &'static str {
    if let Some((col, row)) = state.last_mouse_pos.get() {
        let work_area = state.work_area.get();
        if inside_rect(col, row, work_area) {
            return "work";
        }
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

    match state.focus {
        crate::state::Focus::Log => "log",
        crate::state::Focus::Work => "work",
        crate::state::Focus::Chat => "chat",
    }
}

/// Compute the page size for a panel given its rendered height.
fn page_size_for_height(height: u16) -> f64 {
    (if height > 2 { height - 2 } else { 10 }) as f64
}

fn handle_page_up_down(up: bool, state: &mut crate::state::AppState) {
    // A full-screen detail overlay captures paging while open.
    if page_overlay_scroll(up, state) {
        return;
    }

    if determine_scrolled_panel(state) == "chat" {
        page_chat_scroll(up, state);
    } else {
        page_log_scroll(up, state);
    }
}

/// Pages a full-screen detail overlay's scroll offset, if one is open.
/// Returns `false` (and does nothing) when no overlay is capturing paging.
fn page_overlay_scroll(up: bool, state: &mut crate::state::AppState) -> bool {
    let detail_max = state.detail_max_scroll.get();
    let overlay_scroll = match &mut state.modal {
        crate::state::ModalState::OperationDetail(d) => Some(&mut d.scroll),
        crate::state::ModalState::LogLineDetail(d) => Some(&mut d.scroll),
        _ => None,
    };
    let Some(scroll) = overlay_scroll else {
        return false;
    };
    let page = 10;
    *scroll = if up {
        scroll.saturating_sub(page)
    } else {
        (*scroll + page).min(detail_max)
    };
    true
}

/// Chat: up scrolls forward (higher offset), down scrolls back.
fn page_chat_scroll(up: bool, state: &mut crate::state::AppState) {
    let page_size = page_size_for_height(state.chat_area.get().height);
    let max_scroll = state.chat_max_scroll.get() as f64;
    let current = state.chat_scroll_target.get();
    let new_target = if up {
        (current + page_size).min(max_scroll)
    } else {
        (current - page_size).max(0.0)
    };
    state.chat_scroll_target.set(new_target);
}

/// Log: up scrolls back (lower offset), down scrolls forward.
fn page_log_scroll(up: bool, state: &mut crate::state::AppState) {
    let page_size = page_size_for_height(state.log_area.get().height);
    let max_scroll = state.log_max_scroll.get() as f64;
    if up {
        // Detach follow and page up from the current bottom.
        state.detach_log_follow();
        let current = state.log_scroll_target.get();
        state.log_scroll_target.set((current - page_size).max(0.0));
        return;
    }
    if state.log_follow {
        return;
    }
    let current = state.log_scroll_target.get();
    let new_target = (current + page_size).min(max_scroll);
    state.log_scroll_target.set(new_target);
    // Paging down to the bottom re-engages tail-follow.
    if new_target >= max_scroll {
        state.log_follow = true;
    }
}

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

fn chat_in_progress(state: &crate::state::AppState) -> bool {
    state.turn.is_some()
}

#[cfg(test)]
mod tests {
    use super::parse_run_command;
    use super::run_unsandboxed_command;
    use crate::state::AppState;
    use serde_json::json;

    /// A failed turn is over: its elapsed timer must stop just as it does on
    /// success. The daemon emits `AgentError` for exactly this reason
    /// (`daemon_hub.rs`), and a timer left running also forced a full
    /// transcript rebuild on every frame.
    #[test]
    fn agent_error_stops_the_turn_timer() {
        use crate::mcp_source::SourceEvent;
        use crate::state::ChatEntry;

        let mut state = turn_in_flight();
        super::handle_source_event(
            SourceEvent::AgentError {
                error: "boom".into(),
            },
            &mut state,
        );

        assert!(state.chat.entries().iter().any(|e| matches!(
            e,
            ChatEntry::User {
                duration_ms: Some(_),
                ..
            }
        )));
        assert!(state.turn.is_none(), "an error ends the turn");
    }

    /// A state with one submitted prompt whose turn has not ended yet.
    fn turn_in_flight() -> AppState {
        use crate::state::ChatEntry;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.chat.push(ChatEntry::User {
            text: "hi".into(),
            payload: None,
            started_at: Some(std::time::Instant::now()),
            duration_ms: None,
        });
        state.chat.push(ChatEntry::Assistant {
            content: String::new(),
            streaming: true,
        });
        state.turn = Some(crate::state::ChatTurn::new(None));
        state.focus = crate::state::Focus::Chat;
        state
    }

    fn key(
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, mods)
    }

    /// Usage is attributed to the window the turn was sent to, so each
    /// window's header shows its own spend.
    #[test]
    fn usage_is_attributed_to_the_turns_window() {
        use crate::mcp_source::SourceEvent;
        let mut state = turn_in_flight();
        state.turn.as_mut().unwrap().target_instance = Some("inst-a".into());
        super::handle_source_event(
            SourceEvent::Usage {
                prompt_tokens: 1200,
                completion_tokens: 80,
                total_tokens: 1280,
            },
            &mut state,
        );
        let a = state.window_usage["inst-a"];
        assert_eq!(
            (a.prompt_tokens, a.completion_tokens, a.last_prompt_tokens),
            (1200, 80, 1200)
        );
        assert!(
            !state.window_usage.contains_key(""),
            "not charged to the local window"
        );
    }

    #[test]
    fn going_offline_records_since_when() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.set_daemon_healthy(true);
        assert!(state.daemon_down_since.is_none());
        state.set_daemon_healthy(false);
        let since = state.daemon_down_since.expect("down since recorded");
        state.set_daemon_healthy(false);
        assert_eq!(
            state.daemon_down_since,
            Some(since),
            "stays the first moment"
        );
        state.set_daemon_healthy(true);
        assert!(state.daemon_down_since.is_none());
    }

    #[test]
    fn agent_done_ends_the_turn() {
        let mut state = turn_in_flight();
        super::handle_source_event(crate::mcp_source::SourceEvent::AgentDone, &mut state);
        assert!(state.turn.is_none());
    }

    /// Stream events belong to the turn this TUI started. With none in flight
    /// they are another TUI's turn, or late tokens from one just cancelled, and
    /// must not open a reply that nothing will ever close.
    #[test]
    fn stream_events_without_a_turn_are_ignored() {
        use crate::mcp_source::SourceEvent;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        super::handle_source_event(
            SourceEvent::ChatToken {
                token: "stray".into(),
            },
            &mut state,
        );
        assert!(state.chat.is_empty());
    }

    #[test]
    fn stream_events_keep_the_turn_fresh() {
        use crate::mcp_source::SourceEvent;
        let mut state = turn_in_flight();
        let stale = std::time::Instant::now() - crate::state::TURN_STALL_AFTER;
        state.turn.as_mut().unwrap().last_event = stale;
        super::handle_source_event(SourceEvent::ChatToken { token: "a".into() }, &mut state);
        assert!(
            !state
                .turn
                .as_ref()
                .unwrap()
                .is_stalled(std::time::Instant::now())
        );
    }

    /// Esc on an empty input stops the running turn right away, even if the
    /// daemon is unreachable; the instance's own "cancelled" error, arriving
    /// later, must not print a second end.
    #[test]
    fn esc_cancels_a_running_turn() {
        use crate::mcp_source::SourceEvent;
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut state = turn_in_flight();

        assert!(super::handle_chat_input_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &mut state
        ));
        assert!(state.turn.is_none());
        assert!(!state.should_quit);
        let ends = |s: &AppState| s.chat.entries().len();
        let after_cancel = ends(&state);

        super::handle_source_event(
            SourceEvent::AgentError {
                error: "Cancelled by user".into(),
            },
            &mut state,
        );
        assert_eq!(ends(&state), after_cancel, "no duplicate end");
    }

    /// With text typed, Esc clears it first; the turn keeps running.
    #[test]
    fn esc_with_text_clears_input_before_cancelling() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut state = turn_in_flight();
        state.chat_input.insert_str("draft");
        super::handle_chat_input_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert!(state.chat_input_is_empty());
        assert!(state.turn.is_some());
    }

    #[test]
    fn ctrl_c_cancels_the_turn_then_quits_on_a_second_press() {
        let mut state = turn_in_flight();

        super::interrupt(&mut state);
        assert!(state.turn.is_none());
        assert!(!state.should_quit, "first Ctrl-C only cancels");

        super::interrupt(&mut state);
        assert!(state.should_quit);
    }

    #[test]
    fn ctrl_c_with_nothing_running_quits_at_once() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        super::interrupt(&mut state);
        assert!(state.should_quit);
    }

    /// `q` must not kill running work without a second, deliberate press.
    #[test]
    fn quit_with_running_operations_asks_first() {
        use crate::state::{OpStatus, Operation};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state
            .operations
            .push(Operation::new("op-1", "cargo", OpStatus::Running));

        super::request_quit(&mut state);
        assert!(!state.should_quit);
        assert!(
            state
                .footer_hint
                .as_ref()
                .is_some_and(|(h, _)| h.contains("again"))
        );

        super::request_quit(&mut state);
        assert!(state.should_quit);
    }

    /// A prompt that never reached the daemon ends its turn with a visible
    /// error instead of spinning forever.
    #[test]
    fn a_failed_send_ends_the_turn() {
        let mut state = turn_in_flight();
        super::handle_bridge_event(
            crate::llm_bridge::BridgeEvent::TurnSendFailed("daemon unreachable".into()),
            &mut state,
        );
        assert!(state.turn.is_none());
        assert!(state.chat.entries().iter().any(|e| matches!(
            e,
            crate::state::ChatEntry::Assistant { content, .. } if content.contains("daemon unreachable")
        )));
    }

    /// The `!` path must actually reach the reporter: a window that reports
    /// nothing is exactly the invisible local command this was built to retire
    /// (SPEC R-DAEMON.9).
    ///
    /// Asserted through the window rather than the socket because the window is
    /// where the two halves have to agree — the card the user is looking at and
    /// the operation the rest of the machine sees are the same command, and the
    /// id is what says so.
    #[tokio::test]
    async fn a_bang_command_is_reported_and_its_window_carries_the_operation_id() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process (SPEC R-ISO.1).
        unsafe { std::env::set_var("AHMA_DAEMON_SOCK", dir.path().join("d.sock")) };

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.workspace = dir.path().display().to_string();

        // Without a reporter the command still runs; it is simply unreported.
        run_unsandboxed_command("echo one".to_string(), &mut state);
        assert_eq!(state.windows.len(), 1);
        assert!(
            state.windows[0].op_id.is_none(),
            "no reporter, no operation id to claim"
        );

        state.tui_reporter = Some(crate::tui_reporter::spawn_tui_reporter(
            state.workspace.clone(),
        ));
        run_unsandboxed_command("echo two".to_string(), &mut state);
        let w = state.windows.last().expect("the second window");
        let op_id = w.op_id.clone().expect("a reported command has an id");
        assert!(
            op_id.starts_with("tui_"),
            "the id must name where the work happened: {op_id}"
        );
        assert!(w.is_cli, "and it is still an ordinary CLI window");
        assert_ne!(
            state.windows[0].op_id, state.windows[1].op_id,
            "two commands are two operations"
        );
    }

    /// Before the sandbox locks, `sandbox_status` holds the initial `"UNKNOWN"`.
    /// The guard meant to suppress that from the model's system prompt compared
    /// against lowercase `"unknown"`, which no producer ever emits — so the
    /// guard was dead and every pre-lock turn told the model
    /// `Sandbox: UNKNOWN`, which is noise at best and misleading at worst.
    #[test]
    fn system_prompt_omits_the_sandbox_line_until_the_state_is_known() {
        let state = crate::state::AppState::new("http://localhost:0", "test", true);
        let prompt = super::build_system_prompt(&state);
        assert!(
            !prompt.contains("Sandbox: UNKNOWN"),
            "an unknown sandbox state must not be announced to the model: {prompt}"
        );
    }

    /// A sandbox denial is a first-class outcome, not a flavour of success
    /// (SPEC R24.7, R-PERM.7). `format_friendly_end` matched three statuses and
    /// sent everything else to `_ => "Finished"`, so a denied operation's card
    /// read "Finished in 12ms" — the single worst thing it could say about a
    /// refused write. The refused path must appear too, or the row does not tell
    /// the user what to do about it.
    #[test]
    fn friendly_end_line_names_a_denial_instead_of_calling_it_finished() {
        use crate::state::{OpStatus, Operation};
        let mut op = Operation::new("op-1", "run_terminal_command", OpStatus::Denied);
        op.duration_ms = Some(12);
        op.denial = Some((
            "/etc/hosts".to_string(),
            ahma_common::config::ScopeAccess::Rw,
        ));

        let line = super::format_friendly_end(&op);

        assert!(
            !line.contains("Finished"),
            "a denial must not be reported as a finish: {line}"
        );
        assert!(
            line.to_lowercase().contains("denied"),
            "the line must say it was denied: {line}"
        );
        assert!(
            line.contains("/etc/hosts"),
            "the line must name the refused path: {line}"
        );
    }

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
        let _home = crate::HOME_SEAM_GUARD.lock();
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
        let _home = crate::HOME_SEAM_GUARD.lock();
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
            skills_summary: "Skills: /demo\n".to_string(),
            active_skills: "<active_skill name=\"demo\">".to_string(),
            recent_ops: "OPS-HEAVY\n".to_string(),
            recent_failures: "FAILS-HEAVY\n".to_string(),
        };

        // The default composer includes the full live context.
        let full = FullComposer.compose(&parts);
        assert!(full.contains("OPS-HEAVY") && full.contains("FAILS-HEAVY"));
        assert!(full.contains("Workspace: /w") && full.contains("BASE"));
        assert!(full.contains("Skills: /demo") && full.contains("<active_skill name=\"demo\">"));

        // The minimal composer drops the token-heavy ops/failures, but keeps the
        // cheap workspace/sandbox lines, skills, and the agentic base.
        let minimal = MinimalComposer.compose(&parts);
        assert!(!minimal.contains("OPS-HEAVY") && !minimal.contains("FAILS-HEAVY"));
        assert!(minimal.contains("Workspace: /w") && minimal.contains("Sandbox: on"));
        assert!(
            minimal.contains("Skills: /demo") && minimal.contains("<active_skill name=\"demo\">")
        );
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
        use ahma_core::agent::{builtin_non_mutating_tool_names, needs_approval};
        let non_mutating = builtin_non_mutating_tool_names();

        // Gating write_file and replace_in_file by default
        assert!(needs_approval("write_file", false, &non_mutating));
        assert!(needs_approval("replace_in_file", false, &non_mutating));
        assert!(needs_approval("srv::write_file", false, &non_mutating));
        assert!(needs_approval("srv::replace_in_file", false, &non_mutating));
        // Not gating read_file by default
        assert!(!needs_approval("read_file", false, &non_mutating));
        assert!(!needs_approval("srv::read_file", false, &non_mutating));

        // When tool_approval is enabled, all tools need approval
        assert!(needs_approval("read_file", true, &non_mutating));
        assert!(needs_approval("srv::list_dir", true, &non_mutating));
    }

    /// Two clients each ran an `op-1`. Pinning or opening one must touch
    /// that one, and live output for one must not land in the other.
    #[test]
    fn same_op_id_on_two_instances_stays_separate() {
        use crate::state::{ClickTarget, OpKey, OpStatus, Operation};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        let mut a = Operation::new("op-1", "cargo", OpStatus::Running);
        a.instance_id = Some("A".into());
        let mut b = Operation::new("op-1", "cargo", OpStatus::Running);
        b.instance_id = Some("B".into());
        state.operations.push(a);
        state.operations.push(b);

        let key_b = OpKey::of(&state.operations[1]);
        super::handle_click_target(ClickTarget::PinOperation(key_b.clone()), &mut state);
        assert!(!state.operations[0].pinned);
        assert!(state.operations[1].pinned);

        super::handle_click_target(ClickTarget::OpenOperationDetail(key_b.clone()), &mut state);
        assert_eq!(state.detail_op_key(), Some(key_b));

        super::handle_operation_output(&mut state, Some("B".into()), "op-1", "from B".into());
        assert!(
            state.operations[0]
                .stdout_tail
                .iter()
                .all(|l| l != "from B")
        );
        assert!(
            state.operations[1]
                .stdout_tail
                .iter()
                .any(|l| l == "from B")
        );

        // Output from a third instance whose op is not materialised yet must
        // not be spliced into A's or B's op.
        super::handle_operation_output(&mut state, Some("C".into()), "op-1", "from C".into());
        assert!(
            state
                .operations
                .iter()
                .all(|o| o.stdout_tail.iter().all(|l| l != "from C"))
        );
    }

    /// `/async` and `/sync` are the discoverable way to switch the mode, and
    /// they persist it to `~/.ahma/settings.toml` like the settings panel does.
    #[test]
    fn slash_async_and_sync_persist_the_execution_mode() {
        use ahma_common::config::{AhmaSettings, ExecutionPolicy};
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::HOME_SEAM_GUARD.lock();
        // SAFETY: debug-only test seam; nextest isolates each test in its own process.
        unsafe { std::env::set_var("AHMA_TEST_HOME", dir.path()) };

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        assert!(super::handle_basic_nav_command("/async", &mut state));
        assert_eq!(
            AhmaSettings::load().tools.execution_mode,
            ExecutionPolicy::Async
        );
        assert_eq!(
            state.settings_editor.settings().tools.execution_mode,
            ExecutionPolicy::Async,
            "the settings panel shows the new mode"
        );

        assert!(super::handle_basic_nav_command("/sync", &mut state));
        assert_eq!(
            AhmaSettings::load().tools.execution_mode,
            ExecutionPolicy::Sync
        );

        unsafe { std::env::remove_var("AHMA_TEST_HOME") };
    }

    fn press(state: &mut AppState, code: crossterm::event::KeyCode) {
        super::handle_chat_input_key(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            state,
        );
    }

    /// ↑ on the first line recalls earlier input, ↓ walks back and restores
    /// what was being typed — the shell behaviour everyone expects.
    #[test]
    fn up_and_down_recall_earlier_input() {
        use crossterm::event::KeyCode;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = crate::state::Focus::Chat;
        for msg in ["first", "second"] {
            state.chat_input.insert_str(msg);
            super::submit_chat_input(&mut state);
        }
        state.chat_input.insert_str("draft");

        press(&mut state, KeyCode::Up);
        assert_eq!(state.chat_input_text(), "second");
        press(&mut state, KeyCode::Up);
        assert_eq!(state.chat_input_text(), "first");
        press(&mut state, KeyCode::Up);
        assert_eq!(state.chat_input_text(), "first", "stops at the oldest");
        press(&mut state, KeyCode::Down);
        assert_eq!(state.chat_input_text(), "second");
        press(&mut state, KeyCode::Down);
        assert_eq!(
            state.chat_input_text(),
            "draft",
            "back to what was being typed"
        );
    }

    /// A message that happens to look like `x5` is a message; closing window
    /// 5 is `/x5`. The bare form silently swallowed what the user typed.
    #[test]
    fn a_message_like_x5_is_not_swallowed_as_a_window_command() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = crate::state::Focus::Chat;
        state.chat_input.insert_str("x5");
        let before = state.chat.len();
        super::submit_chat_input(&mut state);
        assert!(
            state.chat.len() > before,
            "the input was answered, not dropped"
        );
    }

    /// Every chat-input key in the help table is actually handled there.
    #[test]
    fn every_documented_chat_key_is_handled() {
        use crate::keymap::{KeyScope, key_docs};
        for doc in key_docs(KeyScope::Chat) {
            for &(code, mods) in doc.keys {
                let mut state = AppState::new("http://localhost:3000", "HTTP", true);
                state.focus = crate::state::Focus::Chat;
                // ↑/↓ recall needs something to recall (and ↓ a place in it);
                // without history they fall through to cursor movement.
                state.remember_input("earlier");
                if code == crossterm::event::KeyCode::Down {
                    state.recall_previous_input();
                }
                let key = crossterm::event::KeyEvent::new(code, mods);
                assert!(
                    super::handle_chat_input_key(key, &mut state),
                    "`{}` ({}) is documented for the chat input but not handled",
                    doc.label,
                    doc.action
                );
            }
        }
    }

    fn say(state: &mut AppState, text: &str) {
        state.chat.push(crate::state::ChatEntry::User {
            text: text.into(),
            payload: None,
            started_at: None,
            duration_ms: Some(1),
        });
    }

    /// Each window keeps its own conversation: switching windows used to
    /// carry one transcript over and send it to another instance and model.
    #[test]
    fn each_window_keeps_its_own_transcript() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.llm_selection = Some(crate::state::LlmSelection::named("p", "m"));
        super::activate_window_chat("win-a", &mut state);
        say(&mut state, "for A");

        super::activate_window_chat("win-b", &mut state);
        assert!(
            state.chat.is_empty(),
            "B starts with its own, empty transcript"
        );
        say(&mut state, "for B");

        super::activate_window_chat("win-a", &mut state);
        let texts: Vec<_> = state
            .chat
            .entries()
            .iter()
            .filter_map(|e| match e {
                crate::state::ChatEntry::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["for A"]);
    }

    /// A running turn streams into the current transcript, so the target
    /// window cannot change under it.
    /// `/resume` brings back the window's saved conversation.
    #[test]
    fn resume_restores_the_windows_saved_conversation() {
        let workspace = tempfile::tempdir().unwrap();
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        say(&mut state, "earlier question");
        let path = super::transcript_path(&state, workspace.path());
        crate::transcripts::save(&path, &crate::transcripts::to_saved(&state.chat)).unwrap();

        state.chat.clear();
        super::resume_transcript(&mut state, workspace.path());
        assert!(state.chat.entries().iter().any(|e| matches!(
            e,
            crate::state::ChatEntry::User { text, .. } if text == "earlier question"
        )));

        // A window with nothing saved says so rather than failing silently.
        let empty = tempfile::tempdir().unwrap();
        state.chat.clear();
        super::resume_transcript(&mut state, empty.path());
        assert!(state.chat.entries().iter().any(|e| matches!(
            e,
            crate::state::ChatEntry::Assistant { content, .. } if content.contains("No saved conversation")
        )));
    }

    #[test]
    fn switching_windows_waits_for_the_running_turn() {
        let mut state = turn_in_flight();
        state.llm_selection = Some(crate::state::LlmSelection::named("p", "m"));
        state.active_target_instance = None;
        super::activate_window_chat("win-b", &mut state);
        assert_eq!(state.active_target_instance, None, "stayed put");
        assert!(
            !state.chat.is_empty(),
            "the running conversation is still shown"
        );
        assert!(
            state
                .footer_hint
                .as_ref()
                .is_some_and(|(h, _)| h.contains("Esc"))
        );
    }

    #[test]
    fn an_unrequested_model_refresh_does_not_open_the_picker() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        // A provider that is not in the discovered list: its models only
        // arrive through the refresh.
        state.current_provider_url = Some("http://p/v1".into());
        super::handle_model_refreshed("http://p/v1".into(), vec!["m1".into()], &mut state);
        assert_eq!(state.available_models, vec!["m1".to_string()]);
        assert!(matches!(state.modal, crate::state::ModalState::None));

        state.model_picker_requested = true;
        super::handle_model_refreshed("http://p/v1".into(), vec!["m1".into()], &mut state);
        let crate::state::ModalState::ModelPicker(picker) = &state.modal else {
            panic!("a requested refresh opens the picker");
        };
        assert_eq!(picker.selected_item(), Some("m1"));
        assert!(!state.model_picker_requested);
    }

    /// Typing a word that starts with `a` must not persist "always allow":
    /// while the chat input holds text, gate keys are text.
    #[test]
    fn approval_keys_are_text_while_typing() {
        use crate::state::{AppState, ApprovalGate, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;
        state.request_approval(ApprovalGate::new("op_1", "write_file", "w"), None);
        state.chat_input.insert_str("h");

        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(!super::handle_approval_key(a, &mut state));
        assert!(state.approval.is_some(), "still pending");

        // With the input empty the user is not typing: the key answers.
        state.clear_chat_input();
        let n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(super::handle_approval_key(n, &mut state));
        assert!(state.approval.is_none());
    }

    /// Enter while typing sends the message; it must not deny a scope grant.
    #[test]
    fn enter_while_typing_does_not_answer_a_scope_grant() {
        use crate::state::{AppState, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;
        state.scope_grant = Some(crate::state::ScopeGrantGate::from_request(
            ahma_common::scope_grant::ScopeGrantRequest {
                decision_id: "d1".into(),
                path: std::path::PathBuf::from("x"),
                access: ahma_common::config::ScopeAccess::Rw,
                reason: ahma_common::scope_grant::GrantReason::PreExecViolation,
                tool: Some("write_file".into()),
            },
        ));
        state.chat_input.insert_str("hello");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!super::handle_scope_grant_key(enter, &mut state));
        assert!(state.scope_grant.is_some());
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

    /// A dropped connection before any answer is retried once, visibly; a
    /// second drop is reported with what to do next, and never loops.
    #[test]
    fn a_dropped_turn_is_retried_once_then_explained() {
        use crate::state::{AppState, ChatEntry, ChatTurn, LlmSelection};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.llm_selection = Some(LlmSelection::named("Ollama", "qwen3.8:27b"));
        state.current_provider_url = Some("http://localhost:11434/v1".into());
        state.turn = Some(ChatTurn::new(None));

        let notices = |state: &AppState| {
            state
                .chat
                .entries()
                .iter()
                .filter_map(|e| match e {
                    ChatEntry::Notice { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        super::end_turn_with_error(&mut state, "error sending request: connection reset");
        assert!(state.turn.is_some(), "the message is sent again");
        assert!(notices(&state)[0].contains("trying again now"));

        super::end_turn_with_error(&mut state, "error sending request: connection reset");
        assert!(state.turn.is_none(), "one retry per message");
        assert!(notices(&state)[1].contains("send it again"));

        // The notices are for the user, never context for the model.
        assert!(
            super::collect_chat_history(&state)
                .iter()
                .all(|m| !m.content.contains("trying again"))
        );
    }

    /// A client's own model goes away with its client: chat moves to the most
    /// recent model ahma runs itself, says so, and moves back when the client
    /// returns — unless the user picked something else in between.
    #[test]
    fn chat_follows_a_client_model_away_and_back() {
        use crate::state::{AppState, ChatEntry, LlmSelection};
        let ollama = ahma_llm_monitor::LocalProvider {
            name: "Ollama".into(),
            base_url: "http://localhost:11434/v1".into(),
            models: vec!["qwen3.8".into()],
        };
        let claude = ahma_llm_monitor::LocalProvider {
            name: "claude-code (its own model)".into(),
            base_url: "mcp://claude-code".into(),
            models: vec!["client's choice".into()],
        };
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.recent_llms = vec![crate::session_config::WindowLlmConfig {
            provider: "Ollama".into(),
            model: "qwen3.8".into(),
            provider_url: Some(ollama.base_url.clone()),
        }];
        let client_sel = LlmSelection::named("claude-code (its own model)", "client's choice");
        state.llm_selection = Some(client_sel.clone());
        state.current_provider_url = Some(claude.base_url.clone());

        // Client disconnects.
        state.available_providers = vec![ollama.clone()];
        super::follow_client_model_availability(&mut state);
        assert_eq!(
            state.current_provider_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        let last_notice = |s: &AppState| match s.chat.entries().back() {
            Some(ChatEntry::Notice { text }) => text.clone(),
            other => panic!("expected a notice, got {other:?}"),
        };
        assert!(last_notice(&state).contains("switched to"));

        // Client returns: back to it, announced.
        state.available_providers = vec![ollama.clone(), claude.clone()];
        super::follow_client_model_availability(&mut state);
        assert_eq!(state.llm_selection.as_ref(), Some(&client_sel));
        assert!(last_notice(&state).contains("switched back"));

        // Gone again, but this time the user picks a model before it returns.
        state.available_providers = vec![ollama.clone()];
        super::follow_client_model_availability(&mut state);
        state.llm_selection = Some(LlmSelection::named("Ollama", "other-model"));
        state.available_providers = vec![ollama, claude];
        super::follow_client_model_availability(&mut state);
        assert_eq!(state.llm_selection.as_ref().unwrap().model, "other-model");
    }

    #[test]
    fn request_errors_are_not_retried() {
        assert!(!super::is_transient_turn_error(
            "HTTP 400: model does not support tools"
        ));
        assert!(!super::is_transient_turn_error("cancelled by user"));
        assert!(super::is_transient_turn_error("llm: operation timed out"));
    }

    /// The trust question: Enter/Esc/`n` never trust; only `y` does, and it
    /// persists so the next start does not ask again (SPEC R-PERM.1.3).
    #[test]
    fn trust_prompt_only_y_trusts_and_it_persists() {
        use crate::state::AppState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let home = tempfile::tempdir().unwrap();
        let _home = crate::HOME_SEAM_GUARD.lock();
        // SAFETY: nextest runs this test in its own process; set before any read.
        unsafe { std::env::set_var("AHMA_TEST_HOME", home.path()) };
        let project = home.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.workspace = dunce::canonicalize(&project)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        super::offer_folder_trust(&mut state);
        assert!(state.trust_prompt.is_some(), "a new folder is asked about");
        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('n')] {
            super::offer_folder_trust(&mut state);
            assert!(super::handle_trust_key(
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut state
            ));
            assert!(state.trust_prompt.is_none());
            assert!(
                !ahma_core::approvals::is_workspace_trusted(&project),
                "{code:?} must not trust"
            );
        }

        super::offer_folder_trust(&mut state);
        assert!(super::handle_trust_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut state
        ));
        assert!(ahma_core::approvals::is_workspace_trusted(&project));

        super::offer_folder_trust(&mut state);
        assert!(
            state.trust_prompt.is_none(),
            "a trusted folder is not asked again"
        );

        // Home itself is never offered.
        state.workspace = home.path().to_string_lossy().into_owned();
        super::offer_folder_trust(&mut state);
        assert!(state.trust_prompt.is_none(), "home is too broad to trust");
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

    /// `?` on an empty chat input opens the help overlay — the default screen
    /// state has both sub-windows closed, so the global `?` binding is
    /// unreachable and this path is the only working route to help by key.
    #[test]
    fn question_mark_on_empty_input_opens_help() {
        use crate::state::{AppState, Focus, ModalState};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;
        let handled = super::handle_chat_input_key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(handled);
        assert!(matches!(state.modal, ModalState::Help));

        // With text in the input, `?` is ordinary typing, not a shortcut: the
        // editor consumes it and no modal opens.
        state.modal = ModalState::None;
        state.chat_input.insert_str("what");
        super::handle_chat_input_key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(matches!(state.modal, ModalState::None));
        assert_eq!(
            state.chat_input.lines(),
            ["what?"],
            "mid-sentence ? must be typed into the editor"
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

        // Start with Focus::Work
        state.focus = Focus::Work;

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

        // Closing a window is the `/x26` command; a bare `x26` is a message.
        state.chat_input.insert_str("/x26");
        super::submit_chat_input(&mut state);

        assert!(!state.windows[0].visible);
        assert_eq!(
            state.windows[0].status,
            crate::state::WindowStatus::Cancelled
        );

        // Restore window
        state.windows[0].visible = true;
        state.windows[0].status = crate::state::WindowStatus::Running;

        // A bare "X26" is a message, not a command: the window is untouched.
        state.chat_input.insert_str("X26");
        super::submit_chat_input(&mut state);

        assert!(state.windows[0].visible);
        assert_eq!(state.windows[0].status, crate::state::WindowStatus::Running);
    }

    /// Only the log pane zooms now: the work view *is* the screen, so there is
    /// nothing to zoom it out of (SPEC R24.9).
    #[test]
    fn only_the_log_pane_zooms() {
        use crate::keymap::Action;
        use crate::state::{AppState, Focus};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);

        state.focus = Focus::Work;
        super::handle_action(Action::ToggleZoom, &mut state);
        assert_eq!(state.zoomed, None, "the work view fills the screen already");

        state.focus = Focus::Log;
        super::handle_action(Action::ToggleZoom, &mut state);
        assert_eq!(state.zoomed, Some(Focus::Log));

        // Esc unzooms first, leaving focus where it was.
        super::handle_action(Action::FocusChat, &mut state);
        assert_eq!(state.zoomed, None);
        assert_eq!(state.focus, Focus::Log);
    }

    /// The wheel scrolls the work view, and stops the next frame from pulling
    /// it back to the cursor — without which the wheel appears not to work.
    #[test]
    fn the_wheel_scrolls_the_work_view_and_detaches_selection_follow() {
        use crate::state::AppState;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state
            .work_area
            .set(ratatui::layout::Rect::new(0, 0, 80, 10));
        state.work_total_rows.set(60);
        assert!(state.work_follow_selection.get());

        super::handle_mouse_scroll(5, 5, false, &mut state);
        assert_eq!(state.ops_scroll.get(), 3, "scrolls down by a wheel notch");
        assert!(
            !state.work_follow_selection.get(),
            "and the next frame must not pull the view back to the cursor"
        );

        super::handle_mouse_scroll(5, 5, true, &mut state);
        assert_eq!(state.ops_scroll.get(), 0, "and back up");

        // A keyboard move re-engages following.
        state.operations.push(crate::state::Operation::new(
            "op-1",
            "run_terminal_command",
            crate::state::OpStatus::Running,
        ));
        state.rebuild_work_view(0);
        super::scroll_focus_down(&mut state);
        assert!(
            state.work_follow_selection.get(),
            "moving the cursor should bring the view with it"
        );
    }

    /// `i` opens chat from the work view; in chat it is a typed character.
    #[test]
    fn i_opens_chat_from_the_work_view() {
        use crate::keymap::{Action, map_key};
        use crate::state::{AppState, Focus, ModalState};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let key = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(
            map_key(key, Focus::Work, &ModalState::None, false),
            Action::ToggleChat
        );
        assert_eq!(
            map_key(key, Focus::Chat, &ModalState::None, false),
            Action::InputChar('i'),
            "in chat it is just a letter"
        );

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        super::handle_action(Action::ToggleChat, &mut state);
        assert!(state.chat_open);
        assert_eq!(state.focus, Focus::Chat);
    }

    /// The work view is always on screen, so `/tasks` focuses it — and `/chat`
    /// is the toggle now, because chat is the thing you choose to do
    /// (SPEC R24.9).
    #[test]
    fn chat_is_the_toggle_and_tasks_focuses_the_work_view() {
        use crate::state::{AppState, Focus};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        assert_eq!(state.focus, Focus::Work, "the TUI opens on the work view");
        assert!(!state.chat_open, "and chat starts closed");

        assert!(super::handle_basic_nav_command("/chat", &mut state));
        assert!(state.chat_open);
        assert_eq!(state.focus, Focus::Chat);

        assert!(super::handle_basic_nav_command("/tasks", &mut state));
        assert_eq!(state.focus, Focus::Work, "/tasks focuses the work view");
        assert!(state.chat_open, "without closing chat");

        assert!(super::handle_basic_nav_command("/chat", &mut state));
        assert!(!state.chat_open);
        assert_eq!(state.focus, Focus::Work, "closing chat returns focus");
    }

    /// Startup notices reach the screen: everything into the log pane, and
    /// warnings additionally into the chat transcript — the pane that is
    /// actually open by default. Previously these were `tracing::warn!` into a
    /// log file the user was never told about.
    #[test]
    fn startup_notices_surface_in_log_and_chat() {
        use crate::startup_notices::{Level, TEST_GUARD, drain, push};
        use crate::state::AppState;

        let _guard = TEST_GUARD.lock();
        let _ = drain();
        push(Level::Info, "quietly reused the running server");
        push(Level::Warn, "Restarted the running ahma server");

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        super::drain_startup_notices(&mut state);

        assert_eq!(state.log.len(), 2, "both notices reach the log pane");
        let chat: String = format!("{:?}", state.chat.entries());
        assert!(
            chat.contains("Restarted the running ahma server"),
            "a warning must also be visible in chat: {chat}"
        );
        assert!(
            !chat.contains("quietly reused"),
            "info-level notices stay in the log pane"
        );
    }

    /// `/scope` toggles the sandbox-scope window without stealing focus — it is
    /// informational (SPEC R5.4(b): the persistent TUI scope panel).
    #[test]
    fn scope_command_toggles_scope_window() {
        use crate::state::{AppState, Focus};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        assert!(super::handle_basic_nav_command("/scope", &mut state));
        assert!(state.scope_window_open);
        assert_eq!(
            state.focus,
            Focus::Work,
            "focus stays where it was — the work view is where the TUI opens"
        );
        assert!(super::handle_basic_nav_command("/scope", &mut state));
        assert!(!state.scope_window_open);
    }

    /// A `sandbox/failed` event keeps its reason on state (for the /scope
    /// window) and pushes an actionable error into the log pane; the next
    /// successful configuration clears it.
    #[test]
    fn sandbox_failed_reason_is_kept_until_configured() {
        use crate::mcp_source::SourceEvent;
        use crate::state::AppState;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        super::handle_source_event(
            SourceEvent::SandboxFailed {
                error: "no usable roots".into(),
            },
            &mut state,
        );
        assert_eq!(state.sandbox_status, crate::state::SandboxAuthority::Failed);
        assert_eq!(
            state.sandbox_failed_reason.as_deref(),
            Some("no usable roots")
        );
        assert!(
            state
                .log
                .iter()
                .any(|e| e.message.contains("no usable roots")),
            "failure reason must reach the log pane"
        );
        super::handle_source_event(
            SourceEvent::SandboxScope {
                scope: crate::state::SandboxScopeInfo {
                    write: vec!["/w".into()],
                    ..Default::default()
                },
            },
            &mut state,
        );
        assert!(state.sandbox_failed_reason.is_none());
        assert_eq!(state.locked_scope_root(), Some("/w"));
    }

    /// Clicking a log row opens that line full-screen, wrapped, so a line wider
    /// than the pane can actually be read. The click also focuses the log pane,
    /// so Esc returns the user to what they were reading.
    #[tokio::test]
    async fn log_row_click_opens_the_line_overlay() {
        use crate::state::{AppState, ClickTarget, Focus, ModalState};
        use ratatui::layout::Rect;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Chat;

        let text = "pid=79950 role=bridge INFO serve_inner: a very long message";
        state.click_targets.borrow_mut().push((
            ClickTarget::OpenLogLine(text.into()),
            Rect::new(0, 4, 60, 1),
        ));

        super::handle_mouse_click(10, 4, &mut state);
        match &state.modal {
            ModalState::LogLineDetail(d) => {
                assert_eq!(d.text, text);
                assert_eq!(d.scroll, 0);
            }
            other => panic!("expected the log-line overlay, got {other:?}"),
        }
        assert_eq!(state.focus, Focus::Log);
    }

    /// Clicking a registered `SectionHeader` target dispatches all the way
    /// through to `toggle_section`, the same accordion open/close a keyboard
    /// `Enter`/`Space` on the header performs. This is the end-to-end
    /// coverage `clicking_a_header_is_a_registered_target` (ui/work.rs) was
    /// missing: that test only checks the target is *registered*, not that a
    /// simulated click actually opens the section.
    #[tokio::test]
    async fn clicking_a_section_header_opens_it() {
        use crate::state::{AppState, ClickTarget, Focus, LlmSelection};
        use ratatui::layout::Rect;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.focus = Focus::Log;
        state.llm_selection = Some(LlmSelection::named("Ollama", "qwen2.5-coder:32b"));

        state.click_targets.borrow_mut().push((
            ClickTarget::SectionHeader("i1".into()),
            Rect::new(0, 2, 60, 1),
        ));

        assert_eq!(state.open_section, None);
        super::handle_mouse_click(10, 2, &mut state);
        assert_eq!(state.open_section.as_deref(), Some("i1"));
        assert_eq!(state.focus, Focus::Chat);

        // Clicking the same (now open) header again closes it.
        super::handle_mouse_click(10, 2, &mut state);
        assert_eq!(state.open_section, None);
        assert_eq!(state.focus, Focus::Work);
    }

    /// The overlay scroll keys drive whichever overlay is open — the log-line
    /// one shares `detail_max_scroll` with the operation overlay.
    #[tokio::test]
    async fn log_line_overlay_scrolls_and_clamps() {
        use crate::keymap::Action;
        use crate::state::{AppState, ModalState};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.open_log_line_detail("a long line".into());
        state.detail_max_scroll.set(3);

        let scroll = |s: &crate::state::AppState| match &s.modal {
            ModalState::LogLineDetail(d) => d.scroll,
            other => panic!("expected the log-line overlay, got {other:?}"),
        };

        super::scroll_detail_overlay(&Action::Down, &mut state);
        assert_eq!(scroll(&state), 1);
        super::scroll_detail_overlay(&Action::Bottom, &mut state);
        assert_eq!(scroll(&state), 3);
        super::scroll_detail_overlay(&Action::Down, &mut state);
        assert_eq!(scroll(&state), 3, "must clamp at the last row");
        super::scroll_detail_overlay(&Action::Top, &mut state);
        assert_eq!(scroll(&state), 0);
        super::scroll_detail_overlay(&Action::Up, &mut state);
        assert_eq!(scroll(&state), 0, "must clamp at the first row");
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

    #[test]
    fn test_activate_window_chat_with_saved_llm() {
        use crate::session_config::WindowLlmConfig;
        use crate::state::{AppState, Focus};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.set_window_llm(
            "claude-code",
            WindowLlmConfig {
                provider: "Ollama".to_string(),
                model: "qwen2.5-coder:32b".to_string(),
                provider_url: Some("http://localhost:11434".to_string()),
            },
        );

        super::activate_window_chat("claude-code", &mut state);

        assert_eq!(state.active_target_instance.as_deref(), Some("claude-code"));
        assert!(state.chat_open);
        assert_eq!(state.focus, Focus::Chat);
        let sel = state.llm_selection.as_ref().unwrap();
        assert_eq!(sel.persistable_provider(), "Ollama");
        assert_eq!(sel.model, "qwen2.5-coder:32b");
    }

    #[test]
    fn test_activate_window_chat_triggers_wizard_when_no_llm() {
        use crate::state::{AppState, ModalState};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.llm_selection = None;
        state.window_llms.clear();

        super::activate_window_chat("claude-code", &mut state);

        assert_eq!(state.active_target_instance.as_deref(), Some("claude-code"));
        assert!(matches!(state.modal, ModalState::LlmSetupProvider(_)));
    }

    fn wizard_pick(state: &mut AppState, prefix: &str) {
        let crate::state::ModalState::LlmSetupProvider(mut picker) =
            std::mem::take(&mut state.modal)
        else {
            panic!("expected the provider step");
        };
        picker.select_prefix(prefix);
        super::submit_llm_setup_provider(picker, state);
    }

    /// Ollama end to end: the model list is the connection test, step 2 offers
    /// what the server actually has, and the saved URL carries `/v1` (without
    /// it every request 404'd).
    #[test]
    fn setup_wizard_connects_then_offers_the_servers_models() {
        use crate::state::{Focus, ModalState};
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.active_target_instance = Some("claude-code".to_string());
        state.llm_selection = None;

        super::start_llm_setup_wizard(&mut state);
        wizard_pick(&mut state, super::SETUP_OLLAMA);
        let pending = state.setup_pending.clone().expect("connecting");
        assert_eq!(pending.base_url, "http://localhost:11434/v1");

        super::handle_model_refreshed(
            pending.base_url.clone(),
            vec!["qwen2.5-coder:32b".into(), "llama3.3".into()],
            &mut state,
        );
        let ModalState::LlmSetupModel {
            mut picker,
            provider_name,
            base_url,
        } = std::mem::take(&mut state.modal)
        else {
            panic!("expected the model step");
        };
        assert_eq!(picker.items, vec!["qwen2.5-coder:32b", "llama3.3"]);
        picker.select_exact("qwen2.5-coder:32b");
        super::submit_llm_setup_model(picker, provider_name, base_url, &mut state);

        assert!(matches!(state.modal, ModalState::None));
        assert!(state.chat_open);
        assert_eq!(state.focus, Focus::Chat);
        let sel = state.llm_selection.as_ref().unwrap();
        assert_eq!(sel.persistable_provider(), "Ollama");
        assert_eq!(sel.model, "qwen2.5-coder:32b");
        assert_eq!(
            state.current_provider_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        let win_cfg = state.get_window_llm("claude-code").unwrap();
        assert_eq!(win_cfg.model, "qwen2.5-coder:32b");
    }

    /// A server that lists nothing is not silently "connected": the wizard
    /// says what to fix and saves nothing.
    #[test]
    fn setup_wizard_reports_an_unreachable_provider() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.llm_selection = None;
        super::start_llm_setup_wizard(&mut state);
        wizard_pick(&mut state, super::SETUP_OLLAMA);
        super::handle_model_refreshed("http://localhost:11434/v1".into(), vec![], &mut state);

        assert!(state.llm_selection.is_none(), "nothing chosen");
        assert!(state.setup_pending.is_none());
        assert!(state.chat.entries().iter().any(|e| matches!(
            e,
            crate::state::ChatEntry::Assistant { content, .. } if content.contains("ollama serve")
        )));
    }

    /// A cloud provider without its key in the environment stops with how to
    /// set it — and never asks for the key itself.
    #[test]
    fn setup_wizard_asks_for_the_key_in_the_environment() {
        // SAFETY: nextest runs each test in its own process (SPEC R-ISO.1).
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        super::start_llm_setup_wizard(&mut state);
        wizard_pick(&mut state, super::SETUP_OPENAI);
        assert!(state.setup_pending.is_none());
        assert!(state.chat.entries().iter().any(|e| matches!(
            e,
            crate::state::ChatEntry::Assistant { content, .. } if content.contains("OPENAI_API_KEY")
        )));
    }

    /// Only clients that declared MCP sampling are offered as a provider; the
    /// old list invented one for every window, so the wizard never ran.
    #[test]
    fn sampling_is_offered_only_for_clients_that_declare_it() {
        let inst = |sampling: bool| ahma_common::daemon_hub::InstanceInfo {
            id: "i".into(),
            mode: "stdio".into(),
            label: "ahma".into(),
            client: Some("vscode".into()),
            sampling,
            ..Default::default()
        };
        assert!(super::virtual_provider_for_instance(&inst(false)).is_none());
        let p = super::virtual_provider_for_instance(&inst(true)).expect("declared sampling");
        assert_eq!(p.name, "vscode (its own model)");
        assert!(
            !p.models
                .iter()
                .any(|m| m.contains("Claude") || m.contains("GPT"))
        );
    }

    #[test]
    fn test_esc_in_empty_chat_input_unfocuses_to_work() {
        use crate::state::{AppState, Focus};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.chat_open = true;
        state.focus = Focus::Chat;
        state.clear_chat_input();

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let handled = super::handle_chat_input_key(key, &mut state);
        assert!(handled);
        assert_eq!(state.focus, Focus::Work);
    }

    /// SPEC R-SK8: `/name [args]` invokes a discovered Agent Skill — the pane
    /// shows the typed command while the LLM payload carries the SKILL.md body.
    mod skills {
        use crate::state::{AppState, ChatEntry};

        /// Workspace with one skill; LLM configured so invocation proceeds.
        fn skill_state(tmp: &tempfile::TempDir) -> AppState {
            let dir = tmp.path().join(".agents").join("skills").join("tui-test");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                "---\nname: tui-test\ndescription: Exercise the TUI skill runner.\n---\nAlways answer in haiku.\n",
            )
            .unwrap();

            let mut state = AppState::new("http://localhost:3000", "HTTP", true);
            state.workspace = tmp.path().to_string_lossy().into_owned();
            state.llm_selection = Some(crate::state::LlmSelection::named("ollama", "test-model"));
            state.current_provider_url = Some("http://localhost:11434/v1".to_string());
            state
        }

        #[test]
        fn slash_name_invokes_skill_with_payload() {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = skill_state(&tmp);

            let handled =
                super::super::handle_skills_nav_command("/tui-test write a poem", &mut state);
            assert!(handled);

            let entries = state.chat.entries();
            let user = entries
                .iter()
                .find_map(|e| match e {
                    ChatEntry::User { text, payload, .. } => Some((text.clone(), payload.clone())),
                    _ => None,
                })
                .expect("skill invocation must push a User entry");
            assert_eq!(
                user.0, "/tui-test write a poem",
                "pane shows the typed command"
            );
            let payload = user.1.expect("skill invocation must carry an LLM payload");
            assert!(payload.contains("Always answer in haiku."), "{payload}");
            assert!(payload.contains("write a poem"), "{payload}");
            assert!(
                matches!(
                    entries.back(),
                    Some(ChatEntry::Assistant {
                        streaming: true,
                        ..
                    })
                ),
                "a streaming assistant entry must be open"
            );
        }

        #[test]
        fn explicit_skill_form_and_missing_skill_report() {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = skill_state(&tmp);

            assert!(super::super::handle_skills_nav_command(
                "/skill tui-test",
                &mut state
            ));
            let has_payload = state.chat.entries().iter().any(|e| {
                matches!(
                    e,
                    ChatEntry::User {
                        payload: Some(_),
                        ..
                    }
                )
            });
            assert!(has_payload, "/skill <name> must invoke like /<name>");

            assert!(super::super::handle_skills_nav_command(
                "/skill no-such-skill-xyzzy",
                &mut state
            ));
            let last = state.chat.entries().back().cloned();
            let Some(ChatEntry::Assistant { content, .. }) = last else {
                panic!("missing-skill report must be an assistant message");
            };
            assert!(content.contains("no-such-skill-xyzzy"), "{content}");
        }

        #[test]
        fn unknown_or_invalid_names_fall_through() {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = skill_state(&tmp);
            assert!(!super::super::handle_skills_nav_command(
                "/no-such-skill-xyzzy",
                &mut state
            ));
            // Uppercase is invalid per the Agent Skills spec — never a skill.
            assert!(!super::super::handle_skills_nav_command(
                "/Bogus", &mut state
            ));
        }

        #[test]
        fn skills_listing_names_workspace_skill() {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = skill_state(&tmp);
            assert!(super::super::handle_skills_nav_command(
                "/skills", &mut state
            ));
            let Some(ChatEntry::Assistant { content, .. }) = state.chat.entries().back() else {
                panic!("listing must be an assistant message");
            };
            assert!(content.contains("/tui-test"), "{content}");
            assert!(
                content.contains("Exercise the TUI skill runner."),
                "{content}"
            );
        }

        #[test]
        fn non_user_invocable_skill_is_listed_but_not_invocable() {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = skill_state(&tmp);
            let dir = tmp.path().join(".agents").join("skills").join("model-only");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                "---\nname: model-only\ndescription: Not for slash use.\nuser-invocable: false\n---\nBody.\n",
            )
            .unwrap();

            assert!(
                !super::super::handle_skills_nav_command("/model-only", &mut state),
                "user-invocable: false must not be /name-invocable"
            );
            let nav = super::super::skill_nav_commands(&state);
            assert!(nav.iter().any(|c| c.command == "/tui-test"));
            assert!(!nav.iter().any(|c| c.command == "/model-only"));

            assert!(super::super::handle_skills_nav_command(
                "/skills", &mut state
            ));
            let Some(ChatEntry::Assistant { content, .. }) = state.chat.entries().back() else {
                panic!("listing must be an assistant message");
            };
            assert!(content.contains("not user-invocable"), "{content}");
        }

        #[test]
        fn collect_chat_history_prefers_payload() {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = skill_state(&tmp);
            state.chat.push(ChatEntry::User {
                text: "/tui-test".into(),
                payload: Some("full instructions".into()),
                started_at: None,
                duration_ms: None,
            });
            let history = super::super::collect_chat_history(&state);
            assert_eq!(history.last().unwrap().content, "full instructions");
        }
    }
}
