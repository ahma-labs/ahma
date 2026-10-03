# ahma_tui Crate Specification

* **Status**: Approved
* **License**: AGPL-3.0
* **Depends on**: `ahma_core`, `ahma_mcp`, `ahma_common`, `ahma_http_mcp_client`, `ahma_llm_monitor`
* **Used by**: `ahma_bin` (`ahma tui`)

## 1. User Story / Problem Statement

*As an interactive user, I want a unified terminal UI to monitor running background operations, chat with the local/remote LLM, and review/resolve security approval gates easily.*

## 2. Acceptance Criteria

Criteria specified in §3 or in the root SPEC are cited by id, not restated.

- **Terminal dashboard**: a full-screen ratatui UI with mouse support and Unicode detection.
- **Unified work view (R24, R24.3, R24.4, R24.9)**: the home view; the mouse wheel scrolls
  it; chat is a toggle (`i`, `/chat`).
- **Current at startup (R24.2)**.
- **Live streaming**: an operation window appears when the hub reports `OpStarted` and streams
  `OpOutput` end to end; polling is only periodic reconciliation against the store of record
  (R24.6).
- **Interactive controls (R24.10.6)**: pin/unpin and cancel a running operation by hotkey or
  click.
- **Its own work is everyone's work (root R-HUB.9)**: `!` commands are reported through the
  TUI's own reporter connection, and marked wherever drawn: a `!` on the row, `UNSANDBOXED` in
  the detail pane and the identity footnote.
- **Chat**: connects to the configured LLM and streams replies with an animated thinking
  indicator.
- **Stoppable, visible turns (R24.10)**. `/compact` keeps the last 4 turns, so the model really
  sees less.
- **Small-model context harness (R24.12.8)**: with a limited-context model, each tool result is
  truncated head+tail with an explicit elision marker, and the conversation is trimmed (system
  prompt and latest messages kept, with an injected notice) to fit the model's context. Set by
  `--context-length <tokens>`, `--small-model-harness` / `--no-small-model-harness` and
  `--minimize-tokens` / `--no-minimize-tokens`; flags take precedence over settings (R-CFG1.2).
- **Sandbox scope panel (root R5.4(b), R-PERM.5.1)**: `/scope` toggles a sub-window showing
  the scope the server locked, parsed from the full `notifications/sandbox/configured` payload
  (`params.scope`): the R5.4 fields, the active-authority line (R7.5), any platform note
  (macOS reads-unconfined) and the `sandbox/failed` reason. The header and input-box
  `sandbox:` labels show the server-locked primary write root once reported; until then they
  show the launch directory with a trailing `?`, because it is a guess. NESTED/DEFERRED chips
  use a colour distinct from INITIALIZING.
- **Chat input prefixes**: `/` opens the command navigator; `#` asks the chat model to break a
  goal into steps; **`!` runs a command completely outside the sandbox**, at full user
  privilege. `!` is reachable only from a human keystroke in the input box — never from an LLM
  turn, a tool call or a replayed event — and every use is disclosed twice: a warning log line
  naming the command, and an `UNSANDBOXED` label on its output window. Disclosure, not
  prohibition, is what R7 requires of an escape hatch.
- **Approval gates (root R-WEB.6, R-WEB.6.3, R-PERM.7.1)**: the TUI renders and resolves
  chat tool approval (`y`/`n`), sandbox scope grants and web egress; all three deny on
  Enter/Esc. A denied operation is selectable and `a` re-raises its grant question.
- **Trusted folders (root R-PERM.1.2, R-PERM.1.3)**.
- **Guided setup, help and settings (R24.12)**.
- **Minimal chrome (R24.11)**.
- **Honest panes (R24.8)**, including the log monitor: real-time log tailing with
  LLM-powered alert notifications.
- **Agent Skills (root R-SK8, R-SK8.3, R-SK8.4)**: `/skills`, `/<name> [args]` and
  `/skill <name> [args]`.
- **Tool-call session reuse (R25)**.

## 3. Requirements owned here

### R24: Live Task Tree (TUI observability)

`ahma tui` opened in a project directory is a **real-time view of all work being done on the
user's behalf in that project** — by every attached MCP client (Claude Code, Cursor,
Antigravity, …) and by the user's own TUI/CLI commands — rendered as a compact caller → subtask
tree. The view must be correct **at startup**, not only for events that happen afterwards.

- **R24.1 — Causality is stamped at the source.** Every `Operation` carries an optional
  `parent_id`: the operation — or synthetic group such as `session:<id>` for persistent-shell
  commands — that spawned it. The parent link is set where the operation is created (adapter),
  carried on `OperationEvent::Started`, and forwarded on the hub wire
  (`HubEvent::OpStarted.parent_id`). Observers **must not** infer hierarchy from descriptions
  or naming conventions.

- **R24.2 — Current at startup ("it just works").** On launch the TUI subscribes to the hub,
  which replays each instance's retained operation history (`OpStarted`, the bounded output
  window that followed it as ordinary `OpOutput` events, then terminal `OpFinished`) before
  live events — including the last hour restored from disk for instances that are no longer
  attached (R-HUB.7), so recent work does not disappear because the client that did it has
  closed. The replay populates the view before, and independently of, any MCP handshake. An
  operation still running when its hub went away replays `interrupted`. Replayed events carry
  wall-clock timestamps (`started_epoch_ms` / `ended_epoch_ms`) so elapsed/duration displays
  are **true times, not time-since-receipt**. When the replay reveals live work for the current
  project from an attached client, the TUI **opens that client's section** automatically; any
  user keystroke disarms this auto-open.

- **R24.3 — Project-scoped by default.** The view shows instances whose **committed** sandbox
  scope (R-HUB.6) covers, or lives inside, the directory the TUI was started in; an instance
  whose scope is not established yet reads *no scope yet* rather than being filtered out. `f`
  toggles all projects. Matching is component-boundary path containment in either direction.
  Instances with no operations still render.

- **R24.4 — Compact tree with accordion drill-in.** Inside the open section (R24.9), one line
  per task: operations beneath the section header, children indented under their parent
  (session groups, spawned subtasks — arbitrary depth). Finished tasks resolve in place to a
  terminal glyph + duration. Space or click on a task expands it inline into its live output
  tail (running) or historic output/result summary (finished); expanding one task collapses the
  previously expanded one (single-expand accordion), and Enter opens the full-screen operation
  detail overlay instead. Instance and session headers fold/unfold their subtree.

- **R24.5 — Additive wire evolution.** The hub socket has no version to negotiate, so every
  protocol change is additive: a new `#[serde(default)]` field (`parent_id`,
  `started_epoch_ms`, `ended_epoch_ms`, `partial`, `interrupted`, `unsandboxed` on `HubEvent`;
  `client`, `session_id`, `client_pid`, `ended_epoch_ms` on `Register`/`InstanceInfo`), or a
  new message type that an older reader skips — never a renamed or removed field or message.
  Mixed-version hub / instance / TUI combinations keep interoperating. The MCP client identity
  (`clientInfo.name`, learned at `initialize` — after hub registration) is conveyed by the
  reporter **reconnecting and re-registering** (reconnect-to-relabel), which also re-replays
  state, not by an `UpdateInstance` message.
  - **A reader skips what it does not know.** A message whose `type` this build does not
    recognise is logged and skipped, never treated as a protocol error. Malformed JSON is still
    an error — a desynchronised stream must not pretend to make progress.
  - **The constraint is on the bytes, not on the Rust types.** Restructuring
    `ClientMsg`/`HubMsg` is permitted whenever the JSON is unchanged, and forbidden whenever it
    is not. The messages the hub forwards verbatim are declared once as `HubRelay` and embedded
    in both enums as `#[serde(untagged)] Relay(HubRelay)`, serializing with no envelope
    (`{"type":"ChatToken","token":"…"}`).
  - `hub::relay_wire_compat` asserts the serialized **string** and reads it back with a
    separately-declared enum, because a round trip through the same type moves both ends
    together and a `serde_json::Value` comparison hides a duplicated tag.

- **R24.6 — One task, one row.** An operation visible both through the hub (instance-tagged)
  and through the TUI's direct MCP status poll (untagged) renders once; the hub copy wins
  because it carries instance grouping.

- **R24.7 — One operation identity, computed at the source.** An operation's human-meaningful
  name is **data on the wire**, not a string an observer reverse-engineers. Observers **must
  not** derive an operation's name from its id, its description prose, or any other naming
  convention.
  - `HubEvent::OpStarted` carries `title` (a human command summary computed **server-side**,
    which is the only place that knows the command), plus `cwd`, the full `command`, and
    `origin` — which attached session initiated the work (`cursor` | `claude-code` | `tui` |
    `cli` | `hook` | …). `origin` is the **client's** identity where there is one, not the
    instance label, which is `ahma` for every session.
  - `HubEvent::OpFinished` carries a numeric `exit_code` in addition to its status string,
    because "failed" without an exit code is not actionable.
  - These are `#[serde(default)]` field additions (R24.5); a reader that receives no `title`
    falls back to the tool name.
  - **One identity line** is rendered from these fields and used **identically** in chat
    history, monitor rows, grant prompts (R-PERM.7), and per-operation log names: a status
    glyph, the `title`, the working directory, and the state — elapsed time while running,
    `exit N` plus duration when finished, or the denial reason when denied. An `origin` badge
    is shown when more than one origin is present in view.
  - History replay (R24.2) carries the same fields, so a TUI opened late shows what earlier
    operations were.

- **R24.8 — A pane must not lie about what it is showing.** Every one of the following binds
  **every** scrollable or size-capped pane — chat history, the log tail, operation windows, and
  each full-screen overlay.
  - **R24.8.1 — Scroll position is truthful.** When a pane is scrolled to its last row, its
    scrollbar thumb **must** be flush with the bottom of the track, and when it is at the first
    row the thumb **must not** be. `ratatui::ScrollbarState::content_length` counts scroll
    **positions** (`max_scroll + 1`), not content rows.
  - **R24.8.2 — Layout budgets what the renderer draws.** The height a pane is allocated
    **must** be computed from the same line count the renderer will emit, including any header,
    separator, or footer rows the renderer adds. The two **must** derive from one shared
    function.
  - **R24.8.3 — Overflow drops the preamble, never the outcome.** When output cannot fit its
    pane, the **tail** is what survives — the newest lines and the result. Echoes of the
    command are recoverable from the title or the detail overlay; the result is not.
  - **R24.8.4 — Truncated content is reachable.** Any pane that clips a line at its edge
    **must** offer a way to read the whole line — click-to-open into a wrapped, scrollable
    overlay, or a wrap toggle.
  - **R24.8.5 — A control names its own key.** Where a pane's title or footer advertises a
    toggle state, it **must** name the key that changes it, never an unrelated one.
  - **R24.8.6 — Identity is the footnote, work is the headline.** A detail pane for an instance
    answers *what was asked and how it went* first — outcome tallies and recent operation
    identities (R24.7) — and demotes transport, pid, uuid, and scope to a single dim line for
    connection debugging.

- **R24.9 — One section per client session; borderless; animated.** The TUI's home view is the
  work view; chat is a thing the user then chooses to do (`i` or `/chat`).
  - **A section per client session**, keyed by `session_id` so it keeps its place and its
    open/closed state when its instance re-registers (R-HUB.6). Hooked commands fold into one
    section (a hook is an instance per command), and this TUI's own `!` commands and chat tool
    calls are one section, *this terminal (you)*. A section's header names its client, its
    scope, whether something is running, the running/queued/succeeded/failed tallies, and the
    command it is running or last ran.
  - **Exactly one section is open**; opening another closes it over a 300 ms eased **layout**
    tween — heights move, colour does not. Interrupting a movement continues the outgoing
    section from the height it is currently drawn at rather than snapping back to full height
    first.
  - **Ordering is independent of activity.** A section that starts or finishes work must not
    change position under the cursor; liveness is the glyph and the tallies. Sections order by
    project, then kind, then client, and *this terminal (you)* is last.
  - **No box.** A section is a horizontal rule with its name written into it; there is no
    border around the sections. Overlays and modals keep theirs. The scrollbar stays, and obeys
    R24.8.1.
  - **One layout function.** The renderer and the hit-test read the same positions, so what is
    drawn and what is clickable cannot disagree — including mid-animation, when a section is
    showing fewer rows than it has (R24.8.2). A pane that is closed claims no clicks.
  - Identity — transport, pids, session id — is the footnote in the detail overlay, never the
    header (R24.8.6).

- **R24.10 — A chat turn is always stoppable and never silently lost.**
  - **R24.10.1 — Cancel reaches the loop.** Esc on an empty chat input, or Ctrl-C anywhere,
    stops the running turn: the TUI sends `CancelPrompt`, the hub routes it to the instance
    running the turn, and that instance aborts the agent loop, wakes any approval waiter, and
    ends the turn with one `AgentError`. The TUI ends the turn locally at once, so cancel works
    even when the hub is gone. A second Ctrl-C within 2 s quits.
  - **R24.10.2 — A turn ends visibly.** A prompt that cannot be delivered ends its turn with
    the reason; a model that has gone quiet for 60 s *while thinking or writing* — or for 10
    minutes while still reading its prompt, which is silent by nature — is marked stalled in
    the footer beside the key that cancels it. Waiting on the user or on a running tool is
    never a stall. `AgentError` stops the turn's timer exactly as `AgentDone` does. A model on
    this machine that Ollama is still loading into memory is its own phase ("loading into
    memory", from `GET /api/ps`, asked only of a loopback server and only until the model is
    resident or output starts; carried as `HubRelay::ChatStatus`). It gets the reading
    allowance, and the reading clock — and the measured reading speed — starts only once it is
    loaded.
  - **R24.10.3 — Stream events belong to a turn.** Chat events arriving while this TUI has no
    turn in flight (another TUI's turn, or output from one just cancelled) are ignored rather
    than opening a reply nothing will close. Text written before a tool call is a finished
    reply and is sent back to the model on later turns.
  - **R24.10.4 — Typing is not answering.** While the chat input holds text, gate keys
    (`y`/`a`/`n`, Enter) are text, and every gate says so; they answer once the input is empty.
    A word starting with `a` must never persist "always allow".
  - **R24.10.5 — Quitting never surprises.** `q` and `/quit` quit at once when nothing is
    running; while operations or a turn are running they warn and quit on a second press.
  - **R24.10.6 — An operation is named by its instance and id.** Operation ids are unique only
    per instance, so cancel, pin, detail and live output resolve `(instance, id)`; cancel is
    routed through the hub (`CancelOperation`) to the owning instance, since the TUI's own MCP
    session cannot see another client's operations.
  - **R24.10.7 — A running turn always says what it is doing.** A status line pinned under the
    transcript (outside the scrolled rows) shows the liveness panel and the turn's phase in
    plain words, from real events only: the model reading its prompt (with the prompt's size
    and, once this session has measured the model's reading speed, the time left), thinking,
    writing (with tokens/second), running a named tool, or waiting for the user's answer (a
    still, dimmed panel — never animated as if the model were busy). When a large prompt on a
    slow model is the reason for the wait, a dim hint says what the user can do (`/compact`,
    `/model`). A turn over 10 s leaves a dim one-line summary (time, tokens read, reading and
    writing speed).
  - **R24.10.8 — A dropped connection is retried once, visibly.** When a turn fails for a
    connection reason (timeout, reset, 5xx) before any answer text arrived, the TUI sends the
    message again once and says so in the transcript; a second failure is reported with what to
    do next. A request error (4xx, refused tool, cancel) is never retried. Which is which comes
    from the typed `transient` flag the agent sends with the error (R-HTTP.2), never from the
    error's text. Both the notice and the final error lead with the plain one-line summary of
    which service failed (R-HTTP.3). These notices are the TUI talking to the user and are
    never sent to the model. A model on this machine (loopback endpoint) gets a 30-minute read
    window and no timeout retries (R-HTTP.2).

- **R24.11 — What a glance is for is always on screen.**
  - **R24.11.1 — One status header in every layout.** It **must** show the execution mode
    (R2.1), whether the ahma server and the hub are reachable — a lost connection spelled out
    with how long it has been down, never a glyph flip alone — the transport, and the sandbox
    as locked (R5.4).
  - **R24.11.2 — Each window shows its own LLM and spend.** A section's rule names the model
    its chat uses and, once used, its context fill and tokens in/out; the chat input's title
    shows the same for the window being typed to, plus tokens/second and elapsed time while a
    turn streams. Usage is charged to the in-flight turn's window. Context fill is shown only
    when the model's window size is known (`--context-length` or the provider's `num_ctx`), and
    cost only when a price is known — neither is guessed; an estimate is labelled as one.
  - **R24.11.3 — Chrome earns its place.** A pane that is part of the layout (chat, input,
    command windows, log, `/scope`) is framed by one title rule carrying its name and live
    facts — never a left, right or bottom border. Only floating overlays (pickers, modals,
    gates) keep a full border. A scrollable pane keeps its right column for the scrollbar. The
    work view gets the rows its content needs (at most half the body) and chat fills the rest;
    a conversation shorter than its pane sits at the bottom against the input. The chat agent's
    own tool session is part of "this terminal (you)", and a header never counts "0 clients".

- **R24.12 — It does what the user expects.**
  - **R24.12.1 — Help cannot drift from the keys.** The help overlay and footer **must** render
    from the one key table (`keymap::KEY_REFERENCE`) that tests check both ways: every
    documented key does something, and every bound key is documented.
  - **R24.12.2 — Setup is checked, and keys are not handled.** `/setup` fetches the provider's
    model list before saving (the connection test) and offers only models the provider has. A
    cloud key is read from the environment and registered as a `${VAR}` reference; the TUI
    never asks for or stores the key. A URL-addressed provider carries its configured key to
    the agent loop. Only a client that declared MCP `sampling` at `initialize` (carried as
    `InstanceInfo.sampling`, a field addition per R24.5) is offered as a provider.
  - **R24.12.3 — A window's conversation is its own.** Switching windows switches transcript,
    and is refused while a reply is streaming. Transcripts are saved after each turn under
    `~/.ahma/transcripts/` — never in the project — and restored with `/resume`.
  - **R24.12.4 — Input behaves like a terminal.** ↑/↓ recall earlier input; a paste goes where
    the user is typing and is visible; replies render Markdown; a clipped tool call opens in
    full on click (R24.8.4); `x3` is a message and `/x3` the command.
  - **R24.12.5 — Chat stays on a model that exists.** A client's own model (`mcp://`) exists
    only while that client is connected. When it goes, chat moves to the most recent model ahma
    runs itself (`.ahma/session.toml` `recent`) — or to none, with a pointer to `/setup` — and
    says so; when the client returns and the user has not chosen another model meanwhile, chat
    moves back, also said. Local model servers are looked for again every 30 s while idle.
  - **R24.12.6 — Two levels of explanation.** `/intro` (alias `/getting-started`) shows ahma in
    one screen — one line per topic — and Enter opens a topic's second level. It opens by
    itself once, on the first run (marker `~/.ahma/intro-shown`). It names only commands that
    exist (tested against the command list).
  - **R24.12.7 — Every setting has a place in `/settings`.** Besides the editable tables, the
    panel shows what this folder is trusted with and has been allowed (trust, always-allowed
    tools, folders granted outside it, web allow/deny lists) and which model chat uses. Trust
    and the folder's tool grants change there only on a confirming second keypress, through the
    same audited ledger path as the gates (R-PERM.2.1); everything else in that category names
    the command that changes it. `/settings <words>` jumps to the first matching row.
  - **R24.12.8 — A small model starts with the core tools and asks for more.** Every tool
    schema is prompt the model re-reads on every turn. When the small-model harness is on
    (always, for a model on this machine), the agent offers the core tools — read, list,
    search, write, edit, patch, run a command, await/status/cancel, todo — plus `more_tools`,
    which lists the other groups (one per bundle, `web`, `logs`, `sandbox`, `edit`, `project`,
    one per MCP server) and opens one from the next request on. Opened groups stay open for
    that folder while the process runs. Opening a group is not a permission: every tool still
    goes through approval. A tool that exists but was not offered is refused with the group
    that holds it, never run. The reading line names what is offered (`tools: core + git`); a
    cloud model is offered everything. `more_tools` exists only inside ahma's own agent loop —
    it is never listed to MCP clients.

### R25: Tool-call session reuse (TUI chat)

Chat tool calls from `ahma tui` **must** reuse a single negotiated MCP session rather than
performing a full `initialize`/`roots/list` handshake and spawning a fresh bridge subprocess
per call. The session id established by a tool call's `get_or_create_session` **must** be fed
back into `state.session_id` so every later tool call in the turn — and across turns — reuses
it instead of racing the bridge's session limit.

## 4. Non-Functional Requirements

- **Resource Usage**: Must remain idle (low CPU) when no active rendering or updates are happening.
- **Terminal Recovery**: Must reliably restore terminal raw mode and clear alternate screens on exit, even upon panic.

## 5. Out of Scope

- Implementing the LLM or MCP protocol directly (delegated to the HTTP bridge or stdio endpoints).
