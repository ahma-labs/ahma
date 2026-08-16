//! Task-tree construction for the monitor view.
//!
//! Pure functions that turn the flat merged operation list (hub instances +
//! the TUI's own server) into the compact tree the monitor pane renders:
//!
//! ```text
//! ▾ claude-code · stdio · ~/github/ahma        2▶ 1⧗ 14✓
//!   ⟳ cargo nextest run                        1m12s
//!   │ Compiling ahma_core v0.15.4
//!   │ Compiling ahma_mcp v0.15.4
//!   ▾ session build-loop
//!     ✓ cargo fmt --all                        0.3s
//!     ⟳ cargo clippy --all-targets             12s
//! ▸ ahma tui (you)                             1▶
//! ```
//!
//! One line per task; children (persistent-session commands, operations
//! spawned by other operations) indent under their parent; the single
//! *expanded* operation shows an inline live/historic output tail (accordion:
//! expanding one collapses the previous). No I/O and no ratatui types here so
//! the shape is unit-testable.

use crate::state::{OpStatus, Operation};
use ahma_common::daemon_hub::InstanceInfo;
use std::collections::{HashMap, HashSet};

/// Number of output-tail lines shown inline under the expanded operation.
pub const EXPANDED_TAIL_LINES: usize = 12;

/// Group key used for operations owned by the TUI's own connection (no hub
/// instance id).
pub const LOCAL_GROUP: &str = "";

/// One renderable row of the task tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub kind: RowKind,
    /// Indentation level (0 = instance header).
    pub depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// Header row for one ahma instance (one connected client / server).
    /// `group_id` is the hub instance id, or [`LOCAL_GROUP`] for the TUI's own
    /// connection.
    Instance {
        group_id: String,
        label: String,
        detail: String,
        counts: GroupCounts,
        collapsed: bool,
    },
    /// Synthetic grouping node, e.g. a persistent shell session.
    Group {
        key: String,
        label: String,
        collapsed: bool,
    },
    /// An operation; `op_index` indexes the `ops` slice passed to
    /// [`build_rows`].
    Op { op_index: usize, expanded: bool },
    /// One inline output line under the expanded operation.
    Output { op_index: usize, text: String },
}

/// Live/terminal tallies for an instance header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupCounts {
    pub running: usize,
    pub queued: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl GroupCounts {
    fn add(&mut self, status: &OpStatus) {
        match status {
            OpStatus::Running => self.running += 1,
            OpStatus::Pending | OpStatus::Waiting => self.queued += 1,
            OpStatus::Succeeded => self.succeeded += 1,
            OpStatus::Failed | OpStatus::Cancelled | OpStatus::Denied => self.failed += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.running + self.queued + self.succeeded + self.failed
    }
}

/// Does `scope` cover (or live inside) the project rooted at `project_root`?
///
/// Component-boundary prefix match in either direction: an instance scoped to
/// the repo root covers a TUI opened in a subdirectory, and an instance scoped
/// to a subdirectory belongs to a TUI opened at the repo root. Pure string
/// logic (no filesystem access) so it is deterministic and testable.
pub fn scope_matches_project(scope: &str, project_root: &str) -> bool {
    fn norm(p: &str) -> String {
        let p = p.trim_end_matches(['/', '\\']);
        p.replace('\\', "/")
    }
    let a = norm(scope);
    let b = norm(project_root);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let (shorter, longer) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    longer == shorter
        || (longer.starts_with(shorter.as_str()) && longer[shorter.len()..].starts_with('/'))
}

/// Inputs that shape the tree besides the operations themselves.
pub struct TreeOptions<'a> {
    /// Registered hub instances (for headers and project filtering).
    pub instances: &'a [InstanceInfo],
    /// The directory the TUI was started in; `None` disables filtering.
    pub project_root: Option<&'a str>,
    /// Show every instance regardless of project (the `f` toggle).
    pub show_all: bool,
    /// Operation id whose output tail is expanded inline (accordion).
    pub expanded_op: Option<&'a str>,
    /// Collapse keys (`inst:<id>`, `grp:<gid>:<key>`) hidden by the user.
    pub collapsed: &'a HashSet<String>,
}

/// Build the renderable rows for the current frame.
pub fn build_rows(ops: &[Operation], opts: &TreeOptions<'_>) -> Vec<TreeRow> {
    // ── 1. Dedup: the TUI's own server can be visible twice — through its hub
    // registration (instance_id = Some) and through the direct MCP status poll
    // (instance_id = None). Prefer the hub copy, which carries instance
    // grouping metadata.
    let hub_ids: HashSet<&str> = ops
        .iter()
        .filter(|o| o.instance_id.is_some())
        .map(|o| o.id.as_str())
        .collect();
    let visible: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.instance_id.is_some() || !hub_ids.contains(o.id.as_str()))
        .map(|(i, _)| i)
        .collect();

    // ── 2. Which instances pass the project filter?
    let instance_visible = |info: &InstanceInfo| -> bool {
        opts.show_all
            || match opts.project_root {
                Some(root) => scope_matches_project(&info.scope, root),
                None => true,
            }
    };
    let shown_instances: Vec<&InstanceInfo> = {
        let mut v: Vec<&InstanceInfo> = opts
            .instances
            .iter()
            .filter(|i| instance_visible(i))
            .collect();
        v.sort_by(|a, b| {
            display_label(a)
                .cmp(&display_label(b))
                .then(a.id.cmp(&b.id))
        });
        v
    };
    let shown_ids: HashSet<&str> = shown_instances.iter().map(|i| i.id.as_str()).collect();

    // ── 3. Bucket operations by group (instance id or LOCAL_GROUP).
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for &i in &visible {
        let op = &ops[i];
        match op.instance_id.as_deref() {
            Some(gid) => {
                // Ops from instances that are not registered any more (or are
                // filtered out) are shown only in show_all mode, under their
                // remembered id, so nothing silently disappears mid-session.
                if shown_ids.contains(gid) || opts.show_all {
                    groups.entry(gid).or_default().push(i);
                }
            }
            None => groups.entry(LOCAL_GROUP).or_default().push(i),
        }
    }

    // Instances first (sorted), then any orphaned groups, then local ops.
    let mut ordered: Vec<(String, Option<&InstanceInfo>)> = Vec::new();
    for info in &shown_instances {
        ordered.push((info.id.clone(), Some(info)));
    }
    let mut orphans: Vec<&str> = groups
        .keys()
        .copied()
        .filter(|g| *g != LOCAL_GROUP && !shown_ids.contains(g))
        .collect();
    orphans.sort_unstable();
    for gid in orphans {
        ordered.push((gid.to_string(), None));
    }
    if groups.contains_key(LOCAL_GROUP) {
        ordered.push((LOCAL_GROUP.to_string(), None));
    }

    // ── 4. Emit rows.
    let mut rows = Vec::new();
    for (gid, info) in &ordered {
        let mut member_idx: Vec<usize> = groups.get(gid.as_str()).cloned().unwrap_or_default();
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

        let inst_key = format!("inst:{gid}");
        let inst_collapsed = opts.collapsed.contains(&inst_key);
        let (label, detail) = match info {
            Some(info) => (
                display_label(info),
                format!("{} · {}", info.mode, short_path(&info.scope)),
            ),
            None if gid == LOCAL_GROUP => ("this terminal (you)".to_string(), String::new()),
            None => (format!("instance {gid}"), "disconnected".to_string()),
        };
        rows.push(TreeRow {
            kind: RowKind::Instance {
                group_id: gid.clone(),
                label,
                detail,
                counts,
                collapsed: inst_collapsed,
            },
            depth: 0,
        });
        if inst_collapsed {
            continue;
        }

        emit_group_ops(ops, gid, &member_idx, opts, &mut rows);
    }
    rows
}

/// Emit the operations of one instance group: session groups, parent/child
/// nesting, and the expanded op's inline output.
fn emit_group_ops(
    ops: &[Operation],
    gid: &str,
    member_idx: &[usize],
    opts: &TreeOptions<'_>,
    rows: &mut Vec<TreeRow>,
) {
    let by_id: HashMap<&str, usize> = member_idx
        .iter()
        .map(|&i| (ops[i].id.as_str(), i))
        .collect();

    // parent op index (in `ops`) → children; sessions keyed separately.
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut sessions: Vec<(String, Vec<usize>)> = Vec::new(); // insertion-ordered
    let mut roots: Vec<usize> = Vec::new();

    for &i in member_idx {
        match ops[i].parent_id.as_deref() {
            Some(p) if by_id.contains_key(p) => children.entry(by_id[p]).or_default().push(i),
            Some(p) if p.starts_with("session:") => {
                match sessions.iter_mut().find(|(k, _)| k == p) {
                    Some((_, v)) => v.push(i),
                    None => sessions.push((p.to_string(), vec![i])),
                }
            }
            _ => roots.push(i),
        }
    }

    fn emit_op(
        ops: &[Operation],
        i: usize,
        depth: u8,
        children: &HashMap<usize, Vec<usize>>,
        opts: &TreeOptions<'_>,
        rows: &mut Vec<TreeRow>,
    ) {
        let expanded = opts.expanded_op == Some(ops[i].id.as_str());
        rows.push(TreeRow {
            kind: RowKind::Op {
                op_index: i,
                expanded,
            },
            depth,
        });
        if expanded {
            let op = &ops[i];
            let tail: Vec<&String> = op
                .stdout_tail
                .iter()
                .rev()
                .take(EXPANDED_TAIL_LINES)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if tail.is_empty() {
                if let Some(summary) = &op.result_summary {
                    rows.push(TreeRow {
                        kind: RowKind::Output {
                            op_index: i,
                            text: summary.clone(),
                        },
                        depth: depth + 1,
                    });
                }
            } else {
                for line in tail {
                    rows.push(TreeRow {
                        kind: RowKind::Output {
                            op_index: i,
                            text: line.clone(),
                        },
                        depth: depth + 1,
                    });
                }
            }
        }
        if let Some(kids) = children.get(&i) {
            for &k in kids {
                emit_op(ops, k, depth + 1, children, opts, rows);
            }
        }
    }

    for i in roots {
        emit_op(ops, i, 1, &children, opts, rows);
    }
    for (key, members) in sessions {
        let grp_key = format!("grp:{gid}:{key}");
        let grp_collapsed = opts.collapsed.contains(&grp_key);
        let label = format!(
            "session {}",
            key.strip_prefix("session:").unwrap_or(key.as_str())
        );
        rows.push(TreeRow {
            kind: RowKind::Group {
                key: grp_key,
                label,
                collapsed: grp_collapsed,
            },
            depth: 1,
        });
        if grp_collapsed {
            continue;
        }
        for i in members {
            emit_op(ops, i, 2, &children, opts, rows);
        }
    }
}

/// Prefer the MCP client identity over the generic instance label.
fn display_label(info: &InstanceInfo) -> String {
    let base = info.client.as_deref().unwrap_or(&info.label);
    format!("{}:{}", base, info.pid)
}

/// Shorten a scope path for the header line: home-relative when possible,
/// then last two components.
fn short_path(p: &str) -> String {
    let normalized = p.replace('\\', "/");
    let parts: Vec<&str> = normalized.trim_end_matches('/').split('/').collect();
    if parts.len() <= 2 {
        return p.to_string();
    }
    format!("…/{}", parts[parts.len() - 2..].join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OpStatus;

    fn inst(id: &str, label: &str, client: Option<&str>, scope: &str) -> InstanceInfo {
        InstanceInfo {
            id: id.into(),
            pid: 7,
            mode: "stdio".into(),
            scope: scope.into(),
            label: label.into(),
            client: client.map(String::from),
        }
    }

    fn op(id: &str, instance: Option<&str>, status: OpStatus) -> Operation {
        let mut o = Operation::new(id, "run_terminal_command", status);
        o.instance_id = instance.map(String::from);
        o
    }

    fn opts<'a>(
        instances: &'a [InstanceInfo],
        project_root: Option<&'a str>,
        show_all: bool,
        expanded_op: Option<&'a str>,
        collapsed: &'a HashSet<String>,
    ) -> TreeOptions<'a> {
        TreeOptions {
            instances,
            project_root,
            show_all,
            expanded_op,
            collapsed,
        }
    }

    // ── scope_matches_project ────────────────────────────────────────────────

    #[test]
    fn scope_match_exact_and_boundaries() {
        assert!(scope_matches_project("/home/u/proj", "/home/u/proj"));
        assert!(scope_matches_project("/home/u/proj", "/home/u/proj/sub"));
        assert!(scope_matches_project("/home/u/proj/sub", "/home/u/proj"));
        assert!(scope_matches_project("/home/u/proj/", "/home/u/proj"));
        // Not a component boundary:
        assert!(!scope_matches_project("/home/u/proj", "/home/u/proj2"));
        assert!(!scope_matches_project("/home/u/other", "/home/u/proj"));
        assert!(!scope_matches_project("", "/home/u/proj"));
    }

    #[test]
    fn scope_match_windows_separators() {
        assert!(scope_matches_project(
            "C:\\Users\\u\\proj",
            "C:\\Users\\u\\proj\\crate"
        ));
    }

    // ── grouping / ordering ──────────────────────────────────────────────────

    #[test]
    fn instances_become_headers_with_counts() {
        let instances = vec![inst("i1", "VS Code", Some("claude-code"), "/p")];
        let ops = vec![
            op("a", Some("i1"), OpStatus::Running),
            op("b", Some("i1"), OpStatus::Succeeded),
            op("c", Some("i1"), OpStatus::Pending),
        ];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));

        let RowKind::Instance {
            label,
            counts,
            collapsed: c,
            ..
        } = &rows[0].kind
        else {
            panic!("first row must be the instance header, got {:?}", rows[0]);
        };
        assert_eq!(label, "claude-code:7", "client identity beats label");
        assert!(!c);
        assert_eq!(counts.running, 1);
        assert_eq!(counts.queued, 1);
        assert_eq!(counts.succeeded, 1);
        // Header + 3 ops
        assert_eq!(rows.len(), 4);
        assert!(
            rows[1..]
                .iter()
                .all(|r| matches!(r.kind, RowKind::Op { .. }))
        );
        assert!(rows[1..].iter().all(|r| r.depth == 1));
    }

    #[test]
    fn project_filter_hides_other_projects_and_show_all_reveals() {
        let instances = vec![
            inst("i1", "A", None, "/work/proj"),
            inst("i2", "B", None, "/work/other"),
        ];
        let ops = vec![
            op("a", Some("i1"), OpStatus::Running),
            op("b", Some("i2"), OpStatus::Running),
        ];
        let collapsed = HashSet::new();

        let rows = build_rows(
            &ops,
            &opts(&instances, Some("/work/proj"), false, None, &collapsed),
        );
        let headers: Vec<&RowKind> = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Instance { .. }))
            .map(|r| &r.kind)
            .collect();
        assert_eq!(headers.len(), 1, "only the matching instance shows");

        let rows_all = build_rows(
            &ops,
            &opts(&instances, Some("/work/proj"), true, None, &collapsed),
        );
        let headers_all = rows_all
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Instance { .. }))
            .count();
        assert_eq!(headers_all, 2, "show_all reveals every instance");
    }

    #[test]
    fn local_ops_group_under_local_header_after_instances() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let ops = vec![
            op("x", None, OpStatus::Running),
            op("a", Some("i1"), OpStatus::Running),
        ];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));
        let labels: Vec<String> = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Instance { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec!["A:7".to_string(), "this terminal (you)".to_string()]
        );
    }

    #[test]
    fn hub_copy_wins_over_local_duplicate() {
        // Same op id via hub (instance) and via the direct MCP poll (None):
        // only the hub copy is rendered.
        let instances = vec![inst("i1", "A", None, "/p")];
        let ops = vec![
            op("dup", None, OpStatus::Running),
            op("dup", Some("i1"), OpStatus::Running),
        ];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));
        let op_rows = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Op { .. }))
            .count();
        assert_eq!(op_rows, 1, "duplicate op must appear once");
        // And no "this terminal" header since the local group is empty.
        let headers = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Instance { .. }))
            .count();
        assert_eq!(headers, 1);
    }

    // ── nesting ──────────────────────────────────────────────────────────────

    #[test]
    fn session_ops_nest_under_group_node() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let mut s1 = op("s1", Some("i1"), OpStatus::Running);
        s1.parent_id = Some("session:dev".into());
        let mut s2 = op("s2", Some("i1"), OpStatus::Succeeded);
        s2.parent_id = Some("session:dev".into());
        let ops = vec![op("root", Some("i1"), OpStatus::Running), s1, s2];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));

        // header, root op, group node, s1, s2
        assert_eq!(rows.len(), 5);
        let RowKind::Group { label, .. } = &rows[2].kind else {
            panic!("expected session group at row 2, got {:?}", rows[2]);
        };
        assert_eq!(label, "session dev");
        assert_eq!(rows[2].depth, 1);
        assert_eq!(rows[3].depth, 2, "session members indent under the group");
        assert_eq!(rows[4].depth, 2);
    }

    #[test]
    fn collapsed_group_hides_members() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let mut s1 = op("s1", Some("i1"), OpStatus::Running);
        s1.parent_id = Some("session:dev".into());
        let ops = vec![s1];
        let mut collapsed = HashSet::new();
        collapsed.insert("grp:i1:session:dev".to_string());
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));
        assert!(
            rows.iter().all(|r| !matches!(r.kind, RowKind::Op { .. })),
            "collapsed session hides member ops"
        );
    }

    #[test]
    fn collapsed_instance_hides_everything_below() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let ops = vec![op("a", Some("i1"), OpStatus::Running)];
        let mut collapsed = HashSet::new();
        collapsed.insert("inst:i1".to_string());
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));
        assert_eq!(rows.len(), 1, "only the header remains");
        let RowKind::Instance { collapsed: c, .. } = &rows[0].kind else {
            panic!("expected header");
        };
        assert!(*c);
    }

    #[test]
    fn op_children_nest_recursively() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let mut child = op("child", Some("i1"), OpStatus::Running);
        child.parent_id = Some("parent".into());
        let mut grandchild = op("grand", Some("i1"), OpStatus::Pending);
        grandchild.parent_id = Some("child".into());
        let ops = vec![
            op("parent", Some("i1"), OpStatus::Running),
            child,
            grandchild,
        ];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));
        let depths: Vec<u8> = rows.iter().map(|r| r.depth).collect();
        assert_eq!(depths, vec![0, 1, 2, 3]);
    }

    #[test]
    fn unknown_parent_falls_back_to_root() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let mut orphan = op("orphan", Some("i1"), OpStatus::Running);
        orphan.parent_id = Some("vanished-op".into());
        let ops = vec![orphan];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));
        assert!(matches!(rows[1].kind, RowKind::Op { .. }));
        assert_eq!(rows[1].depth, 1);
    }

    // ── accordion expansion ──────────────────────────────────────────────────

    #[test]
    fn expanded_op_emits_output_tail_rows() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let mut o = op("a", Some("i1"), OpStatus::Running);
        for n in 0..20 {
            o.stdout_tail.push_back(format!("line {n}"));
        }
        let ops = vec![o, op("b", Some("i1"), OpStatus::Running)];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, Some("a"), &collapsed));

        let outputs: Vec<&str> = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Output { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), EXPANDED_TAIL_LINES, "bounded inline tail");
        assert_eq!(*outputs.last().unwrap(), "line 19", "newest line last");
        assert_eq!(outputs[0], format!("line {}", 20 - EXPANDED_TAIL_LINES));
        // Only op "a" is expanded.
        let expanded_ops = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Op { expanded: true, .. }))
            .count();
        assert_eq!(expanded_ops, 1);
    }

    #[test]
    fn expanded_terminal_op_without_tail_shows_result_summary() {
        let instances = vec![inst("i1", "A", None, "/p")];
        let mut o = op("a", Some("i1"), OpStatus::Succeeded);
        o.result_summary = Some("42 tests passed".into());
        let ops = vec![o];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, Some("a"), &collapsed));
        let RowKind::Output { text, .. } = &rows[2].kind else {
            panic!("expected summary output row, got {:?}", rows.get(2));
        };
        assert_eq!(text, "42 tests passed");
    }

    #[test]
    fn instance_with_no_ops_still_shows_header() {
        let instances = vec![inst("i1", "A", Some("cursor"), "/p")];
        let ops: Vec<Operation> = vec![];
        let collapsed = HashSet::new();
        let rows = build_rows(&ops, &opts(&instances, None, false, None, &collapsed));
        assert_eq!(rows.len(), 1, "idle instance is still visible");
        let RowKind::Instance { counts, .. } = &rows[0].kind else {
            panic!("expected header");
        };
        assert_eq!(counts.total(), 0);
    }
}
