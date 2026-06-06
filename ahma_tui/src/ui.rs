//! Ratatui rendering — all panel draw functions.
//!
//! The top-level [`draw`] function dispatches to either the chat or monitor
//! layout based on `state.mode`.

#[cfg(feature = "tui")]
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

use crate::state::{AppState, ChatEntry, Focus, Mode, NavCommand};
use crate::theme::Theme;

// ─── Top-level draw ───────────────────────────────────────────────────────────

/// Called every redraw tick — the only public entry point in this module.
#[cfg(feature = "tui")]
pub fn draw(frame: &mut Frame, state: &AppState, theme: &Theme) {
    match state.mode {
        Mode::Chat => draw_chat_layout(frame, state, theme),
        Mode::Monitor => draw_monitor_layout(frame, state, theme),
    }

    // Overlays drawn on top of whichever layout is active.
    let full = frame.area();
    if state.show_help {
        draw_help(frame, theme, full);
    }
    if state.navigator.visible {
        draw_navigator(frame, state, theme, full);
    }
    if let Some(picker) = &state.provider_picker {
        draw_picker(frame, picker, theme, full);
    } else if let Some(picker) = &state.model_picker {
        draw_picker(frame, picker, theme, full);
    }
}

// ─── Chat layout ──────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
struct RenderedWindowLayout {
    orig_idx: usize,
    height: u16,
    collapsed: bool,
    visible: bool,
}

#[cfg(feature = "tui")]
fn compute_window_layouts(
    windows: &[crate::state::TuiWindow],
    max_h: u16,
) -> Vec<RenderedWindowLayout> {
    let mut layouts: Vec<RenderedWindowLayout> = windows
        .iter()
        .enumerate()
        .filter(|(_, w)| w.visible)
        .map(|(i, w)| {
            let preferred_h = if w.collapsed {
                1
            } else {
                (w.content.len() + 2).clamp(3, 8) as u16
            };
            RenderedWindowLayout {
                orig_idx: i,
                height: preferred_h,
                collapsed: w.collapsed,
                visible: true,
            }
        })
        .collect();

    // 1. Check if total height fits.
    let mut total_h: u16 = layouts.iter().map(|l| l.height).sum();
    if total_h <= max_h {
        return layouts;
    }

    // 2. Collapse expanded windows starting from the oldest.
    for l in layouts.iter_mut() {
        if total_h <= max_h {
            break;
        }
        if !l.collapsed {
            let old_h = l.height;
            l.collapsed = true;
            l.height = 1;
            total_h = total_h - old_h + 1;
        }
    }

    // 3. If it still doesn't fit, hide oldest windows.
    for l in layouts.iter_mut() {
        if total_h <= max_h {
            break;
        }
        if l.visible {
            let old_h = l.height;
            l.visible = false;
            l.height = 0;
            total_h -= old_h;
        }
    }

    layouts
}

#[cfg(feature = "tui")]
fn window_status_style(status: &str, theme: &Theme) -> Style {
    match status {
        "Running" => theme.running(),
        "Finished" => theme.success(),
        "Error" => theme.failed(),
        "Cancelled" => theme.cancelled(),
        "Pending" => theme.pending(),
        _ => theme.normal(),
    }
}

#[cfg(feature = "tui")]
fn draw_chat_layout(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let full = frame.area();
    let approval_h: u16 = if let Some(gate) = &state.approval {
        if gate.diff.is_some() { 12 } else { 3 }
    } else {
        0
    };

    // Input height: 1–6 lines depending on content, always at least 3 (borders).
    let input_lines = state.chat_input_line_count().clamp(1, 6) as u16;
    let input_h = input_lines + 2; // borders

    let [header_a, chat_a, approval_a, input_a, footer_a] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(approval_h),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .areas(full);

    draw_chat_header(frame, state, theme, header_a);

    // Clear window_rects at start of drawing
    state.window_rects.borrow_mut().clear();

    let visible_count = state.windows.iter().filter(|w| w.visible).count();
    let (history_area, windows_area, layouts) = if visible_count > 0 {
        let max_w_h = chat_a.height.saturating_sub(4);
        let layouts = compute_window_layouts(&state.windows, max_w_h);
        let total_w_h: u16 = layouts.iter().filter(|l| l.visible).map(|l| l.height).sum();
        let [h_area, w_area] =
            Layout::vertical([Constraint::Min(4), Constraint::Length(total_w_h)]).areas(chat_a);
        (h_area, w_area, layouts)
    } else {
        (chat_a, Rect::default(), vec![])
    };

    draw_chat_history(frame, state, theme, history_area);

    if windows_area.height > 0 && !layouts.is_empty() {
        let mut constraints = Vec::new();
        let mut active_layouts = Vec::new();
        for l in &layouts {
            if l.visible && l.height > 0 {
                constraints.push(Constraint::Length(l.height));
                active_layouts.push(l);
            }
        }
        let window_areas = Layout::vertical(constraints).split(windows_area);
        for (area, l) in window_areas.iter().zip(active_layouts) {
            let w = &state.windows[l.orig_idx];
            state.window_rects.borrow_mut().push((w.id, *area));

            let status_style = window_status_style(&w.status, theme);
            if l.collapsed {
                // Draw collapsed window as a single summary line
                let mut spans = vec![
                    Span::styled(" [+] ", theme.dim()),
                    Span::styled(format!("{} ", w.id), theme.normal()),
                    Span::styled(format!("[{}] ", w.status), status_style),
                    Span::styled(w.label.clone(), theme.normal()),
                ];
                let left_len: usize = spans.iter().map(|s| s.content.len()).sum();
                let right_str = format!(" X{}", w.id);
                let pad_width = (area.width as usize).saturating_sub(left_len + right_str.len());
                if pad_width > 0 {
                    spans.push(Span::raw(" ".repeat(pad_width)));
                }
                spans.push(Span::styled(right_str, theme.dim()));

                let line = Line::from(spans);
                let para = Paragraph::new(line);
                frame.render_widget(para, *area);
            } else {
                // Draw expanded window as a border block
                let border_width = 2;
                let title_space = (area.width as usize).saturating_sub(border_width);
                let title_left = format!(" [-] {} {}", w.id, w.label);
                let title_right = format!("X{} ", w.id);
                let pad_width = title_space.saturating_sub(title_left.len() + title_right.len());
                let title_combined = if pad_width > 0 {
                    format!("{}{}{}", title_left, " ".repeat(pad_width), title_right)
                } else {
                    title_left
                };

                let block = Block::default()
                    .title(Span::styled(title_combined, theme.normal()))
                    .borders(Borders::ALL)
                    .border_style(status_style);

                let inner = block.inner(*area);
                frame.render_widget(block, *area);

                let content_text: Vec<Line> = w
                    .content
                    .iter()
                    .map(|line| {
                        let style = if line.starts_with("Starting") {
                            theme.dim()
                        } else if line.starts_with("Finished successfully") {
                            theme.success()
                        } else if line.starts_with("Failed") {
                            theme.failed()
                        } else if line.starts_with("Cancelled") {
                            theme.cancelled()
                        } else if line.starts_with("──") || line.starts_with("--") {
                            theme.dim()
                        } else {
                            theme.normal()
                        };
                        Line::from(Span::styled(line.clone(), style))
                    })
                    .collect();
                let para = Paragraph::new(content_text).wrap(Wrap { trim: false });
                frame.render_widget(para, inner);
            }
        }
    }

    if state.approval.is_some() {
        draw_approval(frame, state, theme, approval_a);
    }
    draw_input_box(frame, state, theme, input_a);
    draw_chat_footer(frame, state, theme, footer_a);
}

#[cfg(feature = "tui")]
fn draw_chat_header(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let mcp_label = match (state.mcp_enabled, state.unicode) {
        (true, true) => " · MCP ✓",
        (true, false) => " · MCP on",
        (false, _) => "",
    };
    let (health_char, health_style) = match (state.server_healthy, state.unicode) {
        (true, true) => (" ● ", theme.healthy()),
        (true, false) => (" * ", theme.healthy()),
        (false, true) => (" ○ ", theme.unhealthy()),
        (false, false) => (" - ", theme.unhealthy()),
    };
    let health_span = Span::styled(health_char, health_style);
    let mut http_count = 0;
    let mut stdio_count = 0;
    for s in &state.mcp_connections.servers {
        if s.enabled {
            match &s.kind {
                crate::mcp_connections::McpServerKind::Http { .. } => http_count += 1,
                crate::mcp_connections::McpServerKind::Stdio { .. } => stdio_count += 1,
            }
        }
    }
    let external_tools = state.mcp_connections.aggregate_tool_names().len();
    let external_part = if http_count > 0 || stdio_count > 0 {
        format!(" · ext (http:{http_count} stdio:{stdio_count}) / {external_tools} tools")
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled(" ahma chat", theme.title()),
        Span::styled(format!("  {}", state.llm_label), theme.normal()),
        Span::styled(mcp_label, theme.dim()),
        Span::styled(external_part, theme.dim()),
        health_span,
        Span::styled(
            format!("{}  q quit  ? help", state.transport_label),
            theme.dim(),
        ),
    ]);

    frame.render_widget(Paragraph::new(line).style(theme.header_bar()), area);
}

#[cfg(feature = "tui")]
fn draw_chat_history(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::Chat;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.chat.is_empty() {
        let hint = chat_history_hint(state);
        frame.render_widget(Paragraph::new(Span::styled(hint, theme.dim())), inner);
        return;
    }

    let visible_h = inner.height as usize;
    let lines = build_chat_history_lines(state, theme, inner.width as usize);
    let scroll = chat_history_scroll_offset(lines.len(), visible_h, state.chat_scroll);
    let visible_lines: Vec<Line<'static>> =
        lines.into_iter().skip(scroll).take(visible_h).collect();
    frame.render_widget(
        Paragraph::new(Text::from(visible_lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(feature = "tui")]
fn chat_history_hint(state: &AppState) -> &'static str {
    if state.llm_label == "no LLM" {
        if state.unicode {
            "  No LLM configured — use /provider to select one"
        } else {
            "  No LLM configured - use /provider to select one"
        }
    } else {
        "  Type a message and press Enter to start chatting"
    }
}

#[cfg(feature = "tui")]
fn build_chat_history_lines(state: &AppState, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for entry in state.chat.entries() {
        push_chat_entry_lines(&mut lines, entry, state, theme, width);
        lines.push(Line::default());
    }

    lines
}

#[cfg(feature = "tui")]
fn push_chat_entry_lines(
    lines: &mut Vec<Line<'static>>,
    entry: &ChatEntry,
    state: &AppState,
    theme: &Theme,
    width: usize,
) {
    match entry {
        ChatEntry::User(text) => push_user_chat_lines(lines, text, theme),
        ChatEntry::Assistant { content, streaming } => {
            push_assistant_chat_lines(lines, content, *streaming, state, theme)
        }
        ChatEntry::ToolCall {
            id: _,
            name,
            args,
            result,
            failed,
        } => push_tool_call_chat_lines(
            lines,
            name,
            args,
            result.as_deref(),
            *failed,
            state,
            theme,
            width,
        ),
    }
}

#[cfg(feature = "tui")]
fn push_user_chat_lines(lines: &mut Vec<Line<'static>>, text: &str, theme: &Theme) {
    for (index, line_str) in text.lines().enumerate() {
        let prefix = if index == 0 { " you  " } else { "      " };
        lines.push(Line::from(vec![
            Span::styled(prefix, theme.dim()),
            Span::styled(line_str.to_string(), theme.normal()),
        ]));
    }
}

#[cfg(feature = "tui")]
fn assistant_stream_cursor(streaming: bool, unicode: bool) -> &'static str {
    match (streaming, unicode) {
        (true, true) => "▌",
        (true, false) => "|",
        (false, _) => "",
    }
}

fn push_assistant_chat_lines(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    streaming: bool,
    state: &AppState,
    theme: &Theme,
) {
    let cursor = assistant_stream_cursor(streaming, state.unicode);
    let display = format!("{content}{cursor}");

    for (index, line_str) in display.lines().enumerate() {
        let prefix = if index == 0 { " ahma " } else { "      " };
        lines.push(Line::from(vec![
            Span::styled(prefix, theme.running()),
            Span::styled(line_str.to_string(), theme.normal()),
        ]));
    }

    if display.is_empty() && streaming {
        lines.push(Line::from(vec![
            Span::styled(" ahma ", theme.running()),
            Span::styled(assistant_stream_cursor(true, state.unicode), theme.dim()),
        ]));
    }
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn push_tool_call_chat_lines(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    args: &str,
    result: Option<&str>,
    failed: bool,
    state: &AppState,
    theme: &Theme,
    width: usize,
) {
    let (glyph, status_style) = tool_call_status(result.is_some(), failed, state, theme);
    lines.push(Line::from(vec![
        Span::styled(" tool ", theme.dim()),
        Span::styled(glyph, status_style),
        Span::styled(format!(" {name}"), theme.dim()),
    ]));

    if !args.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            format!("       {}", truncate(args, width.saturating_sub(8))),
            theme.dim(),
        )));
    }

    if let Some(result) = result {
        lines.push(Line::from(Span::styled(
            format!("       {}", truncate(result, width.saturating_sub(8))),
            theme.dim(),
        )));
    }
}

#[cfg(feature = "tui")]
fn tool_call_status(
    has_result: bool,
    failed: bool,
    state: &AppState,
    theme: &Theme,
) -> (&'static str, Style) {
    match (has_result, failed, state.unicode) {
        (true, true, true) => ("✗", theme.failed()),
        (true, true, false) => ("x", theme.failed()),
        (true, false, true) => ("✓", theme.success()),
        (true, false, false) => ("v", theme.success()),
        (false, _, true) => ("⟳", theme.running()),
        (false, _, false) => (">", theme.running()),
    }
}

#[cfg(feature = "tui")]
fn chat_history_scroll_offset(total: usize, visible_h: usize, from_bottom: usize) -> usize {
    if total > visible_h {
        total.saturating_sub(visible_h).saturating_sub(from_bottom)
    } else {
        0
    }
}

#[cfg(feature = "tui")]
fn insert_input_cursor(lines: &mut Vec<String>, row: usize, col: usize, unicode: bool) {
    let cursor = if unicode { '│' } else { '|' };
    if let Some(line) = lines.get_mut(row) {
        let insert_at = col.min(line.chars().count());
        let byte_idx = line
            .char_indices()
            .nth(insert_at)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| line.len());
        line.insert(byte_idx, cursor);
    } else {
        lines.push(cursor.to_string());
    }
}

#[cfg(feature = "tui")]
fn draw_input_box(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::Chat;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let title_left = if state.llm_label == "no LLM" {
        Line::from(Span::styled(
            " model: no LLM — /provider to configure ",
            theme.title(),
        ))
        .left_aligned()
    } else {
        Line::from(Span::styled(
            format!(" model: {} ", state.llm_label),
            theme.title(),
        ))
        .left_aligned()
    };
    let title_right = Line::from(Span::styled(
        format!(" sandbox: {} ", shorten_path(&state.workspace, 45)),
        theme.dim(),
    ))
    .right_aligned();

    let block = Block::default()
        .title(title_left)
        .title(title_right)
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (cursor_row, cursor_col) = state.chat_input.cursor();
    let mut rendered_lines: Vec<String> = state.chat_input.lines().to_vec();
    if focused {
        insert_input_cursor(&mut rendered_lines, cursor_row, cursor_col, state.unicode);
    }

    let text = rendered_lines.join("\n");
    let para = Paragraph::new(Span::styled(text, theme.normal())).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

#[cfg(feature = "tui")]
fn draw_chat_footer(frame: &mut Frame, _state: &AppState, theme: &Theme, area: Rect) {
    let keys: &[(&str, &str)] = &[
        ("Enter", "send"),
        ("Shift+Enter", "newline"),
        ("/", "commands"),
        ("Tab", "monitor panels"),
        ("q", "quit"),
    ];

    let mut spans: Vec<Span> = vec![];
    for (key, desc) in keys {
        spans.push(Span::styled(format!("  {key} "), theme.footer_key()));
        spans.push(Span::styled(desc.to_string(), theme.footer()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.footer()),
        area,
    );
}

// ─── Monitor layout ───────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_monitor_layout(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let full = frame.area();
    let approval_h: u16 = if state.approval.is_some() { 3 } else { 0 };

    let [header_a, activity_a, middle_a, log_a, approval_a, footer_a] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Min(4),
        Constraint::Length(approval_h),
        Constraint::Length(1),
    ])
    .areas(full);

    let [ops_a, detail_a] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .areas(middle_a);

    draw_header(frame, state, theme, header_a);
    draw_ai_activity(frame, state, theme, activity_a);
    draw_ops_dag(frame, state, theme, ops_a);
    draw_detail(frame, state, theme, detail_a);
    draw_log(frame, state, theme, log_a);

    if state.approval.is_some() {
        draw_approval(frame, state, theme, approval_a);
    }

    draw_footer(frame, state, theme, footer_a);
    if state.palette.visible {
        draw_palette(frame, state, theme, full);
    }
}

// ─── Navigator overlay ────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn overlay_bar_cursor(unicode: bool) -> &'static str {
    if unicode { "│" } else { "|" }
}

#[cfg(feature = "tui")]
fn render_horizontal_rule(frame: &mut Frame, area: Rect, unicode: bool, theme: &Theme) {
    let sep_char = if unicode { "─" } else { "-" };
    frame.render_widget(
        Paragraph::new(Span::styled(
            sep_char.repeat(area.width as usize),
            theme.dim(),
        )),
        area,
    );
}

#[cfg(feature = "tui")]
fn navigator_list_items(
    completions: &[NavCommand],
    selected: usize,
    desc_col: usize,
    inner_width: usize,
    limit: usize,
    theme: &Theme,
) -> Vec<ListItem<'static>> {
    completions
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, cmd)| {
            let selected_row = i == selected;
            let style = if selected_row {
                theme.selected_item()
            } else {
                theme.normal()
            };
            let cmd_str = truncate(&cmd.command, desc_col);
            let desc_str = truncate(cmd.description, inner_width.saturating_sub(desc_col + 2));
            let desc_style = if selected_row { style } else { theme.dim() };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:<width$}", cmd_str, width = desc_col), style),
                Span::styled(format!(" {desc_str}"), desc_style),
            ]))
        })
        .collect()
}

#[cfg(feature = "tui")]
fn draw_navigator(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let w = 64u16.min(area.width);
    let max_items = 12u16;
    let h = (3 + max_items).min(area.height);
    let popup = centered_rect(w, h, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" / Commands ", theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return;
    }

    // Input line.
    let cursor = overlay_bar_cursor(state.unicode);
    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("/ {}{cursor}", state.navigator.input),
            theme.running(),
        )),
        input_area,
    );

    if inner.height < 3 {
        return;
    }

    let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    render_horizontal_rule(frame, sep_area, state.unicode, theme);

    let list_h = inner.height.saturating_sub(2);
    if list_h == 0 || state.navigator.completions.is_empty() {
        return;
    }
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, list_h);

    let desc_col = (inner.width as usize).saturating_sub(32).max(20);
    let items = navigator_list_items(
        &state.navigator.completions,
        state.navigator.selected,
        desc_col,
        inner.width as usize,
        list_h as usize,
        theme,
    );

    let mut list_state = ListState::default().with_selected(Some(state.navigator.selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selected_item()),
        list_area,
        &mut list_state,
    );
}

// ─── Inline picker overlay ────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_picker(frame: &mut Frame, picker: &crate::state::PickerState, theme: &Theme, area: Rect) {
    let w = 60u16.min(area.width);
    let max_items = 10u16;
    let h = (4 + max_items).min(area.height);
    let popup = centered_rect(w, h, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(format!(" {} ", picker.title), theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return;
    }

    let [filter_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let filter_text = if picker.filter.is_empty() {
        " filter: type to narrow".to_string()
    } else {
        format!(" filter: {}", picker.filter)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(filter_text, theme.dim())),
        filter_area,
    );

    let items_filtered = picker.filtered_items();
    let items: Vec<ListItem> = items_filtered
        .iter()
        .take(list_area.height as usize)
        .enumerate()
        .map(|(i, name)| {
            let style = if i == picker.selected {
                theme.selected_item()
            } else {
                theme.normal()
            };
            ListItem::new(Span::styled(format!(" {name}"), style))
        })
        .collect();

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (none available)", theme.dim())),
            list_area,
        );
        return;
    }

    let mut list_state = ListState::default().with_selected(Some(picker.selected));
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selected_item()),
        list_area,
        &mut list_state,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            "  type filter  ↑↓ navigate  Enter select  Esc cancel",
            theme.dim(),
        )),
        hint_area,
    );
}

// ─── Header ───────────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn header_health_span(state: &AppState, theme: &Theme) -> Span<'static> {
    if state.server_healthy {
        let label = if state.unicode {
            " ● HEALTHY"
        } else {
            " * HEALTHY"
        };
        Span::styled(label, theme.healthy())
    } else {
        let label = if state.unicode {
            " ○ OFFLINE"
        } else {
            " - OFFLINE"
        };
        Span::styled(label, theme.unhealthy())
    }
}

#[cfg(feature = "tui")]
fn draw_header(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let health_span = header_health_span(state, theme);

    let sandbox_style = match state.sandbox_status.as_str() {
        "LOCKED" => theme.success(),
        "INITIALIZING" => theme.pending(),
        "FAILED" => theme.failed(),
        _ => theme.unknown_health(),
    };

    let session_part = state
        .session_id
        .as_deref()
        .map(|id| format!(" · session {}", &id[..id.len().min(8)]))
        .unwrap_or_default();

    let workspace_short = shorten_path(&state.workspace, 30);
    let mut http_count = 0;
    let mut stdio_count = 0;
    for s in &state.mcp_connections.servers {
        if s.enabled {
            match &s.kind {
                crate::mcp_connections::McpServerKind::Http { .. } => http_count += 1,
                crate::mcp_connections::McpServerKind::Stdio { .. } => stdio_count += 1,
            }
        }
    }
    let external_tools = state.mcp_connections.aggregate_tool_names().len();
    let external_part = if http_count > 0 || stdio_count > 0 {
        format!(" · ext (http:{http_count} stdio:{stdio_count})/{external_tools}")
    } else {
        String::new()
    };

    let tokens_part = if state.token_usage.total_tokens > 0 {
        let (p, c, t) = (
            state.token_usage.prompt_tokens,
            state.token_usage.completion_tokens,
            state.token_usage.total_tokens,
        );
        let p_fmt = if p > 1000 {
            format!("{:.1}k", p as f64 / 1000.0)
        } else {
            p.to_string()
        };
        let c_fmt = if c > 1000 {
            format!("{:.1}k", c as f64 / 1000.0)
        } else {
            c.to_string()
        };
        let t_fmt = if t > 1000 {
            format!("{:.1}k", t as f64 / 1000.0)
        } else {
            t.to_string()
        };
        format!(" · tkns {p_fmt} in / {c_fmt} out ({t_fmt} ttl)")
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled(" ahma", theme.title()),
        Span::styled(session_part, theme.dim()),
        Span::styled(
            format!(" · sandbox {}", state.sandbox_status),
            sandbox_style,
        ),
        Span::styled(external_part, theme.dim()),
        Span::styled(tokens_part, theme.pending()),
        Span::styled(format!(" · {workspace_short}"), theme.dim()),
        Span::styled(format!(" · {}", state.transport_label), theme.dim()),
        health_span,
        Span::styled("  q quit  ? help", theme.dim()),
    ]);

    let para = Paragraph::new(line).style(theme.header_bar());
    frame.render_widget(para, area);
}

// ─── AI Activity ──────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_ai_activity(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::AiActivity;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let block = Block::default()
        .title(Span::styled(" AI Activity ", theme.title()))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.ai_activity.is_empty() {
        let hint = if state.server_healthy {
            "  Awaiting AI activity…"
        } else {
            "  Connecting to server…"
        };
        frame.render_widget(Paragraph::new(Span::styled(hint, theme.dim())), inner);
        return;
    }

    let visible_h = inner.height as usize;
    let scroll = state
        .activity_scroll
        .min(state.ai_activity.len().saturating_sub(1));

    let items: Vec<ListItem> = state
        .ai_activity
        .iter()
        .skip(scroll)
        .take(visible_h)
        .map(|e| {
            let glyph = e.status.glyph(state.unicode);
            let ts = e.timestamp.format("%H:%M:%S").to_string();
            let elapsed = e
                .elapsed
                .map(|d| format!(" {:.1}s", d.as_secs_f64()))
                .unwrap_or_default();
            let summary = e
                .summary
                .as_deref()
                .map(|s| format!("  → {}", truncate(s, 25)))
                .unwrap_or_default();

            let line = Line::from(vec![
                Span::styled(format!(" {ts} "), theme.dim()),
                Span::styled(format!("{glyph} "), theme.activity_status_style(&e.status)),
                Span::styled(format!("{:<12}", e.method), theme.dim()),
                Span::styled(format!("{:<18}", e.tool), theme.normal()),
                Span::styled(elapsed, theme.dim()),
                Span::styled(summary, theme.dim()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let mut list_state = if focused {
        ListState::default().with_selected(Some(0))
    } else {
        ListState::default()
    };

    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, inner, &mut list_state);

    if state.ai_activity.len() > visible_h {
        let sb = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let mut sb_state = ScrollbarState::new(state.ai_activity.len()).position(scroll);
        frame.render_stateful_widget(sb, inner, &mut sb_state);
    }
}

// ─── Operations DAG ───────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_ops_dag(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::OpsDag;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let block = Block::default()
        .title(Span::styled(" Operations ", theme.title()))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.operations.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  No active operations", theme.dim())),
            inner,
        );
        return;
    }

    let items = build_ops_dag_items(state, theme, inner.width as usize);

    let mut list_state = ListState::default().with_selected(Some(state.ops_selected));
    let list = List::new(items)
        .highlight_style(theme.selected_item())
        .highlight_symbol(if state.unicode { "▶ " } else { "> " });

    frame.render_stateful_widget(list, inner, &mut list_state);
}

#[cfg(feature = "tui")]
fn build_ops_dag_items(state: &AppState, theme: &Theme, width: usize) -> Vec<ListItem<'static>> {
    state
        .operations
        .iter()
        .enumerate()
        .map(|(index, op)| build_ops_dag_item(index, op, state, theme, width))
        .collect()
}

#[cfg(feature = "tui")]
fn build_ops_dag_item(
    index: usize,
    op: &crate::state::Operation,
    state: &AppState,
    theme: &Theme,
    width: usize,
) -> ListItem<'static> {
    let prefix = match (op.parent_id.is_some(), state.unicode) {
        (true, true) => "  └ ",
        (true, false) => "  L ",
        (false, _) => " ",
    };
    let pinned = match (op.pinned, state.unicode) {
        (true, true) => "📌",
        (true, false) => "P",
        (false, _) => "",
    };
    let id_short = &op.id[..op.id.len().min(6)];
    let name_short = truncate(&op.tool_name, width.saturating_sub(22));
    let row_style = if index == state.ops_selected {
        theme.selected_item()
    } else {
        theme.normal()
    };

    let line = Line::from(vec![
        Span::styled(prefix, theme.dim()),
        Span::styled(
            format!("{} ", op.status.glyph(state.unicode)),
            theme.op_status_style(&op.status),
        ),
        Span::styled(format!("{id_short} "), theme.dim()),
        Span::styled(format!("{pinned}{name_short}"), row_style),
        Span::styled(format!("  {}", op.elapsed_display()), theme.dim()),
    ]);
    ListItem::new(line)
}

// ─── Detail pane ──────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_detail(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::OpsDag;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    match state.selected_op() {
        None => render_empty_detail(frame, theme, area, border_style),
        Some(op) => render_selected_detail(frame, state, theme, area, border_style, op),
    }
}

#[cfg(feature = "tui")]
fn render_empty_detail(frame: &mut Frame, theme: &Theme, area: Rect, border_style: Style) {
    let block = Block::default()
        .title(Span::styled(" Detail ", theme.title()))
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Span::styled("  Select an operation", theme.dim())),
        inner,
    );
}

#[cfg(feature = "tui")]
fn render_selected_detail(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    area: Rect,
    border_style: Style,
    op: &crate::state::Operation,
) {
    let title = format!(" {}  {} ", op.id, op.tool_name);
    let block = Block::default()
        .title(Span::styled(title, theme.title()))
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = build_detail_lines(
        op,
        state,
        theme,
        inner.width as usize,
        inner.height as usize,
    );
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

#[cfg(feature = "tui")]
fn build_detail_lines(
    op: &crate::state::Operation,
    state: &AppState,
    theme: &Theme,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    push_detail_summary_lines(&mut lines, op, state, theme, width);
    push_stdout_tail_lines(&mut lines, op, state, theme, width, height);
    lines
}

#[cfg(feature = "tui")]
fn push_detail_summary_lines(
    lines: &mut Vec<Line<'static>>,
    op: &crate::state::Operation,
    state: &AppState,
    theme: &Theme,
    width: usize,
) {
    lines.push(Line::from(vec![
        Span::styled("  status  ", theme.dim()),
        Span::styled(
            format!("{} {:?}", op.status.glyph(state.unicode), op.status),
            theme.op_status_style(&op.status),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  elapsed ", theme.dim()),
        Span::styled(op.elapsed_display(), theme.normal()),
    ]));

    if let Some(cwd) = &op.cwd {
        lines.push(Line::from(vec![
            Span::styled("  cwd     ", theme.dim()),
            Span::styled(shorten_path(cwd, 40), theme.normal()),
        ]));
    }
    if let Some(pid) = op.pid {
        lines.push(Line::from(vec![
            Span::styled("  pid     ", theme.dim()),
            Span::styled(pid.to_string(), theme.normal()),
        ]));
    }
    if !op.args.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  args    ", theme.dim()),
            Span::styled(
                truncate(&op.args.join(" "), width.saturating_sub(12)),
                theme.normal(),
            ),
        ]));
    }
    if let Some(parent) = &op.parent_id {
        lines.push(Line::from(vec![
            Span::styled("  waits   ", theme.dim()),
            Span::styled(parent.clone(), theme.pending()),
        ]));
    }
}

#[cfg(feature = "tui")]
fn push_stdout_tail_lines(
    lines: &mut Vec<Line<'static>>,
    op: &crate::state::Operation,
    state: &AppState,
    theme: &Theme,
    width: usize,
    height: usize,
) {
    if op.stdout_tail.is_empty() {
        return;
    }

    let separator = if state.unicode {
        "─".repeat(width.saturating_sub(4))
    } else {
        "-".repeat(width.saturating_sub(4))
    };
    lines.push(Line::from(Span::styled(
        format!("  {separator}"),
        theme.dim(),
    )));

    let tail_height = height.saturating_sub(lines.len());
    let visible_tail: Vec<_> = op.stdout_tail.iter().rev().take(tail_height).collect();
    for line in visible_tail.into_iter().rev() {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(line, width.saturating_sub(4))),
            theme.dim(),
        )));
    }
}

// ─── Log pane ─────────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn log_filter_indicator(state: &AppState) -> String {
    if state.log_filter_active {
        format!(" filter: {}_", state.log_filter)
    } else if !state.log_filter.is_empty() {
        format!(" filter: {}", state.log_filter)
    } else {
        String::new()
    }
}

#[cfg(feature = "tui")]
fn draw_log(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::Log;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let filter_indicator = log_filter_indicator(state);

    let block = Block::default()
        .title(Span::styled(
            format!(" Log{filter_indicator} "),
            theme.title(),
        ))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let filtered = state.filtered_log();
    if filtered.is_empty() {
        let hint = if state.log.is_empty() {
            "  Awaiting log events…"
        } else {
            "  No entries match filter"
        };
        frame.render_widget(Paragraph::new(Span::styled(hint, theme.dim())), inner);
        return;
    }

    let visible_h = inner.height as usize;
    let scroll = state.log_scroll.min(filtered.len().saturating_sub(1));

    let items: Vec<ListItem> = filtered
        .iter()
        .skip(scroll)
        .take(visible_h)
        .map(|e| {
            let ts = e.timestamp.format("%H:%M:%S").to_string();
            let level_style = theme.log_style(&e.level);
            let msg = truncate(
                &e.message,
                (inner.width as usize).saturating_sub(ts.len() + 8),
            );
            let line = Line::from(vec![
                Span::styled(format!(" {ts} "), theme.dim()),
                Span::styled(e.level.label(), level_style),
                Span::styled(format!(" {msg}"), theme.normal()),
            ]);
            ListItem::new(line)
        })
        .collect();

    frame.render_widget(List::new(items), inner);

    if filtered.len() > visible_h {
        let sb = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let mut sb_state = ScrollbarState::new(filtered.len()).position(scroll);
        frame.render_stateful_widget(sb, inner, &mut sb_state);
    }
}

// ─── Approval banner ──────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_approval(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let gate = match &state.approval {
        Some(g) => g,
        None => return,
    };

    let countdown = gate
        .remaining_secs()
        .map(|s| format!("  ({s}s left)"))
        .unwrap_or_default();

    let desc = truncate(&gate.description, (area.width as usize).saturating_sub(50));
    let warn = if state.unicode { "⚠ " } else { "! " };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {warn}APPROVAL REQUIRED  {}", gate.op_id),
                theme.approval_banner(),
            ),
            Span::styled(format!("  {desc}"), theme.approval_banner()),
            Span::styled(countdown, theme.approval_banner()),
        ]),
        Line::from(vec![
            Span::styled("   [y] approve  ", theme.approval_banner()),
            Span::styled("[n] reject  ", theme.approval_banner()),
            Span::styled(
                "[Tab] focus other panels while deciding",
                theme.approval_banner(),
            ),
        ]),
    ];

    if let Some(diff) = &gate.diff {
        lines.push(Line::from(""));
        for diff_line in diff.lines().take(9) {
            let style = if diff_line.starts_with('+') {
                theme.success()
            } else if diff_line.starts_with('-') {
                theme.failed()
            } else {
                theme.normal()
            };
            lines.push(Line::from(Span::styled(
                format!("    {}", diff_line),
                style,
            )));
        }
    }

    let para = Paragraph::new(Text::from(lines)).style(theme.approval_banner());
    frame.render_widget(para, area);
}

// ─── Footer ───────────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_footer(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let keys: &[(&str, &str)] = match state.focus {
        Focus::AiActivity => &[
            ("Tab", "next pane"),
            ("j/k", "scroll"),
            (":", "command"),
            ("q", "quit"),
            ("?", "help"),
        ],
        Focus::OpsDag => &[
            ("Tab", "next pane"),
            ("j/k", "select"),
            ("c", "cancel"),
            ("p", "pin"),
            (":", "command"),
            ("q", "quit"),
        ],
        Focus::Log => &[
            ("Tab", "next pane"),
            ("j/k", "scroll"),
            ("/", "filter"),
            ("g/G", "top/bottom"),
            ("q", "quit"),
        ],
        Focus::Palette => &[("Esc", "close"), ("Tab", "complete"), ("Enter", "run")],
        Focus::Chat => &[("/", "commands"), ("Tab", "monitor panels"), ("q", "quit")],
    };

    let mut spans: Vec<Span> = vec![];
    for (key, desc) in keys {
        spans.push(Span::styled(format!("  {key} "), theme.footer_key()));
        spans.push(Span::styled(desc.to_string(), theme.footer()));
    }
    let content_len: usize = keys.iter().map(|(k, d)| k.len() + d.len() + 3).sum();
    let padding = (area.width as usize).saturating_sub(content_len);
    spans.push(Span::styled(" ".repeat(padding), theme.footer()));

    let para = Paragraph::new(Line::from(spans)).style(theme.footer());
    frame.render_widget(para, area);
}

// ─── Help overlay ─────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_help(frame: &mut Frame, theme: &Theme, area: Rect) {
    let w = 62u16.min(area.width);
    let h = 38u16.min(area.height);
    let popup = centered_rect(w, h, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" Help — ahma TUI ", theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows: &[(&str, &str)] = &[
        ("GLOBAL", ""),
        ("q / Ctrl-C", "Quit"),
        ("Tab / Shift-Tab", "Cycle focus"),
        ("?", "Toggle this help"),
        ("Esc / ? (when help open)", "Close help overlay"),
        (":", "Open command palette"),
        ("", ""),
        ("CHAT", ""),
        ("Enter", "Send message"),
        ("Shift+Enter", "Insert newline"),
        ("/", "Open command navigator from empty input"),
        ("Esc", "Clear current input"),
        ("Arrow keys / Home / End", "Move within the editor"),
        ("", ""),
        ("PICKERS", ""),
        ("Type", "Filter providers or models"),
        ("Up / Down", "Move selection"),
        ("Enter / Esc", "Choose / cancel"),
        ("", ""),
        ("AI ACTIVITY", ""),
        ("j / k", "Scroll entries"),
        ("g / G", "Top / bottom"),
        ("", ""),
        ("OPERATIONS", ""),
        ("j / k", "Select operation"),
        ("c", "Cancel selected"),
        ("p", "Pin to top"),
        ("a", "Await selected"),
        ("", ""),
        ("LOG", ""),
        ("/", "Start filter (Esc to clear)"),
        ("j / k", "Scroll"),
        ("g / G", "Top / bottom"),
        ("", ""),
        ("APPROVAL BANNER", ""),
        ("y", "Approve gate"),
        ("n", "Reject gate"),
        ("", ""),
        ("COMMAND NAVIGATOR (/)", ""),
        ("Type", "Narrow commands and tools"),
        ("Tab", "Complete selected command"),
        ("Enter", "Run selected command"),
        ("/run <tool> {json}", "Run a tool manually with JSON args"),
        ("", ""),
        ("COMMAND PALETTE (:)", ""),
        ("Tab", "Next completion"),
        ("Enter", "Run command"),
        ("", ""),
        ("WINDOW ACTIONS", ""),
        ("/n", "Restore/expand window with ID n (e.g. /3)"),
        ("/xn", "Close/cancel window with ID n (e.g. /x3)"),
        ("/exit", "Quit the application"),
        ("Mouse Click on Xn", "Close/cancel window"),
        ("Mouse Click on Window", "Toggle expand/collapse"),
    ];

    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, desc)| {
            if key.is_empty() {
                Line::default()
            } else if desc.is_empty() {
                Line::from(Span::styled(format!(" {key}"), theme.title()))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {:<24}", key), theme.footer_key()),
                    Span::styled(desc.to_string(), theme.normal()),
                ])
            }
        })
        .collect();

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

// ─── Command palette overlay ──────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn palette_list_items(
    completions: &[String],
    selected: usize,
    limit: usize,
    theme: &Theme,
) -> Vec<ListItem<'static>> {
    completions
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, name)| {
            let style = if i == selected {
                theme.selected_item()
            } else {
                theme.normal()
            };
            ListItem::new(Span::styled(format!(" {name}"), style))
        })
        .collect()
}

#[cfg(feature = "tui")]
fn draw_palette(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let w = 60u16.min(area.width);
    let max_items = 10u16;
    let h = (3 + max_items).min(area.height);
    let popup = centered_rect(w, h, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" : Command ", theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 {
        return;
    }

    // Input line with blinking cursor illusion
    let cursor = overlay_bar_cursor(state.unicode);
    let input_display = format!("> {}{cursor}", state.palette.input);
    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(input_display, theme.running())),
        input_area,
    );

    if inner.height < 3 || state.palette.completions.is_empty() {
        return;
    }

    let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    render_horizontal_rule(frame, sep_area, state.unicode, theme);

    let list_h = inner.height.saturating_sub(2);
    if list_h == 0 {
        return;
    }
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, list_h);

    let items = palette_list_items(
        &state.palette.completions,
        state.palette.selected_completion,
        list_h as usize,
        theme,
    );

    let mut list_state =
        ListState::default().with_selected(Some(state.palette.selected_completion));
    let list = List::new(items).highlight_style(theme.selected_item());
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

// ─── Utility ──────────────────────────────────────────────────────────────────

fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        let keep = truncated.len().saturating_sub(1);
        format!("{}…", &truncated[..keep])
    } else {
        truncated
    }
}

pub(crate) fn shorten_path(path: &str, max_chars: usize) -> String {
    if path.len() <= max_chars {
        return path.to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let shortened = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if shortened.len() <= max_chars {
        return shortened;
    }
    let keep = max_chars.saturating_sub(1);
    let start = shortened.len().saturating_sub(keep);
    format!("…{}", &shortened[start..])
}

/// Returns a centered `Rect` of the given size within `area`.
#[cfg(feature = "tui")]
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

// ─── Stub when `tui` feature is disabled ─────────────────────────────────────

/// No-op stub so the crate compiles without the `tui` feature.
#[cfg(not(feature = "tui"))]
pub fn draw(_frame: &mut (), _state: &AppState, _theme: &Theme) {}
