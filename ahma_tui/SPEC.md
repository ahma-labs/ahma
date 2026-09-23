# ahma_tui Crate Specification

* **Status**: Approved
* **License**: AGPL-3.0
* **Depends on**: `ahma_core`, `ahma_mcp`, `ahma_common`, `ahma_http_mcp_client`, `ahma_llm_monitor`
* **Used by**: `ahma_bin` (`ahma tui`)

## 1. User Story / Problem Statement

*As an interactive user, I want a unified terminal UI to monitor running background operations, chat with the local/remote LLM, and review/resolve security approval gates easily.*

## 2. Acceptance Criteria

- **Terminal Dashboard**: Implements a full-screen ratatui terminal user interface with mouse support and Unicode detection.
- **Operations Monitor**: Displays all active, pending, and completed background operations with detailed status views.
- **Unified Work View (SPEC R24, R24.9)**: The home view. One borderless section per client session — a rule with the client, its scope, a liveness glyph, the running/queued/succeeded/failed tallies, and the command it is running or last ran — with hooked commands folded into one section and this TUI's own work into *this terminal (you)*. Exactly one section is open at a time; opening another closes it over a 300 ms eased layout tween. Inside the open section, operations with children indented under whatever spawned them (`parent_id`); Space/click expands one into its live or historic output tail, Enter opens the full-screen detail overlay, `f` toggles this-project/all-projects, and the wheel scrolls the view. Chat is a toggle (`i`, `/chat`), not the screen.
- **Its own work is everyone's work (SPEC R-DAEMON.9)**: the TUI subscribes to the hub *and* registers a reporter connection of its own (`mode: "tui"`, a session id stable for the process), so every `!` command it runs is reported like any other client's work — into the history file, into a second TUI, and into the view after a restart. Those commands run outside the sandbox by design, so they carry `unsandboxed` on the wire and are marked wherever they are drawn: a `!` on the row, `UNSANDBOXED` in the detail pane and the identity footnote. The reporter never starts a daemon and never blocks the UI: the command runs, and shows its output, whether or not the report lands.
- **Current at Startup**: Opening the TUI in a project directory immediately shows work already in flight, and recent work that has finished: the daemon's replay (with `started_epoch_ms`/`ended_epoch_ms` back-dating, the retained output window, and the last hour restored from disk) populates the view before and independent of any MCP handshake, and live project work auto-**opens that client's section** until the first user keystroke.
- **Live Streaming**: Operation windows appear when an operation STARTS (hub `OpStarted`) and stream live output lines end-to-end (`OpOutput` from the unified event stream); polling is demoted to periodic reconciliation against the store of record.
- **Interactive Controls**: Allows pin/unpin and cancellation of running background operations via interactive hotkeys or mouse clicks. Cancel is routed to the instance that owns the operation (SPEC R24.10.6).
- **`/intro` and a complete `/settings` (SPEC R24.12.6, R24.12.7)**: a two-level tour shown once on first run; `/settings` adds Access & trust (confirm-to-change) and Model, and `/settings <words>` jumps to a row.
- **Minimal chrome (SPEC R24.11.3)**: panes carry a title rule only (no side or bottom borders); the work view is sized to its content and chat fills the rest, bottom-aligned; the chat agent's tool session is folded into "this terminal (you)".
- **Trusted folders (SPEC R-PERM.1.3)**: one "Trust this folder?" question on first open replaces per-tool questions for everything that runs inside the folder's sandbox; what crosses the boundary still asks. Parallel calls share one question (R-PERM.1.2).
- **Guided, checked setup (SPEC R24.12)**: `/setup` tests the provider by listing its models and offers only those; cloud keys come from the environment as `${VAR}` references, never typed or stored; sampling providers only for clients that declare it. Help and footer render from the tested key table. Per-window transcripts, saved to `~/.ahma/transcripts/` and restored with `/resume`; ↑/↓ input history; Markdown replies; click a tool call to read it in full.
- **Stoppable, visible turns (SPEC R24.10)**: Esc on an empty input or Ctrl-C cancels the running chat turn end to end; a second Ctrl-C quits. A status line pinned under the transcript says what the turn is doing — a local model loading into memory, reading N tokens (with time left once measured, and which tools a small model is offered, R24.12.8), thinking, writing at N tok/s, running a tool, waiting for you — and a dropped connection is retried once, visibly (R24.10.7, R24.10.8). The footer names the cancel key while a turn runs and flags a model that has gone 60 s silent. `q` asks again while work is running. Gate keys never fire while the chat input holds text. `/compact` keeps the last 4 turns, so the model really sees less.
- **Chat Interface**: Connects to the local LLM and streams chat responses, incorporating animated thinking indicators.
- **Small-Model Context Harness**: When chatting with limited-context local models, per-tool-result output is truncated head+tail with an explicit elision marker, and the conversation is trimmed (system prompt + latest messages preserved, with an injected notice) to fit the model's context. Controlled by `--context-length <tokens>`, `--small-model-harness` / `--no-small-model-harness`, and `--minimize-tokens` / `--no-minimize-tokens`; flags take precedence over settings (`AHMA_*` env vars are ignored, R-CFG1.2).
- **Sandbox Scope Panel (SPEC R5.4(b), R-PERM.5.1)**: `/scope` toggles a sub-window showing the
  scope the server actually locked — every write root, read roots, `--tmp` status, kernel
  enforcement, `source:` provenance, the active-authority line (ahma / nested / deferred-to-host /
  disabled, per R7.5), any platform note (macOS reads-unconfined), and the `sandbox/failed` reason
  when configuration failed. The data is parsed from the full `notifications/sandbox/configured`
  payload (`params.scope`, with top-level-params fallback for older emitters). The header and
  input-box `sandbox:` labels render the **server-locked** primary write root once reported; until
  then the TUI's launch directory is shown with a trailing `?` because it is a guess, not the
  boundary. NESTED/DEFERRED chips use a colour distinct from INITIALIZING.
- **Chat input prefixes**: `/` opens the command navigator, `#` asks the chat model to break a goal into steps (a TUI feature — distinct from the removed MTDF `decompose` tool type), and
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

## 3. Requirements owned here

### R24: Live Task Tree (TUI observability)

`ahma tui` opened in a project directory is a **real-time view of all work
being done on the user's behalf in that project** — by every attached MCP
client (Claude Code, Cursor, Antigravity, …) and by the user's own TUI/CLI
commands — rendered as a compact caller → subtask tree. The view must be
correct **at startup**, not only for events that happen afterwards.

- **R24.1 — Causality is stamped at the source.** Every `Operation` carries an
  optional `parent_id`: the operation — or synthetic group such as
  `session:<id>` for persistent-shell commands — that spawned it. The parent
  link is set where the operation is created (adapter), carried on
  `OperationEvent::Started`, and forwarded on the hub wire
  (`DaemonEvent::OpStarted.parent_id`). Observers **must not** infer hierarchy
  from descriptions or naming conventions.

- **R24.2 — Current at startup ("it just works").** On launch the TUI
  subscribes to the daemon, which replays each instance's retained operation
  history (`OpStarted`, the bounded output window that followed it as ordinary
  `OpOutput` events, then terminal `OpFinished`) before live events — including
  the last hour restored from disk for instances that are no longer attached
  (R-DAEMON.7), so recent work does not disappear because the client that did
  it has closed. An operation still running when its daemon went away replays
  `interrupted`. Replayed events carry wall-clock timestamps
  (`started_epoch_ms` / `ended_epoch_ms`) so elapsed/duration displays are
  **true times, not time-since-receipt**. When the replay reveals live work for
  the current project from an attached client, the TUI **opens that client's
  section** automatically — not merely a view of everything, which the user
  would then have to click into; any user keystroke disarms this auto-open.

- **R24.3 — Project-scoped by default.** The view shows instances whose
  **committed** sandbox scope (R-DAEMON.6) covers, or lives inside, the
  directory the TUI was started in; an instance whose scope is not established
  yet reads *no scope yet* rather than being filtered out — it is not somewhere
  else, it is not yet anywhere;
  `f` toggles all projects. Matching is component-boundary path containment in
  either direction. Instances with no operations still render (an idle,
  attached client is information, not noise).

- **R24.4 — Compact tree with accordion drill-in.** Inside the open section
  (R24.9), one line per task: operations beneath the section header,
  children indented under their parent (session groups, spawned subtasks —
  arbitrary depth). Finished tasks resolve in place to a terminal glyph +
  duration. Space or click on a task expands it inline into its live output
  tail (running) or historic output/result summary (finished); expanding one
  task collapses the previously expanded one (single-expand accordion), and
  Enter opens the full-screen operation detail overlay instead.
  Instance and session headers fold/unfold their subtree.

- **R24.5 — Field-only wire evolution.** The protocol additions
  (`parent_id`, `started_epoch_ms`, `ended_epoch_ms`, `partial`, `interrupted`,
  `unsandboxed` on `DaemonEvent`; `client`, `session_id`, `client_pid`,
  `ended_epoch_ms` on `Register`/`InstanceInfo`) are `#[serde(default)]` **field**
  additions — never new message variants — so mixed-version daemon / instance
  / TUI combinations keep interoperating. The MCP client identity
  (`clientInfo.name`, learned at `initialize` — after hub registration) is
  conveyed by the reporter **reconnecting and re-registering**
  (reconnect-to-relabel), which also re-replays state, rather than by a new
  `UpdateInstance` message.
  - **A reader skips what it does not know.** A message whose `type` this build
    does not recognise is logged and skipped, never treated as a protocol
    error: dropping the connection made every future message addition a hard
    incompatibility on a socket that has no version to negotiate. Malformed
    JSON is still an error — a desynchronised stream must not pretend to make
    progress.
  - **The constraint is on the bytes, not on the Rust types.** Restructuring
    `ClientMsg`/`DaemonMsg` is permitted whenever the JSON is unchanged, and
    forbidden whenever it is not — there is no version to negotiate on this
    socket, so a daemon left running across an upgrade is the reader that
    decides. `HubRelay` is the worked example: the ten messages the hub forwards
    verbatim are declared once and embedded in both enums as
    `#[serde(untagged)] Relay(HubRelay)`, which still serializes as
    `{"type":"ChatToken","token":"…"}` with no envelope.
  - A test that only round-trips a message through its own type **cannot**
    enforce this, because both ends move together; nor can one that compares
    `serde_json::Value`, because a duplicated tag silently collapses in a map.
    `daemon_hub::relay_wire_compat` therefore asserts the serialized **string**
    and reads it back with a separately-declared pre-collapse enum.

- **R24.6 — One task, one row.** An operation visible both through the hub
  (instance-tagged) and through the TUI's direct MCP status poll (untagged)
  renders once; the hub copy wins because it carries instance grouping.

- **R24.7 — One operation identity, computed at the source.** An operation's
  human-meaningful name is **data on the wire**, not a string an observer
  reverse-engineers. Observers **must not** derive an operation's name from its
  id, its description prose, or any other naming convention: a row reading
  `op_41_echo_hello`, or a bare tool name, is a *data* defect, and no formatter
  can repair data that was never sent.
  - `DaemonEvent::OpStarted` carries `title` (a human command summary computed
    **server-side**, which is the only place that knows the command), plus
    `cwd`, the full `command`, and `origin` — which attached session initiated
    the work (`cursor` | `claude-code` | `tui` | `cli` | `hook` | …). `origin`
    is the **client's** identity where there is one, not the instance label,
    which is `ahma` for every session and so tells a reader nothing about which
    window started the work.
  - `DaemonEvent::OpFinished` carries a numeric `exit_code` in addition to its
    status string, because "failed" without an exit code is not actionable.
  - These are `#[serde(default)]` **field** additions, permitted by R24.5;
    mixed-version combinations keep interoperating, and a reader that receives
    no `title` falls back to its legacy heuristics.
  - **One identity line** is rendered from these fields and used **identically**
    in chat history, monitor rows, grant prompts (R-PERM.7), and per-operation
    log names: a status glyph, the `title`, the working directory, and the
    state — elapsed time while running, `exit N` plus duration when finished, or
    the denial reason when denied. An `origin` badge is shown when more than one
    origin is present in view, which is what makes an interleaved timeline of
    IDE-initiated and TUI-initiated work legible.
  - Because history replay (R24.2) carries the same fields, fixing the wire
    fixes late-attach replay for free: a TUI opened *after* an IDE has been
    working shows what those operations **were**, not what their ids looked like.

- **R24.8 — A pane must not lie about what it is showing.** Every one of the
  following binds **every** scrollable or size-capped pane — chat history, the
  log tail, operation windows, and each full-screen overlay — not just the pane
  where the rule was first noticed. The defect this prevents is not a rendering bug
  but a *disclosure* one: the machinery works and the surface fails to say so.
  - **R24.8.1 — Scroll position is truthful.** When a pane is scrolled to its
    last row, its scrollbar thumb **must** be flush with the bottom of the
    track, and when it is at the first row the thumb **must not** be. A thumb
    parked short of the end is indistinguishable from "there is more below",
    which is the single most-reported TUI complaint. Note that
    `ratatui::ScrollbarState::content_length` counts scroll **positions**
    (`max_scroll + 1`), not content rows; passing the row count silently
    produces exactly this lie, and the error shrinks as content grows, so a long
    log tail looks correct while a short chat pane does not.
  - **R24.8.2 — Layout budgets what the renderer draws.** The height a pane is
    allocated **must** be computed from the same line count the renderer will
    emit, including any header, separator, or footer rows the renderer adds. The
    two **must** derive from one shared function; when they disagreed, a `!pwd`
    window was sized for its output alone, rendered two rows taller, and clipped
    away the answer it existed to report.
  - **R24.8.3 — Overflow drops the preamble, never the outcome.** When output
    cannot fit its pane, the **tail** is what survives — the newest lines and the
    result. Echoes of the command are recoverable from the title or the detail
    overlay; the result is not.
  - **R24.8.4 — Truncated content is reachable.** Any pane that clips a line at
    its edge **must** offer a way to read the whole line — click-to-open into a
    wrapped, scrollable overlay, or a wrap toggle. Content the user can see the
    beginning of but can never finish reading is not "displayed".
  - **R24.8.5 — A control names its own key.** Where a pane's title or footer
    advertises a toggle state, it **must** name the key that changes it. Listing
    a state next to an unrelated key ("`[Wrap: Off | …] Press 'l' to switch`",
    where `l` opens the file switcher and `w` wraps) is worse than listing none.
  - **R24.8.6 — Identity is the footnote, work is the headline.** A detail pane
    for an instance answers *what was asked and how it went* first — outcome
    tallies and recent operation identities (R24.7) — and demotes transport,
    pid, uuid, and scope to a single dim line for connection debugging.

- **R24.9 — One section per client session; borderless; animated.** The TUI's
  home view is the work view: what is being done on the user's behalf is what
  someone opens this window to find out, and chat is a thing they then choose to
  do (`i` or `/chat`).
  - **A section per client session**, keyed by `session_id` so it keeps its
    place and its open/closed state when its instance re-registers
    (R-DAEMON.6). Hooked commands fold into one section — a hook is an instance
    per command, so one section each would be a wall of one-line sections — and
    this TUI's own `!` commands and chat tool calls are one section, *this
    terminal (you)*. A section's header names its client, its scope, whether
    something is running, the running/queued/succeeded/failed tallies, and the
    command it is running or last ran, so a closed section is informative rather
    than just a name.
  - **Exactly one section is open**; opening another closes it over a 300 ms
    eased **layout** tween — heights move, colour does not. Interrupting a
    movement continues the outgoing section from the height it is currently
    drawn at rather than snapping back to full height first.
  - **Ordering is independent of activity.** A section that starts or finishes
    work must not change position under the cursor; liveness is the glyph and
    the tallies. Sections order by project, then kind, then client, and *this
    terminal (you)* is last.
  - **No box.** A section is a horizontal rule with its name written into it,
    and the rules are the structure; a border around them is a second frame
    around a frame. Overlays and modals keep theirs. The scrollbar stays, and
    obeys R24.8.1.
  - **One layout function.** The renderer and the hit-test read the same
    positions, so what is drawn and what is clickable cannot disagree —
    including mid-animation, when a section is showing fewer rows than it has
    (R24.8.2). A pane that is closed claims no clicks.
  - Identity — transport, pids, session id — is the footnote in the detail
    overlay, never the header (R24.8.6).
- **R24.10 — A chat turn is always stoppable and never silently lost.**
  - **R24.10.1 — Cancel reaches the loop.** Esc on an empty chat input, or
    Ctrl-C anywhere, stops the running turn: the TUI sends `CancelPrompt`, the
    hub routes it to the instance running the turn, and that instance aborts
    the agent loop, wakes any approval waiter, and ends the turn with one
    `AgentError`. The TUI ends the turn locally at once, so cancel works even
    when the daemon is gone. A second Ctrl-C within 2 s quits. `CancelPrompt`
    and `CancelOperation` are new message types; a reader that predates them
    skips them (R24.5), so against an older instance the turn still ends in the
    TUI while that instance finishes it unobserved.
  - **R24.10.2 — A turn ends visibly.** A prompt that cannot be delivered ends
    its turn with the reason; a model that has gone quiet for 60 s *while
    thinking or writing* — or for 10 minutes while still reading its prompt,
    which is silent by nature — is marked stalled in the footer beside the key
    that cancels it. Waiting on the user or on a running tool is never a stall.
    `AgentError` stops the turn's timer exactly as `AgentDone` does. A model
    on this machine that Ollama is still loading into memory is its own phase
    ("loading into memory", from `GET /api/ps`, asked only of a loopback
    server and only until the model is resident or output starts; carried as
    `HubRelay::ChatStatus`, a new message older TUIs skip per R24.5). It gets
    the reading allowance, and the reading clock — and the measured reading
    speed — starts only once it is loaded.
  - **R24.10.7 — A running turn always says what it is doing.** A status line
    pinned under the transcript (outside the scrolled rows) shows the liveness
    panel and the turn's phase in plain words, from real events only: the model
    reading its prompt (with the prompt's size and, once this session has
    measured the model's reading speed, the time left), thinking, writing (with
    tokens/second), running a named tool, or waiting for the user's answer (a
    still, dimmed panel — never animated as if the model were busy). When a
    large prompt on a slow model is the reason for the wait, a dim hint says
    what the user can do (`/compact`, `/model`). A turn over 10 s leaves a dim
    one-line summary (time, tokens read, reading and writing speed).
  - **R24.10.8 — A dropped connection is retried once, visibly.** When a turn
    fails for a connection reason (timeout, reset, 5xx) before any answer text
    arrived, the TUI sends the message again once and says so in the
    transcript; a second failure is reported with what to do next. A request
    error (4xx, refused tool, cancel) is never retried. These notices are the
    TUI talking to the user and are never sent to the model. A model on this
    machine (loopback endpoint) gets a 30-minute read window and no timeout
    retries: it is silent while it reads the prompt, and re-sending restarts
    that reading from zero.
  - **R24.10.3 — Stream events belong to a turn.** Chat events arriving while
    this TUI has no turn in flight (another TUI's turn, or output from one just
    cancelled) are ignored rather than opening a reply nothing will close.
    Text written before a tool call is a finished reply and is sent back to the
    model on later turns.
  - **R24.10.4 — Typing is not answering.** While the chat input holds text,
    gate keys (`y`/`a`/`n`, Enter) are text, and every gate says so; they answer
    once the input is empty. A word starting with `a` must never persist
    "always allow".
  - **R24.10.5 — Quitting never surprises.** `q` and `/quit` quit at once when
    nothing is running; while operations or a turn are running they warn and
    quit on a second press.
  - **R24.10.6 — An operation is named by its instance and id.** Operation ids
    are unique only per instance, so cancel, pin, detail and live output resolve
    `(instance, id)`; cancel is routed through the hub (`CancelOperation`) to
    the owning instance, since the TUI's own MCP session cannot see another
    client's operations.

- **R24.11 — What a glance is for is always on screen.**
  - **R24.11.1 — One status header in every layout.** It **must** show the
    execution mode (R2.1), whether the ahma server and the daemon are reachable
    — a lost connection spelled out with how long it has been down, never a
    glyph flip alone — the transport, and the sandbox as locked (R5.4). Showing
    these only in a zoomed pane hid a dead daemon from anyone in the default
    layout.
  - **R24.11.2 — Each window shows its own LLM and spend.** A section's rule
    names the model its chat uses and, once used, its context fill and tokens
    in/out; the chat input's title shows the same for the window being typed to,
    plus tokens/second and elapsed time while a turn streams. Usage is charged to
    the in-flight turn's window. Context fill is shown only when the model's
    window size is known (`--context-length` or the provider's `num_ctx`), and
    cost only when a price is known — neither is guessed; an estimate is labelled
    as one.

  - **R24.11.3 — Chrome earns its place.** A pane that is part of the layout
    (chat, input, command windows, log, `/scope`) is framed by one title rule
    carrying its name and live facts — never a left, right or bottom border;
    the terminal's edges already bound it. Only floating overlays (pickers,
    modals, gates) keep a full border. A scrollable pane keeps its right column
    for the scrollbar. The work view gets the rows its content needs (at most
    half the body) and chat fills the rest; a conversation shorter than its pane
    sits at the bottom against the input. The chat agent's own tool session is
    part of "this terminal (you)", and a header never counts "0 clients".

- **R24.12 — It does what the user expects.**
  - **R24.12.1 — Help cannot drift from the keys.** The help overlay and footer
    **must** render from the one key table (`keymap::KEY_REFERENCE`) that tests
    check both ways: every documented key does something, and every bound key
    is documented.
  - **R24.12.2 — Setup is checked, and keys are not handled.** `/setup` fetches
    the provider's model list before saving (the connection test) and offers
    only models the provider has. A cloud key is read from the environment and
    registered as a `${VAR}` reference; the TUI never asks for or stores the key.
    A URL-addressed provider carries its configured key to the agent loop. Only
    a client that declared MCP `sampling` at `initialize` (carried as
    `InstanceInfo.sampling`, field-only per R24.5) is offered as a provider.
  - **R24.12.6 — Two levels of explanation.** `/intro` (alias
    `/getting-started`) shows ahma in one screen — one line per topic — and
    Enter opens a topic's second level. It opens by itself once, on the first
    run (marker `~/.ahma/intro-shown`). It names only commands that exist
    (tested against the command list).
  - **R24.12.7 — Every setting has a place in `/settings`.** Besides the
    editable tables, the panel shows what this folder is trusted with and has
    been allowed (trust, always-allowed tools, folders granted outside it, web
    allow/deny lists) and which model chat uses. Trust and the folder's tool
    grants change there only on a confirming second keypress, through the same
    audited ledger path as the gates (R-PERM.2.1); everything else in that
    category names the command that changes it. `/settings <words>` jumps to
    the first matching row.
  - **R24.12.8 — A small model starts with the core tools and asks for
    more.** Every tool schema is prompt the model re-reads on every turn. When
    the small-model harness is on (always, for a model on this machine), the
    agent offers the core tools — read, list, search, write, edit, patch, run a
    command, await/status/cancel, todo — plus `more_tools`, which lists the
    other groups (one per bundle, `web`, `logs`, `sandbox`, `edit`, `project`,
    one per MCP server) and opens one from the next request on. Opened groups
    stay open for that folder while the process runs. Opening a group is not a
    permission: every tool still goes through approval. A tool that exists but
    was not offered is refused with the group that holds it, never run. The
    reading line names what is offered (`tools: core + git`); a cloud model is
    offered everything, as before. `more_tools` exists only inside ahma's own
    agent loop — it is never listed to MCP clients.
  - **R24.12.5 — Chat stays on a model that exists.** A client's own model
    (`mcp://`) exists only while that client is connected. When it goes, chat
    moves to the most recent model ahma runs itself (`.ahma/session.toml`
    `recent`) — or to none, with a pointer to `/setup` — and says so; when the
    client returns and the user has not chosen another model meanwhile, chat
    moves back, also said. Local model servers are looked for again every 30 s
    while idle.
  - **R24.12.3 — A window's conversation is its own.** Switching windows switches
    transcript, and is refused while a reply is streaming. Transcripts are saved
    after each turn under `~/.ahma/transcripts/` — never in the project — and
    restored with `/resume`.
  - **R24.12.4 — Input behaves like a terminal.** ↑/↓ recall earlier input; a
    paste goes where the user is typing and is visible; replies render Markdown;
    a clipped tool call opens in full on click (R24.8.4); `x3` is a message and
    `/x3` the command.

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
