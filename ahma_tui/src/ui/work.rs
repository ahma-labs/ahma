//! Drawing the unified work view (SPEC R24.9).
//!
//! No box. A section is a horizontal rule with its name written into it, and
//! the rules *are* the structure — a border drawn around them would be a second
//! frame around a frame, which is what the old stacked panes looked like.
//!
//! Every position comes from `work_view::layout`, the same function the input
//! layer hit-tests against, so what is drawn and what is clickable cannot drift
//! apart (SPEC R24.8.2) — including mid-animation, when a section is showing
//! fewer rows than it has.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{
    draw_group_tree_row, draw_output_tree_row, draw_scrollbar, instance_tally_spans,
    selection_marker,
};
use crate::state::{AppState, ClickTarget};
use crate::task_tree::RowKind;
use crate::theme::Theme;
use crate::work_view::{self, Hit, Section};

/// Draw the work view into `area`.
///
/// `now_ms` is passed rather than read so an animation frame can be rendered at
/// an exact instant in a test.
pub fn draw_work_view(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect, now_ms: u64) {
    state.work_area.set(area);
    state.rebuild_work_view(now_ms);

    let heights = state.section_heights(now_ms);
    let sections = state.work_sections.borrow().clone();
    let (slots, total) = work_view::layout(&sections, &heights);
    *state.work_slots.borrow_mut() = slots.clone();
    state.work_total_rows.set(total);

    if sections.is_empty() {
        draw_empty_state(frame, state, theme, area);
        return;
    }

    // Reserve the scrollbar column only when there is something to scroll, so a
    // view that fits does not carry a permanently empty gutter.
    let overflows = total > area.height as usize;
    let content_width = if overflows {
        area.width.saturating_sub(1)
    } else {
        area.width
    };

    let selected_y = work_view::nav_row_y(&slots, &sections, state.ops_selected);
    let scroll = work_view::scroll_for(
        selected_y,
        state.ops_scroll.get(),
        area.height as usize,
        total,
        state.work_follow_selection.get(),
    );
    state.ops_scroll.set(scroll);

    let last_row = area.y + area.height;
    for (screen_y, virtual_y) in (area.y..last_row).zip(scroll..total) {
        let row_area = Rect {
            x: area.x,
            y: screen_y,
            width: content_width,
            height: 1,
        };
        if let Some(hit) = work_view::hit(&slots, virtual_y) {
            draw_hit_row(
                frame, state, theme, row_area, &sections, &slots, hit, now_ms,
            );
        }
    }

    if overflows {
        let bar = Rect {
            x: area.x + area.width.saturating_sub(1),
            y: area.y,
            width: 1,
            height: area.height,
        };
        draw_scrollbar(frame, theme, total, area.height as usize, scroll, bar);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_hit_row(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    row_area: Rect,
    sections: &[Section],
    slots: &[work_view::Slot],
    hit: Hit,
    now_ms: u64,
) {
    match hit {
        Hit::Header(key) => {
            let Some(section) = sections.iter().find(|s| s.key == key) else {
                return;
            };
            let nav = nav_index_of_header(sections, &key);
            let selected = nav == Some(state.ops_selected);
            draw_section_rule(frame, state, theme, row_area, section, selected, now_ms);
            state
                .click_targets
                .borrow_mut()
                .push((ClickTarget::SectionHeader(key), row_area));
        }
        Hit::Row { key, inner } => {
            let Some(section) = sections.iter().find(|s| s.key == key) else {
                return;
            };
            let Some(row) = section.rows.get(inner) else {
                return;
            };
            let nav = nav_index_of_row(sections, slots, &key, inner);
            let selected = nav == Some(state.ops_selected);
            let nav_idx = nav.unwrap_or(usize::MAX);
            match &row.kind {
                RowKind::Op { op_index, expanded } => super::draw_op_tree_row(
                    frame, state, theme, nav_idx, row_area, row.depth, *op_index, *expanded,
                    selected,
                ),
                RowKind::Output { text, .. } => {
                    draw_output_tree_row(frame, state, theme, nav_idx, row_area, row.depth, text)
                }
                RowKind::Group {
                    key: group_key,
                    label,
                    collapsed,
                } => {
                    let _ = group_key;
                    draw_group_tree_row(
                        frame, state, theme, nav_idx, row_area, selected, label, *collapsed,
                    )
                }
                // A nested header cannot occur inside a section's rows.
                RowKind::Instance { .. } => {}
            }
        }
    }
}

/// The navigable index of a section's header row.
fn nav_index_of_header(sections: &[Section], key: &str) -> Option<usize> {
    let mut nav = 0usize;
    for section in sections {
        if section.key == key {
            return Some(nav);
        }
        nav += 1;
        if section.open {
            nav += section.rows.len();
        }
    }
    None
}

/// The navigable index of the `inner`th row of `key`.
fn nav_index_of_row(
    sections: &[Section],
    _slots: &[work_view::Slot],
    key: &str,
    inner: usize,
) -> Option<usize> {
    let mut nav = 0usize;
    for section in sections {
        if section.key == key {
            // A section that is animating shut is drawn but not navigable: its
            // rows are on their way out, and selecting one would put the cursor
            // somewhere that is about to stop existing.
            return section.open.then_some(nav + 1 + inner);
        }
        nav += 1;
        if section.open {
            nav += section.rows.len();
        }
    }
    None
}

/// `── claude-code (1) · …/github/ahma ── ⢷⡪ ───────── 2⟳ 1◷ 14✓ ──`
fn draw_section_rule(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    area: Rect,
    section: &Section,
    selected: bool,
    now_ms: u64,
) {
    let unicode = state.unicode;
    let dash = if unicode { "─" } else { "-" };
    let mut spans: Vec<Span<'static>> = Vec::new();

    // Left edge: the selection marker replaces the first rule character, so a
    // selected header does not become one column wider than the others.
    if selected {
        spans.push(Span::styled(
            format!("{} ", selection_marker(true, unicode)),
            theme.section_rule_live(),
        ));
    } else {
        spans.push(Span::styled(format!("{dash}{dash} "), theme.section_rule()));
    }

    let title_style = if selected {
        theme.section_title_selected()
    } else {
        theme.section_title()
    };
    let mut title = section.header.client.clone();
    if let Some(n) = section.header.ordinal {
        title.push_str(&format!(" ({n})"));
    }
    spans.push(Span::styled(title, title_style));

    if !section.header.scope_short.is_empty() {
        spans.push(Span::styled(
            format!(" · {}", section.header.scope_short),
            theme.dim(),
        ));
    }

    // The window's model and spend: which LLM Enter will chat with here, and
    // how full its context is, without opening the chat.
    let llm = section_llm_suffix(state, &section.key);
    if !llm.is_empty() {
        spans.push(Span::styled(llm, theme.dim()));
    }

    // A liveness glyph beside a section that is running something, so "which of
    // these is actually doing anything" is answerable at a glance.
    if section.header.running {
        let glyphs = crate::liveness::panel_glyphs(
            section.key.len() as u64,
            now_ms / 150,
            crate::liveness::PanelPattern::CrissCross,
            unicode,
        );
        spans.push(Span::styled(format!(" {glyphs}"), theme.running()));
    }

    // A closed section still says what it is doing, or last did: a name alone
    // makes the reader open every section to find the one they want.
    if !section.open
        && let Some(latest) = &section.header.latest_title
    {
        let glyph = section
            .header
            .latest_status
            .as_ref()
            .map(|s| s.glyph(unicode))
            .unwrap_or("");
        spans.push(Span::styled(
            format!(" {dash}{dash} {glyph} {latest}"),
            theme.dim(),
        ));
    }

    let tallies = instance_tally_spans(&section.header.counts, unicode, theme);
    let tally_width: usize = tallies.iter().map(|s| s.content.chars().count()).sum();

    // Fill to the right edge so the rule is a rule and not a ragged line.
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let width = area.width as usize;
    let fill = width.saturating_sub(used + tally_width + 3);
    let rule_style = if section.header.running {
        theme.section_rule_live()
    } else {
        theme.section_rule()
    };
    spans.push(Span::styled(format!(" {}", dash.repeat(fill)), rule_style));
    spans.extend(tallies);
    spans.push(Span::styled(format!(" {dash}{dash}"), rule_style));

    super::truncate_row_spans_to_width(&mut spans, width);
    let mut paragraph = Paragraph::new(Line::from(spans));
    if selected {
        paragraph = paragraph.style(Style::default());
    }
    frame.render_widget(paragraph, area);
}

fn draw_empty_state(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let filter = if state.show_all_projects {
        "all projects"
    } else {
        "this project"
    };
    let lines = vec![
        Line::from(Span::styled(
            format!("Nothing has run in {filter} yet."),
            theme.dim(),
        )),
        Line::from(Span::styled(
            "[f] all projects   [i] chat   ? help".to_string(),
            theme.dim(),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

/// The left half of the status header: project filter, clients, tallies.
pub fn work_header_spans(state: &AppState, theme: &Theme) -> Vec<Span<'static>> {
    let filter = if state.show_all_projects {
        "all projects [f]"
    } else {
        "this project [f]"
    };
    let sections = state.work_sections.borrow();
    let clients = sections
        .iter()
        .filter(|s| !matches!(s.header.kind, crate::work_view::SectionKind::Local))
        .count();
    let mut totals = crate::task_tree::GroupCounts::default();
    for section in sections.iter() {
        totals.running += section.header.counts.running;
        totals.queued += section.header.counts.queued;
        totals.succeeded += section.header.counts.succeeded;
        totals.failed += section.header.counts.failed;
    }

    let mut spans = vec![
        Span::styled(" ahma · work · ".to_string(), theme.title()),
        Span::styled(filter.to_string(), theme.dim()),
        Span::styled(
            format!("   {clients} client{}", if clients == 1 { "" } else { "s" }),
            theme.dim(),
        ),
    ];
    spans.extend(instance_tally_spans(&totals, state.unicode, theme));
    spans
}

/// ` · qwen2.5-coder · ctx 38% (49k/128k) · ↑12.3k ↓2.1k` for a section: the
/// LLM this window chats with and what it has spent. Empty when the window
/// has no LLM of its own and no usage.
pub(crate) fn section_llm_suffix(state: &AppState, key: &str) -> String {
    let saved = state.get_window_llm(key);
    let is_active = state.active_target_instance.as_deref() == Some(key);
    let (model, base_url) = match saved {
        Some(cfg) => (Some(cfg.model.clone()), cfg.provider_url.clone()),
        None if is_active => (
            state.llm_selection.as_ref().map(|s| s.model.clone()),
            state.current_provider_url.clone(),
        ),
        None => (None, None),
    };
    let ctx = base_url.as_deref().and_then(|u| state.context_window(u));
    let meter = super::window_meter(state.window_usage.get(key), ctx, state.unicode);
    let mut out = String::new();
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        out.push_str(&format!(" · {model}"));
    }
    if !meter.is_empty() {
        out.push_str(&format!(" · {meter}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{OpStatus, Operation};
    use ahma_common::daemon_hub::InstanceInfo;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn instance(id: &str, client: &str, scope: &str) -> InstanceInfo {
        InstanceInfo {
            id: id.into(),
            pid: 7,
            mode: "stdio".into(),
            scope: scope.into(),
            label: "ahma".into(),
            client: Some(client.into()),
            session_id: Some(format!("sess-{id}")),
            client_pid: Some(99),
            sampling: false,
            ended_epoch_ms: None,
        }
    }

    fn state_with_work() -> AppState {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.active_instances = vec![
            instance("a", "claude-code", "/work/proj"),
            instance("b", "cursor", "/work/other"),
        ];
        let mut running = Operation::new("op-1", "run_terminal_command", OpStatus::Running);
        running.instance_id = Some("a".into());
        running.title = Some("cargo nextest run".into());
        let mut done = Operation::new("op-2", "run_terminal_command", OpStatus::Succeeded);
        done.instance_id = Some("b".into());
        done.title = Some("cargo build".into());
        state.operations = vec![running, done];
        state.show_all_projects = true;
        state
    }

    fn render(state: &AppState, width: u16, height: u16) -> String {
        let theme = Theme::new(true);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                draw_work_view(f, state, &theme, area, 1_000);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The view is rules and rows, not a box. A border would be a second frame
    /// drawn around the structure the headers already provide.
    /// A section names the LLM Enter will chat with in it and what that
    /// window has spent, so the choice is visible without opening the chat.
    #[test]
    fn a_section_shows_its_window_model_and_spend() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        assert_eq!(section_llm_suffix(&state, "inst-a"), "");

        state.set_window_llm(
            "inst-a",
            crate::session_config::WindowLlmConfig {
                provider: "Ollama".into(),
                model: "qwen2.5-coder".into(),
                provider_url: None,
            },
        );
        state.window_usage.insert(
            "inst-a".into(),
            crate::state::WindowUsage {
                prompt_tokens: 1500,
                completion_tokens: 300,
                last_prompt_tokens: 1500,
            },
        );
        assert_eq!(
            section_llm_suffix(&state, "inst-a"),
            " · qwen2.5-coder · ↑1.5k ↓300"
        );
    }

    #[test]
    fn the_work_view_draws_no_box() {
        let screen = render(&state_with_work(), 80, 12);
        for ch in ['┌', '┐', '└', '┘', '│', '├', '┤'] {
            assert!(
                !screen.contains(ch),
                "the work view must not be boxed, found {ch:?} in:\n{screen}"
            );
        }
        let first = screen.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("▶ ") || first.starts_with("──"),
            "the first line is a section rule, marked when selected:\n{screen}"
        );
    }

    /// A closed section says what it is doing, so a reader does not have to
    /// open each one to find the one they want.
    #[test]
    fn a_closed_section_names_what_it_is_doing() {
        let screen = render(&state_with_work(), 80, 12);
        assert!(screen.contains("claude-code"), "{screen}");
        assert!(screen.contains("cursor"), "{screen}");
        assert!(
            screen.contains("cargo nextest run"),
            "the running command is on the header:\n{screen}"
        );
    }

    /// Opening a section shows its rows; every other section stays one line.
    #[test]
    fn opening_a_section_shows_its_rows_and_the_others_stay_one_line() {
        let mut state = state_with_work();
        state.rebuild_work_view(0);
        let key = state.work_sections.borrow()[0].key.clone();
        state.open_section = Some(key);

        let screen = render(&state, 80, 12);
        let rules = screen
            .lines()
            .filter(|l| l.starts_with("──") || l.starts_with("▶ "))
            .count();
        assert_eq!(rules, 2, "both sections still show a header:\n{screen}");
        assert!(
            screen.contains("[P]"),
            "the open section's operation rows are drawn:\n{screen}"
        );
    }

    /// Mid-animation the opening section is drawn at a fraction of its height.
    #[test]
    fn an_animating_section_is_drawn_partly_open() {
        let mut state = state_with_work();
        // Give the section enough rows for a partial height to be visible.
        for i in 0..8 {
            let mut op = Operation::new(
                format!("extra-{i}"),
                "run_terminal_command",
                OpStatus::Succeeded,
            );
            op.instance_id = Some("a".into());
            op.title = Some(format!("step {i}"));
            state.operations.push(op);
        }
        state.rebuild_work_view(0);
        let key = state.work_sections.borrow()[0].key.clone();
        state.open_section = Some(key.clone());
        state.accordion = Some(crate::accordion::AccordionAnim {
            opening: Some(crate::accordion::Track { key, from: 0 }),
            closing: None,
            started_at_ms: 1_000,
        });

        // t=0 of the movement: nothing of the section is drawn yet.
        let theme = Theme::new(true);
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|f| {
                draw_work_view(
                    f,
                    &state,
                    &theme,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 16,
                    },
                    1_000,
                )
            })
            .unwrap();
        let heights_at_start = state.section_heights(1_000);
        let heights_midway = state.section_heights(1_150);
        let heights_at_end = state.section_heights(1_400);

        assert_eq!(heights_at_start[0], 0, "starts closed");
        assert!(
            heights_midway[0] > 0 && heights_midway[0] < heights_at_end[0],
            "midway it is partly open: {heights_midway:?} vs {heights_at_end:?}"
        );
        assert!(heights_at_end[0] > 0, "and ends open");
    }

    #[test]
    fn clicking_a_header_is_a_registered_target() {
        let state = state_with_work();
        let _ = render(&state, 80, 12);
        let targets = state.click_targets.borrow();
        assert!(
            targets
                .iter()
                .any(|(t, _)| matches!(t, ClickTarget::SectionHeader(_))),
            "each header is clickable"
        );
    }

    #[test]
    fn ascii_terminals_get_dashes_instead_of_rules() {
        let mut state = state_with_work();
        state.unicode = false;
        let theme = Theme::new(false);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|f| {
                draw_work_view(
                    f,
                    &state,
                    &theme,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 8,
                    },
                    0,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let first: String = (0..80)
            .map(|x| buffer.cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(
            first.starts_with("> ") || first.starts_with("--"),
            "ASCII fallback rule: {first}"
        );
        assert!(!first.contains('─'), "no box-drawing in an ASCII terminal");
    }

    #[test]
    fn an_empty_view_says_so_and_names_the_keys() {
        let state = AppState::new("http://localhost:3000", "HTTP", true);
        let screen = render(&state, 80, 6);
        assert!(screen.contains("Nothing has run"), "{screen}");
        assert!(screen.contains("[f]"), "{screen}");
        assert!(screen.contains("[i] chat"), "{screen}");
    }

    /// R24.8.1: the gutter appears only when there is something to scroll.
    ///
    /// The bar paints with a background colour rather than a glyph, so this
    /// reads the cell colours the way the pane's own scrollbar test does.
    #[test]
    fn the_scrollbar_appears_only_when_the_view_overflows() {
        let theme = Theme::new(true);
        let thumb = theme.scrollbar_thumb().bg;
        let track = theme.scrollbar_track().bg;

        let gutter_colours = |height: u16| -> Vec<Option<ratatui::style::Color>> {
            let mut state = state_with_work();
            state.rebuild_work_view(0);
            let key = state.work_sections.borrow()[0].key.clone();
            state.open_section = Some(key);
            for i in 0..40 {
                let mut op = Operation::new(
                    format!("many-{i}"),
                    "run_terminal_command",
                    OpStatus::Succeeded,
                );
                op.instance_id = Some("a".into());
                op.title = Some(format!("step {i}"));
                state.operations.push(op);
            }
            let (w, h) = (80u16, height);
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| {
                    draw_work_view(f, &state, &theme, Rect::new(0, 0, w, h), 0);
                })
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..h)
                .map(|y| buf.cell((w - 1, y)).unwrap().bg.into())
                .collect()
        };

        let short = gutter_colours(8);
        assert!(
            short.contains(&thumb) && short.contains(&track),
            "an overflowing view shows a thumb on a track: {short:?}"
        );

        let tall = gutter_colours(60);
        assert!(
            !tall.contains(&thumb) && !tall.contains(&track),
            "a view that fits carries no gutter: {tall:?}"
        );
    }
}
