//! Ratatui rendering — all panel draw functions.
//!
//! The top-level [`draw`] function dispatches to either the chat or monitor
//! layout based on `state.mode`.

#![allow(dead_code)]

#[cfg(feature = "tui")]
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

use crate::state::{AppState, ChatEntry, ClickTarget, Focus, Mode, NavCommand};
use crate::theme::Theme;

// ─── Top-level draw ───────────────────────────────────────────────────────────

/// Called every redraw tick — the only public entry point in this module.
#[cfg(feature = "tui")]
pub fn draw(frame: &mut Frame, state: &AppState, theme: &Theme) {
    match state.mode {
        Mode::Chat => draw_chat_layout(frame, state, theme),
        Mode::Monitor => draw_monitor_layout(frame, state, theme),
    }

    // Overlays drawn on top of whichever layout is active. At most one user
    // overlay is open (SPEC R23); the palette is rendered inline in the layout,
    // so it is a no-op here.
    let full = frame.area();
    match &state.modal {
        crate::state::ModalState::None | crate::state::ModalState::Palette(_) => {}
        crate::state::ModalState::Help => draw_help(frame, theme, full),
        crate::state::ModalState::Navigator(_) => draw_navigator(frame, state, theme, full),
        crate::state::ModalState::ProviderPicker(picker)
        | crate::state::ModalState::ModelPicker(picker) => draw_picker(frame, picker, theme, full),
        crate::state::ModalState::LogFiles { .. } => {
            draw_log_files_modal(frame, state, theme, full)
        }
    }
    if state.settings_editor.open {
        draw_settings_panel(frame, state, theme, full);
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
fn window_status_style(status: crate::state::WindowStatus, theme: &Theme) -> Style {
    use crate::state::WindowStatus;
    match status {
        WindowStatus::Running => theme.running(),
        WindowStatus::Finished => theme.success(),
        WindowStatus::Error => theme.failed(),
        WindowStatus::Cancelled => theme.cancelled(),
        WindowStatus::Pending => theme.pending(),
    }
}

fn draw_collapsed_window(
    frame: &mut Frame,
    w: &crate::state::TuiWindow,
    area: Rect,
    theme: &Theme,
    status_style: Style,
) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let f = (ms / 150) as usize;
    let status_str = if w.status == crate::state::WindowStatus::Running {
        let spinner = if theme.unicode {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            frames[f % frames.len()]
        } else {
            let frames = ["-", "\\", "|", "/"];
            frames[f % frames.len()]
        };
        format!("Running {}", spinner)
    } else {
        w.status.label().to_string()
    };

    let mut spans = vec![
        Span::styled(" [+] ", theme.dim()),
        Span::styled(format!("{} ", w.id), theme.normal()),
        Span::styled(format!("[{}] ", status_str), status_style),
        Span::styled(w.label.clone(), theme.normal()),
    ];
    let left_len: usize = spans.iter().map(|s| s.content.len()).sum();
    let right_str = format!(" x{}", w.id);
    let pad_width = (area.width as usize).saturating_sub(left_len + right_str.len());
    if pad_width > 0 {
        spans.push(Span::raw(" ".repeat(pad_width)));
    }
    spans.push(Span::styled(right_str, theme.dim()));

    let line = Line::from(spans);
    let para = Paragraph::new(line);
    frame.render_widget(para, area);
}

#[cfg(feature = "tui")]
fn get_running_spinner(unicode: bool) -> &'static str {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let f = (ms / 150) as usize;
    if unicode {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        frames[f % frames.len()]
    } else {
        let frames = ["-", "\\", "|", "/"];
        frames[f % frames.len()]
    }
}

#[cfg(feature = "tui")]
fn build_window_title(w: &crate::state::TuiWindow, width: u16, unicode: bool) -> String {
    let border_width = 2;
    let title_space = (width as usize).saturating_sub(border_width);

    let status_str = if w.status == crate::state::WindowStatus::Running {
        format!("[Running {}]", get_running_spinner(unicode))
    } else {
        format!("[{}]", w.status)
    };

    let title_left = format!(" [-] {} {} {}", w.id, status_str, w.label);
    let title_right = format!("x{} ", w.id);
    let pad_width = title_space.saturating_sub(title_left.len() + title_right.len());
    if pad_width > 0 {
        format!("{}{}{}", title_left, " ".repeat(pad_width), title_right)
    } else {
        title_left
    }
}

fn draw_expanded_window(
    frame: &mut Frame,
    w: &crate::state::TuiWindow,
    area: Rect,
    theme: &Theme,
    status_style: Style,
) {
    let title_combined = build_window_title(w, area.width, theme.unicode);

    let block = Block::default()
        .title(Span::styled(title_combined, theme.normal()))
        .borders(Borders::ALL)
        .border_style(status_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // ── Command → output card layout ─────────────────────────────────────────
    // Show the shell command as a distinct header line followed by a separator,
    // then the output.  This gives the "command → output" visual structure that
    // makes it easy to match output back to the operation that produced it.
    let mut content_lines: Vec<Line> = Vec::new();

    // Command header (shown when non-empty and distinct from the label).
    if !w.command.is_empty() && w.command != w.label {
        let cmd_display = format!("$ {}", w.command);
        content_lines.push(Line::from(Span::styled(cmd_display, theme.dim())));
        // Separator
        let sep_char = if area.width > 0 { "─" } else { "-" };
        content_lines.push(Line::from(Span::styled(
            sep_char.repeat(inner.width as usize),
            theme.dim(),
        )));
    }

    // Output lines
    for line in &w.content {
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
        content_lines.push(Line::from(Span::styled(line.clone(), style)));
    }

    let para = Paragraph::new(content_lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

#[cfg(feature = "tui")]
fn draw_single_window(
    frame: &mut Frame,
    w: &crate::state::TuiWindow,
    l: &RenderedWindowLayout,
    area: Rect,
    theme: &Theme,
) {
    let status_style = window_status_style(w.status, theme);
    if l.collapsed {
        draw_collapsed_window(frame, w, area, theme, status_style);
    } else {
        draw_expanded_window(frame, w, area, theme, status_style);
    }
}

#[cfg(feature = "tui")]
fn draw_windows_layout(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    windows_area: Rect,
    layouts: &[RenderedWindowLayout],
) {
    if windows_area.height == 0 || layouts.is_empty() {
        return;
    }
    let mut constraints = Vec::new();
    let mut active_layouts = Vec::new();
    for l in layouts {
        if l.visible && l.height > 0 {
            constraints.push(Constraint::Length(l.height));
            active_layouts.push(l);
        }
    }
    let window_areas = Layout::vertical(constraints).split(windows_area);
    for (area, l) in window_areas.iter().zip(active_layouts) {
        let w = &state.windows[l.orig_idx];
        state.window_rects.borrow_mut().push((w.id, *area));
        draw_single_window(frame, w, l, *area, theme);
    }
}

#[cfg(feature = "tui")]
fn draw_chat_layout(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let full = frame.area();
    let approval_h: u16 = if let Some(gate) = &state.approval {
        // +2 for the rounded border (top/bottom). 2 content lines normally,
        // or 2 + blank + up to 9 diff lines when a diff is attached.
        if gate.diff.is_some() { 14 } else { 4 }
    } else {
        0
    };

    // Input height target: 1-6 lines based on wrapped content
    let inner_width = full.width.saturating_sub(2);
    let wrapped_line_count = state
        .chat_input_line_count(inner_width as usize)
        .clamp(1, 6);
    state
        .chat_input_height_target
        .set(wrapped_line_count as f64);

    let input_lines = state
        .chat_input_height_current
        .get()
        .round()
        .clamp(1.0, 6.0) as u16;
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

    draw_windows_layout(frame, state, theme, windows_area, &layouts);

    if state.approval.is_some() {
        draw_approval(frame, state, theme, approval_a);
    }
    draw_input_box(frame, state, theme, input_a);
    draw_chat_footer(frame, state, theme, footer_a);
}

#[cfg(feature = "tui")]
fn get_mcp_label(mcp_enabled: bool, unicode: bool) -> &'static str {
    match (mcp_enabled, unicode) {
        (true, true) => " · MCP ✓",
        (true, false) => " · MCP on",
        (false, _) => "",
    }
}

#[cfg(feature = "tui")]
fn get_health_indicator(
    server_healthy: bool,
    unicode: bool,
    theme: &Theme,
) -> (&'static str, Style) {
    match (server_healthy, unicode) {
        (true, true) => (" ●", theme.healthy()),
        (true, false) => (" *", theme.healthy()),
        (false, true) => (" ○", theme.unhealthy()),
        (false, false) => (" -", theme.unhealthy()),
    }
}

#[cfg(feature = "tui")]
fn get_daemon_indicator(
    daemon_healthy: bool,
    unicode: bool,
    theme: &Theme,
) -> (&'static str, Style) {
    let daemon_char = match (daemon_healthy, unicode) {
        (true, true) => " ● DMON",
        (true, false) => " * DMON",
        (false, true) => " ○ DMON",
        (false, false) => " - DMON",
    };
    let daemon_style = if daemon_healthy {
        theme.healthy()
    } else {
        theme.unhealthy()
    };
    (daemon_char, daemon_style)
}

#[cfg(feature = "tui")]
fn get_mcp_connection_counts(
    servers: &[crate::mcp_connections::McpServerConfig],
) -> (usize, usize) {
    let mut http_count = 0;
    let mut stdio_count = 0;
    for s in servers {
        if s.enabled {
            match &s.kind {
                crate::mcp_connections::McpServerKind::Http { .. } => http_count += 1,
                crate::mcp_connections::McpServerKind::Stdio { .. } => stdio_count += 1,
            }
        }
    }
    (http_count, stdio_count)
}

#[cfg(feature = "tui")]
fn draw_chat_header(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let mcp_label = get_mcp_label(state.mcp_enabled, state.unicode);
    let (health_char, health_style) =
        get_health_indicator(state.server_healthy, state.unicode, theme);
    let health_span = Span::styled(health_char, health_style);
    let (daemon_char, daemon_style) =
        get_daemon_indicator(state.daemon_healthy, state.unicode, theme);
    let daemon_span = Span::styled(daemon_char, daemon_style);

    let (http_count, stdio_count) = get_mcp_connection_counts(&state.mcp_connections.servers);
    let external_tools = state.mcp_connections.aggregate_tool_names().len();
    let external_part = if http_count > 0 || stdio_count > 0 {
        format!(" · ext (http:{http_count} stdio:{stdio_count}) / {external_tools} tools")
    } else {
        String::new()
    };

    let max_path_len = if area.width > 120 {
        35
    } else if area.width > 100 {
        25
    } else {
        15
    };
    let workspace_short = shorten_path(&state.workspace, max_path_len);
    let sandbox_style = match state.sandbox_status.as_str() {
        "LOCKED" => theme.success(),
        "INITIALIZING" => theme.pending(),
        "FAILED" => theme.failed(),
        _ => theme.unknown_health(),
    };
    let sandbox_part = if !state.workspace.is_empty() {
        format!(" · sandbox: {workspace_short}")
    } else {
        String::new()
    };
    let sandbox_status_part = if !state.sandbox_status.is_empty() {
        format!(" [{}]", state.sandbox_status)
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled(" ahma chat", theme.title()),
        Span::styled(
            format!("  {}", shorten_llm_label(&state.llm_label)),
            theme.normal(),
        ),
        Span::styled(mcp_label, theme.dim()),
        Span::styled(external_part, theme.dim()),
        Span::styled(sandbox_part, theme.dim()),
        Span::styled(sandbox_status_part, sandbox_style),
        health_span,
        Span::styled(" · ", theme.dim()),
        daemon_span,
        Span::styled(format!("  {}", state.transport_label), theme.dim()),
    ]);

    frame.render_widget(Paragraph::new(line).style(theme.header_bar()), area);
}

/// Word-wrap a single logical [`Line`] into one or more physical rows that each
/// fit within `width` columns, preserving every span's style. Words longer than
/// `width` are hard-split. This is the *authoritative* wrap used both for
/// rendering and for scroll-bounds math, so the two can never disagree — the
/// number of physical rows the chat occupies is exactly `wrap_line_to_rows(..).len()`.
///
/// Column accounting is by character count (ignoring double-width Unicode
/// codepoints), which matches typical ASCII/Latin chat content. Called every
/// frame against the live inner width, so it re-wraps automatically on resize.
#[cfg(feature = "tui")]
fn wrap_line_to_rows(line: &ratatui::text::Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);

    // Flatten the line into (char, style) cells so words can be re-segmented
    // across span boundaries while keeping each character's original style.
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            let style = s.style;
            s.content.chars().map(move |c| (c, style))
        })
        .collect();

    // Preserve blank lines as a single empty row (spacers between entries).
    if cells.is_empty() {
        return vec![Line::default()];
    }

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();

    let flush = |cur: &mut Vec<(char, Style)>, rows: &mut Vec<Vec<(char, Style)>>| {
        rows.push(std::mem::take(cur));
    };

    let mut i = 0;
    while i < cells.len() {
        let is_space = cells[i].0.is_whitespace();
        let start = i;
        while i < cells.len() && cells[i].0.is_whitespace() == is_space {
            i += 1;
        }
        let segment = &cells[start..i];

        if is_space {
            // Whitespace that fits stays on the line; whitespace that would spill
            // past the edge is dropped at the wrap point (so the next row does
            // not start with stray leading spaces).
            if cur.len() + segment.len() <= width {
                cur.extend_from_slice(segment);
            } else {
                flush(&mut cur, &mut rows);
            }
        } else if segment.len() <= width {
            // A word that fits on its own; move it to the next row if needed.
            if cur.len() + segment.len() > width && !cur.is_empty() {
                flush(&mut cur, &mut rows);
            }
            cur.extend_from_slice(segment);
        } else {
            // A word longer than the whole width: hard-split across rows.
            for &cell in segment {
                if cur.len() == width {
                    flush(&mut cur, &mut rows);
                }
                cur.push(cell);
            }
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }

    rows.into_iter().map(cells_to_line).collect()
}

/// Coalesce a row of (char, style) cells into a styled [`Line`], merging runs of
/// identical styles into single spans.
#[cfg(feature = "tui")]
fn cells_to_line(cells: Vec<(char, Style)>) -> Line<'static> {
    if cells.is_empty() {
        return Line::default();
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_style = cells[0].1;
    let mut cur_text = String::new();
    for (ch, style) in cells {
        if style == cur_style {
            cur_text.push(ch);
        } else {
            spans.push(Span::styled(std::mem::take(&mut cur_text), cur_style));
            cur_style = style;
            cur_text.push(ch);
        }
    }
    spans.push(Span::styled(cur_text, cur_style));
    Line::from(spans)
}

/// Flatten logical lines into the physical rows they occupy at `width`, in order.
#[cfg(feature = "tui")]
fn wrap_lines_to_rows(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    lines
        .iter()
        .flat_map(|l| wrap_line_to_rows(l, width))
        .collect()
}

/// Number of physical rows a logical [`Line`] occupies when wrapped at `width`.
#[cfg(feature = "tui")]
fn line_wrapped_rows(line: &ratatui::text::Line<'_>, width: usize) -> usize {
    wrap_line_to_rows(line, width).len()
}

/// Total wrapped physical rows for a slice of logical lines at the given width.
/// Returns 0 for an empty slice (used as a scroll position offset).
#[cfg(feature = "tui")]
fn total_wrapped_rows(lines: &[ratatui::text::Line<'_>], width: usize) -> usize {
    lines.iter().map(|l| line_wrapped_rows(l, width)).sum()
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

    state.chat_area.set(inner);

    if state.chat.is_empty() {
        state.chat_max_scroll.set(0);
        let hint = chat_history_hint(state);
        frame.render_widget(
            Paragraph::new(Span::styled(hint, theme.input_placeholder())),
            inner,
        );
        return;
    }

    let visible_h = inner.height as usize;
    // Reserve the rightmost column for the scrollbar so the wrap width is stable
    // whether or not the bar is currently visible — otherwise showing the bar
    // would re-wrap the text, which could change the row count and oscillate.
    let text_width = (inner.width as usize).saturating_sub(1).max(1);

    // Pre-wrap into physical rows at the *current* width, so one rendered row
    // equals one screen line. All scroll math is then in true screen rows and
    // re-derived every frame — a terminal resize immediately re-wraps and
    // re-bounds the scroll, and chat_scroll == 0 always shows the real bottom.
    let logical = build_chat_history_lines(state, theme, text_width);
    let rows = wrap_lines_to_rows(&logical, text_width);
    let max_scroll = rows.len().saturating_sub(visible_h);
    state.chat_max_scroll.set(max_scroll);

    let scroll = chat_history_scroll_offset(rows.len(), visible_h, state.chat_scroll);
    let visible_rows: Vec<Line<'static>> =
        rows.iter().skip(scroll).take(visible_h).cloned().collect();
    // Rows are already wrapped to `text_width`; render without ratatui's wrap so
    // the rendered height matches the row count exactly. Confine the paragraph to
    // the reserved text column width to leave room for the scrollbar.
    let text_area = Rect {
        width: inner.width.saturating_sub(1).max(1),
        ..inner
    };
    frame.render_widget(Paragraph::new(Text::from(visible_rows)), text_area);

    if rows.len() > visible_h {
        let sb = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        // Physical-row units so position, viewport, and content length all match
        // the scroll offset used for rendering (rows.len() / visible_h / scroll).
        let mut sb_state = ScrollbarState::new(rows.len())
            .viewport_content_length(visible_h)
            .position(scroll);
        frame.render_stateful_widget(sb, inner, &mut sb_state);
    }
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
        ChatEntry::User {
            text,
            started_at,
            duration_ms,
        } => {
            push_user_chat_lines(lines, text, *started_at, *duration_ms, theme, width);
        }
        ChatEntry::Assistant { content, streaming } => {
            push_assistant_chat_lines(lines, content, *streaming, state, theme);
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
fn format_duration(started_at: Option<std::time::Instant>, duration_ms: Option<u64>) -> String {
    if let Some(ms) = duration_ms {
        if ms < 1000 {
            format!("{}ms", ms)
        } else {
            format!("{}s", ms / 1000)
        }
    } else if let Some(start) = started_at {
        let elapsed = start.elapsed();
        let ms = elapsed.as_millis();
        if ms < 1000 {
            format!("{}ms", ms)
        } else {
            format!("{}s", elapsed.as_secs())
        }
    } else {
        String::new()
    }
}

#[cfg(feature = "tui")]
fn push_user_chat_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    started_at: Option<std::time::Instant>,
    duration_ms: Option<u64>,
    theme: &Theme,
    width: usize,
) {
    let dur_str = format_duration(started_at, duration_ms);

    let raw_lines: Vec<&str> = text.lines().collect();
    if raw_lines.is_empty() {
        return;
    }

    let first_line = raw_lines[0];
    let prefix = "you  ";

    if !dur_str.is_empty() && width > 10 {
        let prefix_len = prefix.chars().count();
        let dur_len = dur_str.chars().count();
        let max_text_len = width.saturating_sub(prefix_len + dur_len + 2); // leave margin

        if first_line.chars().count() <= max_text_len {
            let padding = width.saturating_sub(prefix_len + first_line.chars().count() + dur_len);
            lines.push(Line::from(vec![
                Span::styled(prefix, theme.dim()),
                Span::styled(first_line.to_string(), theme.normal()),
                Span::styled(" ".repeat(padding), theme.normal()),
                Span::styled(dur_str, theme.dim()),
            ]));
        } else {
            let first_part: String = first_line.chars().take(max_text_len).collect();
            let second_part: String = first_line.chars().skip(max_text_len).collect();

            let padding = width.saturating_sub(prefix_len + first_part.chars().count() + dur_len);
            lines.push(Line::from(vec![
                Span::styled(prefix, theme.dim()),
                Span::styled(first_part, theme.normal()),
                Span::styled(" ".repeat(padding), theme.normal()),
                Span::styled(dur_str, theme.dim()),
            ]));

            lines.push(Line::from(vec![
                Span::styled("     ", theme.dim()),
                Span::styled(second_part, theme.normal()),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled(prefix, theme.dim()),
            Span::styled(first_line.to_string(), theme.normal()),
        ]));
    }

    for line_str in raw_lines.iter().skip(1) {
        lines.push(Line::from(vec![
            Span::styled("     ", theme.dim()),
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

    // First-line prefix carries the liveness glyph while streaming (e.g. `⢷ ahma `)
    // and collapses the glyph to a space once the turn is done (` ahma `). The
    // continuation indent matches the 7-column prefix width so wrapped text stays
    // aligned under the response.
    let prefix = assistant_line_prefix(streaming, state);
    const CONT_INDENT: &str = "       "; // 7 spaces == width of "X ahma "

    for (index, line_str) in display.lines().enumerate() {
        if index == 0 {
            lines.push(Line::from(vec![
                Span::styled(prefix.clone(), theme.running()),
                Span::styled(line_str.to_string(), theme.normal()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(CONT_INDENT, theme.running()),
                Span::styled(line_str.to_string(), theme.normal()),
            ]));
        }
    }

    if display.is_empty() && streaming {
        lines.push(Line::from(vec![
            Span::styled(prefix, theme.running()),
            Span::styled(assistant_stream_cursor(true, state.unicode), theme.dim()),
        ]));
    }
}

/// Build the `ahma` response prefix. While the turn is live the leading glyph is
/// the random Braille liveness pulse (`state.liveness_glyph`); when complete it
/// collapses to a space so the column reads ` ahma`.
#[cfg(feature = "tui")]
fn assistant_line_prefix(streaming: bool, state: &AppState) -> String {
    let glyph = if streaming { state.liveness_glyph } else { ' ' };
    format!("{glyph} ahma ")
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

fn get_input_title_left(state: &AppState, theme: &Theme) -> Line<'static> {
    if state.llm_label == "no LLM" {
        Line::from(Span::styled(
            " ahma: no LLM — /provider to configure ",
            theme.title(),
        ))
        .left_aligned()
    } else {
        Line::from(Span::styled(
            format!(" ahma: {} ", state.llm_label),
            theme.title(),
        ))
        .left_aligned()
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

    let title_left = get_input_title_left(state, theme);
    let title_right = Line::from(Span::styled(
        format!(" sandbox: {} ", shorten_path(&state.workspace, 45)),
        theme.dim(),
    ))
    .right_aligned();

    let block = Block::default()
        .title(title_left)
        .title(title_right)
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(theme.input_bg());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    state.chat_input_area.set(area);

    let (cursor_row, cursor_col) = state.chat_input.cursor();
    let mut rendered_lines: Vec<String> = state.chat_input.lines().to_vec();
    let is_empty = rendered_lines.len() == 1 && rendered_lines[0].is_empty();

    if is_empty {
        let placeholder =
            "Type a message... ($/%/! sandboxed cmd · !! UNSANDBOXED · # decompose · / commands)";
        let text = if focused {
            let cursor = if state.unicode { "│" } else { "|" };
            format!("{}{}", cursor, placeholder)
        } else {
            placeholder.to_string()
        };
        let style = theme.input_placeholder().patch(theme.input_bg());
        let para = Paragraph::new(Span::styled(text, style)).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    } else {
        if focused {
            insert_input_cursor(&mut rendered_lines, cursor_row, cursor_col, state.unicode);
        }
        let text = rendered_lines.join("\n");
        let style = theme.normal().patch(theme.input_bg());
        let para = Paragraph::new(Span::styled(text, style)).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }
}

#[cfg(feature = "tui")]
fn draw_chat_footer(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    // Mode indicator at the left.
    let mode_label = match state.mode {
        Mode::Chat => "CHAT",
        Mode::Monitor => "MONITOR",
    };

    // Mode-specific key hints.
    let keys: &[(&str, &str)] = match state.mode {
        Mode::Chat => &[
            ("Enter", "send"),
            ("Shift+Enter", "newline"),
            ("/", "commands"),
            ("Tab", "monitor panels"),
            ("/quit", "quit"),
        ],
        Mode::Monitor => &[
            ("↑↓", "navigate ops"),
            ("Tab", "cycle panels"),
            ("Enter", "send chat"),
            ("/mode chat", "chat view"),
            ("q", "quit"),
        ],
    };

    let mut spans: Vec<Span> = vec![
        Span::styled(format!(" {mode_label} "), theme.footer_key()),
        Span::styled(" │", theme.dim()),
    ];
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
    if state.log_zoom_enabled {
        let [header_a, log_a, footer_a] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .areas(full);

        draw_chat_header(frame, state, theme, header_a);
        draw_log(frame, state, theme, log_a);
        draw_chat_footer(frame, state, theme, footer_a);
        return;
    }

    let approval_h: u16 = if let Some(gate) = &state.approval {
        // +2 for the rounded border (top/bottom). 2 content lines normally,
        // or 2 + blank + up to 9 diff lines when a diff is attached.
        if gate.diff.is_some() { 14 } else { 4 }
    } else {
        0
    };

    // Input height target: 1-6 lines based on wrapped content
    let inner_width = full.width.saturating_sub(2);
    let wrapped_line_count = state
        .chat_input_line_count(inner_width as usize)
        .clamp(1, 6);
    state
        .chat_input_height_target
        .set(wrapped_line_count as f64);

    let input_lines = state
        .chat_input_height_current
        .get()
        .round()
        .clamp(1.0, 6.0) as u16;
    let input_h = input_lines + 2; // borders

    let [
        header_a,
        monitor_a,
        approval_a,
        chat_history_a,
        input_a,
        footer_a,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Percentage(50),
        Constraint::Length(approval_h),
        Constraint::Min(4),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .areas(full);

    draw_chat_header(frame, state, theme, header_a);

    // Left panel is Operations (40% width), Right panel is vertical split of Detail (50% height) and Logs (50% height)
    let [ops_a, right_panel_a] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .areas(monitor_a);

    let [detail_a, log_a] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(right_panel_a);

    draw_ops_dag(frame, state, theme, ops_a);
    draw_detail(frame, state, theme, detail_a);
    draw_log(frame, state, theme, log_a);

    if state.approval.is_some() {
        draw_approval(frame, state, theme, approval_a);
    }

    draw_chat_history(frame, state, theme, chat_history_a);
    draw_input_box(frame, state, theme, input_a);
    draw_chat_footer(frame, state, theme, footer_a);

    if state.palette().is_some() {
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
    let Some(nav) = state.navigator() else {
        return;
    };
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
            format!("/ {}{cursor}", nav.input),
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
    if list_h == 0 || nav.completions.is_empty() {
        return;
    }
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, list_h);

    let desc_col = (inner.width as usize).saturating_sub(32).max(20);
    let items = navigator_list_items(
        &nav.completions,
        nav.selected,
        desc_col,
        inner.width as usize,
        list_h as usize,
        theme,
    );

    let mut list_state = ListState::default().with_selected(Some(nav.selected));
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

    let (filter_text, filter_style) = if picker.filter.is_empty() {
        (
            " filter: type to narrow".to_string(),
            theme.input_placeholder(),
        )
    } else {
        (format!(" filter: {}", picker.filter), theme.normal())
    };
    frame.render_widget(
        Paragraph::new(Span::styled(filter_text, filter_style)),
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
#[cfg(feature = "tui")]
fn format_tokens_part(state: &AppState) -> String {
    if state.token_usage.total_tokens > 0 {
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
    }
}

#[cfg(feature = "tui")]
fn format_external_part(state: &AppState) -> String {
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
    if http_count > 0 || stdio_count > 0 {
        format!(" · ext (http:{http_count} stdio:{stdio_count})/{external_tools}")
    } else {
        String::new()
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

    let max_path_len = if area.width > 120 {
        35
    } else if area.width > 100 {
        25
    } else {
        15
    };
    let workspace_short = shorten_path(&state.workspace, max_path_len);
    let external_part = format_external_part(state);
    let tokens_part = format_tokens_part(state);

    let daemon_char = match (state.daemon_healthy, state.unicode) {
        (true, true) => " · ● DMON",
        (true, false) => " · * DMON",
        (false, true) => " · ○ DMON",
        (false, false) => " · - DMON",
    };
    let daemon_style = if state.daemon_healthy {
        theme.healthy()
    } else {
        theme.unhealthy()
    };
    let daemon_span = Span::styled(daemon_char, daemon_style);

    let sandbox_part = if !state.workspace.is_empty() {
        format!(" · sandbox: {workspace_short}")
    } else {
        String::new()
    };
    let sandbox_status_part = if !state.sandbox_status.is_empty() {
        format!(" [{}]", state.sandbox_status)
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled(" ahma", theme.title()),
        Span::styled(session_part, theme.dim()),
        Span::styled(sandbox_part, theme.dim()),
        Span::styled(sandbox_status_part, sandbox_style),
        Span::styled(external_part, theme.dim()),
        Span::styled(tokens_part, theme.pending()),
        Span::styled(format!(" · {}", state.transport_label), theme.dim()),
        health_span,
        daemon_span,
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
                .map(|d| {
                    let ms = d.as_millis();
                    if ms < 1000 {
                        format!(" {ms}ms")
                    } else {
                        format!(" {}s", d.as_secs())
                    }
                })
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
        let mut sb_state = ScrollbarState::new(state.ai_activity.len())
            .viewport_content_length(visible_h)
            .position(scroll);
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

    state.ops_area.set(area);

    if state.operations.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  No active operations", theme.dim())),
            inner,
        );
        return;
    }

    let display_rows = inner.height as usize;
    let visible_ops = state.operations.len();

    // Auto-adjust scroll offset to keep selected in view
    let mut scroll = state.ops_scroll.get();
    if state.ops_selected < scroll {
        scroll = state.ops_selected;
    } else if state.ops_selected >= scroll + display_rows {
        scroll = state.ops_selected - display_rows + 1;
    }
    scroll = scroll.min(visible_ops.saturating_sub(display_rows));
    state.ops_scroll.set(scroll);

    let rows_to_draw = display_rows.min(visible_ops - scroll);

    for i in 0..rows_to_draw {
        let op_idx = scroll + i;
        let op = &state.operations[op_idx];
        let row_area = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);

        // We split row horizontally:
        // Details: Constraint::Min(5)
        // Pin button: Constraint::Length(5)
        // Cancel button: Constraint::Length(5)
        let [details_a, pin_a, cancel_a] = Layout::horizontal([
            Constraint::Min(5),
            Constraint::Length(5),
            Constraint::Length(5),
        ])
        .areas(row_area);

        // Render details
        let item = build_ops_dag_item(op_idx, op, state, theme, details_a.width as usize);
        frame.render_widget(Paragraph::new(item), details_a);

        // Register select click target
        state
            .click_targets
            .borrow_mut()
            .push((ClickTarget::SelectOperation(op_idx), details_a));

        // Render Pin button: "[P]"
        let pin_text = " [P] ";
        let pin_style = if op.pinned {
            theme.running()
        } else {
            theme.dim()
        };
        frame.render_widget(Paragraph::new(Span::styled(pin_text, pin_style)), pin_a);

        // Register pin click target
        state
            .click_targets
            .borrow_mut()
            .push((ClickTarget::PinOperation(op.id.clone()), pin_a));

        // Render Cancel button: "[X]"
        if !op.status.is_terminal() {
            frame.render_widget(
                Paragraph::new(Span::styled(" [X] ", theme.failed())),
                cancel_a,
            );
            // Register cancel click target
            state
                .click_targets
                .borrow_mut()
                .push((ClickTarget::CancelOperation(op.id.clone()), cancel_a));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_dag_metadata_spans(
    line_spans: &mut Vec<Span<'static>>,
    display_name: &str,
    row_style: Style,
    id_part: &str,
    instance_part: &str,
    elapsed_part: &str,
    rem_width: usize,
    theme: &Theme,
) {
    let meta_len_full = id_part.len() + instance_part.len() + elapsed_part.len();
    if rem_width.saturating_sub(meta_len_full) >= 15 {
        let name_max_len = rem_width.saturating_sub(meta_len_full);
        line_spans.push(Span::styled(
            truncate(display_name, name_max_len),
            row_style,
        ));
        line_spans.push(Span::styled(id_part.to_string(), theme.dim()));
        line_spans.push(Span::styled(instance_part.to_string(), theme.dim()));
        line_spans.push(Span::styled(elapsed_part.to_string(), theme.dim()));
        return;
    }

    let meta_len_no_inst = id_part.len() + elapsed_part.len();
    if rem_width.saturating_sub(meta_len_no_inst) >= 15 {
        let name_max_len = rem_width.saturating_sub(meta_len_no_inst);
        line_spans.push(Span::styled(
            truncate(display_name, name_max_len),
            row_style,
        ));
        line_spans.push(Span::styled(id_part.to_string(), theme.dim()));
        line_spans.push(Span::styled(elapsed_part.to_string(), theme.dim()));
        return;
    }

    let meta_len_no_id_no_inst = elapsed_part.len();
    if rem_width.saturating_sub(meta_len_no_id_no_inst) >= 10 {
        let name_max_len = rem_width.saturating_sub(meta_len_no_id_no_inst);
        line_spans.push(Span::styled(
            truncate(display_name, name_max_len),
            row_style,
        ));
        line_spans.push(Span::styled(elapsed_part.to_string(), theme.dim()));
        return;
    }

    line_spans.push(Span::styled(truncate(display_name, rem_width), row_style));
}

#[cfg(feature = "tui")]
fn build_ops_dag_item(
    index: usize,
    op: &crate::state::Operation,
    state: &AppState,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let is_selected = index == state.ops_selected;
    let sel_symbol = if is_selected {
        if state.unicode { "▶ " } else { "> " }
    } else {
        "  "
    };

    let prefix = match (op.parent_id.is_some(), state.unicode) {
        (true, true) => "└ ",
        (true, false) => "L ",
        (false, _) => "",
    };

    let clean_id_str = op.clean_id();
    let id_part = format!(" [{}]", clean_id_str);
    let elapsed_part = format!("  {}", op.elapsed_display());
    let instance_part = if let Some(label) = &op.instance_label {
        let pid_part = op.pid.map(|p| format!(":{}", p)).unwrap_or_default();
        format!(" [{}{}]", label, pid_part)
    } else {
        String::new()
    };

    let fixed_prefix_len = sel_symbol.len() + prefix.len() + 2;
    let rem_width = width.saturating_sub(fixed_prefix_len);

    let display_name = op.display_name();

    let mut line_spans = vec![
        Span::styled(
            sel_symbol,
            if is_selected {
                theme.selected_item()
            } else {
                theme.normal()
            },
        ),
        Span::styled(prefix, theme.dim()),
        Span::styled(
            format!("{} ", op.status.glyph(state.unicode)),
            theme.op_status_style(&op.status),
        ),
    ];

    let row_style = if index == state.ops_selected {
        theme.selected_item()
    } else {
        theme.normal()
    };

    push_dag_metadata_spans(
        &mut line_spans,
        &display_name,
        row_style,
        &id_part,
        &instance_part,
        &elapsed_part,
        rem_width,
        theme,
    );

    Line::from(line_spans)
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

    state.detail_area.set(area);

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
    let title = if let Some(label) = &op.instance_label {
        let pid_part = op.pid.map(|p| format!(":{}", p)).unwrap_or_default();
        format!(" {} [{}{}]  {} ", op.id, label, pid_part, op.tool_name)
    } else {
        format!(" {}  {} ", op.id, op.tool_name)
    };
    let block = Block::default()
        .title(Span::styled(title, theme.title()))
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let [header_row_a, rest_a] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    let [status_a, analyze_a] = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(11), // " [Analyze] "
    ])
    .areas(header_row_a);

    let status_line = Line::from(vec![
        Span::styled("status  ", theme.dim()),
        Span::styled(
            format!("{} {:?}", op.status.glyph(state.unicode), op.status),
            theme.op_status_style(&op.status),
        ),
        Span::styled(format!("  ({})", op.elapsed_display()), theme.dim()),
    ]);
    frame.render_widget(Paragraph::new(status_line), status_a);

    let analyze_btn = Span::styled(" [Analyze] ", theme.running());
    frame.render_widget(Paragraph::new(analyze_btn), analyze_a);

    // Register Analyze ClickTarget
    state
        .click_targets
        .borrow_mut()
        .push((ClickTarget::AnalyzeOperation(op.id.clone()), analyze_a));

    let lines = build_detail_lines(
        op,
        state,
        theme,
        rest_a.width as usize,
        rest_a.height as usize,
    );
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(para, rest_a);
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

    if !op.alerts.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Alerts:", theme.failed())));
        for alert in &op.alerts {
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {alert}"),
                theme.failed(),
            )));
        }
    }

    push_stdout_tail_lines(&mut lines, op, state, theme, width, height);
    lines
}

#[cfg(feature = "tui")]
fn push_detail_summary_lines(
    lines: &mut Vec<Line<'static>>,
    op: &crate::state::Operation,
    _state: &AppState,
    theme: &Theme,
    width: usize,
) {
    if let Some(cwd) = &op.cwd {
        lines.push(Line::from(vec![
            Span::styled("cwd     ", theme.dim()),
            Span::styled(shorten_path(cwd, 40), theme.normal()),
        ]));
    }
    if let Some(pid) = op.pid {
        lines.push(Line::from(vec![
            Span::styled("pid     ", theme.dim()),
            Span::styled(pid.to_string(), theme.normal()),
        ]));
    }
    if !op.args.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("args    ", theme.dim()),
            Span::styled(
                truncate(&op.args.join(" "), width.saturating_sub(10)),
                theme.normal(),
            ),
        ]));
    }
    if let Some(parent) = &op.parent_id {
        lines.push(Line::from(vec![
            Span::styled("waits   ", theme.dim()),
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
#[cfg(feature = "tui")]
fn get_file_display_lines(state: &AppState, theme: &Theme, inner_width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![];
    let max_width = inner_width.saturating_sub(1) as usize; // leave 1 col margin
    for raw_line in &state.active_log_lines {
        let clean_line = raw_line.replace('\t', "    ");
        if state.log_wrap_enabled && max_width > 0 {
            for sub_line in wrap_line(&clean_line, max_width) {
                lines.push(style_raw_log_line(&sub_line, theme));
            }
        } else {
            lines.push(style_raw_log_line(&clean_line, theme));
        }
    }
    lines
}

#[cfg(feature = "tui")]
fn get_system_display_lines(
    state: &AppState,
    theme: &Theme,
    inner_width: u16,
) -> Vec<Line<'static>> {
    let filtered = state.filtered_log();
    let mut lines = vec![];
    let max_width = inner_width.saturating_sub(1) as usize;
    for e in filtered {
        let ts = e.timestamp.format("%H:%M:%S").to_string();
        let level_label = e.level.label();
        let msg = &e.message;
        let full_line = format!("{} {} {}", ts, level_label, msg);
        if state.log_wrap_enabled && max_width > 0 {
            for sub_line in wrap_line(&full_line, max_width) {
                lines.push(style_system_log_line(&sub_line, &ts, level_label, theme));
            }
        } else {
            lines.push(style_system_log_line(&full_line, &ts, level_label, theme));
        }
    }
    lines
}

#[cfg(feature = "tui")]
fn render_empty_log_hint(frame: &mut Frame, state: &AppState, theme: &Theme, inner: Rect) {
    state.log_max_scroll.set(0);
    let hint = if state.active_log_file.is_none() {
        if state.log.is_empty() {
            "  Awaiting log events…"
        } else {
            "  No entries match filter"
        }
    } else {
        "  Log file is empty / awaiting data…"
    };
    frame.render_widget(Paragraph::new(Span::styled(hint, theme.dim())), inner);
}

#[cfg(feature = "tui")]
fn render_scrollbar(
    frame: &mut Frame,
    total_len: usize,
    visible_h: usize,
    scroll: usize,
    inner: Rect,
) {
    if total_len > visible_h {
        let sb = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let mut sb_state = ScrollbarState::new(total_len)
            .viewport_content_length(visible_h)
            .position(scroll);
        frame.render_stateful_widget(sb, inner, &mut sb_state);
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

    let log_title = if let Some(ref file) = state.active_log_file {
        format!(" Log: {} ", file)
    } else {
        " Log: system ".to_string()
    };
    let wrap_str = if state.log_wrap_enabled { "On" } else { "Off" };
    let zoom_str = if state.log_zoom_enabled { "On" } else { "Off" };
    let filter_indicator = log_filter_indicator(state);

    let block = Block::default()
        .title(Span::styled(
            format!(
                "{}{}[Wrap: {} | Zoom: {} | Press 'l' to switch] ",
                log_title, filter_indicator, wrap_str, zoom_str
            ),
            theme.title(),
        ))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    state.log_area.set(inner);

    // If active log file is selected and not approved, render warning banner
    if let Some(ref file) = state.active_log_file
        && let Some(info) = state.log_files.iter().find(|f| &f.name == file)
        && !info.is_approved
    {
        frame.render_widget(block, area);
        draw_blocked_symlink_banner(frame, file, info, theme, inner);
        return;
    }

    frame.render_widget(block, area);

    // Get lines to display
    let display_lines = if let Some(ref _file) = state.active_log_file {
        get_file_display_lines(state, theme, inner.width)
    } else {
        get_system_display_lines(state, theme, inner.width)
    };

    if display_lines.is_empty() {
        render_empty_log_hint(frame, state, theme, inner);
        return;
    }

    let visible_h = inner.height as usize;
    let max_scroll = display_lines.len().saturating_sub(visible_h);
    state.log_max_scroll.set(max_scroll);
    // While following, stay pinned to the newest line so freshly arrived output
    // is always visible at the bottom; otherwise honor the user's scroll offset.
    let scroll = if state.log_follow {
        max_scroll
    } else {
        state.log_scroll.min(max_scroll)
    };

    let visible_lines: Vec<Line> = display_lines
        .iter()
        .skip(scroll)
        .take(visible_h)
        .cloned()
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), inner);
    render_scrollbar(frame, display_lines.len(), visible_h, scroll, inner);
}

#[cfg(feature = "tui")]
fn draw_blocked_symlink_banner(
    frame: &mut Frame,
    _file: &str,
    info: &crate::state::LogFileInfo,
    theme: &Theme,
    area: Rect,
) {
    let target_str = info.symlink_target.as_deref().unwrap_or("unknown");
    let path_buf = std::path::PathBuf::from(&info.path);
    let parent = path_buf.parent().unwrap_or(std::path::Path::new(""));
    let target_path = std::path::PathBuf::from(target_str);
    let full_target = if target_path.is_absolute() {
        target_path
    } else {
        parent.join(target_path)
    };
    let full_target_str = full_target.to_string_lossy();

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  ⚠️ SECURITY WARNING: OUT-OF-SCOPE SYMLINK ⚠️",
            theme.failed().bold(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Log file "),
            Span::styled(&info.path, theme.normal().bold()),
            Span::raw(" is a symbolic link pointing to:"),
        ]),
        Line::from(Span::styled(
            format!("    {}", full_target_str),
            theme.failed(),
        )),
        Line::from(""),
        Line::from("  This destination lies outside your configured workspace sandbox scopes."),
        Line::from("  For security, reading out-of-scope files is blocked by default."),
        Line::from(""),
        Line::from(Span::styled(
            "  Press [a] to approve this symlink exception and start tailing.",
            theme.success().bold(),
        )),
        Line::from(""),
    ];
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

#[cfg(feature = "tui")]
fn build_log_file_list_item(
    f: &crate::state::LogFileInfo,
    is_active: bool,
    is_selected: bool,
    theme: &Theme,
) -> ListItem<'static> {
    let item_style = if is_selected {
        theme.normal().bg(Color::Cyan).fg(Color::Black)
    } else {
        theme.normal()
    };
    let active_marker = if is_active { "● " } else { "  " };
    let size_str = format_size(f.size_bytes);

    let (status_str, status_style) = if f.is_symlink {
        if f.is_approved {
            (" [Approved Symlink]", theme.success())
        } else {
            (" [Blocked Out-of-Scope]", theme.failed().bold())
        }
    } else {
        ("", theme.dim())
    };

    ListItem::new(Line::from(vec![
        Span::styled(active_marker, theme.success()),
        Span::styled(format!("{:<25}", f.name), item_style.bold()),
        Span::styled(format!(" {:>8}", size_str), item_style),
        Span::styled(
            status_str,
            if is_selected {
                item_style
            } else {
                status_style
            },
        ),
    ]))
}

#[cfg(feature = "tui")]
fn draw_log_files_modal(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let selected = state.log_files_selected().unwrap_or(0);
    let popup = centered_rect(70, 15, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" Log Switcher ", theme.title().bold()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut items = vec![];

    // Item 0: System Logs
    let is_active = state.active_log_file.is_none();
    let is_selected = selected == 0;
    let style = if is_selected {
        theme.normal().bg(Color::Cyan).fg(Color::Black)
    } else {
        theme.normal()
    };
    let active_marker = if is_active { "● " } else { "  " };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(active_marker, theme.success()),
        Span::styled("System Logs", style.bold()),
        Span::styled(" (internal warnings & info)", style),
    ])));

    // Item 1..N: Log Files
    for (i, f) in state.log_files.iter().enumerate() {
        let is_active = state.active_log_file.as_ref() == Some(&f.name);
        let is_selected = selected == i + 1;
        items.push(build_log_file_list_item(f, is_active, is_selected, theme));
    }

    let list = List::new(items);
    frame.render_widget(list, inner);
}

#[cfg(feature = "tui")]
fn style_raw_log_line<'a>(line: &str, theme: &Theme) -> Line<'a> {
    let line_upper = line.to_uppercase();
    let style = if line_upper.contains("ERROR") || line_upper.contains("ERR") {
        theme.failed()
    } else if line_upper.contains("WARN") || line_upper.contains("WARNING") {
        theme.pending()
    } else if line_upper.contains("INFO") {
        theme.success()
    } else if line_upper.contains("DEBUG") || line_upper.contains("TRACE") {
        theme.dim()
    } else {
        theme.normal()
    };
    Line::from(Span::styled(line.to_string(), style))
}

#[cfg(feature = "tui")]
fn style_system_log_line<'a>(line: &str, _ts: &str, level_label: &str, theme: &Theme) -> Line<'a> {
    let style = if level_label.contains("ERR") {
        theme.failed()
    } else if level_label.contains("WARN") {
        theme.pending()
    } else if level_label.contains("INFO") {
        theme.success()
    } else {
        theme.dim()
    };
    Line::from(Span::styled(line.to_string(), style))
}

#[cfg(feature = "tui")]
fn wrap_line(line: &str, max_width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut wrapped = vec![];
    let mut current = String::new();
    for c in line.chars() {
        current.push(c);
        if current.len() >= max_width {
            wrapped.push(current);
            current = String::new();
        }
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
    wrapped
}

#[cfg(feature = "tui")]
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
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
        .map(|s| format!("  ·  {s}s left"))
        .unwrap_or_default();

    let warn = if state.unicode { "⚠ " } else { "! " };

    // A red outline keeps the prompt unmistakable without a jarring full-bleed
    // background. The keyword lives in the title; details sit inside.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(theme.approval_border())
        .title(Span::styled(
            format!(" {warn}APPROVAL REQUIRED "),
            theme.approval_border(),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let desc = truncate(&gate.description, (inner.width as usize).saturating_sub(24));

    let note = gate
        .note
        .as_deref()
        .map(|n| format!("   ·  {n}"))
        .unwrap_or_default();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{}  ", gate.op_id), theme.dim()),
            Span::styled(desc, theme.normal()),
            Span::styled(note, theme.approval_note()),
            Span::styled(countdown, theme.dim()),
        ]),
        Line::from(vec![
            Span::styled("[y]", theme.approval_key()),
            Span::styled(" approve    ", theme.normal()),
            Span::styled("[a]", theme.approval_key()),
            Span::styled(" always allow    ", theme.normal()),
            Span::styled("[n]", theme.approval_key()),
            Span::styled(" reject", theme.normal()),
        ]),
    ];

    if let Some(diff) = &gate.diff {
        lines.push(Line::from(""));
        let budget = inner.height.saturating_sub(3) as usize;
        for diff_line in diff.lines().take(budget) {
            let style = if diff_line.starts_with('+') {
                theme.success()
            } else if diff_line.starts_with('-') {
                theme.failed()
            } else {
                theme.normal()
            };
            lines.push(Line::from(Span::styled(format!("  {}", diff_line), style)));
        }
    }

    let para = Paragraph::new(Text::from(lines));
    frame.render_widget(para, inner);
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

/// Format a slice of (key, description) pairs into styled [`Line`]s.
///
/// `key_width` controls the left-column padding so two-column and single-column
/// layouts can each use the width that fits their available space.
#[cfg(feature = "tui")]
fn format_help_rows<'a>(
    rows: &[(&'a str, &'a str)],
    key_width: usize,
    theme: &Theme,
) -> Vec<Line<'a>> {
    rows.iter()
        .map(|(key, desc)| {
            if key.is_empty() {
                Line::default()
            } else if desc.is_empty() {
                Line::from(Span::styled(format!(" {key}"), theme.title()))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {:<key_width$}", key), theme.footer_key()),
                    Span::styled(desc.to_string(), theme.normal()),
                ])
            }
        })
        .collect()
}

#[cfg(feature = "tui")]
fn draw_help(frame: &mut Frame, theme: &Theme, area: Rect) {
    let use_two_columns = area.width >= 100;

    let w = if use_two_columns {
        100u16.min(area.width)
    } else {
        62u16.min(area.width)
    };
    let h = if use_two_columns {
        35u16.min(area.height)
    } else {
        45u16.min(area.height)
    };
    let popup = centered_rect(w, h, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" Help — ahma TUI ", theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let left_rows: &[(&str, &str)] = &[
        ("GLOBAL", ""),
        ("q / Ctrl-C", "Quit"),
        ("Tab / Shift-Tab", "Cycle focus"),
        ("?", "Toggle this help"),
        ("Esc / ?", "Close help overlay"),
        (":", "Open command palette"),
        ("", ""),
        ("CHAT", ""),
        ("Enter", "Send message"),
        ("Shift+Enter", "Insert newline"),
        ("Esc", "Clear current input"),
        ("Arrows / Home / End", "Move within editor"),
        ("", ""),
        ("CHAT INPUT PREFIXES", ""),
        (
            "$ / % / ! <command>",
            "Run terminal command in sandbox (e.g. $ pwd)",
        ),
        (
            "!! <command>",
            "Run OUTSIDE sandbox — unrestricted, human-only (e.g. !! make install)",
        ),
        ("# <goal>", "Decompose goal using LLM"),
        ("/", "Open navigator (from empty input)"),
        ("", ""),
        ("PICKERS", ""),
        ("Type", "Filter providers/models"),
        ("Up / Down", "Move selection"),
        ("Enter / Esc", "Choose / cancel"),
        ("", ""),
        ("APPROVAL BANNER", ""),
        ("y", "Approve gate"),
        ("n", "Reject gate"),
    ];

    let right_rows: &[(&str, &str)] = &[
        ("COMMAND NAVIGATOR (/)", ""),
        ("Type", "Narrow commands/tools"),
        ("Tab", "Complete selected command"),
        ("Enter", "Run selected command"),
        ("/help, /?", "Show keyboard reference"),
        ("/run <tool> {json}", "Run tool with JSON args"),
        ("", ""),
        ("COMMAND PALETTE (:)", ""),
        ("Tab", "Next completion"),
        ("Enter", "Run command"),
        ("", ""),
        ("WINDOW ACTIONS", ""),
        ("/n", "Restore/expand window n"),
        ("/xn", "Close/cancel window n"),
        ("/quit", "Quit the application"),
        ("Mouse Click on Xn", "Close/cancel window"),
        ("Mouse Click on Window", "Toggle expand/collapse"),
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
        ("/analyze [op_id]", "Ask AI to analyze operation"),
        ("/monitor file <path>", "Start log monitoring"),
        ("", ""),
        ("LOG", ""),
        ("/", "Start filter (Esc to clear)"),
        ("j / k", "Scroll"),
        ("g / G", "Top / bottom"),
    ];

    let single_rows: &[(&str, &str)] = &[
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
        ("Esc", "Clear current input"),
        ("Arrow keys / Home / End", "Move within the editor"),
        ("", ""),
        ("CHAT INPUT PREFIXES", ""),
        (
            "$ or % or ! <command>",
            "Run terminal command in sandbox (e.g. $ pwd)",
        ),
        (
            "!! <command>",
            "Run OUTSIDE sandbox — unrestricted, human-only (e.g. !! make install)",
        ),
        ("# <goal>", "Decompose goal using LLM (e.g. # run tests)"),
        ("/", "Open command navigator (from empty input)"),
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
        ("/analyze [op_id]", "Ask AI to analyze operation"),
        ("/monitor file <path>", "Start log monitoring"),
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
        ("/help, /?", "Show keyboard reference"),
        ("/run <tool> {json}", "Run a tool manually with JSON args"),
        ("", ""),
        ("COMMAND PALETTE (:)", ""),
        ("Tab", "Next completion"),
        ("Enter", "Run command"),
        ("", ""),
        ("WINDOW ACTIONS", ""),
        ("/n", "Restore/expand window n (e.g. /3)"),
        ("/xn", "Close/cancel window n (e.g. /x3)"),
        ("/quit", "Quit the application"),
        ("Mouse Click on Xn", "Close/cancel window"),
        ("Mouse Click on Window", "Toggle expand/collapse"),
    ];

    if use_two_columns {
        let chunks = Layout::horizontal([
            Constraint::Percentage(49),
            Constraint::Length(2),
            Constraint::Percentage(49),
        ])
        .split(inner);

        let left_lines = format_help_rows(left_rows, 21, theme);
        let right_lines = format_help_rows(right_rows, 21, theme);

        let left_para = Paragraph::new(Text::from(left_lines)).wrap(Wrap { trim: false });
        let right_para = Paragraph::new(Text::from(right_lines)).wrap(Wrap { trim: false });

        let sep = Block::default()
            .borders(Borders::LEFT)
            .border_style(theme.border_unfocused());

        frame.render_widget(left_para, chunks[0]);
        frame.render_widget(sep, chunks[1]);
        frame.render_widget(right_para, chunks[2]);
    } else {
        let lines = format_help_rows(single_rows, 24, theme);
        let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }
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
    let Some(palette) = state.palette() else {
        return;
    };
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
    let input_display = format!("> {}{cursor}", palette.input);
    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(input_display, theme.running())),
        input_area,
    );

    if inner.height < 3 || palette.completions.is_empty() {
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
        &palette.completions,
        palette.selected_completion,
        list_h as usize,
        theme,
    );

    let mut list_state = ListState::default().with_selected(Some(palette.selected_completion));
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

pub(crate) fn shorten_llm_label(label: &str) -> String {
    if let Some((provider, model)) = label.split_once(" / ") {
        let provider = provider.trim();
        let model = model.trim();
        let model_clean = model.split(':').next().unwrap_or(model).trim();
        format!("{}/{}", provider, model_clean)
    } else {
        label.to_string()
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

#[cfg(feature = "tui")]
fn format_setting_value(value: &crate::settings_editor::SettingValue) -> String {
    match value {
        crate::settings_editor::SettingValue::Bool(v) => {
            if *v {
                "on".to_string()
            } else {
                "off".to_string()
            }
        }
        crate::settings_editor::SettingValue::String(v) => v.clone(),
        crate::settings_editor::SettingValue::U64(v) => format!("{}", v),
        crate::settings_editor::SettingValue::U32(v) => format!("{}", v),
        crate::settings_editor::SettingValue::Usize(v) => format!("{}", v),
        crate::settings_editor::SettingValue::StringList(v) => format!("[{}]", v.join(", ")),
    }
}

#[cfg(feature = "tui")]
fn draw_settings_panel(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let popup = centered_rect(90, 22, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" Settings (edit & persist) ", theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split inner area into sidebar (left) and content (right)
    let chunks = Layout::horizontal([Constraint::Length(20), Constraint::Min(20)]).split(inner);

    let sidebar_area = chunks[0];
    let content_area = chunks[1];

    // 1. Draw sidebar (categories list)
    let mut sidebar_items = vec![];
    use crate::settings_editor::SettingsCategory;
    for (i, cat) in SettingsCategory::ALL.iter().enumerate() {
        let is_selected = state.settings_editor.selected_category == i;
        let style = if is_selected {
            theme.title().bg(Color::DarkGray)
        } else {
            theme.normal()
        };
        let label = format!(" {} {}", cat.icon(), cat.label());
        sidebar_items.push(ListItem::new(Line::from(vec![Span::styled(label, style)])));
    }
    let sidebar_list = List::new(sidebar_items).block(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(theme.dim()),
    );
    frame.render_widget(sidebar_list, sidebar_area);

    // 2. Draw content pane (settings items for selected category)
    let selected_cat = SettingsCategory::ALL[state.settings_editor.selected_category];
    let items = state.settings_editor.items_for_category(selected_cat);

    let mut content_items = vec![];
    for (i, item) in items.iter().enumerate() {
        let is_selected = state.settings_editor.selected_item == i;
        let item_style = if is_selected {
            theme.selected_item()
        } else {
            theme.normal()
        };

        // Render value indicator
        let val_string = format_setting_value(&item.value);

        let sec_indicator = if item.security_tier {
            Span::styled(" [locked]", theme.dim())
        } else {
            Span::raw("")
        };

        let label_style = if is_selected {
            theme.title()
        } else {
            theme.normal()
        };

        let line = Line::from(vec![
            Span::styled(format!("  {: <25}", item.label), label_style),
            Span::styled(format!("  {: <15}", val_string), theme.success()),
            sec_indicator,
            Span::styled(format!("  — {}", item.description), theme.dim()),
        ]);

        content_items.push(ListItem::new(line).style(item_style));
    }

    // Split content area into items list (top) and footer/hints (bottom)
    let content_chunks =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(content_area);

    let list_area = content_chunks[0];
    let footer_area = content_chunks[1];

    let content_list = List::new(content_items);
    frame.render_widget(content_list, list_area);

    // Draw status message and action hints
    let status_str = if let Some((msg, _)) = &state.settings_editor.status_message {
        msg.clone()
    } else if state.settings_editor.dirty {
        "● Unsaved changes".to_string()
    } else {
        "".to_string()
    };

    let footer_line = Line::from(vec![
        Span::styled(format!("  {}", status_str), theme.pending()),
        Span::styled(
            "  [Space] Toggle  [r] Reset  [s] Save  [Esc/q] Close ",
            theme.dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer_line), footer_area);
}

// ─── Stub when `tui` feature is disabled ─────────────────────────────────────

/// No-op stub so the crate compiles without the `tui` feature.
#[cfg(not(feature = "tui"))]
pub fn draw(_frame: &mut (), _state: &AppState, _theme: &Theme) {}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;
    use ratatui::text::{Line, Span};

    fn make_line(text: &str) -> Line<'static> {
        Line::from(Span::raw(text.to_string()))
    }

    #[test]
    fn test_shorten_llm_label() {
        assert_eq!(shorten_llm_label("no LLM"), "no LLM");
        assert_eq!(
            shorten_llm_label("Ollama / qwen3.6:27b-mlx"),
            "Ollama/qwen3.6"
        );
        assert_eq!(
            shorten_llm_label("profile:my-profile / gemma2:latest"),
            "profile:my-profile/gemma2"
        );
        assert_eq!(
            shorten_llm_label("http://localhost:11434 / deepseek-coder:6.7b"),
            "http://localhost:11434/deepseek-coder"
        );
    }

    #[test]
    fn test_line_wrapped_rows_short() {
        // A line that fits in one row.
        let line = make_line("hello");
        assert_eq!(line_wrapped_rows(&line, 80), 1);
    }

    #[test]
    fn test_line_wrapped_rows_exact_fit() {
        // Exactly fills the width → 1 row.
        let line = make_line("abcde");
        assert_eq!(line_wrapped_rows(&line, 5), 1);
    }

    #[test]
    fn test_line_wrapped_rows_one_over() {
        // One char over the width → 2 rows.
        let line = make_line("abcdef");
        assert_eq!(line_wrapped_rows(&line, 5), 2);
    }

    #[test]
    fn test_line_wrapped_rows_empty() {
        let line = make_line("");
        assert_eq!(line_wrapped_rows(&line, 80), 1);
    }

    #[test]
    fn test_total_wrapped_rows_all_short() {
        // All lines fit in one row each.
        let lines: Vec<Line> = (0..5).map(|_| make_line("hi")).collect();
        assert_eq!(total_wrapped_rows(&lines, 80), 5);
    }

    #[test]
    fn test_total_wrapped_rows_wrapping() {
        // 3 short lines + 1 long line that wraps into 3 rows.
        let lines = vec![
            make_line("short"),
            make_line("short"),
            make_line("short"),
            make_line("a".repeat(25).as_str()), // 25 chars @ width=10 → 3 rows
        ];
        assert_eq!(total_wrapped_rows(&lines, 10), 6); // 3×1 + 3
    }

    #[test]
    fn test_scrollbar_position_at_top() {
        // scroll=0 means nothing above → position=0 in wrapped space.
        let lines: Vec<Line> = (0..20).map(|_| make_line("hi")).collect();
        let pos = total_wrapped_rows(&lines[..0], 80);
        assert_eq!(pos, 0); // empty slice → 0
    }

    #[test]
    fn test_scrollbar_position_at_max_scroll() {
        // At max scroll (visible_h=10, total=20): skip 10 lines.
        let lines: Vec<Line<'static>> = (0..20).map(|_| make_line("hi")).collect();
        let scroll = 10_usize;
        let total_w = total_wrapped_rows(&lines, 80);
        let pos_w = total_wrapped_rows(&lines[..scroll], 80);
        assert_eq!(total_w, 20);
        assert_eq!(pos_w, 10);
    }

    #[test]
    fn test_scrollbar_thumb_reaches_bottom_with_wrapped_content() {
        // 10 normal lines + 1 very long line (wraps into 5 rows at width=10).
        // At max scroll (scroll = 1 line from top so bottom is visible),
        // the wrapped position should be close to (total_wrapped - visible_h).
        let mut lines: Vec<Line<'static>> = (0..10).map(|_| make_line("short")).collect();
        lines.push(make_line(&"x".repeat(50))); // 50 chars @ width=10 → 5 rows
        let total_w = total_wrapped_rows(&lines, 10); // 10 + 5 = 15
        assert_eq!(total_w, 15);
        // If visible_h=5, max logical scroll = 11 - 5 = 6.
        // Scrolled to max (skip 6 logical lines), pos_w = 6.
        let pos_w = total_wrapped_rows(&lines[..6], 10);
        assert_eq!(pos_w, 6);
        // The thumb should NOT be at position 6 of a 10-line logical space (60%),
        // but at 6 of a 15-row wrapped space (40%). The important thing: pos_w < total_w.
        assert!(pos_w < total_w);
    }

    fn row_text(line: &Line) -> String {
        line.spans.iter().map(|s| &*s.content).collect()
    }

    #[test]
    fn wrap_line_word_boundary_keeps_whole_words() {
        // Greedy word wrap at width 8: "hello " (6) + "world" (5) overflows → wrap.
        let rows = wrap_line_to_rows(&make_line("hello world foo"), 8);
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["hello ", "world ", "foo"]);
    }

    #[test]
    fn wrap_line_hard_splits_overlong_word() {
        // A single word longer than the width is split at the column boundary.
        let rows = wrap_line_to_rows(&make_line(&"x".repeat(25)), 10);
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx"]);
    }

    #[test]
    fn wrap_line_blank_stays_one_row() {
        assert_eq!(wrap_line_to_rows(&Line::default(), 10).len(), 1);
        assert_eq!(wrap_line_to_rows(&make_line(""), 10).len(), 1);
    }

    #[test]
    fn wrap_preserves_span_styles_across_split() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        // 4 red + 4 blue, no spaces → one over-long "word" hard-split at width 4.
        let line = Line::from(vec![Span::styled("aaaa", red), Span::styled("bbbb", blue)]);
        let rows = wrap_line_to_rows(&line, 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(row_text(&rows[0]), "aaaa");
        assert!(rows[0].spans.iter().all(|s| s.style == red));
        assert_eq!(row_text(&rows[1]), "bbbb");
        assert!(rows[1].spans.iter().all(|s| s.style == blue));
    }

    #[test]
    fn no_row_exceeds_width_after_wrapping() {
        let logical = vec![
            make_line("ahma here is a fairly long assistant answer that must wrap"),
            make_line(&"verylongunbreakabletoken".repeat(3)),
        ];
        let width = 12;
        for row in wrap_lines_to_rows(&logical, width) {
            let len: usize = row.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(len <= width, "row {len:?} chars exceeds width {width}");
        }
    }

    /// Regression: with logical-line scroll math, pinning to the bottom
    /// (chat_scroll == 0) sliced the last `visible_h` *logical* lines, which —
    /// once word-wrapped — overflowed the viewport and clipped the end of the
    /// answer, with no way to scroll further down. After the fix the scroll math
    /// is in physical rows, so the true final row is always the last visible row.
    #[test]
    fn wrapped_bottom_row_reachable_at_scroll_zero() {
        let width = 10;
        let visible_h = 4;
        // A header line plus one long line that wraps into several rows.
        let logical = vec![make_line("ahma hi"), make_line(&"word ".repeat(10))];
        let rows = wrap_lines_to_rows(&logical, width);
        assert!(
            rows.len() > visible_h,
            "content must overflow the viewport for this test"
        );

        // chat_scroll == 0 → pinned to the newest content.
        let scroll = chat_history_scroll_offset(rows.len(), visible_h, 0);
        let visible: Vec<&Line> = rows.iter().skip(scroll).take(visible_h).collect();
        assert_eq!(visible.len(), visible_h);

        // The genuine final wrapped row is the last visible row — bottom reached.
        assert_eq!(
            row_text(visible.last().unwrap()),
            row_text(rows.last().unwrap())
        );
        // And every visible row fits, so nothing is clipped off the bottom edge.
        for row in &visible {
            let len: usize = row.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(len <= width);
        }
    }

    /// Documents the old bug directly: slicing the last `visible_h` *logical*
    /// lines and then wrapping produces MORE physical rows than the viewport can
    /// show, so the bottom would be clipped.
    #[test]
    fn logical_line_slice_overflows_viewport() {
        let width = 10;
        let visible_h = 4;
        let logical = [make_line("a"), make_line(&"x".repeat(50))]; // last → 5 rows
        let scroll = logical.len().saturating_sub(visible_h); // 0
        let slice: Vec<Line<'static>> = logical
            .iter()
            .skip(scroll)
            .take(visible_h)
            .cloned()
            .collect();
        let wrapped = wrap_lines_to_rows(&slice, width);
        assert!(
            wrapped.len() > visible_h,
            "old logical-line slice overflows the viewport (clips the bottom)"
        );
    }
}
