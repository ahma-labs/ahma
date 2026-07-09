//! Application state — single source of truth for all TUI panels.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use crate::mcp_connections::McpConnectionManager;
use crate::session_config::TuiSessionConfig;
#[cfg(feature = "tui")]
use ratatui::layout::Rect;
#[cfg(feature = "tui")]
use tui_textarea::TextArea;

// ─── Ring-buffer capacities ───────────────────────────────────────────────────

pub const ACTIVITY_RING_CAP: usize = 64;
pub const LOG_RING_CAP: usize = 500;
pub const STDOUT_TAIL_CAP: usize = 100;
pub const CHAT_HISTORY_CAP: usize = 200;

// ─── TUI mode ─────────────────────────────────────────────────────────────────

/// Top-level mode of the TUI.  Switched with `/mode chat` / `/mode monitor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Chat-first interface: multi-line input → LLM, `/` opens command navigator.
    #[default]
    Chat,
    /// Original 4-pane monitoring dashboard (AI Activity, Ops DAG, Detail, Log).
    Monitor,
}

// ─── Liveness State Machine ───────────────────────────────────────────────────

/// State of the turn's streaming liveness indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LivenessState {
    #[default]
    Idle,
    Thinking,
    Streaming,
}

// ─── Chat history ─────────────────────────────────────────────────────────────

/// A single entry in the chat history.
#[derive(Debug, Clone)]
pub enum ChatEntry {
    /// A message submitted by the user.
    User {
        text: String,
        started_at: Option<std::time::Instant>,
        duration_ms: Option<u64>,
    },
    /// Reasoning / "thinking" output from the model, shown in lower contrast so
    /// the user can see it is thinking without it dominating the answer.
    Thinking {
        content: String,
        /// True while reasoning tokens are still arriving.
        streaming: bool,
    },
    /// A response from the LLM, potentially still streaming.
    Assistant {
        content: String,
        /// True while tokens are still arriving.
        streaming: bool,
    },
    /// An ahma tool call issued by the LLM (MCP bridge).
    ToolCall {
        id: String,
        name: String,
        args: String,
        /// `None` while the call is in flight; `Some(result)` when done.
        result: Option<String>,
        failed: bool,
    },
}

/// Ring buffer of chat history entries (capped at `CHAT_HISTORY_CAP`).
#[derive(Debug, Default)]
pub struct ChatHistory {
    entries: VecDeque<ChatEntry>,
}

impl ChatHistory {
    pub fn push(&mut self, entry: ChatEntry) {
        if self.entries.len() >= CHAT_HISTORY_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn entries(&self) -> &VecDeque<ChatEntry> {
        &self.entries
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drop completed entries, keeping only work still in progress: assistant
    /// output that is still streaming and tool calls awaiting a result. Used by
    /// `/clear` so an ongoing turn is not yanked off the screen mid-flight.
    pub fn retain_in_flight(&mut self) {
        self.entries.retain(|entry| {
            matches!(
                entry,
                ChatEntry::Assistant {
                    streaming: true,
                    ..
                } | ChatEntry::ToolCall { result: None, .. }
            )
        });
    }

    /// Append a token to the last `Assistant` entry if it's still streaming.
    /// If no such entry exists, creates a new one.
    pub fn append_token(&mut self, token: &str) {
        if let Some(ChatEntry::Assistant {
            content,
            streaming: true,
        }) = self.entries.back_mut()
        {
            content.push_str(token);
        } else {
            self.push(ChatEntry::Assistant {
                content: token.to_string(),
                streaming: true,
            });
        }
    }

    /// Append a reasoning/"thinking" token to the last streaming `Thinking`
    /// entry, creating one if the latest entry isn't an in-flight thinking block.
    pub fn append_thinking(&mut self, token: &str) {
        if let Some(ChatEntry::Thinking {
            content,
            streaming: true,
        }) = self.entries.back_mut()
        {
            content.push_str(token);
        } else {
            self.push(ChatEntry::Thinking {
                content: token.to_string(),
                streaming: true,
            });
        }
    }

    /// Mark the last assistant entry (and any still-streaming thinking block) as
    /// no longer streaming, so their live cursors/glyphs collapse.
    pub fn finish_stream(&mut self) {
        if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.back_mut() {
            *streaming = false;
        }
        for entry in self.entries.iter_mut() {
            if let ChatEntry::Thinking { streaming, .. } = entry {
                *streaming = false;
            }
        }
    }

    /// Locate the latest `User` entry and set its `duration_ms` based on `started_at` elapsed time.
    pub fn finish_user_timing(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::User {
                started_at,
                duration_ms,
                ..
            } = entry
            {
                if duration_ms.is_none()
                    && let Some(start) = started_at
                {
                    *duration_ms = Some(start.elapsed().as_millis() as u64);
                }
                break;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Compaction: removes ToolCall entries from old history to save tokens.
    /// `keep_latest` specifies how many of the most recent entries are preserved intact.
    pub fn compact(&mut self, keep_latest: usize) {
        let len = self.entries.len();
        if len <= keep_latest {
            return;
        }
        let cutoff = len - keep_latest;

        let mut new_entries = VecDeque::with_capacity(len);
        for (i, entry) in self.entries.drain(..).enumerate() {
            if i >= cutoff {
                new_entries.push_back(entry);
            } else {
                match entry {
                    ChatEntry::ToolCall { .. } => {
                        // Drop old tool calls to save context window space
                    }
                    other => new_entries.push_back(other),
                }
            }
        }
        self.entries = new_entries;
    }

    pub fn start_tool_call(&mut self, id: String, name: String, args: String) {
        self.push(ChatEntry::ToolCall {
            id,
            name,
            args,
            result: None,
            failed: false,
        });
    }

    pub fn finish_tool_call(&mut self, id: &str, result: String, failed: bool) {
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::ToolCall {
                id: entry_id,
                result: entry_result,
                failed: entry_failed,
                ..
            } = entry
                && entry_id == id
            {
                *entry_result = Some(result);
                *entry_failed = failed;
                break;
            }
        }
    }
}

// ─── Command navigator ────────────────────────────────────────────────────────

/// A command available in the `/` navigator.
#[derive(Debug, Clone)]
pub struct NavCommand {
    /// The full command string, e.g. `/help`.
    pub command: String,
    /// Short description shown in the picker.
    pub description: &'static str,
}

/// The full set of built-in `/` commands.
pub fn builtin_commands() -> Vec<NavCommand> {
    vec![
        NavCommand {
            command: "/help".into(),
            description: "show keyboard reference",
        },
        NavCommand {
            command: "/?".into(),
            description: "show keyboard reference (alias)",
        },
        NavCommand {
            command: "/mode chat".into(),
            description: "switch to chat interface",
        },
        NavCommand {
            command: "/mode monitor".into(),
            description: "switch to monitor dashboard",
        },
        NavCommand {
            command: "/provider".into(),
            description: "select LLM provider",
        },
        NavCommand {
            command: "/model".into(),
            description: "select model for current provider",
        },
        NavCommand {
            command: "/minimize on".into(),
            description: "enable token minimization (concise prompts, compressed output)",
        },
        NavCommand {
            command: "/minimize off".into(),
            description: "disable token minimization (default)",
        },
        NavCommand {
            command: "/mcp on".into(),
            description: "enable ahma as MCP tool server",
        },
        NavCommand {
            command: "/mcp off".into(),
            description: "disable ahma MCP tool server",
        },
        NavCommand {
            command: "/mcp list".into(),
            description: "list configured MCP client servers",
        },
        NavCommand {
            command: "/mcp refresh".into(),
            description: "refresh tools from configured MCP servers",
        },
        NavCommand {
            command: "/mcp add http <url> [name]".into(),
            description: "add an HTTP MCP server",
        },
        NavCommand {
            command: "/mcp add stdio <cmd> [args] [--name <n>]".into(),
            description: "add a stdio MCP server",
        },
        NavCommand {
            command: "/mcp remove <name>".into(),
            description: "remove a configured MCP server",
        },
        NavCommand {
            command: "/run <tool> {json}".into(),
            description: "invoke an ahma tool directly with optional JSON args",
        },
        NavCommand {
            command: "/tools".into(),
            description: "list available ahma tools",
        },
        NavCommand {
            command: "/operations".into(),
            description: "jump to operations panel",
        },
        NavCommand {
            command: "/logs".into(),
            description: "jump to log panel",
        },
        NavCommand {
            command: "/approve".into(),
            description: "approve pending gate",
        },
        NavCommand {
            command: "/reject".into(),
            description: "reject pending gate",
        },
        NavCommand {
            command: "/clear".into(),
            description: "clear chat & finished windows (logs kept)",
        },
        NavCommand {
            command: "/agent list".into(),
            description: "list saved agent profiles",
        },
        NavCommand {
            command: "/agent save <name>".into(),
            description: "save current setup as an agent profile",
        },
        NavCommand {
            command: "/agent load <name>".into(),
            description: "load an agent profile",
        },
        NavCommand {
            command: "/agent delete <name>".into(),
            description: "delete an agent profile",
        },
        NavCommand {
            command: "/export markdown".into(),
            description: "export chat transcript to markdown",
        },
        NavCommand {
            command: "/settings".into(),
            description: "open settings panel (edit & persist)",
        },
        NavCommand {
            command: "/quit".into(),
            description: "quit the application",
        },
        // /exit intentionally omitted — still handled, just not advertised
    ]
}

/// State for the `/` command navigator overlay.
#[derive(Debug, Clone, Default)]
pub struct CommandNavigator {
    /// Text typed after `/`.
    pub input: String,
    /// Filtered list of matching commands.
    pub completions: Vec<NavCommand>,
    /// Index of the highlighted completion.
    pub selected: usize,
}

impl CommandNavigator {
    /// Build a freshly-opened navigator with completions seeded from `tools`.
    /// Visibility is owned by [`ModalState`], not this struct (SPEC R23).
    pub fn opened(tools: &[String]) -> Self {
        let mut nav = CommandNavigator::default();
        nav.refresh_completions(tools);
        nav
    }

    /// Rebuild completions from builtins + dynamic `/run <tool>` entries.
    pub fn refresh_completions(&mut self, tools: &[String]) {
        let mut cmds = builtin_commands();
        // Add a `/run <tool>` entry for every known ahma tool.
        for t in tools {
            cmds.push(NavCommand {
                command: format!("/run {t}"),
                description: "run ahma tool",
            });
        }

        if self.input.is_empty() {
            self.completions = cmds;
        } else {
            let q = self.input.to_lowercase();
            let q_clean = q.trim_start_matches('/');
            self.completions = cmds
                .into_iter()
                .filter(|c| {
                    c.command.to_lowercase().contains(&q)
                        || c.description.to_lowercase().contains(&q)
                        || (c.command == "/quit"
                            && !q_clean.is_empty()
                            && "exit".starts_with(q_clean))
                })
                .collect();
        }
        self.selected = self.selected.min(self.completions.len().saturating_sub(1));
    }

    pub fn select_next(&mut self) {
        let n = self.completions.len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
        }
    }

    pub fn select_prev(&mut self) {
        let n = self.completions.len();
        if n > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(n - 1);
        }
    }

    /// Apply the selected completion to the input field (TAB).
    pub fn tab_complete(&mut self) {
        if let Some(cmd) = self.completions.get(self.selected) {
            self.input = cmd.command.trim_start_matches('/').to_string();
        }
    }

    /// The command string of the currently selected entry, or the raw input.
    pub fn selected_command(&self) -> String {
        self.completions
            .get(self.selected)
            .map(|c| c.command.clone())
            .unwrap_or_else(|| format!("/{}", self.input))
    }
}

// ─── Inline picker ────────────────────────────────────────────────────────────

/// Generic inline picker (used for provider and model selection).
#[derive(Debug, Clone)]
pub struct PickerState {
    pub title: String,
    pub items: Vec<String>,
    pub selected: usize,
    pub filter: String,
}

impl PickerState {
    pub fn new(title: impl Into<String>, items: Vec<String>) -> Self {
        Self {
            title: title.into(),
            items,
            selected: 0,
            filter: String::new(),
        }
    }

    pub fn filtered_items(&self) -> Vec<&str> {
        if self.filter.is_empty() {
            self.items.iter().map(|s| s.as_str()).collect()
        } else {
            let q = self.filter.to_lowercase();
            self.items
                .iter()
                .filter(|s| s.to_lowercase().contains(&q))
                .map(|s| s.as_str())
                .collect()
        }
    }

    pub fn selected_item(&self) -> Option<&str> {
        self.filtered_items().get(self.selected).copied()
    }

    pub fn select_next(&mut self) {
        let n = self.filtered_items().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
        }
    }

    pub fn select_prev(&mut self) {
        let n = self.filtered_items().len();
        if n > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(n - 1);
        }
    }

    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
    }

    pub fn filter_pop(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    pub fn select_exact(&mut self, item: &str) {
        if let Some(index) = self
            .filtered_items()
            .iter()
            .position(|candidate| *candidate == item)
        {
            self.selected = index;
        }
    }

    /// Select the first item that starts with `prefix`. Useful when display rows
    /// carry trailing annotations (e.g. ` · num_ctx …`) the caller can't
    /// reproduce verbatim.
    pub fn select_prefix(&mut self, prefix: &str) {
        if let Some(index) = self
            .filtered_items()
            .iter()
            .position(|candidate| candidate.starts_with(prefix))
        {
            self.selected = index;
        }
    }
}

// ─── Focus ────────────────────────────────────────────────────────────────────

/// Which panel currently receives keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Chat input box (default in Chat mode).
    #[default]
    Chat,
    AiActivity,
    OpsDag,
    Log,
    Palette,
}

impl Focus {
    /// Cycle through monitor panels (skips Chat — return there with Esc).
    pub fn cycle_next(self) -> Self {
        match self {
            Self::Chat => Self::OpsDag,
            Self::AiActivity => Self::OpsDag,
            Self::OpsDag => Self::Log,
            Self::Log => Self::Chat,
            Self::Palette => Self::Chat,
        }
    }

    pub fn cycle_prev(self) -> Self {
        match self {
            Self::Chat => Self::Log,
            Self::AiActivity => Self::Chat,
            Self::OpsDag => Self::Chat,
            Self::Log => Self::OpsDag,
            Self::Palette => Self::Chat,
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
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

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
    pub started_time: chrono::DateTime<chrono::Local>,
    pub description: String,
    pub cwd: Option<String>,
    pub args: Vec<String>,
    /// ID of the parent operation this one is waiting for (for DAG rendering).
    pub parent_id: Option<String>,
    /// Tail of stdout for the detail pane.
    pub stdout_tail: VecDeque<String>,
    pub alerts: Vec<String>,
    pub pid: Option<u32>,
    pub pinned: bool,
    /// UUID of the ahma instance this operation belongs to (set for daemon-sourced ops).
    pub instance_id: Option<String>,
    /// Short human-readable label for the owning instance (e.g. `"VS Code"`).
    pub instance_label: Option<String>,
    pub completed_at: Option<Instant>,
    pub result_summary: Option<String>,
    pub duration_ms: Option<u64>,
    pub scope: Option<String>,
}

fn try_parse_run_terminal_command(description: &str, id: &str) -> Option<String> {
    let get_cmd_json = || -> Option<String> {
        let start_idx = description.find('{')?;
        let end_idx = description.rfind('}')?;
        if start_idx >= end_idx {
            return None;
        }
        let val: serde_json::Value =
            serde_json::from_str(&description[start_idx..=end_idx]).ok()?;
        val.get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    if let Some(cmd) = get_cmd_json() {
        return Some(cmd);
    }

    if description.starts_with("Execute ")
        && let Some(in_idx) = description.rfind(" in ")
    {
        let cmd = &description["Execute ".len()..in_idx];
        if !cmd.is_empty() {
            return Some(cmd.to_string());
        }
    }

    if id.starts_with("op_") {
        let parts: Vec<&str> = id.split('_').collect();
        if parts.len() >= 3 && parts[1].chars().all(|c| c.is_ascii_digit()) {
            return Some(parts[2..].join(" ").replace('_', " "));
        }
    }

    None
}

impl Operation {
    pub fn new(id: impl Into<String>, tool_name: impl Into<String>, status: OpStatus) -> Self {
        Self {
            id: id.into(),
            tool_name: tool_name.into(),
            status,
            started_at: Some(Instant::now()),
            started_time: chrono::Local::now(),
            description: String::new(),
            cwd: None,
            args: vec![],
            parent_id: None,
            stdout_tail: VecDeque::with_capacity(STDOUT_TAIL_CAP),
            alerts: vec![],
            pid: None,
            pinned: false,
            instance_id: None,
            instance_label: None,
            completed_at: None,
            result_summary: None,
            duration_ms: None,
            scope: None,
        }
    }

    pub fn elapsed_display(&self) -> String {
        if let Some(ms) = self.duration_ms {
            if ms < 1000 {
                return format!("{ms}ms");
            } else {
                return format!("{}s", ms / 1000);
            }
        }
        if let (Some(start), Some(end)) = (self.started_at, self.completed_at) {
            let d = if end >= start {
                end.duration_since(start)
            } else {
                std::time::Duration::ZERO
            };
            let ms = d.as_millis();
            if ms < 1000 {
                format!("{ms}ms")
            } else {
                format!("{}s", d.as_secs())
            }
        } else {
            let secs = self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            format!("{secs}s")
        }
    }

    pub fn display_name(&self) -> String {
        if self.tool_name == "run_terminal_command"
            && let Some(cmd) = try_parse_run_terminal_command(&self.description, &self.id)
        {
            return cmd;
        }
        self.tool_name.clone()
    }

    pub fn clean_id(&self) -> String {
        if self.id.starts_with("op_") || self.id.starts_with("op-") {
            let sep = if self.id.starts_with("op_") { '_' } else { '-' };
            let parts: Vec<&str> = self.id.split(sep).collect();
            if parts.len() >= 2 && parts[1].chars().all(|c| c.is_ascii_digit()) {
                return format!("op{}{}", sep, parts[1]);
            }
        }
        self.id[..self.id.len().min(6)].to_string()
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

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct LogFileInfo {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified: Option<String>,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub is_approved: bool,
}

// ─── Approval gate ────────────────────────────────────────────────────────────

/// A pending gate that requires user approval before an operation continues.
#[derive(Debug, Clone)]
pub struct ApprovalGate {
    pub op_id: String,
    /// Raw tool name (e.g. `list_dir`), used when persisting an "always allow".
    pub tool: String,
    pub description: String,
    /// Short scope hint (e.g. "new workspace …") shown dim when the prompt
    /// appears in an unfamiliar workspace; `None` when no context is warranted.
    pub note: Option<String>,
    pub deadline: Option<Instant>,
    pub diff: Option<String>,
}

impl ApprovalGate {
    /// Create a gate for `op_id`/`tool` with a human-readable `description`.
    ///
    /// The optional context (note, deadline, diff) starts empty; attach it with
    /// the `with_*` builders. Accepting `impl Into<String>` lets callers pass
    /// `&str` literals or owned `String`s without sprinkling `.to_string()`.
    pub fn new(
        op_id: impl Into<String>,
        tool: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            op_id: op_id.into(),
            tool: tool.into(),
            description: description.into(),
            note: None,
            deadline: None,
            diff: None,
        }
    }

    /// Attach a short scope-hint note (shown dim in unfamiliar workspaces).
    #[must_use]
    pub fn with_note(mut self, note: Option<String>) -> Self {
        self.note = note;
        self
    }

    /// Attach a deadline after which the gate auto-expires.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    /// Attach a diff preview to display alongside the prompt.
    #[must_use]
    pub fn with_diff(mut self, diff: Option<String>) -> Self {
        self.diff = diff;
        self
    }

    pub fn remaining_secs(&self) -> Option<u64> {
        self.deadline.map(|d| {
            let now = Instant::now();
            if d > now { (d - now).as_secs() } else { 0 }
        })
    }
}

// ─── Scope-grant gate ─────────────────────────────────────────────────────────

/// A pending "grant access to X?" prompt raised when a sandboxed command was
/// blocked by an out-of-scope path (SPEC R5.4.7). Three-valued: the default/Enter
/// choice is the safe Deny — widening (`y`=read+write, `r`=read-only) requires an
/// explicit non-default key (R5.3.1). Lives in its own `AppState.scope_grant`
/// field, parallel to `approval`, because it is daemon-raised and may coexist with
/// an open user overlay (it is rendered as an overlay, not a `ModalState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeGrantGate {
    pub decision_id: String,
    /// The literal canonical path the prompt is about (never abbreviated).
    pub path: String,
    /// The access the detector inferred is needed.
    pub access: ahma_common::config::ScopeAccess,
    /// Whether the path was found up front or via a stderr denial heuristic.
    pub reason: ahma_common::scope_grant::GrantReason,
    /// The tool/command that tripped the scope, if known.
    pub tool: Option<String>,
}

impl ScopeGrantGate {
    /// Build a gate from a hub [`ahma_common::scope_grant::ScopeGrantRequest`].
    pub fn from_request(request: ahma_common::scope_grant::ScopeGrantRequest) -> Self {
        Self {
            decision_id: request.decision_id,
            path: request.path.display().to_string(),
            access: request.access,
            reason: request.reason,
            tool: request.tool,
        }
    }
}

/// A pending web-egress approval prompt (SPEC R-WEB.6), the network-egress parallel
/// of [`ScopeGrantGate`]. Rendered as a top overlay; the default/Enter choice is the
/// safe Deny.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebApprovalGate {
    pub decision_id: String,
    /// The host the prompt is about (e.g. `api.github.com`).
    pub domain: String,
    /// The full URL that triggered the prompt, shown for context.
    pub url: String,
    /// The tool that requested egress, if known.
    pub tool: Option<String>,
}

impl WebApprovalGate {
    /// Build a gate from a hub [`ahma_common::web_approval::WebApprovalRequest`].
    pub fn from_request(request: ahma_common::web_approval::WebApprovalRequest) -> Self {
        Self {
            decision_id: request.decision_id,
            domain: request.domain,
            url: request.url,
            tool: request.tool,
        }
    }
}

// ─── Click target ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickTarget {
    CancelOperation(String),
    PinOperation(String),
    AnalyzeOperation(String),
    SelectOperation(usize),
    CloseWindow(usize),
    ToggleWindow(usize),
    /// A row of the monitor task tree: click selects it and toggles it
    /// (accordion expand for ops, collapse for instance/session headers).
    TreeRow(usize),
}

// ─── Command palette ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    pub input: String,
    pub completions: Vec<String>,
    pub selected_completion: usize,
    /// When set, user must type `y` to confirm before the command is dispatched.
    pub confirm_prompt: Option<String>,
}

impl PaletteState {
    /// Reset the palette's transient fields when (re)opening. Visibility is
    /// owned by [`ModalState`], not this struct (SPEC R23).
    pub fn reset(&mut self) {
        self.input.clear();
        self.completions.clear();
        self.selected_completion = 0;
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

// ─── Modal overlays ───────────────────────────────────────────────────────────

/// The single active overlay/modal (SPEC R23).
///
/// At most one user overlay is open at a time, so previously-representable
/// invalid combinations — e.g. the help screen *and* the command navigator *and*
/// the log-file switcher all "open" at once — are now unrepresentable. Each
/// variant owns the data its overlay needs, giving one source of truth for both
/// "which overlay is open" and its contents.
///
/// The bridge-driven approval gate is intentionally *not* a variant here: it is
/// raised asynchronously by the daemon and may legitimately be pending while a
/// user overlay is open, so it lives in its own guarded field
/// ([`AppState::request_approval`]).
#[derive(Debug, Default)]
pub enum ModalState {
    /// No overlay is open.
    #[default]
    None,
    /// The help screen.
    Help,
    /// The `/` command navigator.
    Navigator(CommandNavigator),
    /// The command palette / confirm prompt.
    Palette(PaletteState),
    /// The inline provider picker.
    ProviderPicker(PickerState),
    /// The inline model picker.
    ModelPicker(PickerState),
    /// The log-file switcher, with the highlighted row index.
    LogFiles { selected: usize },
}

impl ModalState {
    /// True when some overlay is open.
    pub fn is_open(&self) -> bool {
        !matches!(self, ModalState::None)
    }
}

// ─── Application state ────────────────────────────────────────────────────────

/// A window representing a running or finished CLI command or LLM call.
/// Display status of a [`TuiWindow`] (a CLI-command or LLM-call card).
///
/// Typed rather than a free-form string so comparisons and styling are
/// exhaustively checked by the compiler (SPEC R23: states are not modeled as
/// strings). Derived from an operation's [`OpStatus`] via `window_status_for`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowStatus {
    Pending,
    Running,
    Finished,
    Cancelled,
    Error,
}

impl WindowStatus {
    /// The human-readable label shown in window titles and headers.
    pub fn label(self) -> &'static str {
        match self {
            WindowStatus::Pending => "Pending",
            WindowStatus::Running => "Running",
            WindowStatus::Finished => "Finished",
            WindowStatus::Cancelled => "Cancelled",
            WindowStatus::Error => "Error",
        }
    }

    /// True once the window has reached a final state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WindowStatus::Finished | WindowStatus::Cancelled | WindowStatus::Error
        )
    }
}

impl std::fmt::Display for WindowStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone)]
pub struct TuiWindow {
    pub id: usize,
    pub label: String,
    pub status: WindowStatus,
    pub content: Vec<String>,
    pub collapsed: bool,
    pub finished_at: Option<std::time::Instant>,
    pub is_cli: bool,
    pub command: String,
    pub working_dir: String,
    pub llm_model: Option<String>,
    pub visible: bool,
    pub abort_tx: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    pub op_id: Option<String>,
}

/// Seed the liveness xorshift generator from the wall clock, forced non-zero
/// (xorshift64 is stuck at zero). The exact value is irrelevant — it only needs
/// to differ run-to-run so the random light pattern is not identical each launch.
fn liveness_initial_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos | 1
}

/// Map an xorshift state to a "liveness light" glyph. In unicode mode this is a
/// random non-blank cell from the Braille Patterns block (U+2801..=U+28FF) so it
/// reads as a shifting cluster of dots; otherwise a rotating ASCII spinner char.
fn liveness_char(state: u64, unicode: bool) -> char {
    if unicode {
        // 0x2800 is the all-dots-off (blank) cell; skip it so the glyph is always visible.
        let offset = (state % 255) as u32 + 1;
        char::from_u32(0x2800 + offset).unwrap_or('⠿')
    } else {
        const ASCII: [char; 4] = ['|', '/', '-', '\\'];
        ASCII[(state % ASCII.len() as u64) as usize]
    }
}

/// Top-level application state — owns all panel data and UI mode.
pub struct AppState {
    pub windows: Vec<TuiWindow>,
    pub next_window_id: usize,
    /// Set by `/clear`. Operations that had already completed before this
    /// instant are not re-materialised as windows when a source re-pushes them,
    /// so cleared results stay cleared.
    pub cleared_at: Option<Instant>,
    #[cfg(feature = "tui")]
    pub window_rects: std::cell::RefCell<Vec<(usize, Rect)>>,
    #[cfg(not(feature = "tui"))]
    pub window_rects: std::cell::RefCell<Vec<(usize, ())>>,

    // ── Connection ──
    pub server_url: String,
    pub mcp_http_base_url: String,
    pub transport_label: String,
    pub server_healthy: bool,
    pub daemon_healthy: bool,
    pub session_id: Option<String>,
    pub sandbox_status: String,
    pub workspace: String,
    /// Token/context preferences resolved from CLI flags (`--minimize-tokens`,
    /// `--small-model-harness`, `--context-length`).  Flag values override
    /// settings.toml and the deprecated env vars.
    pub token_prefs: crate::TokenPrefs,
    /// Effective token-minimization state for display and the `/minimize` switch.
    /// Resolved once at startup (flag > env > settings) and kept in sync by the
    /// `/minimize on|off` command, which also persists `settings.tools`.
    pub minimize_tokens: bool,
    /// Prompt (input) token count of the **most recent** model turn. Unlike the
    /// cumulative `token_usage`, this is the exact current context fill, used for
    /// the status-bar context-window %. Zero until a turn reports usage.
    pub last_prompt_tokens: u32,

    // ── Panel data ──
    pub ai_activity: VecDeque<AiActivityEntry>,
    pub operations: Vec<Operation>,
    /// Live output lines that arrived (via the daemon hub) before the operation
    /// they belong to was materialised in `operations`. Keyed by op id. The
    /// daemon hub delivers `OpStarted` and `OpOutput` over one ordered socket,
    /// but on a hub reconnect only `OpStarted`/`OpFinished` are replayed — so an
    /// output line can outrun its operation. Rather than drop it (which left fast
    /// commands like `!pwd` showing no result until the next status poll), we
    /// stash it here and flush into the op's `stdout_tail` in `upsert_operation`.
    pub pending_output: HashMap<String, VecDeque<String>>,
    pub log: VecDeque<LogEntry>,
    pub approval: Option<ApprovalGate>,
    /// Pending scope-grant prompt, if any (parallel to `approval`).
    pub scope_grant: Option<ScopeGrantGate>,
    /// Pending web-egress approval prompt, if any (parallel to `scope_grant`).
    pub web_approval: Option<WebApprovalGate>,
    pub tools_list: Vec<crate::mcp_connections::ToolInfo>,
    pub mcp_connections: McpConnectionManager,

    // ── Chat ──
    pub mode: Mode,
    pub chat: ChatHistory,
    /// Current text in the multi-line input box.
    #[cfg(feature = "tui")]
    pub chat_input: TextArea<'static>,
    #[cfg(not(feature = "tui"))]
    pub chat_input: (),
    /// LLM provider label shown in header (e.g. "Ollama / llama3.2").
    pub llm_label: String,
    /// Concrete provider URL used for API calls and persisted in session config.
    pub current_provider_url: Option<String>,
    pub active_profile: Option<String>,
    /// True when ahma-as-MCP is active.
    pub mcp_enabled: bool,
    /// Discovered + configured providers.
    pub available_providers: Vec<ahma_llm_monitor::LocalProvider>,
    /// Discovered providers (from probing/local discovery)
    pub discovered_providers: Vec<ahma_llm_monitor::LocalProvider>,
    /// Active instances registered with the daemon
    pub active_instances: Vec<ahma_common::daemon_hub::InstanceInfo>,
    /// Available models for the current provider.
    pub available_models: Vec<String>,
    /// Chat scroll offset (lines from bottom = 0 is newest).
    pub chat_scroll: usize,
    /// The single active overlay/modal (navigator, palette, pickers, help,
    /// log-file switcher). Replaces the former independent bool/Option flags so
    /// two overlays can never be open at once (SPEC R23).
    pub modal: ModalState,

    // ── UI state ──
    pub focus: Focus,
    pub ops_selected: usize,
    pub ops_scroll: std::cell::Cell<usize>,

    // ── Task tree (monitor view) ──
    /// Operation id whose output tail is expanded inline. Accordion: setting a
    /// new id implicitly collapses the previous one (SPEC R24).
    pub expanded_op: Option<String>,
    /// Collapse keys (`inst:<id>`, `grp:<gid>:<key>`) the user has folded.
    pub collapsed_nodes: std::collections::HashSet<String>,
    /// Show instances from every project, not just the one the TUI started in.
    pub show_all_projects: bool,
    /// Directory the TUI was started in; drives the project filter.
    pub project_root: Option<String>,
    /// Rows rendered by the last frame of the task tree; rebuilt on every draw
    /// and read by key/mouse handlers to resolve the selection.
    pub task_rows: std::cell::RefCell<Vec<crate::task_tree::TreeRow>>,
    /// Armed at startup: the first hub replay that shows live project work
    /// switches to monitor mode so the ongoing tasks are immediately visible.
    /// Disarmed by any user input.
    pub auto_view_pending: bool,
    #[cfg(feature = "tui")]
    pub chat_area: std::cell::Cell<Rect>,
    #[cfg(not(feature = "tui"))]
    pub chat_area: std::cell::Cell<()>,
    #[cfg(feature = "tui")]
    pub log_area: std::cell::Cell<Rect>,
    #[cfg(not(feature = "tui"))]
    pub log_area: std::cell::Cell<()>,
    #[cfg(feature = "tui")]
    pub ops_area: std::cell::Cell<Rect>,
    #[cfg(not(feature = "tui"))]
    pub ops_area: std::cell::Cell<()>,
    #[cfg(feature = "tui")]
    pub detail_area: std::cell::Cell<Rect>,
    #[cfg(not(feature = "tui"))]
    pub detail_area: std::cell::Cell<()>,
    #[cfg(feature = "tui")]
    pub chat_input_area: std::cell::Cell<Rect>,
    #[cfg(not(feature = "tui"))]
    pub chat_input_area: std::cell::Cell<()>,
    #[cfg(feature = "tui")]
    pub last_mouse_pos: std::cell::Cell<Option<(u16, u16)>>,
    #[cfg(not(feature = "tui"))]
    pub last_mouse_pos: std::cell::Cell<()>,

    // --- Animation states ---
    pub chat_scroll_target: std::cell::Cell<f64>,
    pub chat_scroll_current: std::cell::Cell<f64>,
    pub log_scroll_target: std::cell::Cell<f64>,
    pub log_scroll_current: std::cell::Cell<f64>,
    pub chat_input_height_target: std::cell::Cell<f64>,
    pub chat_input_height_current: std::cell::Cell<f64>,
    pub chat_max_scroll: std::cell::Cell<usize>,
    pub log_max_scroll: std::cell::Cell<usize>,
    pub activity_scroll: usize,
    /// Tracked token usage for the current session.
    pub token_usage: ahma_llm_monitor::client::TokenUsage,

    // --- Monitor mode state ---
    pub log_scroll: usize,
    /// Tail-follow: when true the log pane stays pinned to the newest line and
    /// tracks new output as it arrives (the default when the monitor opens).
    /// Scrolling up detaches follow; scrolling back to the bottom re-engages it.
    pub log_follow: bool,
    pub log_filter: String,
    pub log_filter_active: bool,
    pub log_files: Vec<LogFileInfo>,
    pub active_log_file: Option<String>,
    pub active_log_lines: Vec<String>,
    pub log_wrap_enabled: bool,
    pub log_zoom_enabled: bool,
    #[cfg(feature = "tui")]
    pub click_targets: std::cell::RefCell<Vec<(ClickTarget, Rect)>>,
    #[cfg(not(feature = "tui"))]
    pub click_targets: std::cell::RefCell<Vec<(ClickTarget, ())>>,
    #[cfg(feature = "tui")]
    pub ops_list_state: std::cell::RefCell<ratatui::widgets::ListState>,
    #[cfg(not(feature = "tui"))]
    pub ops_list_state: std::cell::RefCell<()>,

    // ── Settings editor ──
    pub settings_editor: crate::settings_editor::SettingsEditor,

    // ── Liveness indicator ──
    /// Two-cell Braille "liveness" glyph shown in front of the streaming `ahma`
    /// response line (e.g. `⢷⡪ ahma`). Re-randomised *fast* by
    /// [`AppState::mark_stream_activity`] on every token/thinking/tool/usage
    /// signal (proving results are actively coming back), and *slowly* by
    /// [`AppState::tick_waiting_spinner`] while the request is in flight but
    /// quiet (proving it is still alive — vs frozen/timed-out). Two spaces when
    /// the turn is complete.
    pub liveness_glyph: String,
    pub liveness_state: LivenessState,
    /// xorshift64 state driving the random liveness glyph. Never zero.
    pub liveness_seed: u64,
    /// When the last visible stream signal (token/thinking/tool/usage) arrived.
    /// Drives the fast-vs-slow spinner cadence. `None` until the first signal.
    pub last_stream_activity: Option<std::time::Instant>,
    /// When the slow "still waiting" spinner last advanced.
    pub last_wait_tick: Option<std::time::Instant>,

    // ── Config ──
    pub unicode: bool,
    pub should_quit: bool,
    /// Sender half of the bridge channel; set by app.rs after spawning.
    #[cfg(feature = "tui")]
    pub bridge_tx: Option<tokio::sync::mpsc::Sender<crate::llm_bridge::BridgeEvent>>,
    #[cfg(not(feature = "tui"))]
    pub bridge_tx: Option<()>,
    #[cfg(feature = "tui")]
    pub mcp_source_tx: Option<tokio::sync::mpsc::Sender<crate::mcp_source::McpSourceCommand>>,
    #[cfg(not(feature = "tui"))]
    pub mcp_source_tx: Option<()>,
    #[cfg(feature = "tui")]
    pub approval_tx: Option<tokio::sync::oneshot::Sender<bool>>,
    #[cfg(not(feature = "tui"))]
    pub approval_tx: Option<()>,
}

#[cfg(feature = "tui")]
impl AppState {
    /// Raise an approval gate — the single guarded entry into the
    /// approval-pending state.
    ///
    /// If an approval is already pending it is auto-rejected first so its
    /// `oneshot` waiter is always resolved exactly once and never silently
    /// dropped. Previously a second `RequestApproval` overwrote `approval`
    /// while leaking the prior `approval_tx`, leaving the earlier caller's
    /// receiver dangling (SPEC R23: the pending gate and its sender are one
    /// state, mutated only through this method).
    pub fn request_approval(
        &mut self,
        gate: ApprovalGate,
        tx: Option<tokio::sync::oneshot::Sender<bool>>,
    ) {
        if self.approval.take().is_some()
            && let Some(old_tx) = self.approval_tx.take()
        {
            // Auto-reject the superseded gate so the waiter is not left hanging.
            let _ = old_tx.send(false);
        }
        self.approval = Some(gate);
        self.approval_tx = tx;
    }
}

impl AppState {
    // ── Modal accessors ──
    //
    // `self.modal` is the single source of truth for which overlay is open
    // (SPEC R23). These helpers project it back to the per-overlay views the
    // rest of the code uses, so no caller mutates the discriminant directly.

    /// True when any user overlay is open.
    pub fn modal_open(&self) -> bool {
        self.modal.is_open()
    }

    /// Re-randomise the two-cell liveness glyph from the xorshift generator.
    /// Low-level; prefer [`Self::mark_stream_activity`] (fast pulse on real
    /// output) or [`Self::tick_waiting_spinner`] (slow pulse while waiting).
    pub fn bump_liveness(&mut self) {
        // xorshift64 — cheap, dependency-free, and seeded away from zero. Draw
        // two cells so the indicator is wider and its motion is easier to see.
        let mut x = self.liveness_seed;
        let mut glyph = String::with_capacity(8);
        for _ in 0..2 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            glyph.push(liveness_char(x, self.unicode));
        }
        self.liveness_seed = x;
        self.liveness_glyph = glyph;
    }

    /// Record real server output (token/thinking/tool/usage) and pulse the
    /// spinner *fast*. The rapid, per-token change is the reliable "results are
    /// actively coming back" signal.
    pub fn mark_stream_activity(&mut self, state: LivenessState) {
        self.liveness_state = state;
        self.bump_liveness();
        self.last_stream_activity = Some(std::time::Instant::now());
    }

    /// Advance the spinner on a slow cadence while a request is in flight but
    /// quiet (no recent output) — the "still alive, just waiting" signal that is
    /// visibly distinct from the fast streaming pulse. Call once per animation
    /// tick *only while a turn is in progress*. Returns `true` if the glyph
    /// changed (so the caller can redraw).
    pub fn tick_waiting_spinner(&mut self) -> bool {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        // If output arrived very recently, token events are driving the fast
        // pulse — don't also tick here.
        let streaming_recently = self
            .last_stream_activity
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(350));
        if streaming_recently {
            return false;
        }
        let due = self
            .last_wait_tick
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(700));
        if due {
            self.bump_liveness();
            self.last_wait_tick = Some(now);
            true
        } else {
            false
        }
    }

    /// Clear the liveness glyph back to blank once the turn is complete and no
    /// further server updates are expected for the current response line.
    pub fn reset_liveness(&mut self) {
        self.liveness_glyph = "  ".to_string();
        self.liveness_state = LivenessState::Idle;
        self.last_stream_activity = None;
        self.last_wait_tick = None;
    }

    /// True when a text-entry overlay (navigator, palette, or an inline picker)
    /// is open. Used to decide whether a bare key should be consumed as overlay
    /// input rather than a global shortcut (e.g. approval `y`/`n`).
    pub fn text_entry_modal_open(&self) -> bool {
        matches!(
            self.modal,
            ModalState::Navigator(_)
                | ModalState::Palette(_)
                | ModalState::ProviderPicker(_)
                | ModalState::ModelPicker(_)
        )
    }

    /// Close whatever overlay is currently open.
    pub fn close_modal(&mut self) {
        self.modal = ModalState::None;
    }

    /// True when the help screen is open.
    pub fn is_help_open(&self) -> bool {
        matches!(self.modal, ModalState::Help)
    }

    /// Toggle the help screen, closing any other overlay first.
    pub fn toggle_help(&mut self) {
        self.modal = if self.is_help_open() {
            ModalState::None
        } else {
            ModalState::Help
        };
    }

    /// The command navigator, if it is open.
    pub fn navigator(&self) -> Option<&CommandNavigator> {
        match &self.modal {
            ModalState::Navigator(n) => Some(n),
            _ => None,
        }
    }

    /// The command navigator (mutable), if it is open.
    pub fn navigator_mut(&mut self) -> Option<&mut CommandNavigator> {
        match &mut self.modal {
            ModalState::Navigator(n) => Some(n),
            _ => None,
        }
    }

    /// The command palette, if it is open.
    pub fn palette(&self) -> Option<&PaletteState> {
        match &self.modal {
            ModalState::Palette(p) => Some(p),
            _ => None,
        }
    }

    /// The command palette (mutable), if it is open.
    pub fn palette_mut(&mut self) -> Option<&mut PaletteState> {
        match &mut self.modal {
            ModalState::Palette(p) => Some(p),
            _ => None,
        }
    }

    /// The inline provider picker, if it is open.
    pub fn provider_picker(&self) -> Option<&PickerState> {
        match &self.modal {
            ModalState::ProviderPicker(p) => Some(p),
            _ => None,
        }
    }

    /// The inline provider picker (mutable), if it is open.
    pub fn provider_picker_mut(&mut self) -> Option<&mut PickerState> {
        match &mut self.modal {
            ModalState::ProviderPicker(p) => Some(p),
            _ => None,
        }
    }

    /// Close the provider picker and return its state, if it was open.
    pub fn take_provider_picker(&mut self) -> Option<PickerState> {
        match std::mem::take(&mut self.modal) {
            ModalState::ProviderPicker(p) => Some(p),
            other => {
                self.modal = other;
                None
            }
        }
    }

    /// The inline model picker, if it is open.
    pub fn model_picker(&self) -> Option<&PickerState> {
        match &self.modal {
            ModalState::ModelPicker(p) => Some(p),
            _ => None,
        }
    }

    /// The inline model picker (mutable), if it is open.
    pub fn model_picker_mut(&mut self) -> Option<&mut PickerState> {
        match &mut self.modal {
            ModalState::ModelPicker(p) => Some(p),
            _ => None,
        }
    }

    /// Close the model picker and return its state, if it was open.
    pub fn take_model_picker(&mut self) -> Option<PickerState> {
        match std::mem::take(&mut self.modal) {
            ModalState::ModelPicker(p) => Some(p),
            other => {
                self.modal = other;
                None
            }
        }
    }

    /// The highlighted row of the log-file switcher, if it is open.
    pub fn log_files_selected(&self) -> Option<usize> {
        match self.modal {
            ModalState::LogFiles { selected } => Some(selected),
            _ => None,
        }
    }

    /// Open the log-file switcher at row `selected`.
    pub fn open_log_files_modal(&mut self, selected: usize) {
        self.modal = ModalState::LogFiles { selected };
    }

    /// Update the highlighted row of the log-file switcher (no-op if closed).
    pub fn set_log_files_selected(&mut self, selected: usize) {
        if let ModalState::LogFiles { selected: s } = &mut self.modal {
            *s = selected;
        }
    }

    pub fn new(
        server_url: impl Into<String>,
        transport_label: impl Into<String>,
        unicode: bool,
    ) -> Self {
        let workspace = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_default();

        // Try to load per-directory session config.
        let session = std::env::current_dir()
            .ok()
            .and_then(|cwd| TuiSessionConfig::load(&cwd).ok().flatten());
        let llm_label = session
            .as_ref()
            .map(|s| format!("{} / {}", s.provider, s.model))
            .unwrap_or_else(|| "no LLM".to_string());
        let current_provider_url = session.as_ref().and_then(|s| {
            s.provider_url.clone().or_else(|| {
                if s.provider.starts_with("http") {
                    Some(s.provider.clone())
                } else {
                    None
                }
            })
        });
        let mcp_enabled = session.as_ref().map(|s| s.mcp_enabled).unwrap_or(true);
        let mcp_connections = std::env::current_dir()
            .ok()
            .and_then(|cwd| McpConnectionManager::load(&cwd).ok())
            .unwrap_or_default();
        let active_profile = session.as_ref().and_then(|s| s.active_profile.clone());

        Self {
            server_url: server_url.into(),
            mcp_http_base_url: String::new(),
            transport_label: transport_label.into(),
            server_healthy: false,
            daemon_healthy: false,
            session_id: None,
            sandbox_status: "UNKNOWN".to_string(),
            workspace,
            token_prefs: crate::TokenPrefs::default(),
            minimize_tokens: false,
            last_prompt_tokens: 0,
            ai_activity: VecDeque::with_capacity(ACTIVITY_RING_CAP),
            operations: vec![],
            pending_output: HashMap::new(),
            log: VecDeque::with_capacity(LOG_RING_CAP),
            approval: None,
            scope_grant: None,
            web_approval: None,
            tools_list: vec![],
            mcp_connections,

            mode: Mode::default(),
            chat: ChatHistory::default(),
            chat_input: TextArea::default(),
            llm_label,
            current_provider_url,
            active_profile,
            mcp_enabled,
            available_providers: vec![],
            discovered_providers: vec![],
            active_instances: vec![],
            available_models: vec![],
            chat_scroll: 0,
            modal: ModalState::None,

            focus: Focus::default(),
            ops_selected: 0,
            ops_scroll: std::cell::Cell::new(0),
            expanded_op: None,
            collapsed_nodes: std::collections::HashSet::new(),
            show_all_projects: false,
            project_root: None,
            task_rows: std::cell::RefCell::new(Vec::new()),
            auto_view_pending: true,
            #[cfg(feature = "tui")]
            chat_area: std::cell::Cell::new(Rect::default()),
            #[cfg(not(feature = "tui"))]
            chat_area: std::cell::Cell::new(()),
            #[cfg(feature = "tui")]
            log_area: std::cell::Cell::new(Rect::default()),
            #[cfg(not(feature = "tui"))]
            log_area: std::cell::Cell::new(()),
            #[cfg(feature = "tui")]
            ops_area: std::cell::Cell::new(Rect::default()),
            #[cfg(not(feature = "tui"))]
            ops_area: std::cell::Cell::new(()),
            #[cfg(feature = "tui")]
            detail_area: std::cell::Cell::new(Rect::default()),
            #[cfg(not(feature = "tui"))]
            detail_area: std::cell::Cell::new(()),
            #[cfg(feature = "tui")]
            chat_input_area: std::cell::Cell::new(Rect::default()),
            #[cfg(not(feature = "tui"))]
            chat_input_area: std::cell::Cell::new(()),
            #[cfg(feature = "tui")]
            last_mouse_pos: std::cell::Cell::new(None),
            #[cfg(not(feature = "tui"))]
            last_mouse_pos: std::cell::Cell::new(()),
            chat_scroll_target: std::cell::Cell::new(0.0),
            chat_scroll_current: std::cell::Cell::new(0.0),
            log_scroll_target: std::cell::Cell::new(0.0),
            log_scroll_current: std::cell::Cell::new(0.0),
            chat_input_height_target: std::cell::Cell::new(1.0),
            chat_input_height_current: std::cell::Cell::new(1.0),
            chat_max_scroll: std::cell::Cell::new(0),
            log_max_scroll: std::cell::Cell::new(0),
            activity_scroll: 0,
            token_usage: ahma_llm_monitor::client::TokenUsage::default(),
            log_scroll: 0,
            log_follow: true,
            log_filter: String::new(),
            log_filter_active: false,
            log_files: vec![],
            active_log_file: None,
            active_log_lines: vec![],
            log_wrap_enabled: false,
            log_zoom_enabled: false,
            #[cfg(feature = "tui")]
            click_targets: std::cell::RefCell::new(vec![]),
            #[cfg(not(feature = "tui"))]
            click_targets: std::cell::RefCell::new(vec![]),
            #[cfg(feature = "tui")]
            ops_list_state: std::cell::RefCell::new(ratatui::widgets::ListState::default()),
            #[cfg(not(feature = "tui"))]
            ops_list_state: std::cell::RefCell::new(()),

            settings_editor: crate::settings_editor::SettingsEditor::default(),

            windows: vec![],
            next_window_id: 0,
            cleared_at: None,
            window_rects: std::cell::RefCell::new(vec![]),

            liveness_glyph: "  ".to_string(),
            liveness_state: LivenessState::Idle,
            liveness_seed: liveness_initial_seed(),
            last_stream_activity: None,
            last_wait_tick: None,
            unicode,
            should_quit: false,
            bridge_tx: None,
            #[cfg(feature = "tui")]
            mcp_source_tx: None,
            #[cfg(not(feature = "tui"))]
            mcp_source_tx: None,
            #[cfg(feature = "tui")]
            approval_tx: None,
            #[cfg(not(feature = "tui"))]
            approval_tx: None,
        }
    }

    /// Clear the visible chat history and finished command/LLM windows, then
    /// reset every scroll position to its startup default. Work still in
    /// progress is preserved: streaming assistant output, in-flight tool calls,
    /// and running/pending windows all stay on screen and keep updating.
    ///
    /// The monitor log (and its loaded file) is intentionally left untouched —
    /// `/clear` resets the chat surface, not the log feed. Scroll positions snap
    /// back to defaults: chat/ops/activity to the newest entry, and the log back
    /// to following the tail.
    pub fn clear_screen(&mut self) {
        // Chat: keep only the turn(s) still in flight.
        self.chat.retain_in_flight();

        // Windows: keep only those still running/pending (no finish timestamp).
        self.windows.retain(|w| w.finished_at.is_none());

        // Watermark so already-completed operations a source re-pushes are not
        // resurrected as windows (see `window_suppressed_by_clear`).
        self.cleared_at = Some(Instant::now());

        // Reset scroll positions to their startup defaults.
        self.chat_scroll = 0;
        self.chat_max_scroll.set(0);
        self.activity_scroll = 0;
        self.ops_selected = 0;
        self.ops_scroll.set(0);
        self.sync_chat_scroll_to_animation();

        // Log content is preserved; only the view snaps back to the tail.
        self.log_follow = true;
        self.log_scroll = self.log_max_scroll.get();
        self.sync_log_scroll_to_animation();
    }

    /// True when `op` finished before the most recent `/clear` and therefore
    /// should not be re-shown as a window. Operations still running (or that
    /// started after the clear) are never suppressed.
    pub fn window_suppressed_by_clear(&self, op: &Operation) -> bool {
        let Some(cleared) = self.cleared_at else {
            return false;
        };
        if !op.status.is_terminal() {
            return false;
        }
        match op.completed_at.or(op.started_at) {
            Some(t) => t <= cleared,
            // Terminal but with no timestamp at all: treat as pre-clear.
            None => true,
        }
    }

    pub fn sync_chat_scroll_to_animation(&self) {
        self.chat_scroll_target.set(self.chat_scroll as f64);
        self.chat_scroll_current.set(self.chat_scroll as f64);
    }

    pub fn sync_log_scroll_to_animation(&self) {
        self.log_scroll_target.set(self.log_scroll as f64);
        self.log_scroll_current.set(self.log_scroll as f64);
    }

    /// Detach tail-follow before a manual scroll. Snaps `log_scroll` to the
    /// current bottom (the last rendered `log_max_scroll`) so the upcoming
    /// up/page-up movement starts from where the user was actually looking,
    /// not from a stale offset left over while following.
    pub fn detach_log_follow(&mut self) {
        if self.log_follow {
            self.log_follow = false;
            self.log_scroll = self.log_max_scroll.get();
            self.sync_log_scroll_to_animation();
        }
    }

    /// Re-engage tail-follow if a downward scroll has reached the bottom, so new
    /// log lines resume tracking at the bottom automatically.
    pub fn maybe_reengage_log_follow(&mut self) {
        if self.log_scroll >= self.log_max_scroll.get() {
            self.log_follow = true;
        }
    }

    pub fn chat_input_text(&self) -> String {
        #[cfg(feature = "tui")]
        {
            self.chat_input.lines().join("\n")
        }
        #[cfg(not(feature = "tui"))]
        {
            String::new()
        }
    }

    pub fn chat_input_line_count(&self, width: usize) -> usize {
        #[cfg(feature = "tui")]
        {
            let mut total = 0;
            for line in self.chat_input.lines() {
                total += count_wrapped_lines(line, width);
            }
            total.max(1)
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = width;
            1
        }
    }
}

#[cfg(feature = "tui")]
fn parse_word_len<I: Iterator<Item = char>>(
    _first_char: char,
    chars: &mut std::iter::Peekable<I>,
) -> usize {
    let mut word_len = 1;
    while let Some(&next_c) = chars.peek() {
        if next_c != ' ' {
            word_len += 1;
            chars.next();
        } else {
            break;
        }
    }
    word_len
}

fn handle_word_fit(current_line_len: &mut usize, lines: &mut usize, word_len: usize, width: usize) {
    if *current_line_len + word_len <= width {
        *current_line_len += word_len;
    } else {
        if *current_line_len > 0 {
            *lines += 1;
        }
        let mut rem = word_len;
        while rem > width {
            *lines += 1;
            rem -= width;
        }
        *current_line_len = rem;
    }
}

#[cfg(feature = "tui")]
fn count_wrapped_lines(line: &str, width: usize) -> usize {
    let width = width.max(1);
    if line.is_empty() {
        return 1;
    }
    let mut lines = 0;
    let mut current_line_len = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            if current_line_len < width {
                current_line_len += 1;
            } else {
                lines += 1;
                current_line_len = 0;
            }
        } else {
            let word_len = parse_word_len(c, &mut chars);
            handle_word_fit(&mut current_line_len, &mut lines, word_len, width);
        }
    }
    if current_line_len > 0 {
        lines += 1;
    }
    lines.max(1)
}

impl AppState {
    pub fn chat_input_is_empty(&self) -> bool {
        self.chat_input_text().is_empty()
    }

    pub fn chat_input_is_blank(&self) -> bool {
        self.chat_input_text().trim().is_empty()
    }

    pub fn clear_chat_input(&mut self) {
        #[cfg(feature = "tui")]
        {
            self.chat_input = TextArea::default();
        }
    }

    /// Insert pasted text into the chat input without submitting it.
    ///
    /// The trailing newline that terminals append to a paste (e.g. pasting
    /// `"somecommand\n"`) is stripped so the paste is shown but not sent — the
    /// user presses Enter to submit. Interior newlines are preserved, so a
    /// multi-line paste becomes multiple input lines (one request, not many).
    pub fn paste_into_chat_input(&mut self, text: &str) {
        #[cfg(feature = "tui")]
        {
            let trimmed = text.trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                self.chat_input.insert_str(trimmed);
            }
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = text;
        }
    }

    pub fn selected_model(&self) -> String {
        self.llm_label
            .rsplit_once(" / ")
            .map(|(_, model)| model.trim().to_string())
            .unwrap_or_default()
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
        // New-line auto-scroll is handled by `log_follow`: when following, the
        // log pane renders pinned to the bottom (see `draw_log`), so there is no
        // scroll offset to nudge here. The previous heuristic adjusted
        // `log_scroll` by raw entry count, which was wrong once line-wrapping
        // made one entry span several rendered rows.
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

    /// Number of navigable rows in the ops pane. When the task tree has been
    /// drawn its rows are authoritative; before the first draw (or in tests
    /// that never render) fall back to the flat operation list.
    pub fn ops_row_count(&self) -> usize {
        let rows = self.task_rows.borrow();
        if rows.is_empty() {
            self.operations.len()
        } else {
            rows.len()
        }
    }

    /// Resolve the current ops-pane selection to an index into `operations`.
    /// Tree rows that are not operations (instance headers, session groups)
    /// resolve to the operation they relate to when unambiguous (`Output`
    /// rows → their op), otherwise `None`.
    pub fn selected_op_index(&self) -> Option<usize> {
        let rows = self.task_rows.borrow();
        if rows.is_empty() {
            return if self.ops_selected < self.operations.len() {
                Some(self.ops_selected)
            } else {
                None
            };
        }
        match rows.get(self.ops_selected).map(|r| &r.kind) {
            Some(crate::task_tree::RowKind::Op { op_index, .. })
            | Some(crate::task_tree::RowKind::Output { op_index, .. }) => Some(*op_index),
            _ => None,
        }
    }

    pub fn selected_op(&self) -> Option<&Operation> {
        self.selected_op_index()
            .and_then(|i| self.operations.get(i))
    }

    /// Enter/click on the selected task-tree row: accordion-expand an
    /// operation (collapsing the previously expanded one), or fold/unfold an
    /// instance or session header.
    pub fn toggle_selected_tree_node(&mut self) {
        use crate::task_tree::RowKind;
        let action = {
            let rows = self.task_rows.borrow();
            rows.get(self.ops_selected).map(|r| match &r.kind {
                RowKind::Instance { group_id, .. } => TreeToggle::Fold(format!("inst:{group_id}")),
                RowKind::Group { key, .. } => TreeToggle::Fold(key.clone()),
                RowKind::Op { op_index, .. } | RowKind::Output { op_index, .. } => {
                    TreeToggle::Expand(*op_index)
                }
            })
        };
        match action {
            Some(TreeToggle::Fold(key)) => self.toggle_collapse_key(key),
            Some(TreeToggle::Expand(op_index)) => {
                if let Some(op) = self.operations.get(op_index) {
                    if self.expanded_op.as_deref() == Some(op.id.as_str()) {
                        self.expanded_op = None;
                    } else {
                        self.expanded_op = Some(op.id.clone());
                    }
                }
            }
            None => {}
        }
    }

    /// Toggle membership of a fold key in `collapsed_nodes`.
    fn toggle_collapse_key(&mut self, key: String) {
        if !self.collapsed_nodes.remove(&key) {
            self.collapsed_nodes.insert(key);
        }
    }

    /// Compute which lines of `incoming` are genuinely new relative to
    /// `existing`: the largest k where `incoming[..k]` equals the suffix of
    /// `existing` marks already-seen content; everything after k is appended.
    /// With no overlap the whole incoming tail is considered new.
    fn tail_suffix_to_append(
        existing: &std::collections::VecDeque<String>,
        incoming: &std::collections::VecDeque<String>,
    ) -> Vec<String> {
        if existing.is_empty() || incoming.is_empty() {
            return incoming.iter().cloned().collect();
        }
        let max_k = existing.len().min(incoming.len());
        for k in (1..=max_k).rev() {
            let suffix_matches = existing
                .iter()
                .skip(existing.len() - k)
                .zip(incoming.iter().take(k))
                .all(|(a, b)| a == b);
            if suffix_matches {
                return incoming.iter().skip(k).cloned().collect();
            }
        }
        incoming.iter().cloned().collect()
    }

    pub fn upsert_operation(&mut self, op: Operation) {
        let id = op.id.clone();
        match self
            .operations
            .iter_mut()
            .find(|o| o.id == op.id && o.instance_id == op.instance_id)
        {
            Some(existing) => Self::merge_operation(existing, op),
            None => self.operations.push(op),
        }
        // Flush any output that arrived before this op was materialised (see
        // `pending_output`). Append into the (now present) op's tail via the
        // same overlap-dedup merge the poll/hub paths use, so a later poll that
        // re-sends the full tail does not duplicate these lines.
        if let Some(pending) = self.pending_output.remove(&id)
            && let Some(existing) = self.operations.iter_mut().find(|o| o.id == id)
        {
            let new_lines = Self::tail_suffix_to_append(&existing.stdout_tail, &pending);
            for line in new_lines {
                if existing.stdout_tail.len() >= STDOUT_TAIL_CAP {
                    existing.stdout_tail.pop_front();
                }
                existing.stdout_tail.push_back(line);
            }
        }
        self.clamp_ops_selection();
    }

    /// Stash a live output line for an operation not yet present in
    /// `operations`. Flushed into the op's tail by [`Self::upsert_operation`]
    /// when the operation is first materialised. Bounded per-op at
    /// `STDOUT_TAIL_CAP`; the number of distinct buffered ops is capped so a
    /// stream of output for ops that never materialise cannot grow without
    /// bound. Returns `true` if the line was buffered.
    pub fn buffer_pending_output(&mut self, op_id: &str, line: String) -> bool {
        // Cap distinct buffered ops: if this is a new op id and we are already
        // at the cap, drop the line rather than grow unboundedly.
        const MAX_PENDING_OPS: usize = 64;
        if !self.pending_output.contains_key(op_id) && self.pending_output.len() >= MAX_PENDING_OPS
        {
            return false;
        }
        let buf = self.pending_output.entry(op_id.to_string()).or_default();
        if buf.len() >= STDOUT_TAIL_CAP {
            buf.pop_front();
        }
        buf.push_back(line);
        true
    }

    /// Merge an incoming operation update into the matching existing entry.
    ///
    /// Status-poll updates (from the daemon HTTP source) may arrive interleaved
    /// with push updates (from the daemon hub).  Each source may populate a
    /// different subset of fields, so we merge rather than replace to avoid
    /// clobbering output that was delivered by a different path.
    fn merge_operation(existing: &mut Operation, op: Operation) {
        // Status always advances (but never regresses from terminal back to running).
        if !existing.status.is_terminal() || op.status.is_terminal() {
            existing.status = op.status;
        }

        // Scalar metadata: take from incoming if it carries a richer value.
        if !op.description.is_empty() {
            existing.description = op.description;
        }
        if op.cwd.is_some() {
            existing.cwd = op.cwd;
        }
        if !op.args.is_empty() {
            existing.args = op.args;
        }
        if op.pid.is_some() {
            existing.pid = op.pid;
        }
        if op.scope.is_some() {
            existing.scope = op.scope;
        }
        if op.result_summary.is_some() {
            existing.result_summary = op.result_summary;
        }
        if op.completed_at.is_some() {
            existing.completed_at = op.completed_at;
        }
        if op.duration_ms.is_some() {
            existing.duration_ms = op.duration_ms;
        }

        // Merge stdout tails without duplicating: the poll path re-sends the
        // operation's FULL current tail on every cycle, and the hub path
        // streams the same lines incrementally.  Find the largest overlap
        // between the existing tail's suffix and the incoming tail's prefix,
        // then append only the genuinely new remainder.
        let new_lines = Self::tail_suffix_to_append(&existing.stdout_tail, &op.stdout_tail);
        for line in new_lines {
            if existing.stdout_tail.len() >= STDOUT_TAIL_CAP {
                existing.stdout_tail.pop_front();
            }
            existing.stdout_tail.push_back(line);
        }

        // Append new alerts (deduplicate by content).
        for alert in op.alerts {
            if !existing.alerts.contains(&alert) {
                existing.alerts.push(alert);
            }
        }

        // pinned is sticky — once pinned it stays pinned.
        existing.pinned = existing.pinned || op.pinned;
    }

    fn clamp_ops_selection(&mut self) {
        let count = self.ops_row_count();
        if count > 0 {
            self.ops_selected = self.ops_selected.min(count - 1);
        }
    }
}

/// What a task-tree toggle resolves to (see
/// [`AppState::toggle_selected_tree_node`]).
enum TreeToggle {
    /// Fold/unfold a header (instance or session group) by collapse key.
    Fold(String),
    /// Accordion-expand the operation at this index into `operations`.
    Expand(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Selection resolves through the drawn task-tree rows (not raw operation
    /// indices), and Enter behaves as a single-expand accordion for ops and a
    /// fold toggle for headers (SPEC R24.4).
    #[test]
    #[cfg(feature = "tui")]
    fn tree_selection_resolves_through_rows_and_toggles_accordion() {
        use crate::task_tree::{RowKind, TreeRow};
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.operations
            .push(Operation::new("op-a", "tool", OpStatus::Running));
        s.operations
            .push(Operation::new("op-b", "tool", OpStatus::Running));
        // Simulate a drawn frame with deliberately reordered ops to prove the
        // rows are authoritative for resolution.
        *s.task_rows.borrow_mut() = vec![
            TreeRow {
                kind: RowKind::Instance {
                    group_id: "i1".into(),
                    label: "claude-code".into(),
                    detail: String::new(),
                    counts: Default::default(),
                    collapsed: false,
                },
                depth: 0,
            },
            TreeRow {
                kind: RowKind::Op {
                    op_index: 1,
                    expanded: false,
                },
                depth: 1,
            },
            TreeRow {
                kind: RowKind::Op {
                    op_index: 0,
                    expanded: false,
                },
                depth: 1,
            },
        ];

        // Header row resolves to no operation; op rows map through op_index.
        s.ops_selected = 0;
        assert!(s.selected_op().is_none());
        s.ops_selected = 1;
        assert_eq!(s.selected_op().unwrap().id, "op-b");

        // Accordion: expanding a second op collapses the first implicitly.
        s.toggle_selected_tree_node();
        assert_eq!(s.expanded_op.as_deref(), Some("op-b"));
        s.ops_selected = 2;
        s.toggle_selected_tree_node();
        assert_eq!(s.expanded_op.as_deref(), Some("op-a"));
        // Toggling the same op collapses it.
        s.toggle_selected_tree_node();
        assert_eq!(s.expanded_op, None);

        // Header toggle folds and unfolds the instance subtree.
        s.ops_selected = 0;
        s.toggle_selected_tree_node();
        assert!(s.collapsed_nodes.contains("inst:i1"));
        s.toggle_selected_tree_node();
        assert!(!s.collapsed_nodes.contains("inst:i1"));
    }

    /// Before any frame is drawn the row list is empty and selection falls
    /// back to flat operation indices, so headless/chat-only flows keep
    /// working.
    #[test]
    #[cfg(feature = "tui")]
    fn selection_falls_back_to_flat_list_before_first_draw() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.operations
            .push(Operation::new("only", "tool", OpStatus::Running));
        s.ops_selected = 0;
        assert_eq!(s.selected_op().unwrap().id, "only");
        assert_eq!(s.ops_row_count(), 1);
    }

    #[test]
    #[cfg(feature = "tui")]
    fn paste_strips_trailing_newline_without_submitting() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        // Pasting "somecommand\n" shows "somecommand" — the trailing newline is
        // dropped so it is not auto-submitted; the user must press Enter.
        state.paste_into_chat_input("somecommand\n");
        assert_eq!(state.chat_input_text(), "somecommand");
    }

    #[test]
    #[cfg(feature = "tui")]
    fn paste_strips_trailing_crlf() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.paste_into_chat_input("somecommand\r\n");
        assert_eq!(state.chat_input_text(), "somecommand");
    }

    #[test]
    #[cfg(feature = "tui")]
    fn paste_keeps_interior_newlines_as_multiline_input() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        // A multi-line paste becomes multiple input lines (a single request),
        // with the trailing newline still stripped.
        state.paste_into_chat_input("line one\nline two\n");
        assert_eq!(state.chat_input_text(), "line one\nline two");
    }

    #[test]
    #[cfg(feature = "tui")]
    fn paste_appends_to_existing_input() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.chat_input.insert_str("echo ");
        state.paste_into_chat_input("hello\n");
        assert_eq!(state.chat_input_text(), "echo hello");
    }

    #[test]
    #[cfg(feature = "tui")]
    fn paste_of_only_newline_is_noop() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.paste_into_chat_input("\n");
        assert!(state.chat_input_is_empty());
    }

    #[test]
    #[cfg(feature = "tui")]
    fn test_count_wrapped_lines() {
        assert_eq!(count_wrapped_lines("hello world", 10), 2);
        assert_eq!(count_wrapped_lines("a verylongword", 10), 3);
        assert_eq!(count_wrapped_lines(" ", 10), 1);
        assert_eq!(count_wrapped_lines("", 10), 1);
        assert_eq!(count_wrapped_lines("one two three four five", 100), 1);
    }

    #[test]
    fn liveness_char_is_visible_braille_in_unicode_mode() {
        // Every possible xorshift residue maps to a non-blank Braille cell.
        for state in 0u64..512 {
            let c = liveness_char(state, true);
            let cp = c as u32;
            assert!(
                (0x2801..=0x28FF).contains(&cp),
                "expected a non-blank braille cell, got U+{cp:04X}"
            );
        }
    }

    #[test]
    fn liveness_char_is_ascii_spinner_without_unicode() {
        for state in 0u64..16 {
            assert!(matches!(
                liveness_char(state, false),
                '|' | '/' | '-' | '\\'
            ));
        }
    }

    #[test]
    fn bump_liveness_changes_glyph_and_reset_clears_it() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        assert_eq!(s.liveness_glyph, "  ", "starts idle (two blanks)");

        s.bump_liveness();
        let first = s.liveness_glyph.clone();
        assert_eq!(
            first.chars().count(),
            2,
            "the glyph is a two-cell wide indicator"
        );
        assert_ne!(first, "  ", "a bump produces a visible glyph");

        // The seed advances, so successive bumps move the glyph (deterministically
        // via xorshift). Collect a few and confirm they are not all identical.
        let mut seen = std::collections::HashSet::new();
        seen.insert(first);
        for _ in 0..8 {
            s.bump_liveness();
            seen.insert(s.liveness_glyph.clone());
        }
        assert!(seen.len() > 1, "liveness glyph should change across bumps");

        s.reset_liveness();
        assert_eq!(
            s.liveness_glyph, "  ",
            "reset collapses the glyph to two blanks"
        );
    }

    #[test]
    fn waiting_spinner_ticks_slowly_and_not_during_active_streaming() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        // Fresh in-flight turn, no output yet → first wait-tick fires immediately.
        assert!(s.tick_waiting_spinner(), "first waiting tick advances");
        // Immediately calling again is throttled by the slow cadence.
        assert!(
            !s.tick_waiting_spinner(),
            "slow cadence throttles back-to-back ticks"
        );
        // Real output just arrived → waiting ticks are suppressed (tokens drive it).
        s.mark_stream_activity(LivenessState::Streaming);
        assert!(
            !s.tick_waiting_spinner(),
            "no waiting tick while output is actively streaming"
        );
    }

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
        assert_eq!(Focus::Chat.cycle_next(), Focus::OpsDag);
        assert_eq!(Focus::OpsDag.cycle_next(), Focus::Log);
        assert_eq!(Focus::Log.cycle_next(), Focus::Chat);
        assert_eq!(Focus::Log.cycle_prev(), Focus::OpsDag);
        assert_eq!(Focus::Chat.cycle_prev(), Focus::Log);
    }

    #[test]
    fn upsert_operation_updates_existing() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.upsert_operation(Operation::new("op1", "cargo_build", OpStatus::Running));
        s.upsert_operation(Operation::new("op1", "cargo_build", OpStatus::Succeeded));
        assert_eq!(s.operations.len(), 1);
        assert_eq!(s.operations[0].status, OpStatus::Succeeded);
    }

    #[test]
    fn upsert_operation_multi_instance() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        let mut op1 = Operation::new("op1", "cargo_build", OpStatus::Running);
        op1.instance_id = Some("inst1".to_string());
        let mut op2 = Operation::new("op1", "cargo_build", OpStatus::Running);
        op2.instance_id = Some("inst2".to_string());

        s.upsert_operation(op1);
        s.upsert_operation(op2);

        assert_eq!(s.operations.len(), 2);
    }

    #[test]
    fn upsert_merges_tails_without_duplication() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        let mut op = Operation::new("op1", "cargo_build", OpStatus::Running);
        op.stdout_tail = vec!["a".to_string(), "b".to_string()].into();
        s.upsert_operation(op);

        // Poll re-sends the full tail plus one new line — only "c" must append.
        let mut update = Operation::new("op1", "cargo_build", OpStatus::Running);
        update.stdout_tail = vec!["a".to_string(), "b".to_string(), "c".to_string()].into();
        s.upsert_operation(update);
        assert_eq!(
            s.operations[0].stdout_tail,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );

        // Identical re-send appends nothing.
        let mut same = Operation::new("op1", "cargo_build", OpStatus::Running);
        same.stdout_tail = vec!["a".to_string(), "b".to_string(), "c".to_string()].into();
        s.upsert_operation(same);
        assert_eq!(s.operations[0].stdout_tail.len(), 3);
    }

    #[test]
    fn pending_output_flushes_when_op_materialises() {
        // Regression: an OpOutput line that arrives before its operation (e.g.
        // after a daemon-hub reconnect) must not be dropped. It is buffered and
        // flushed into the op's tail once the op appears, so fast commands like
        // `!pwd` still show their output.
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);

        // Output arrives first — no matching op yet.
        assert!(s.buffer_pending_output("op1", "/work/dir".to_string()));
        assert!(s.operations.is_empty());

        // The op's snapshot lands afterwards (empty tail, as OpStarted carries
        // no output) and must absorb the buffered line.
        s.upsert_operation(Operation::new(
            "op1",
            "run_terminal_command",
            OpStatus::Running,
        ));
        assert_eq!(s.operations.len(), 1);
        assert_eq!(
            s.operations[0].stdout_tail,
            vec!["/work/dir".to_string()],
            "buffered output should flush into the materialised op"
        );
        // Buffer is consumed, not left to leak or double-flush.
        assert!(s.pending_output.is_empty());
    }

    #[test]
    fn pending_output_flush_does_not_duplicate_against_poll_tail() {
        // If the op later materialises via the poll path (which re-sends the
        // full tail), the overlap-dedup merge must not duplicate the line the
        // hub already buffered.
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.buffer_pending_output("op1", "line-a".to_string());

        // Poll snapshot already includes "line-a" in its tail.
        let mut op = Operation::new("op1", "run_terminal_command", OpStatus::Succeeded);
        op.stdout_tail = vec!["line-a".to_string()].into();
        s.upsert_operation(op);

        assert_eq!(s.operations[0].stdout_tail, vec!["line-a".to_string()]);
    }

    #[test]
    fn pending_output_caps_distinct_ops() {
        // A flood of output for ops that never materialise must not grow the
        // buffer without bound.
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        for i in 0..64 {
            assert!(s.buffer_pending_output(&format!("op{i}"), "x".to_string()));
        }
        // 65th distinct op is refused.
        assert!(!s.buffer_pending_output("op64", "x".to_string()));
        // But more output for an already-buffered op is still accepted.
        assert!(s.buffer_pending_output("op0", "y".to_string()));
        assert_eq!(s.pending_output.len(), 64);
    }

    #[test]
    fn tail_suffix_to_append_no_overlap_appends_all() {
        let existing: std::collections::VecDeque<String> =
            vec!["x".to_string(), "y".to_string()].into();
        let incoming: std::collections::VecDeque<String> =
            vec!["p".to_string(), "q".to_string()].into();
        assert_eq!(
            AppState::tail_suffix_to_append(&existing, &incoming),
            vec!["p".to_string(), "q".to_string()]
        );
    }

    #[test]
    fn test_chat_history_compact() {
        let mut hist = ChatHistory::default();
        hist.push(ChatEntry::User {
            text: "Hello!".into(),
            started_at: None,
            duration_ms: None,
        });
        hist.start_tool_call("call_1".into(), "test_tool".into(), "{}".into());
        hist.append_token("I ran the tool.");
        hist.finish_stream();
        hist.push(ChatEntry::User {
            text: "Thanks.".into(),
            started_at: None,
            duration_ms: None,
        });

        assert_eq!(hist.entries.len(), 4);
        hist.compact(1); // Keep the last 1 item ("Thanks.") intact

        // The tool call (at index 1) should be dropped, but user and assistant messages kept.
        assert_eq!(hist.entries.len(), 3);
        assert!(matches!(hist.entries[0], ChatEntry::User { .. }));
        assert!(matches!(hist.entries[1], ChatEntry::Assistant { .. }));
        assert!(matches!(hist.entries[2], ChatEntry::User { .. }));
    }

    #[test]
    fn test_operation_elapsed_display() {
        let mut op = Operation::new("op1", "run_terminal_command", OpStatus::Running);
        // Test in-progress elapsed formatting (always in seconds)
        let display = op.elapsed_display();
        assert!(display.ends_with('s'));

        // Test completed elapsed formatting (stops counting using completed_at)
        let now = Instant::now();
        op.started_at = Some(now);
        op.completed_at = Some(now + Duration::from_secs(67));
        assert_eq!(op.elapsed_display(), "67s");

        // Test completed elapsed formatting (stops counting using duration_ms)
        op.duration_ms = Some(42000);
        assert_eq!(op.elapsed_display(), "42s");
    }

    #[test]
    fn test_operation_display_name() {
        // Test non-run_terminal_command fallback
        let op_cargo = Operation::new("op1", "cargo_build", OpStatus::Running);
        assert_eq!(op_cargo.display_name(), "cargo_build");

        // Test run_terminal_command with structured description
        let mut op_term = Operation::new("op2", "run_terminal_command", OpStatus::Running);
        op_term.description = "/bin/sh {\"c_flag\": true, \"command\": \"sleep 10\"}".to_string();
        assert_eq!(op_term.display_name(), "sleep 10");

        // Test run_terminal_command with "Execute <cmd> in <dir>" description (daemon reporter)
        let mut op_term_exec = Operation::new("op3", "run_terminal_command", OpStatus::Running);
        op_term_exec.description =
            "Execute sleep 30 in /Users/paulhoughton/github/ahma".to_string();
        assert_eq!(op_term_exec.display_name(), "sleep 30");

        // Test run_terminal_command with op_id fallback
        let mut op_term_fallback_id = Operation::new(
            "op_1_cargo_build_release",
            "run_terminal_command",
            OpStatus::Running,
        );
        op_term_fallback_id.description = "invalid description".to_string();
        assert_eq!(op_term_fallback_id.display_name(), "cargo build release");

        // Test run_terminal_command with fallback description
        let mut op_term_fallback = Operation::new("op4", "run_terminal_command", OpStatus::Running);
        op_term_fallback.description = "invalid description".to_string();
        assert_eq!(op_term_fallback.display_name(), "run_terminal_command");
    }

    #[test]
    fn test_operation_clean_id() {
        let op1 = Operation::new("op_1_sleep_30", "run_terminal_command", OpStatus::Running);
        assert_eq!(op1.clean_id(), "op_1");

        let op2 = Operation::new(
            "op_10_cargo_build",
            "run_terminal_command",
            OpStatus::Running,
        );
        assert_eq!(op2.clean_id(), "op_10");

        let op3 = Operation::new("op-001", "run_terminal_command", OpStatus::Running);
        assert_eq!(op3.clean_id(), "op-001");

        let op4 = Operation::new("a8f9c2d3", "run_terminal_command", OpStatus::Running);
        assert_eq!(op4.clean_id(), "a8f9c2");
    }

    fn test_window(id: usize, status: WindowStatus, finished: bool) -> TuiWindow {
        let (abort_tx, _abort_rx) = tokio::sync::oneshot::channel::<()>();
        TuiWindow {
            id,
            label: format!("win {id}"),
            status,
            content: vec![],
            collapsed: false,
            finished_at: if finished { Some(Instant::now()) } else { None },
            is_cli: true,
            command: String::new(),
            working_dir: String::new(),
            llm_model: None,
            visible: true,
            abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(Some(abort_tx))),
            op_id: None,
        }
    }

    #[test]
    fn retain_in_flight_keeps_only_active_turns() {
        let mut hist = ChatHistory::default();
        hist.push(ChatEntry::User {
            text: "done turn".into(),
            started_at: None,
            duration_ms: None,
        });
        hist.push(ChatEntry::Assistant {
            content: "finished reply".into(),
            streaming: false,
        });
        hist.start_tool_call("call_done".into(), "tool".into(), "{}".into());
        hist.finish_tool_call("call_done", "ok".into(), false);
        // In-flight work that must survive a clear:
        hist.start_tool_call("call_live".into(), "tool".into(), "{}".into());
        hist.push(ChatEntry::Assistant {
            content: "streaming…".into(),
            streaming: true,
        });

        hist.retain_in_flight();

        assert_eq!(hist.len(), 2);
        assert!(matches!(
            hist.entries()[0],
            ChatEntry::ToolCall { result: None, .. }
        ));
        assert!(matches!(
            hist.entries()[1],
            ChatEntry::Assistant {
                streaming: true,
                ..
            }
        ));
    }

    #[test]
    fn clear_screen_drops_finished_keeps_running_and_resets_scroll() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.windows.push(test_window(1, WindowStatus::Running, false));
        s.windows.push(test_window(2, WindowStatus::Finished, true));
        s.windows.push(test_window(3, WindowStatus::Pending, false));

        s.chat.push(ChatEntry::User {
            text: "hi".into(),
            started_at: None,
            duration_ms: None,
        });

        // Logs must be preserved across a clear.
        s.push_log(LogEntry {
            timestamp: chrono::Local::now(),
            level: LogLevel::Info,
            message: "keep me".into(),
        });

        // Dirty scroll state that clear must reset.
        s.chat_scroll = 42;
        s.activity_scroll = 7;
        s.ops_selected = 3;
        s.ops_scroll.set(9);
        s.log_follow = false;

        s.clear_screen();

        let ids: Vec<usize> = s.windows.iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![1, 3], "finished window removed, in-flight kept");
        assert!(s.chat.is_empty(), "completed chat entries cleared");
        assert_eq!(s.log.len(), 1, "log feed preserved");
        assert_eq!(s.chat_scroll, 0);
        assert_eq!(s.activity_scroll, 0);
        assert_eq!(s.ops_selected, 0);
        assert_eq!(s.ops_scroll.get(), 0);
        assert!(s.log_follow, "log re-engages tail-follow");
        assert!(s.cleared_at.is_some());
    }

    #[test]
    fn window_suppressed_by_clear_only_hits_pre_clear_terminal_ops() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);

        // Before any clear nothing is suppressed.
        let done = Operation::new("op1", "cargo_build", OpStatus::Succeeded);
        assert!(!s.window_suppressed_by_clear(&done));

        s.clear_screen();

        // A terminal op that finished before the clear is suppressed.
        let mut old = Operation::new("op2", "cargo_build", OpStatus::Succeeded);
        old.started_at = Some(Instant::now() - Duration::from_secs(5));
        old.completed_at = Some(Instant::now() - Duration::from_secs(4));
        assert!(s.window_suppressed_by_clear(&old));

        // A still-running op is never suppressed, even if it started earlier.
        let mut running = Operation::new("op3", "cargo_build", OpStatus::Running);
        running.started_at = Some(Instant::now() - Duration::from_secs(5));
        assert!(!s.window_suppressed_by_clear(&running));

        // A brand-new op that completes after the clear stays visible.
        let mut fresh = Operation::new("op4", "cargo_build", OpStatus::Succeeded);
        fresh.completed_at = Some(Instant::now() + Duration::from_secs(1));
        assert!(!s.window_suppressed_by_clear(&fresh));
    }

    #[test]
    fn test_navigator_completions_filtering() {
        let tools = vec!["cargo_build".to_string()];

        // When input is empty, should return all builtins (including /quit, but NOT /q) plus dynamic tools
        let mut nav = CommandNavigator::opened(&tools);
        assert!(nav.completions.iter().any(|c| c.command == "/quit"));
        assert!(!nav.completions.iter().any(|c| c.command == "/q"));
        assert!(
            nav.completions
                .iter()
                .any(|c| c.command == "/run cargo_build")
        );

        // When input is "quit", should match /quit
        nav.input = "quit".to_string();
        nav.refresh_completions(&tools);
        assert_eq!(nav.completions.len(), 1);
        assert_eq!(nav.completions[0].command, "/quit");

        // When input is "/quit", should match /quit
        nav.input = "/quit".to_string();
        nav.refresh_completions(&tools);
        assert_eq!(nav.completions.len(), 1);
        assert_eq!(nav.completions[0].command, "/quit");

        // When input is "exit", should match /quit (via the "exit" starts_with alias check)
        nav.input = "exit".to_string();
        nav.refresh_completions(&tools);
        assert_eq!(nav.completions.len(), 1);
        assert_eq!(nav.completions[0].command, "/quit");

        // When input is "ex", should match /quit (via alias) and /export markdown (via prefix)
        nav.input = "ex".to_string();
        nav.refresh_completions(&tools);
        assert_eq!(nav.completions.len(), 2);
        assert!(nav.completions.iter().any(|c| c.command == "/quit"));
        assert!(
            nav.completions
                .iter()
                .any(|c| c.command == "/export markdown")
        );

        // When input is "/exi", should match /quit
        nav.input = "/exi".to_string();
        nav.refresh_completions(&tools);
        assert_eq!(nav.completions.len(), 1);
        assert_eq!(nav.completions[0].command, "/quit");

        // When input is "q", should match /quit and potentially other commands containing 'q', but not /q
        nav.input = "q".to_string();
        nav.refresh_completions(&tools);
        assert!(!nav.completions.iter().any(|c| c.command == "/q"));
        assert!(nav.completions.iter().any(|c| c.command == "/quit"));
    }

    /// SPEC R23: at most one user overlay is open at a time. Opening a second
    /// overlay replaces the first; the projection accessors agree with the
    /// active variant and report `None` for every other overlay.
    #[test]
    fn modal_state_is_mutually_exclusive() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        assert!(!s.modal_open());

        // Opening the help screen.
        s.modal = ModalState::Help;
        assert!(s.is_help_open());
        assert!(s.navigator().is_none());
        assert!(s.palette().is_none());

        // Opening the navigator replaces help — both are never open at once.
        s.modal = ModalState::Navigator(CommandNavigator::opened(&[]));
        assert!(!s.is_help_open());
        assert!(s.navigator().is_some());
        assert!(s.text_entry_modal_open());

        // Opening the log-file switcher replaces the navigator.
        s.open_log_files_modal(2);
        assert!(s.navigator().is_none());
        assert_eq!(s.log_files_selected(), Some(2));
        assert!(!s.text_entry_modal_open());

        // Closing returns to the no-overlay state.
        s.close_modal();
        assert!(!s.modal_open());
        assert_eq!(s.log_files_selected(), None);
    }

    /// `take_*_picker` extracts the picker and closes the modal; calling it for
    /// the wrong picker leaves the modal untouched.
    #[test]
    fn take_picker_extracts_and_closes() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.modal = ModalState::ProviderPicker(PickerState::new("p", vec!["a".into()]));

        // Wrong picker: no extraction, modal preserved.
        assert!(s.take_model_picker().is_none());
        assert!(s.provider_picker().is_some());

        // Right picker: extracted and modal closed.
        let picker = s.take_provider_picker();
        assert!(picker.is_some());
        assert!(!s.modal_open());
    }
}
