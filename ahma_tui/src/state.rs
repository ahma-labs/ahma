//! Application state — single source of truth for all TUI panels.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use crate::mcp_connections::McpConnectionManager;
use crate::session_config::TuiSessionConfig;
use ratatui::layout::Rect;
use tui_textarea::TextArea;

// ─── Ring-buffer capacities ───────────────────────────────────────────────────

pub const ACTIVITY_RING_CAP: usize = 64;
pub const LOG_RING_CAP: usize = 500;
pub const STDOUT_TAIL_CAP: usize = 100;
pub const CHAT_HISTORY_CAP: usize = 200;

// ─── Liveness State Machine ───────────────────────────────────────────────────

/// How long a turn may go without any server event before the TUI says it
/// looks stalled. Long enough for a slow model's first token, short enough that
/// a turn silently lost to a dead daemon is not left looking busy.
pub const TURN_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// The same, for a model still reading its prompt: that is silent by nature
/// and can legitimately take minutes on a local model (the status line says
/// so meanwhile), but a turn lost to a dead daemon must still be flagged.
pub const READING_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

/// Token spend for one window's conversations, as reported by its provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// The last turn's prompt size: how full the context window is now.
    pub last_prompt_tokens: u32,
}

/// Window in which a second Ctrl-C quits. The first one cancels a running turn.
pub const CTRL_C_QUIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// A chat turn submitted to the daemon and not yet ended by `AgentDone` or
/// `AgentError`. Unlike `liveness_state`, which stays `Idle` until the first
/// server event, this exists from the moment the prompt is sent.
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub started: std::time::Instant,
    /// Last event of any kind for this turn; drives the stall hint.
    pub last_event: std::time::Instant,
    /// The instance the prompt was routed to, so a cancel goes to the same one.
    pub target_instance: Option<String>,
    /// When the first answer token arrived, for the live tokens/second meter.
    pub first_token_at: Option<std::time::Instant>,
    /// Characters of answer streamed so far this turn.
    pub streamed_chars: usize,
    /// What the turn is doing right now, in words the user can act on.
    pub phase: TurnPhase,
    /// When [`Self::phase`] began — the clock the status line shows.
    pub phase_since: std::time::Instant,
    /// How long the model took to start answering its last request (from the
    /// request to its first token or thought). Paired with the prompt size the
    /// next `Usage` reports, it becomes the model's measured reading speed.
    pub last_prefill: Option<std::time::Duration>,
}

/// What a running turn is doing, derived only from real events — never a guess
/// dressed up as progress. Shown on the chat status line so a slow local model
/// reads as "busy reading 37k tokens", not as dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnPhase {
    /// The request is with the model and it has not produced anything yet:
    /// it is loading, or reading (prefilling) the whole prompt.
    Reading,
    /// The model is reasoning (thinking tokens are arriving).
    Thinking,
    /// The answer is streaming.
    Writing,
    /// A tool the model asked for is running.
    Tool { name: String },
    /// Blocked on the user answering an approval — not the model's time.
    AwaitingYou,
}

impl ChatTurn {
    pub fn new(target_instance: Option<String>) -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            last_event: now,
            target_instance,
            first_token_at: None,
            streamed_chars: 0,
            phase: TurnPhase::Reading,
            phase_since: now,
            last_prefill: None,
        }
    }

    /// Move to `phase`, restarting its clock only when it actually changes.
    /// Leaving [`TurnPhase::Reading`] for model output records how long the
    /// model took to read its prompt.
    pub fn enter(&mut self, phase: TurnPhase) {
        if self.phase == phase {
            return;
        }
        let now = std::time::Instant::now();
        if self.phase == TurnPhase::Reading
            && matches!(phase, TurnPhase::Thinking | TurnPhase::Writing)
        {
            self.last_prefill = Some(now.duration_since(self.phase_since));
        }
        self.phase = phase;
        self.phase_since = now;
        self.last_event = now;
    }

    /// Record a streamed answer token.
    pub fn note_token(&mut self, token: &str) {
        self.enter(TurnPhase::Writing);
        self.first_token_at
            .get_or_insert_with(std::time::Instant::now);
        self.streamed_chars += token.len();
    }

    /// Approximate output rate (≈4 characters per token), once there is
    /// enough of a sample to mean something.
    pub fn tokens_per_sec(&self, now: std::time::Instant) -> Option<u32> {
        let secs = now.duration_since(self.first_token_at?).as_secs_f64();
        (secs >= 0.5 && self.streamed_chars >= 16)
            .then(|| ((self.streamed_chars as f64 / 4.0) / secs).round() as u32)
    }

    /// Quiet for too long *on the model's side*. Waiting on the user is never
    /// a stall, and neither is a tool that is running (it has its own
    /// progress); reading a prompt gets the longer [`READING_STALL_AFTER`].
    pub fn is_stalled(&self, now: std::time::Instant) -> bool {
        let quiet = now.duration_since(self.last_event);
        match self.phase {
            TurnPhase::Thinking | TurnPhase::Writing => quiet >= TURN_STALL_AFTER,
            TurnPhase::Reading => quiet >= READING_STALL_AFTER,
            TurnPhase::Tool { .. } | TurnPhase::AwaitingYou => false,
        }
    }
}

/// The one-line, always-current account of a running turn shown under the
/// transcript: what is happening, for how long, and — when the numbers justify
/// it — what the user could do about it.
///
/// `prompt_tokens` is the best available size of what the model is reading and
/// `read_rate` the model's measured reading speed (tokens/second), when known.
pub fn turn_status_text(
    turn: &ChatTurn,
    now: std::time::Instant,
    model: &str,
    prompt_tokens: u32,
    read_rate: Option<f64>,
) -> (String, Option<String>) {
    let in_phase = now.duration_since(turn.phase_since);
    let clock = fmt_duration_short(in_phase);
    match &turn.phase {
        TurnPhase::Reading => {
            let mut text = if prompt_tokens > 0 {
                format!(
                    "{model} is reading {} tokens of context · {clock}",
                    fmt_tokens_short(prompt_tokens)
                )
            } else {
                format!("waiting for {model} · {clock}")
            };
            let expected = read_rate
                .filter(|r| *r > 0.0 && prompt_tokens > 0)
                .map(|r| std::time::Duration::from_secs_f64(f64::from(prompt_tokens) / r));
            if let Some(expected) = expected {
                match expected.checked_sub(in_phase) {
                    Some(left) if left.as_secs() >= 1 => {
                        text.push_str(&format!(" · ~{} left", fmt_duration_short(left)));
                    }
                    _ => text.push_str(" · taking longer than last time"),
                }
            }
            let slow = read_rate.is_some_and(|r| r < 200.0) || in_phase.as_secs() >= 60;
            let hint = (slow && prompt_tokens >= 20_000).then(|| {
                "large context for this model — /compact to shrink it, or /model for a faster one"
                    .to_string()
            });
            (text, hint)
        }
        TurnPhase::Thinking => (format!("{model} is thinking · {clock}"), None),
        TurnPhase::Writing => {
            let rate = turn
                .tokens_per_sec(now)
                .map(|r| format!(" · {r} tok/s"))
                .unwrap_or_default();
            (format!("{model} is writing{rate}"), None)
        }
        TurnPhase::Tool { name } => (format!("running {name} · {clock}"), None),
        TurnPhase::AwaitingYou => (format!("waiting for your answer above · {clock}"), None),
    }
}

/// `37k`, `850` — the size style of the status line.
fn fmt_tokens_short(tokens: u32) -> String {
    if tokens >= 1_000 {
        format!("{}k", (tokens + 500) / 1_000)
    } else {
        tokens.to_string()
    }
}

/// `42s`, `3m05s`, `1h02m` — short, and stable in width within a unit.
fn fmt_duration_short(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// State of the turn's streaming liveness indicator. Each state maps to a
/// directional panel pattern (see [`crate::liveness`]): thinking shimmers
/// (nondeterministic exploration), streaming rains (the answer pouring down),
/// a dispatched tool call scrolls right (data flowing out to the tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LivenessState {
    #[default]
    Idle,
    Thinking,
    Streaming,
    /// A tool call is dispatched and the turn is waiting on its result.
    ToolWait,
}

impl LivenessState {
    /// The panel pattern this state animates with.
    pub fn pattern(self) -> crate::liveness::PanelPattern {
        use crate::liveness::PanelPattern;
        match self {
            Self::Streaming => PanelPattern::Rain,
            Self::ToolWait => PanelPattern::ScrollRight,
            Self::Idle | Self::Thinking => PanelPattern::Shimmer,
        }
    }
}

// ─── Chat history ─────────────────────────────────────────────────────────────

/// A single entry in the chat history.
#[derive(Debug, Clone)]
pub enum ChatEntry {
    /// A message submitted by the user.
    User {
        text: String,
        /// When set, this — not `text` — is what the LLM receives for this turn.
        /// `text` stays what the pane displays. Used by `/skill` invocations
        /// (SPEC R-SK8): the pane shows the typed command while the model gets
        /// the full SKILL.md instructions, on this and every later turn.
        payload: Option<String>,
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
    /// ahma talking to the user about the conversation — a retry, a turn's
    /// cost — shown dim in the transcript and **never** sent to the model.
    Notice { text: String },
}

/// Ring buffer of chat history entries (capped at `CHAT_HISTORY_CAP`).
///
/// `entries` is private on purpose: every mutation goes through a method, and
/// every method bumps `generation`. The renderer caches the wrapped transcript
/// rows keyed on this counter (see `AppState::chat_rows_cache`), so a mutation
/// path that bypassed the bump would serve stale chat — keep the field private.
#[derive(Debug, Default)]
pub struct ChatHistory {
    entries: VecDeque<ChatEntry>,
    /// Bumped on every mutation; cache-invalidation key for derived render data.
    generation: u64,
}

impl ChatHistory {
    pub fn push(&mut self, entry: ChatEntry) {
        if self.entries.len() >= CHAT_HISTORY_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn entries(&self) -> &VecDeque<ChatEntry> {
        &self.entries
    }

    /// Monotonic change counter: unchanged generation ⇒ unchanged entries.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.generation = self.generation.wrapping_add(1);
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
        self.generation = self.generation.wrapping_add(1);
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
            self.generation = self.generation.wrapping_add(1);
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
            self.generation = self.generation.wrapping_add(1);
        } else {
            self.push(ChatEntry::Thinking {
                content: token.to_string(),
                streaming: true,
            });
        }
    }

    /// End the turn: every assistant reply and thinking block stops streaming,
    /// so live cursors collapse and `collect_chat_history` sends the text back
    /// to the model next turn. The empty placeholder pushed at submit is
    /// dropped if no text ever landed in it.
    pub fn finish_stream(&mut self) {
        self.seal_streaming();
        self.entries.retain(|entry| {
            !matches!(entry, ChatEntry::Assistant { content, streaming: false } if content.is_empty())
        });
        self.generation = self.generation.wrapping_add(1);
    }

    /// Mark every streaming assistant/thinking entry as finished.
    fn seal_streaming(&mut self) {
        for entry in self.entries.iter_mut() {
            if let ChatEntry::Assistant { streaming, .. } | ChatEntry::Thinking { streaming, .. } =
                entry
            {
                *streaming = false;
            }
        }
    }

    /// Locate the latest `User` entry and set its `duration_ms` based on `started_at` elapsed time.
    pub fn finish_user_timing(&mut self) {
        let latest_user = self.entries.iter_mut().rev().find_map(|entry| match entry {
            ChatEntry::User {
                started_at,
                duration_ms,
                ..
            } => Some((started_at, duration_ms)),
            _ => None,
        });
        // Nothing to time: no user turn yet, already timed, or never started.
        let Some((started_at, duration_ms)) = latest_user else {
            return;
        };
        if duration_ms.is_some() {
            return;
        }
        let Some(start) = started_at else {
            return;
        };
        *duration_ms = Some(start.elapsed().as_millis() as u64);
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Keep only the last `keep_turns` turns (a turn is a user message and
    /// everything after it) and drop the rest, so the model really sees less
    /// on the next turn. Returns how many turns were dropped.
    ///
    /// This used to drop only tool-call rows, which `collect_chat_history`
    /// never sends anyway, while reporting that the context was compacted.
    pub fn compact(&mut self, keep_turns: usize) -> usize {
        let user_positions: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, ChatEntry::User { .. }))
            .map(|(i, _)| i)
            .collect();
        let dropped = user_positions.len().saturating_sub(keep_turns);
        let cut = match keep_turns {
            0 => self.entries.len(),
            _ => user_positions.get(dropped).copied().unwrap_or(0),
        };
        self.entries.drain(..cut);
        self.generation = self.generation.wrapping_add(1);
        dropped
    }

    pub fn start_tool_call(&mut self, id: String, name: String, args: String) {
        self.seal_streaming();
        self.push(ChatEntry::ToolCall {
            id,
            name,
            args,
            result: None,
            failed: false,
        });
    }

    pub fn finish_tool_call(&mut self, id: &str, result: String, failed: bool) {
        let mut closed = false;
        for entry in self.entries.iter_mut().rev() {
            if let ChatEntry::ToolCall {
                id: entry_id,
                result: entry_result @ None,
                failed: entry_failed,
                ..
            } = entry
                && entry_id == id
            {
                *entry_result = Some(result);
                *entry_failed = failed;
                self.generation = self.generation.wrapping_add(1);
                closed = true;
                break;
            }
        }
        // Once the last pending call is back the model is working again: give
        // the liveness pulse a live reply to ride on until its next token.
        let pending = self
            .entries
            .iter()
            .any(|e| matches!(e, ChatEntry::ToolCall { result: None, .. }));
        if closed && !pending {
            self.push(ChatEntry::Assistant {
                content: String::new(),
                streaming: true,
            });
        }
    }
}

/// Cached physical rows of the chat transcript, so an idle redraw tick does not
/// re-format and re-wrap the whole history (O(total transcript chars) per frame
/// otherwise — the single biggest per-frame allocation in the TUI).
///
/// `key` records the inputs the rows were built from: chat generation, wrap
/// width, and unicode mode. `None` marks the rows as valid for this frame only —
/// used while the transcript renders wall-clock/animation content (streaming
/// cursor, liveness glyph, live elapsed time) that no key can capture.
#[derive(Default)]
pub struct ChatRowsCache {
    pub key: Option<(u64, usize, bool)>,
    pub rows: Vec<ratatui::text::Line<'static>>,
    /// The transcript entry each row belongs to, so a click on a row can open
    /// that entry in full.
    pub owners: Vec<usize>,
    /// First visible row as last drawn, for mapping a click to a row.
    pub scroll: usize,
}

// ─── Command navigator ────────────────────────────────────────────────────────

/// A command available in the `/` navigator.
#[derive(Debug, Clone)]
pub struct NavCommand {
    /// The full command string, e.g. `/help`.
    pub command: String,
    /// Short description shown in the picker.
    pub description: String,
}

/// The complete sandbox scope as reported by the server in
/// `notifications/sandbox/configured` (SPEC R5.4: scope is always visible with
/// provenance). This is the TUI-side mirror of the server's `ScopeView` JSON —
/// the one canonical representation every surface renders.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxScopeInfo {
    /// Directories the AI may write to (display strings, server-canonicalized).
    pub write: Vec<String>,
    /// Read-only directories granted beyond the write roots.
    pub read: Vec<String>,
    /// Whether the system temp directory was added via `--tmp`.
    pub tmp: bool,
    /// Whether kernel enforcement is active (`false` == `--no-sandbox`).
    pub enforced: bool,
    /// Provenance of the scope: `explicit` | `roots/list` | `elicited` | `container`.
    pub source: Option<String>,
    /// Active-sandbox token: `ahma` | `ahma_nested_in_host` | `deferred_to_host` | `disabled`.
    pub active: Option<String>,
    /// Detected host sandbox label (Cursor, Claude Code, …), when nested/deferred.
    pub host: Option<String>,
    /// The loud R7 disclosure line, when ahma is not the sole authority.
    pub disclosure: Option<String>,
    /// Platform limitation that cannot be expressed as scope (e.g. macOS
    /// reads-unconfined, SPEC R-PERM.5.1). Must be shown, not buried.
    pub platform_note: Option<String>,
}

/// Every slash command the TUI advertises, as `(command, description)`.
///
/// The single list. `builtin_commands()` (the `/` palette) maps it, and
/// `ui::help_rows_reference_only_known_commands` checks the help overlay against
/// it, so a command cannot be dispatched, listed and documented from three
/// hand-kept copies that drift — which is exactly what had happened: `/analyze`
/// and `/compact` were dispatched and in the help but missing from the palette,
/// and `/provider add` / `/provider numctx` were in neither, reachable only by
/// typing them exactly.
///
/// `/exit` is deliberately absent: it is handled but not advertised.
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show keyboard reference"),
    ("/?", "show keyboard reference (alias)"),
    ("/setup", "connect an LLM, step by step (alias /connect)"),
    ("/provider", "select LLM provider"),
    ("/model", "select model for current provider"),
    (
        "/minimize on",
        "enable token minimization (concise prompts, compressed output)",
    ),
    ("/minimize off", "disable token minimization (default)"),
    ("/mcp on", "enable ahma as MCP tool server"),
    ("/mcp off", "disable ahma MCP tool server"),
    ("/mcp list", "list configured MCP client servers"),
    ("/mcp refresh", "refresh tools from configured MCP servers"),
    ("/mcp add http <url> [name]", "add an HTTP MCP server"),
    (
        "/mcp add stdio <cmd> [args] [--name <n>]",
        "add a stdio MCP server",
    ),
    ("/mcp remove <name>", "remove a configured MCP server"),
    (
        "/run <tool> {json}",
        "invoke an ahma tool directly with optional JSON args",
    ),
    ("/tools", "list available ahma tools"),
    ("/skills", "list Agent Skills invocable with /<name>"),
    ("/tasks", "focus the work view"),
    ("/chat", "open or close the chat pane"),
    ("/log", "open log view window"),
    ("/scope", "show the locked sandbox scope and its provenance"),
    (
        "/log file <path> [prompt]",
        "start background log monitor on file",
    ),
    ("/approve", "approve pending gate"),
    ("/reject", "reject pending gate"),
    ("/clear", "clear chat & finished windows (logs kept)"),
    (
        "/resume",
        "bring back this window's saved conversation (~/.ahma/transcripts)",
    ),
    ("/agent list", "list saved agent profiles"),
    (
        "/agent save <name>",
        "save current setup as an agent profile",
    ),
    ("/agent load <name>", "load an agent profile"),
    ("/agent delete <name>", "delete an agent profile"),
    ("/export markdown", "export chat transcript to markdown"),
    ("/intro", "ahma in one screen — Enter on a line for more"),
    ("/getting-started", "same as /intro"),
    (
        "/settings [search]",
        "every setting: model, trust & access, tools, sandbox — [search] jumps to a row",
    ),
    (
        "/sync",
        "tool calls wait for their result (default; saved to settings)",
    ),
    (
        "/async",
        "tool calls return an id, collect with await (saved to settings)",
    ),
    ("/analyze [op_id]", "ask the LLM to analyze an operation"),
    (
        "/compact",
        "keep only the last 4 turns (the model sees less)",
    ),
    ("/provider add", "add a provider to the registry"),
    (
        "/provider numctx",
        "set the context length for the current provider",
    ),
    ("/quit", "quit the application"),
    // /exit intentionally omitted — still handled, just not advertised
];

pub fn builtin_commands() -> Vec<NavCommand> {
    SLASH_COMMANDS
        .iter()
        .map(|(command, description)| NavCommand {
            command: (*command).into(),
            description: (*description).into(),
        })
        .collect()
}

/// Advance `selected` by one step through a list of length `len`, wrapping
/// around at either end. Returns `selected` unchanged when `len == 0` (empty
/// list has no valid index to move to).
fn wrapping_step(selected: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return selected;
    }
    if forward {
        (selected + 1) % len
    } else {
        selected.checked_sub(1).unwrap_or(len - 1)
    }
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
    /// `/name` entries for discovered Agent Skills (SPEC R-SK8), captured when
    /// the navigator opens so keystroke filtering does not re-scan the disk.
    pub skill_commands: Vec<NavCommand>,
}

impl CommandNavigator {
    /// Build a freshly-opened navigator with completions seeded from `tools`
    /// and discovered Agent Skills.
    /// Visibility is owned by [`ModalState`], not this struct (SPEC R23).
    pub fn opened(tools: &[String], skills: Vec<NavCommand>) -> Self {
        let mut nav = CommandNavigator {
            skill_commands: skills,
            ..CommandNavigator::default()
        };
        nav.refresh_completions(tools);
        nav
    }

    /// Does `cmd` match the palette query? `query` must already be lowercased.
    /// Matching is a substring test over both the command and its description,
    /// plus one alias: `/quit` also answers to a prefix of "exit".
    fn command_matches(cmd: &NavCommand, query: &str) -> bool {
        let query_no_slash = query.trim_start_matches('/');
        let is_exit_alias = cmd.command == "/quit"
            && !query_no_slash.is_empty()
            && "exit".starts_with(query_no_slash);
        cmd.command.to_lowercase().contains(query)
            || cmd.description.to_lowercase().contains(query)
            || is_exit_alias
    }

    /// Rebuild completions from builtins + dynamic `/run <tool>` and skill
    /// entries.
    pub fn refresh_completions(&mut self, tools: &[String]) {
        let mut cmds = builtin_commands();
        cmds.extend(self.skill_commands.iter().cloned());
        // Add a `/run <tool>` entry for every known ahma tool.
        for t in tools {
            cmds.push(NavCommand {
                command: format!("/run {t}"),
                description: "run ahma tool".into(),
            });
        }

        if self.input.is_empty() {
            self.completions = cmds;
        } else {
            let query = self.input.to_lowercase();
            self.completions = cmds
                .into_iter()
                .filter(|c| Self::command_matches(c, &query))
                .collect();
        }
        self.selected = self.selected.min(self.completions.len().saturating_sub(1));
    }

    pub fn select_next(&mut self) {
        self.selected = wrapping_step(self.selected, self.completions.len(), true);
    }

    pub fn select_prev(&mut self) {
        self.selected = wrapping_step(self.selected, self.completions.len(), false);
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
        self.selected = wrapping_step(self.selected, self.filtered_items().len(), true);
    }

    pub fn select_prev(&mut self) {
        self.selected = wrapping_step(self.selected, self.filtered_items().len(), false);
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
    /// The unified work view — what the TUI opens into (SPEC R24.9). It is the
    /// answer to "what is happening on my behalf", which is what someone opens
    /// this window to find out; chat is a thing you then choose to do.
    #[default]
    Work,
    /// The chat pane, when it is open.
    Chat,
    Log,
}

impl Focus {
    fn active_order(chat_open: bool, log_open: bool) -> Vec<Self> {
        let mut order = vec![Self::Work];
        if chat_open {
            order.push(Self::Chat);
        }
        if log_open {
            order.push(Self::Log);
        }
        order
    }

    /// Cycle through active panels.
    pub fn cycle_next_active(self, tasks_open: bool, log_open: bool) -> Self {
        let order = Self::active_order(tasks_open, log_open);
        let pos = order.iter().position(|&f| f == self).unwrap_or(0);
        order[(pos + 1) % order.len()]
    }

    pub fn cycle_prev_active(self, tasks_open: bool, log_open: bool) -> Self {
        let order = Self::active_order(tasks_open, log_open);
        let pos = order.iter().position(|&f| f == self).unwrap_or(0);
        order[(pos + order.len() - 1) % order.len()]
    }

    /// Panes that can be maximised to the full screen with `z`.
    pub fn is_zoomable(self) -> bool {
        // The work view is the screen, so there is nothing to zoom it *from*;
        // the log pane is the one thing worth filling the terminal with.
        matches!(self, Self::Log)
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

// ─── Operation events (monitor activity feed) ────────────────────────────────

/// Ring capacity for the monitor activity feed.
const EVENT_RING_CAP: usize = 200;

/// One entry in the monitor activity feed: an operation started or reached a
/// terminal state on any connected instance. Rows are clickable — they open
/// the operation's full-screen detail view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpEvent {
    pub timestamp: chrono::DateTime<chrono::Local>,
    pub kind: OpEventKind,
    pub op_id: String,
    /// Human title at the time of the event (`Operation::display_name`).
    pub title: String,
    pub instance: Option<String>,
    /// Set on `Finished` events when the runner reported a duration.
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpEventKind {
    Started,
    Finished(OpStatus),
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
    /// Refused by the sandbox rather than failed on its own terms. A denial is
    /// a first-class outcome, not a flavour of failure (SPEC R-PERM.7): the row
    /// says which path was refused, and the user can re-raise the grant
    /// question from it (R-PERM.7.1).
    Denied,
    /// Still running when the daemon watching it went away, and reconstructed
    /// from the history file at the next start. Distinct from `Failed`: the
    /// command may well have succeeded, and claiming it failed would be an
    /// invention (SPEC R-DAEMON.7).
    Interrupted,
}

impl OpStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Denied | Self::Interrupted
        )
    }

    /// The status word as it appears on the hub wire — the vocabulary the
    /// identity line falls back to when there is no exit code to show.
    pub fn wire_word(&self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Pending => "Pending",
            Self::Succeeded => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Waiting => "Waiting",
            // A denial and an interruption travel the wire as a failure plus a
            // field, so the wire word stays "Failed" for pre-upgrade readers
            // (R24.5).
            Self::Denied | Self::Interrupted => "Failed",
        }
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
                Self::Denied => "✗",
                // Not a cross: nobody established that this failed.
                Self::Interrupted => "⁉",
            }
        } else {
            match self {
                Self::Running => ">",
                Self::Pending => ".",
                Self::Succeeded => "v",
                Self::Failed => "x",
                Self::Cancelled => "-",
                Self::Waiting => "|",
                Self::Denied => "x",
                Self::Interrupted => "?",
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
    /// The human title, computed **server-side** and carried on the wire
    /// (SPEC R24.7). When present it is authoritative — the legacy parse chain
    /// below is only for events from a pre-R24.7 server.
    pub title: Option<String>,
    /// The full command, for the detail pane.
    pub command: Option<String>,
    /// Which attached session initiated this work (`cursor`, `tui`, `hook`, …).
    pub origin: Option<String>,
    /// Process exit code, when the runner reported one.
    pub exit_code: Option<i64>,
    /// The path and access a sandbox denial refused, when `status` is
    /// [`OpStatus::Denied`]. Held so the row can name the path and the grant
    /// question can be re-raised for exactly that pair (SPEC R-PERM.7.1).
    pub denial: Option<(String, ahma_common::config::ScopeAccess)>,
    /// This row was reconstructed from its terminal event alone: the hub never
    /// saw the operation start (it aged out, or the daemon restarted mid-run).
    /// Its outcome is real; its command and working directory are unknown.
    pub partial: bool,
    /// When the most recent live output line arrived (set locally, not from
    /// the wire). Drives the fast-vs-slow cadence of the card's activity panel.
    pub last_output_at: Option<Instant>,
    /// The command ran **outside** the kernel sandbox, at the user's full
    /// privilege — today only a `!` command someone typed into this TUI
    /// (SPEC R-DAEMON.9). Drawn as such: a unified view in which the one
    /// unconfined row looks like all the others is the wrong view.
    pub unsandboxed: bool,
}

/// Strategy 1: pull `command` out of an embedded JSON blob in the description.
fn parse_command_from_json(description: &str) -> Option<String> {
    let start_idx = description.find('{')?;
    let end_idx = description.rfind('}')?;
    if start_idx >= end_idx {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(&description[start_idx..=end_idx]).ok()?;
    val.get("command")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Strategy 2: parse the `Execute <cmd> in <cwd>` sentence form.
fn parse_command_from_execute_sentence(description: &str) -> Option<String> {
    if !description.starts_with("Execute ") {
        return None;
    }
    let in_idx = description.rfind(" in ")?;
    let cmd = &description["Execute ".len()..in_idx];
    if cmd.is_empty() {
        None
    } else {
        Some(cmd.to_string())
    }
}

/// Strategy 3: derive a command-ish label from an `op_<n>_<words>` id.
fn parse_command_from_op_id(id: &str) -> Option<String> {
    if !id.starts_with("op_") {
        return None;
    }
    let parts: Vec<&str> = id.split('_').collect();
    if parts.len() >= 3 && parts[1].chars().all(|c| c.is_ascii_digit()) {
        Some(parts[2..].join(" ").replace('_', " "))
    } else {
        None
    }
}

fn try_parse_run_terminal_command(description: &str, id: &str) -> Option<String> {
    parse_command_from_json(description)
        .or_else(|| parse_command_from_execute_sentence(description))
        .or_else(|| parse_command_from_op_id(id))
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
            denial: None,
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
            title: None,
            command: None,
            origin: None,
            exit_code: None,
            last_output_at: None,
            partial: false,
            unsandboxed: false,
        }
    }

    /// Format a millisecond count as `<n>ms` below one second, else `<n>s`.
    fn format_ms_duration(ms: u128) -> String {
        if ms < 1000 {
            format!("{ms}ms")
        } else {
            format!("{}s", ms / 1000)
        }
    }

    pub fn elapsed_display(&self) -> String {
        if let Some(ms) = self.duration_ms {
            return Self::format_ms_duration(u128::from(ms));
        }
        if let (Some(start), Some(end)) = (self.started_at, self.completed_at) {
            let d = end.saturating_duration_since(start);
            return Self::format_ms_duration(d.as_millis());
        }
        let secs = self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        format!("{secs}s")
    }

    /// The name a human reads for this operation.
    ///
    /// The server now computes this and sends it (`title`, SPEC R24.7), because
    /// only the server knows what command it ran. Everything below the first
    /// branch is a **fallback for pre-R24.7 servers** — the old guessing chain
    /// that parsed a command out of JSON, then out of an "Execute …" sentence,
    /// then out of the operation *id*, which is how rows ended up reading
    /// `op_41_echo_hello`. Keep it until the wire-compat window closes; do not
    /// extend it. Guessing is what we are getting rid of.
    pub fn display_name(&self) -> String {
        if let Some(title) = &self.title
            && !title.trim().is_empty()
        {
            return title.clone();
        }
        if self.tool_name == "run_terminal_command"
            && let Some(cmd) = try_parse_run_terminal_command(&self.description, &self.id)
        {
            return cmd;
        }
        self.tool_name.clone()
    }

    /// This operation as the one canonical identity (SPEC R24.7), ready to render
    /// identically in chat, monitor rows, grant prompts, and log names.
    pub fn identity(&self) -> ahma_common::op_identity::OpIdentity<'_> {
        use ahma_common::op_identity::{OpIdentity, OpOutcome};

        let outcome = match self.status {
            OpStatus::Running | OpStatus::Pending => OpOutcome::Running {
                elapsed_secs: self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0),
            },
            // "denied: /path" says what happened and what to do about it;
            // "failed" says neither (SPEC R24.7, R-PERM.7).
            OpStatus::Denied => OpOutcome::Denied {
                reason: match &self.denial {
                    Some((path, access)) => {
                        format!("{path} ({}) outside sandbox scope", access.short())
                    }
                    None => "outside sandbox scope".to_string(),
                },
            },
            _ => OpOutcome::Finished {
                exit_code: self.exit_code,
                status: self.status.wire_word().to_string(),
                duration_ms: self.duration_ms.unwrap_or(0),
            },
        };

        OpIdentity {
            title: self.title_ref(),
            cwd: self.cwd.as_deref(),
            origin: self.origin.as_deref(),
            outcome,
        }
    }

    /// Borrow the title without allocating when the wire supplied one.
    fn title_ref(&self) -> &str {
        match &self.title {
            Some(t) if !t.trim().is_empty() => t,
            _ => &self.tool_name,
        }
    }

    pub fn clean_id(&self) -> String {
        // `op_<n>_...` / `op-<n>-...` ids collapse to just the numbered prefix.
        let sep = match self.id.get(..3) {
            Some("op_") => Some('_'),
            Some("op-") => Some('-'),
            _ => None,
        };
        if let Some(sep) = sep
            && let Some(number) = self.id.split(sep).nth(1)
            && number.chars().all(|c| c.is_ascii_digit())
        {
            return format!("op{sep}{number}");
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
    /// The workspace an "always allow" is persisted under — the one the asking
    /// agent checks, which is not necessarily this TUI's working directory.
    pub workspace: String,
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
            workspace: String::new(),
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
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = workspace.into();
        self
    }

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

/// Which operation an action is about. Operation ids are unique only within
/// the instance that ran them, so an id alone can name another client's
/// operation: cancel, pin and detail must carry the instance too.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpKey {
    pub id: String,
    /// `None` when the source did not say (the TUI's own `!` windows, the
    /// poll path); such a key falls back to the first operation with the id.
    pub instance_id: Option<String>,
}

impl OpKey {
    pub fn of(op: &Operation) -> Self {
        Self {
            id: op.id.clone(),
            instance_id: op.instance_id.clone(),
        }
    }

    /// A key for an id whose instance is not known.
    pub fn bare(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            instance_id: None,
        }
    }

    /// Index of the operation this key names in `ops`.
    pub fn position_in(&self, ops: &[Operation]) -> Option<usize> {
        let exact = ops
            .iter()
            .position(|o| o.id == self.id && o.instance_id == self.instance_id);
        match &self.instance_id {
            Some(_) => exact,
            None => exact.or_else(|| ops.iter().position(|o| o.id == self.id)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickTarget {
    CancelOperation(OpKey),
    PinOperation(OpKey),
    AnalyzeOperation(OpKey),
    SelectOperation(usize),
    CloseWindow(usize),
    ToggleWindow(usize),
    /// A row of the monitor task tree: click selects it and toggles it
    /// (accordion expand for ops, collapse for instance/session headers).
    TreeRow(usize),
    /// A section header in the work view: click opens that section and closes
    /// whichever was open (SPEC R24.9).
    SectionHeader(String),
    /// Open the full-screen detail view for this operation.
    OpenOperationDetail(OpKey),
    /// Open the full-screen detail view for one log line. The text is captured
    /// at draw time because the log is re-derived (and re-filtered) every frame,
    /// so a row index would not survive until the click is handled.
    OpenLogLine(String),
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
    /// The two-level `/intro` tour: the highlighted topic, and whether its
    /// second level is open.
    Intro { selected: usize, expanded: bool },
    /// The `/` command navigator.
    Navigator(CommandNavigator),
    /// The inline provider picker.
    ProviderPicker(PickerState),
    /// The inline model picker.
    ModelPicker(PickerState),
    /// The log-file switcher, with the highlighted row index.
    LogFiles { selected: usize },
    /// Full-screen drill-in for one operation (Enter or click on it).
    OperationDetail(OperationDetailState),
    /// Full-screen drill-in for one log line (click on it). The log pane does
    /// not wrap by default, so a long line is truncated at the pane edge with
    /// no way to read the rest; this shows it wrapped and scrollable.
    LogLineDetail(LogLineDetailState),
    /// Step 1 of LLM setup wizard: select provider.
    LlmSetupProvider(PickerState),
    /// Step 2 of LLM setup wizard: pick one of the models the provider listed
    /// (the listing was also the connection test).
    LlmSetupModel {
        picker: PickerState,
        provider_name: String,
        base_url: String,
    },
}

/// A cloud provider the setup wizard registers in `~/.ahma/config.toml` once a
/// model is chosen. The key is stored as a `${VAR}` reference, never the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudSetup {
    pub config_name: &'static str,
    pub kind: ahma_common::config::ProviderKind,
    pub key_env: &'static str,
}

/// A provider chosen in wizard step 1, waiting for its model list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPending {
    pub provider_name: String,
    pub base_url: String,
    pub cloud: Option<CloudSetup>,
    /// The listing arrived and the model picker is open.
    pub connected: bool,
}

/// State of the full-screen log-line detail overlay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogLineDetailState {
    /// The full text of the line, captured at click time.
    pub text: String,
    /// Scroll offset in rendered rows; clamped at draw time via
    /// [`AppState::detail_max_scroll`].
    pub scroll: usize,
}

/// State of the full-screen operation detail overlay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperationDetailState {
    /// Id of the operation being inspected.
    pub op_id: String,
    /// Its instance, so the overlay and its keys act on that operation and
    /// not on another instance's operation with the same id.
    pub instance_id: Option<String>,
    /// Scroll offset into the rendered lines; clamped at draw time via
    /// [`AppState::detail_max_scroll`].
    pub scroll: usize,
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

/// What a window content line *is*, rather than what it happens to start with.
///
/// The window used to be a `Vec<String>`, so two consumers recovered the
/// structure by prefix-matching the rendered text — with different,
/// non-overlapping lists. `output_line_style` keyed off "Starting",
/// "Finished successfully", "Failed", "Cancelled", "──", "--";
/// `last_output_line` keyed off `!= "____"`, "Starting ", "Started ", "──",
/// "--". Neither knew about "Finished" or "Denied", and a stdout line that
/// merely began with "Failed" was painted as a terminal failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    /// The `Starting <tool> at <time>` header.
    Start,
    /// A line of the command's own stdout/stderr.
    Output,
    /// The live-edge marker shown while the operation is still running.
    LiveEdge,
    /// The rule drawn between the output and the outcome line.
    Separator,
    /// The terminal outcome line, carrying the status it reports so the style
    /// follows the outcome rather than the wording.
    End(WindowStatus),
}

/// One line of a window's content, with its role attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLine {
    pub kind: LineKind,
    pub text: String,
}

impl WindowLine {
    pub fn start(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Start,
            text: text.into(),
        }
    }
    pub fn output(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Output,
            text: text.into(),
        }
    }
    /// The marker drawn at the live edge of a running operation's output.
    pub fn live_edge() -> Self {
        Self {
            kind: LineKind::LiveEdge,
            text: "____".to_string(),
        }
    }
    pub fn separator(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Separator,
            text: text.into(),
        }
    }
    pub fn end(status: WindowStatus, text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::End(status),
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TuiWindow {
    pub id: usize,
    pub label: String,
    pub status: WindowStatus,
    pub content: Vec<WindowLine>,
    pub collapsed: bool,
    pub finished_at: Option<std::time::Instant>,
    /// Wall-clock duration of the finished operation, for the collapsed
    /// one-line summary. `None` while running or when the runner did not
    /// report one.
    pub duration_ms: Option<u64>,
    /// Mirrors the backing operation's `last_output_at` — the card's activity
    /// panel animates fast while output is arriving, slow when quiet.
    pub last_output_at: Option<std::time::Instant>,
    pub is_cli: bool,
    pub command: String,
    pub working_dir: String,
    pub llm_model: Option<String>,
    pub visible: bool,
    pub abort_tx: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    pub op_id: Option<String>,
}

impl TuiWindow {
    /// The most recent real output line — the collapsed running row shows it as
    /// a live "what is it doing" tail. Skips the live-edge marker, separators,
    /// and the friendly start line, which carry no activity information.
    pub fn last_output_line(&self) -> Option<&str> {
        self.content
            .iter()
            .rev()
            .filter(|l| l.kind == LineKind::Output)
            .map(|l| l.text.trim())
            .find(|t| !t.is_empty())
    }
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

/// Top-level application state — owns all panel data and UI mode.
pub struct AppState {
    pub windows: Vec<TuiWindow>,
    pub next_window_id: usize,
    /// Set by `/clear`. Operations that had already completed before this
    /// instant are not re-materialised as windows when a source re-pushes them,
    /// so cleared results stay cleared.
    pub cleared_at: Option<Instant>,
    pub window_rects: std::cell::RefCell<Vec<(usize, Rect)>>,

    // ── Connection ──
    pub server_url: String,
    pub mcp_http_base_url: String,
    pub transport_label: String,
    pub server_healthy: bool,
    pub daemon_healthy: bool,
    pub session_id: Option<String>,
    pub sandbox_status: SandboxAuthority,
    pub workspace: String,
    /// The currently targeted instance or window for chat (SPEC R24, R24.9).
    pub active_target_instance: Option<String>,
    /// Last-used LLM configurations per window/session.
    pub window_llms: HashMap<String, crate::session_config::WindowLlmConfig>,
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
    /// Monitor activity feed: op started/finished transitions, newest first.
    pub events: VecDeque<OpEvent>,
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
    /// Measured prompt-reading speed (tokens/second) per model label, from
    /// this session's own turns. Feeds the "~2m left" estimate.
    pub read_rates: std::collections::HashMap<String, f64>,
    /// Automatic retries spent on the current message (see
    /// `app::end_turn_with_error`); reset when the user sends a new one.
    pub turn_retries: u32,
    /// Recently chosen models ahma runs itself, most recent first (persisted in
    /// `.ahma/session.toml`): the fallback when a client's own model goes away.
    pub recent_llms: Vec<crate::session_config::WindowLlmConfig>,
    /// The client model chat was moved off because its client disconnected:
    /// (that selection, its `mcp://` URL, the stand-in chosen for it). Lets chat
    /// move back when the client returns, unless the user chose since.
    pub displaced_client_model: Option<(LlmSelection, String, LlmSelection)>,
    /// The one-time "trust this folder?" question (SPEC R-PERM.1.3), holding
    /// the canonical folder it is about. `None` once answered, or when the
    /// folder is already trusted or may never be (home, a root).
    pub trust_prompt: Option<String>,
    pub tools_list: Vec<crate::mcp_connections::ToolInfo>,
    pub mcp_connections: McpConnectionManager,

    // ── Windows & Panels ──
    pub tasks_window_open: bool,
    pub log_window_open: bool,
    /// The `/scope` sub-window showing the locked sandbox scope (SPEC R5.4(b)).
    pub scope_window_open: bool,
    /// Scroll offset of the help overlay, in rendered rows. The overlay is
    /// size-capped, so without this its tail — including the section that
    /// documents the log pane — was unreachable on ordinary terminals, in
    /// breach of the honest-panes rule it exists to describe (SPEC R24.8).
    pub help_scroll: u16,
    /// Complete scope + provenance from `notifications/sandbox/configured`.
    /// `None` until the server reports it (or when attached daemon-only).
    pub sandbox_scope: Option<SandboxScopeInfo>,
    /// Error text from `notifications/sandbox/failed`, kept until the next
    /// successful configuration so the reason outlives the log scrollback.
    pub sandbox_failed_reason: Option<String>,
    /// Persistent scope grants read from `~/.ahma/settings.toml`
    /// (`[sandbox] persistent_scopes`), shown in the `/scope` panel as
    /// `(path, access)`. These are the roots the user granted themselves, as
    /// distinct from the workspace the client reported — without them the
    /// panel cannot say *why* an out-of-workspace root is writable.
    pub granted_scopes: Vec<(String, String)>,
    pub chat: ChatHistory,
    /// Current text in the multi-line input box.
    pub chat_input: TextArea<'static>,
    /// What the TUI is pointed at, or `None` for "no LLM". The header label is
    /// derived from this at render time by [`AppState::llm_label`].
    pub llm_selection: Option<LlmSelection>,
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
    /// Agent Skills currently active in the chat session (SPEC R-SK8.4).
    pub active_skills: Vec<ahma_common::skills::Skill>,
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
    /// The one open section (SPEC R24.9), keyed as `work_view::Section::key`.
    pub open_section: Option<String>,
    /// The section movement in flight, if any.
    pub accordion: Option<crate::accordion::AccordionAnim>,
    /// This frame's sections and their laid-out positions, so key handling and
    /// hit-testing read exactly what was drawn (R24.8.2).
    pub work_sections: std::cell::RefCell<Vec<crate::work_view::Section>>,
    pub work_slots: std::cell::RefCell<Vec<crate::work_view::Slot>>,
    /// Total rows the work view wants, for the scrollbar.
    pub work_total_rows: std::cell::Cell<usize>,
    /// Whether the next frame should pull the selection into view.
    ///
    /// A keyboard move should; a wheel scroll must not, or the very next frame
    /// re-centres on a selection the user did not move and the wheel appears
    /// not to work.
    pub work_follow_selection: std::cell::Cell<bool>,
    /// Whether the chat pane is open (it is a toggle now, not the home view).
    pub chat_open: bool,
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
    pub chat_area: std::cell::Cell<Rect>,
    pub log_area: std::cell::Cell<Rect>,
    pub work_area: std::cell::Cell<Rect>,
    pub chat_input_area: std::cell::Cell<Rect>,
    pub last_mouse_pos: std::cell::Cell<Option<(u16, u16)>>,

    // --- Animation states ---
    pub chat_scroll_target: std::cell::Cell<f64>,
    pub chat_scroll_current: std::cell::Cell<f64>,
    pub log_scroll_target: std::cell::Cell<f64>,
    pub log_scroll_current: std::cell::Cell<f64>,
    pub chat_input_height_target: std::cell::Cell<f64>,
    pub chat_input_height_current: std::cell::Cell<f64>,
    pub chat_max_scroll: std::cell::Cell<usize>,
    /// Max scroll of the operation-detail overlay, computed at draw time from
    /// the rendered line count so key handlers can clamp (`G`, `j`).
    pub detail_max_scroll: std::cell::Cell<usize>,
    pub log_max_scroll: std::cell::Cell<usize>,
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
    /// Which pane is maximised to the full screen, if any (`z` toggles the
    /// focused pane; Esc restores). Replaces the old log-only zoom flag.
    pub zoomed: Option<Focus>,
    /// Cumulative count of log lines received — the frame counter for the log
    /// title's rain panel (fast when lines pour in, still when quiet).
    pub log_lines_total: u64,
    pub click_targets: std::cell::RefCell<Vec<(ClickTarget, Rect)>>,
    pub ops_list_state: std::cell::RefCell<ratatui::widgets::ListState>,
    /// Wrapped-transcript row cache — see [`ChatRowsCache`]. Interior mutability
    /// because it is filled during rendering, which is pure over `&AppState`.
    pub chat_rows_cache: std::cell::RefCell<ChatRowsCache>,

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
    /// Seed personalising the panel's dot pattern; fixed per session.
    pub liveness_seed: u64,
    /// Frame counter for the directional panel. Advancing it one step moves
    /// the dots one cell along the current pattern's direction — fast on real
    /// stream events, slow on the waiting heartbeat.
    pub liveness_frame: u64,
    /// When the last visible stream signal (token/thinking/tool/usage) arrived.
    /// Drives the fast-vs-slow spinner cadence. `None` until the first signal.
    pub last_stream_activity: Option<std::time::Instant>,
    /// When the slow "still waiting" spinner last advanced.
    pub last_wait_tick: Option<std::time::Instant>,

    // ── Config ──
    pub unicode: bool,
    pub should_quit: bool,
    /// The chat turn in flight, if any (see [`ChatTurn`]).
    pub turn: Option<ChatTurn>,
    /// The setup wizard's provider, while its connection is being checked.
    pub setup_pending: Option<SetupPending>,
    /// The other windows' transcripts, by section key (the current one is
    /// `chat`, belonging to `chat_key`).
    pub transcripts: HashMap<String, ChatHistory>,
    /// The section key `chat` belongs to (`""` = this terminal).
    pub chat_key: String,
    /// Messages sent from the chat input, oldest first, for ↑/↓ recall.
    pub input_history: VecDeque<String>,
    /// Where ↑/↓ recall currently is in `input_history`; `None` when editing
    /// fresh input.
    pub history_pos: Option<usize>,
    /// What was being typed before recall started, restored by ↓ past the end.
    pub history_draft: String,
    /// Token spend per window (section key; the local section for chat with
    /// no target window). Usage events carry no instance, but the TUI runs one
    /// turn at a time, so each belongs to the in-flight turn's window.
    pub window_usage: HashMap<String, WindowUsage>,
    /// When the ahma server / the daemon was last seen going down, so the
    /// header can say for how long rather than just "offline".
    pub server_down_since: Option<std::time::Instant>,
    pub daemon_down_since: Option<std::time::Instant>,
    /// `(base_url, num_ctx)` of the provider last looked up, so the header can
    /// show context fill each frame without re-reading the config file.
    pub ctx_window_cache: std::cell::RefCell<Option<(String, Option<u32>)>>,
    /// A model-list refresh the user asked for is in flight; its result may
    /// open the model picker. Unrequested refreshes never do.
    pub model_picker_requested: bool,
    /// When a quit was requested while work was running. A second request
    /// inside [`CTRL_C_QUIT_WINDOW`] quits; otherwise the first one only warns.
    pub quit_armed_at: Option<std::time::Instant>,
    /// A one-line transient hint shown in the footer (e.g. "press q again to
    /// quit"), with the moment it was raised so it can fade.
    pub footer_hint: Option<(String, std::time::Instant)>,
    /// Sender half of the bridge channel; set by app.rs after spawning.
    pub bridge_tx: Option<tokio::sync::mpsc::Sender<crate::llm_bridge::BridgeEvent>>,
    pub mcp_source_tx: Option<tokio::sync::mpsc::Sender<crate::mcp_source::McpSourceCommand>>,
    pub approval_tx: Option<tokio::sync::oneshot::Sender<bool>>,
    /// This TUI's own connection to the hub, so the `!` commands it runs are
    /// visible everywhere the rest of the work is (SPEC R-DAEMON.9). `None`
    /// only in tests, which build state without a runtime.
    pub tui_reporter: Option<crate::tui_reporter::TuiReporter>,
}

/// Which provider a selection came from.
///
/// The distinction exists because a profile is *not* a provider name: it is an
/// alias carrying a URL and a model. Rendering it as `profile:my-profile` in the
/// header is right; persisting that same text as `[agent].provider` is not, and
/// that is exactly what happened while the two were one formatted string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderRef {
    /// A provider from the registry, by name.
    Named(String),
    /// A saved profile, by alias. Its endpoint travels in
    /// `AppState::current_provider_url`; there is no registry name to persist.
    Profile(String),
}

/// The model the TUI is currently pointed at.
///
/// Replaces a formatted `"{provider} / {model}"` string with a `"no LLM"`
/// sentinel, which six sites re-parsed with `rsplit_once(" / ")` and five
/// compared against the sentinel — inconsistently, some also checking
/// `is_empty()`. Two leaks came out of that: `/mcp on` with nothing selected
/// persisted `[agent].provider = "no LLM"` into the user's global
/// `~/.ahma/settings.toml` (the file the MCP sub-agent reads), and a loaded
/// profile persisted `profile:<alias>` as a provider name no registry contains.
///
/// `None` *is* "no LLM": the sentinel comparisons become `is_none()`, and the
/// display label is produced only at the render edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmSelection {
    pub provider: ProviderRef,
    pub model: String,
}

impl LlmSelection {
    /// A selection made by picking a provider from the registry.
    pub fn named(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: ProviderRef::Named(provider.into()),
            model: model.into(),
        }
    }

    /// A selection made by loading a saved profile.
    pub fn profile(alias: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: ProviderRef::Profile(alias.into()),
            model: model.into(),
        }
    }

    /// The header label: `provider / model`, or `profile:alias / model`.
    pub fn display_label(&self) -> String {
        match &self.provider {
            ProviderRef::Named(name) => format!("{name} / {}", self.model),
            ProviderRef::Profile(alias) => format!("profile:{alias} / {}", self.model),
        }
    }

    /// The provider name as it should be *persisted*, or `""` for a profile —
    /// whose endpoint is persisted as a URL instead. Never an alias, never a
    /// display sentinel.
    pub fn persistable_provider(&self) -> &str {
        match &self.provider {
            ProviderRef::Named(name) => name,
            ProviderRef::Profile(_) => "",
        }
    }

    /// The registry provider name, when the selection has one. `None` for a
    /// profile, so callers that need a real provider (the num_ctx picker,
    /// settings persistence) cannot be handed an alias.
    pub fn provider_name(&self) -> Option<&str> {
        match &self.provider {
            ProviderRef::Named(name) => Some(name),
            ProviderRef::Profile(_) => None,
        }
    }
}

/// Who is actually enforcing the sandbox for this session, as the TUI knows it.
///
/// Previously a formatted `String` ("LOCKED", "NESTED: cursor", …) produced at
/// the source and re-interpreted downstream by prefix matching — the label was
/// the interface, so the typed token it was built from had to be recovered from
/// text. Two bugs came directly from that: the status chip's colour was decided
/// by `starts_with("NESTED")`, and the system-prompt guard compared against
/// lowercase `"unknown"` which no producer ever emitted, so the model was told
/// `Sandbox: UNKNOWN` on every pre-lock turn.
///
/// The label is now produced only at the render edge, by [`Self::label`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SandboxAuthority {
    /// No sandbox report has arrived on this connection yet.
    #[default]
    Unknown,
    /// The server is still negotiating scope; tool calls are held.
    Initializing,
    /// ahma's own kernel sandbox is the sole authority.
    Locked,
    /// No kernel confinement is in effect.
    Unsandboxed,
    /// ahma is enforcing, nested inside the named host sandbox.
    Nested(Option<String>),
    /// ahma deferred to the named host sandbox and is NOT enforcing.
    Deferred(Option<String>),
    /// Sandbox configuration failed.
    Failed,
}

impl SandboxAuthority {
    /// Classify the wire token from a `sandbox/configured` notification. An
    /// unknown or absent token means LOCKED, preserving compatibility with
    /// servers that predate the token (SPEC R5.4).
    pub fn from_token(active: Option<&str>, host: Option<&str>) -> Self {
        use ahma_common::mcp_methods as m;
        match active {
            Some(m::ACTIVE_SANDBOX_NESTED_IN_HOST) => Self::Nested(host.map(str::to_string)),
            Some(m::ACTIVE_SANDBOX_DEFERRED_TO_HOST) => Self::Deferred(host.map(str::to_string)),
            Some(m::ACTIVE_SANDBOX_DISABLED) => Self::Unsandboxed,
            _ => Self::Locked,
        }
    }

    /// The compact status-bar chip text.
    pub fn label(&self) -> String {
        match self {
            Self::Unknown => "UNKNOWN".to_string(),
            Self::Initializing => "INITIALIZING".to_string(),
            Self::Locked => "LOCKED".to_string(),
            Self::Unsandboxed => "UNSANDBOXED".to_string(),
            Self::Failed => "FAILED".to_string(),
            Self::Nested(Some(h)) => format!("NESTED: {h}"),
            Self::Nested(None) => "NESTED".to_string(),
            Self::Deferred(Some(h)) => format!("DEFERRED: {h}"),
            Self::Deferred(None) => "DEFERRED".to_string(),
        }
    }

    /// Whether ahma is the only thing protecting this session. False for the
    /// host-relative modes and for no confinement at all — the cases whose
    /// disclosure must be surfaced loudly (SPEC R7).
    pub fn is_sole_authority(&self) -> bool {
        matches!(self, Self::Locked | Self::Initializing | Self::Unknown)
    }

    /// Whether a sandbox report has actually arrived. Guards surfaces that must
    /// not assert a sandbox state they do not know — notably the model's system
    /// prompt, where "UNKNOWN" is worse than silence.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

impl AppState {
    /// Where a `!` command run in this TUI reports its progress, if this
    /// process managed to open a reporter connection.
    ///
    /// Mints the operation id as well as handing back the channel, because the
    /// two must agree and there is exactly one correct way to pair them.
    pub fn bang_report(&self) -> Option<crate::llm_bridge::BangReport> {
        let reporter = self.tui_reporter.clone()?;
        let op_id = crate::tui_reporter::next_bang_op_id(reporter.session_id());
        Some(crate::llm_bridge::BangReport { reporter, op_id })
    }

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

    /// Advance the panel one frame along the current state's pattern.
    /// Low-level; prefer [`Self::mark_stream_activity`] (fast pulse on real
    /// output) or [`Self::tick_waiting_spinner`] (slow pulse while waiting).
    pub fn bump_liveness(&mut self) {
        self.liveness_frame = self.liveness_frame.wrapping_add(1);
        self.liveness_glyph = crate::liveness::panel_glyphs(
            self.liveness_seed,
            self.liveness_frame,
            self.liveness_state.pattern(),
            self.unicode,
        );
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

    /// The window a chat turn belongs to: its target instance, or the local
    /// section when chat has no target window.
    pub fn usage_key(target: Option<&str>) -> String {
        target
            .map(str::to_string)
            .unwrap_or_else(|| crate::task_tree::LOCAL_GROUP.to_string())
    }

    /// Show window `key`'s conversation, putting the current one away. The
    /// wrapped-row cache is keyed on a per-transcript generation counter, so it
    /// must be dropped here or it could serve the other window's rows.
    pub fn switch_transcript(&mut self, key: &str) {
        if self.chat_key == key {
            return;
        }
        let incoming = self.transcripts.remove(key).unwrap_or_default();
        let outgoing = std::mem::replace(&mut self.chat, incoming);
        let old_key = std::mem::replace(&mut self.chat_key, key.to_string());
        self.transcripts.insert(old_key, outgoing);
        *self.chat_rows_cache.borrow_mut() = ChatRowsCache::default();
        self.chat_scroll = 0;
    }

    /// Record a health transition, remembering when it went down.
    pub fn set_server_healthy(&mut self, healthy: bool) {
        match (self.server_healthy, healthy) {
            (true, false) => self.server_down_since = Some(std::time::Instant::now()),
            (_, true) => self.server_down_since = None,
            _ => {}
        }
        self.server_healthy = healthy;
    }

    pub fn set_daemon_healthy(&mut self, healthy: bool) {
        match (self.daemon_healthy, healthy) {
            (true, false) => self.daemon_down_since = Some(std::time::Instant::now()),
            (_, true) => self.daemon_down_since = None,
            _ => {}
        }
        self.daemon_healthy = healthy;
    }

    /// The model's context window in tokens: `--context-length`, else the
    /// selected provider's `num_ctx` from `~/.ahma/config.toml` (looked up
    /// once per provider). `None` when neither says — a percentage of an
    /// unknown whole is noise, so it is not guessed.
    pub fn context_window(&self, base_url: &str) -> Option<u32> {
        if let Some(n) = self.token_prefs.context_length.filter(|&n| n > 0) {
            return Some(n);
        }
        if base_url.is_empty() {
            return None;
        }
        if let Some((url, n)) = self.ctx_window_cache.borrow().as_ref()
            && url == base_url
        {
            return *n;
        }
        let n = ahma_common::config::AhmaConfig::load()
            .num_ctx_for_base_url(base_url)
            .filter(|&n| n > 0);
        *self.ctx_window_cache.borrow_mut() = Some((base_url.to_string(), n));
        n
    }

    /// True while the user is typing into the chat input. Gate keys (`y`/`a`/
    /// `n`, Enter) are text then, not answers: a word starting with `a` must
    /// not persist "always allow", and Enter must send the message, not deny a
    /// grant. Esc clears the input, after which the gate keys answer again.
    pub fn typing_in_chat(&self) -> bool {
        self.focus == Focus::Chat && !self.chat_input_is_empty()
    }

    /// True when a text-entry overlay (navigator or an inline picker) is open.
    /// Used to decide whether a bare key should be consumed as overlay input
    /// rather than a global shortcut (e.g. approval `y`/`n`).
    pub fn text_entry_modal_open(&self) -> bool {
        matches!(
            self.modal,
            ModalState::Navigator(_)
                | ModalState::ProviderPicker(_)
                | ModalState::ModelPicker(_)
                | ModalState::LlmSetupProvider(_)
                | ModalState::LlmSetupModel { .. }
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

    /// The inline provider picker, if it is open.
    pub fn provider_picker(&self) -> Option<&PickerState> {
        match &self.modal {
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

    /// Close the LLM setup provider picker and return its state.
    pub fn take_llm_setup_provider(&mut self) -> Option<PickerState> {
        match std::mem::take(&mut self.modal) {
            ModalState::LlmSetupProvider(p) => Some(p),
            other => {
                self.modal = other;
                None
            }
        }
    }

    /// Close the LLM setup model picker and return its state and provider details.
    pub fn take_llm_setup_model(&mut self) -> Option<(PickerState, String, String)> {
        match std::mem::take(&mut self.modal) {
            ModalState::LlmSetupModel {
                picker,
                provider_name,
                base_url,
            } => Some((picker, provider_name, base_url)),
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
        // Canonical, like the `--path` branch in `app::run`: this string keys
        // approval grants, and a symlinked spelling would never match the
        // canonical sandbox root the agent checks them against.
        let workspace = std::env::current_dir()
            .ok()
            .map(|p| dunce::canonicalize(&p).unwrap_or(p))
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_default();

        // Try to load per-directory session config.
        let session = std::env::current_dir()
            .ok()
            .and_then(|cwd| TuiSessionConfig::load(&cwd).ok().flatten());
        let llm_selection = session
            .as_ref()
            .filter(|s| !s.provider.trim().is_empty())
            .map(|s| LlmSelection::named(s.provider.clone(), s.model.clone()));
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
        let window_llms = session
            .as_ref()
            .map(|s| s.window_llms.clone())
            .unwrap_or_default();

        Self {
            server_url: server_url.into(),
            mcp_http_base_url: String::new(),
            transport_label: transport_label.into(),
            server_healthy: false,
            daemon_healthy: false,
            session_id: None,
            sandbox_status: SandboxAuthority::Unknown,
            workspace,
            active_target_instance: None,
            window_llms,
            token_prefs: crate::TokenPrefs::default(),
            minimize_tokens: false,
            last_prompt_tokens: 0,
            ai_activity: VecDeque::with_capacity(ACTIVITY_RING_CAP),
            events: VecDeque::with_capacity(EVENT_RING_CAP),
            operations: vec![],
            pending_output: HashMap::new(),
            log: VecDeque::with_capacity(LOG_RING_CAP),
            approval: None,
            scope_grant: None,
            web_approval: None,
            read_rates: std::collections::HashMap::new(),
            turn_retries: 0,
            recent_llms: session
                .as_ref()
                .map(|s| s.recent.clone())
                .unwrap_or_default(),
            displaced_client_model: None,
            trust_prompt: None,
            tools_list: vec![],
            mcp_connections,

            tasks_window_open: false,
            log_window_open: false,
            scope_window_open: false,
            help_scroll: 0,
            sandbox_scope: None,
            sandbox_failed_reason: None,
            granted_scopes: Vec::new(),
            chat: ChatHistory::default(),
            chat_input: TextArea::default(),
            llm_selection,
            current_provider_url,
            active_profile,
            mcp_enabled,
            available_providers: vec![],
            discovered_providers: vec![],
            active_instances: vec![],
            available_models: vec![],
            active_skills: vec![],
            chat_scroll: 0,
            modal: ModalState::None,

            focus: Focus::default(),
            ops_selected: 0,
            ops_scroll: std::cell::Cell::new(0),
            expanded_op: None,
            open_section: None,
            accordion: None,
            work_sections: std::cell::RefCell::new(Vec::new()),
            work_slots: std::cell::RefCell::new(Vec::new()),
            work_total_rows: std::cell::Cell::new(0),
            work_follow_selection: std::cell::Cell::new(true),
            chat_open: false,
            collapsed_nodes: std::collections::HashSet::new(),
            show_all_projects: false,
            project_root: None,
            task_rows: std::cell::RefCell::new(Vec::new()),
            auto_view_pending: true,
            chat_area: std::cell::Cell::new(Rect::default()),
            log_area: std::cell::Cell::new(Rect::default()),
            work_area: std::cell::Cell::new(Rect::default()),
            chat_input_area: std::cell::Cell::new(Rect::default()),
            last_mouse_pos: std::cell::Cell::new(None),
            chat_scroll_target: std::cell::Cell::new(0.0),
            chat_scroll_current: std::cell::Cell::new(0.0),
            log_scroll_target: std::cell::Cell::new(0.0),
            log_scroll_current: std::cell::Cell::new(0.0),
            chat_input_height_target: std::cell::Cell::new(1.0),
            chat_input_height_current: std::cell::Cell::new(1.0),
            chat_max_scroll: std::cell::Cell::new(0),
            detail_max_scroll: std::cell::Cell::new(0),
            log_max_scroll: std::cell::Cell::new(0),
            token_usage: ahma_llm_monitor::client::TokenUsage::default(),
            log_scroll: 0,
            log_follow: true,
            log_filter: String::new(),
            log_filter_active: false,
            log_files: vec![],
            active_log_file: None,
            active_log_lines: vec![],
            log_wrap_enabled: false,
            zoomed: None,
            log_lines_total: 0,
            click_targets: std::cell::RefCell::new(vec![]),
            ops_list_state: std::cell::RefCell::new(ratatui::widgets::ListState::default()),
            chat_rows_cache: std::cell::RefCell::new(ChatRowsCache::default()),

            settings_editor: crate::settings_editor::SettingsEditor::default(),

            windows: vec![],
            next_window_id: 0,
            cleared_at: None,
            window_rects: std::cell::RefCell::new(vec![]),

            liveness_glyph: "  ".to_string(),
            liveness_state: LivenessState::Idle,
            turn: None,
            setup_pending: None,
            transcripts: HashMap::new(),
            chat_key: String::new(),
            input_history: VecDeque::new(),
            history_pos: None,
            history_draft: String::new(),
            window_usage: HashMap::new(),
            server_down_since: None,
            daemon_down_since: None,
            ctx_window_cache: std::cell::RefCell::new(None),
            model_picker_requested: false,
            quit_armed_at: None,
            footer_hint: None,
            liveness_seed: liveness_initial_seed(),
            liveness_frame: 0,
            last_stream_activity: None,
            last_wait_tick: None,
            unicode,
            should_quit: false,
            bridge_tx: None,
            mcp_source_tx: None,
            approval_tx: None,
            tui_reporter: None,
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

        // Clear session active skills.
        self.active_skills.clear();

        // Reset scroll positions to their startup defaults.
        self.chat_scroll = 0;
        self.chat_max_scroll.set(0);
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
        self.chat_input.lines().join("\n")
    }

    pub fn chat_input_line_count(&self, width: usize) -> usize {
        {
            let mut total = 0;
            for line in self.chat_input.lines() {
                total += count_wrapped_lines(line, width);
            }
            total.max(1)
        }
    }
}

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
        return;
    }
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

/// Account for a single space: it either fits on the current line or closes it
/// and is swallowed by the wrap (a wrapped line does not start with a space).
fn handle_space_fit(current_line_len: &mut usize, lines: &mut usize, width: usize) {
    if *current_line_len < width {
        *current_line_len += 1;
    } else {
        *lines += 1;
        *current_line_len = 0;
    }
}

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
            handle_space_fit(&mut current_line_len, &mut lines, width);
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

    /// Remember a sent message for ↑ recall (consecutive repeats once).
    pub fn remember_input(&mut self, text: &str) {
        const INPUT_HISTORY_CAP: usize = 200;
        self.history_pos = None;
        if self.input_history.back().map(String::as_str) == Some(text) {
            return;
        }
        if self.input_history.len() >= INPUT_HISTORY_CAP {
            self.input_history.pop_front();
        }
        self.input_history.push_back(text.to_string());
    }

    /// ↑: step to the previous sent message. Returns false when there is
    /// nothing earlier, so the key can fall through to cursor movement.
    pub fn recall_previous_input(&mut self) -> bool {
        let pos = match self.history_pos {
            None if self.input_history.is_empty() => return false,
            None => {
                self.history_draft = self.chat_input_text();
                self.input_history.len() - 1
            }
            Some(0) => return true,
            Some(p) => p - 1,
        };
        self.history_pos = Some(pos);
        let text = self.input_history[pos].clone();
        self.replace_chat_input(&text);
        true
    }

    /// ↓: step to the next sent message, or back to the draft past the end.
    pub fn recall_next_input(&mut self) -> bool {
        let Some(pos) = self.history_pos else {
            return false;
        };
        if pos + 1 < self.input_history.len() {
            self.history_pos = Some(pos + 1);
            let text = self.input_history[pos + 1].clone();
            self.replace_chat_input(&text);
        } else {
            self.history_pos = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.replace_chat_input(&draft);
        }
        true
    }

    fn replace_chat_input(&mut self, text: &str) {
        self.clear_chat_input();
        self.chat_input.insert_str(text);
    }

    pub fn clear_chat_input(&mut self) {
        {
            self.chat_input = TextArea::default();
        }
    }

    /// A terminal paste. It goes where the user is typing: the log filter if
    /// that is active, otherwise the chat input — opened and focused first,
    /// because text pasted into a closed or unfocused input is text the user
    /// cannot see until it is sent.
    pub fn handle_paste(&mut self, text: &str) {
        if self.log_filter_active {
            let first_line = text.lines().next().unwrap_or_default();
            self.log_filter.push_str(first_line);
            self.log_scroll = 0;
            return;
        }
        self.chat_open = true;
        self.focus = Focus::Chat;
        self.paste_into_chat_input(text);
    }

    /// Insert pasted text into the chat input without submitting it.
    ///
    /// The trailing newline that terminals append to a paste (e.g. pasting
    /// `"somecommand\n"`) is stripped so the paste is shown but not sent — the
    /// user presses Enter to submit. Interior newlines are preserved, so a
    /// multi-line paste becomes multiple input lines (one request, not many).
    pub fn paste_into_chat_input(&mut self, text: &str) {
        {
            let trimmed = text.trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                self.chat_input.insert_str(trimmed);
            }
        }
    }

    pub fn selected_model(&self) -> String {
        self.llm_selection
            .as_ref()
            .map(|s| s.model.clone())
            .unwrap_or_default()
    }

    /// The header label for the current selection. `"no LLM"` when there is
    /// none — a rendering of `None`, not a value anything stores or compares.
    pub fn llm_label(&self) -> String {
        self.llm_selection
            .as_ref()
            .map(|s| s.display_label())
            .unwrap_or_else(|| "no LLM".to_string())
    }

    /// Append to the monitor activity feed, newest first, bounded.
    pub fn push_op_event(&mut self, event: OpEvent) {
        if self.events.len() >= EVENT_RING_CAP {
            self.events.pop_back();
        }
        self.events.push_front(event);
    }

    /// The sandbox path the header may honestly display: the server-locked
    /// primary write root once `sandbox/configured` has arrived, and only then.
    /// Before lock, the TUI knows its own launch directory but NOT the scope —
    /// the two can differ (container-root fallback, auto-narrowing, a different
    /// `roots/list` answer), so the launch path is returned separately by the
    /// caller and must be labelled as unconfirmed (SPEC R5.4: no scope decision
    /// is communicated by guesswork).
    pub fn locked_scope_root(&self) -> Option<&str> {
        self.sandbox_scope
            .as_ref()
            .and_then(|s| s.write.first())
            .map(String::as_str)
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
        // Advances the log title's rain panel one frame per line.
        self.log_lines_total = self.log_lines_total.wrapping_add(1);
        // New-line auto-scroll is handled by `log_follow`: when following, the
        // log pane renders pinned to the bottom (see `draw_log`), so there is no
        // scroll offset to nudge here. The previous heuristic adjusted
        // `log_scroll` by raw entry count, which was wrong once line-wrapping
        // made one entry span several rendered rows.
    }

    /// Allocation-free substring test for an already-lowercased ASCII needle.
    ///
    /// The log filter is typed by a human into a search box, so it is ASCII in
    /// practice; matching case-insensitively on bytes avoids allocating a
    /// lowercased copy of every log line on every frame.
    fn contains_ignore_ascii_case(haystack: &str, needle_lower: &str) -> bool {
        let (h, n) = (haystack.as_bytes(), needle_lower.as_bytes());
        match n.len() {
            0 => true,
            len if len > h.len() => false,
            len => h.windows(len).any(|w| w.eq_ignore_ascii_case(n)),
        }
    }

    pub fn filtered_log(&self) -> Vec<&LogEntry> {
        if self.log_filter.is_empty() {
            self.log.iter().collect()
        } else {
            // One allocation for the needle, none per entry. This runs on every
            // redraw of the log pane — up to LOG_RING_CAP entries, at animation
            // frame rate — so the previous `to_lowercase()` per message and per
            // level label allocated a fresh String for each entry, each frame.
            let q = self.log_filter.to_lowercase();
            self.log
                .iter()
                .filter(|e| {
                    Self::contains_ignore_ascii_case(&e.message, &q)
                        || Self::contains_ignore_ascii_case(e.level.label(), &q)
                })
                .collect()
        }
    }

    /// Rebuild this frame's sections and navigable rows.
    ///
    /// The one place the work view's model is built, called by the renderer
    /// *and* by every handler that needs to know what is selected. It used to
    /// be built inside the draw, so a key press before the first frame — or in
    /// any test that never rendered — operated on a different model than the
    /// one the user could see.
    pub fn rebuild_work_view(&self, now_ms: u64) {
        let area = self.work_area.get();
        let opts = crate::work_view::SectionOptions {
            instances: &self.active_instances,
            project_root: self.project_root.as_deref(),
            show_all: self.show_all_projects,
            open_section: self.open_section.as_deref(),
            closing_section: self
                .accordion
                .as_ref()
                .filter(|a| a.is_active(now_ms))
                .and_then(|a| a.closing.as_ref())
                .map(|t| t.key.as_str()),
            expanded_op: self.expanded_op.as_deref(),
            collapsed: &self.collapsed_nodes,
            tail_lines: crate::work_view::tail_lines_for(area.height),
        };
        let sections = crate::work_view::build_sections(&self.operations, &opts);
        *self.task_rows.borrow_mut() = crate::work_view::flatten(&sections);
        *self.work_sections.borrow_mut() = sections;
    }

    /// The content height each section is drawn at this frame: its full height
    /// when open, nothing when closed, and the eased value while moving.
    pub fn section_heights(&self, now_ms: u64) -> Vec<usize> {
        let sections = self.work_sections.borrow();
        sections
            .iter()
            .map(|s| self.section_height(s, now_ms))
            .collect()
    }

    /// The content height a single section is drawn at this frame — see
    /// [`Self::section_heights`].
    fn section_height(&self, s: &crate::work_view::Section, now_ms: u64) -> usize {
        let natural = s.rows.len();
        let open_height = if s.open { natural } else { 0 };
        match self.accordion.as_ref() {
            Some(anim) if anim.is_active(now_ms) => anim
                .height_for(&s.key, natural, now_ms)
                .unwrap_or(open_height),
            _ => open_height,
        }
    }

    /// Open `key`, closing whichever section was open — the accordion (R24.9).
    /// Toggling the open section shuts it.
    pub fn toggle_section(&mut self, key: &str, now_ms: u64) {
        let next = if self.open_section.as_deref() == Some(key) {
            None
        } else {
            Some(key.to_string())
        };
        let natural_of = |k: &str| -> usize {
            self.work_sections
                .borrow()
                .iter()
                .find(|s| s.key == k)
                .map(|s| s.rows.len())
                .unwrap_or(0)
        };
        self.accordion = crate::accordion::AccordionAnim::retarget(
            self.accordion.as_ref(),
            self.open_section.as_deref(),
            next.as_deref(),
            natural_of,
            now_ms,
        );
        self.open_section = next;
        self.rebuild_work_view(now_ms);
        // The rows under the cursor may have just gone; keep the selection on
        // something that exists.
        let rows = self.ops_row_count();
        if rows > 0 && self.ops_selected >= rows {
            self.ops_selected = rows - 1;
        }
        self.work_follow_selection.set(true);
    }

    /// Ensure that section `key` is open (opening it if currently closed).
    pub fn open_section(&mut self, key: &str, now_ms: u64) {
        if self.open_section.as_deref() == Some(key) {
            return;
        }
        let next = Some(key.to_string());
        let natural_of = |k: &str| -> usize {
            self.work_sections
                .borrow()
                .iter()
                .find(|s| s.key == k)
                .map(|s| s.rows.len())
                .unwrap_or(0)
        };
        self.accordion = crate::accordion::AccordionAnim::retarget(
            self.accordion.as_ref(),
            self.open_section.as_deref(),
            next.as_deref(),
            natural_of,
            now_ms,
        );
        self.open_section = next;
        self.rebuild_work_view(now_ms);
        let rows = self.ops_row_count();
        if rows > 0 && self.ops_selected >= rows {
            self.ops_selected = rows - 1;
        }
        self.work_follow_selection.set(true);
    }

    /// Get the last-used LLM configuration for a window.
    pub fn get_window_llm(
        &self,
        section_key: &str,
    ) -> Option<&crate::session_config::WindowLlmConfig> {
        self.window_llms.get(&self.window_llm_key(section_key))
    }

    /// Record the last-used LLM configuration for a window.
    pub fn set_window_llm(
        &mut self,
        section_key: &str,
        config: crate::session_config::WindowLlmConfig,
    ) {
        let key = self.window_llm_key(section_key);
        self.window_llms.insert(key, config);
    }

    /// The key a window's LLM choice is saved under. A section is keyed by
    /// the daemon's instance id, which is stable only while that daemon and
    /// that IDE session live; a choice saved under it was lost on every
    /// restart and left a dead entry behind. What the user means by "this
    /// window" across restarts is the client in this workspace.
    pub fn window_llm_key(&self, section_key: &str) -> String {
        match self.active_instances.iter().find(|i| i.id == section_key) {
            Some(inst) => {
                let who = inst.client.as_deref().unwrap_or(&inst.label);
                format!("{who}@{}", inst.scope)
            }
            None => section_key.to_string(),
        }
    }

    /// Number of navigable rows in the work view.
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

    /// The full text of a tool call, for the detail overlay: a chat row clips
    /// it to the pane width, and clipped content must be reachable (R24.8.4).
    pub fn tool_call_detail(&self, entry_idx: usize) -> Option<String> {
        match self.chat.entries().get(entry_idx)? {
            ChatEntry::ToolCall {
                name,
                args,
                result,
                failed,
                ..
            } => {
                let args = serde_json::from_str::<serde_json::Value>(args)
                    .and_then(|v| serde_json::to_string_pretty(&v))
                    .unwrap_or_else(|_| args.clone());
                let outcome = match (result, failed) {
                    (None, _) => "(still running)".to_string(),
                    (Some(r), true) => format!("FAILED\n{r}"),
                    (Some(r), false) => r.clone(),
                };
                Some(format!(
                    "{name}\n\nArguments:\n{args}\n\nResult:\n{outcome}"
                ))
            }
            _ => None,
        }
    }

    /// Open the full-screen detail overlay for one operation.
    pub fn open_operation_detail(&mut self, key: OpKey) {
        self.detail_max_scroll.set(0);
        self.modal = ModalState::OperationDetail(OperationDetailState {
            op_id: key.id,
            instance_id: key.instance_id,
            scroll: 0,
        });
    }

    pub fn find_op(&self, key: &OpKey) -> Option<&Operation> {
        key.position_in(&self.operations)
            .map(|i| &self.operations[i])
    }

    pub fn find_op_mut(&mut self, key: &OpKey) -> Option<&mut Operation> {
        key.position_in(&self.operations)
            .map(|i| &mut self.operations[i])
    }

    /// The operation shown in the detail overlay, if it is open.
    pub fn detail_op_key(&self) -> Option<OpKey> {
        match &self.modal {
            ModalState::OperationDetail(d) => Some(OpKey {
                id: d.op_id.clone(),
                instance_id: d.instance_id.clone(),
            }),
            _ => None,
        }
    }

    /// Open the full-screen detail overlay for one log line, wrapped so the
    /// whole line is readable rather than truncated at the pane edge.
    pub fn open_log_line_detail(&mut self, text: String) {
        self.detail_max_scroll.set(0);
        self.modal = ModalState::LogLineDetail(LogLineDetailState { text, scroll: 0 });
    }

    /// Enter on the selected task-tree row: drill into an operation's
    /// full-screen detail view, or open the accordion section for a
    /// session/instance header — the same `open_section` toggle a click or
    /// `Space` on that header performs (docs/tui.md: "Enter / click a
    /// header ... open"). A nested group header still folds in place.
    pub fn open_selected_tree_detail(&mut self, now_ms: u64) {
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
            Some(TreeToggle::Fold(key)) => match key.strip_prefix("inst:") {
                Some(section) => {
                    let section = section.to_string();
                    self.toggle_section(&section, now_ms);
                }
                None => self.toggle_collapse_key(key),
            },
            Some(TreeToggle::Expand(op_index)) => {
                if let Some(op) = self.operations.get(op_index) {
                    let key = OpKey::of(op);
                    self.open_operation_detail(key);
                }
            }
            None => {}
        }
    }

    /// Enter/click on the selected task-tree row: accordion-expand an
    /// operation (collapsing the previously expanded one), or fold/unfold an
    /// instance or session header.
    pub fn toggle_selected_tree_node(&mut self, now_ms: u64) {
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
            Some(TreeToggle::Fold(key)) => match key.strip_prefix("inst:") {
                // A section header opens the accordion; a group header inside a
                // section still folds in place.
                Some(section) => {
                    let section = section.to_string();
                    self.toggle_section(&section, now_ms);
                }
                None => self.toggle_collapse_key(key),
            },
            Some(TreeToggle::Expand(op_index)) => self.toggle_expanded_op(op_index),
            None => {}
        }
    }

    /// Accordion-expand the operation at `op_index`, or collapse it if it is
    /// already the expanded one.
    fn toggle_expanded_op(&mut self, op_index: usize) {
        let Some(op) = self.operations.get(op_index) else {
            return;
        };
        if self.expanded_op.as_deref() == Some(op.id.as_str()) {
            self.expanded_op = None;
        } else {
            self.expanded_op = Some(op.id.clone());
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

    /// Merge `op` into the already-known `existing`, yielding an activity-ring
    /// event only when the merge crosses from live to terminal — so a repeated
    /// poll of an already-finished op does not re-announce it.
    fn merge_and_detect_finish(existing: &mut Operation, op: Operation) -> Option<OpEvent> {
        let was_terminal = existing.status.is_terminal();
        Self::merge_operation(existing, op);
        (!was_terminal && existing.status.is_terminal()).then(|| OpEvent {
            timestamp: chrono::Local::now(),
            kind: OpEventKind::Finished(existing.status.clone()),
            op_id: existing.id.clone(),
            title: existing.display_name(),
            instance: existing.instance_label.clone(),
            duration_ms: existing.duration_ms,
        })
    }

    /// The activity-ring event for an op seen for the very first time: `Started`
    /// for a live op, `Finished` for one that arrives already terminal (e.g. a
    /// hub replay) — stamped with the op's own start time, not now.
    fn first_sight_event(op: &Operation) -> OpEvent {
        OpEvent {
            timestamp: op.started_time,
            kind: if op.status.is_terminal() {
                OpEventKind::Finished(op.status.clone())
            } else {
                OpEventKind::Started
            },
            op_id: op.id.clone(),
            title: op.display_name(),
            instance: op.instance_label.clone(),
            duration_ms: op.duration_ms,
        }
    }

    pub fn upsert_operation(&mut self, op: Operation) {
        let id = op.id.clone();
        // Feed the monitor activity ring exactly once per transition.
        let event = match self
            .operations
            .iter_mut()
            .find(|o| o.id == op.id && o.instance_id == op.instance_id)
        {
            Some(existing) => Self::merge_and_detect_finish(existing, op),
            None => {
                let event = Self::first_sight_event(&op);
                self.operations.push(op);
                Some(event)
            }
        };
        if let Some(event) = event {
            self.push_op_event(event);
        }
        self.flush_pending_output(&id);
        self.clamp_ops_selection();
    }

    /// Flush any output that arrived before operation `id` was materialised
    /// (see `pending_output`). Appends into the (now present) op's tail via
    /// the same overlap-dedup merge the poll/hub paths use, so a later poll
    /// that re-sends the full tail does not duplicate these lines.
    fn flush_pending_output(&mut self, id: &str) {
        let Some(pending) = self.pending_output.remove(id) else {
            return;
        };
        let Some(existing) = self.operations.iter_mut().find(|o| o.id == id) else {
            return;
        };
        let new_lines = Self::tail_suffix_to_append(&existing.stdout_tail, &pending);
        Self::append_stdout_lines(existing, new_lines);
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
        Self::merge_scalar_metadata(existing, &op);

        // Status always advances (but never regresses from terminal back to running).
        if !existing.status.is_terminal() || op.status.is_terminal() {
            existing.status = op.status;
        }

        // Merge stdout tails without duplicating: the poll path re-sends the
        // operation's FULL current tail on every cycle, and the hub path
        // streams the same lines incrementally.  Find the largest overlap
        // between the existing tail's suffix and the incoming tail's prefix,
        // then append only the genuinely new remainder.
        let new_lines = Self::tail_suffix_to_append(&existing.stdout_tail, &op.stdout_tail);
        Self::append_stdout_lines(existing, new_lines);

        Self::merge_alerts(&mut existing.alerts, op.alerts);

        // pinned is sticky — once pinned it stays pinned.
        existing.pinned = existing.pinned || op.pinned;
    }

    fn merge_scalar_metadata(existing: &mut Operation, op: &Operation) {
        if !op.description.is_empty() {
            existing.description = op.description.clone();
        }
        if !op.args.is_empty() {
            existing.args = op.args.clone();
        }
        Self::prefer_incoming(&mut existing.cwd, op.cwd.clone());
        Self::prefer_incoming(&mut existing.pid, op.pid);
        Self::prefer_incoming(&mut existing.scope, op.scope.clone());
        Self::prefer_incoming(&mut existing.result_summary, op.result_summary.clone());
        Self::prefer_incoming(&mut existing.completed_at, op.completed_at);
        Self::prefer_incoming(&mut existing.duration_ms, op.duration_ms);
    }

    fn merge_alerts(existing_alerts: &mut Vec<String>, incoming_alerts: Vec<String>) {
        for alert in incoming_alerts {
            if !existing_alerts.contains(&alert) {
                existing_alerts.push(alert);
            }
        }
    }

    /// Overwrite an optional field only when the incoming update supplies a
    /// value, so a source that leaves the field unset cannot clear it.
    fn prefer_incoming<T>(existing: &mut Option<T>, incoming: Option<T>) {
        if incoming.is_some() {
            *existing = incoming;
        }
    }

    /// Append `new_lines` to `existing`'s stdout tail, evicting from the
    /// front once the tail exceeds `STDOUT_TAIL_CAP`. Bumps
    /// `last_output_at` when any line is appended. Shared by the poll-merge
    /// (`merge_operation`) and pending-output-flush (`flush_pending_output`)
    /// paths, which both append to a capped tail using the same eviction rule.
    fn append_stdout_lines(existing: &mut Operation, new_lines: Vec<String>) {
        if !new_lines.is_empty() {
            existing.last_output_at = Some(Instant::now());
        }
        for line in new_lines {
            if existing.stdout_tail.len() >= STDOUT_TAIL_CAP {
                existing.stdout_tail.pop_front();
            }
            existing.stdout_tail.push_back(line);
        }
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

    fn turn_in(phase: TurnPhase, for_secs: u64) -> (ChatTurn, std::time::Instant) {
        let mut turn = ChatTurn::new(None);
        turn.enter(phase);
        let now = turn.phase_since + std::time::Duration::from_secs(for_secs);
        (turn, now)
    }

    /// The 2026-09-23 session: qwen spent minutes reading a 37k-token prompt
    /// and the screen said nothing. Every phase must say what is happening.
    #[test]
    fn status_line_names_each_phase_in_plain_words() {
        let (reading, now) = turn_in(TurnPhase::Reading, 72);
        let (text, hint) = turn_status_text(&reading, now, "qwen3.8", 37_400, Some(145.0));
        assert_eq!(
            text,
            "qwen3.8 is reading 37k tokens of context · 1m12s · ~3m05s left"
        );
        assert!(
            hint.unwrap().contains("/compact"),
            "slow + large → actionable hint"
        );

        let (thinking, now) = turn_in(TurnPhase::Thinking, 5);
        assert_eq!(
            turn_status_text(&thinking, now, "qwen3.8", 0, None).0,
            "qwen3.8 is thinking · 5s"
        );

        let (tool, now) = turn_in(
            TurnPhase::Tool {
                name: "cargo build".into(),
            },
            42,
        );
        assert_eq!(
            turn_status_text(&tool, now, "qwen3.8", 0, None).0,
            "running cargo build · 42s"
        );

        let (you, now) = turn_in(TurnPhase::AwaitingYou, 3);
        assert!(
            turn_status_text(&you, now, "m", 0, None)
                .0
                .contains("waiting for your answer")
        );
    }

    #[test]
    fn reading_past_the_estimate_says_so_instead_of_counting_negative() {
        let (turn, now) = turn_in(TurnPhase::Reading, 400);
        let (text, _) = turn_status_text(&turn, now, "m", 10_000, Some(100.0));
        assert!(text.ends_with("taking longer than last time"), "{text}");
    }

    #[test]
    fn leaving_reading_records_how_long_the_model_took_to_start() {
        let mut turn = ChatTurn::new(None);
        assert_eq!(turn.phase, TurnPhase::Reading);
        turn.note_token("hi");
        assert_eq!(turn.phase, TurnPhase::Writing);
        assert!(turn.last_prefill.is_some());
    }

    /// Waiting on the user, or on a running tool, is never the model stalling.
    #[test]
    fn only_a_silent_model_counts_as_stalled() {
        let late = TURN_STALL_AFTER + std::time::Duration::from_secs(1);
        for (phase, stalled) in [
            (TurnPhase::AwaitingYou, false),
            (TurnPhase::Tool { name: "t".into() }, false),
            (TurnPhase::Thinking, true),
        ] {
            let mut turn = ChatTurn::new(None);
            turn.enter(phase.clone());
            assert_eq!(
                turn.is_stalled(turn.last_event + late),
                stalled,
                "{phase:?}"
            );
        }
    }

    #[test]
    fn test_log_level_parse_level() {
        assert_eq!(LogLevel::parse_level("WARN"), LogLevel::Warn);
        assert_eq!(LogLevel::parse_level("ERROR"), LogLevel::Error);
        assert_eq!(LogLevel::parse_level("DEBUG"), LogLevel::Debug);
        assert_eq!(LogLevel::parse_level("INFO"), LogLevel::Info);
        assert_eq!(LogLevel::parse_level("UNKNOWN"), LogLevel::Info);
    }

    #[test]
    fn test_pending_approval_gate_with_deadline() {
        let now = Instant::now();
        let gate = ApprovalGate::new("dec-1", "tool", "summary")
            .with_deadline(Some(now + Duration::from_secs(10)));
        assert!(gate.remaining_secs().unwrap() <= 10);
    }

    #[test]
    fn test_chat_input_is_blank() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        assert!(s.chat_input_is_blank());
        s.chat_input = tui_textarea::TextArea::from(["   "]);
        assert!(s.chat_input_is_blank());
        assert!(!s.chat_input_is_empty());
        s.chat_input = tui_textarea::TextArea::from(["hello"]);
        assert!(!s.chat_input_is_blank());
    }

    /// Selection resolves through the drawn task-tree rows (not raw operation
    /// indices), and Enter behaves as a single-expand accordion for ops and a
    /// fold toggle for headers (SPEC R24.4).
    #[test]
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
        s.toggle_selected_tree_node(0);
        assert_eq!(s.expanded_op.as_deref(), Some("op-b"));
        s.ops_selected = 2;
        s.toggle_selected_tree_node(0);
        assert_eq!(s.expanded_op.as_deref(), Some("op-a"));
        // Toggling the same op collapses it.
        s.toggle_selected_tree_node(0);
        assert_eq!(s.expanded_op, None);
    }

    /// A header row opens its section rather than folding a subtree in place:
    /// the sections *are* the structure now (SPEC R24.9).
    #[test]
    fn a_header_row_opens_its_section_and_closes_the_other() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.active_instances = vec![
            instance_info("i1", "claude-code", "/work/proj"),
            instance_info("i2", "cursor", "/work/proj"),
        ];
        let mut a = Operation::new("op-a", "run_terminal_command", OpStatus::Running);
        a.instance_id = Some("i1".into());
        let mut b = Operation::new("op-b", "run_terminal_command", OpStatus::Running);
        b.instance_id = Some("i2".into());
        s.operations = vec![a, b];
        s.rebuild_work_view(0);

        // Select the first header and open it.
        s.ops_selected = 0;
        let first = s.work_sections.borrow()[0].key.clone();
        s.toggle_selected_tree_node(0);
        assert_eq!(s.open_section.as_deref(), Some(first.as_str()));
        assert!(
            s.collapsed_nodes.is_empty(),
            "a section header is not a collapse key any more"
        );

        // Opening the other section closes the first — the accordion.
        let second = s
            .work_sections
            .borrow()
            .iter()
            .find(|sec| sec.key != first)
            .map(|sec| sec.key.clone())
            .expect("a second section");
        s.toggle_section(&second, 1_000);
        assert_eq!(s.open_section.as_deref(), Some(second.as_str()));
        let anim = s.accordion.as_ref().expect("the movement is animated");
        assert_eq!(anim.closing.as_ref().unwrap().key, first);
        assert_eq!(anim.opening.as_ref().unwrap().key, second);

        // And toggling the open one shuts it.
        s.toggle_section(&second, 2_000);
        assert_eq!(s.open_section, None);
    }

    fn instance_info(id: &str, client: &str, scope: &str) -> ahma_common::daemon_hub::InstanceInfo {
        ahma_common::daemon_hub::InstanceInfo {
            id: id.into(),
            pid: 7,
            mode: "stdio".into(),
            scope: scope.into(),
            label: "ahma".into(),
            client: Some(client.into()),
            session_id: Some(format!("sess-{id}")),
            client_pid: None,
            sampling: false,
            ended_epoch_ms: None,
        }
    }

    /// The activity feed records exactly one Started per live op and one
    /// Finished per terminal transition — repeated terminal upserts (status
    /// re-polls) must not duplicate events.
    #[test]
    fn op_events_fire_once_per_transition() {
        use crate::state::{OpEvent, OpEventKind};
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);

        let mut op = Operation::new("op-e", "run_terminal_command", OpStatus::Running);
        op.title = Some("cargo build".into());
        s.upsert_operation(op.clone());
        assert_eq!(s.events.len(), 1);
        assert!(matches!(s.events[0].kind, OpEventKind::Started));
        assert_eq!(s.events[0].title, "cargo build");

        // Same op re-sent still running → no new event.
        s.upsert_operation(op.clone());
        assert_eq!(s.events.len(), 1);

        // Terminal transition → one Finished event.
        op.status = OpStatus::Succeeded;
        op.duration_ms = Some(2100);
        s.upsert_operation(op.clone());
        assert_eq!(s.events.len(), 2);
        assert!(matches!(
            s.events[0].kind,
            OpEventKind::Finished(OpStatus::Succeeded)
        ));
        assert_eq!(s.events[0].duration_ms, Some(2100));

        // Re-poll of the already-terminal op → still no new event.
        s.upsert_operation(op);
        assert_eq!(s.events.len(), 2);

        // First sight of an already-finished op (hub replay) → Finished,
        // stamped with the op's own start time.
        let mut replay = Operation::new("op-r", "tool", OpStatus::Failed);
        replay.title = Some("failing thing".into());
        s.upsert_operation(replay);
        assert!(matches!(
            s.events[0].kind,
            OpEventKind::Finished(OpStatus::Failed)
        ));

        // The ring is bounded.
        for i in 0..300 {
            s.push_op_event(OpEvent {
                timestamp: chrono::Local::now(),
                kind: OpEventKind::Started,
                op_id: format!("op-{i}"),
                title: "t".into(),
                instance: None,
                duration_ms: None,
            });
        }
        assert!(s.events.len() <= 200);
    }

    /// Enter drills into the full-screen detail overlay for op rows, and opens
    /// the accordion section for a session/instance header — the same
    /// `open_section` toggle a click or `Space` on that header performs
    /// (matching `docs/tui.md`'s "Enter / click a header → open").
    #[test]
    fn enter_opens_detail_overlay_for_ops_and_opens_section_for_headers() {
        use crate::task_tree::{RowKind, TreeRow};
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.operations
            .push(Operation::new("op-a", "tool", OpStatus::Running));
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
                    op_index: 0,
                    expanded: false,
                },
                depth: 1,
            },
        ];

        // Op row → detail overlay opens on that op, scroll reset.
        s.ops_selected = 1;
        s.open_selected_tree_detail(0);
        match &s.modal {
            ModalState::OperationDetail(d) => {
                assert_eq!(d.op_id, "op-a");
                assert_eq!(d.scroll, 0);
            }
            other => panic!("expected OperationDetail modal, got {other:?}"),
        }

        // Header row → opens the accordion section, no overlay. (The
        // open-then-close toggle itself is `toggle_section`'s own contract,
        // covered by `a_header_row_opens_its_section_and_closes_the_other`;
        // this test only needs to prove `Enter` reaches `toggle_section` at
        // all, which is what regressed.)
        s.modal = ModalState::None;
        s.ops_selected = 0;
        assert_eq!(s.open_section, None);
        s.open_selected_tree_detail(0);
        assert!(matches!(s.modal, ModalState::None));
        assert_eq!(s.open_section.as_deref(), Some("i1"));
    }

    /// Before any frame is drawn the row list is empty and selection falls
    /// back to flat operation indices, so headless/chat-only flows keep
    /// working.
    #[test]
    fn selection_falls_back_to_flat_list_before_first_draw() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.operations
            .push(Operation::new("only", "tool", OpStatus::Running));
        s.ops_selected = 0;
        assert_eq!(s.selected_op().unwrap().id, "only");
        assert_eq!(s.ops_row_count(), 1);
    }

    #[test]
    fn paste_strips_trailing_newline_without_submitting() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        // Pasting "somecommand\n" shows "somecommand" — the trailing newline is
        // dropped so it is not auto-submitted; the user must press Enter.
        state.paste_into_chat_input("somecommand\n");
        assert_eq!(state.chat_input_text(), "somecommand");
    }

    #[test]
    fn paste_strips_trailing_crlf() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.paste_into_chat_input("somecommand\r\n");
        assert_eq!(state.chat_input_text(), "somecommand");
    }

    #[test]
    fn paste_keeps_interior_newlines_as_multiline_input() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        // A multi-line paste becomes multiple input lines (a single request),
        // with the trailing newline still stripped.
        state.paste_into_chat_input("line one\nline two\n");
        assert_eq!(state.chat_input_text(), "line one\nline two");
    }

    #[test]
    fn paste_appends_to_existing_input() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.chat_input.insert_str("echo ");
        state.paste_into_chat_input("hello\n");
        assert_eq!(state.chat_input_text(), "echo hello");
    }

    #[test]
    fn paste_of_only_newline_is_noop() {
        let mut state = AppState::new("http://localhost:3000", "HTTP", true);
        state.paste_into_chat_input("\n");
        assert!(state.chat_input_is_empty());
    }

    #[test]
    fn test_count_wrapped_lines() {
        assert_eq!(count_wrapped_lines("hello world", 10), 2);
        assert_eq!(count_wrapped_lines("a verylongword", 10), 3);
        assert_eq!(count_wrapped_lines(" ", 10), 1);
        assert_eq!(count_wrapped_lines("", 10), 1);
        assert_eq!(count_wrapped_lines("one two three four five", 100), 1);
    }

    /// Every `ChatHistory` mutation must bump the generation counter — the
    /// renderer caches the wrapped transcript keyed on it, so a mutation that
    /// forgot to bump would freeze the chat pane on stale content.
    #[test]
    fn chat_generation_bumps_on_every_mutation() {
        let mut c = ChatHistory::default();
        let mut last = c.generation();
        let mut expect_bump = |c: &ChatHistory, what: &str| {
            assert_ne!(c.generation(), last, "{what} must bump the generation");
            last = c.generation();
        };

        c.push(ChatEntry::User {
            text: "hi".into(),
            payload: None,
            started_at: Some(std::time::Instant::now()),
            duration_ms: None,
        });
        expect_bump(&c, "push");

        c.finish_user_timing();
        expect_bump(&c, "finish_user_timing (sets a duration)");

        c.append_thinking("mull");
        expect_bump(&c, "append_thinking (new entry)");
        c.append_thinking(" it over");
        expect_bump(&c, "append_thinking (existing entry)");

        c.append_token("tok");
        expect_bump(&c, "append_token (new entry)");
        c.append_token("en");
        expect_bump(&c, "append_token (existing entry)");

        c.finish_stream();
        expect_bump(&c, "finish_stream");

        c.start_tool_call("t1".into(), "echo".into(), "{}".into());
        expect_bump(&c, "start_tool_call");
        c.finish_tool_call("t1", "ok".into(), false);
        expect_bump(&c, "finish_tool_call");

        c.compact(0);
        expect_bump(&c, "compact");

        c.retain_in_flight();
        expect_bump(&c, "retain_in_flight");

        c.clear();
        expect_bump(&c, "clear");
    }

    /// The chat prefix pattern follows the turn state — the direction IS the
    /// meaning (thinking shimmers, streaming rains, tool dispatch scrolls
    /// right).
    #[test]
    fn liveness_state_maps_to_panel_pattern() {
        use crate::liveness::PanelPattern;
        assert_eq!(LivenessState::Thinking.pattern(), PanelPattern::Shimmer);
        assert_eq!(LivenessState::Streaming.pattern(), PanelPattern::Rain);
        assert_eq!(LivenessState::ToolWait.pattern(), PanelPattern::ScrollRight);
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

    /// Tab cycles from the work view outwards: it is the home pane, so it is
    /// where the cycle starts and returns to (SPEC R24.9).
    #[test]
    fn focus_cycles_from_the_work_view_outwards() {
        assert_eq!(Focus::Work.cycle_next_active(true, true), Focus::Chat);
        assert_eq!(Focus::Chat.cycle_next_active(true, true), Focus::Log);
        assert_eq!(Focus::Log.cycle_next_active(true, true), Focus::Work);
        assert_eq!(Focus::Log.cycle_prev_active(true, true), Focus::Chat);
        assert_eq!(Focus::Work.cycle_prev_active(true, true), Focus::Log);

        // With nothing else open there is nowhere else to go.
        assert_eq!(Focus::Work.cycle_next_active(false, false), Focus::Work);
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
            payload: None,
            started_at: None,
            duration_ms: None,
        });
        hist.start_tool_call("call_1".into(), "test_tool".into(), "{}".into());
        hist.append_token("I ran the tool.");
        hist.finish_stream();
        hist.push(ChatEntry::User {
            text: "Thanks.".into(),
            payload: None,
            started_at: None,
            duration_ms: None,
        });

        // Keep the last turn: the whole first turn (user, tool call, reply)
        // goes, so the next prompt carries less context — the point.
        assert_eq!(hist.compact(1), 1);
        assert_eq!(hist.entries.len(), 1);
        assert!(matches!(&hist.entries[0], ChatEntry::User { text, .. } if text == "Thanks."));

        // Nothing older left to drop.
        assert_eq!(hist.compact(1), 0);
        assert_eq!(hist.entries.len(), 1);
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

    /// The bug the user actually reported: a TUI opened after an IDE had been
    /// working showed rows like `op_41_echo_hello` — a name reverse-engineered from
    /// the operation *id*, because the wire carried nothing better.
    ///
    /// With R24.7 the server sends a title, and it wins over every heuristic.
    #[test]
    fn the_wire_title_wins_over_every_guess() {
        let mut op = Operation::new(
            "op_41_echo_hello",
            "run_terminal_command",
            OpStatus::Running,
        );
        // The description and the id are both things the old code would have mined
        // for a name. The title outranks both, because only the server knew.
        op.description = "Execute something misleading in /elsewhere".to_string();
        op.title = Some("cargo nextest run -p ahma_core".to_string());

        assert_eq!(op.display_name(), "cargo nextest run -p ahma_core");
    }

    /// A pre-R24.7 server sends no title, so the legacy chain still has to work —
    /// mixed versions must not regress to a blank row.
    #[test]
    fn without_a_wire_title_the_legacy_chain_still_names_the_row() {
        let mut op = Operation::new(
            "op_41_echo_hello",
            "run_terminal_command",
            OpStatus::Running,
        );
        op.title = None;
        op.description = String::new();
        // This is the old, bad name — kept working on purpose for old servers.
        assert_eq!(op.display_name(), "echo hello");
    }

    /// The identity line is what the user reads: what ran, where, and how it ended.
    #[test]
    fn a_finished_operation_renders_its_command_and_exit_status() {
        let mut op = Operation::new("op_1", "run_terminal_command", OpStatus::Failed);
        op.title = Some("cargo build".to_string());
        op.cwd = Some("/Users/me/github/ahma".to_string());
        op.exit_code = Some(101);
        op.duration_ms = Some(3_400);

        let line = op.identity().render(false);
        assert_eq!(line, "✗ cargo build · ahma · exit 101 · 3s");
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
            duration_ms: None,
            last_output_at: None,
            is_cli: true,
            command: String::new(),
            working_dir: String::new(),
            llm_model: None,
            visible: true,
            abort_tx: std::sync::Arc::new(tokio::sync::Mutex::new(Some(abort_tx))),
            op_id: None,
        }
    }

    /// The live tail skips markers and friendly lines so the collapsed running
    /// row shows real output, not scaffolding.
    #[test]
    fn last_output_line_skips_markers() {
        let mut w = test_window(1, WindowStatus::Running, false);
        w.content = vec![
            WindowLine::start("Started at 06:06:33"),
            WindowLine::output("Compiling ahma_core v0.16.1"),
            WindowLine::live_edge(),
        ];
        assert_eq!(w.last_output_line(), Some("Compiling ahma_core v0.16.1"));

        w.content = vec![
            WindowLine::start("Starting run_terminal_command at 06:06:33"),
            WindowLine::live_edge(),
        ];
        assert_eq!(w.last_output_line(), None);

        // A stdout line that merely *looks* like a marker is still output. The
        // old prefix filter dropped it.
        w.content = vec![
            WindowLine::start("Started at 06:06:33"),
            WindowLine::output("-- applying migration"),
        ];
        assert_eq!(w.last_output_line(), Some("-- applying migration"));
    }

    /// Text the model wrote before a tool call is a finished reply the moment
    /// the tool call starts. Left flagged `streaming`, it kept its live cursor
    /// forever and was never sent back to the model in later turns.
    #[test]
    fn tool_call_seals_the_reply_written_before_it() {
        let mut hist = ChatHistory::default();
        hist.push(ChatEntry::Assistant {
            content: String::new(),
            streaming: true,
        });
        hist.append_token("let me look");
        hist.start_tool_call("c1".into(), "read_file".into(), "{}".into());

        assert!(matches!(
            &hist.entries()[0],
            ChatEntry::Assistant { content, streaming: false } if content == "let me look"
        ));

        hist.finish_tool_call("c1", "ok".into(), false);
        hist.append_token("done");
        hist.finish_stream();

        let streaming = hist.entries().iter().filter(|e| {
            matches!(
                e,
                ChatEntry::Assistant {
                    streaming: true,
                    ..
                }
            )
        });
        assert_eq!(streaming.count(), 0);
    }

    /// The empty placeholder pushed at submit must not survive the turn when
    /// thinking or a tool call came first and no text ever landed in it.
    #[test]
    fn finish_stream_drops_an_empty_placeholder() {
        let mut hist = ChatHistory::default();
        hist.push(ChatEntry::Assistant {
            content: String::new(),
            streaming: true,
        });
        hist.append_thinking("hmm");
        hist.append_token("answer");
        hist.finish_stream();

        let assistants: Vec<_> = hist
            .entries()
            .iter()
            .filter_map(|e| match e {
                ChatEntry::Assistant { content, streaming } => Some((content.as_str(), *streaming)),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec![("answer", false)]);
    }

    /// A window's LLM choice survives the client reconnecting under a new
    /// instance id: it is the same client in the same workspace.
    #[test]
    fn window_llm_survives_a_new_instance_id() {
        use crate::session_config::WindowLlmConfig;
        let inst = |id: &str| ahma_common::daemon_hub::InstanceInfo {
            id: id.into(),
            scope: "/w".into(),
            label: "ahma".into(),
            client: Some("claude-code".into()),
            ..Default::default()
        };
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.active_instances = vec![inst("uuid-1")];
        s.set_window_llm(
            "uuid-1",
            WindowLlmConfig {
                provider: "Ollama".into(),
                model: "qwen".into(),
                provider_url: None,
            },
        );

        s.active_instances = vec![inst("uuid-2")];
        assert_eq!(
            s.get_window_llm("uuid-2").map(|c| c.model.as_str()),
            Some("qwen")
        );
        assert_eq!(s.window_llms.len(), 1);
    }

    #[test]
    fn a_paste_lands_where_the_user_can_see_it() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.chat_open = false;
        s.focus = Focus::Work;
        s.handle_paste("cargo test\n");
        assert!(s.chat_open && s.focus == Focus::Chat);
        assert_eq!(s.chat_input_text(), "cargo test");

        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.log_filter_active = true;
        s.handle_paste("ERROR\nignored");
        assert_eq!(s.log_filter, "ERROR");
        assert!(s.chat_input_is_empty());
    }

    #[test]
    fn retain_in_flight_keeps_only_active_turns() {
        let mut hist = ChatHistory::default();
        hist.push(ChatEntry::User {
            text: "done turn".into(),
            payload: None,
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
            payload: None,
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
        s.ops_selected = 3;
        s.ops_scroll.set(9);
        s.log_follow = false;

        s.clear_screen();

        let ids: Vec<usize> = s.windows.iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![1, 3], "finished window removed, in-flight kept");
        assert!(s.chat.is_empty(), "completed chat entries cleared");
        assert_eq!(s.log.len(), 1, "log feed preserved");
        assert_eq!(s.chat_scroll, 0);
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
        let mut nav = CommandNavigator::opened(&tools, vec![]);
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

        // When input is "ex", should match /quit (via alias) and /export markdown
        // (via prefix). Membership rather than an exact count: the filter also
        // matches descriptions, so any command whose description happens to
        // contain "ex" — "/provider numctx" describes a *context* length —
        // legitimately joins the list, and asserting a total here just makes the
        // test fail whenever the command table grows.
        nav.input = "ex".to_string();
        nav.refresh_completions(&tools);
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

    /// SPEC R-SK8: discovered Agent Skills appear in the `/` navigator and
    /// survive keystroke-driven refreshes.
    #[test]
    fn navigator_includes_skill_commands() {
        let skills = vec![NavCommand {
            command: "/my-skill".into(),
            description: "Agent Skill — does things".into(),
        }];
        let mut nav = CommandNavigator::opened(&[], skills);
        assert!(nav.completions.iter().any(|c| c.command == "/my-skill"));

        nav.input = "my-sk".to_string();
        nav.refresh_completions(&[]);
        assert!(nav.completions.iter().any(|c| c.command == "/my-skill"));

        nav.input = "zzz-no-match".to_string();
        nav.refresh_completions(&[]);
        assert!(!nav.completions.iter().any(|c| c.command == "/my-skill"));
    }

    #[test]
    fn clear_screen_resets_active_skills() {
        let mut s = AppState::new("http://localhost:3000", "HTTP", true);
        s.active_skills.push(ahma_common::skills::Skill {
            name: "test-skill".into(),
            description: "desc".into(),
            user_invocable: true,
            path: std::path::PathBuf::from("/tmp/test-skill/SKILL.md"),
            body: "body".into(),
        });
        assert_eq!(s.active_skills.len(), 1);

        s.clear_screen();
        assert!(s.active_skills.is_empty());
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

        // Opening the navigator replaces help — both are never open at once.
        s.modal = ModalState::Navigator(CommandNavigator::opened(&[], vec![]));
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

#[cfg(test)]
mod llm_selection_tests {
    use super::{LlmSelection, ProviderRef};

    /// `/mcp on` with nothing selected used to split the display label "no LLM",
    /// find no " / ", and persist the whole sentinel as
    /// `[agent].provider = "no LLM"` in the user's global ~/.ahma/settings.toml —
    /// the file the MCP sub-agent reads. `None` must persist nothing.
    #[test]
    fn no_selection_persists_no_provider() {
        let selection: Option<LlmSelection> = None;
        let (provider, model) = match selection.as_ref() {
            Some(s) => (s.persistable_provider().to_string(), s.model.clone()),
            None => (String::new(), String::new()),
        };
        assert_eq!(provider, "");
        assert_eq!(model, "");
    }

    /// A loaded profile rendered as `profile:<alias> / <model>`, and splitting
    /// that label yielded `profile:<alias>` — persisted as a provider name no
    /// registry contains. A profile carries a URL, not a registry name, so the
    /// persistable provider must be empty and the URL persisted separately.
    #[test]
    fn profile_selection_never_persists_its_alias_as_a_provider() {
        let selection = LlmSelection::profile("my-profile", "gemma2:latest");
        assert_eq!(
            selection.display_label(),
            "profile:my-profile / gemma2:latest",
            "the alias is still what the header shows"
        );
        assert_eq!(
            selection.persistable_provider(),
            "",
            "but it must never be persisted as a provider name"
        );
        assert_eq!(
            selection.provider_name(),
            None,
            "and callers wanting a registry provider must get None"
        );
    }

    #[test]
    fn named_selection_round_trips_provider_and_model() {
        let selection = LlmSelection::named("Ollama", "llama3.2");
        assert_eq!(selection.display_label(), "Ollama / llama3.2");
        assert_eq!(selection.persistable_provider(), "Ollama");
        assert_eq!(selection.provider_name(), Some("Ollama"));
        assert_eq!(selection.provider, ProviderRef::Named("Ollama".into()));
    }
}
