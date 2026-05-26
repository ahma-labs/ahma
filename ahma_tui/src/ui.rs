//! Ratatui rendering — all panel draw functions.
//!
//! The top-level [`draw`] function is called every redraw tick and delegates
//! to per-panel helpers.
//!
//! Layout (from top to bottom):
//! ```text
//! ┌─ header (1 line) ──────────────────────────────────────────────────────┐
//! │ session · sandbox · workspace · transport · health                     │
//! ├─ AI Activity (8 lines) ────────────────────────────────────────────────┤
//! │ Most-recent MCP tool calls, newest first                               │
//! ├─ Operations DAG (40%) ──┬─ Detail (60%) ─────────────────────────────┤
//! │ Live ops tree           │ Selected op metadata + stdout tail           │
//! ├─ Log (fill) ────────────┴────────────────────────────────────────────┤
//! │ Scrollable, filterable log ring-buffer                                │
//! ├─ Approval banner (0 or 3 lines) ─────────────────────────────────────┤
//! ├─ Footer (1 line) ─────────────────────────────────────────────────────┤
//! └────────────────────────────────────────────────────────────────────────┘
//! ```

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

use crate::state::{AppState, Focus};
use crate::theme::Theme;

// ─── Top-level draw ───────────────────────────────────────────────────────────

/// Called every redraw tick — the only public entry point in this module.
#[cfg(feature = "tui")]
pub fn draw(frame: &mut Frame, state: &AppState, theme: &Theme) {
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

    if state.show_help {
        draw_help(frame, theme, full);
    }
    if state.palette.visible {
        draw_palette(frame, state, theme, full);
    }
}

// ─── Header ───────────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_header(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let health_span = if state.server_healthy {
        Span::styled(
            if state.unicode {
                " ● HEALTHY"
            } else {
                " * HEALTHY"
            },
            theme.healthy(),
        )
    } else {
        Span::styled(
            if state.unicode {
                " ○ OFFLINE"
            } else {
                " - OFFLINE"
            },
            theme.unhealthy(),
        )
    };

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

    let line = Line::from(vec![
        Span::styled(" ahma", theme.title()),
        Span::styled(session_part, theme.dim()),
        Span::styled(
            format!(" · sandbox {}", state.sandbox_status),
            sandbox_style,
        ),
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

    let items: Vec<ListItem> = state
        .operations
        .iter()
        .enumerate()
        .map(|(i, op)| {
            let glyph = op.status.glyph(state.unicode);
            let is_child = op.parent_id.is_some();
            let prefix = if is_child {
                if state.unicode { "  └ " } else { "  L " }
            } else {
                " "
            };
            let pinned = if op.pinned {
                if state.unicode { "📌" } else { "P" }
            } else {
                ""
            };
            let id_short = &op.id[..op.id.len().min(6)];
            let name_short = truncate(&op.tool_name, (inner.width as usize).saturating_sub(22));
            let elapsed = op.elapsed_display();

            let row_style = if i == state.ops_selected {
                theme.selected_item()
            } else {
                theme.normal()
            };

            let line = Line::from(vec![
                Span::styled(prefix, theme.dim()),
                Span::styled(format!("{glyph} "), theme.op_status_style(&op.status)),
                Span::styled(format!("{id_short} "), theme.dim()),
                Span::styled(format!("{pinned}{name_short}"), row_style),
                Span::styled(format!("  {elapsed}"), theme.dim()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let mut list_state = ListState::default().with_selected(Some(state.ops_selected));
    let list = List::new(items)
        .highlight_style(theme.selected_item())
        .highlight_symbol(if state.unicode { "▶ " } else { "> " });

    frame.render_stateful_widget(list, inner, &mut list_state);
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
        None => {
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
        Some(op) => {
            let title = format!(" {}  {} ", op.id, op.tool_name);
            let block = Block::default()
                .title(Span::styled(title, theme.title()))
                .borders(Borders::ALL)
                .border_style(border_style);
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let mut lines: Vec<Line> = vec![];
            let glyph = op.status.glyph(state.unicode);
            lines.push(Line::from(vec![
                Span::styled("  status  ", theme.dim()),
                Span::styled(
                    format!("{glyph} {:?}", op.status),
                    theme.op_status_style(&op.status),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  elapsed ", theme.dim()),
                Span::styled(op.elapsed_display(), theme.normal()),
            ]));
            if let Some(ref cwd) = op.cwd {
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
                        truncate(
                            &op.args.join(" "),
                            (inner.width as usize).saturating_sub(12),
                        ),
                        theme.normal(),
                    ),
                ]));
            }
            if let Some(ref parent) = op.parent_id {
                lines.push(Line::from(vec![
                    Span::styled("  waits   ", theme.dim()),
                    Span::styled(parent.clone(), theme.pending()),
                ]));
            }

            // Stdout tail
            if !op.stdout_tail.is_empty() {
                let sep = if state.unicode {
                    "─".repeat((inner.width as usize).saturating_sub(4))
                } else {
                    "-".repeat((inner.width as usize).saturating_sub(4))
                };
                lines.push(Line::from(Span::styled(format!("  {sep}"), theme.dim())));
                let tail_h = (inner.height as usize).saturating_sub(lines.len());
                for s in op
                    .stdout_tail
                    .iter()
                    .rev()
                    .take(tail_h)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  {}",
                            truncate(s, (inner.width as usize).saturating_sub(4))
                        ),
                        theme.dim(),
                    )));
                }
            }

            let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
            frame.render_widget(para, inner);
        }
    }
}

// ─── Log pane ─────────────────────────────────────────────────────────────────

#[cfg(feature = "tui")]
fn draw_log(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let focused = state.focus == Focus::Log;
    let border_style = if focused {
        theme.border_focused()
    } else {
        theme.border_unfocused()
    };

    let filter_indicator = if state.log_filter_active {
        format!(" filter: {}_", state.log_filter)
    } else if !state.log_filter.is_empty() {
        format!(" filter: {}", state.log_filter)
    } else {
        String::new()
    };

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

    let line1 = Line::from(vec![
        Span::styled(
            format!(" {warn}APPROVAL REQUIRED  {}", gate.op_id),
            theme.approval_banner(),
        ),
        Span::styled(format!("  {desc}"), theme.approval_banner()),
        Span::styled(countdown, theme.approval_banner()),
    ]);
    let line2 = Line::from(vec![
        Span::styled("   [y] approve  ", theme.approval_banner()),
        Span::styled("[n] reject  ", theme.approval_banner()),
        Span::styled(
            "[Tab] focus other panels while deciding",
            theme.approval_banner(),
        ),
    ]);

    let para = Paragraph::new(Text::from(vec![line1, line2])).style(theme.approval_banner());
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
    let h = 28u16.min(area.height);
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
        (":", "Open command palette"),
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
        ("COMMAND PALETTE (:)", ""),
        ("Tab", "Next completion"),
        ("Enter", "Run command"),
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
    let cursor = if state.unicode { "│" } else { "|" };
    let input_display = format!("> {}{cursor}", state.palette.input);
    let input_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(input_display, theme.running())),
        input_area,
    );

    if inner.height < 3 || state.palette.completions.is_empty() {
        return;
    }

    // Separator
    let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(
            if state.unicode {
                "─".repeat(inner.width as usize)
            } else {
                "-".repeat(inner.width as usize)
            },
            theme.dim(),
        )),
        sep_area,
    );

    // Completions
    let list_h = inner.height.saturating_sub(2);
    if list_h == 0 {
        return;
    }
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, list_h);

    let items: Vec<ListItem> = state
        .palette
        .completions
        .iter()
        .take(list_h as usize)
        .enumerate()
        .map(|(i, name)| {
            let style = if i == state.palette.selected_completion {
                theme.selected_item()
            } else {
                theme.normal()
            };
            ListItem::new(Span::styled(format!(" {name}"), style))
        })
        .collect();

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

fn shorten_path(path: &str, max_chars: usize) -> String {
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
pub fn draw(_state: &AppState, _theme: &Theme) {}
