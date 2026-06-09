//! Application state — single source of truth for all TUI panels.

use std::{
    collections::VecDeque,
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

// ─── Chat history ─────────────────────────────────────────────────────────────

/// A single entry in the chat history.
#[derive(Debug, Clone)]
pub enum ChatEntry {
    /// A message submitted by the user.
    User(String),
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

    /// Mark the last assistant entry as no longer streaming.
    pub fn finish_stream(&mut self) {
        if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.back_mut() {
            *streaming = false;
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
            description: "clear chat history",
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
            command: "/exit".into(),
            description: "quit the application",
        },
        NavCommand {
            command: "/q".into(),
            description: "quit the application (alias)",
        },
        NavCommand {
            command: "/quit".into(),
            description: "quit the application (alias)",
        },
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
    /// True when the overlay is visible.
    pub visible: bool,
}

impl CommandNavigator {
    pub fn open(&mut self, tools: &[String]) {
        self.visible = true;
        self.input.clear();
        self.selected = 0;
        self.refresh_completions(tools);
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.input.clear();
        self.completions.clear();
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
            self.completions = cmds
                .into_iter()
                .filter(|c| {
                    c.command.to_lowercase().contains(&q)
                        || c.description.to_lowercase().contains(&q)
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
            return format!("{}s", ms / 1000);
        }
        let secs = if let (Some(start), Some(end)) = (self.started_at, self.completed_at) {
            if end >= start {
                end.duration_since(start).as_secs()
            } else {
                0
            }
        } else {
            self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0)
        };
        format!("{secs}s")
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
    pub description: String,
    pub deadline: Option<Instant>,
    pub diff: Option<String>,
}

impl ApprovalGate {
    pub fn remaining_secs(&self) -> Option<u64> {
        self.deadline.map(|d| {
            let now = Instant::now();
            if d > now { (d - now).as_secs() } else { 0 }
        })
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

/// A window representing a running or finished CLI command or LLM call.
#[derive(Debug, Clone)]
pub struct TuiWindow {
    pub id: usize,
    pub label: String,
    pub status: String, // "Pending", "Running", "Finished", "Cancelled", "Error"
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

/// Top-level application state — owns all panel data and UI mode.
pub struct AppState {
    pub windows: Vec<TuiWindow>,
    pub next_window_id: usize,
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

    // ── Panel data ──
    pub ai_activity: VecDeque<AiActivityEntry>,
    pub operations: Vec<Operation>,
    pub log: VecDeque<LogEntry>,
    pub approval: Option<ApprovalGate>,
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
    /// Command navigator state.
    pub navigator: CommandNavigator,
    /// Inline provider picker (Some when active).
    pub provider_picker: Option<PickerState>,
    /// Inline model picker (Some when active).
    pub model_picker: Option<PickerState>,

    // ── UI state ──
    pub focus: Focus,
    pub ops_selected: usize,
    pub ops_scroll: std::cell::Cell<usize>,
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
    pub log_filter: String,
    pub log_filter_active: bool,
    pub palette: PaletteState,
    pub log_files: Vec<LogFileInfo>,
    pub active_log_file: Option<String>,
    pub active_log_lines: Vec<String>,
    pub log_files_modal_open: bool,
    pub log_files_modal_selected: usize,
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
    pub show_help: bool,

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
            ai_activity: VecDeque::with_capacity(ACTIVITY_RING_CAP),
            operations: vec![],
            log: VecDeque::with_capacity(LOG_RING_CAP),
            approval: None,
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
            navigator: CommandNavigator::default(),
            provider_picker: None,
            model_picker: None,

            focus: Focus::default(),
            ops_selected: 0,
            ops_scroll: std::cell::Cell::new(0),
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
            log_filter: String::new(),
            log_filter_active: false,
            palette: PaletteState::default(),
            log_files: vec![],
            active_log_file: None,
            active_log_lines: vec![],
            log_files_modal_open: false,
            log_files_modal_selected: 0,
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
            show_help: false,

            windows: vec![],
            next_window_id: 0,
            window_rects: std::cell::RefCell::new(vec![]),

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

    pub fn sync_chat_scroll_to_animation(&self) {
        self.chat_scroll_target.set(self.chat_scroll as f64);
        self.chat_scroll_current.set(self.chat_scroll as f64);
    }

    pub fn sync_log_scroll_to_animation(&self) {
        self.log_scroll_target.set(self.log_scroll as f64);
        self.log_scroll_current.set(self.log_scroll as f64);
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
        if let Some(existing) = self
            .operations
            .iter_mut()
            .find(|o| o.id == op.id && o.instance_id == op.instance_id)
        {
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
    #[cfg(feature = "tui")]
    fn test_count_wrapped_lines() {
        assert_eq!(count_wrapped_lines("hello world", 10), 2);
        assert_eq!(count_wrapped_lines("a verylongword", 10), 3);
        assert_eq!(count_wrapped_lines(" ", 10), 1);
        assert_eq!(count_wrapped_lines("", 10), 1);
        assert_eq!(count_wrapped_lines("one two three four five", 100), 1);
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
    fn test_chat_history_compact() {
        let mut hist = ChatHistory::default();
        hist.push(ChatEntry::User("Hello!".into()));
        hist.start_tool_call("call_1".into(), "test_tool".into(), "{}".into());
        hist.append_token("I ran the tool.");
        hist.finish_stream();
        hist.push(ChatEntry::User("Thanks.".into()));

        assert_eq!(hist.entries.len(), 4);
        hist.compact(1); // Keep the last 1 item ("Thanks.") intact

        // The tool call (at index 1) should be dropped, but user and assistant messages kept.
        assert_eq!(hist.entries.len(), 3);
        assert!(matches!(hist.entries[0], ChatEntry::User(_)));
        assert!(matches!(hist.entries[1], ChatEntry::Assistant { .. }));
        assert!(matches!(hist.entries[2], ChatEntry::User(_)));
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
}
