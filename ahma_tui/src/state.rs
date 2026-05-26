//! Application state — single source of truth for all TUI panels.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

// ─── Ring-buffer capacities ───────────────────────────────────────────────────

pub const ACTIVITY_RING_CAP: usize = 64;
pub const LOG_RING_CAP: usize = 500;
pub const STDOUT_TAIL_CAP: usize = 100;

// ─── Focus ────────────────────────────────────────────────────────────────────

/// Which panel currently receives keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    AiActivity,
    OpsDag,
    Log,
    Palette,
}

impl Focus {
    pub fn cycle_next(self) -> Self {
        match self {
            Self::AiActivity => Self::OpsDag,
            Self::OpsDag => Self::Log,
            Self::Log => Self::AiActivity,
            Self::Palette => Self::AiActivity,
        }
    }

    pub fn cycle_prev(self) -> Self {
        match self {
            Self::AiActivity => Self::Log,
            Self::OpsDag => Self::AiActivity,
            Self::Log => Self::OpsDag,
            Self::Palette => Self::AiActivity,
        }
    }
}

// ─── AI Activity ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Success,
    Failed,
    Cancelled,
}

impl ActivityStatus {
    pub fn glyph(&self, unicode: bool) -> &'static str {
        if unicode {
            match self {
                Self::Running => "⟳",
                Self::Success => "✓",
                Self::Failed => "✗",
                Self::Cancelled => "⊘",
            }
        } else {
            match self {
                Self::Running => ">",
                Self::Success => "v",
                Self::Failed => "x",
                Self::Cancelled => "-",
            }
        }
    }
}

/// A single entry in the AI Activity ring buffer.
#[derive(Debug, Clone)]
pub struct AiActivityEntry {
    pub timestamp: chrono::DateTime<chrono::Local>,
    /// MCP method, e.g. `"tools/call"`, `"tools/list"`.
    pub method: String,
    /// Tool name, e.g. `"cargo_test"`.
    pub tool: String,
    pub status: ActivityStatus,
    pub elapsed: Option<Duration>,
    /// Short human-readable result, e.g. `"ok"` or `"error: ..."`
    pub summary: Option<String>,
    pub op_id: Option<String>,
}

// ─── Operations ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpStatus {
    Running,
    Pending,
    Succeeded,
    Failed,
    Cancelled,
    /// Awaiting another operation (blocked dependency).
    Waiting,
}

impl OpStatus {
    pub fn glyph(&self, unicode: bool) -> &'static str {
        if unicode {
            match self {
                Self::Running => "⟳",
                Self::Pending => "◷",
                Self::Succeeded => "✓",
                Self::Failed => "✗",
                Self::Cancelled => "⊘",
                Self::Waiting => "⏸",
            }
        } else {
            match self {
                Self::Running => ">",
                Self::Pending => ".",
                Self::Succeeded => "v",
                Self::Failed => "x",
                Self::Cancelled => "-",
                Self::Waiting => "|",
            }
        }
    }
}

/// A live or recently-completed async operation.
#[derive(Debug, Clone)]
pub struct Operation {
    pub id: String,
    pub tool_name: String,
    pub status: OpStatus,
    pub started_at: Option<Instant>,
    pub cwd: Option<String>,
    pub args: Vec<String>,
    /// ID of the parent operation this one is waiting for (for DAG rendering).
    pub parent_id: Option<String>,
    /// Tail of stdout for the detail pane.
    pub stdout_tail: VecDeque<String>,
    pub pid: Option<u32>,
    pub pinned: bool,
}

impl Operation {
    pub fn new(id: impl Into<String>, tool_name: impl Into<String>, status: OpStatus) -> Self {
        Self {
            id: id.into(),
            tool_name: tool_name.into(),
            status,
            started_at: Some(Instant::now()),
            cwd: None,
            args: vec![],
            parent_id: None,
            stdout_tail: VecDeque::with_capacity(STDOUT_TAIL_CAP),
            pid: None,
            pinned: false,
        }
    }

    pub fn elapsed_display(&self) -> String {
        let secs = self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m{:02}s", secs / 60, secs % 60)
        }
    }
}

// ─── Log ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

impl LogLevel {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Info => "INFO ",
            Self::Warn => "WARN ",
            Self::Error => "ERROR",
            Self::Debug => "DEBUG",
        }
    }

    pub fn parse_level(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "WARN" | "WARNING" => Self::Warn,
            "ERROR" | "ERR" => Self::Error,
            "DEBUG" | "TRACE" => Self::Debug,
            _ => Self::Info,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: chrono::DateTime<chrono::Local>,
    pub level: LogLevel,
    pub message: String,
}

// ─── Approval gate ────────────────────────────────────────────────────────────

/// A pending gate that requires user approval before an operation continues.
#[derive(Debug, Clone)]
pub struct ApprovalGate {
    pub op_id: String,
    pub description: String,
    pub deadline: Option<Instant>,
}

impl ApprovalGate {
    pub fn remaining_secs(&self) -> Option<u64> {
        self.deadline.map(|d| {
            let now = Instant::now();
            if d > now {
                (d - now).as_secs()
            } else {
                0
            }
        })
    }
}

// ─── Command palette ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    pub input: String,
    pub completions: Vec<String>,
    pub selected_completion: usize,
    pub visible: bool,
    /// When set, user must type `y` to confirm before the command is dispatched.
    pub confirm_prompt: Option<String>,
}

impl PaletteState {
    pub fn open(&mut self) {
        self.visible = true;
        self.input.clear();
        self.completions.clear();
        self.selected_completion = 0;
        self.confirm_prompt = None;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.input.clear();
        self.completions.clear();
        self.confirm_prompt = None;
    }

    pub fn update_completions(&mut self, tools: &[String]) {
        if self.input.is_empty() {
            self.completions = tools.to_vec();
        } else {
            let q = self.input.to_lowercase();
            self.completions = tools
                .iter()
                .filter(|t| t.to_lowercase().contains(&q))
                .cloned()
                .collect();
        }
        self.selected_completion = 0;
    }
}

// ─── Application state ────────────────────────────────────────────────────────

/// Top-level application state — owns all panel data and UI mode.
pub struct AppState {
    // ── Connection ──
    pub server_url: String,
    pub transport_label: String,
    pub server_healthy: bool,
    pub session_id: Option<String>,
    pub sandbox_status: String,
    pub workspace: String,

    // ── Panel data ──
    pub ai_activity: VecDeque<AiActivityEntry>,
    pub operations: Vec<Operation>,
    pub log: VecDeque<LogEntry>,
    pub approval: Option<ApprovalGate>,
    pub tools_list: Vec<String>,

    // ── UI state ──
    pub focus: Focus,
    pub ops_selected: usize,
    pub activity_scroll: usize,
    pub log_scroll: usize,
    pub log_filter: String,
    pub log_filter_active: bool,
    pub palette: PaletteState,
    pub show_help: bool,

    // ── Config ──
    pub unicode: bool,
    pub should_quit: bool,
}

impl AppState {
    pub fn new(
        server_url: impl Into<String>,
        transport_label: impl Into<String>,
        unicode: bool,
    ) -> Self {
        let workspace = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_default();
        Self {
            server_url: server_url.into(),
            transport_label: transport_label.into(),
            server_healthy: false,
            session_id: None,
            sandbox_status: "UNKNOWN".to_string(),
            workspace,
            ai_activity: VecDeque::with_capacity(ACTIVITY_RING_CAP),
            operations: vec![],
            log: VecDeque::with_capacity(LOG_RING_CAP),
            approval: None,
            tools_list: vec![],

            focus: Focus::default(),
            ops_selected: 0,
            activity_scroll: 0,
            log_scroll: 0,
            log_filter: String::new(),
            log_filter_active: false,
            palette: PaletteState::default(),
            show_help: false,

            unicode,
            should_quit: false,
        }
    }

    pub fn push_activity(&mut self, entry: AiActivityEntry) {
        if self.ai_activity.len() >= ACTIVITY_RING_CAP {
            self.ai_activity.pop_back();
        }
        self.ai_activity.push_front(entry);
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log.len() >= LOG_RING_CAP {
            self.log.pop_front();
        }
        self.log.push_back(entry);
        // When a new log line arrives, auto-scroll to bottom if we're already at or near bottom.
        let visible_len = self.log.len();
        if self.log_scroll + 5 >= visible_len.saturating_sub(1) {
            self.log_scroll = visible_len.saturating_sub(1);
        }
    }

    pub fn filtered_log(&self) -> Vec<&LogEntry> {
        if self.log_filter.is_empty() {
            self.log.iter().collect()
        } else {
            let q = self.log_filter.to_lowercase();
            self.log
                .iter()
                .filter(|e| {
                    e.message.to_lowercase().contains(&q)
                        || e.level.label().to_lowercase().contains(&q)
                })
                .collect()
        }
    }

    pub fn selected_op(&self) -> Option<&Operation> {
        self.operations.get(self.ops_selected)
    }

    pub fn upsert_operation(&mut self, op: Operation) {
        if let Some(existing) = self.operations.iter_mut().find(|o| o.id == op.id) {
            *existing = op;
        } else {
            self.operations.push(op);
        }
        self.clamp_ops_selection();
    }

    fn clamp_ops_selection(&mut self) {
        if !self.operations.is_empty() {
            self.ops_selected = self.ops_selected.min(self.operations.len() - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_activity_caps_at_ring_capacity() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        for i in 0..ACTIVITY_RING_CAP + 10 {
            s.push_activity(AiActivityEntry {
                timestamp: chrono::Local::now(),
                method: "tools/call".into(),
                tool: format!("tool_{i}"),
                status: ActivityStatus::Success,
                elapsed: None,
                summary: None,
                op_id: None,
            });
        }
        assert_eq!(s.ai_activity.len(), ACTIVITY_RING_CAP);
        // Most-recent entry should be at the front.
        assert!(s.ai_activity[0].tool.contains("tool_"));
    }

    #[test]
    fn focus_cycles_correctly() {
        assert_eq!(Focus::AiActivity.cycle_next(), Focus::OpsDag);
        assert_eq!(Focus::OpsDag.cycle_next(), Focus::Log);
        assert_eq!(Focus::Log.cycle_next(), Focus::AiActivity);
        assert_eq!(Focus::Log.cycle_prev(), Focus::OpsDag);
    }

    #[test]
    fn upsert_operation_updates_existing() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.upsert_operation(Operation::new("op1", "cargo_build", OpStatus::Running));
        s.upsert_operation(Operation::new("op1", "cargo_build", OpStatus::Succeeded));
        assert_eq!(s.operations.len(), 1);
        assert_eq!(s.operations[0].status, OpStatus::Succeeded);
    }
}
