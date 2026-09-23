//! The unified work view's model (SPEC R24.9).
//!
//! One section per client session — a Claude Code window, a Cursor window, the
//! hooks that ran in a terminal, this TUI's own commands — each with a header
//! that says who is working, where, and how it is going. Exactly one section is
//! open at a time; the rest are one line each.
//!
//! Everything here is pure: no ratatui types, no I/O, no clock. The renderer
//! asks for rows and heights, the input layer asks what is at a coordinate, and
//! both get the same answer from the same functions — which is what stops a
//! pane from being laid out to one size and drawn at another (SPEC R24.8.2).

use crate::state::{OpStatus, Operation};
use crate::task_tree::{
    GroupCounts, LOCAL_GROUP, RowKind, TreeOptions, TreeRow, emit_group_ops, scope_matches_project,
    short_path,
};
use ahma_common::daemon_hub::InstanceInfo;
use std::collections::{HashMap, HashSet};

/// What a section represents. Ordering of the variants is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SectionKind {
    /// An attached MCP client session (Claude Code, Cursor, …).
    Client,
    /// Commands an editor ran through ahma's shell hook.
    Hooks,
    /// A session that has ended but whose work is still inside the replay
    /// window — shown so recent work does not vanish the moment its client
    /// closes.
    Ended,
    /// This TUI: its own `!` commands and its chat's tool calls.
    Local,
}

/// The one line a section always shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionHeader {
    pub kind: SectionKind,
    /// Who is working: the MCP client's own name where it gave one.
    pub client: String,
    /// Distinguishes two sections that would otherwise read identically — two
    /// windows of the same editor on the same project.
    pub ordinal: Option<u8>,
    /// Where, shortened for a header line.
    pub scope_short: String,
    pub counts: GroupCounts,
    /// Something is running right now, so the header earns a liveness glyph.
    pub running: bool,
    /// What this section is doing, or last did — so a closed section is still
    /// informative rather than just a name.
    pub latest_title: Option<String>,
    pub latest_status: Option<OpStatus>,
    /// Transport, pids and session id: the footnote for connection debugging,
    /// which belongs in the detail overlay and not on the header (R24.8.6).
    pub identity: String,
    /// Whether this section's scope matches the project the TUI was opened in.
    pub in_project: bool,
}

/// One client's work: a header, and the rows shown when it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Stable across re-registration: the session id where there is one, so a
    /// section keeps its place and its open/closed state when its instance
    /// re-registers (SPEC R-DAEMON.6).
    pub key: String,
    pub header: SectionHeader,
    /// Content rows, built only for the open section (and, while it animates
    /// shut, the one closing).
    pub rows: Vec<TreeRow>,
    pub open: bool,
}

/// What shapes the sections besides the operations themselves.
pub struct SectionOptions<'a> {
    pub instances: &'a [InstanceInfo],
    /// The directory the TUI was started in; `None` disables filtering.
    pub project_root: Option<&'a str>,
    /// Show every project, not just this one (the `f` toggle).
    pub show_all: bool,
    /// The section whose rows are shown.
    pub open_section: Option<&'a str>,
    /// A section still animating shut: its rows are needed to draw the closing
    /// frames, but it is not navigable.
    pub closing_section: Option<&'a str>,
    /// The operation whose output tail is expanded inside the open section.
    pub expanded_op: Option<&'a str>,
    /// Fold state for groups *inside* a section (`grp:…`).
    pub collapsed: &'a HashSet<String>,
    /// How many output lines an expanded operation shows.
    pub tail_lines: usize,
}

/// How many output lines fit an expanded operation, given the space available.
///
/// Adaptive rather than fixed: a twelve-line tail is most of a short terminal
/// and a sliver of a tall one.
pub fn tail_lines_for(area_height: u16) -> usize {
    if area_height == 0 {
        // Nothing has been drawn yet; the historical fixed window is as good a
        // guess as any and keeps pre-draw row maths stable.
        return crate::task_tree::EXPANDED_TAIL_LINES;
    }
    ((area_height as usize) / 2).clamp(4, 24)
}

/// Build the sections for this frame.
pub fn build_sections(ops: &[Operation], opts: &SectionOptions<'_>) -> Vec<Section> {
    let visible = crate::task_tree::dedup_visible_ops(ops);
    let by_key = bucket_ops_by_section(ops, &visible, opts);
    let mut sections = assemble_sections(ops, &by_key, opts);
    order_sections(&mut sections);
    assign_ordinals(&mut sections);
    sections
}

/// The section key an operation belongs to.
fn section_key_for_op(op: &Operation, hook_instances: &HashSet<&str>) -> String {
    match op.instance_id.as_deref() {
        Some(id) if hook_instances.contains(id) => HOOKS_KEY.to_string(),
        // A hook whose instance has already gone still belongs with the hooks:
        // the operation carries its own origin, which outlives the registration.
        _ if op.origin.as_deref() == Some("hook") => HOOKS_KEY.to_string(),
        Some(id) => id.to_string(),
        None => LOCAL_GROUP.to_string(),
    }
}

/// The key every hook shares. Hooked commands are one instance per command, so
/// one section per instance would be a wall of one-line sections.
pub const HOOKS_KEY: &str = "hooks";

fn bucket_ops_by_section(
    ops: &[Operation],
    visible: &[usize],
    opts: &SectionOptions<'_>,
) -> HashMap<String, Vec<usize>> {
    let hook_instances: HashSet<&str> = opts
        .instances
        .iter()
        .filter(|i| i.mode == "hook")
        .map(|i| i.id.as_str())
        .collect();

    let shown: HashSet<&str> = opts
        .instances
        .iter()
        .filter(|i| instance_visible(i, opts))
        .map(|i| i.id.as_str())
        .collect();

    let mut out: HashMap<String, Vec<usize>> = HashMap::new();
    for &i in visible {
        let op = &ops[i];
        let key = section_key_for_op(op, &hook_instances);
        let keep = match op.instance_id.as_deref() {
            Some(id) => shown.contains(id) || opts.show_all || key == HOOKS_KEY,
            None => true,
        };
        if keep {
            out.entry(key).or_default().push(i);
        }
    }
    out
}

fn instance_visible(info: &InstanceInfo, opts: &SectionOptions<'_>) -> bool {
    if opts.show_all {
        return true;
    }
    match opts.project_root {
        // An instance whose scope is not established yet is not "somewhere
        // else" — it is not yet anywhere. Showing it is how a user sees a
        // session that is still handshaking, rather than wondering where their
        // editor went (SPEC R24.3).
        Some(_) if info.scope.is_empty() => true,
        Some(root) => scope_matches_project(&info.scope, root),
        None => true,
    }
}

fn assemble_sections(
    ops: &[Operation],
    by_key: &HashMap<String, Vec<usize>>,
    opts: &SectionOptions<'_>,
) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // A section per attached (or recently ended) instance, hooks folded into one.
    for info in opts.instances.iter().filter(|i| instance_visible(i, opts)) {
        let key = if info.mode == "hook" {
            HOOKS_KEY.to_string()
        } else {
            info.id.clone()
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        let members = by_key.get(&key).cloned().unwrap_or_default();
        sections.push(section_for(&key, Some(info), ops, &members, opts));
    }

    // Then any section that has work but no instance to explain it: an
    // instance that unregistered before this frame, or the TUI's own commands.
    let mut orphans: Vec<&String> = by_key.keys().filter(|k| !seen.contains(*k)).collect();
    orphans.sort();
    for key in orphans {
        let members = by_key.get(key).cloned().unwrap_or_default();
        sections.push(section_for(key, None, ops, &members, opts));
    }

    sections
}

fn section_for(
    key: &str,
    info: Option<&InstanceInfo>,
    ops: &[Operation],
    members: &[usize],
    opts: &SectionOptions<'_>,
) -> Section {
    let mut member_idx = members.to_vec();
    member_idx.sort_by(|&a, &b| {
        ops[a]
            .started_time
            .cmp(&ops[b].started_time)
            .then_with(|| ops[a].id.cmp(&ops[b].id))
    });

    let mut counts = GroupCounts::default();
    for &i in &member_idx {
        counts.add(&ops[i].status);
    }

    let open = opts.open_section == Some(key);
    let animating = opts.closing_section == Some(key);
    let rows = if open || animating {
        let mut rows = Vec::new();
        let tree_opts = TreeOptions {
            instances: opts.instances,
            project_root: opts.project_root,
            show_all: opts.show_all,
            expanded_op: opts.expanded_op,
            collapsed: opts.collapsed,
        };
        emit_group_ops(ops, key, &member_idx, &tree_opts, &mut rows);
        rows
    } else {
        Vec::new()
    };

    let (latest_title, latest_status) = latest_activity(ops, &member_idx);

    Section {
        key: key.to_string(),
        header: SectionHeader {
            kind: kind_for(key, info),
            client: client_name(key, info),
            ordinal: None,
            scope_short: scope_for(key, info, ops, &member_idx),
            counts,
            running: counts.running > 0,
            latest_title,
            latest_status,
            identity: identity_footnote(info),
            in_project: info.is_none_or(|i| instance_visible(i, opts)),
        },
        rows,
        open,
    }
}

fn kind_for(key: &str, info: Option<&InstanceInfo>) -> SectionKind {
    if key == HOOKS_KEY {
        return SectionKind::Hooks;
    }
    if key == LOCAL_GROUP {
        return SectionKind::Local;
    }
    match info {
        Some(i) if i.mode == "tui" => SectionKind::Local,
        Some(i) if i.ended_epoch_ms.is_some() => SectionKind::Ended,
        Some(_) => SectionKind::Client,
        // Work whose instance is gone from the list entirely.
        None => SectionKind::Ended,
    }
}

fn client_name(key: &str, info: Option<&InstanceInfo>) -> String {
    if key == HOOKS_KEY {
        return "hooks".to_string();
    }
    if key == LOCAL_GROUP {
        return "this terminal (you)".to_string();
    }
    match info {
        Some(i) if i.mode == "tui" => "this terminal (you)".to_string(),
        Some(i) => i.client.clone().unwrap_or_else(|| i.label.clone()),
        None => "ended session".to_string(),
    }
}

fn scope_for(
    key: &str,
    info: Option<&InstanceInfo>,
    ops: &[Operation],
    members: &[usize],
) -> String {
    if key == LOCAL_GROUP {
        return String::new();
    }
    if let Some(info) = info {
        if info.scope.is_empty() {
            // Honest about a scope that does not exist yet, rather than
            // printing a placeholder that reads like a directory (R24.3).
            return "no scope yet".to_string();
        }
        return short_path(&info.scope);
    }
    members
        .iter()
        .find_map(|&i| ops[i].scope.as_deref())
        .map(short_path)
        .unwrap_or_default()
}

/// What this section is doing, or last did.
///
/// A running operation wins, because that is what the user wants to know now;
/// otherwise the most recent one, so a closed section still says something.
fn latest_activity(ops: &[Operation], members: &[usize]) -> (Option<String>, Option<OpStatus>) {
    let newest_running = members
        .iter()
        .filter(|&&i| matches!(ops[i].status, OpStatus::Running))
        .max_by_key(|&&i| ops[i].started_time);
    let pick = newest_running.or_else(|| members.iter().max_by_key(|&&i| ops[i].started_time));
    match pick {
        Some(&i) => (Some(ops[i].display_name()), Some(ops[i].status.clone())),
        None => (None, None),
    }
}

/// Transport, pids and session id — the connection-debugging footnote
/// (SPEC R24.8.6). Never the headline.
pub fn identity_footnote(info: Option<&InstanceInfo>) -> String {
    let Some(info) = info else {
        return String::new();
    };
    let mut parts = vec![info.mode.clone(), format!("pid {}", info.pid)];
    if let Some(pid) = info.client_pid {
        parts.push(format!("client pid {pid}"));
    }
    if let Some(session) = &info.session_id {
        parts.push(format!("session {session}"));
    }
    if info.ended_epoch_ms.is_some() {
        parts.push("ended".to_string());
    }
    parts.join(" · ")
}

/// Display order.
///
/// Deliberately independent of activity: a section that starts or finishes work
/// must not jump position under the user's cursor. Liveness is shown by the
/// header's glyph and tallies, not by reordering.
fn order_sections(sections: &mut [Section]) {
    sections.sort_by(|a, b| {
        b.header
            .in_project
            .cmp(&a.header.in_project)
            .then(a.header.kind.cmp(&b.header.kind))
            .then(a.header.client.cmp(&b.header.client))
            .then(a.key.cmp(&b.key))
    });
}

/// Number sections that would otherwise read identically — two windows of the
/// same editor on the same project.
fn assign_ordinals(sections: &mut [Section]) {
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for s in sections.iter() {
        *counts
            .entry((s.header.client.clone(), s.header.scope_short.clone()))
            .or_default() += 1;
    }
    let mut seen: HashMap<(String, String), u8> = HashMap::new();
    for s in sections.iter_mut() {
        let key = (s.header.client.clone(), s.header.scope_short.clone());
        if counts.get(&key).copied().unwrap_or(0) < 2 {
            continue;
        }
        let n = seen.entry(key).or_insert(0);
        *n += 1;
        s.header.ordinal = Some(*n);
    }
}

/// The navigable rows: every section's header, plus the open section's content.
pub fn flatten(sections: &[Section]) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for section in sections {
        rows.push(TreeRow {
            kind: RowKind::Instance {
                group_id: section.key.clone(),
                label: section.header.client.clone(),
                detail: section.header.scope_short.clone(),
                counts: section.header.counts,
                collapsed: !section.open,
            },
            depth: 0,
        });
        if section.open {
            rows.extend(section.rows.iter().cloned());
        }
    }
    rows
}

/// Where one section sits in the view's own coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub key: String,
    /// Row of this section's header.
    pub header_y: usize,
    /// Row its content starts on.
    pub content_y: usize,
    /// Content rows drawn this frame — eased while animating, so it is not
    /// always `natural_h`.
    pub alloc_h: usize,
    /// Content rows the section would show fully open.
    pub natural_h: usize,
}

/// Lay the sections out, one header row each plus whatever content height the
/// caller allocated.
///
/// The renderer and the hit-test share this, so what is drawn and what is
/// clicked can never disagree (SPEC R24.8.2).
pub fn layout(sections: &[Section], alloc: &[usize]) -> (Vec<Slot>, usize) {
    let mut slots = Vec::with_capacity(sections.len());
    let mut y = 0usize;
    for (i, section) in sections.iter().enumerate() {
        let alloc_h = alloc.get(i).copied().unwrap_or(0).min(section.rows.len());
        slots.push(Slot {
            key: section.key.clone(),
            header_y: y,
            content_y: y + 1,
            alloc_h,
            natural_h: section.rows.len(),
        });
        y += 1 + alloc_h;
    }
    (slots, y)
}

/// What is at a row of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// A section's header line.
    Header(String),
    /// A content row: which section, and how far into its rows.
    Row { key: String, inner: usize },
}

/// What is at `y`, in the view's own coordinates.
pub fn hit(slots: &[Slot], y: usize) -> Option<Hit> {
    for slot in slots {
        if y == slot.header_y {
            return Some(Hit::Header(slot.key.clone()));
        }
        if slot.alloc_h > 0 && y >= slot.content_y && y < slot.content_y + slot.alloc_h {
            return Some(Hit::Row {
                key: slot.key.clone(),
                inner: y - slot.content_y,
            });
        }
    }
    None
}

/// Where a navigable row index sits in view coordinates.
pub fn nav_row_y(slots: &[Slot], sections: &[Section], nav_index: usize) -> Option<usize> {
    let mut nav = 0usize;
    for (slot, section) in slots.iter().zip(sections) {
        if nav == nav_index {
            return Some(slot.header_y);
        }
        nav += 1;
        if section.open {
            for inner in 0..section.rows.len() {
                if nav == nav_index {
                    return Some(slot.content_y + inner);
                }
                nav += 1;
            }
        }
    }
    None
}

/// The scroll offset for the next frame.
///
/// `follow` is what separates a keyboard move — which should pull the selection
/// into view — from a wheel scroll, which must not be undone by the very next
/// frame re-centring on a selection the user did not move.
pub fn scroll_for(
    selected_y: Option<usize>,
    current: usize,
    visible: usize,
    total: usize,
    follow: bool,
) -> usize {
    let max_scroll = total.saturating_sub(visible);
    let mut scroll = current.min(max_scroll);
    if follow
        && visible > 0
        && let Some(y) = selected_y
    {
        if y < scroll {
            scroll = y;
        } else if y >= scroll + visible {
            scroll = y + 1 - visible;
        }
    }
    scroll.min(max_scroll)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(id: &str, client: Option<&str>, scope: &str, mode: &str) -> InstanceInfo {
        InstanceInfo {
            id: id.into(),
            pid: 7,
            mode: mode.into(),
            scope: scope.into(),
            label: "ahma".into(),
            client: client.map(String::from),
            session_id: Some(format!("sess-{id}")),
            client_pid: Some(99),
            sampling: false,
            ended_epoch_ms: None,
        }
    }

    fn op(id: &str, instance: Option<&str>, status: OpStatus) -> Operation {
        let mut o = Operation::new(id, "run_terminal_command", status);
        o.instance_id = instance.map(String::from);
        o.title = Some(format!("cmd {id}"));
        o
    }

    fn opts<'a>(
        instances: &'a [InstanceInfo],
        collapsed: &'a HashSet<String>,
        open: Option<&'a str>,
    ) -> SectionOptions<'a> {
        SectionOptions {
            instances,
            project_root: None,
            show_all: false,
            open_section: open,
            closing_section: None,
            expanded_op: None,
            collapsed,
            tail_lines: 12,
        }
    }

    #[test]
    fn one_section_per_client_with_hooks_and_local_folded() {
        let instances = vec![
            instance("a", Some("claude-code"), "/work/proj", "stdio"),
            instance("b", Some("cursor"), "/work/other", "stdio"),
            instance("h", Some("claude-code"), "/work/proj", "hook"),
        ];
        let ops = vec![
            op("1", Some("a"), OpStatus::Running),
            op("2", Some("b"), OpStatus::Succeeded),
            op("3", Some("h"), OpStatus::Succeeded),
            op("4", None, OpStatus::Running),
        ];
        let collapsed = HashSet::new();
        let sections = build_sections(&ops, &opts(&instances, &collapsed, None));

        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        assert!(keys.contains(&"a") && keys.contains(&"b"));
        assert!(keys.contains(&HOOKS_KEY), "hooks fold into one section");
        assert!(
            keys.contains(&LOCAL_GROUP),
            "the TUI's own work has a section"
        );
        let local = sections.iter().find(|s| s.key == LOCAL_GROUP).unwrap();
        assert_eq!(local.header.client, "this terminal (you)");
        assert_eq!(
            sections.last().unwrap().key,
            LOCAL_GROUP,
            "and it comes last"
        );
    }

    /// A hooked command whose instance has already gone still lands with the
    /// hooks: the operation carries its own origin, which outlives the
    /// registration that produced it.
    #[test]
    fn a_hook_op_whose_instance_has_gone_still_lands_in_the_hooks_section() {
        let instances: Vec<InstanceInfo> = vec![];
        let mut hook_op = op("1", Some("vanished"), OpStatus::Succeeded);
        hook_op.origin = Some("hook".into());
        let ops = vec![hook_op];
        let collapsed = HashSet::new();
        let sections = build_sections(&ops, &opts(&instances, &collapsed, None));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].key, HOOKS_KEY);
        assert_eq!(sections[0].header.kind, SectionKind::Hooks);
    }

    /// Position must not depend on activity: a section that starts or finishes
    /// work would otherwise jump under the user's cursor.
    #[test]
    fn ordering_does_not_change_when_work_starts_or_finishes() {
        let instances = vec![
            instance("a", Some("claude-code"), "/work/proj", "stdio"),
            instance("b", Some("cursor"), "/work/proj", "stdio"),
        ];
        let collapsed = HashSet::new();

        let before = build_sections(
            &[
                op("1", Some("a"), OpStatus::Running),
                op("2", Some("b"), OpStatus::Succeeded),
            ],
            &opts(&instances, &collapsed, None),
        );
        let after = build_sections(
            &[
                op("1", Some("a"), OpStatus::Succeeded),
                op("2", Some("b"), OpStatus::Running),
                op("3", Some("b"), OpStatus::Running),
            ],
            &opts(&instances, &collapsed, None),
        );

        let keys = |v: &[Section]| -> Vec<String> { v.iter().map(|s| s.key.clone()).collect() };
        assert_eq!(keys(&before), keys(&after));
    }

    #[test]
    fn two_windows_of_one_editor_on_one_project_are_numbered() {
        let instances = vec![
            instance("a", Some("claude-code"), "/work/proj", "stdio"),
            instance("b", Some("claude-code"), "/work/proj", "stdio"),
            instance("c", Some("cursor"), "/work/proj", "stdio"),
        ];
        let collapsed = HashSet::new();
        let sections = build_sections(&[], &opts(&instances, &collapsed, None));

        let claude: Vec<Option<u8>> = sections
            .iter()
            .filter(|s| s.header.client == "claude-code")
            .map(|s| s.header.ordinal)
            .collect();
        assert_eq!(
            claude,
            vec![Some(1), Some(2)],
            "identical headers are numbered"
        );
        let cursor = sections
            .iter()
            .find(|s| s.header.client == "cursor")
            .unwrap();
        assert_eq!(cursor.header.ordinal, None, "a unique header is not");
    }

    #[test]
    fn the_header_says_what_is_running_now_and_falls_back_to_the_last_thing() {
        let instances = vec![instance("a", Some("claude-code"), "/work/proj", "stdio")];
        let collapsed = HashSet::new();

        let mut older = op("1", Some("a"), OpStatus::Running);
        older.started_time -= chrono::Duration::seconds(60);
        let newest_done = op("2", Some("a"), OpStatus::Succeeded);
        let sections = build_sections(&[older, newest_done], &opts(&instances, &collapsed, None));
        assert_eq!(
            sections[0].header.latest_title.as_deref(),
            Some("cmd 1"),
            "a running op wins over a newer finished one"
        );
        assert!(sections[0].header.running);

        let sections = build_sections(
            &[
                op("1", Some("a"), OpStatus::Succeeded),
                op("2", Some("a"), OpStatus::Failed),
            ],
            &opts(&instances, &collapsed, None),
        );
        assert!(
            sections[0].header.latest_title.is_some(),
            "a quiet section still says what it last did"
        );
        assert!(!sections[0].header.running);
    }

    #[test]
    fn only_the_open_section_has_rows_and_a_closing_one_keeps_them() {
        let instances = vec![
            instance("a", Some("claude-code"), "/work/proj", "stdio"),
            instance("b", Some("cursor"), "/work/proj", "stdio"),
        ];
        let ops = vec![
            op("1", Some("a"), OpStatus::Running),
            op("2", Some("b"), OpStatus::Running),
        ];
        let collapsed = HashSet::new();
        let mut o = opts(&instances, &collapsed, Some("a"));
        o.closing_section = Some("b");
        let sections = build_sections(&ops, &o);

        let a = sections.iter().find(|s| s.key == "a").unwrap();
        let b = sections.iter().find(|s| s.key == "b").unwrap();
        assert!(a.open && !a.rows.is_empty());
        assert!(
            !b.open && !b.rows.is_empty(),
            "a closing section keeps its rows so it can be drawn shrinking"
        );

        let rows = flatten(&sections);
        let headers = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Instance { .. }))
            .count();
        assert_eq!(headers, sections.len(), "every section shows its header");
        let content = rows.len() - headers;
        assert_eq!(content, a.rows.len(), "only the open section's rows appear");
    }

    #[test]
    fn an_unestablished_scope_is_shown_rather_than_hidden() {
        let instances = vec![instance("a", Some("claude-code"), "", "stdio")];
        let collapsed = HashSet::new();
        let mut o = opts(&instances, &collapsed, None);
        o.project_root = Some("/work/proj");
        let sections = build_sections(&[], &o);

        assert_eq!(sections.len(), 1, "a session still handshaking is visible");
        assert_eq!(sections[0].header.scope_short, "no scope yet");
    }

    #[test]
    fn tail_lines_adapt_to_the_space_available() {
        assert_eq!(tail_lines_for(0), crate::task_tree::EXPANDED_TAIL_LINES);
        assert_eq!(tail_lines_for(10), 5);
        assert_eq!(tail_lines_for(6), 4, "clamped at the bottom");
        assert_eq!(tail_lines_for(200), 24, "and at the top");
    }

    fn section(key: &str, rows: usize, open: bool) -> Section {
        Section {
            key: key.into(),
            header: SectionHeader {
                kind: SectionKind::Client,
                client: key.into(),
                ordinal: None,
                scope_short: "/w".into(),
                counts: GroupCounts::default(),
                running: false,
                latest_title: None,
                latest_status: None,
                identity: String::new(),
                in_project: true,
            },
            rows: (0..rows)
                .map(|i| TreeRow {
                    kind: RowKind::Op {
                        op_index: i,
                        expanded: false,
                    },
                    depth: 1,
                })
                .collect(),
            open,
        }
    }

    #[test]
    fn layout_places_headers_and_content_and_hit_agrees_with_it() {
        let sections = vec![
            section("a", 3, false),
            section("b", 4, true),
            section("c", 2, false),
        ];
        let (slots, total) = layout(&sections, &[0, 4, 0]);

        assert_eq!(slots[0].header_y, 0);
        assert_eq!(slots[1].header_y, 1);
        assert_eq!(slots[2].header_y, 6);
        assert_eq!(total, 7, "three headers plus four open rows");

        assert_eq!(hit(&slots, 0), Some(Hit::Header("a".into())));
        assert_eq!(
            hit(&slots, 3),
            Some(Hit::Row {
                key: "b".into(),
                inner: 1
            })
        );
        assert_eq!(hit(&slots, 6), Some(Hit::Header("c".into())));
        assert_eq!(hit(&slots, 99), None);
    }

    /// Mid-animation a section shows fewer rows than it has; what is drawn and
    /// what is clickable must still be the same rows (R24.8.2).
    #[test]
    fn a_partly_open_section_is_hit_tested_at_the_height_it_is_drawn() {
        let sections = vec![section("a", 6, true), section("b", 2, false)];
        let (slots, total) = layout(&sections, &[2, 0]);
        assert_eq!(slots[0].alloc_h, 2);
        assert_eq!(slots[1].header_y, 3);
        assert_eq!(total, 4);
        assert!(hit(&slots, 2).is_some(), "the drawn rows are hittable");
        assert_eq!(
            hit(&slots, 3),
            Some(Hit::Header("b".into())),
            "and the rows it has not drawn are not"
        );
    }

    #[test]
    fn scrolling_follows_the_selection_only_when_asked() {
        // A wheel scroll is kept.
        assert_eq!(scroll_for(Some(0), 5, 10, 30, false), 5);
        // A keyboard move pulls the selection into view, both ways.
        assert_eq!(scroll_for(Some(0), 5, 10, 30, true), 0);
        assert_eq!(scroll_for(Some(25), 0, 10, 30, true), 16);
        // And never past the end.
        assert_eq!(scroll_for(None, 99, 10, 30, false), 20);
        assert_eq!(scroll_for(None, 99, 40, 30, false), 0);
    }

    #[test]
    fn nav_rows_map_to_the_lines_they_are_drawn_on() {
        let sections = vec![section("a", 2, true), section("b", 3, false)];
        let (slots, _) = layout(&sections, &[2, 0]);
        assert_eq!(nav_row_y(&slots, &sections, 0), Some(0)); // header a
        assert_eq!(nav_row_y(&slots, &sections, 1), Some(1)); // a's first row
        assert_eq!(nav_row_y(&slots, &sections, 2), Some(2));
        assert_eq!(nav_row_y(&slots, &sections, 3), Some(3)); // header b
        assert_eq!(nav_row_y(&slots, &sections, 4), None);
    }

    #[test]
    fn the_identity_footnote_carries_the_connection_details() {
        let info = instance("a", Some("claude-code"), "/w", "stdio");
        let footnote = identity_footnote(Some(&info));
        assert!(footnote.contains("stdio"));
        assert!(footnote.contains("pid 7"));
        assert!(footnote.contains("session sess-a"));
        assert_eq!(identity_footnote(None), "");
    }
}
