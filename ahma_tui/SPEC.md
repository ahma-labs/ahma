# ahma_tui Crate Specification

* **Status**: Approved
* **Date**: 2026-06-12

## 1. User Story / Problem Statement

*As an interactive user, I want a unified terminal UI to monitor running background operations, chat with the local/remote LLM, and review/resolve security approval gates easily.*

## 2. Acceptance Criteria

- **Terminal Dashboard**: Implements a full-screen ratatui terminal user interface with mouse support and Unicode detection.
- **Operations Monitor**: Displays all active, pending, and completed background operations with detailed status views.
- **Unified Work View (SPEC R24, R24.9)**: The home view. One borderless section per client session — a rule with the client, its scope, a liveness glyph, the running/queued/succeeded/failed tallies, and the command it is running or last ran — with hooked commands folded into one section and this TUI's own work into *this terminal (you)*. Exactly one section is open at a time; opening another closes it over a 300 ms eased layout tween. Inside the open section, operations with children indented under whatever spawned them (`parent_id`); Space/click expands one into its live or historic output tail, Enter opens the full-screen detail overlay, `f` toggles this-project/all-projects, and the wheel scrolls the view. Chat is a toggle (`i`, `/chat`), not the screen.
- **Current at Startup**: Opening the TUI in a project directory immediately shows work already in flight, and recent work that has finished: the daemon's replay (with `started_epoch_ms`/`ended_epoch_ms` back-dating, the retained output window, and the last hour restored from disk) populates the view before and independent of any MCP handshake, and live project work auto-**opens that client's section** until the first user keystroke.
- **Live Streaming**: Operation windows appear when an operation STARTS (hub `OpStarted`) and stream live output lines end-to-end (`OpOutput` from the unified event stream); polling is demoted to periodic reconciliation against the store of record.
- **Interactive Controls**: Allows pin/unpin and cancellation of running background operations via interactive hotkeys or mouse clicks.
- **Chat Interface**: Connects to the local LLM and streams chat responses, incorporating animated thinking indicators.
- **Small-Model Context Harness**: When chatting with limited-context local models, per-tool-result output is truncated head+tail with an explicit elision marker, and the conversation is trimmed (system prompt + latest messages preserved, with an injected notice) to fit the model's context. Controlled by `--context-length <tokens>`, `--small-model-harness` / `--no-small-model-harness`, and `--minimize-tokens` / `--no-minimize-tokens`; flags take precedence over (deprecated) env vars and settings.
- **Sandbox Scope Panel (SPEC R5.4(b), R-PERM.5.1)**: `/scope` toggles a sub-window showing the
  scope the server actually locked — every write root, read roots, `--tmp` status, kernel
  enforcement, `source:` provenance, the active-authority line (ahma / nested / deferred-to-host /
  disabled, per R7.5), any platform note (macOS reads-unconfined), and the `sandbox/failed` reason
  when configuration failed. The data is parsed from the full `notifications/sandbox/configured`
  payload (`params.scope`, with top-level-params fallback for older emitters). The header and
  input-box `sandbox:` labels render the **server-locked** primary write root once reported; until
  then the TUI's launch directory is shown with a trailing `?` because it is a guess, not the
  boundary. NESTED/DEFERRED chips use a colour distinct from INITIALIZING.
- **Chat input prefixes**: `/` opens the command navigator, `#` runs LLM goal decomposition, and
  **`!` runs a command completely outside the sandbox**, at full user privilege. The `!` escape is
  reachable only from a human keystroke in the input box — never from an LLM turn, a tool call, or
  a replayed event — and every use is disclosed twice: a warning log line naming the command, and
  an `UNSANDBOXED` label on the resulting output window. It exists because a user who cannot run
  one unconfined command from inside ahma will run it in another terminal, where ahma can neither
  disclose nor record it; the honest escape hatch is the one that stays visible (SPEC R7's rule
  that enforcement is never silently disabled applies to its *disclosure*, not to forbidding it).
- **Approval Gate Prompts**: Renders and resolves the three security gates the TUI owns — chat
  tool approval (`y`/`n`), sandbox scope grants, and web egress (SPEC R-WEB.6). All three default
  to deny on Enter/Esc. A denied operation is selectable in the task tree and `a` re-raises its
  grant question through the owning instance's broker (R-PERM.7.1).
- **Log Monitor integration**: Displays real-time tailing of log files and LLM-powered alert notifications. A log line clipped at the pane edge can be clicked to open it wrapped and scrollable (SPEC R24.8.4).
- **Honest panes (SPEC R24.8)**: Every scrollable or size-capped pane tells the truth about what it is showing — the scrollbar thumb reaches the bottom exactly when the content does, layout budgets the rows the renderer actually draws, overflow keeps the result rather than the command echo, clipped content stays reachable, and each advertised toggle names its own key.
- **Agent Skills (SPEC §10 R-SK8)**: `/skills` lists skills discovered per the [Agent Skills open standard](https://agentskills.io/specification); `/<name> [args]` (or explicitly `/skill <name> [args]`) runs one — the pane shows the typed command while the LLM receives the full `SKILL.md` instructions on that and every later turn. Discovered skills appear in the `/` command navigator; invalid skill directories are disclosed, not hidden.
- **Tool-call session reuse (SPEC R25)**: Chat tool calls reuse a single negotiated MCP session rather than performing a full `initialize`/`roots/list` handshake and spawning a fresh bridge subprocess per call. The session id established by a tool call's `get_or_create_session` is fed back into `state.session_id` so every later tool call in the turn — and across turns — reuses it instead of racing the bridge's session limit.

## 3. Non-Functional Requirements

- **Resource Usage**: Must remain idle (low CPU) when no active rendering or updates are happening.
- **Terminal Recovery**: Must reliably restore terminal raw mode and clear alternate screens on exit, even upon panic.

## 4. Out of Scope

- Implementing the LLM or MCP protocol directly (delegated to the HTTP bridge or stdio endpoints).
