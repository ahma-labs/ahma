//! Ratatui rendering — all panel draw functions.
//!
//! The top-level [`draw`] function renders one chat-first layout: a header, the
//! chat body with its optional `/scope`, `/tasks` and `/log` sub-windows stacked
//! above it, the approval banner, the input box, and a footer — then whichever
//! single overlay is open on top (SPEC R23).

#[cfg(feature = "tui")]
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

use crate::state::{AppState, ChatEntry, ClickTarget, Focus, NavCommand};
use crate::theme::Theme;

// ─── Top-level draw ───────────────────────────────────────────────────────────

/// Called every redraw tick — the only public entry point in this module.
#[cfg(feature = "tui")]
pub fn draw(frame: &mut Frame, state: &AppState, theme: &Theme) {
    // Click targets describe *this* frame's screen and nothing else. They were
    // only ever cleared when an overlay happened to open, so they accumulated
    // for the life of the session: the vector grew without bound, every click
    // cloned it, and — because `handle_mouse_click` takes the first rect that
    // contains the point — a stale rect from an earlier frame could out-rank the
    // widget actually drawn there. Reset at the top of the frame that rebuilds
    // them. (The overlays clear again, to drop the layer they cover.)
    state.click_targets.borrow_mut().clear();

    draw_chat_layout(frame, state, theme);

    // Overlays drawn on top of whichever layout is active. At most one user
    // overlay is open (SPEC R23).
    let full = frame.area();
    match &state.modal {
        crate::state::ModalState::None => {}
        crate::state::ModalState::Help => draw_help(frame, state, theme, full),
        crate::state::ModalState::Navigator(_) => draw_navigator(frame, state, theme, full),
        crate::state::ModalState::ProviderPicker(picker)
        | crate::state::ModalState::ModelPicker(picker) => draw_picker(frame, picker, theme, full),
        crate::state::ModalState::LogFiles { .. } => {
            draw_log_files_modal(frame, state, theme, full)
        }
        crate::state::ModalState::OperationDetail(detail) => {
            draw_operation_detail(frame, state, detail, theme, full)
        }
        crate::state::ModalState::LogLineDetail(detail) => {
            draw_log_line_detail(frame, state, detail, theme, full)
        }
    }
    if state.settings_editor.open {
        draw_settings_panel(frame, state, theme, full);
    }
    // The scope-grant prompt is a security decision — draw it last so it sits on
    // top of every other overlay.
    if state.scope_grant.is_some() {
        draw_scope_grant_modal(frame, state, theme, full);
    }
    // The web-approval prompt is likewise a security decision; draw it on top too.
    if state.web_approval.is_some() {
        draw_web_approval_modal(frame, state, theme, full);
    }
}

// ─── Chat layout ──────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
struct RenderedWindowLayout {
    orig_idx: usize,
    height: u16,
    collapsed: bool,
    visible: bool,
    /// Error windows are never auto-collapsed by the layout budget — a failure
    /// the user has not looked at yet must not shrink to one line on its own.
    keep_open: bool,
}

/// Whether [`draw_expanded_window`] will prepend a `$ <command>` header and its
/// separator rule. The layout budget and the renderer must ask the *same*
/// question (SPEC R24.8.2) — when they disagreed, a window was sized for its
/// output rows only and then rendered two rows taller, pushing the tail off the
/// bottom. For a one-line command like `!pwd` that is the entire answer.
#[cfg(feature = "tui")]
fn window_has_command_header(w: &crate::state::TuiWindow) -> bool {
    !w.command.is_empty() && w.command != w.label
}

/// Logical line count [`draw_expanded_window`] renders inside the borders:
/// the optional command header (2 rows) plus one row per output line.
#[cfg(feature = "tui")]
fn expanded_window_line_count(w: &crate::state::TuiWindow) -> usize {
    let header = if window_has_command_header(w) { 2 } else { 0 };
    header + w.content.len()
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
                // +2 for the top/bottom border.
                (expanded_window_line_count(w) + 2).clamp(3, 8) as u16
            };
            RenderedWindowLayout {
                orig_idx: i,
                height: preferred_h,
                collapsed: w.collapsed,
                visible: true,
                keep_open: w.status == crate::state::WindowStatus::Error,
            }
        })
        .collect();

    // 1. Check if total height fits.
    let mut total_h: u16 = layouts.iter().map(|l| l.height).sum();
    if total_h <= max_h {
        return layouts;
    }

    // 2. Collapse expanded windows starting from the oldest.
    total_h = collapse_expanded_windows(&mut layouts, max_h, total_h);

    // 3. If it still doesn't fit, hide oldest windows.
    if total_h > max_h {
        hide_overflow_windows(&mut layouts, max_h, total_h);
    }

    layouts
}

/// Collapse expanded windows, oldest first, until the total height fits within
/// `max_h` (or every window is already collapsed). Returns the updated total height.
#[cfg(feature = "tui")]
fn collapse_expanded_windows(
    layouts: &mut [RenderedWindowLayout],
    max_h: u16,
    mut total_h: u16,
) -> u16 {
    for l in layouts.iter_mut() {
        if total_h <= max_h {
            break;
        }
        if !l.collapsed && !l.keep_open {
            let old_h = l.height;
            l.collapsed = true;
            l.height = 1;
            total_h = total_h - old_h + 1;
        }
    }
    total_h
}

/// Hide the oldest windows entirely (after collapsing was not enough) until the
/// total height fits within `max_h`.
#[cfg(feature = "tui")]
fn hide_overflow_windows(layouts: &mut [RenderedWindowLayout], max_h: u16, mut total_h: u16) {
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

/// Colour for the sandbox status chip. `NESTED: <host>` / `DEFERRED: <host>`
/// carry a variable host suffix, so match by prefix. Nested/deferred get their
/// own colour (ahma is not the sole authority — that must not look like
/// INITIALIZING, which merely means "starting up"); UNSANDBOXED/FAILED are
/// alarming.
fn sandbox_status_style(status: &str, theme: &Theme) -> Style {
    match status {
        "LOCKED" => theme.success(),
        "INITIALIZING" => theme.pending(),
        "FAILED" | "UNSANDBOXED" => theme.failed(),
        s if s.starts_with("NESTED") || s.starts_with("DEFERRED") => theme.host_authority(),
        _ => theme.unknown_health(),
    }
}

/// One-character outcome mark for a terminal window status.
fn status_glyph(status: crate::state::WindowStatus, unicode: bool) -> &'static str {
    use crate::state::WindowStatus;
    match (status, unicode) {
        (WindowStatus::Finished, true) => "✓",
        (WindowStatus::Finished, false) => "v",
        (WindowStatus::Error, true) => "✗",
        (WindowStatus::Error, false) => "x",
        (WindowStatus::Cancelled, true) => "⊘",
        (WindowStatus::Cancelled, false) => "~",
        (WindowStatus::Pending, true) => "…",
        (WindowStatus::Pending, false) => ".",
        (WindowStatus::Running, _) => "",
    }
}

/// `842ms` below one second, `2.1s` above — compact enough for a one-line row.
fn format_duration_short(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[cfg(feature = "tui")]
fn append_collapsed_running_spans(
    spans: &mut Vec<Span<'_>>,
    w: &crate::state::TuiWindow,
    area_width: usize,
    theme: &Theme,
    status_style: Style,
) {
    spans.push(Span::styled(
        format!("[Running {}] ", window_running_panel(w, theme.unicode)),
        status_style,
    ));
    spans.push(Span::styled(w.label.clone(), theme.normal()));
    if let Some(tail) = w.last_output_line() {
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let budget = area_width.saturating_sub(used + format!(" x{}", w.id).len());
        if budget > 4 {
            let sep = if theme.unicode { " — " } else { " - " };
            spans.push(Span::styled(
                format!("{sep}{}", truncate(tail, budget.saturating_sub(sep.len()))),
                theme.dim(),
            ));
        }
    }
}

#[cfg(feature = "tui")]
fn append_collapsed_terminal_spans(
    spans: &mut Vec<Span<'_>>,
    w: &crate::state::TuiWindow,
    theme: &Theme,
    status_style: Style,
) {
    spans.push(Span::styled(
        format!("{} ", status_glyph(w.status, theme.unicode)),
        status_style,
    ));
    if let Some(ms) = w.duration_ms {
        spans.push(Span::styled(
            format!("{} ", format_duration_short(ms)),
            theme.dim(),
        ));
    }
    spans.push(Span::styled(w.label.clone(), theme.dim()));
}

/// A collapsed window is one line. Running rows keep full contrast, animate,
/// and carry a live tail of the latest output so "what is it doing" is visible
/// without expanding. Terminal rows compress to a dim glyph + duration + title
/// summary that stays scannable in a stack of finished operations.
fn draw_collapsed_window(
    frame: &mut Frame,
    w: &crate::state::TuiWindow,
    area: Rect,
    theme: &Theme,
    status_style: Style,
) {
    let mut spans = vec![
        Span::styled(" [+] ", theme.dim()),
        Span::styled(format!("{} ", w.id), theme.normal()),
    ];
    if w.status == crate::state::WindowStatus::Running {
        append_collapsed_running_spans(&mut spans, w, area.width as usize, theme, status_style);
    } else {
        append_collapsed_terminal_spans(&mut spans, w, theme, status_style);
    }

    let left_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right_str = format!(" [x{}]", w.id);
    let pad_width = (area.width as usize).saturating_sub(left_len + right_str.len());
    if pad_width > 0 {
        spans.push(Span::raw(" ".repeat(pad_width)));
    }
    spans.push(Span::styled(right_str, theme.dim()));

    let line = Line::from(spans);
    let para = Paragraph::new(line);
    frame.render_widget(para, area);
}

/// Milliseconds since the epoch — the clock that drives stateless panels.
#[cfg(feature = "tui")]
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Activity panel for a running card: dots stream right→left — output flowing
/// in from the external process — fast while output is actually arriving,
/// slow heartbeat when the process is alive but quiet. Seeded per window so
/// concurrent cards animate independently.
#[cfg(feature = "tui")]
fn window_running_panel(w: &crate::state::TuiWindow, unicode: bool) -> String {
    let active = w
        .last_output_at
        .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2));
    let frame = wall_ms() / if active { 150 } else { 700 };
    crate::liveness::panel_glyphs(
        w.id as u64 + 1,
        frame,
        crate::liveness::PanelPattern::ScrollLeft,
        unicode,
    )
}

#[cfg(feature = "tui")]
fn build_window_title(w: &crate::state::TuiWindow, width: u16, unicode: bool) -> String {
    let border_width = 2;
    let title_space = (width as usize).saturating_sub(border_width);

    let status_str = if w.status == crate::state::WindowStatus::Running {
        format!("[Running {}]", window_running_panel(w, unicode))
    } else {
        format!("[{}]", w.status)
    };

    let title_left = format!(" [-] {} {} {}", w.id, status_str, w.label);
    let title_right = format!("[x{}] ", w.id);
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
    if window_has_command_header(w) {
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
        let style = output_line_style(line, theme);
        content_lines.push(Line::from(Span::styled(line.clone(), style)));
    }

    // Anchor to the *end* of the output (SPEC R24.8.3). A window is capped at 8
    // rows and long or wrapped output overflows it; rendering from the top then
    // shows the command echo and hides the result, which is the one thing the
    // window exists to report. The command itself is still in the border title.
    let scroll = total_wrapped_rows(&content_lines, inner.width as usize)
        .saturating_sub(inner.height as usize);
    let para = Paragraph::new(content_lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(para, inner);
}

/// Full-screen drill-in for one operation (Enter or click on a card/tree row).
///
/// Shows the complete identity (title, real command, cwd, instance, origin),
/// the outcome (status, exit code, duration), alerts, and a scrollable view of
/// the buffered output tail. Scroll state lives in [`OperationDetailState`];
/// the max offset is published through `state.detail_max_scroll` so the key
/// handlers can clamp without re-rendering.
#[cfg(feature = "tui")]
fn draw_operation_detail(
    frame: &mut Frame,
    state: &AppState,
    detail: &crate::state::OperationDetailState,
    theme: &Theme,
    area: Rect,
) {
    // The overlay owns the screen: drop click targets and window rects that
    // the layers underneath registered this frame so a click cannot reach a
    // covered card or tree row.
    state.click_targets.borrow_mut().clear();
    state.window_rects.borrow_mut().clear();

    frame.render_widget(Clear, area);

    let op = state.operations.iter().find(|o| o.id == detail.op_id);
    let title = match op {
        Some(op) => format!(" Operation — {} ", truncate(&op.display_name(), 60)),
        None => " Operation (no longer tracked) ".to_string(),
    };
    let block = Block::default()
        .title(Span::styled(title, theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    let Some(op) = op else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "This operation is gone (cleared or from a closed instance). Esc to close.",
                theme.dim(),
            ))),
            inner,
        );
        state.detail_max_scroll.set(0);
        return;
    };

    // Bottom row: action buttons + key hints; everything above scrolls.
    let [body_a, footer_a] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    // Reserve the scrollbar column *before* wrapping, whether or not the bar is
    // currently visible — otherwise showing the bar would re-wrap the text,
    // change the row count, and oscillate (same reasoning as the chat pane).
    let text_width = (body_a.width as usize).saturating_sub(1).max(1);
    let lines = operation_detail_lines(op, theme, text_width);
    // The paragraph wraps, so a scroll offset is measured in *rendered rows*,
    // not logical lines. Measuring with `lines.len()` left the last wrapped rows
    // unreachable whenever any line was wider than the pane.
    let total_rows = total_wrapped_rows(&lines, text_width);
    let max_scroll = total_rows.saturating_sub(body_a.height as usize);
    state.detail_max_scroll.set(max_scroll);
    let scroll = detail.scroll.min(max_scroll);

    let text_a = Rect {
        width: body_a.width.saturating_sub(1).max(1),
        ..body_a
    };
    let para = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(para, text_a);
    // Without this the overlay scrolled silently: no thumb, and no indication
    // that there was anything below the fold.
    draw_scrollbar(
        frame,
        theme,
        total_rows,
        body_a.height as usize,
        scroll,
        body_a,
    );

    // Footer: clickable actions on the left, key hints on the right.
    let is_live = matches!(
        op.status,
        crate::state::OpStatus::Running
            | crate::state::OpStatus::Pending
            | crate::state::OpStatus::Waiting
    );
    let cancel_btn = if is_live { " [Cancel] " } else { "" };
    let pin_btn = if op.pinned { " [Unpin] " } else { " [Pin] " };
    let analyze_btn = " [Analyze] ";

    let mut x = footer_a.x;
    let mut register = |label: &str, target: Option<ClickTarget>| -> Span<'static> {
        let w = label.chars().count() as u16;
        if let Some(t) = target
            && w > 0
        {
            let rect = Rect::new(x, footer_a.y, w.min(footer_a.width), 1);
            state.click_targets.borrow_mut().push((t, rect));
        }
        x += w;
        Span::styled(label.to_string(), theme.running())
    };
    let spans = vec![
        register(
            cancel_btn,
            (!cancel_btn.is_empty()).then(|| ClickTarget::CancelOperation(op.id.clone())),
        ),
        register(pin_btn, Some(ClickTarget::PinOperation(op.id.clone()))),
        register(
            analyze_btn,
            Some(ClickTarget::AnalyzeOperation(op.id.clone())),
        ),
        Span::styled(
            "  Esc close · j/k scroll · g/G top/bottom · c cancel",
            theme.dim(),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), footer_a);
}

/// Full-screen drill-in for one log line (click on it in the log pane) — the
/// reachability half of SPEC R24.8.4.
///
/// The log pane renders unwrapped by default, so any line wider than the pane
/// is silently cut at the edge — and the interesting part of a structured log
/// line (the message) sits *after* the pid/role/timestamp/level preamble, so
/// what gets cut is exactly what the reader wanted. This shows the line wrapped
/// and scrollable, reusing the scroll bookkeeping of the operation overlay.
#[cfg(feature = "tui")]
fn draw_log_line_detail(
    frame: &mut Frame,
    state: &AppState,
    detail: &crate::state::LogLineDetailState,
    theme: &Theme,
    area: Rect,
) {
    // The overlay owns the screen — see `draw_operation_detail`.
    state.click_targets.borrow_mut().clear();
    state.window_rects.borrow_mut().clear();

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(" Log line ", theme.title()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    let [body_a, footer_a] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    // Reserve the scrollbar column before wrapping, so toggling the bar cannot
    // re-wrap the text and oscillate the row count.
    let text_width = (body_a.width as usize).saturating_sub(1).max(1);
    let lines = vec![Line::from(Span::styled(
        detail.text.clone(),
        theme.normal(),
    ))];
    let total_rows = total_wrapped_rows(&lines, text_width);
    let max_scroll = total_rows.saturating_sub(body_a.height as usize);
    state.detail_max_scroll.set(max_scroll);
    let scroll = detail.scroll.min(max_scroll);

    let text_a = Rect {
        width: body_a.width.saturating_sub(1).max(1),
        ..body_a
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        text_a,
    );
    draw_scrollbar(
        frame,
        theme,
        total_rows,
        body_a.height as usize,
        scroll,
        body_a,
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Esc close · j/k scroll · g/G top/bottom",
            theme.dim(),
        ))),
        footer_a,
    );
}

/// The scrollable body of the operation detail overlay.
#[cfg(feature = "tui")]
fn operation_detail_lines(
    op: &crate::state::Operation,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let kv = |k: &str, v: String, style: Style| {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), theme.dim()),
            Span::styled(v, style),
        ])
    };

    let mut lines = vec![kv("id", op.id.clone(), theme.normal())];
    if let Some(instance) = &op.instance_label {
        lines.push(kv("instance", instance.clone(), theme.normal()));
    }
    if let Some(origin) = &op.origin {
        lines.push(kv("origin", origin.clone(), theme.normal()));
    }
    let mut status = format!("{:?}", op.status);
    if let Some(code) = op.exit_code {
        status.push_str(&format!(" (exit {code})"));
    }
    lines.push(kv("status", status, theme.op_status_style(&op.status)));
    lines.push(kv(
        "started",
        op.started_time.format("%Y-%m-%d %H:%M:%S").to_string(),
        theme.normal(),
    ));
    lines.push(kv("duration", op.elapsed_display(), theme.normal()));
    if let Some(cwd) = &op.cwd {
        lines.push(kv("cwd", cwd.clone(), theme.normal()));
    }
    if let Some(pid) = op.pid {
        lines.push(kv("pid", pid.to_string(), theme.normal()));
    }
    if let Some(cmd) = &op.command {
        // The full command, wrapped by the Paragraph — shown in full, this is
        // the detail view's reason to exist.
        lines.push(kv("command", format!("$ {cmd}"), theme.normal()));
    }
    for alert in &op.alerts {
        lines.push(kv("alert", format!("⚠ {alert}"), theme.failed()));
    }

    lines.push(Line::from(Span::styled(
        "─".repeat(width.max(1)),
        theme.dim(),
    )));

    if op.stdout_tail.is_empty() {
        lines.push(Line::from(Span::styled("(no output yet)", theme.dim())));
    } else {
        for out in &op.stdout_tail {
            lines.push(Line::from(Span::styled(out.clone(), theme.normal())));
        }
    }
    if let Some(summary) = &op.result_summary {
        lines.push(Line::from(Span::styled(
            "─".repeat(width.max(1)),
            theme.dim(),
        )));
        lines.push(Line::from(Span::styled(
            summary.clone(),
            theme.op_status_style(&op.status),
        )));
    }
    lines
}

/// Style for one line of window output, based on well-known status prefixes
/// ("Starting", "Finished successfully", "Failed", "Cancelled") or separators.
fn output_line_style(line: &str, theme: &Theme) -> Style {
    if line.starts_with("Starting") || line.starts_with("Started at") {
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
    }
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
fn compute_chat_input_height(state: &AppState, full_width: u16) -> u16 {
    let inner_width = full_width.saturating_sub(2);
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
    input_lines + 2 // borders
}

#[cfg(feature = "tui")]
fn draw_zoomed_chat_pane(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    chat_a: Rect,
    zoom: Focus,
) {
    match zoom {
        Focus::OpsDag => {
            state.ops_area.set(chat_a);
            draw_ops_dag(frame, state, theme, chat_a);
        }
        Focus::Log => {
            state.log_area.set(chat_a);
            draw_log(frame, state, theme, chat_a);
        }
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn draw_unzoomed_chat_layout(frame: &mut Frame, state: &AppState, theme: &Theme, chat_a: Rect) {
    let mut constraints = Vec::new();
    let show_scope = state.scope_window_open;
    let show_tasks = state.tasks_window_open;
    let show_log = state.log_window_open;

    if show_scope {
        // Sized to content (honest panes, R24.8: budget the rows the renderer
        // draws); scope_window_lines caps itself rather than relying on clipping.
        let scope_h = scope_window_height(state, chat_a);
        constraints.push(Constraint::Length(scope_h));
    }
    if show_tasks {
        let tasks_h = (chat_a.height / 3).clamp(6, 16);
        constraints.push(Constraint::Length(tasks_h));
    }
    if show_log {
        let log_h = (chat_a.height / 3).clamp(6, 16);
        constraints.push(Constraint::Length(log_h));
    }
    constraints.push(Constraint::Min(4));

    let areas = Layout::vertical(constraints).split(chat_a);
    let mut idx = 0;
    if show_scope {
        let area = areas[idx];
        idx += 1;
        draw_scope_window(frame, state, theme, area);
    }
    if show_tasks {
        let area = areas[idx];
        idx += 1;
        state.ops_area.set(area);
        draw_ops_dag(frame, state, theme, area);
    } else {
        state.ops_area.set(Rect::default());
    }
    if show_log {
        let area = areas[idx];
        idx += 1;
        state.log_area.set(area);
        draw_log(frame, state, theme, area);
    } else {
        state.log_area.set(Rect::default());
    }

    let chat_content_area = areas[idx];
    state.chat_area.set(chat_content_area);

    let visible_count = state.windows.iter().filter(|w| w.visible).count();
    let (history_area, windows_area, layouts) = if visible_count > 0 {
        let max_w_h = chat_content_area.height.saturating_sub(4);
        let layouts = compute_window_layouts(&state.windows, max_w_h);
        let total_w_h: u16 = layouts.iter().filter(|l| l.visible).map(|l| l.height).sum();
        let [h_area, w_area] =
            Layout::vertical([Constraint::Min(4), Constraint::Length(total_w_h)])
                .areas(chat_content_area);
        (h_area, w_area, layouts)
    } else {
        (chat_content_area, Rect::default(), vec![])
    };

    draw_chat_history(frame, state, theme, history_area);
    draw_windows_layout(frame, state, theme, windows_area, &layouts);
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

    let input_h = compute_chat_input_height(state, full.width);

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

    if let Some(zoom) = state.zoomed {
        draw_zoomed_chat_pane(frame, state, theme, chat_a, zoom);
    } else {
        draw_unzoomed_chat_layout(frame, state, theme, chat_a);
    }

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
    // Losing the server says so in a word. A one-character glyph flip is not a
    // state change a user notices, and the chat input keeps looking live
    // meanwhile — so the disconnected case is spelled out.
    match (server_healthy, unicode) {
        (true, true) => (" ●", theme.healthy()),
        (true, false) => (" *", theme.healthy()),
        (false, true) => (" ○ OFFLINE", theme.unhealthy()),
        (false, false) => (" - OFFLINE", theme.unhealthy()),
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
fn max_header_workspace_len(width: u16) -> usize {
    if width > 120 {
        35
    } else if width > 100 {
        25
    } else {
        15
    }
}

/// Total number of external MCP tools, without materialising the formatted,
/// sorted tool-name list ([`McpConnectionManager::aggregate_tools`]) — this
/// runs on every rendered frame, where only the count matters.
#[cfg(feature = "tui")]
fn external_tool_count(state: &AppState) -> usize {
    state
        .mcp_connections
        .tools_by_server
        .values()
        .map(Vec::len)
        .sum()
}

#[cfg(feature = "tui")]
fn format_external_tools_part(state: &AppState) -> String {
    let (http_count, stdio_count) = get_mcp_connection_counts(&state.mcp_connections.servers);
    // Only the count is needed — summing per-server lengths avoids cloning and
    // sorting every ToolInfo on each rendered frame.
    let external_tools = external_tool_count(state);
    if http_count > 0 || stdio_count > 0 {
        format!(" · ext (http:{http_count} stdio:{stdio_count}) / {external_tools} tools")
    } else {
        String::new()
    }
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

    let external_part = format_external_tools_part(state);
    let max_path_len = max_header_workspace_len(area.width);
    let sandbox_style = sandbox_status_style(&state.sandbox_status, theme);
    // Honest scope display (SPEC R5.4): once the server reports its locked
    // scope, show *that* — not the TUI's launch directory, which can differ
    // (container-root fallback, auto-narrowing). Before the report arrives the
    // launch path is only a guess, so mark it as such instead of presenting it
    // as the boundary.
    let sandbox_part = match state.locked_scope_root() {
        Some(root) => {
            let extra = state
                .sandbox_scope
                .as_ref()
                .map(|s| s.write.len().saturating_sub(1))
                .unwrap_or(0);
            let short = shorten_path(root, max_path_len);
            if extra > 0 {
                format!(" · sandbox: {short} +{extra}")
            } else {
                format!(" · sandbox: {short}")
            }
        }
        None if !state.workspace.is_empty() => {
            let short = shorten_path(&state.workspace, max_path_len);
            format!(" · sandbox: {short}?")
        }
        None => String::new(),
    };
    let sandbox_status_part = if !state.sandbox_status.is_empty() {
        format!(" [{}]", state.sandbox_status)
    } else {
        String::new()
    };

    let active_skills_part = if !state.active_skills.is_empty() {
        format!(" · skills:{}", state.active_skills.len())
    } else {
        String::new()
    };

    // Token spend and context fill — the numbers that decide whether to keep
    // chatting or /compact. Computed all along but only ever rendered by a
    // header that had no callers; narrow terminals get the context-fill share
    // alone, since that is the part that changes a decision.
    let tokens_part = if area.width > 100 {
        format_tokens_part(state)
    } else {
        context_fill_segment(state)
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
        Span::styled(active_skills_part, theme.normal()),
        Span::styled(tokens_part, theme.dim()),
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

    let mut i = 0;
    while i < cells.len() {
        let is_space = cells[i].0.is_whitespace();
        let start = i;
        while i < cells.len() && cells[i].0.is_whitespace() == is_space {
            i += 1;
        }
        let segment = &cells[start..i];

        if is_space {
            wrap_whitespace_segment(&mut cur, &mut rows, segment, width);
        } else {
            wrap_word_segment(&mut cur, &mut rows, segment, width);
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }

    rows.into_iter().map(cells_to_line).collect()
}

/// Append a run of whitespace cells to the row under construction.
///
/// Whitespace that fits stays on the line; whitespace that would spill past
/// the edge is dropped at the wrap point (so the next row does not start
/// with stray leading spaces).
#[cfg(feature = "tui")]
fn wrap_whitespace_segment(
    cur: &mut Vec<(char, Style)>,
    rows: &mut Vec<Vec<(char, Style)>>,
    segment: &[(char, Style)],
    width: usize,
) {
    if cur.len() + segment.len() <= width {
        cur.extend_from_slice(segment);
    } else {
        rows.push(std::mem::take(cur));
    }
}

/// Append a run of non-whitespace cells (a "word") to the row under
/// construction, wrapping to a new row as needed. Words longer than the
/// whole width are hard-split across rows.
#[cfg(feature = "tui")]
fn wrap_word_segment(
    cur: &mut Vec<(char, Style)>,
    rows: &mut Vec<Vec<(char, Style)>>,
    segment: &[(char, Style)],
    width: usize,
) {
    if segment.len() <= width {
        if cur.len() + segment.len() > width && !cur.is_empty() {
            rows.push(std::mem::take(cur));
        }
        cur.extend_from_slice(segment);
        return;
    }
    for &cell in segment {
        if cur.len() == width {
            rows.push(std::mem::take(cur));
        }
        cur.push(cell);
    }
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

/// Render a vertical scrollbar on the right edge of `inner` whose thumb length
/// is proportional to the visible window (`visible_h`) over the total content
/// (`total_len`), positioned at `scroll`. All three values are in the same
/// units the caller scrolls in (physical rows for wrapped text, list items
/// otherwise), so the thumb stays proportional as content grows and re-bounds
/// when a resize re-wraps the content. The thumb is painted as a solid filled
/// cell instead of ratatui's default `█` glyph, which renders as dashes under
/// terminal line-spacing. No bar is drawn when everything already fits.
///
/// Scroll position must be truthful (SPEC R24.8.1): `ScrollbarState::content_length`
/// is the number of **scroll positions**, not content rows, because ratatui places
/// the thumb at `position / (content_length - 1 + viewport_content_length)` of the
/// track. Feed it the position count so "scrolled to the end" paints the thumb
/// flush against the bottom.
#[cfg(feature = "tui")]
fn draw_scrollbar(
    frame: &mut Frame,
    theme: &Theme,
    total_len: usize,
    visible_h: usize,
    scroll: usize,
    inner: Rect,
) {
    if total_len <= visible_h {
        return;
    }
    let sb = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_symbol(" ")
        .thumb_style(theme.scrollbar_thumb())
        .track_symbol(Some(" "))
        .track_style(theme.scrollbar_track());
    let positions = total_len - visible_h + 1;
    let mut sb_state = ScrollbarState::new(positions)
        .viewport_content_length(visible_h)
        .position(scroll.min(positions - 1));
    frame.render_stateful_widget(sb, inner, &mut sb_state);
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

    // Physical-row units so position, viewport, and content length all match
    // the scroll offset used for rendering (rows.len() / visible_h / scroll).
    draw_scrollbar(frame, theme, rows.len(), visible_h, scroll, inner);
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

    let entries = state.chat.entries();
    let count = entries.len();
    for (idx, entry) in entries.iter().enumerate() {
        let is_last = idx + 1 == count;
        push_chat_entry_lines(&mut lines, entry, is_last, state, theme, width);
        lines.push(Line::default());
    }

    lines
}

#[cfg(feature = "tui")]
fn push_chat_entry_lines(
    lines: &mut Vec<Line<'static>>,
    entry: &ChatEntry,
    is_last: bool,
    state: &AppState,
    theme: &Theme,
    width: usize,
) {
    match entry {
        ChatEntry::User {
            text,
            payload: _,
            started_at,
            duration_ms,
        } => {
            push_user_chat_lines(lines, text, *started_at, *duration_ms, theme, width);
        }
        ChatEntry::Thinking {
            content,
            streaming: _,
        } => {
            let active = is_last && state.liveness_state == crate::state::LivenessState::Thinking;
            push_thinking_chat_lines(lines, content, active, state, theme);
        }
        ChatEntry::Assistant {
            content,
            streaming: _,
        } => {
            let active = is_last && state.liveness_state == crate::state::LivenessState::Streaming;
            push_assistant_chat_lines(lines, content, active, state, theme);
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
    const CONT_INDENT: &str = "        "; // 8 spaces == width of "GG ahma "

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

/// Render a reasoning/"thinking" block entirely in lower-contrast (dim) style so
/// the user can see the model is thinking — and read it if they care — without it
/// competing with the actual answer. Prefixed `X think ` (8 cols) where `X` is the
/// live Braille pulse while streaming.
fn push_thinking_chat_lines(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    streaming: bool,
    state: &AppState,
    theme: &Theme,
) {
    let cursor = assistant_stream_cursor(streaming, state.unicode);
    let display = format!("{content}{cursor}");
    let glyph: &str = if streaming {
        &state.liveness_glyph
    } else {
        "  "
    };
    let prefix = format!("{glyph} think ");
    const CONT_INDENT: &str = "         "; // 9 spaces == width of "GG think "

    for (index, line_str) in display.lines().enumerate() {
        let pfx = if index == 0 {
            prefix.clone()
        } else {
            CONT_INDENT.to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(pfx, theme.dim()),
            Span::styled(line_str.to_string(), theme.dim()),
        ]));
    }
}

/// Build the `ahma` response prefix. While the turn is live the leading glyph is
/// the random Braille liveness pulse (`state.liveness_glyph`); when complete it
/// collapses to a space so the column reads ` ahma`.
#[cfg(feature = "tui")]
fn assistant_line_prefix(streaming: bool, state: &AppState) -> String {
    let glyph: &str = if streaming {
        &state.liveness_glyph
    } else {
        "  "
    };
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
    // Same honesty rule as the header: prefer the server-locked scope; the
    // launch path is a guess until then and is marked with `?`.
    let title_right = Line::from(Span::styled(
        match state.locked_scope_root() {
            Some(root) => format!(" sandbox: {} ", shorten_path(root, 45)),
            None => format!(" sandbox: {}? ", shorten_path(&state.workspace, 45)),
        },
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

    let para = if is_empty {
        input_placeholder_paragraph(theme, focused, state.unicode)
    } else {
        if focused {
            insert_input_cursor(&mut rendered_lines, cursor_row, cursor_col, state.unicode);
        }
        input_text_paragraph(theme, &rendered_lines)
    };
    frame.render_widget(para, inner);
}

/// Paragraph shown when the chat input is empty: the placeholder hint, with a
/// leading cursor glyph when focused.
#[cfg(feature = "tui")]
fn input_placeholder_paragraph(theme: &Theme, focused: bool, unicode: bool) -> Paragraph<'static> {
    let placeholder = "Type a message... (! UNSANDBOXED cmd · # decompose · / commands)";
    let text = if focused {
        let cursor = if unicode { "│" } else { "|" };
        format!("{}{}", cursor, placeholder)
    } else {
        placeholder.to_string()
    };
    let style = theme.input_placeholder().patch(theme.input_bg());
    Paragraph::new(Span::styled(text, style)).wrap(Wrap { trim: false })
}

/// Paragraph rendering the current (non-empty) chat input lines.
#[cfg(feature = "tui")]
fn input_text_paragraph(theme: &Theme, rendered_lines: &[String]) -> Paragraph<'static> {
    let text = rendered_lines.join("\n");
    let style = theme.normal().patch(theme.input_bg());
    Paragraph::new(Span::styled(text, style)).wrap(Wrap { trim: false })
}

#[cfg(feature = "tui")]
fn draw_chat_footer(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let mode_label = "AHMA";

    let keys: &[(&str, &str)] = match state.focus {
        Focus::OpsDag => &[
            ("↑↓", "nav ops"),
            ("Space", "fold/unfold"),
            ("Tab", "cycle panels"),
            ("Esc", "focus chat"),
            ("/quit", "quit"),
        ],
        Focus::Log => &[
            ("↑↓", "scroll"),
            ("w", "wrap"),
            ("l", "files"),
            ("Tab", "cycle panels"),
            ("Esc", "focus chat"),
            ("/quit", "quit"),
        ],
        _ => &[
            ("Enter", "send"),
            ("Shift+Enter", "newline"),
            ("/", "commands"),
            ("?", "help"),
            ("/tasks", "tasks view"),
            ("/log", "log view"),
            ("/scope", "sandbox"),
            ("/quit", "quit"),
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

/// Content lines for the `/scope` window — the persistent TUI scope panel
/// (SPEC R5.4(b), R-PERM.5.1). Mirrors the vocabulary of the server's canonical
/// `ScopeView::render_text` (write/read/tmp/source) so every surface reads the
/// same, and states who is actually protecting the session (SPEC R7.5).
#[cfg(feature = "tui")]
fn scope_window_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    const MAX_ROOTS_SHOWN: usize = 4;
    let mut lines: Vec<Line<'static>> = Vec::new();

    if let Some(reason) = &state.sandbox_failed_reason {
        lines.push(Line::from(Span::styled(
            format!("FAILED: {reason}"),
            theme.failed(),
        )));
        lines.push(Line::from(Span::styled(
            "Tool calls are refused until a scope locks. Fix: open a workspace folder, pass \
             --sandbox-scope <path>, or configure [sandbox] container_root; then restart.",
            theme.dim(),
        )));
        return lines;
    }

    let Some(scope) = &state.sandbox_scope else {
        let status = if state.sandbox_status.is_empty() {
            "UNKNOWN"
        } else {
            state.sandbox_status.as_str()
        };
        let explanation = match status {
            "INITIALIZING" => {
                "The server is still negotiating scope (roots/list or elicitation). \
                 Tool calls are held until it locks."
            }
            _ => {
                "No sandbox report received on this connection yet \
                 (daemon-only attach, or an older server)."
            }
        };
        lines.push(Line::from(Span::styled(
            format!("Scope not reported — status: {status}"),
            theme.pending(),
        )));
        lines.push(Line::from(Span::styled(explanation, theme.dim())));

        // A hub-only attach never receives `sandbox/configured`, but every
        // registered instance reports the scope it is running under. Showing
        // that beats showing nothing — clearly attributed, because it is the
        // instance's own claim rather than a scope this TUI saw locked, and it
        // carries no enforcement or provenance information.
        for inst in state.active_instances.iter().take(3) {
            lines.push(Line::from(vec![
                Span::styled("reported: ", theme.dim()),
                Span::styled(shorten_path(&inst.scope, 46), theme.normal()),
                Span::styled(format!("  by {} ({})", inst.label, inst.mode), theme.dim()),
            ]));
        }
        return lines;
    };

    // Who is actually protecting this session. NESTED/DEFERRED/DISABLED must
    // read differently from plain enforcement — R7.5's honesty rule.
    let (authority, authority_style) = match scope.active.as_deref() {
        Some("ahma_nested_in_host") => (
            format!(
                "ahma (kernel), nested inside {}",
                scope.host.as_deref().unwrap_or("a host sandbox")
            ),
            theme.host_authority(),
        ),
        Some("deferred_to_host") => (
            format!(
                "{} — ahma is NOT enforcing",
                scope.host.as_deref().unwrap_or("host sandbox")
            ),
            theme.host_authority(),
        ),
        Some("disabled") => ("NONE — no kernel confinement".to_string(), theme.failed()),
        _ => ("ahma (kernel)".to_string(), theme.success()),
    };
    let enforcement_style = if scope.enforced {
        theme.success()
    } else {
        theme.failed()
    };
    let enforcement = if scope.enforced {
        "ENFORCED"
    } else {
        "DISABLED"
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{enforcement} · "), enforcement_style),
        Span::styled("authority: ", theme.dim()),
        Span::styled(authority, authority_style),
    ]));

    if scope.write.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("write : ", theme.dim()),
            Span::styled("(none — awaiting scope)", theme.pending()),
        ]));
    } else {
        for (i, root) in scope.write.iter().take(MAX_ROOTS_SHOWN).enumerate() {
            let label = if i == 0 { "write : " } else { "        " };
            lines.push(Line::from(vec![
                Span::styled(label, theme.dim()),
                Span::styled(shorten_path(root, 70), theme.normal()),
            ]));
        }
        if scope.write.len() > MAX_ROOTS_SHOWN {
            lines.push(Line::from(Span::styled(
                format!(
                    "        … +{} more (run `ahma status` for the full list)",
                    scope.write.len() - MAX_ROOTS_SHOWN
                ),
                theme.dim(),
            )));
        }
    }

    let read_summary = if scope.read.is_empty() {
        "(none beyond write roots)".to_string()
    } else if scope.read.len() <= 2 {
        scope
            .read
            .iter()
            .map(|p| shorten_path(p, 34))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!(
            "{} +{} more",
            shorten_path(&scope.read[0], 34),
            scope.read.len() - 1
        )
    };
    lines.push(Line::from(vec![
        Span::styled("read  : ", theme.dim()),
        Span::styled(read_summary, theme.normal()),
    ]));

    lines.push(Line::from(vec![
        Span::styled("tmp   : ", theme.dim()),
        Span::styled(
            if scope.tmp { "ON" } else { "OFF" }.to_string(),
            theme.normal(),
        ),
        Span::styled(" · source: ", theme.dim()),
        Span::styled(
            scope.source.clone().unwrap_or_else(|| "unknown".into()),
            theme.normal(),
        ),
    ]));

    // The roots that exist because the user granted them, named separately
    // from the workspace so an out-of-workspace writable path is explicable —
    // and reversible: the revoke command is stated, not left to be searched
    // for (SPEC R5.4.6, grants are inspectable and reversible by name).
    if !state.granted_scopes.is_empty() {
        let shown = state
            .granted_scopes
            .iter()
            .take(2)
            .map(|(path, access)| format!("{} ({access})", shorten_path(path, 30)))
            .collect::<Vec<_>>()
            .join(", ");
        let more = state.granted_scopes.len().saturating_sub(2);
        let suffix = if more > 0 {
            format!(" +{more} more")
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled("grants: ", theme.dim()),
            Span::styled(format!("{shown}{suffix}"), theme.normal()),
        ]));
        lines.push(Line::from(Span::styled(
            "        `ahma sandbox list` / `ahma sandbox revoke <path>` to review or remove",
            theme.dim(),
        )));
    }

    // Platform limitation that cannot be expressed as scope (macOS reads
    // unconfined) — shown here per SPEC R-PERM.5.1, not buried in docs.
    if let Some(note) = &scope.platform_note {
        lines.push(Line::from(Span::styled(
            format!("note  : {}", truncate(note, 110)),
            theme.pending(),
        )));
    }
    // Persistent R7 disclosure when ahma is not the sole authority — the log
    // line scrolls away; this window is where it stays visible.
    if matches!(
        scope.active.as_deref(),
        Some("ahma_nested_in_host") | Some("deferred_to_host") | Some("disabled")
    ) && let Some(d) = &scope.disclosure
    {
        lines.push(Line::from(Span::styled(truncate(d, 110), theme.pending())));
    }
    lines
}

/// Height for the `/scope` window: sized to its content (+2 border rows),
/// never more than half the available area — honest panes (R24.8) budget the
/// rows the renderer actually draws.
#[cfg(feature = "tui")]
fn scope_window_height(state: &AppState, area: Rect) -> u16 {
    let theme = Theme::new(state.unicode);
    let content = scope_window_lines(state, &theme).len() as u16;
    (content + 2).clamp(4, (area.height / 2).max(4))
}

/// The persistent sandbox-scope sub-window, toggled with `/scope`. Informational
/// only (no focus, no scrolling): the content self-caps and names the overflow.
#[cfg(feature = "tui")]
fn draw_scope_window(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let block = Block::default()
        .title(Span::styled(" Sandbox · scope ", theme.title()))
        .title(Line::from(Span::styled(" /scope closes ", theme.dim())).right_aligned())
        .borders(Borders::ALL)
        .border_style(theme.border_unfocused());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(scope_window_lines(state, theme)), inner);
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
            let desc_str = truncate(&cmd.description, inner_width.saturating_sub(desc_col + 2));
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

/// Approximate characters per token, matching the agent's budget math.
#[cfg(feature = "tui")]
const STATUS_CHARS_PER_TOKEN: usize = 4;

/// Estimates a token count from a character count using the
/// ~[`STATUS_CHARS_PER_TOKEN`] chars/token heuristic.
#[cfg(feature = "tui")]
fn estimate_tokens(chars: usize) -> u32 {
    (chars / STATUS_CHARS_PER_TOKEN) as u32
}

#[cfg(feature = "tui")]
fn format_tokens_part(state: &AppState) -> String {
    token_status_segment(
        state.token_usage.total_tokens,
        state.token_usage.prompt_tokens,
        state.token_usage.completion_tokens,
        state.last_prompt_tokens,
        conversation_chars(state),
        state.token_prefs.context_length,
    )
}

/// Just the `· NN% ctx` share of [`token_status_segment`], for headers too
/// narrow to carry the full in/out/total breakdown. Empty when the model's
/// context window is unknown — a percentage of an unknown whole is noise.
#[cfg(feature = "tui")]
fn context_fill_segment(state: &AppState) -> String {
    let Some(window) = state.token_prefs.context_length.filter(|&w| w > 0) else {
        return String::new();
    };
    let used = if state.last_prompt_tokens > 0 {
        state.last_prompt_tokens
    } else {
        estimate_tokens(conversation_chars(state))
    };
    if used == 0 {
        return String::new();
    }
    let pct = ((used as f64 / window as f64) * 100.0).round() as u32;
    format!(" · {}% ctx", pct.min(999))
}

/// Total characters of the visible conversation — the basis for a token estimate
/// when the provider does not report usage, and for the context-fill fallback.
#[cfg(feature = "tui")]
fn conversation_chars(state: &AppState) -> usize {
    use crate::state::ChatEntry;
    state
        .chat
        .entries()
        .iter()
        .map(|e| match e {
            // Count what the LLM actually receives: a `/skill` invocation sends
            // its payload, not the short displayed command.
            ChatEntry::User { text, payload, .. } => payload.as_deref().unwrap_or(text).len(),
            ChatEntry::Thinking { content, .. } | ChatEntry::Assistant { content, .. } => {
                content.len()
            }
            ChatEntry::ToolCall { args, result, .. } => {
                args.len() + result.as_ref().map_or(0, |r| r.len())
            }
        })
        .sum()
}

/// Abbreviate a token count (e.g. `1.2k`).
#[cfg(feature = "tui")]
fn fmt_token_count(n: u32) -> String {
    if n > 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Build the status-bar token segment. Pure, for testability.
///
/// - Exact cumulative usage when the provider reports it (`total_tokens > 0`);
///   otherwise a `~est` derived from the conversation size, so providers that
///   return no `usage` still show a counter.
/// - A best-effort context-window fill `%` when the window is known: exact from
///   the last turn's prompt tokens when available, else estimated.
#[cfg(feature = "tui")]
fn token_status_segment(
    total_tokens: u32,
    prompt_tokens: u32,
    completion_tokens: u32,
    last_prompt_tokens: u32,
    conversation_chars: usize,
    ctx_window: Option<u32>,
) -> String {
    let est_conv = estimate_tokens(conversation_chars);
    if total_tokens == 0 && est_conv == 0 {
        return String::new();
    }

    let mut out = if total_tokens > 0 {
        format!(
            " · tkns {} in / {} out ({} ttl)",
            fmt_token_count(prompt_tokens),
            fmt_token_count(completion_tokens),
            fmt_token_count(total_tokens),
        )
    } else {
        format!(" · ~{} tkns est", fmt_token_count(est_conv))
    };

    if let Some(window) = ctx_window.filter(|&w| w > 0) {
        let used = if last_prompt_tokens > 0 {
            last_prompt_tokens
        } else {
            est_conv
        };
        let pct = ((used as f64 / window as f64) * 100.0).round() as u32;
        out.push_str(&format!(" · {}% ctx", pct.min(999)));
    }
    out
}

// ─── Operations DAG ───────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn draw_task_tree_row(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    row: &crate::task_tree::TreeRow,
    row_idx: usize,
    row_area: Rect,
    is_selected: bool,
) {
    use crate::task_tree::RowKind;
    match &row.kind {
        RowKind::Instance {
            label,
            detail,
            counts,
            collapsed,
            ..
        } => draw_instance_tree_row(
            frame,
            state,
            theme,
            row_idx,
            row_area,
            is_selected,
            label,
            detail,
            counts,
            *collapsed,
        ),
        RowKind::Group {
            label, collapsed, ..
        } => draw_group_tree_row(
            frame,
            state,
            theme,
            row_idx,
            row_area,
            is_selected,
            label,
            *collapsed,
        ),
        RowKind::Output { text, .. } => {
            draw_output_tree_row(frame, state, theme, row_idx, row_area, row.depth, text)
        }
        RowKind::Op { op_index, expanded } => draw_op_tree_row(
            frame,
            state,
            theme,
            row_idx,
            row_area,
            row.depth,
            *op_index,
            *expanded,
            is_selected,
        ),
    }
}

#[cfg(feature = "tui")]
fn draw_ops_dag(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    use crate::task_tree::{TreeOptions, build_rows};

    let focused = state.focus == Focus::OpsDag;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let title = if state.show_all_projects {
        " Tasks · all projects — [f] this project "
    } else {
        " Tasks · this project — [f] all "
    };
    let block = Block::default()
        .title(Span::styled(title, theme.title()))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    state.ops_area.set(area);

    // Rebuild the tree for this frame; key/mouse handlers resolve the
    // selection through the stored rows.
    {
        let rows = build_rows(
            &state.operations,
            &TreeOptions {
                instances: &state.active_instances,
                project_root: state.project_root.as_deref(),
                show_all: state.show_all_projects,
                expanded_op: state.expanded_op.as_deref(),
                collapsed: &state.collapsed_nodes,
            },
        );
        *state.task_rows.borrow_mut() = rows;
    }
    let rows = state.task_rows.borrow();

    if rows.is_empty() {
        let hint = if state.show_all_projects {
            "  No tasks yet — connected clients appear here as they work"
        } else {
            "  No tasks in this project yet — [f] shows all projects"
        };
        frame.render_widget(Paragraph::new(Span::styled(hint, theme.dim())), inner);
        return;
    }

    let display_rows = inner.height as usize;
    let selected = state.ops_selected.min(rows.len() - 1);

    // Auto-adjust scroll offset to keep the selected row in view.
    let scroll = compute_ops_scroll(selected, state.ops_scroll.get(), display_rows, rows.len());
    state.ops_scroll.set(scroll);

    let rows_to_draw = display_rows.min(rows.len() - scroll);

    // Reserve the scrollbar column when the tree overflows (SPEC R24.8.1). Row
    // count does not depend on width here — these are list rows, not wrapped
    // text — so a conditional reservation cannot oscillate, and a tree that
    // fits keeps its full width.
    let overflows = rows.len() > display_rows;
    let row_width = if overflows {
        inner.width.saturating_sub(1).max(1)
    } else {
        inner.width
    };

    for i in 0..rows_to_draw {
        let row_idx = scroll + i;
        let row = &rows[row_idx];
        let row_area = Rect::new(inner.x, inner.y + i as u16, row_width, 1);
        let is_selected = row_idx == selected;
        draw_task_tree_row(frame, state, theme, row, row_idx, row_area, is_selected);
    }

    draw_scrollbar(frame, theme, rows.len(), display_rows, scroll, inner);
}

/// Adjust the tree's scroll offset so the selected row stays within the
/// visible window, clamped to the available row count.
#[cfg(feature = "tui")]
fn compute_ops_scroll(
    selected: usize,
    current_scroll: usize,
    display_rows: usize,
    total_rows: usize,
) -> usize {
    let mut scroll = current_scroll;
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + display_rows {
        scroll = selected - display_rows + 1;
    }
    scroll.min(total_rows.saturating_sub(display_rows))
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn draw_instance_tree_row(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    row_idx: usize,
    row_area: Rect,
    is_selected: bool,
    label: &str,
    detail: &str,
    counts: &crate::task_tree::GroupCounts,
    collapsed: bool,
) {
    let line = build_instance_row(
        label,
        detail,
        counts,
        collapsed,
        is_selected,
        state.unicode,
        theme,
        row_area.width as usize,
    );
    frame.render_widget(Paragraph::new(line), row_area);
    state
        .click_targets
        .borrow_mut()
        .push((ClickTarget::TreeRow(row_idx), row_area));
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn draw_group_tree_row(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    row_idx: usize,
    row_area: Rect,
    is_selected: bool,
    label: &str,
    collapsed: bool,
) {
    let marker = fold_marker(collapsed, state.unicode);
    let sel = selection_marker(is_selected, state.unicode);
    let text = truncate(&format!("{sel}  {marker} {label}"), row_area.width as usize);
    let style = if is_selected {
        theme.selected_item()
    } else {
        theme.dim()
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), row_area);
    state
        .click_targets
        .borrow_mut()
        .push((ClickTarget::TreeRow(row_idx), row_area));
}

#[cfg(feature = "tui")]
fn draw_output_tree_row(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    row_idx: usize,
    row_area: Rect,
    depth: u8,
    text: &str,
) {
    let indent = "  ".repeat(depth as usize);
    let bar = if state.unicode { "│ " } else { "| " };
    let line = truncate(&format!("  {indent}{bar}{text}"), row_area.width as usize);
    frame.render_widget(Paragraph::new(Span::styled(line, theme.dim())), row_area);
    state
        .click_targets
        .borrow_mut()
        .push((ClickTarget::TreeRow(row_idx), row_area));
}

#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn draw_op_tree_row(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    row_idx: usize,
    row_area: Rect,
    row_depth: u8,
    op_index: usize,
    expanded: bool,
    is_selected: bool,
) {
    let Some(op) = state.operations.get(op_index) else {
        return;
    };
    let [details_a, pin_a, cancel_a] = Layout::horizontal([
        Constraint::Min(5),
        Constraint::Length(5),
        Constraint::Length(5),
    ])
    .areas(row_area);

    let item = build_tree_op_item(
        op,
        row_depth,
        expanded,
        is_selected,
        state,
        theme,
        details_a.width as usize,
    );
    frame.render_widget(Paragraph::new(item), details_a);
    state
        .click_targets
        .borrow_mut()
        .push((ClickTarget::TreeRow(row_idx), details_a));

    let pin_style = if op.pinned {
        theme.running()
    } else {
        theme.dim()
    };
    frame.render_widget(Paragraph::new(Span::styled(" [P] ", pin_style)), pin_a);
    state
        .click_targets
        .borrow_mut()
        .push((ClickTarget::PinOperation(op.id.clone()), pin_a));

    if !op.status.is_terminal() {
        frame.render_widget(
            Paragraph::new(Span::styled(" [X] ", theme.failed())),
            cancel_a,
        );
        state
            .click_targets
            .borrow_mut()
            .push((ClickTarget::CancelOperation(op.id.clone()), cancel_a));
    }
}

#[cfg(feature = "tui")]
fn fold_marker(collapsed: bool, unicode: bool) -> &'static str {
    match (collapsed, unicode) {
        (true, true) => "▸",
        (false, true) => "▾",
        (true, false) => ">",
        (false, false) => "v",
    }
}

#[cfg(feature = "tui")]
fn selection_marker(selected: bool, unicode: bool) -> &'static str {
    match (selected, unicode) {
        (true, true) => "▶",
        (true, false) => ">",
        (false, _) => " ",
    }
}

/// Instance header: `▾ claude-code · stdio · …/github/ahma   2▶ 1⧗ 14✓ 1✗`
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn build_instance_row(
    label: &str,
    detail: &str,
    counts: &crate::task_tree::GroupCounts,
    collapsed: bool,
    is_selected: bool,
    unicode: bool,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let sel = selection_marker(is_selected, unicode);
    let marker = fold_marker(collapsed, unicode);

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            format!("{sel} {marker} "),
            if is_selected {
                theme.selected_item()
            } else {
                theme.dim()
            },
        ),
        Span::styled(
            label.to_string(),
            if is_selected {
                theme.selected_item()
            } else {
                theme.title()
            },
        ),
    ];
    if counts.running > 0 {
        // Aggregate activity: mixed directions (CrissCross) on the slow
        // heartbeat — several things happening under this instance at once.
        let seed = label
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        spans.push(Span::styled(
            format!(
                " {}",
                crate::liveness::panel_glyphs(
                    seed,
                    wall_ms() / 700,
                    crate::liveness::PanelPattern::CrissCross,
                    unicode,
                )
            ),
            theme.running(),
        ));
    }
    if !detail.is_empty() {
        spans.push(Span::styled(format!("  {detail}"), theme.dim()));
    }

    // Right-hand tallies: how much is being done in parallel, at a glance.
    let (r, q, s, f) = (
        counts.running,
        counts.queued,
        counts.succeeded,
        counts.failed,
    );
    let mut tallies: Vec<(usize, &str, Style)> = Vec::new();
    let (run_g, que_g, ok_g, fail_g) = if unicode {
        ("⟳", "◷", "✓", "✗")
    } else {
        (">", ".", "v", "x")
    };
    if r > 0 {
        tallies.push((r, run_g, theme.running()));
    }
    if q > 0 {
        tallies.push((q, que_g, theme.dim()));
    }
    if s > 0 {
        tallies.push((s, ok_g, theme.success()));
    }
    if f > 0 {
        tallies.push((f, fail_g, theme.failed()));
    }
    if counts.total() == 0 {
        spans.push(Span::styled("  idle".to_string(), theme.dim()));
    } else {
        for (n, glyph, style) in tallies {
            spans.push(Span::styled(format!("  {n}{glyph}"), style));
        }
    }

    truncate_row_spans_to_width(&mut spans, width);
    Line::from(spans)
}

/// Rough width guard: truncate the detail span first, then the label span if
/// necessary, so the row fits within `width` columns.
#[cfg(feature = "tui")]
fn truncate_row_spans_to_width(spans: &mut [Span<'static>], width: usize) {
    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= width || width <= 8 {
        return;
    }
    let mut over = total - width;

    // Try truncating the detail span (index 2) first
    if spans.len() > 2 {
        let det = spans[2].content.to_string();
        let det_len = det.chars().count();
        if det_len > over + 5 {
            let keep = det_len - over;
            spans[2] = Span::styled(truncate(&det, keep), spans[2].style);
            over = 0;
        } else if det_len > 5 {
            spans[2] = Span::styled(truncate(&det, 4), spans[2].style);
            let new_total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            over = new_total.saturating_sub(width);
        }
    }

    // If still over, truncate the label (index 1)
    if over > 0 {
        let lbl = spans[1].content.to_string();
        let keep = lbl.chars().count().saturating_sub(over + 1);
        spans[1] = Span::styled(truncate(&lbl, keep.max(4)), spans[1].style);
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

/// One operation row of the task tree: indent by depth, status glyph, name,
/// id, elapsed. The owning instance is the header above, so no instance tag.
#[cfg(feature = "tui")]
#[allow(clippy::too_many_arguments)]
fn build_tree_op_item(
    op: &crate::state::Operation,
    depth: u8,
    expanded: bool,
    is_selected: bool,
    state: &AppState,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let sel_symbol = if is_selected {
        if state.unicode { "▶ " } else { "> " }
    } else {
        "  "
    };
    let indent = "  ".repeat(depth.max(1) as usize - 1);
    let expand_mark = match (expanded, state.unicode) {
        (true, true) => "▾ ",
        (true, false) => "v ",
        (false, _) => "",
    };

    let clean_id_str = op.clean_id();
    let id_part = format!(" [{}]", clean_id_str);
    // A finished row shows *how* it finished, not just how long it took. "Failed"
    // without a code is not actionable; `exit 101` is (SPEC R24.7). Operations that
    // are not processes have no code, and say nothing rather than inventing one.
    // A denial is not a failure and must not read as one: name the path the
    // kernel refused, and the key that asks for it (SPEC R-PERM.7/.7.1). This
    // is the row a user acts on, so the affordance belongs on the row.
    let elapsed_part = match (&op.denial, op.exit_code) {
        (Some((path, access)), _) if op.status == crate::state::OpStatus::Denied => {
            format!("  denied: {} ({access}) · [a] ask", shorten_path(path, 28))
        }
        (_, Some(code)) if op.status.is_terminal() => {
            format!("  exit {code} · {}", op.elapsed_display())
        }
        _ => format!("  {}", op.elapsed_display()),
    };

    let fixed_prefix_len = sel_symbol.len() + indent.len() + expand_mark.len() + 2;
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
        Span::styled(indent, theme.dim()),
        Span::styled(
            format!("{} ", op.status.glyph(state.unicode)),
            theme.op_status_style(&op.status),
        ),
        Span::styled(expand_mark, theme.dim()),
    ];

    let row_style = if is_selected {
        theme.selected_item()
    } else {
        theme.normal()
    };

    push_dag_metadata_spans(
        &mut line_spans,
        &display_name,
        row_style,
        &id_part,
        "",
        &elapsed_part,
        rem_width,
        theme,
    );

    Line::from(line_spans)
}

// ─── Detail pane ──────────────────────────────────────────────────────────────

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

/// Title text for the log panel: active file (or "system"), current filter
/// indicator, and the wrap/zoom toggle states.
#[cfg(feature = "tui")]
fn build_log_title(state: &AppState) -> String {
    let log_title = if let Some(ref file) = state.active_log_file {
        format!(" Log: {} ", file)
    } else {
        " Log: system ".to_string()
    };
    let wrap_str = if state.log_wrap_enabled { "On" } else { "Off" };
    let zoom_str = if state.zoomed == Some(Focus::Log) {
        "On"
    } else {
        "Off"
    };
    // While tail-following, a rain panel pours at the rate log lines arrive
    // (each line advances one frame; a slow clock term keeps it barely alive
    // when quiet). Detached follow = frozen panel.
    let panel = if state.log_follow {
        let frame = state.log_lines_total.wrapping_add(wall_ms() / 2000);
        format!(
            "{} ",
            crate::liveness::panel_glyphs(
                0x10C5,
                frame,
                crate::liveness::PanelPattern::Rain,
                state.unicode,
            )
        )
    } else {
        String::new()
    };
    let filter_indicator = log_filter_indicator(state);
    // A control names its own key (SPEC R24.8.5). The old form listed Wrap and
    // Zoom and then said "Press 'l' to switch", which reads as though `l` drove
    // them — it opens the file switcher; `w` wraps and `Enter` zooms.
    format!(
        "{}{}{}[w Wrap:{} | Enter Zoom:{} | l file | click a line to open] ",
        panel, log_title, filter_indicator, wrap_str, zoom_str
    )
}

#[cfg(feature = "tui")]
fn draw_log(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::Log;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let block = Block::default()
        .title(Span::styled(build_log_title(state), theme.title()))
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

    // One click target per visible row, so a line that is too long for the pane
    // can be opened and read in full. Registered before the paragraph is drawn
    // so the rects match exactly what is on screen this frame.
    register_log_line_click_targets(state, &visible_lines, inner);

    frame.render_widget(Paragraph::new(visible_lines), inner);
    draw_scrollbar(frame, theme, display_lines.len(), visible_h, scroll, inner);
}

/// Register a [`ClickTarget::OpenLogLine`] for each rendered log row, carrying
/// that row's plain text. Blank rows are skipped — there is nothing to open.
#[cfg(feature = "tui")]
fn register_log_line_click_targets(state: &AppState, visible_lines: &[Line<'_>], inner: Rect) {
    let mut targets = state.click_targets.borrow_mut();
    for (i, line) in visible_lines.iter().enumerate() {
        let text: String = line.spans.iter().map(|s| &*s.content).collect();
        if text.trim().is_empty() {
            continue;
        }
        let rect = Rect {
            x: inner.x,
            y: inner.y + i as u16,
            width: inner.width,
            height: 1,
        };
        targets.push((ClickTarget::OpenLogLine(text), rect));
    }
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

    // Vocabulary matched to the scope-grant modal — "outside the sandbox
    // scope", the path shown literally, the key named in a bracketed hint —
    // so the two out-of-scope questions read as one idiom rather than two.
    // The glyph is unicode-gated (the emoji here were the only ones in the
    // crate, ungated and double-width, and SPEC R22.3 forbids them anyway).
    let warn = if theme.unicode { "⚠" } else { "!" };
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {warn} Log file · outside sandbox scope"),
            theme.approval_border(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ", theme.normal()),
            Span::styled(&info.path, theme.normal().bold()),
            Span::styled(" is a symlink pointing to:", theme.normal()),
        ]),
        Line::from(Span::styled(
            format!("    {}", full_target_str),
            theme.pending().bold(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  That destination lies outside this workspace, so reading it is blocked.",
            theme.dim(),
        )),
        Line::from(Span::styled(
            "  Approving affects this view only — it records no persistent grant.",
            theme.dim(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [a] ", theme.approval_key()),
            Span::styled("Read it anyway", theme.normal().bold()),
            Span::styled("    any other key = leave it blocked", theme.dim()),
        ]),
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

/// The "grant access to X?" overlay raised when a sandboxed command was blocked by
/// an out-of-scope path (SPEC R5.4.7). The path is shown literally; Deny is the
/// highlighted default and Enter/Esc deny — widening requires an explicit key.
#[cfg(feature = "tui")]
fn draw_scope_grant_modal(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let Some(gate) = &state.scope_grant else {
        return;
    };
    let popup = centered_rect(76, 14, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            " Sandbox · grant access? ",
            theme.title().bold(),
        ))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let reason = match gate.reason {
        ahma_common::scope_grant::GrantReason::PreExecViolation => {
            "a command targeted a path outside the sandbox"
        }
        ahma_common::scope_grant::GrantReason::StderrHeuristic => {
            "a command was blocked accessing a path outside the sandbox"
        }
    };
    let tool = gate.tool.as_deref().unwrap_or("A sandboxed command");

    let lines = vec![
        Line::from(vec![
            Span::styled(tool.to_string(), theme.normal().bold()),
            Span::styled(
                format!(" needs {} access to:", gate.access.label()),
                theme.normal(),
            ),
        ]),
        Line::from(Span::styled(gate.path.clone(), theme.success().bold())),
        Line::from(""),
        Line::from(Span::styled(format!("Why: {reason}."), theme.dim())),
        Line::from(Span::styled(
            "Approving records a persistent grant in ~/.ahma/settings.toml. This running",
            theme.dim(),
        )),
        Line::from(Span::styled(
            "session's scope stays locked: ask the agent to run the `restart` tool (or",
            theme.dim(),
        )),
        Line::from(Span::styled(
            "restart ahma) to apply the grant now — otherwise it applies on next start.",
            theme.dim(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [n] ", theme.failed().bold()),
            Span::styled("Deny (default)    ", theme.normal().bold()),
            Span::styled("[r] ", theme.pending().bold()),
            Span::styled("Grant read-only    ", theme.normal()),
            Span::styled("[y] ", theme.success().bold()),
            Span::styled("Grant read+write", theme.normal()),
        ]),
        Line::from(Span::styled("  Enter / Esc = Deny", theme.dim())),
    ];

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

fn draw_web_approval_modal(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let Some(gate) = &state.web_approval else {
        return;
    };
    let popup = centered_rect(76, 14, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(" Web · allow egress? ", theme.title().bold()))
        .borders(Borders::ALL)
        .border_style(theme.border_focused());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let tool = gate.tool.as_deref().unwrap_or("A tool");
    // R-WEB.6.1: the URL is attacker-influenced text rendered inside a security
    // prompt. Cap it so a crafted long URL cannot push the buttons off a
    // fixed-height popup or bury the question in wrapped noise.
    let url = truncate(&gate.url, 256);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(tool.to_string(), theme.normal().bold()),
            Span::styled(" wants to reach the domain:", theme.normal()),
        ]),
        Line::from(Span::styled(gate.domain.clone(), theme.success().bold())),
        Line::from(Span::styled(format!("  {url}"), theme.dim())),
    ];

    // R-WEB.6.4: cleartext HTTP is permitted (some dev environments need it)
    // but never silently — the caution is part of the question.
    if gate.url.starts_with("http://") {
        let warn = if state.unicode { "⚠ " } else { "! " };
        lines.push(Line::from(Span::styled(
            format!("  {warn}cleartext HTTP — this traffic is not encrypted"),
            theme.pending().bold(),
        )));
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "Approving applies to this session; 'always' also saves it to",
            theme.dim(),
        )),
        Line::from(Span::styled(
            "~/.ahma/settings.toml. The request that triggered this is denied — retry it.",
            theme.dim(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [n] ", theme.failed().bold()),
            Span::styled("Deny (default)  ", theme.normal().bold()),
            Span::styled("[o] ", theme.pending().bold()),
            Span::styled("Once  ", theme.normal()),
            Span::styled("[s] ", theme.pending().bold()),
            Span::styled("Session  ", theme.normal()),
            Span::styled("[a] ", theme.success().bold()),
            Span::styled("Always", theme.normal()),
        ]),
        Line::from(Span::styled("  Enter / Esc = Deny", theme.dim())),
    ]);

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
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
fn draw_help(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
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
        .title(Line::from(Span::styled(" ↑↓ scroll · Esc closes ", theme.dim())).right_aligned())
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
        ("", ""),
        ("CHAT", ""),
        ("Enter", "Send message"),
        ("Shift+Enter", "Insert newline"),
        ("Esc", "Clear current input"),
        ("Arrows / Home / End", "Move within editor"),
        ("", ""),
        ("CHAT INPUT PREFIXES", ""),
        (
            "! <command>",
            "Run OUTSIDE the sandbox — unrestricted, human-only (e.g. ! pwd)",
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
        ("/skills", "List Agent Skills; run one with /<name> [args]"),
        ("", ""),
        ("WINDOW ACTIONS", ""),
        ("/n", "Restore/expand window n"),
        ("/xn", "Close/cancel window n"),
        ("/quit", "Quit the application"),
        ("Click [xn]", "Close/cancel window"),
        ("Click [+]/[-]", "Toggle expand/collapse"),
        ("Click card", "Open operation details"),
        ("", ""),
        ("TASKS (tree)", ""),
        ("j / k", "Select row"),
        ("Enter", "Open full-screen operation details; fold headers"),
        (
            "Space / Click",
            "Expand task into live/historic output (accordion); fold headers",
        ),
        ("f", "Toggle this-project / all-projects filter"),
        ("c", "Cancel selected"),
        ("p", "Pin selected"),
        ("a", "Ask for access (on a denied operation)"),
        ("/analyze [op_id]", "Ask AI to analyze operation"),
        ("/log file <path>", "Start log monitoring"),
        ("/scope", "Show sandbox scope & provenance"),
        ("", ""),
        ("LOG", ""),
        ("/", "Start filter (Esc to clear)"),
        ("j / k", "Scroll"),
        ("g / G", "Top / bottom"),
        ("w", "Toggle line wrap"),
        ("Enter", "Zoom/restore the pane"),
        ("l", "Switch log file"),
        ("Click a line", "Open it full-screen, wrapped"),
    ];

    let single_rows: &[(&str, &str)] = &[
        ("GLOBAL", ""),
        ("q / Ctrl-C", "Quit"),
        ("Tab / Shift-Tab", "Cycle focus"),
        ("?", "Toggle this help"),
        ("Esc / ? (when help open)", "Close help overlay"),
        ("", ""),
        ("CHAT", ""),
        ("Enter", "Send message"),
        ("Shift+Enter", "Insert newline"),
        ("Esc", "Clear current input"),
        ("Arrow keys / Home / End", "Move within the editor"),
        ("", ""),
        ("CHAT INPUT PREFIXES", ""),
        (
            "! <command>",
            "Run OUTSIDE the sandbox — unrestricted, human-only (e.g. ! pwd)",
        ),
        ("# <goal>", "Decompose goal using LLM (e.g. # run tests)"),
        ("/", "Open command navigator (from empty input)"),
        ("", ""),
        ("PICKERS", ""),
        ("Type", "Filter providers or models"),
        ("Up / Down", "Move selection"),
        ("Enter / Esc", "Choose / cancel"),
        ("", ""),
        ("TASKS (tree)", ""),
        ("j / k", "Select row"),
        ("Enter", "Open full-screen operation details; fold headers"),
        (
            "Space / Click",
            "Expand task into live/historic output (accordion); fold headers",
        ),
        ("f", "Toggle this-project / all-projects filter"),
        ("c", "Cancel selected"),
        ("p", "Pin selected"),
        ("a", "Ask for access (on a denied operation)"),
        ("/analyze [op_id]", "Ask AI to analyze operation"),
        ("/log file <path>", "Start log monitoring"),
        ("/scope", "Show sandbox scope & provenance"),
        ("", ""),
        ("LOG", ""),
        ("/", "Start filter (Esc to clear)"),
        ("j / k", "Scroll"),
        ("g / G", "Top / bottom"),
        ("w", "Toggle line wrap"),
        ("Enter", "Zoom/restore the pane"),
        ("l", "Switch log file"),
        ("Click a line", "Open it full-screen, wrapped"),
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
        ("/skills", "List Agent Skills; run one with /<name> [args]"),
        ("", ""),
        ("WINDOW ACTIONS", ""),
        ("/n", "Restore/expand window n (e.g. /3)"),
        ("/xn", "Close/cancel window n (e.g. /x3)"),
        ("/quit", "Quit the application"),
        ("Mouse Click on Xn", "Close/cancel window"),
        ("Mouse Click on Window", "Toggle expand/collapse"),
    ];

    // The content is taller than the cap on ordinary terminals, so it scrolls
    // and says so — the scrollbar thumb reaching bottom exactly when the
    // content does is the honest-panes contract (SPEC R24.8.1).
    let (total_rows, scroll) = if use_two_columns {
        let chunks = Layout::horizontal([
            Constraint::Percentage(49),
            Constraint::Length(2),
            Constraint::Percentage(49),
        ])
        .split(inner);

        let left_lines = format_help_rows(left_rows, 21, theme);
        let right_lines = format_help_rows(right_rows, 21, theme);
        let total = left_lines.len().max(right_lines.len());
        let scroll = clamp_scroll(state.help_scroll, total, inner.height);

        let left_para = Paragraph::new(Text::from(left_lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        let right_para = Paragraph::new(Text::from(right_lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));

        let sep = Block::default()
            .borders(Borders::LEFT)
            .border_style(theme.border_unfocused());

        frame.render_widget(left_para, chunks[0]);
        frame.render_widget(sep, chunks[1]);
        frame.render_widget(right_para, chunks[2]);
        (total, scroll)
    } else {
        let lines = format_help_rows(single_rows, 24, theme);
        let total = lines.len();
        let scroll = clamp_scroll(state.help_scroll, total, inner.height);
        let para = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(para, inner);
        (total, scroll)
    };

    draw_scrollbar(
        frame,
        theme,
        total_rows,
        inner.height as usize,
        scroll as usize,
        inner,
    );
}

/// Clamp a requested scroll offset so the last row is the last thing shown —
/// scrolling past the end leaves a blank pane and lies about there being more.
#[cfg(feature = "tui")]
fn clamp_scroll(requested: u16, total_rows: usize, visible_h: u16) -> u16 {
    let max = (total_rows as u16).saturating_sub(visible_h);
    requested.min(max)
}

// ─── Utility ──────────────────────────────────────────────────────────────────

fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = s.chars();
    let mut truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        // Drop the last *character*, not the last byte — byte-slicing here
        // panicked mid-codepoint on any multibyte tail (CJK paths, emoji).
        truncated.pop();
        format!("{truncated}…")
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
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let shortened = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    let char_count = shortened.chars().count();
    if char_count <= max_chars {
        return shortened;
    }
    // Keep the trailing `keep` characters. Character-based, not byte-based:
    // byte indexing panicked mid-codepoint on non-ASCII paths (`~/Résumé/…`).
    let keep = max_chars.saturating_sub(1);
    let tail: String = shortened
        .chars()
        .skip(char_count.saturating_sub(keep))
        .collect();
    format!("…{tail}")
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
    let sidebar_items = build_settings_sidebar_items(state, theme);
    let sidebar_list = List::new(sidebar_items).block(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(theme.dim()),
    );
    frame.render_widget(sidebar_list, sidebar_area);

    // 2. Draw content pane (settings items for selected category)
    use crate::settings_editor::SettingsCategory;
    let selected_cat = SettingsCategory::ALL[state.settings_editor.selected_category];
    let items = state.settings_editor.items_for_category(selected_cat);
    let content_items = build_settings_content_items(state, theme, &items);

    // Split content area into items list (top) and footer/hints (bottom)
    let content_chunks =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(content_area);

    let list_area = content_chunks[0];
    let footer_area = content_chunks[1];

    // Keep the selected row on screen and tell the truth about overflow — a
    // category taller than the fixed popup silently lost its tail (R24.8.1).
    let total = content_items.len();
    let visible = list_area.height as usize;
    let offset = state
        .settings_editor
        .selected_item
        .saturating_sub(visible.saturating_sub(1))
        .min(total.saturating_sub(visible));
    let content_list = List::new(content_items);
    let mut list_state = ListState::default().with_offset(offset);
    frame.render_stateful_widget(content_list, list_area, &mut list_state);
    draw_scrollbar(frame, theme, total, visible, offset, list_area);

    // Draw status message and action hints
    frame.render_widget(
        Paragraph::new(settings_footer_line(state, theme)),
        footer_area,
    );
}

/// One `ListItem` per settings category, highlighting the currently selected one.
#[cfg(feature = "tui")]
fn build_settings_sidebar_items(state: &AppState, theme: &Theme) -> Vec<ListItem<'static>> {
    use crate::settings_editor::SettingsCategory;
    SettingsCategory::ALL
        .iter()
        .enumerate()
        .map(|(i, cat)| {
            let is_selected = state.settings_editor.selected_category == i;
            let style = if is_selected {
                theme.title().bg(Color::DarkGray)
            } else {
                theme.normal()
            };
            let label = format!(" {} {}", cat.icon(), cat.label());
            ListItem::new(Line::from(vec![Span::styled(label, style)]))
        })
        .collect()
}

/// One `ListItem` per setting in the selected category: label, current value,
/// an optional `[locked]` indicator for security-tier settings, and description.
#[cfg(feature = "tui")]
fn build_settings_content_items(
    state: &AppState,
    theme: &Theme,
    items: &[crate::settings_editor::SettingItem],
) -> Vec<ListItem<'static>> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_selected = state.settings_editor.selected_item == i;
            let item_style = if is_selected {
                theme.selected_item()
            } else {
                theme.normal()
            };

            let val_string = format_setting_value(&item.value);

            // Distinguish the three kinds of row the panel actually holds.
            // Without this, a string/numeric row looked identical to a togglable
            // one and simply swallowed Space — most of the panel read as broken
            // rather than read-only.
            let editable = matches!(item.value, crate::settings_editor::SettingValue::Bool(_))
                && !item.security_tier;
            let sec_indicator = if item.security_tier {
                Span::styled(" [locked]", theme.dim())
            } else if editable {
                Span::styled(" [space]", theme.dim())
            } else {
                Span::styled(" [file]  ", theme.dim())
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

            ListItem::new(line).style(item_style)
        })
        .collect()
}

/// Footer line: status/dirty message on the left, key hints on the right.
#[cfg(feature = "tui")]
fn settings_footer_line(state: &AppState, theme: &Theme) -> Line<'static> {
    let status_str = if let Some((msg, _)) = &state.settings_editor.status_message {
        msg.clone()
    } else if state.settings_editor.dirty {
        "● Unsaved changes".to_string()
    } else {
        "".to_string()
    };

    Line::from(vec![
        Span::styled(format!("  {}", status_str), theme.pending()),
        Span::styled(
            "  [Space] Toggle  [r] Reset  [s] Save  [Esc/q] Close   ([file] = edit settings.toml)",
            theme.dim(),
        ),
    ])
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
    fn char_heuristic_estimator_divides_by_four() {
        assert_eq!(estimate_tokens(4000), 1000);
        assert_eq!(estimate_tokens(3), 0);
    }

    fn layout_window(
        status: crate::state::WindowStatus,
        content_lines: usize,
    ) -> crate::state::TuiWindow {
        crate::state::TuiWindow {
            id: 0,
            label: "w".into(),
            status,
            content: vec!["line".into(); content_lines],
            collapsed: false,
            finished_at: None,
            duration_ms: None,
            last_output_at: None,
            is_cli: true,
            command: String::new(),
            working_dir: String::new(),
            llm_model: None,
            visible: true,
            abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            op_id: None,
        }
    }

    /// The layout budget collapses old windows to fit — but never an Error
    /// window: a failure the user has not seen must stay readable.
    #[test]
    fn error_windows_survive_auto_collapse() {
        use crate::state::WindowStatus;
        let windows = vec![
            layout_window(WindowStatus::Error, 6),
            layout_window(WindowStatus::Finished, 6),
            layout_window(WindowStatus::Finished, 6),
        ];
        // Each expanded window wants 8 rows; force a squeeze into 12.
        let layouts = compute_window_layouts(&windows, 12);
        assert!(
            !layouts[0].collapsed,
            "error window must not be auto-collapsed"
        );
        assert!(
            layouts[1].collapsed && layouts[2].collapsed,
            "finished windows are the ones that fold"
        );
    }

    /// The reported symptom: `!pwd` printed nothing visible. The window was
    /// sized from `content.len()` alone while the renderer also prepended a
    /// `$ pwd` header and a separator, so the two rows it did not budget for
    /// pushed the answer and the outcome off the bottom.
    #[test]
    fn window_layout_budgets_the_command_header_it_renders() {
        use crate::state::WindowStatus;
        let mut w = layout_window(WindowStatus::Finished, 2);
        w.label = "UNSANDBOXED: pwd in ~/github/ahma".into();
        w.command = "pwd".into();
        w.content = vec!["/Users/paul/github/ahma".into(), "Finished in 0.0s".into()];

        assert!(
            window_has_command_header(&w),
            "a distinct command must render its header"
        );
        assert_eq!(
            expanded_window_line_count(&w),
            4,
            "2 header rows + 2 output rows"
        );

        let layouts = compute_window_layouts(std::slice::from_ref(&w), 40);
        let inner_h = layouts[0].height as usize - 2; // borders
        assert!(
            inner_h >= expanded_window_line_count(&w),
            "window sized {inner_h} rows for {} rendered lines — the answer is cut off",
            expanded_window_line_count(&w)
        );
    }

    /// A window with no command header must not be padded for one.
    #[test]
    fn window_layout_omits_the_header_budget_when_no_command_is_shown() {
        use crate::state::WindowStatus;
        let w = layout_window(WindowStatus::Finished, 2);
        assert!(!window_has_command_header(&w));
        assert_eq!(expanded_window_line_count(&w), 2);
    }

    /// Render a real window and read back the text of each row, so the test
    /// sees exactly what the user sees.
    fn render_window_rows(w: &crate::state::TuiWindow, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let theme = Theme::new(true);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                draw_expanded_window(frame, w, area, &theme, theme.normal());
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// End-to-end for the `!pwd` report: the command's actual output must be on
    /// screen at the height the layout picks for it.
    #[test]
    fn expanded_window_shows_the_command_output() {
        use crate::state::WindowStatus;
        let mut w = layout_window(WindowStatus::Finished, 0);
        w.label = "UNSANDBOXED: pwd in ~/github/ahma".into();
        w.command = "pwd".into();
        w.content = vec!["/Users/paul/github/ahma".into(), "Finished in 0.0s".into()];

        let h = compute_window_layouts(std::slice::from_ref(&w), 40)[0].height;
        let rows = render_window_rows(&w, 60, h);
        let screen = rows.join("\n");
        assert!(
            screen.contains("/Users/paul/github/ahma"),
            "the command's output must be visible, got:\n{screen}"
        );
    }

    /// When output overflows the capped window, the *tail* survives — the last
    /// lines and the outcome, not the command echo the border already shows.
    #[test]
    fn expanded_window_overflow_keeps_the_tail_not_the_head() {
        use crate::state::WindowStatus;
        let mut w = layout_window(WindowStatus::Finished, 0);
        w.label = "UNSANDBOXED: ls in ~/github/ahma".into();
        w.command = "ls".into();
        w.content = (0..40).map(|i| format!("entry-{i}")).collect();

        let h = compute_window_layouts(std::slice::from_ref(&w), 40)[0].height;
        let screen = render_window_rows(&w, 60, h).join("\n");
        assert!(
            screen.contains("entry-39"),
            "the newest output must survive overflow, got:\n{screen}"
        );
        assert!(
            !screen.contains("entry-0\n") && !screen.ends_with("entry-0"),
            "the oldest output is what should scroll away, got:\n{screen}"
        );
    }

    /// Every rendered log row gets a click target carrying its full text, so a
    /// line truncated at the pane edge can still be opened and read. Blank rows
    /// are skipped — there is nothing behind them.
    #[test]
    fn log_rows_register_click_targets_with_their_full_text() {
        let state = AppState::new("http://localhost:3000", "HTTP", true);
        let inner = Rect::new(0, 5, 40, 3);
        let lines = vec![
            make_line("pid=1 role=bridge INFO a very long message"),
            make_line("   "),
            make_line("pid=1 role=bridge WARN another line"),
        ];
        register_log_line_click_targets(&state, &lines, inner);

        let targets = state.click_targets.borrow();
        assert_eq!(targets.len(), 2, "the blank row must not be clickable");
        match &targets[0] {
            (ClickTarget::OpenLogLine(text), rect) => {
                assert_eq!(text, "pid=1 role=bridge INFO a very long message");
                assert_eq!((rect.y, rect.height), (5, 1));
            }
            other => panic!("expected an OpenLogLine target, got {other:?}"),
        }
        match &targets[1] {
            (ClickTarget::OpenLogLine(text), rect) => {
                assert_eq!(text, "pid=1 role=bridge WARN another line");
                assert_eq!(rect.y, 7, "row index must map to its screen row");
            }
            other => panic!("expected an OpenLogLine target, got {other:?}"),
        }
    }

    /// A tree taller than its pane scrolls silently unless it says so. The
    /// selected row is kept in view automatically, so without a bar there is no
    /// cue at all that rows exist above or below.
    #[test]
    fn task_tree_shows_a_scrollbar_only_when_it_overflows() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let theme = Theme::new(true);
        let thumb_bg = theme.scrollbar_thumb().bg;
        let track_bg = theme.scrollbar_track().bg;

        // `n` operations under one instance header; the pane shows 8 rows.
        let render = |n: usize| -> Vec<Option<Color>> {
            let mut state = AppState::new("http://localhost:3000", "HTTP", true);
            state.tasks_window_open = true;
            for i in 0..n {
                let mut op = crate::state::Operation::new(
                    format!("op_{i}"),
                    "run_terminal_command",
                    crate::state::OpStatus::Running,
                );
                op.title = Some(format!("cargo test {i}"));
                state.operations.push(op);
            }
            let (w, h) = (60u16, 10u16);
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| draw_ops_dag(frame, &state, &theme, Rect::new(0, 0, w, h)))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            // Column w-2 is inside the block border, where the bar renders.
            (1..h - 1)
                .map(|y| buf.cell((w - 2, y)).unwrap().bg.into())
                .collect()
        };

        let few = render(2);
        assert!(
            !few.iter().any(|bg| *bg == thumb_bg || *bg == track_bg),
            "a tree that fits must not steal a column for a bar, got {few:?}"
        );

        let many = render(60);
        assert!(
            many.contains(&thumb_bg),
            "an overflowing tree must show its scroll position, got {many:?}"
        );
    }

    /// The help screen claimed `$ / % / ! <command>` ran "in sandbox" and that
    /// `!!` was the escape. Only `!` is handled, and it is the escape — so the
    /// help told the user a sandbox bypass was sandboxed, and named two
    /// prefixes that do nothing. Documentation that inverts a security
    /// polarity is worse than none (SPEC R7: enforcement is never silently
    /// disabled, and disclosure is loud).
    #[test]
    fn help_describes_the_bang_prefix_as_the_sandbox_escape() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let theme = Theme::new(true);
        let state = AppState::new("http://localhost:3000", "HTTP", true);
        // Both the two-column (>=100 wide) and single-column layouts.
        for width in [120u16, 70] {
            let mut terminal = Terminal::new(TestBackend::new(width, 50)).unwrap();
            terminal
                .draw(|frame| draw_help(frame, &state, &theme, Rect::new(0, 0, width, 50)))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let screen: String = (0..50)
                .map(|y| {
                    (0..width)
                        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");

            assert!(
                !screen.contains("in sandbox"),
                "width {width}: no chat prefix runs in the sandbox, got:\n{screen}"
            );
            assert!(
                !screen.contains("!!"),
                "width {width}: `!!` is not a prefix the code implements, got:\n{screen}"
            );
            assert!(
                screen.contains("OUTSIDE"),
                "width {width}: the `!` escape must be disclosed, got:\n{screen}"
            );
        }
    }

    /// Click targets are rebuilt from scratch every frame. They used to be
    /// cleared only when an overlay opened, so they grew for the whole session
    /// and a stale rect could win the first-match lookup over the widget
    /// actually drawn at that point.
    #[test]
    fn drawing_a_frame_discards_the_previous_frame_targets() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let theme = Theme::new(true);
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.tasks_window_open = true;

        // A target left over from an earlier frame, at a rect nothing occupies.
        state.click_targets.borrow_mut().push((
            ClickTarget::OpenLogLine("stale".into()),
            Rect::new(0, 0, 5, 1),
        ));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &state, &theme)).unwrap();

        let targets = state.click_targets.borrow();
        assert!(
            !targets
                .iter()
                .any(|(t, _)| matches!(t, ClickTarget::OpenLogLine(s) if s == "stale")),
            "the previous frame's targets must not survive into this one"
        );
    }

    /// The scope panel states the whole posture: enforcement, who the
    /// authority is, every write root, tmp, provenance, and the user's own
    /// grants with the command that removes them (SPEC R5.4, R5.4.6).
    #[test]
    fn scope_window_states_roots_authority_and_provenance() {
        use crate::state::{AppState, SandboxScopeInfo};
        let theme = Theme::new(true);
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.sandbox_scope = Some(SandboxScopeInfo {
            write: vec!["/home/u/proj".into()],
            read: vec![],
            tmp: false,
            enforced: true,
            source: Some("roots/list".into()),
            active: Some("ahma".into()),
            ..Default::default()
        });
        state.granted_scopes = vec![("/opt/cache".into(), "rw".into())];

        let text = scope_window_lines(&state, &theme)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect::<Vec<String>>()
            .join("\n");

        assert!(text.contains("ENFORCED"), "{text}");
        assert!(text.contains("ahma (kernel)"), "{text}");
        assert!(text.contains("/home/u/proj"), "{text}");
        assert!(text.contains("source: roots/list"), "{text}");
        assert!(text.contains("/opt/cache (rw)"), "grants shown: {text}");
        assert!(text.contains("ahma sandbox revoke"), "revoke named: {text}");
    }

    /// Deferring to a host means ahma is not enforcing; the panel must say that
    /// in words (SPEC R7.5), not leave it to a colour.
    #[test]
    fn scope_window_names_host_authority_when_deferred() {
        use crate::state::{AppState, SandboxScopeInfo};
        let theme = Theme::new(true);
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.sandbox_scope = Some(SandboxScopeInfo {
            write: vec!["/home/u/proj".into()],
            enforced: false,
            active: Some("deferred_to_host".into()),
            host: Some("Cursor".into()),
            ..Default::default()
        });

        let text = scope_window_lines(&state, &theme)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect::<Vec<String>>()
            .join("\n");

        assert!(text.contains("Cursor"), "{text}");
        assert!(text.contains("ahma is NOT enforcing"), "{text}");
    }

    /// Before any report arrives the panel says so and explains why, rather
    /// than rendering an empty box or implying a scope it has not been told.
    #[test]
    fn scope_window_is_explicit_when_nothing_reported() {
        use crate::state::AppState;
        let theme = Theme::new(true);
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.sandbox_status = "INITIALIZING".to_string();

        let text = scope_window_lines(&state, &theme)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect::<Vec<String>>()
            .join("\n");

        assert!(text.contains("Scope not reported"), "{text}");
        assert!(text.contains("still negotiating scope"), "{text}");
    }

    /// The help overlay is capped shorter than its content, so its tail — the
    /// LOG section, including the click-a-line affordance R24.8.4 requires it
    /// to advertise — must be reachable by scrolling. Before this it was not
    /// rendered at all, in the pane that documents the rule.
    #[test]
    fn help_tail_is_reachable_by_scrolling() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::new(true);
        let render = |scroll: u16| -> String {
            let mut state = AppState::new("http://localhost:3000", "HTTP", true);
            state.help_scroll = scroll;
            let mut terminal = Terminal::new(TestBackend::new(70, 30)).unwrap();
            terminal
                .draw(|frame| draw_help(frame, &state, &theme, Rect::new(0, 0, 70, 30)))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..30)
                .map(|y| {
                    (0..70)
                        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let top = render(0);
        let bottom = render(200); // clamped to the true maximum
        assert!(top.contains("GLOBAL"), "top shows the first section");
        assert!(
            !top.contains("Switch log file"),
            "the tail must genuinely be off-screen at rest, else this proves nothing"
        );
        assert!(
            bottom.contains("Switch log file"),
            "scrolling to the end must reveal the LOG section:\n{bottom}"
        );
    }

    /// Scrolling past the end is clamped, so the last row stays the last thing
    /// shown rather than scrolling away into blank space.
    #[test]
    fn clamp_scroll_stops_at_the_last_row() {
        assert_eq!(clamp_scroll(0, 50, 10), 0);
        assert_eq!(clamp_scroll(5, 50, 10), 5);
        assert_eq!(clamp_scroll(999, 50, 10), 40);
        // Content that fits never scrolls.
        assert_eq!(clamp_scroll(999, 8, 10), 0);
    }

    /// The web modal is a security prompt over attacker-influenced text, so
    /// SPEC R-WEB.6 pins its contents: all three tiers offered, the URL capped
    /// (R-WEB.6.1), and cleartext HTTP called out (R-WEB.6.4).
    #[test]
    fn web_modal_offers_three_tiers_and_flags_cleartext() {
        use crate::state::WebApprovalGate;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::new(true);
        let render = |url: &str| -> String {
            let mut state = AppState::new("http://localhost:3000", "HTTP", true);
            state.web_approval = Some(WebApprovalGate {
                decision_id: "d".into(),
                domain: "example.com".into(),
                url: url.to_string(),
                tool: Some("fetch_webpage".into()),
            });
            let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
            terminal
                .draw(|f| draw_web_approval_modal(f, &state, &theme, Rect::new(0, 0, 90, 24)))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..24)
                .map(|y| {
                    (0..90)
                        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let https = render("https://example.com/a");
        for tier in ["[n]", "[o]", "[s]", "[a]"] {
            assert!(
                https.contains(tier),
                "tier {tier} must be offered:\n{https}"
            );
        }
        assert!(
            !https.contains("cleartext"),
            "https must not raise the cleartext caution"
        );

        let http = render("http://example.com/a");
        assert!(http.contains("cleartext"), "http must be flagged:\n{http}");
    }

    /// A crafted long URL must not be able to push the buttons out of a
    /// fixed-height popup (R-WEB.6.1).
    #[test]
    fn web_modal_caps_a_hostile_url() {
        let long = format!("https://example.com/{}", "a".repeat(4000));
        assert!(truncate(&long, 256).chars().count() <= 256);
    }

    #[test]
    fn status_glyphs_distinguish_outcomes() {
        use crate::state::WindowStatus;
        assert_eq!(status_glyph(WindowStatus::Finished, true), "✓");
        assert_eq!(status_glyph(WindowStatus::Error, true), "✗");
        assert_eq!(status_glyph(WindowStatus::Finished, false), "v");
        assert_eq!(status_glyph(WindowStatus::Error, false), "x");
    }

    #[test]
    fn duration_short_formats_ms_and_seconds() {
        assert_eq!(format_duration_short(842), "842ms");
        assert_eq!(format_duration_short(2140), "2.1s");
    }

    #[test]
    fn token_status_segment_empty_when_no_data() {
        assert_eq!(token_status_segment(0, 0, 0, 0, 0, None), "");
    }

    #[test]
    fn token_status_segment_shows_exact_usage() {
        // Provider reported usage → exact cumulative counts, no context window.
        let s = token_status_segment(1700, 1200, 500, 1200, 0, None);
        assert_eq!(s, " · tkns 1.2k in / 500 out (1.7k ttl)");
    }

    /// The narrow-header variant keeps the decision-relevant half (context
    /// fill) and drops the in/out/total breakdown. With no known context
    /// window there is no percentage to state, so it renders nothing rather
    /// than a percentage of an unknown whole.
    #[test]
    fn context_fill_segment_needs_a_known_window() {
        use crate::state::AppState;
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.token_prefs.context_length = None;
        state.last_prompt_tokens = 2048;
        assert_eq!(context_fill_segment(&state), "");

        state.token_prefs.context_length = Some(4096);
        assert_eq!(context_fill_segment(&state), " · 50% ctx");

        // Nothing sent yet and nothing to estimate → still nothing to say.
        state.last_prompt_tokens = 0;
        assert_eq!(context_fill_segment(&state), "");
    }

    #[test]
    fn token_status_segment_estimates_when_no_usage() {
        // No API usage, but a 6000-char conversation → ~1500 token estimate.
        let s = token_status_segment(0, 0, 0, 0, 6000, None);
        assert_eq!(s, " · ~1.5k tkns est");
    }

    #[test]
    fn token_status_segment_context_pct_exact_from_last_prompt() {
        // 4096-token window, last turn's prompt was 2048 → 50% (exact).
        let s = token_status_segment(3000, 2048, 200, 2048, 9999, Some(4096));
        assert!(s.ends_with(" · 50% ctx"), "got {s:?}");
    }

    #[test]
    fn token_status_segment_context_pct_estimated_without_usage() {
        // No usage at all: % falls back to the conversation estimate.
        // 8000 chars → 2000 tokens; window 8000 → 25%.
        let s = token_status_segment(0, 0, 0, 0, 8000, Some(8000));
        assert_eq!(s, " · ~2.0k tkns est · 25% ctx");
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

    /// Render `draw_scrollbar` into a test buffer and return, for the scrollbar
    /// column (rightmost), the per-row `(symbol, bg_color)` so tests can inspect
    /// what the thumb/track actually paint.
    #[cfg(feature = "tui")]
    fn render_scrollbar_column(
        total_len: usize,
        visible_h: usize,
        scroll: usize,
        height: u16,
    ) -> Vec<(String, Option<Color>)> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let theme = Theme::new(true);
        let width = 6u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                draw_scrollbar(frame, &theme, total_len, visible_h, scroll, area);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let cell = buf.cell((width - 1, y)).unwrap();
                (cell.symbol().to_string(), cell.bg.into())
            })
            .collect()
    }

    #[test]
    fn scrollbar_thumb_is_solid_proportional_and_top_anchored() {
        // 40 rows of content, 10 visible, pinned to the top (scroll 0).
        let height = 10u16;
        let col = render_scrollbar_column(40, 10, 0, height);
        let thumb_bg = Theme::new(true).scrollbar_thumb().bg;
        let track_bg = Theme::new(true).scrollbar_track().bg;

        // The thumb is painted as a filled cell (space), never the `█` glyph that
        // renders as dashes under terminal line-spacing.
        for (sym, _) in &col {
            assert_ne!(
                sym, "█",
                "thumb/track must not use the gap-prone full block"
            );
        }

        let thumb_rows: Vec<usize> = col
            .iter()
            .enumerate()
            .filter(|(_, (_, bg))| *bg == thumb_bg)
            .map(|(i, _)| i)
            .collect();
        // Proportional: ~visible/total of the bar, and strictly shorter than the
        // whole track (content overflows the viewport).
        assert!(
            !thumb_rows.is_empty() && thumb_rows.len() < height as usize,
            "thumb should be a proper sub-range of the track, got {thumb_rows:?}"
        );
        // Contiguous: a single run, no gaps (the dashed-thumb regression).
        assert_eq!(
            thumb_rows.last().unwrap() - thumb_rows.first().unwrap() + 1,
            thumb_rows.len(),
            "thumb cells must be contiguous, got {thumb_rows:?}"
        );
        // Top-anchored at scroll 0.
        assert_eq!(*thumb_rows.first().unwrap(), 0);
        // Every non-thumb cell is the visible track groove.
        for (i, (_, bg)) in col.iter().enumerate() {
            if !thumb_rows.contains(&i) {
                assert_eq!(*bg, track_bg, "row {i} should be track groove");
            }
        }
    }

    /// The long-standing complaint: scrolled fully to the end, the thumb stopped
    /// short of the bottom edge, so the pane looked like it had more below.
    /// `ScrollbarState::content_length` counts scroll *positions*, not rows;
    /// feeding it the row count parks the thumb at `total/(total-1+visible)` of
    /// the track. Checked across shapes because the error shrinks as content
    /// grows — which is why the log pane looked fine and the chat pane did not.
    #[test]
    fn scrollbar_thumb_reaches_the_bottom_at_max_scroll() {
        let thumb_bg = Theme::new(true).scrollbar_thumb().bg;
        let height = 10u16;
        for (total, visible) in [(12usize, 10usize), (15, 10), (40, 10), (400, 10)] {
            let max_scroll = total - visible;
            let col = render_scrollbar_column(total, visible, max_scroll, height);
            let last = col.last().expect("scrollbar column is non-empty");
            assert_eq!(
                last.1, thumb_bg,
                "total={total} visible={visible}: at max scroll the thumb must \
                 reach the bottom row of the track, got column {col:?}"
            );
        }
    }

    /// The complement: at rest (nothing scrolled) the thumb must NOT touch the
    /// bottom, or "you are at the end" would be indistinguishable from "you are
    /// at the start".
    #[test]
    fn scrollbar_thumb_leaves_the_bottom_free_at_top_scroll() {
        let thumb_bg = Theme::new(true).scrollbar_thumb().bg;
        let col = render_scrollbar_column(40, 10, 0, 10);
        assert_ne!(
            col.last().unwrap().1,
            thumb_bg,
            "at scroll 0 the bottom of the track must be groove, got {col:?}"
        );
    }

    #[test]
    fn scrollbar_thumb_grows_proportionally_as_content_shrinks() {
        // Same viewport; less overflow ⇒ a longer thumb (closer to filling the bar).
        let long_content = render_scrollbar_column(100, 10, 0, 10);
        let short_content = render_scrollbar_column(15, 10, 0, 10);
        let thumb_bg = Theme::new(true).scrollbar_thumb().bg;
        let count =
            |col: &[(String, Option<Color>)]| col.iter().filter(|(_, bg)| *bg == thumb_bg).count();
        assert!(
            count(&short_content) > count(&long_content),
            "thumb should be longer when less content overflows the viewport"
        );
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
