# ahma_tui Crate Specification

* **Status**: Approved
* **Date**: 2026-06-12

## 1. User Story / Problem Statement

*As an interactive user, I want a unified terminal UI to monitor running background operations, chat with the local/remote LLM, and review/resolve security approval gates easily.*

## 2. Acceptance Criteria

- **Terminal Dashboard**: Implements a full-screen ratatui terminal user interface with mouse support and Unicode detection.
- **Operations Monitor**: Displays all active, pending, and completed background operations with detailed status views.
- **Live Task Tree (SPEC R24)**: The monitor pane renders a project-scoped tree — one header per attached instance (MCP client identity, transport, scope, running/queued/succeeded/failed tallies), operations beneath it, children indented under the operation or session that spawned them (`parent_id`). Enter/click accordion-expands one task into its live or historic output tail; instance and session headers fold their subtree; `f` toggles this-project/all-projects.
- **Current at Startup**: Opening the TUI in a project directory immediately shows work already in flight: the hub replay (with `started_epoch_ms`/`ended_epoch_ms` back-dating) populates the tree before/independent of any MCP handshake, and live project work from an attached client auto-opens the task view until the first user keystroke.
- **Live Streaming**: Operation windows appear when an operation STARTS (hub `OpStarted`) and stream live output lines end-to-end (`OpOutput` from the unified event stream); polling is demoted to periodic reconciliation against the store of record.
- **Interactive Controls**: Allows pin/unpin and cancellation of running background operations via interactive hotkeys or mouse clicks.
- **Chat Interface**: Connects to the local LLM and streams chat responses, incorporating animated thinking indicators.
- **Small-Model Context Harness**: When chatting with limited-context local models, per-tool-result output is truncated head+tail with an explicit elision marker, and the conversation is trimmed (system prompt + latest messages preserved, with an injected notice) to fit the model's context. Controlled by `--context-length <tokens>`, `--small-model-harness` / `--no-small-model-harness`, and `--minimize-tokens` / `--no-minimize-tokens`; flags take precedence over (deprecated) env vars and settings.
- **Approval Gate Prompts**: Intercepts and renders prompts for security checkpoints (e.g. renewal gates, elevation requests).
- **Log Monitor integration**: Displays real-time tailing of log files and LLM-powered alert notifications. A log line clipped at the pane edge can be clicked to open it wrapped and scrollable (SPEC R24.8.4).
- **Honest panes (SPEC R24.8)**: Every scrollable or size-capped pane tells the truth about what it is showing — the scrollbar thumb reaches the bottom exactly when the content does, layout budgets the rows the renderer actually draws, overflow keeps the result rather than the command echo, clipped content stays reachable, and each advertised toggle names its own key.
- **Agent Skills (SPEC §15 R-SK8)**: `/skills` lists skills discovered per the [Agent Skills open standard](https://agentskills.io/specification); `/<name> [args]` (or explicitly `/skill <name> [args]`) runs one — the pane shows the typed command while the LLM receives the full `SKILL.md` instructions on that and every later turn. Discovered skills appear in the `/` command navigator; invalid skill directories are disclosed, not hidden.
- **Tool-call session reuse (SPEC R25)**: Chat tool calls reuse a single negotiated MCP session rather than performing a full `initialize`/`roots/list` handshake and spawning a fresh bridge subprocess per call. The session id established by a tool call's `get_or_create_session` is fed back into `state.session_id` so every later tool call in the turn — and across turns — reuses it instead of racing the bridge's session limit.

## 3. Non-Functional Requirements

- **Resource Usage**: Must remain idle (low CPU) when no active rendering or updates are happening.
- **Terminal Recovery**: Must reliably restore terminal raw mode and clear alternate screens on exit, even upon panic.

## 4. Out of Scope

- Implementing the LLM or MCP protocol directly (delegated to the HTTP bridge or stdio endpoints).
