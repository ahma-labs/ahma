# Ahma Requirements

> This document states **what** Ahma does and **why**, and how its parts fit together.
> Each crate's own `SPEC.md` holds the requirements that crate alone implements; the
> [requirement index](#9-requirement-index) says where each id family lives. How to build,
> test and contribute is in [AGENTS.md](AGENTS.md). User guides are in [docs/](docs/).
>
> Requirement ids are stable: code cites them, and `scripts/check-spec-ids.sh` fails CI when
> a cited id is defined in no spec. Move a requirement, keep its id.

## 1. What Ahma is

Ahma (Finnish for "wolverine") is an MCP server that lets AI agents run a project's real
command-line tools — builds, tests, formatters, git, log tails — inside a kernel-enforced
sandbox scoped to the project, with every command tracked as an operation the agent and the
human can watch, cancel and audit. It exists because the alternative an agent otherwise
reaches for, unrestricted terminal access, is both too powerful and too opaque.

### 1.1 Design invariants

Every requirement below serves one of these. When two requirements seem to conflict, the
invariant decides.

1. **The kernel is the boundary.** Commands run under Landlock (Linux), Seatbelt (macOS) or
   Job Objects (Windows) with a write scope fixed for the session. The scope is committed
   once and never widened afterwards (R5.1). Application-layer checks are defence in depth,
   never the boundary.
2. **Nothing is silent.** ahma never silently disables enforcement, drops an argument,
   skips a check or degrades a guarantee. Whichever sandbox is authoritative, and any
   platform gap, is disclosed where the user will see it (R5.4, R7).
3. **Every call is an operation.** Each tool call is tracked in one `OperationMonitor`:
   visible, cancellable, its full output spilled to a file, recorded in the audit log.
   The execution mode changes only how long the call waits for it (R2).
4. **Agent-writable is not trusted.** Anything inside the scope can be written by the
   agent, so nothing inside it may change what ahma or another trusted program executes
   without a human in the loop: tool definitions are never hot-reloaded, and writes that a
   trusted program later executes are denied or loudly disclosed (R-HANDOFF).
5. **Configuration is explicit and resolved once.** CLI flags and settings files only;
   `AHMA_*` environment variables are not a configuration source. The two most dangerous
   switches are CLI-flag-only. Configuration is immutable after startup (R-CFG).
6. **One ledger, one question.** Every permission — filesystem grants, web domains, tool
   approvals, trusted folders — lives in one ledger outside every sandbox scope, and each
   decision is asked at most once, on the best surface available, failing closed (R-PERM).
7. **One implementation per rule.** A rule that binds two surfaces is implemented once and
   shared (built-in tool names, the MCP handshake client, the event stream, retired-env
   handling), so surfaces cannot drift apart.

## 2. Architecture

### 2.1 Crates and licenses

```text
            ahma_common  (foundation: config, permissions, event stream, hub wire types)
                 ▲
   ┌─────────────┼───────────────────────────────────────────────┐
 ahma_vault  ahma_bundle  ahma_update  ahma_llm_monitor  ahma_http_mcp_client  ahma_http_bridge
 ahma_log_monitor  ahma_harness_guard  ahma_harness_tools   (no deps)
                 ▲
             ahma_mcp  (the engine and the whole `ahma` CLI)
                 ▲
             ahma_core (embedding facade + chat agent)        ahma_simplify (optional)
                 ▲                                                 ▲
             ahma_tui  ──────────────▶  ahma_bin  ◀────────────────┘
             (AGPL-3.0)                 (AGPL-3.0: the `ahma` binary)
```

Everything except `ahma_tui` and `ahma_bin` is MIT OR Apache-2.0, so the engine can be
embedded without copyleft; the end-user product surface is AGPL-3.0. A permissive crate
never depends on an AGPL one (`scripts/check-license-boundaries.sh`). Dev-only crates:
`ahma_test_support`, `generate_tool_schema`, `xtask`, `workspace-hack`.

### 2.2 Processes

```text
 IDE / agent ──stdio──▶ ahma serve stdio (frontend proxy)
                               │ HTTP or Unix socket
 ahma tui ────────────────────▶│
 terminal hooks ──────────────▶▼
                        per-user hub  (bridge + observability hub, R-HUB)
                               │ one per MCP session (R10)
                               ▼
                        ahma serve stdio worker ── sandboxed commands
                        (commits its own scope, R5.1)
```

- An editor launches `ahma serve stdio`. By default that process is a thin proxy to the
  **per-user hub**, started on first use, which hosts the Streamable HTTP endpoint and
  the hub that every surface (TUI, hooks, other windows) reports to and reads from.
- The hub runs **one worker subprocess per MCP session**. The worker owns the session's
  sandbox scope, executes every command, and reports operations to the hub. The hub
  itself runs no commands.
- `ahma serve http|unix` starts an operator-owned bridge with the same session model.
- `ahma tui` is a client of the hub: it shows every session's work and answers the
  permission questions the workers raise.

### 2.3 One tool call

1. The client calls a tool. MTDF tools come from `.ahma/*.json` (§5); built-ins from
   `ahma_mcp::builtin_tool`. Calls wait until the session's scope is committed (R5.1.2).
2. The adapter builds the command and spawns it inside the platform sandbox with the
   locked scope (R5, R6). An execution-audit record is written before the spawn
   (R-HANDOFF.10).
3. `OperationMonitor` tracks it and is the only emitter onto the `OperationEvent` stream
   (§2.4). Output is redacted, spilled in full to `<log dir>/operations/<id>.log`, and kept
   as a bounded tail.
4. In `sync` mode the call returns the result when the command ends, within the client's
   request budget; in `async` mode it returns the operation id after a short window and the
   client collects the result with `await` (R2).
5. Subscribers turn the stream into MCP progress notifications, hub events for the TUI,
   and audit records.

### 2.4 Unified operation event stream

All operation lifecycle data flows through one broadcast stream of
`ahma_common::event_dispatcher::OperationEvent` (`Started` / `OutputLine` / `Progress` /
`Alert` / `Completed` / `Failed` / `Cancelled` / `TimedOut`):

- **Single emission point**: `OperationMonitor` emits every lifecycle event, and exactly one
  terminal event per operation. No other component emits them.
- **Ordering**: on a terminal transition the monitor writes history, signals the completion
  watch, then emits the event, so a reader woken by the watch sees complete history.
- **The stream is a feed, not the store of record**: a lagging subscriber reconciles from
  monitor state; `await` never depends on the broadcast.

### 2.5 Execution mode resolution

`tools.execution_mode` (`sync` | `async`) is a server-operator decision, resolved once at
startup: `--sync` / `--async` (the last one on the command line wins), then the project and
user settings files, then the default **`async`** — safe as a default because writers are
ordered by the workspace write queue (R2.7). Per call, an MTDF tool may pass `blocking: false`
to return after the adaptive inline window (R2.6.1) in sync mode; `run_terminal_command` has
no caller-selectable mode (R2.6.3). What each mode does is R2.1; the `initialize`
instructions describe the session's mode (R1.5.4).

### 2.6 Trust boundaries

| Zone | Who can write it | Rule |
|---|---|---|
| The session scope (workspace) | the agent | Everything here is untrusted input to ahma (invariant 4) |
| `~/.ahma` (settings, ledger, logs outside the tree) | the user, ahma outside the sandbox | Never inside any sandbox scope (R5.4.8), so the agent cannot read or extend its own grants |
| Paths a trusted program executes (git hooks, editor/harness auto-run config, hub sockets) | — | Deny-write, or allow with loud disclosure (R-HANDOFF) |
| The network | — | Outbound unrestricted unless `--restrict-network` (R-WEB.16); listening never restricted, disclosed (R-LISTEN); `fetch_webpage` is governed by `[web]` (R-WEB) |

## Quick Status

`tests-pass`: implemented and covered by tests. `in-progress`: partly done; what is missing
is listed in §11 Known gaps. `experimental`: implemented and tested, behind an opt-in, and
may change.

| Component | Status | Notes |
|---|---|---|
| MTDF tool execution, schema validation, sequences | tests-pass | R1, R4, §5 |
| Tracked operations, async by default | tests-pass | R2; mode resolution §2.5 |
| Workspace write queue | tests-pass | R2.7; kernel-enforced read-only lane on Linux/macOS, none on Windows (R2.7.4); `ahma queue` (R2.7.9); `ps` under the profile (R6.2.8) |
| Unified operation event stream | tests-pass | §2.4 |
| Output spill files | tests-pass | `<log dir>/operations/<id>.log`, advertised as `output_file` |
| Built-in tools | tests-pass | `BuiltinTool::ALL`; file tools withheld from clients with native ones (R26) |
| Tool reload | tests-pass | Explicit `restart` only (R1.4) |
| Linux sandbox (Landlock) | tests-pass | Reads and writes confined; deny tier application-layer only, shell writes to it detected and reported (R6.1.7) |
| macOS sandbox (Seatbelt) | tests-pass | Writes confined; reads unconfined except a credential denylist (R6.2.2, R6.2.3); signals and GPU confined (R6.2.6, R6.2.7) |
| Windows sandbox | in-progress | Job Objects only; no OS path boundary (R6.3.3) |
| Nested sandbox detection and deferral (R7) | tests-pass | Hooks and MCP server apply ahma's own sandbox; defer only on kernel proof (R7.6) |
| Trust-handoff hardening (R-HANDOFF) | in-progress | Kernel-enforced on macOS; elsewhere ahma's file tools refuse and shell-command writes are detected after the fact (R6.1.7) |
| Execution audit log | tests-pass | `<log dir>/audit.jsonl` on every execution path (R-HANDOFF.10) |
| Unified permissions and doctor | tests-pass | One ledger under `~/.ahma`; `ahma doctor [--fix]` (R-PERM, R-DOCTOR) |
| Grant prompts | tests-pass | One body on every surface, `grant_prompt::render` (R-PERM.3.4); per-session budget (R-PERM.4.5) |
| Grant advisor and decision habits | tests-pass | R-PERM.8, R-DOCTOR.8 |
| Scope-downgrade prompts (R5.3) | tests-pass | `--tmp` asked as a session grant (R5.2.5); one decision per question across surfaces (R5.3.3) |
| Configuration standard (R-CFG) | tests-pass | Sources, tiers, provenance (`ahma settings show --origin`, startup report), strict parsing |
| STDIO, HTTP bridge, Streamable HTTP, session isolation | tests-pass | R8, R10 (`ahma_http_bridge/SPEC.md`) |
| Per-user hub (R-HUB) | tests-pass | One `AF_UNIX` rendezvous on every OS (R-HUB.2) |
| HTTP MCP client | tests-pass | OAuth 2.0 + PKCE (`ahma_http_mcp_client/SPEC.md`) |
| Web egress policy for `fetch_webpage` (R-WEB) | tests-pass | Three-tier approval, private-range block, redirect guard |
| Subprocess egress restriction (R-WEB.16) | tests-pass | Opt-in `--restrict-network`; kernel-enforced on macOS and Linux 6.7+ |
| Outbound HTTP retry and failure wording (R-HTTP) | tests-pass | `ahma_common::http_retry`; SSE reconnect and `xtask` not covered (R-HTTP.4) |
| Live log monitoring (`livelog`, `--log-monitor`) | tests-pass | §5.5, R9 |
| TUI | tests-pass | R24, R25 (`ahma_tui/SPEC.md`) |
| Task vaults | experimental | `--task-vault` (`ahma_vault/SPEC.md`) |
| Bundle audit | experimental | Checksum is not a signature (`ahma_bundle/SPEC.md`) |
| `ahma setup` / `ahma uninstall` | tests-pass | R-SETUP, R-UNINSTALL |
| Self-update and provenance | tests-pass | `ahma_update/SPEC.md` |
| Release signing (R-SIGN) | in-progress | Developer-ID signing wired, awaiting the Apple secrets (R-SIGN.1); Windows is a stated assumption (R-SIGN.3) |
| OpenTelemetry export | tests-pass | `otel` cargo feature, off by default |
| Code complexity analysis (`ahma simplify`) | tests-pass | `ahma_simplify/SPEC.md` |

---

## 3. Core Requirements

### R1: Configuration-Driven Tools

- **R1.1**: The system **must** adapt any CLI tool for use as MCP tools based on declarative JSON configuration files.
- **R1.2**: All tool definitions **must** be stored in `.json` files within a `tools/` directory (default: `.ahma/`).
- **R1.2.1**: **Auto-Detection**: When `--tools-dir` is not explicitly provided, the system **must** check for a `.ahma` directory in the current working directory. If found, it **must** be used as the tools directory. If not found, the system **must** log a warning and operate with only the built-in tools (`ahma_mcp::builtin_tool::BuiltinTool::ALL`).
- **R1.2.2**: When `--tools-dir` is explicitly provided via CLI argument, that path **must** take precedence over auto-detection.
- **R1.3**: The system **must not** be recompiled to add, remove, or modify a tool.
- **R1.4**: **Reload is explicit, never watched.** The system **must not** watch the tools directory for changes. A tool definition is a command ahma will run and lives inside the agent-writable workspace, so a watcher would turn writing that file into executing it with no human in between (R-HANDOFF.1, R-HANDOFF.7). Reload happens only through the explicit `restart` tool.
  - `restart` replaces the process on every transport (bridge: `POST /restart` → `terminate_all` → `exit(0)`), so it does not send `notifications/tools/list_changed` and **must not** be expected to: no session survives to receive it. The client learns the new tool set by re-initializing.
  - The notification **must** be sent on the one path where the tool set changes inside a live session: `mcp_service::sandbox_config::update_tools`, which overlays a connecting client's `<root>/.ahma/` mid-handshake.

### R1.5: Built-in Tool Names Are Reserved

* **R1.5.1**: The tools ahma implements itself are declared once
  (`ahma_mcp/src/builtin_tool.rs`). The protocol dispatch, the client-visibility
  filter, the harness-guard name healer and the config validator all read that
  one declaration, and the dispatch match is exhaustive over it — a built-in
  that is declared but not dispatched fails to compile rather than returning
  "tool not found".
* **R1.5.2**: A configured tool **must** be refused at load if its name is one
  of them, with an error naming the conflict and the file to rename. Silently
  loading it and then filtering it out of `tools/list` is not acceptable: the
  user gets neither the tool nor a reason.
* **R1.5.3**: Adding a built-in **must** force an explicit answer to whether it
  is exempt from the sandbox-ready gate (R5.1.2), whether ahma's own agent loop
  may call it, and whether it is a harness file tool withheld from clients with
  native equivalents. These are exhaustive matches, not membership lists.
* **R1.5.4**: The `instructions` field of the MCP `initialize` response directs the model to
  use `run_terminal_command` for command execution and describes the session's actual
  execution mode (R2.1).

### R2: Tracked Operations, Async by Default

- **R2.1**: **Every call is a tracked operation; the mode decides how long it waits.** A tool call **must** start its command as an operation in `OperationMonitor` in both modes, so it is visible to `status`, the TUI and the audit log, cancellable, and spills its full output to `output_file`. `tools.execution_mode` (§2.5, R2.4; described to the model per R1.5.4) then decides the answer:
  - **`sync`** — the call waits for the operation to finish and returns its result in the inline format (R2.6.2). The wait is bounded exactly as a default `await` on it would be (R2.5, R2.6.5, R2.6.5.3), because a result written into a connection the client has abandoned is lost; an operation still running at that bound returns its id with a statement that it is still running and how to collect it (`await`). Progress keeps flowing to the call's token during the wait. A sequence waits for its steps within one such window.
  - **`async` (the default)** — the call waits the adaptive inline window (R2.6.1), then returns the operation id; the caller collects the result with `await`, and keeps thinking, reading and planning meanwhile. Operations that may write the workspace still run one at a time, in arrival order (R2.7).
  - The legacy direct execution path (not a tracked operation) is used only by CLI one-shot mode and by the deprecated `blocking: true` / `"synchronous": true` (R2.3).
  - **Why async is the default**: a long command (`cargo nextest run`) overlaps the model's own thinking instead of stalling it, and the workspace write queue (R2.7) keeps two commands from interleaving their effects on the workspace; a short command still answers inline within the adaptive window (R2.6.1).
- **R2.2**: On completion, the system **must** store results reliably in `OperationMonitor` (pull channel) and **should** push a best-effort MCP progress notification. Clients rely on the `await` tool for guaranteed result delivery; the push notification is an optimistic shortcut to avoid a round-trip.
- **R2.2.1**: **A per-client progress suppression must be overridable, and must say so when it isn't measured.** `McpClientType::supports_progress()` may suppress R2.2's push for a specific client (currently: Cursor, believed to log a client-side error for valid progress tokens). MCP notifications are one-way, so ahma cannot observe whether the behaviour it works around still exists; unlike the elicitation-budget table (`McpClientType::elicitation_budget`, R5.3.1), which is set from captured measurements, a progress suppression **must** say in its own doc comment when it is asserted rather than measured. `tools.force_progress_notifications` / `--force-progress-notifications` **must** let an operator override the suppression uniformly once it is known to be stale.
- **R2.3**: **Static `synchronous` flag (deprecated).** `"synchronous": true` in an MTDF definition selects the legacy direct (untracked) path; new definitions omit it and rely on `tools.execution_mode`.
- **R2.4**: **Resolution.** The server's mode is `tools.execution_mode`, resolved per §2.5. `--sync` and `--async` override each other, last one wins, because a worker receives its hub's flags first and its session's after them. The retired `tools.force_sync` key is parsed and ignored; an operator who wants every call to block sets `execution_mode = "sync"`.
- **R2.5**: **`await` soft timeout.** The `await` tool waits at most `tools.await_timeout_secs` seconds (default `1800`, i.e. 30 minutes). Resolution order: the call's optional `timeout_seconds` argument, then the `--await-timeout` CLI flag, then `tools.await_timeout_secs` in `settings.toml`, then the compiled-in default. When awaiting by tool filter and no explicit `timeout_seconds` is given, the effective wait is `max(default, longest pending operation timeout)` so a legitimately long operation is never cut short by a shorter await default.
- **R2.5.1**: The timeout is **soft**: expiry **must not** cancel the awaited operation(s), and the returned text **must** state that the work is still running in the background and that the client should call `await` again (by `id` where one was given) to keep waiting. This distinguishes "your wait ended" from "your operation died".
- **R2.5.2**: The resolved timeout from R2.5 **must** be the only deadline on the wait — no inner bound may pre-empt it. `OperationMonitor::wait_for_operation`'s own default cap is shorter than the await default, so `await` **must** opt out of it (`wait_for_operation_bounded(id, None)`). Otherwise expiry past that inner cap is misreported as "completed but no result available" (by `id`) or silently drops a still-running operation from an apparently successful result (by tool filter), defeating R2.5.1.
- **R2.5.3**: **Progress follows the waiting request.** While an `await` is in flight, progress notifications for the operations it is waiting on **must** carry *that request's* `progressToken`, not the token of the `tools/call` that started the operation: the starting call's token is retired by the time anyone awaits, so a validating client discards notifications sent under it and a long `await` shows no liveness. The displaced target **must** be restored when the await returns, so the best-effort completion push (R2.2) still lands.
- **R2.5.4**: **Push notifications assume a listening caller.** The R2.2 push is delivered over the *same live transport connection* that started the operation (`Peer<RoleServer>::notify_progress` in `progress_push.rs`); it is not queued and never replayed, and nothing wakes a caller that has stopped listening. A caller that might stop listening before the operation completes (ends its turn, hands off, disconnects, or exits) **must** block on `await` before doing so. The server `instructions` (`mcp_service/mod.rs`) and the `/ahma` skill doc carry the caller-facing version of this warning.
- **R2.5.5**: **A timeout's accuracy is not a substitute for checking.** How promptly `await` returns and whether the work is finished are different problems, and only the caller can solve the second. The server `instructions` (`mcp_service/mod.rs`) and the `/ahma` skill doc **must** tell the caller to confirm every `operation_id` it started has reached a terminal state before declaring a task complete. This is disclosure, not enforcement: MCP gives the server no way to stop a client summarizing prematurely.

#### R2.6: The inline/async decision belongs to ahma

An operation that finishes quickly should not cost the model an extra `await`
round-trip, and one that takes minutes must not hold an MCP request open. Which
of the two is happening is not known when the call arrives, so ahma waits a
bounded window and decides from the outcome.

- **R2.6.1**: **Adaptive inline window (async mode).** A `tools/call` that spawns an async operation **must** wait a bounded window for it to finish and return the result inline if it does; otherwise it returns the operation id. The window is **not** a fixed constant — it is chosen from whether the session has other operations in flight:
  - **nothing else running** → the long window (`INLINE_WINDOW_IDLE_SECS`). The model has nothing to overlap with and its next move would be `await` anyway, so the wait is free wall-clock and may save a whole LLM turn.
  - **operations already in flight** → the short window (`INLINE_WINDOW_BUSY_SECS`). The model is fanning out; holding this response delays the next command.

  The window **must** additionally be clamped to at most half the caller's single-request budget (R2.6.5), so the call that was meant to save a round-trip can never instead exceed what the client will wait for.
- **R2.6.2**: **A completed operation always states its outcome.** An inline result **must** begin with the operation identity line (R24.7) — what ran, exit code, duration — followed by the command's output, or `(no output)` when it produced none. Bare stdout is not sufficient: a successful silent command (`cargo fmt --check` on clean code) would yield an empty result a model cannot distinguish from a broken tool. A progress notification is best-effort (R2.2) and **must not** be the only place the exit code and duration appear.
- **R2.6.3**: **No caller-selectable synchronous mode on `run_terminal_command`.** Its input schema **must not** advertise `sync`, `blocking`, `execution_mode`, or any equivalent. Models do not use such a flag selectively — they default to it — and a synchronous `cargo` build blocks one MCP request for longer than several clients tolerate. `execution_mode` remains accepted but unadvertised as the CLI/test escape hatch; the server's mode is the operator's (§2.5). In sync mode a command still returns its result without the model asking, bounded by the client's budget.
- **R2.6.4**: **Ignored arguments are disclosed.** When `run_terminal_command` receives arguments it does not act on, it **must** execute normally and name them in the result. Silently dropping an argument leaves a model believing it took effect.
- **R2.6.5**: **Fallback single-request budget.** When no live-channel signal is available (R2.6.5.3), ahma **must** bound how long it holds one MCP request open by one conservative budget, the same for every client (`McpClientType::request_budget`, `FALLBACK_REQUEST_BUDGET`) — not a per-`clientInfo.name` table, because a product name is no evidence that the connection is alive. Both the R2.6.1 window ceiling and the R2.5 `await` default are bounded by it.
- **R2.6.6**: **A time limit is announced before it stops anything.** A command may run for at most `[tools] timeout_secs` (default 1800 s), which a repository's own `.ahma/settings.toml` may raise for that repository (it is a preference, R-CFG2) and an MCP call may override with `timeout_seconds`. An async operation **must** be alerted once when it has used 80% of its limit, and a command stopped at the limit **must** be told, in the same message, where the limit lives and how to raise it. A healthy 25–30 minute build gate sat at the 30-minute default with neither.
- **R2.6.5.1**: **A shortened wait says so.** When the fallback budget cuts an `await` below the timeout resolved for it, the result **must** state the wait that was applied, the wait that was asked for, and that the operation is still running; otherwise a caller cannot tell a deliberate cap from a hung operation. An explicit `timeout_seconds` argument is honoured verbatim (R2.5.1) and **must not** be reported as capped.
- **R2.6.5.2**: **The fallback is overridable.** `tools.request_budget_override_secs` (settings.toml) / `--request-budget-secs` (CLI) **must** let an operator replace the effective fallback budget for every client uniformly. A `clientInfo.name` that matches no known client (`McpClientType::Unknown`, which gets the conservative defaults) **must** be logged at `warn`, once per distinct name per process, naming the override.
- **R2.6.5.3**: **A confirmed live channel is verified directly, not guessed.**
  - *Probing.* When `AhmaMcpService::push_channel_open()` confirms the bridge has a live push channel to the real client, `await` **must not** apply the R2.6.5 clamp: it uses the full resolved R2.5 timeout and sends a bare MCP `ping` every `LIVENESS_PROBE_INTERVAL`, each bounded by `LIVENESS_PROBE_TIMEOUT`. The first failed probe ends the wait as the R2.5.1 soft timeout, since the caller-facing outcome is the same.
  - *Channel signal.* `ahma_http_bridge` sends the subprocess `notifications/ahma/pushChannelChanged` (params `{"connected": <bool>}`, typed as `ahma_common::mcp_methods::PushChannelChangedParams`) whenever its session's SSE stream opens. `push_channel_open()` defaults to `false` for a worker behind the bridge; a session with no confirmed channel (before the internal proxy hop's SSE opens, or a configured default sandbox scope that lets a client skip SSE by design) **must** fall back to R2.6.5. A process serving the editor **directly** over stdio (the in-process fallback, R-LIFECYCLE.3) starts with it `true`: the pipe is the client, and the probe's `ping` goes to the client itself.
  - *The bridge answers the probe iff it holds the live channel.* `ahma_http_bridge` **must** answer a subprocess-initiated `ping` itself when the session has at least one SSE subscriber, and **must** leave it unanswered when it has none, so the probe times out and the wait ends on the "client gone" verdict. Why: rmcp's streamable-HTTP client awaits each POST inline, so a ping relayed through ahma's stdio proxy would not reach the client until the `tools/call` response lands, while the live SSE stream is itself the liveness being verified. When the client dies its proxy exits, the stream closes, and the next probe goes unanswered.
  - *Requests are not responses.* A subprocess message that carries a `method` is a request: the bridge **must not** match it against a pending client call by id, because the subprocess's and the client's request-id counters are independent and collide.
- **R2.6.5.4**: **A wait that ends early reports the time that passed, never the time that was asked for.** When a liveness probe ends an `await` (R2.6.5.3), the result **must** state how long the wait actually lasted (whole seconds, never exceeding the wall clock), state the requested timeout separately, say that the wait ended because the client stopped answering probes rather than because the timeout expired, and carry the R2.5.1 still-running notice.

#### R2.7: The workspace write queue — safe async

Async overlaps a long command with the model's thinking. Without ordering it also lets two
commands mutate one workspace at once, or lets the model edit sources while a test run is
compiling them. The workspace write queue keeps the overlap and removes the interleaving:
the model thinks concurrently; the workspace is written by one ahma command at a time, in
the order the model asked. It is a lock, not a scheduler — ahma never reorders, merges or
drops work — and it is built so that no crash, kill or power loss can leave it held.

- **R2.7.1**: **One writer per workspace, in arrival order.** Every operation in the
  *exclusive* lane (R2.7.4) **must** take its workspace's lease before it spawns and hold it
  until its whole process tree has exited. A place in line **must** be taken synchronously
  when the call arrives, before anything is awaited or spawned, so arrival order — not task
  scheduling — is execution order. A queued operation stays `Pending`, is cancellable, and
  **must** keep proving liveness (the idle-output watchdog must not kill it for being
  queued); its wait is bounded by its own timeout. When it leaves the queue its clocks
  restart, so its reported duration and its timeout measure the command, and the wait is
  recorded (`queue_wait_ms`) and stated in its result. Every execution path takes part: the
  async path, the synchronous path (terminal hooks, CLI one-shots, `synchronous: true`
  tools), PTY and persistent-session commands. The synchronous path has no operation to show
  as queued, so it **must** announce a wait once (a terminal hook writes it to stderr, where
  the harness shows it) and, if its turn does not come within the command's own timeout,
  fail saying the command was **not run** and who held the workspace — never hang silently
  until the caller gives up. `tools.workspace_queue = false` turns it off.
- **R2.7.2**: **The workspace is the repository.** The key is the nearest ancestor of the
  working directory that contains `.git` (a directory, or the file of a linked worktree — so
  worktrees are separate workspaces and the unit of parallelism); without one, the longest
  sandbox scope containing the directory; without that, the directory. Paths are
  canonicalized first. Unrelated workspaces never wait for each other. Command mutex groups
  (`tools.mutex_groups`) are keyed the same way — never by working directory, because
  `cargo test` in a member crate and `cargo build` at the root share one `target/` — and
  match the command line the caller wrote, not the shell program that runs it.
- **R2.7.3**: **A command that has not started says so.** When a call returns while its
  operation is still waiting for its workspace, the result **must** say `NOT started —
  queued behind` the operation(s) ahead of it, named by id, command and age, that it will run
  automatically in order, and that it must not be sent again. A queued `sed -i` that reads
  like an applied one is the failure this prevents.
- **R2.7.4**: **Three lanes, and the read-only lane is the kernel's word, not ahma's.**
  - *Exclusive* (the default) takes the lease.
  - *Read-only* takes none and never waits. The command is spawned with a sandbox that
    grants the workspace (and its git storage) **read and execute only** — Landlock on
    Linux, a write-free Seatbelt profile on macOS — plus `GIT_OPTIONAL_LOCKS=0`, so a
    command wrongly classified read-only fails with a permission error; it cannot write.
    Where the kernel cannot enforce it (Windows, Test-mode sandboxes, a macOS ahma nested
    in another Seatbelt profile, a kernel without Landlock) the read-only lane **must not**
    exist and every command is exclusive.
  - *Service* — long-lived by design, like the log monitors (`livelog`) — takes no lease,
    because holding it for a server's life would stall every later command.
  - *MTDF tools* take their lane from `concurrency` (`exclusive` | `read_only` |
    `service`), the nearest declaration winning (subcommand over parent subcommand over
    tool), resolved once when the definition is parsed. The bundled tools **must** declare
    their lanes, because the default is exclusive: their plain reads (`file-tools`
    `ls`/`cat`/`grep`/…, `git` `status`/`log`, `gh` list/view commands) are `read_only`, and
    `gh run_watch`, which follows a CI run for minutes, is `service`.
  - *Shell command lines* are classified by a conservative classifier. It reads as
    readers: plain readers (`git status/diff/log/show`, `rg`, `grep`, `ls`, `cat`, `tail` —
    followers included, since a reader that never ends must not hold the workspace for its
    whole life —, `ps`, `sed` without `-i`, `gh` viewing commands, `curl` without an output
    file, ahma's own listing commands, …); **pipelines and lists** of readers (`grep … |
    head`, `cd src && ls`, `2>&1`, `>/dev/null`, a plain `$VAR`); `sleep`; a `NAME=value`
    assignment before a reader, or on its own (`S=/path; grep … $S`); an
    `until`/`while`/`if`/`for … in` list (a standalone `!` negating a command included) whose every command reads (a CI poll such as
    `until gh pr checks 87; do sleep 60; done`); and the system diagnostics an agent runs to
    see why a job is slow (`uptime`, `sysctl` without `-w` or `name=value`, `vm_stat`,
    `iostat`, `top`, `lsof`, `pgrep`). Those must never queue behind the job they inspect. A `$(…)` substitution is judged by the command it runs, so
    `echo "$(gh pr view 87)"` is a reader and `echo $(rm x)` is not. It treats a backtick
    substitution, a writing redirection, grouping, an escape or an unknown program as
    exclusive. It is permissive only where the kernel lane makes a mistake harmless.
- **R2.7.5**: **No result is lost to a forgotten `await`.** Each session remembers every
  operation it started whose call returned without the result. Once one has finished, its
  outcome (identity line, output tail, output file) **must** be prepended to the next tool
  result the session returns, whatever the tool, exactly once. An `await` delivers exactly
  the operations it renders in its answer — never one it did not show (an `await` without an
  id waits only for operations still running, and a `tools` filter excludes others), which
  is then prepended to that same answer instead.
- **R2.7.6**: **Drift is reported, because not every writer can be ordered.** A harness's
  own editor (Claude Code's `Edit`) never passes through ahma. After an operation of at
  least two seconds that is not a service, ahma **must** list the files in its workspace
  whose modification time falls inside its run — `.gitignore`d paths, `.git/` and ahma's log
  directory excluded, at most 20 named, the walk bounded, and never outside the sandbox
  scope (a `.git` above every scope may key the lease, but the walk is clipped to the
  longest scope containing the working directory) — in the result
  (`changed_during_run`), saying who could have written them: anyone including the command,
  or, for a read-only command, someone else. It is detection, not prevention: optimistic
  concurrency, with no watcher and nothing to clean up.
- **R2.7.7**: **No stale locks, by construction.** The cross-process half of the lease is
  an advisory kernel lock (`flock` / `LockFileEx`) on a rendezvous file in the per-user
  runtime directory (`<runtime dir>/locks`), **never** inside any sandbox scope, where the
  agent could delete it while it is held. The kernel releases it when the holder dies for
  any reason; no lock is ever represented by a file's existence. The holder publishes who it
  is beside it (advisory; the kernel lock is authoritative). A command run under a lease
  inherits `AHMA_HELD_WORKSPACE_LEASE`, and an ahma it starts does not wait for a lease its
  ancestor holds (which would deadlock). The detached per-user hub **must not** inherit
  it: it outlives the tree that started it, and it and every session worker it spawns would
  otherwise skip that workspace's lock for good. If the rendezvous file cannot be opened,
  ordering degrades to in-process only, with a `warn` — it never wedges. Within one process
  the order is strict FIFO; across processes the kernel lock guarantees mutual exclusion but
  not arrival order.
- **R2.7.8**: **Edits wait for commands that rewrite sources.**
  - *Refusal.* ahma's own file tools (`write_file`, `replace_in_file`, `multi_edit`,
    `apply_patch`) **must** refuse an edit while an exclusive operation that may rewrite
    source files holds the edited file's workspace, naming it, how long the same command
    usually takes, and saying to `await` or `cancel` it (`tools.edit_guard`, default on).
    The full refusal is given once per running command; every later refusal for the same
    command is one line. The file tools and the hook below share one decision
    (`WorkspaceQueue::edit_conflict`).
  - *What blocks.* What a holder does to sources is classified where the command is known
    and published with the holder (`SourceEffect`). A build, test or linter *reads* sources
    and **must not** block an edit: the drift report (R2.7.6) already says the run may have
    seen it, and a refusal would force an `await` that undoes the overlap R2.7 exists for. A
    formatter, codemod, checkout or package manager *rewrites* them and blocks. Anything
    unrecognised is treated as a rewriter, but blocks only inside the subtree it runs in
    (its footprint) when that is narrower than the workspace. A holder record without these
    fields (written by an older ahma) blocks every edit.
  - *Project readers.* A project may declare its own source-reading wrappers
    (`tools.source_readers`, a preference-tier list of command prefixes matched in whole
    words); a wrong entry costs a stale verdict the drift report names, never confinement.
  - *Pre-edit hooks.* `ahma hooks install` **must** install the same check as a pre-edit
    hook in every supported client that has one — Claude Code, Codex (`apply_patch`, paths
    read from the patch), Copilot CLI, Cursor and Antigravity; VS Code's agents run those
    files — each in that client's own matcher and decision format, under its own managed id
    so `uninstall` and `status` see it apart from the shell hook. The hook never waits,
    always exits 0, and emits only a deny; otherwise no decision where the client's contract
    defines "no decision", and the plain `allow` ahma's shell hook already sends where that
    is unverified (Cursor, Antigravity; R5.5.5). It checks tool names itself, because some
    hosts (VS Code's Local agent) ignore matchers. It cannot see the server's sandbox
    scopes, so outside a git repository it also probes every ancestor of the edited file
    that has a rendezvous file — the server keys such a workspace by its scope. A string is
    read for patch paths only when it *is* a patch (`*** Begin Patch`), never when file
    content merely quotes one.
  - A client with no pre-edit hook, or one that does not enforce the deny, is covered by
    R2.7.6 alone.
- **R2.7.9**: **The queue is visible, and seeing it never queues.** `ahma queue` lists every
  workspace lease published beside its lock — the holder's command, pid, age, session and
  whether that process is still alive — with a first line that says whether anything is
  blocked at all (R-PERM.9), and the same facts appear in the TUI and the `status` tool;
  `ahma queue` and `status` render them with one function (`queue_report`), so they never
  disagree. It reads the records without taking a lock, so an agent whose every command is
  waiting can still run it. A dead holder is listed as dead (its OS lock is already gone,
  R2.7.7), never hidden. A lock directory that exists but cannot be read — inside ahma's
  own sandbox it is out of scope by design — **must** be reported as "cannot tell", with
  where to look instead, and never as "no workspace is held".

### R3: Performance

- **R3.1**: Command dispatch **must** stay low-latency via direct sandboxed spawns; the budget is enforced by the `latency_guard_test` benchmarks.
- **R3.2**: Persistent shell sessions (opt-in via `session_id`) are tracked per session and automatically cleaned up on shutdown.

### R4: JSON Schema Validation

- **R4.1**: All tool configurations **must** be validated against the MTDF schema at server startup.
- **R4.2**: Invalid configurations **must** be rejected with clear error messages.
- **R4.3**: Schema supports: `string`, `boolean`, `integer`, `array`, required fields, and `"format": "path"` for security.

---

## 3.5 Configuration Standard (R-CFG)

Server configuration (everything except MTDF tool definitions) **must** be deterministic, inspectable, and tamper-resistant. Environment variables are ambient, persistent state: they leak across sessions, are settable by any process sharing the user's environment, and are invisible at the invocation site, so they are not a configuration source. R-CFG is the single source of truth for configuration resolution.

### R-CFG1: Configuration Sources and Precedence

- **R-CFG1.1**: There are exactly four configuration sources, resolved highest-precedence first:
  1. **CLI flags** — including flags passed via the `args` array in an IDE's `mcp.json`. This is the canonical way to configure ahma per-project from an MCP client.
  2. **Project settings** — `<workspace>/.ahma/settings.toml` (Preference-tier keys only, see R-CFG2).
  3. **User settings** — `~/.ahma/settings.toml` (or `--settings-path <file>`).
  4. **Compiled-in defaults**.
- **R-CFG1.2**: `AHMA_*` environment variables are **not** a configuration source. A set retired variable (R-CFG7) is ignored, with a startup `warn` naming the replacement flag or key.
  - **R-CFG1.2.1**: **Retirement binds every binary and every subcommand.** A variable ignored by `ahma serve` **must** be ignored by `ahma-tui`, by subcommands with their own configuration resolution (`ahma update`, `ahma uninstall`), and by MCP tool handlers alike; otherwise one variable has two meanings inside one product. The warn-and-ignore verdict **must** live in **one** function every surface calls, low enough in the crate graph that every surface can reach it, and that function **must** return only *whether* the variable was set, never its value, so a caller cannot accidentally honor one. This **must** be enforced by a test that reads the documented retired set and fails on any direct read.
  - The one exception is the **bootstrap installers** (`scripts/install.sh`, `install.ps1`, `install-local.sh`), which run *before* any `ahma` binary exists: there is no flag to pass and no settings file to read. Once `ahma` exists, `ahma update --install-dir` is the supported route.
- **R-CFG1.3**: The only environment variables production code may read are: (a) platform/ecosystem standards (`HOME`, `PATH`, `RUST_LOG`, `NO_COLOR`, `TERM`, `XDG_*`, `APPDATA`, `USERPROFILE`, `OTEL_*`, `TRACEPARENT`), and (b) **internal plumbing** variables used for parent→child process communication (e.g. `AHMA_MCP_ARGS` bridge→subprocess). Internal plumbing variables **must** be listed in a single table in `docs/environment-variables.md` marked `INTERNAL`, **must** be set only by ahma itself, and **must never** widen security scope relative to the parent's resolved configuration.
- **R-CFG1.4**: Every boolean setting **must** be expressible as on *and* off at every source level (`--x` / `--no-x` flag pairs; `Option<bool>` settings keys). OR-combining sources (where any source can enable but none can disable) is forbidden — a higher-precedence source **must** be able to turn a lower-precedence setting off.

### R-CFG2: Trust Tiers

- **R-CFG2.1**: Every setting is classified into one of two tiers:
  - **Security tier (S)**: anything that weakens or shapes the security boundary — sandbox disable/defer, sandbox scopes, working dirs, temp access, package-cache write, task vault, auth token and token path, rate limits, TLS directory, session isolation, update signature verification.
  - **Preference tier (P)**: everything else — timeouts, tool bundles, tools dir, logging, token minimization, instance label, transport tuning.
- **R-CFG2.2**: Security-tier settings **must not** be honored from the project settings file. A cloned repository must not be able to weaken the sandbox that is about to contain it (the `.vscode/tasks.json` attack class). Security-tier keys found in `<workspace>/.ahma/settings.toml` **must** be ignored and reported at `warn` with the key names.
- **R-CFG2.3**: The two most dangerous switches — disabling the sandbox entirely and skipping update signature verification — **must** be CLI-flag-only (`--no-sandbox`, `--insecure-skip-verify`). They may not be set from any settings file, so that they are always visible at the invocation site (process listing, `mcp.json` args) and never persist invisibly.
- **R-CFG2.4**: Nested-sandbox auto-detection (R7) remains the only non-CLI path to a disabled internal sandbox, and **must** log why it triggered.

### R-CFG3: Project Settings File

- **R-CFG3.1**: `<workspace>/.ahma/settings.toml` is loaded when the tools directory auto-detection (R1.2.1) or `--tools-dir` identifies a `.ahma` directory. Same schema as user settings; Security-tier keys rejected per R-CFG2.2.
- **R-CFG3.2**: Merge semantics are per-key scalar override (project over user). List-valued keys replace, never concatenate, so the effective value is always attributable to one source.
- **R-CFG3.3**: `--no-settings` disables **both** settings files for the invocation.

### R-CFG4: Resolve Once, Then Immutable

- **R-CFG4.1**: All configuration **must** be resolved exactly once at startup into an immutable resolved-config structure passed down by constructor argument. No production code may read configuration (env, settings files) after startup; runtime re-reads are a tamper channel.
- **R-CFG4.2**: Only the configuration-resolution module may call `std::env::var*` for `AHMA_*` names. This **must** be enforced by a CI check (grep test or clippy `disallowed-methods`) with an explicit allowlist for R-CFG1.3 reads.
- **R-CFG4.3**: The sandbox scope derived from resolved configuration remains subject to R5.1: set once, never mutated.

### R-CFG5: Provenance and Observability

- **R-CFG5.1**: `ahma settings show --origin` **must** print every effective setting with its value, source (`cli` / `project` / `user` / `default`), and for file sources the file path.
- **R-CFG5.2**: At startup the server **must** log one `info` line per setting whose effective value differs from the compiled-in default, including its source. Security-tier deviations, and keys with no known tier, **must** log at `warn`. The report and `ahma settings show --origin` use one provenance resolver (`ahma_common::settings_origin`), so they cannot name different sources for one key.
- **R-CFG5.3**: The documented precedence and the implemented precedence **must** be the same and **must** be covered by a matrix test (every source pair, at least one Preference and one Security key).

### R-CFG6: Strict Parsing (Fail Closed)

- **R-CFG6.1**: A settings file that exists **and can be read** but fails to parse **must** abort startup with a clear error. Silently falling back to defaults is forbidden — a tampered or corrupted file must not silently change behavior. This applies only to a *read-but-unparseable* file: a settings file that cannot be *read* at all — missing, or permission-denied because it lives in the out-of-scope control-plane directory `~/.ahma` (R5.4.8) — is **not** a parse failure and **must** fall back to compiled-in defaults (with a `warn` for the permission-denied case), never abort.
- **R-CFG6.2**: Unknown keys in the `[sandbox]` and `[auth]` tables **must** abort startup (a typo in a security key must not be silently ignored). Unknown keys elsewhere **must** produce a `warn` listing each key (forward compatibility).
- **R-CFG6.3**: On Unix, a settings file that is group- or world-writable **should** produce a startup `warn` naming the file and `chmod go-w`.

### R-CFG7: Retired `AHMA_*` configuration variables

- **R-CFG7.1**: The security-tier `AHMA_*` variables (`AHMA_DISABLE_SANDBOX`, `AHMA_SANDBOX_SCOPE`, `AHMA_SANDBOX_DEFER`, `AHMA_WORKING_DIRS`, `AHMA_TMP_ACCESS`, `AHMA_DISABLE_TEMP`, `AHMA_NO_PACKAGE_CACHE_WRITE`, `AHMA_TASK_VAULT`, `AHMA_REQUIRE_TOKEN`, `AHMA_REQUIRE_TOKEN_PATH`, `AHMA_TLS_DIR`, `AHMA_INSECURE_SKIP_VERIFY`) are ignored with a `warn` (R-CFG1.2).
- **R-CFG7.2**: Every other `AHMA_*` configuration variable is ignored the same way; only the R-CFG1.3 allowlist and the INTERNAL/TEST variables of `docs/environment-variables.md` are read.
- **R-CFG7.3**: Retiring or adding a variable updates `docs/environment-variables.md`, `skills/ahma/SKILL.md` and the README in the same PR (AGENTS.md R-DOC).

### R-CFG8: Required Tests

- **R-CFG8.1**: Red team: with `--no-sandbox` in the environment, the sandbox **must** still be enforced (write outside scope blocked).
- **R-CFG8.2**: Red team: a project `<workspace>/.ahma/settings.toml` containing `sandbox.disable = true`, widened `sandbox.scopes`, or `auth` keys **must not** affect behavior, and the ignored keys **must** appear in startup warnings.
- **R-CFG8.3**: Precedence matrix per R-CFG5.3; parse-failure abort per R-CFG6.1; `--no-x` overriding a settings-file `x = true` per R-CFG1.4.

### R-CFG9: Test-Only Configuration

Production and test code must be separated so that test harness machinery can never alter production security decisions.

- **R-CFG9.1**: Test-only behavior **must** be controlled exclusively through:
  1. `#[cfg(test)]` compile-time gates (preferred — zero runtime cost in production builds).
  2. The `AppConfig.is_server_child` field (set only via the `--server-child` CLI flag or the `AHMA_SERVER_CHILD` internal plumbing variable, which is set by the parent ahma process before spawning a subprocess).
  3. Constructor/function parameters (dependency injection).
- **R-CFG9.2**: Production code **must not** read `NEXTEST`, `CARGO_MANIFEST_DIR`, `CARGO_LLVM_COV`, `CARGO_TARGET_DIR`, or any other cargo-set environment variable. These variables are set by the build/test toolchain and must not influence runtime security decisions. The `--server-child` flag is the exclusive mechanism for subprocess detection in production. **Single carve-out (R-ISO.1):** `NEXTEST` / `NEXTEST_RUN_ID` may be read for exactly one purpose — forcing test isolation of endpoint rendezvous (private sockets instead of the per-user ones), via `ahma_common::test_isolation` only. This influence is fail-closed by construction: the variable can only *restrict* the process to private endpoints; it can never widen filesystem/network access, restart shared services, or weaken a sandbox decision. (Production already reads `AHMA_TEST_ISOLATION` to the same effect, so this adds no new attacker capability.)
- **R-CFG9.3**: Test helper code inside `#[cfg(test)]` blocks or `test_utils` modules **may** read `AHMA_TEST_BINARY`, `CARGO_TARGET_DIR`, `NEXTEST`, and `CARGO_LLVM_COV` to locate test fixtures and adjust timeouts. These reads are acceptable because they are gated behind compile-time test flags and do not run in production binaries.
- **R-CFG9.4**: The `AHMA_HUB_SOCK` variable is a test-isolation helper set by `init_test_hub_isolation()`. It **must** only be read inside `#[cfg(test)]`-gated code paths or in functions that are explicitly documented as test-only. It is INTERNAL plumbing (not user-facing) and **must** be listed in `docs/environment-variables.md` as `INTERNAL/TEST`.
- **R-CFG9.5**: **Environment variable minimization.** Configuration is declared on the command line or in explicit structures (`AppConfig`) and passed down by constructor argument, never queried from the environment at the point of use (R-CFG4.1).

---


## 4. Security - Kernel-Enforced Sandboxing

The sandbox scope defines the root directory boundary. AI has **full read/write access** within it. What holds *outside* it differs by platform and **must not** be stated as one guarantee:

| Platform | Writes outside scope | Reads outside scope | Mechanism | Gap |
|---|---|---|---|---|
| Linux | denied | denied (R6.1.6) | Landlock, applied per spawn (R6.1.4) | trust-handoff deny tier is application-layer only; command writes to it are detected and reported (R6.1.7, R-HANDOFF.4) |
| macOS | denied | **allowed**, except a credential denylist (R6.2.3) | Seatbelt (R6.2) | reads not kernel-scoped (R6.2.2) |
| Windows | **allowed** | **allowed** | Job Object: process lifetime only (R6.3.2) | no OS filesystem boundary (R6.3.3, R6.3.9; open: §11) |

Every R5.4 scope surface **must** state its platform's row (R-PERM.5.1). Beyond the scope, reads are limited to what each backend can express: platform-invariant system paths, the toolchain directories shipped profiles grant (R-PERM.5), and explicitly granted feature scopes (`--livelog`).

Confining writes is necessary but not sufficient: a legitimate write *inside* the scope can later be executed by a trusted component that was never sandboxed — see **R-HANDOFF**.

### R5: Sandbox Scope

**Design principles (govern all of R5):** scope is never inferred from spoofable signals; the complete scope is always visible with its provenance; the user is prompted *only* on a genuine security downgrade (never on routine establishment or narrowing); and when the user cannot be asked, ahma fails to a clear, shown default rather than silently widening or running unsandboxed. "No surprises" is the controlling invariant.

#### Scope ownership and lifetime

- **R5.1**: **Per-enforcing-process ownership, lock-once**: A sandbox scope is owned by the **server process that enforces it**, committed once for that process's life and never mutated afterwards (the lock-once invariant): no path through the client, a tool argument or a model can re-lock, replace or widen the committed *workspace* scope. The one addition allowed is a **human-approved** grant (R5.4.5) — a named external directory beside the workspace scope, applied live and, at a persistent tier, also written to settings (R-PERM.2, R-PERM.4.1) — and every such addition is audited and announced (R5.4, R5.4.6). The enforcing process depends on the transport:
  - **Direct stdio** (`ahma serve stdio --server-child`, test harnesses, CLI mode): the server process; its one scope gates every request it serves.
  - **HTTP/Unix bridge**: a **dedicated subprocess** per session, spawned by the per-user hub (R-HUB.4), which owns and commits its own scope. Sessions never share a mutable scope object, and two clients in different workspaces get two independently locked sandboxes (R10). An operator's explicit `--sandbox-scope` is passed to and locked by every session subprocess (R5.2.2). The **hub** never carries a scope (R-HUB.9).
- **R5.1.1**: **Single commit point**: Every scope commit — from `roots/list`, an explicit flag, or the container root — **must** go through one atomic compare-and-swap on the instance scope state machine; there is no second path that can set or widen scope after lock. On **both** transports: the HTTP bridge swallows a post-lock `roots/list_changed` (R10.5), and the direct-stdio path (`configure_sandbox_from_roots`) latches the commit once and treats any later `roots/list` / `roots/list_changed` as a tolerated no-op — it does **not** re-request `roots/list` or re-derive scope.
- **R5.1.2**: **Sandbox configuration never blocks the session's message loop.** Configuring the scope needs a server→client `roots/list` round-trip, and MCP notification handlers run on the loop that dispatches requests, so awaiting it inside `on_initialized` / `on_roots_list_changed` stalls every request pipelined behind it — a slow `roots/list` answer would leave `tools/list` undispatched past the bridge's request timeout. It **must** therefore run off the loop, one at a time (a burst of `roots/list_changed` **must not** start concurrent queries).
- **R5.1.2.1**: Running configuration off the loop **must not** weaken the scope invariant. Every `tools/call` **must** wait (bounded) for an in-flight configuration to settle before executing; read-only protocol traffic (`tools/list`) **must not** wait. The provisional pre-`roots/list` scope is a **subset** of the committed one, so a call that ran early would be denied work about to become legal. On expiry the call proceeds against the scope committed so far — narrower, never wider.
- **R5.1.2.2**: The R5.1.2.1 gate **must** be applied once, at `tools/call` dispatch, and cover **every** tool that resolves a workspace path — including the built-in file tools (`write_file`, `replace_in_file`, `read_file`, `list_dir`, `file_search`, `grep_search`) — never per handler. Exempt are the session's own control surface (`status`, `await`, `cancel`, `restart`, `todo_write`) and `sandbox_grant`, which is itself how a scope gets widened.

#### Scope source (no spoofable inference)

- **R5.2**: **Scope source precedence**: The locked scope **must** be derived from exactly one of the following, in order; the chosen source **must** be recorded for display (R5.4):
  1. **Explicit** `--sandbox-scope` / `--working-directories` (CLI, user settings file, or task vault) — locked immediately; `roots/list` is **not** requested (R5.2.2).
  2. **Client `roots/list`** — the workspace roots reported by the MCP client. An **empty** roots array is **not** a usable answer (R5.2.7); it falls through to the next source.
  3. **Container root, auto-narrowed** — the user-configured container directory, with the writable set narrowed to the one project subtree actually in use (R5.2.3, R5.2.6).

  There is no fourth source; a human widens a committed scope only through a grant (R5.4.5). When none yields a scope the server **must** fail loudly with remediation (R5.2.3) — it **must not** invent one.
- **R5.2.1**: **No marker-based inference**: The server **must not** infer or accept a sandbox scope from project-marker files (`.git`, `Cargo.toml`, `package.json`, etc.) or any other spoofable, ambient signal in the current working directory. The launch CWD is **not** trusted as a scope on its own; it becomes the scope only by being reported through `roots/list` (R5.2 step 2) or named explicitly (step 1).
  - **R5.2.1.1**: **The TUI's launch directory is an explicit human choice, not an inference.** `ahma tui [PATH]` passes `PATH` — defaulting to the directory the human launched it from — as `--sandbox-scope`, an R5.2 step-1 explicit scope: a human typing `ahma tui` in a shell has *chosen* that directory, unlike the MCP server's CWD, which whoever wrote the client config sets. The TUI **must** canonicalize the candidate and pre-flight it through the server's hard rejections (exists — never created by a launcher — and not `$HOME`/an ancestor/a filesystem root, R5.2.4), so a bad launch directory fails at launch with the reason.
- **R5.2.2**: **Explicit scope is locked and never widened**: When the scope is explicit (R5.2 step 1), the server **must not** request or apply `roots/list` and **must not** widen the scope by any means, so a compromised or buggy client cannot widen an operator-chosen scope and roots-less clients get a stable one.
- **R5.2.3**: **Container root — user-owned, never invented**: When the client supplies no usable roots and no explicit scope is configured, the server **must** derive its scope from the **container root**: one directory, configured as `[sandbox] container_root` in `~/.ahma/settings.toml`, that contains the user's projects (e.g. `~/github`). It is surfaced with `source: container` (R5.4) and always auto-narrowed (R5.2.6).
  - The container root is **user-owned configuration only** and **must not** be settable from a client-owned MCP config file (`mcp_config.json`/`mcp.json`, written by `ahma setup` and editable by whoever configures the client). A CLI `--sandbox-scope` remains an *explicit* scope (R5.2 step 1), not a container, and is not auto-narrowed.
  - With no container root configured, the server **must** fail loudly and actionably: no scope is locked, and `tools/call` is refused with the paths and the exact remediation (configure `[sandbox] container_root`, pass `--sandbox-scope`, or answer the elicitation prompt), per R5.4 and R5.4.7. **There is no implicit fallback directory**: a scratch directory the user never chose is a silent redirect, not a boundary.
  - Under the per-user hub (and `serve http|unix`) only the worker knows the container root; the bridge carries no scope (R-HUB.9). An empty `roots/list` answer therefore leaves the session waiting for the worker's own `sandbox/configured` (or `failed`) when a container root is configured, and fails it at once only when there is none.
  - The server **must not** lock to the launch CWD, the system temp directory, the home directory, an ancestor of the home directory, or a filesystem root (R5.2.4).
- **R5.2.4**: **Hard rejections**: The system temp directory, the home directory, **any strict ancestor of the home directory** (`/Users`, `/home`, `/Volumes`, `C:\Users`), and any filesystem root (`/`, `C:\`, UNC root) **must never** be a locked scope, even after symlink resolution (R5.7), for **every** scope source including an explicit `--sandbox-scope`. Any escape hatch **must** live in user-owned `~/.ahma/settings.toml` as an explicit list of paths — never a boolean, CLI flag or environment variable, which a client config can carry.
  - Terminal hooks are a scope source too: a hooked command's working directory becomes its writable scope only when it is none of these. A harness started in the home directory or at a root gets the temp directory as its only writable scope, and its scope disclosure (R5.4.10) says so and to open a project folder.
- **R5.2.5**: **Temp dir is opt-in and auxiliary only**: The system temp directory **must not** be in scope unless enabled via `--tmp` or `[sandbox] tmp_access = true`, and then only as an auxiliary scope after the primary, never the sole or primary scope. Enabling `--tmp` is a downgrade (R5.3). A server treats it as a request: once the scope is committed and announced, it asks once per session, through the permission ladder (R-PERM.3, `GrantReason::StartupFlag`), for a grant of the canonical temp directory. Only deny, once and session are offered, and an `always` or lease answer from any surface is held to the session (`GrantDecision::within_offer`), because the temp directory is shared by every process on the machine. With nobody to ask, or when the hard denylist refuses it for this workspace, the directory stays out. A one-shot `ahma tool run` includes it at startup, because the human typed the flag at the invocation.
- **R5.2.6**: **Auto-narrowing within the container — narrowing only, never widening**: A container root (R5.2.3) **must not** be the writable scope in its entirety. The writable set **must** narrow to the single immediate child of the container that the session's first path-resolving tool call touches; the rest of the container is committed **read-only**. A container like `~/github` spans every repository the user owns, so without narrowing an injected prompt could plant a `.git/hooks/post-checkout` in an unrelated project.
  - The narrowing signal comes from tool-call input and is **attacker-influenceable**, and is safe only *because it can only narrow* within a container the user already authorized; R5.1.1 still rejects any widening. This is the only sanctioned use of a derived path in scope selection and does **not** relax R5.2.1.
  - The container is committed at the handshake as normal; narrowing is a later, one-shot tightening of that committed scope, not a deferred commit (R5.1.2's gate refuses `tools/call` until a scope is committed). Before narrowing the writable scope is exactly what the user authorized; after it, strictly less.
  - Narrowing **must** be one-shot under concurrency: two tool calls arriving together **must not** both narrow, or the second would re-point the writable scope from tool input. A session that needs a second project **must** obtain it through R5.4.5 (a human-confirmed grant), never by re-narrowing.
  - **Narrowing bounds the container, not the machine.** It does nothing for paths shared across every project by construction — above all the toolchain caches shipped profiles grant `rw` (R-HANDOFF.8).
- **R5.2.7**: **Empty roots is not usable roots**: A client that advertises `roots` and answers `roots/list` with an **empty** array has reported no workspace. The server **must** treat this as "no usable roots" and fall through to the next source (R5.2 step 3, the container root) — never treat it as a scope, never stall waiting for a better one. Disclosure (R5.4) **must** distinguish it from a client that does not support `roots` at all, because the remediation differs.
- **R5.2.8**: **A substituted working directory is a scope decision**: When a tool call omits its working directory and the server supplies one from the locked scope, the tool result **must** disclose the substitution, so the model can tell that ahma, not the caller, chose the directory — otherwise errors like `fatal: not a git repository` read as the user's own.
  - When the scope's source is one the **user did not choose** — a container root (R5.2.3) before narrowing, or any future non-explicit default — the call **must** fail with an actionable error naming the missing parameter, the scope, its provenance and the remediation. Under R5.2.6 the working directory is also the signal that selects the subtree to narrow to.
  - This binds **every** surface that resolves a working directory — the `run_terminal_command` handler and the MTDF-configured tool path alike.

#### Downgrade prompts (ask only when it matters)

Status: of the downgrades below, `--tmp` is asked (R5.2.5), widening beyond the established scope happens only through human-approved grants (R5.4.5), and a hook about to run unsandboxed is asked (R5.5.3). Client roots broader than an established scope have no live instance, because explicit scopes never request roots (R5.2.2) and a post-commit `roots/list_changed` is a no-op (R5.1.1). Every question is raised through the permission ladder (`PermissionBroker` and `GrantCoordinator`, R-PERM.3), never a second coordinator; scopes are per session (R5.1), so no decision is shared between sessions or held pending for a later one.

- **R5.3**: **Prompt only on a genuine downgrade**: The server **must** prompt **only** when an action would reduce the security posture: widening the writable set beyond the established scope, accepting client roots broader than an established scope, adding the system temp directory (`--tmp`, R5.2.5), or a terminal hook about to run unsandboxed (R5.5.3). Disabling kernel enforcement is never prompted: it is CLI-flag-only (`--no-sandbox`, R-CFG2.3), so it is visible at the invocation site. First-time scope **establishment** and any **narrowing** are applied and shown (R5.4), never prompted. Routine operation **must not** generate confirmation prompts ("no security theater").
- **R5.3.1**: **Elicitation channel**: Downgrade prompts are delivered via MCP `elicitation/create` to every attached session whose client advertised `elicitation` at `initialize`. The prompt **must** show the literal paths affected (never a vague "Allow workspace?"). The default-focused choice **must** be the narrowest/safest option; a *widening* choice **must** require an explicit, non-default selection (Enter alone **must not** widen).
  - **Elicitation is an optional upgrade, never a dependency.** Every flow that uses it **must** work without it; no scope decision may be reachable *only* through elicitation.
  - **The server's wait stays under the client's patience.** Clients cancel a server-initiated request at an undisclosed deadline (≈60 s measured), so the elicitation wait **must** be bounded by the per-client elicitation budget (`McpClientType::elicitation_budget`), strictly below that client's measured cancel deadline. It is a separate value from the uniform request budget (R2.6.5): a human reading a path and choosing needs far longer than a `tools/call` is allowed. The bound binds **every** elicitation the server raises, including R-PERM.3 grant prompts.
  - **A client-side `cancel` is not a user's answer.** MCP `cancel` means "dismissed without an explicit choice", and a client timeout produces it with no human involved. The server **must not** record `cancel` as a denial, **must not** suppress a later prompt for the same path on its strength (the ask-once rule of R5.4.7 binds *decided* outcomes), and **must** fall through to the out-of-band path (R5.3.2) with remediation the agent can relay.
- **R5.3.2**: **Cannot-ask fallback**: When no attached client can be asked (none advertises `elicitation`, or the prompt was cancelled undecided) and no explicit scope is configured, the server **must not** silently widen, invent a scope, or run unsandboxed. It falls through to the container root (R5.2.3) if configured, and otherwise refuses tool calls with remediation naming the out-of-band paths — the TUI grant modal, or `ahma sandbox grant <PATH>` — the only ways to establish a broader scope.
- **R5.3.3**: **One question, many surfaces**: A question shown on more than one live surface (the client's prompt and the TUI) is one decision under one `decision_id`, owned by the server, not any client. The first answer wins; the server **must** dismiss the question on the other surfaces (`notifications/cancelled` for that `decision_id`). When the session that raised an open question terminates, the server **must** resolve it as cancelled-not-decided and dismiss it everywhere. Surfaces are asked one at a time (R-PERM.3), so two answers never race.
- **R5.3.4**: Retired: answers never race, because surfaces are asked one at a time (R5.3.3).
- **R5.3.5**: **Decision freshness**: A `decision_id` **must** bind to the session generation that created it. An answer arriving after the handshake deadline (R10) or after the session was recycled **must** be rejected, never applied to a new session.
- **R5.3.6**: Retired: a scope belongs to one session (R5.1), so a TUI answer with no session to apply it to is not held for a later one; a human widens the next session's scope with a grant (R5.4.5).

#### Visibility (nothing silent)

- **R5.4**: **Scope is always visible with provenance**: The complete locked scope — every writable root, every read-only root, `--tmp` status, and whether kernel enforcement is on — with its **`source:`** (`explicit` | `roots/list` | `container`) **must** be rendered through one canonical representation and surfaced at:
  - (a) the startup banner and the `status` MCP tool, whose `SANDBOX` section also names the enforcing sandbox (R7) and every persistent grant in force with its provenance and workspace — the one surface the agent, and a human reading its transcript, reach without a TUI;
  - (b) the persistent TUI scope panel, which **must** also list every session attached to the hub with the sandbox it reported. An instance **must** report its writable roots, read-only roots, grants in force and enforcement token to the hub at registration and again at every commit, narrowing and applied grant;
  - (c) the `notifications/sandbox/configured` payload (R5.6); and (d) the body of every scope-related error (e.g. the 409 returned before lock).

  The execution audit log (R-HANDOFF.10) **must** carry the session id, client and scopes on every `tool_call`, and a terminal hook **must** record every decision it makes (`hook_decision`). No scope decision may be communicated only via an internal log line.

#### Subprocess propagation and defaults

- **R5.4.1**: **Scope propagation to subprocesses**: When the stdio MCP server spawns a background bridge or per-session subprocesses, it **must** forward only genuinely explicit `--sandbox-scope` values (never the provisional CWD or temp). The scratch flag (`--scratch`, forwarded under its deprecated alias `--sandbox`) and `--tmp` are forwarded as booleans so each subprocess derives the scratch and temp auxiliary scopes itself (`build_stdio_server_args`).
- **R5.4.2**: **Default install carries no scope and no downgrade**: The MCP server configuration `ahma setup` installs for Cursor, VSCode, Claude, Antigravity, Codex and LM Studio **must not** include `--tmp` (a downgrade, R5.2.5) and **must not** inject a scope the user did not choose — no `--sandbox-scope`, and no directory pre-created by setup. These files are **client-owned** (R5.2.3). A client that reports no usable roots reaches its scope through elicitation (R5.3.1) or the container root (R5.2.3).
  - Generated configs **must not** carry `--sandbox`: it is a deprecated alias for `--scratch` (an auxiliary scratch directory) and never toggled the kernel sandbox, so it advertises a protection it does not provide.
  - **Antigravity supports `roots/list`**, verified on the wire: it declares `roots: {listChanged: true}` and `elicitation: {form: {}, url: {}}` at `initialize` (protocol `2025-11-25`, `clientInfo.name = "antigravity-client"`) and answers `roots/list` with `{"roots": []}` — roots-**empty** (R5.2.7), not roots-less. Client capability claims in this document **must** cite wire evidence.
  - **Antigravity's grant matching is token-based word-prefix matching, so a rewriting hook must self-register clean token-prefix patterns.** The shipped `agy` binary (`cortex/utils/commandutils.MatchesConfig`, `cortex/shared.coversCommand`) splits a command into words and matches tokens in sequence: plain tokens byte-for-byte, `regex:`-prefixed tokens via Go `regexp`. A grant like `command(/path/to/ahma hooks run-shell)` therefore covers every command beginning with those words, including all trailing flags of the wrapped form (`--wrapped-by ahma-hooks-wrapper-v1 --cwd <cwd> [--session-id <id>] --command <cmd>`). Bare regex syntax without `regex:` (trailing `.*`, character classes, `\.`) and single quotes inside `command(...)` match literally and **must not** be used. The hook registers these patterns in `permissionOverrides` and as user grants in `~/.gemini/antigravity-cli/settings.json` (`permissions.allow`) and `~/.gemini/config/config.json`.
- **R5.4.3**: **Write Protection**: The system **must** block any attempt to write outside the locked scope, including via command arguments (e.g. `touch /outside/file`).

#### Persistent scope grants (external tool directories)

- **R5.4.4**: **User-granted persistent scopes survive `roots/list`**: Directories in `[sandbox] persistent_scopes` (`~/.ahma/settings.toml`) are external locations a trusted tool needs outside the workspace (a build cache, a shared toolchain). Each entry carries an `access` (`rw` default, or `ro`), the `workspace` it was granted for (R5.4.11), and optional `granted_by` / `granted_at` / `note` provenance. Every persistent scope that applies to the session **must** be folded into the initial scope set before first enforcement and re-appended on each `roots/list` update — `rw` into the writable set, `ro` into the read-only set — and the set of applicable grants is re-decided at every commit against the scopes being committed.
- **R5.4.5**: **Grants are authored only by the unsandboxed control plane, never by a sandboxed command — and never on the model's word**:
  - **One write path.** `persistent_scopes` lives in `~/.ahma` (R5.4.8), so no sandboxed command — a `run_terminal_command` child, a build script — can author a grant. Grants are written only through `ahma_common::scope_grant::persist_grant`, shared by the CLI (`ahma sandbox grant`), an approved elicitation answer relayed by the permission broker, and the TUI modal; it applies the hard denylist and appends the audit record itself, so no surface can skip either.
  - **`sandbox_grant` requests; it never writes.** Without `confirm: true` it only previews (the absolute settings-file path and the exact line). With `confirm: true` it raises the question through the R-PERM.3 ladder and returns the answer the broker recorded — approved and for how long (`once`, `session`, `always`), declined, still waiting at the TUI, or nobody could be asked — never a guess read back from the settings file, which cannot see `once`/`session` answers or which workspace a record belongs to. It **must never** persist on `confirm: true` alone, for **any** client type: a client that cannot show a prompt cannot approve, and is not assumed to have asked a human first.
  - **Hard denylist.** Refused outright, even when a human approves: the filesystem root; the exact `$HOME`; any parent of a live workspace scope (except the enclosing git repository root or main-worktree repository of an active scope, which is the project itself); credential directories and everything inside them (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.docker`, `~/.config/gh|gcloud`; matched by path prefix after resolving the deepest existing ancestor, so a key file is refused like its directory and a not-yet-existing path cannot slip past through a symlinked parent); `~/.ahma` and everything in it; and OS system directories. It binds records already in the file too: a denylisted record is skipped when the sandbox loads its grants, with a warning naming the revoke command. No model-supplied input can override it.
  - The AI may *propose* a scope and form the exact line; only the human *approves* it.
- **R5.4.6**: **Grants are inspectable and reversible by name**: `ahma sandbox list` **must** show every persistent scope with its access level and provenance and name the absolute settings file that holds them — the file the user reviews and edits by hand or through `ahma sandbox revoke`. `ahma sandbox grant`/`revoke` and the `sandbox_grant` tool **must** confirm the change and name that same file. When each kind of grant takes effect is R-PERM.4.1.
- **R5.4.7**: **Auto-detection raises the grant question, never the grant**: when a sandboxed command is blocked by an out-of-scope path — rejected up front by path validation (path known exactly) or surfaced by a stderr denial signature (a heuristic *candidate* path) — the server **must** surface the denial as a structured `sandbox_denial` payload on the tool error (path, access, current scopes, remediation) and/or a "grant access to X?" prompt. Remediation flows only through R5.4.5, so detection itself **must not** widen the live session. The same `(path, access)` **must** be asked at most once per session (a denied or already-granted path **must not** re-prompt), and a stderr-extracted path **must** be canonicalized and shown literally, so a forged denial line can at worst raise a human-gated prompt.
- **R5.4.8**: **`~/.ahma` is out of scope for read *and* write** (canonical statement; other requirements point here): `~/.ahma` holds ahma's control-plane state — settings, persistent grants (R5.4.4), task vault, credentials — and is **never** part of any workspace scope: kernel-unwritable and kernel-unreadable from inside the sandbox, and on the hard denylist (R5.4.5), so a sandboxed tool can neither author grants nor read ahma's secrets.
  - Consequence: whenever ahma itself runs inside its own sandbox (its test suite, a nested or re-entrant ahma, an ahma spawned by another ahma), reading `~/.ahma/settings.toml` fails with `PermissionDenied`. The loader **must** treat that, like `NotFound`, as "no user settings" — falling back to the compiled-in defaults (sandbox on) with a `warn` — and **must not** abort. R-CFG6.1 owns the parsing rules: a readable file that fails to parse still aborts.
- **R5.4.11**: **A grant is bound to the workspace it was made for**: a persistent scope **must** record the `workspace` (project root) of the session whose human approved it, and the sandbox **must** apply it only to a session whose committed scope lies inside that workspace, contains it, or is enclosed by it as a git worktree (`scope_grant::grant_applies`), so a broad grant approved for one project cannot widen another project's agent.
  - The broker and the TUI modal stamp the session's committed primary scope; `ahma sandbox grant` defaults to the git repository enclosing the current directory (`--workspace <DIR>` names another; `--global` is the explicit opt-out).
  - A record with no `workspace` is a legacy global grant: it still applies everywhere, and `ahma sandbox list`, `ahma permissions list` and `ahma doctor` **must** say so and how to narrow it.

#### Terminal hooks (one-time consent, never silent)

- **R5.4.10**: **A hooked session is told its scope once, where it can read it**: the first rewritten command of a hook session (keyed by the harness's `session_id`) **must** carry a scope disclosure — ahma's kernel sandbox is enforcing, the writable directories, the platform's read limit (R6.2.2 on macOS), and that only a human can widen it — and again whenever the scope set changes; every other command stays silent. The disclosure **must** use the fields the harness renders (Claude Code: `hookSpecificOutput.additionalContext` for the model and top-level `systemMessage` for the user; Cursor: `agent_message`/`user_message`); a field the harness ignores (`hookSpecificOutput.agentMessage`) is silence.
- **R5.5.3**: **Hook fall-open requires one-time, session-scoped consent**: A terminal hook that can sandbox the command runs normally. When the hook binary **executes** but cannot sandbox (stale/incapable, or the kernel sandbox is unavailable), it **must not** silently run the command unsandboxed:
  - The **first** such invocation in a session **fails closed**: it returns `deny` to the IDE with the reason, `ahma hooks doctor` / repair guidance, and how to approve unsandboxed mode. `ahma hooks doctor`, `ahma hooks approve-unsandboxed` and `ahma hooks revoke` manage this state.
  - **Residual (binary cannot execute at all)**: if the hook binary is missing or crashes/times out before deciding, the IDE-level `failClosed` setting governs. The default install keeps `failClosed: false` as a deliberate anti-wedge valve, because consent itself needs the binary to run; this failure is loud (the IDE surfaces it) and repaired with `ahma hooks doctor` / reinstall.
  - Consent is collected **out-of-band** (an `elicitation/create` to an attached client/TUI, or `ahma hooks approve-unsandboxed`) — never mid-command — and, being maximal widening, **must** be an explicit, deliberate action, never an Enter-default.
  - Consent is scoped to **workspace + session generation**, **must not** persist across restarts, and lapses after eight hours. Its marker lives in ahma's owner-only runtime directory — never the shared temp directory, where another user could pre-create it or a sandboxed command write it; with no runtime directory, consent cannot be recorded and fall-open stays closed.
  - While consent is active, every surface (R5.4) **must** show a prominent banner that hooks are running unsandboxed and how many commands have done so.
- **R5.5.4**: **Hook default-enablement is gated on the permission ladder, per client**: `ahma setup` **must not** install hooks for a client until it passes the gate of **R-PERM.6**, and **must** state the reason rather than omit them unexplained.
- **R5.5.5**: **An explicit hook allow is a scoped grant, not a default response**: where a `PreToolUse` contract distinguishes an explicit allow (bypasses the client's own permission system for that call) from an undecided response (the client's normal permission flow still runs), a hook **must** emit the explicit allow **only** when ahma substitutes its own sandbox for the call (a rewrite into `ahma hooks run-shell`).
  - Every other outcome — hooks inactive, the command already wrapped, or the R5.5.3 fail-open with no sandbox — **must** leave the decision unset: ahma is not the control for that call, and force-approving it would disable the client's permission system on ahma's behalf.
  - Claude Code's `hookSpecificOutput.permissionDecision` has an undecided outcome (`ahma_mcp/src/hooks/mod.rs::build_structured_hook_output`). Whether Cursor's `permission` and Antigravity's `decision` fields have one is unverified, so their builders are not yet held to this rule (open: §11).
- **R5.5.6**: **Native file edits are confined to the same scope as hooked shell commands**: a harness's own file-edit tools (Claude Code `Edit`/`Write`/`MultiEdit`/`NotebookEdit`, Codex `apply_patch`, Copilot `edit`/`create`, Cursor `Write`, Antigravity `write_to_file`) never pass through the shell sandbox, so `ahma hooks install` **must** install the pre-edit guard (`ahma hooks edit-guard`) by default with every shell hook (`--no-edit-guard` declines it; `ahma hooks status` shows `installed+guard` vs `installed`).
  - The guard **must** refuse an edit whose target, canonicalised as far as it exists, lies outside the hook's scope for the session's directory (the enclosing repository per R5.2.1, plus persistent `rw` grants and the temp directory — exactly what a hooked shell command may write), with a reason naming the path, the scope and the human-only remediation (`ahma sandbox grant <dir>`). The R2.7.8 write-queue refusal is checked after the scope.
  - The guard is active under the same switch as the shell hook (`AHMA_HOOKS`, auto-detection).
- **R5.5.7**: **A harness's own working set needs no grant**: directories a harness keeps for its own bookkeeping that hold no project data — Claude Code's plan-mode documents (`~/.claude/plans`) and per-session scratchpad (`/tmp/claude-<uid>/`) — **must**, when they exist, be writable by that harness's native edit tools and hooked shell commands without a grant, and **must** appear in the hook scope disclosure (R5.4.10).

#### Lifecycle notifications and path canonicalization

- **R5.6**: **Lifecycle Notifications**: The system **must** emit JSON-RPC notifications for sandbox lifecycle events:
  - `notifications/sandbox/configured`: sandbox initialized from roots (payload `{"scope": {...}}`, the canonical R5.4 summary: `enforced`, `write`, `read`, `tmp`, `source`, `active`, `active_disclosure`, plus `host` when a host sandbox is involved and `reads_unrestricted`/`platform_note` where reads are not kernel-scoped).
  - `notifications/sandbox/failed`: initialization failed (payload `{"error": "message"}`).
  - `notifications/sandbox/terminated`: the session ended (payload `{"reason": "reason"}`).
  - The payload shapes are defined once, as typed structs in `ahma_common::mcp_methods` (`SandboxLifecycleParams`, `SandboxScopeSummary`, `SandboxTerminatedParams`); emitters and parsers **must** use them, and parsers **must** be lenient — a missing or malformed field takes its default — because the transition announced has already happened.
- **R5.6.1**: **Best-Effort Delivery over Pipes**: In HTTP bridge mode, lifecycle notifications are written as raw JSON-RPC to the subprocess's stdout for the bridge to intercept. Delivery is best-effort: a broken pipe (Unix `EPIPE`, Windows OS error 232) during teardown **must not** panic the process. All stdout notification writes **must** use `utils::stdio::emit_stdout_notification`, which logs a broken pipe at `debug` as non-fatal and logs other I/O errors at `warn` and returns them to the caller. Code **must not** use `println!` or `print!` for protocol data on stdout; they panic on write errors.
- **R5.6.2**: **Every request is answered**: a stdio request that parses as JSON but not as an MCP message (wrong `params` shape, for example) **must** be answered with a JSON-RPC error carrying its `id` (code `-32600`), and the transport **must** go on reading, so the client is never left waiting on that `id`. Notifications and responses are owed nothing and are skipped.
- **R5.7**: **Path Canonicalization**: All paths **must** be canonicalized with `dunce::canonicalize` before validation, so a symlink pointing outside the sandbox cannot bypass it. `dunce` is used instead of `std::fs::canonicalize` to avoid the Windows `\\?\` extended-length prefix.

### R6: Platform-Specific Enforcement

#### R6.1: Linux (Landlock)

- **R6.1.1**: Uses Landlock (kernel 5.13+) for kernel-level FS sandboxing.
- **R6.1.2**: If Landlock is unavailable and sandbox is not explicitly disabled, server **must** refuse to start with upgrade instructions.
- **R6.1.3**: If the user explicitly opts into compatibility mode with `--no-sandbox`, the server **must** start unsandboxed and warn that Ahma sandboxing is disabled until the kernel is upgraded. (`AHMA_DISABLE_SANDBOX` is retired and ignored, R-CFG7.1.)
- **R6.1.4**: **Spawn-time enforcement (per command)**: `landlock_restrict_self(2)` restricts only the calling thread and threads/processes created after it, so enforcement inside a running async runtime does **not** cover commands spawned from pre-existing worker threads. Every child created through `Sandbox::create_command` (including PTY execution) **must** have the Landlock ruleset — built from the sandbox's current scopes — applied in `pre_exec`, between `fork` and `exec`, where the child is single-threaded. Process-level enforcement at startup remains as defense-in-depth for the server itself.
- **R6.1.5**: **Availability probing**: Landlock availability **must** be determined by calling `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` — never by kernel version or `/sys/kernel/security/lsm`, both of which report false positives in containers (seccomp-blocked syscall, unmounted securityfs, LSM compiled out).
- **R6.1.6**: **Reads are confined on Linux**: Landlock rules are file-descriptor-based allow-lists, so the read-only set is as explicit as the writable one: platform-invariant system directories are added read+execute by the backend (`sandbox/landlock.rs`, `add_landlock_system_rules`), toolchain directories arrive through shipped profiles (R-PERM.5), and anything not named is unreadable. This **must not** be generalized to the other backends (§4 table).
- **R6.1.7**: **Landlock cannot carve a deny hole inside an allowed subtree**: the ABI ahma targets grants access through `PathBeneath` rules, with no deny rule and no ordering, so a path *beneath* an allowed directory cannot be subtracted. Any rule of the form "the workspace is writable **except** these paths inside it" is unenforceable by the Linux kernel and **must** be implemented as an application-layer check and disclosed as such (R-HANDOFF.4). macOS SBPL is last-match-wins, so a later `deny` genuinely subtracts.
  - **Detection where the kernel cannot prevent.** Where the deny tier is not kernel-enforced (Linux; Windows, R6.3.9; any sandbox that is not enforcing), every command that may write **must** be bracketed by a bounded inventory of the deny-tier set (resolved as in R-HANDOFF.2: every `<git_dir>/hooks` and `<root>/.ahma`, excluding ahma's own log directory). Any created, modified, removed or newly executable entry **must** be reported as a `TRUST-HANDOFF WRITE` line leading the result after the identity line (R2.6.2), at `warn`, as an operation alert, and as a `handoff_write` audit event (R-HANDOFF.10). It is detection only: nothing is reverted, and a target past the bound is disclosed as incomplete on every command. Owned by `sandbox::handoff_watch`. Kernel prevention stays open (§11).
- **R6.1.8**: **Moves within writable areas work**: a rename or hard link that changes a file's directory **must** succeed when both directories are writable. Landlock ABI 1 refuses every such operation with `EXDEV` ("Invalid cross-device link"); `mv` hides it with a copy fallback, but any program calling `rename(2)` across directories failed. The ruleset therefore targets ABI 2 and grants its `Refer` right wherever it grants writes, and nowhere else. On kernels before 5.19 the right is dropped (best effort) and the old behaviour remains.

#### R6.2: macOS (Seatbelt)

- **R6.2.1**: Uses `sandbox-exec` with Seatbelt profiles (SBPL).
- **R6.2.2**: **On macOS the sandbox is a write boundary, not a read boundary**: the profile opens with `(deny default)` and confines **writes** to the locked scope, the necessary temp paths and the paths shipped profiles grant (R-PERM.5). **Reads are not confined**: the backend emits a bare `(allow file-read*)` (`sandbox/seatbelt.rs`, `get_macos_system_rules`), because on Apple Silicon and macOS 26+ the APFS firmlink / cryptex layout makes `bash` and `dyld` resolve paths to vnodes that match no `/usr`, `/System`, … subpath, so subpath read rules would deny the commands rather than confine them. This limitation **must** be disclosed per R-PERM.5.1 (`sandbox/profiles.rs::macos_read_disclosure`).
- **R6.2.3**: **On macOS a denylist, not the scope, is what keeps secrets unreadable**: each entry is emitted as `(deny file-read* (subpath …))` **after** the global allow and **before** the workspace-scope allows, so under last-match-wins an explicit scope grant still wins while the denied paths stay denied by default. The set is owned by `sandbox/credential_reads.rs` and **must not** be enumerated here — it is tuned so no common build / test / VCS tool breaks, is extensible via `[sandbox] deny_credential_reads`, and covers ahma's own control plane (R5.4.8), plaintext cloud and VCS credential stores, private key material, and container daemon sockets (R-HANDOFF.6). The login keychain has its own `[sandbox] allow_keychain` toggle (default on): it is encrypted at rest and `securityd` gates secret extraction regardless of file access, so denying it mostly breaks `gh` and `git-credential-osxkeychain`.
  - A denylist denies only what is named, so it is a **weaker** guarantee than a scope and **must** be described as one wherever it is surfaced.
  - Denying key *files* while keeping `SSH_AUTH_SOCK` in the child environment is deliberate (R-HANDOFF.5).
- **R6.2.4**: **CRITICAL**: `/var` is symlink to `/private/var` on macOS; profiles **must** use real paths.
- **R6.2.5**: **Package Cache Write** (default on): `~/.cargo/registry/` and `~/.cargo/git/` (and cargo's root lock files) are writable so `cargo add` / `cargo update` work inside the sandbox without a `--sandbox-scope ~/.cargo` that would also grant cargo's binaries and credentials. The writable set is computed from `$CARGO_HOME` (or `~/.cargo`) and excludes `bin/`, `config.toml` and `credentials.toml`; the path list lives in the shipped `rust` profile (R-PERM.5). `--no-package-cache-write` / `[sandbox] package_cache_write = false` downgrades those `rw` rules to `rx`; it is the mitigation for the cross-project persistence channel this write opens (**R-HANDOFF.8**) and **must** be documented as such.
- **R6.2.6**: **Signals stay inside the command's own sandbox**: the Seatbelt profile **must** grant `(allow signal (target same-sandbox))`, not a blanket `(allow signal)`, so a sandboxed command can signal only the process tree it started under the same profile — not another session's build or any unrelated process of the same user. ahma **must** recognise the shell's `kill: (N) - Operation not permitted` and explain it as a boundary (the pid, that it belongs to another session, that a human must stop it). `[sandbox] signal_other_processes = true` is the explicit opt-out, and ahma **must** log a warning at startup when it is on.
- **R6.2.7**: **The GPU is a withheld capability, not a grantable path**: the Seatbelt profile **must not** grant `iokit-open` by default. `[sandbox] allow_gpu = true` is the explicit opt-in and **must** add only the Metal user-client classes Apple's own profiles allow (`AGX*`, `IOAccel*`/`IGAccel*`, the IOGPUFamily device client a paravirtualised GPU presents in a VM, and `IOSurface`), never a blanket `(allow iokit-open)`. As a capability it **must not** be offered at any grant prompt or by `sandbox_grant`; ahma **must** recognise the failure signatures (`failed to create command queue`, a nil Metal device, a Seatbelt `iokit-open` denial) and tell the agent it is a setting a human changes, with the exact key, and **must** disclose the withheld GPU on every R5.4 surface while it is off (R-PERM.5.1). The opt-in covers Metal compute, not only opening the device: a 256 MB buffer, a kernel compiled at runtime and a dispatch all succeed with it (a macOS CI probe holds this), so a GPU workload that fails inside the sandbox with `allow_gpu` off needs that one setting, nothing more. A failed Metal buffer allocation (ggml's `failed to allocate buffer`, a nil `MTLBuffer`) is the same capability; and a Metal failure while `allow_gpu` is on is reported as a suspected remaining denial, with the lines to quote, never as the tool's own failure.
- **R6.2.8**: **A sandboxed command can see processes**: the Seatbelt profile **must** grant `process-info*` so `pgrep`, `lsof` and ahma's own listing work; the process list is visible to every process of the user, so denying it protects nothing. `/bin/ps` is setuid root and **no** sandbox can execute a setuid binary, so ahma **must** ship `ahma ps` (pid, parent, start, whether the process is itself sandboxed, command line; read-only, never queued), and the hook, the skill and the denial text **must** say "use `ahma ps`" rather than leave `bash: /bin/ps: Operation not permitted` reading as a broken sandbox.
- **R6.2.11**: **A command may open pseudo-terminals of its own, and no other**: the profile allows `pseudo-tty`, `/dev/ptmx`, and `/dev/ttys*` only together with the `com.apple.sandbox.pty` extension the kernel issues to the process that allocated that terminal (Apple's own form). Test runners and `script` need a terminal (the iOS test runner failed with `openpty: Operation not permitted`); another process's terminal, such as the user's own shell, stays unreachable.
- **R6.2.9**: **xcrun tools run in the read-only lane**: the read-only lane (R2.7.4) grants no temp writes, since a workspace under the temp directory would become writable, except xcrun's cache file, matched exactly (`/private/var/folders/*/*/T/xcrun_db…`). macOS developer tools such as `strings`, `otool` and `nm` are xcrun shims that write it on first use and failed with "couldn't create cache file" without it.
- **R6.2.10**: **Launching apps through LaunchServices is never granted**: `open App.app`, `open -a` and `open <URL>` hand the launch to LaunchServices, which starts the app outside every sandbox; ahma has no quarantine, so an app planted in the workspace would run unconfined. `lsopen` stays denied, with no setting and no prompt. A refused launch (LSOpen `-10810`, `-10822`, `-54`) **must** be explained with what to do instead: run the app's binary directly (`App.app/Contents/MacOS/App`), which keeps it in the sandbox.

#### R6.3: Windows (AppContainer / Job Objects)

> **Security gate**: Windows GA requires this section at `tests-pass`. Until then strict mode **must** fail closed (`SandboxError::PrerequisiteFailed`) so the server never runs unsandboxed without the explicit `--no-sandbox` opt-out.

Status: AppContainer spawn isolation is built but disabled (`sandbox::windows::appcontainer_spawn_enabled`); until R6.3.3 holds both ways Windows has no OS filesystem boundary (open: §11).

##### Architecture decision

Two mechanisms, both in `sandbox/windows.rs`:

1. **AppContainer** (Windows 8+) — the path-confinement mechanism. Each tool subprocess runs under an AppContainer SID granted read+execute on the Windows system directory and full access only to the locked scope.
2. **Job Objects with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`** — applied unconditionally at server startup by `enforce_windows_sandbox`, so every child is killed when the server exits. It restricts no paths; R6.3.3 needs AppContainer.

##### Acceptance criteria (required before GA)

- **R6.3.1**: `check_windows_sandbox_available()` **must** return `Ok(())` when the AppContainer API is available (Windows 8+; probed by calling `CreateAppContainerProfile` with an invalid name) and `PrerequisiteFailed` on older systems.
- **R6.3.2**: `enforce_windows_sandbox(roots)` **must** apply Job Object containment at server startup so child processes die on server exit; its signature mirrors `enforce_landlock_sandbox` (`&[PathBuf]`), and already running inside a job is non-fatal.
- **R6.3.3**: Writes outside the scope **must** be blocked at the OS level, *and* a write inside it **must** still succeed — a sandbox that blocks everything proves nothing. It is met only when a `windows-latest` run shows in-scope writes and reads succeeding and out-of-scope ones blocked. The AppContainer path, measured on `windows-latest` by `appcontainer_dacl_diagnostics` (a shell-free probe, `ahma.exe` re-entered under the reserved first argument `__ahma-appcontainer-probe`, handled before CLI parsing), holds both ways: inside the container a write or read inside the scope succeeds and one outside is denied. It stays disabled, with its three behavioural tests `#[ignore]`d, because ordinary tools cannot run inside it: writing to `NUL` is denied, and so is stat, list or canonicalize of every ancestor of the scope, which also breaks Windows PowerShell 5.1 started from an 8.3 short path (open: §11).
  - **R6.3.3.1**: Two AppContainer limitations follow from the design and **must** be disclosed on the R5.4 surfaces, not worked around silently:
    - **(a)** AppContainer blocks loopback unless the container is registered via `CheckNetIsolation LoopbackExempt`, and the egress proxy (R-WEB.16) binds `127.0.0.1`, so `--restrict-network` and AppContainer isolation are **mutually exclusive**. With both requested, ahma **must** fail startup with an error naming both remedies, and egress restriction **must not** take effect that session: a proxy that `HTTP_PROXY`-honouring tools cannot reach, while other tools open their own sockets, would make the operator believe egress is gated when it is not, and R-WEB.16.8's approval prompt could never fire to say so (R7). The exclusion is conditional on the container: the spawn path (`sandbox/command.rs`) and the proxy (`shell/modes/server.rs`) both read `appcontainer_spawn_enabled()`, and while it is off `--restrict-network` **must** work on Windows as it does elsewhere.
    - **(b)** The user's `%TEMP%` lies outside every scope, so `TEMP`/`TMP` are redirected to the per-container folder (`GetAppContainerFolderPath`); a command's output will not be found in the user's `%TEMP%`, and R7.5's honest register applies.
- **R6.3.4**: `tools/call` issued before sandbox lock (state != `Locked`) **must** return HTTP 409 / JSON-RPC `-32001` on Windows, identical to Linux/macOS behavior.
- **R6.3.5**: Filesystem root scopes (`C:\`, `D:\`, UNC `\\server\share`) **must** be rejected by `canonicalize_scopes` with `SandboxError::PrerequisiteFailed`, identical to Unix `/` rejection.
- **R6.3.6**: PowerShell (built into Windows 10/11) **must** be documented as a runtime requirement; the server should emit a clear startup error if `powershell` is absent.
- **R6.3.7**: All existing integration tests that exercise sandbox gating logic **must** pass on Windows CI with no `#[ignore]` waivers.
- **R6.3.8**: **Cross-Platform Test Scripts**: Tests that generate and execute scripts **must** provide equivalent `bash` (Unix) and `PowerShell` (Windows) logic, **must not** rely on `bash.exe` or `sh.exe` on Windows (no WSL dependency), and **must** use one uniform helper (e.g. `write_cross_platform_script`).
- **R6.3.9**: **Reads are not confined on Windows either, for a different reason than on macOS**: a Job Object (R6.3.2) restricts no paths in either direction (`enforce_windows_sandbox`); path confinement comes only from AppContainer (R6.3.3). Application-layer path validation (R5.7, `path_security`) binds paths ahma itself resolves, not a spawned command's own syscalls. The R5.4 surfaces **must** state this (R-PERM.5.1) rather than display a scope the kernel is not enforcing.

##### Windows path model

- Sandbox scope paths use native Windows absolute paths (e.g., `C:\Users\name\project`).
- File URIs from MCP clients are parsed by `ahma_common::file_uri`, which handles `file:///C:/...` (drive letter) and `file://server/share/path` (UNC) forms.
- `normalize_path_lexically` never pops a `Prefix` or `RootDir` component (enforced by `scopes.rs`).

### R7: Nested Sandbox Detection and Deferral

A "host sandbox" is an outer kernel sandbox ahma runs inside (Cursor, Claude Code's Bash sandbox, VS Code, Docker). ahma can *name* a host from environment markers (`CURSOR_SANDBOX`/`CURSOR_AGENT`, `CLAUDECODE`/`CLAUDE_CODE_ENTRYPOINT`, `VSCODE_*`, `/.dockerenv`/`container`), but a marker proves only which harness launched the process, never that its sandbox is on. ahma **must not** chase a host's private internals (e.g. its injected build-cache env vars) to coexist and **must not** stand down on a marker; it applies its own sandbox on every execution path, defers only on the kernel's own proof (R7.6), and **always discloses which sandbox is active** (R5.4).

- **R7.1**: System **must** detect when running inside a host sandbox and, where possible, name it (Cursor/Claude Code/VS Code/Docker/an outer ahma); otherwise report it as an unidentified outer sandbox. Naming is for disclosure; it never changes enforcement.
- **R7.2 (terminal hooks — apply ahma's sandbox; defer only on kernel proof)**: An ahma terminal hook **must** rewrite every shell command into `ahma hooks run-shell` so it runs under ahma's own kernel sandbox, whatever host marker is set. Markers (`CLAUDECODE`, `CURSOR_SANDBOX`, `VSCODE_*`, …) may name an outer sandbox, never decide one: Claude Code sets `CLAUDECODE=1` whether or not its Bash sandbox is on. The only deferral is `run-shell`'s when the kernel refuses to nest ahma's profile (R7.6), disclosed on **every** such command on stderr, naming the host when a marker allows. `AHMA_PREFER_OWN_SANDBOX` is retired; own-sandbox is the only behaviour. Host build-cache friction (a redirected `CARGO_TARGET_DIR` the host denies) is diagnosed and explained (R5.5.3 remediation), not avoided by standing down.
- **R7.3 (MCP / standalone — stay authoritative)**: When ahma itself executes commands (the MCP `run_terminal_command` path, or standalone), the host sandbox does **not** wrap those executions, so ahma **remains authoritative** and applies its own sandbox. If it cannot, it **must** fail loudly with instructions (use `--no-sandbox` to defer to the host explicitly) — it **must never** silently run unsandboxed.
- **R7.4**: When `--no-sandbox` is used, the outer sandbox provides security and ahma's internal sandbox is disabled; the active-sandbox disclosure **must** reflect this (deferred-to-host when a host is detected, otherwise disabled).
- **R7.5 (honesty limit)**: Detecting a host does **not** prove its sandbox is *enabled*, which is why detection alone never decides enforcement (R7.2). Whenever ahma defers (R7.4 `--no-sandbox`, R7.6 kernel refusal), the disclosure **must** state that protection now depends on the host.
- **R7.6 (macOS Seatbelt cannot nest — defer at every execution path, never fail opaquely)**: macOS refuses to apply a Seatbelt profile inside a process already confined by one whose profile denies *anything* (`sandbox_apply: Operation not permitted`; only a no-op `(allow default)` outer profile permits nesting), so every real sandbox, ahma's included, forbids it. Every child of a confined process inherits the confinement, so running a command bare inside one is still kernel-sandboxed by the outer boundary.
  - ahma **must** decide when a `Sandbox` is **constructed** (not at the first spawn) whether this process can apply its own profile, from the kernel's answer (`sandbox_check` on its own pid) confirmed by a refused nesting probe — never environment markers alone (R7.5). This binds the in-process library path (tests, embedders) exactly as it binds `ahma serve` startup.
  - When nesting is refused the instance defers: commands spawn bare, `is_enforced()` is false, every R5.4 surface (startup banner, `sandbox/configured`, `ahma status`, TUI) reports `deferred_to_host`, and the deferral is logged at `warn` with the R7.5 disclosure and remediation.
  - When the outer sandbox is ahma itself it **must** be named (R7.1): every command ahma sandboxes carries `AHMA_OUTER_SANDBOX_PID=<pid>` (a marker ahma sets, not a setting it reads — R-CFG2.3 is unaffected), and a nested ahma that finds it reports "an outer ahma" with remediation naming `run_terminal_command`.
  - A `sandbox-exec` that cannot *execute* at all (missing, or SIGKILLed by the outer profile) remains the R7.3 hard stop: that is not proof of an outer sandbox. Linux Landlock and Windows Job Objects nest fine and are unaffected.
  - A **confined process must not spawn the per-user hub** (R-HUB.3): a hub that inherited an outer sandbox would defer every client's enforcement to it. Hooks in that state skip registration; a frontend fails loudly with the R7.5 remediation. The exception is test isolation (R-ISO.1): a process under a test harness resolves only its run's private endpoint, so the hub it starts is that run's alone.
- **R7.7**: **A child tool that applies its own sandbox is a capability refusal, not a path.** SwiftPM's manifest loader and `xcodebuild` package resolution call `sandbox-exec` themselves, so inside ahma's sandbox they fail with `sandbox_apply: Operation not permitted`. ahma **must** recognise that signature, explain it as the nesting limit, **must not** offer a directory grant for it, and names the tool's own switch (`swift build --disable-sandbox`; for `xcodebuild`, `-IDEPackageSupportDisableManifestSandbox=YES` on the command itself, which the agent can add with no human step; for Swift macros, whose plugin server sandboxes itself too and fails with "produced malformed response", `OTHER_SWIFT_FLAGS=$(inherited) -disable-sandbox`) as the way through.

### R-HANDOFF: Trust Handoff — Legitimate Writes That Something Trusted Later Executes

**Problem this family solves.** The requirements above answer "may the agent write here?"; this family answers "who reads it later, and what do they do with it?". In this class of attack **the agent never breaks the sandbox**: it writes a file it is entitled to write, and a **trusted component that was never sandboxed** — the user's `git`, an IDE extension host, the harness's hook engine, a container daemon — executes it later. The pattern is published (Pillar Security, 2026, against Cursor, Codex, Gemini CLI and Antigravity; one instance carries a CVE, R-HANDOFF.1), and ahma is in scope because its workspace is *freely* writable by design.

**Design principles (govern all of R-HANDOFF).** The deliverable is **make the handoff visible**, not "block the attack": blocking every file something later auto-executes would break `git config user.email` and "set up my editor for this project", and prompting on each would reintroduce the fatigue R5.3 forbids. So the response is two-tier (R-HANDOFF.3), boundaries are defined against abstractions rather than path spellings (R-HANDOFF.2), and wherever enforcement is not possible the gap is stated, per R7.

- **R-HANDOFF.1**: **The threat class is in scope and has a fixed shape**: (a) the agent writes a file the sandbox legitimately permits; (b) a trusted, unsandboxed component discovers it **by convention**; (c) it executes on a later trigger by the user or the harness. Illustrative instances:
  - a script under a git hooks directory, run by the user's next `commit` / `checkout` / `push`;
  - a `pyvenv.cfg` plus a planted `bin/python`, run by the VS Code Python extension's discovery from the **unsandboxed extension host**;
  - a `.vscode/tasks.json` entry with `runOn: folderOpen`, run the next time the folder is opened;
  - a harness settings file's `Stop` hook (`.claude/settings.local.json`), run by the harness's hook engine at the end of a turn — **CVE-2026-48124**, CVSS 8.5;
  - a container daemon socket used to start a `--privileged` container with a host bind mount, so the *daemon* performs the write (R-HANDOFF.6).

  The specification is the shape (a)–(c) plus the two-tier response (R-HANDOFF.3), not this list. The concrete path sets are shipped data owned by the sandbox modules, as profiles are (R-PERM.5), so they can be inspected, disabled and extended.
- **R-HANDOFF.2**: **A boundary is defined against the abstraction, never against a path spelling**: a path regex like `^.*/\.git/config$` is defeated by `git init --separate-git-dir=.git-alt`, which `git` honours while the pattern never matches. A rule that protects something a trusted consumer owns **must** be computed from the **resolved** form that consumer uses — the resolved git directory (`git rev-parse --git-dir`, following the `gitdir:` indirection in a `.git` *file*), the resolved virtualenv root, the harness's own settings-resolution order — and **must** be re-resolved rather than cached across a session. A rule that can only be written as a pattern **must** be labelled best-effort where it is surfaced.
  - **R-HANDOFF.2.1**: **Widening the boundary needs evidence the workspace cannot manufacture.** A command in a linked worktree or a `--separate-git-dir` checkout writes git storage *outside* the workspace (`<main>/.git`, `<main>/.git/worktrees/<name>`), so it needs a grant — which **must not** be computed from the same resolution the deny rules use:
    - the **deny** set follows *any* `gitdir:` pointer, deliberately — a lying pointer only adds a harmless extra deny;
    - the **grant** set may include only a directory already inside a scope, or one that **proves it knows this workspace** through a back-reference the trusted consumer writes: `<git_dir>/gitdir` naming the `.git` pointer file that was followed (`git worktree add`), or `core.worktree` in `<git_dir>/config` (`git init --separate-git-dir`). A `commondir` hop is followed only out of an already-verified directory, and only when the common dir owns the worktree dir that named it.

    The `.git` pointer file is inside the workspace and agent-writable, while the back-reference is outside it, so forging one needs the very access being requested; a shape test ("contains `HEAD` and `objects`") can be forged and is not sufficient. Everything **must** fail closed and be disclosed per R7 — a refused pointer or `commondir` hop as a warning, once per `(path, reason)` per process; a plain `.git` directory outside every scope (what a pre-scope tool probe finds) at debug — and grants **must** be emitted *before* every deny group, so credential denies, the SSH private-key deny and `<git_dir>/hooks` outrank them under last-match-wins. Hook denial stays platform-asymmetric (R-HANDOFF.4).
  - **Re-resolution has a floor, and the floor is disclosed.** A kernel policy is fixed at spawn, so a repository created *during* a command is covered from the *next* command; resolution scans to a bounded depth, so a repository cloned far below the workspace root is not reached by the kernel rules. ahma's own write tools resolve at write time and have neither limit. Per R-HANDOFF.4 both limits **must** be stated where the protection is described.
- **R-HANDOFF.3**: **Two tiers, and they must not be collapsed into one**:
  1. **Deny-write** — paths no legitimate agent task needs to write. **No question is asked**: the write fails with the R5.4.7 `sandbox_denial` payload and the R-PERM.3 rung-3 remediation. Hook directories under a resolved git dir, container daemon sockets, private key material, ahma's own control plane (R5.4.8) and ahma's project tool-config directory (R-HANDOFF.7) belong here.
  2. **Allow, and disclose loudly** — paths legitimate to write *and* auto-executed later: editor task/launch configuration, harness settings files, per-project VCS configuration. The write **succeeds** and **must** be surfaced as a first-class event (R-PERM.7) on the R5.4 surfaces, naming the file **and the trigger that will execute it** ("this runs the next time you open this folder") — the file name alone does not tell a user that a write became a future execution.

  Tier membership and each entry's human-readable reason are owned by `sandbox/exec_config.rs`; this document does not hold the list.
- **R-HANDOFF.3.3**: **A deny-write path with a legitimate author has a named, disclosed opt-in**: some deny-write members are written by real workflows — a repository's own git hook, a project's `.ahma/` tool definitions — and a default with no way out pushes users to disable the sandbox wholesale. Each such path therefore **must** have an operator toggle that defaults to denied; removes the path from the kernel rules and the application-layer guard **together** (relaxing only one fails later with a bare `Operation not permitted`); is disclosed at startup per R7, naming what became writable **and what will execute it**; and is named **in the denial message itself**. Paths with no legitimate author — hub sockets, fabricated interpreters — get no toggle, and their denial **must** say so.
- **R-HANDOFF.3.4**: **A grant question names what will run the target**: when a grant would let the agent write a path *outside* the scope that something trusted executes later — a shell startup file (every new shell), a login item (`~/Library/LaunchAgents`, `~/.config/autostart`; at login) or a `.git/hooks` directory (git on commit, checkout and push) — the prompt classifies it high risk and says in one factual sentence what will run it and that nobody will be asked when it does. These are warnings, not refusals. The list lives with the denylist (`scope_grant::auto_execution_warning`).
- **R-HANDOFF.4**: **Enforcement is asymmetric across platforms, and the asymmetry is a requirement-level fact**: the deny-write tier is a **hole inside an allowed subtree**, and platforms differ on whether the kernel can express one.
  - **macOS — kernel-enforced, for rules that are concrete subpaths.** SBPL is last-match-wins, so a `(deny file-write* …)` after the workspace allow subtracts (as the credential read denies do, R6.2.3). A *shape*-matched rule (the virtualenv interpreter case, where a kernel deny on every `bin/python*` would break `python -m venv`) is application-layer everywhere, so the kernel deny set and the write-tool-only set are **different sets** and code **must not** blur them.
  - **Linux — application-layer only** (R6.1.7). ahma enforces the hole in its own file tools, so it is **bypassable via `run_terminal_command`**: a shell child inherits the workspace-wide Landlock write right. Such a write is detected and reported after the command (R6.1.7), never prevented.
  - **Windows** — the application-layer check is the only control (§4 table); shell-command writes are detected as on Linux (R6.1.7).

  Per R7 and R7.5 this asymmetry **must** be disclosed wherever the protection is claimed: a Linux user **must** be able to learn from ahma itself that the protection stops at the shell, because an application-layer check presented as kernel-enforced is relied upon.
- **R-HANDOFF.5**: **A child's environment is part of the handoff surface**: two categories **must** be stripped from every sandboxed child environment, alongside the secret-pattern scrub owned by `sandbox/command.rs`:
  1. **Code-injection variables** — those that make an unrelated process load or run something of the agent's choosing (`BASH_ENV`, `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES` and its family, and future platform equivalents).
  2. **Client-redirect variables** — those that re-point a trusted client at an attacker-chosen endpoint (`DOCKER_HOST`; R-HANDOFF.6).

  `SSH_AUTH_SOCK` is deliberately **kept**: the socket lets a process *use* keys, not read them, so git-over-ssh keeps working while the key files stay denied (R6.2.3). The stripped set is data owned by `sandbox/command.rs` and is not enumerated here.
- **R-HANDOFF.6**: **Delegated execution is execution**: a local daemon that accepts work over a socket — a container runtime above all — is a "do this outside the sandbox" service: a `--privileged` container with a host bind mount turns write access to a socket into unrestricted host write access. Daemon sockets therefore belong to the deny-write tier (R-HANDOFF.3 tier 1), their reads to the credential deny set (R6.2.3), and their client-redirect variables to the stripped set (R-HANDOFF.5). Containerized builds stay available by granting the socket explicitly through R5.4.5, where a human sees what is handed over.
- **R-HANDOFF.7**: **ahma's own project tool configuration is not agent-writable, and does not hot-reload**: a workspace's `.ahma/` tool-config directory defines commands ahma itself runs, so an agent that could write it could define a tool and call it. It is in the deny-write tier despite sitting inside the workspace. There is no tools-directory watcher (R1.4); reload happens only through the explicit `restart` tool. This is separate from R5.4.8 (the user-level `~/.ahma`); the project directory is a different trust tier (R-CFG2).

#### Known limitation: shared toolchain caches are a cross-project channel

- **R-HANDOFF.8**: **A machine-global package cache is cross-project persistence, and this is a disclosed residual risk**: the shipped `rust` profile grants **read-write** on `$CARGO_HOME/registry` and `$CARGO_HOME/git` (both pre-created), and profiles ship **enabled by default** (`sandbox/profiles.rs`); cargo needs the write to extract `.crate` archives and materialize git dependencies. Consequently **an agent working in project X can edit the extracted source of a cached crate, and that code runs — as a build script or proc macro — when the user later builds an unrelated project Y**, with no sandbox rule violated.
  - R5.2.6's container narrowing does not help: the cache is shared by every project on the machine.
  - The mitigation is `--no-package-cache-write` / `[sandbox] package_cache_write = false`, which downgrades the profile's `rw` rules to `rx` so the toolchain stays runnable (R6.2.5). It **must** be documented as the answer to this named risk, and the risk **must** appear alongside the profile in the R5.4 scope surfaces and in `ahma permissions list` (R-PERM.5.2).
  - This is a residual risk, not a bug: `cargo add` / `cargo update` need the write, and the alternative — granting all of `$CARGO_HOME` — would hand over `credentials.toml` and every binary on the user's PATH, which the profile's `deny_write` assertion prevents. The requirement is **disclosure plus an available opt-out**.
  - Any shipped profile that grants `rw` on a machine-global cache opens the same channel and **must** state its cross-project consequence in its own description.
- **R-HANDOFF.9**: **Build-time code execution is part of the agent's write set**: build scripts (`build.rs`) and procedural macros run at build time with the **full write set of the sandbox** — the workspace, the temp scopes, and every `rw` path an enabled profile granted. **Adding a dependency is adding code that runs locally**; the sandbox bounds *where* that code can write, never *whether* it runs. R5.4.5 relies on this bound (a build script cannot author a grant), and R-HANDOFF.8 is the case where it composes into cross-project persistence.
- **R-HANDOFF.10**: **Every execution leaves durable provenance, on the default path and not only in a vault**: a trust handoff (R-HANDOFF.1) is discovered after the fact, and operation output says what a command printed, not that it ran.
  - ahma **must** write an append-only execution audit log on **every** execution path — synchronous, asynchronous, PTY and session — not only inside a task vault. Each execution records a `tool_call` **before** the process is spawned, and exactly one matching `tool_complete` on **every** terminal path (success, failure, timeout, cancellation, spawn error), so no panic, `SIGKILL` or power loss can leave the log without a record of what was asked for.
  - Sandbox denials **must** land in the same log, whether refused by path validation or surfaced as a kernel denial at runtime (R5.4.7). The wire format **must** be the vault's (`ahma_mcp::vault::audit::AuditEvent`), so one reader parses both logs, and a test **must** assert the compatibility.
  - Recorded fields **must** be redacted through the *same* function operation output uses, and every free-form field **must** be individually bounded, which keeps one event one `write` syscall so concurrent `O_APPEND` writes do not interleave without a process-wide lock.
  - A deny-tier write the kernel did not stop (R6.1.7) **must** land in the same log as a `handoff_write` event, one per entry.
  - **An audit write failure must never fail the operation it records** (R-PERM.2.2, generalized), but **must** be reported at `warn` naming the path.

### R-PERM: Unified Permissions Model

**Problem this family solves.** When the sandbox blocks something the user legitimately wants, the user can grant an exception. Kernel denial detection (R5.4.7), persistent grants (R5.4.4–R5.4.8), elicitation prompts (R5.3.1), the TUI modal (R-WEB.6) and the dedup coordinator (R-WEB.7) converge on one ledger (R-PERM.1), one record shape (R-PERM.2) and one question ladder (R-PERM.3), so that a hook denial has a path to a user decision (R-PERM.6) and toolchain carve-outs are data, not code (R-PERM.5).

**Design principles (govern all of R-PERM), inherited from R5:** the kernel denial *is* the discovery mechanism — ahma cannot predict what applications it has never seen need, but the kernel reports the exact path at the moment of need. The loop is **deny → detect → ask once, with context → remember at a chosen tier → apply**. Nothing is silent; nothing self-widens; the user is asked only on a genuine downgrade; when nobody can be asked, ahma fails closed to a shown default.

#### One ledger

- **R-PERM.1**: **All persistent permissions live in `~/.ahma/`, and nowhere else**: filesystem scope grants (R5.4.4), web-domain grants (R-WEB.5), per-workspace tool approvals, log-symlink targets (`log-target`, R9.2) and hook unsandboxed consent (R5.5.3) **must** share one control-plane directory. `~/.config/ahma/` is not a permission store: an `approvals.json` or `log_exceptions.json` found there **must** be migrated once, non-destructively, leaving the legacy file with a `.migrated` suffix. The ledger directory is `~/.ahma` (R5.4.8), so a sandboxed command **cannot** author it.
- **R-PERM.1.1**: **Tool trust is keyed by workspace, and never leaks between them**: a `tool`-kind grant records the **canonicalized workspace root** it applies to. Approving `cargo_build` in one project **must not** approve it in another, because the code it would run is different. The key is canonicalized (`workspace_key`) so a symlinked or non-normalized spelling still matches the grant given and cannot dodge a revocation.
- **R-PERM.1.2**: **One answer is one question**: a tool call that needs approval is asked about at most once per `(workspace, tool)` at a time. Parallel calls to the same tool **must** share the question, and each **must** re-check the persisted grants once it holds the question, so an "always allow" (or a trust, R-PERM.1.3) for the first covers those queued behind it. The grant is persisted under the workspace the *asking agent* checks (its locked sandbox root, carried as `ApprovalRequested.workspace`, field-only per R24.5), never the answering surface's working directory.
- **R-PERM.1.3**: **Trusted folders**: the first time `ahma tui` opens a folder it asks once, "Trust this folder?". Yes records trust for the canonical folder (as the `*` entry of its `tool_approvals`, so an older reader keeps asking). In a trusted folder every tool that runs **inside the folder's kernel sandbox** runs without asking. Trust **never** covers what reaches past that boundary: tools on external MCP servers (`server::tool`), `sandbox_grant`, `logs_approve`, `fetch_webpage` (its own egress gate, R-WEB.6), `!` commands (R-HUB.9), or any change to `~/.ahma` or a project's `.ahma/` (R5.4.8, R-HANDOFF.7). Trust is never offered for — and `trust_workspace` refuses — a filesystem root, the home directory, or an ancestor of it. Enter, Esc and `n` answer "ask per tool" (R5.3.1). Revoke with `ahma permissions revoke tool '*' --workspace <dir>`.
- **R-PERM.2**: **One record shape, one preview, one confirmation**: every grant, of every kind, is representable as `{kind: fs-scope | web-domain | net-host | tool | hook-unsandboxed, subject, access, tier, granted_by, granted_at, surface, note}`, with `tier` one of `once` | `session` | `lease` | `always`.
  - A `once` grant is **never** stored; for a filesystem scope it is applied live for the next command the session starts and retired when the one after it starts (`Sandbox::add_once_grant`, `Sandbox::begin_command`), and passes the live gate (R-PERM.4.3) like every tier. A `session` grant lives **only** in memory and dies with the instance.
  - Only the persistent tiers (`always`, and `lease`, which ends on its own, R-PERM.2.3) are written to disk, and only after the preview-and-approve exchange R5.4.5 mandates, for every kind: the user is shown the **absolute file path** and the **exact line(s)** to be written, and nothing is written without explicit approval.
  - The R5.4.5 hard denylist gates **every** write path into the ledger because it runs inside the one persist function those surfaces share; the MCP tool has no write path of its own.
- **R-PERM.2.1**: **One CLI, one audit trail**: `ahma permissions list | grant | revoke` **must** manage every kind through the same preview-and-confirm path, showing provenance (`granted_by`, `surface`) for each record. Kind-scoped aliases (`ahma sandbox grant|list|revoke`, `ahma web allow|list|revoke`, `ahma network allow|list|revoke`) **must** keep working, because ahma emits them as remediation. Every persist and revoke **must** append one record to an append-only audit log in `~/.ahma/`.
- **R-PERM.2.2**: **Recording a decision must never destroy it** (R-WEB.9.3, generalized to every kind): an **audit-log write failure is non-fatal** and **must not** fail the grant it records (it is logged, not propagated), and a **legacy-migration failure (R-PERM.1) must never block startup** (the worst case is re-approving a tool once). Neither relaxation runs the other way: a failure to **persist a grant** is fatal to that grant and **must** be reported.
- **R-PERM.2.3**: **A grant can be a lease, and a lease never cuts off work in flight**: a saved filesystem grant may carry `expires_at` (Unix seconds; `ahma sandbox grant --for <dur>`), for something a task needs outside the project for a while rather than forever.
  - Every grant prompt offers it between `session` and `always` — "read-only / read-write for 24 hours (saved; ends on its own)", `l`/`L` in the TUI, `read-only-24h`/`read-write-24h` in the elicitation form — and the advisor may recommend it.
  - An expired lease is **never applied** — not at load, not by terminal hooks or the edit guard (which rebuild their scope per command from the same filtered list) — and stays in the file, shown as expired with the renew command, until renewed or revoked. The settings writer carries the field, so a lease survives ahma regenerating the file.
  - A running server withdraws a lapsed lease **when the next command starts** (`Sandbox::begin_command`), never under a running command, whose kernel policy was fixed at spawn; withdrawal only narrows (R5.1).
  - `ahma sandbox renew` extends a lease through `persist_grant`, so the denylist and audit apply; a grant without an expiry has nothing to renew.
  - Every listing shows when a lease ends, and `ahma permissions list --expiring <dur>` and the `status` tool's SANDBOX section (leases ending within 12h, each with its renew command) let a long unattended run be checked **before** it starts.

#### R-DOCTOR: ahma explains itself, and repairs only with consent

- **R-DOCTOR.1 — One set of checks.** `ahma doctor` and the TUI's `/doctor` run the same checks (`ahma_common::doctor`): settings parse, granted folders that no longer exist, approvals for folders that no longer exist, the hub's build (as its own `/health` reports it) against this binary's, this folder's trust, and the most repeated warnings in the latest log. Each finding says what it costs and what would fix it. The checks are read-only.
- **R-DOCTOR.2 — A fix is shown, then confirmed, one at a time.** A fix changes nothing until the user answered `y` to that exact fix (TUI modal, or `--fix` on a terminal; without a terminal nothing changes). Every fix writes through `AhmaSettings::update` and is recorded in the permission audit log.
- **R-DOCTOR.3 — The model advises; it never applies.** `/doctor <question>` sends the report and the question to the chat model with rules it must keep: it cannot change settings (they are outside every sandbox, R5.4.8), it names the `/settings` row, `/doctor fix <n>` or `ahma` command the user can use instead, and it never suggests widening access without saying what that would allow. Nothing in its answer reaches a fix.
- **R-DOCTOR.4 — Tests never touch the real home.** In debug builds under a test harness, a test that did not choose a home (`AHMA_TEST_HOME`) gets a private per-run one in the build's target directory (`<target>/tmp/ahma-test-homes`), which a test binary and every `ahma` it spawns resolve alike and which lies inside the workspace, so the suite also runs inside ahma's own sandbox. A runtime directory that cannot be created is reported and treated as unavailable, and a failed spawn names its command and working directory.
- **R-DOCTOR.5 — Antigravity permission health and repair.** `ahma doctor` and `ahma doctor --fix` **must** inspect `~/.gemini/antigravity-cli/settings.json` and `~/.gemini/config/config.json`. `ahma doctor` warns about bloated one-off wrapped command entries (`command(...)` containing `hooks run-shell` and `--command`, `--payload-base64` or `--cwd`) and malformed entries (`.*`, `\.`, or multiline entries without `regex:`); `--fix` prunes them and ensures the clean prefix token grants (`command({exe} hooks run-shell)`, `command(ahma hooks run-shell)`, etc., R5.4.2) are installed.
- **R-DOCTOR.6 — Git can authenticate from inside the sandbox, or the doctor says exactly what to run.** The sandbox denies reads of `~/.ssh/id_*` (R6.2.3) and forwards only the agent socket, so a sandboxed `git push` over SSH works only if the agent holds the key (on the host ssh reads the key file directly, so an empty agent is invisible until the first sandboxed push). `ahma doctor` **must** report whether git's `credential.helper` can run under the sandbox (`osxkeychain` needs `[sandbox] allow_keychain`, the `gh` helper needs `~/.config/gh` readable) and, when it cannot, name the one key or command; the build diagnostics **must** recognise an HTTPS authentication failure (`could not read Username for 'https://…'`, `Authentication failed for 'https://…'`) with the same exact remediation the SSH `Permission denied (publickey)` gets.
- **R-DOCTOR.7 — A helper left running inside a sandbox is found by its shape, not its name.** A long-lived process started by a sandboxed command (a sccache server, a Gradle or Kotlin daemon) inherits the sandbox, outlives the command and serves every session while able to write only the checkout it was born in, so every other checkout fails with a bare `Operation not permitted`. `ahma doctor` **must** report any process that is Seatbelt-confined, has been reparented to launchd, and runs the user's own executable (under `$HOME`, so an App-Sandboxed application never matches), with the one restart line; `--fix` restarts the protocol it knows (`sccache --stop-server` / `--start-server`) and refuses when the doctor is itself confined. An unconfined ahma server also restarts a confined sccache at startup, and the build diagnostic names the shape.
- **R-DOCTOR.8 — The doctor reports whether prompts are being read.** From the permissions audit log it **must** report, over the last 200 answered grant prompts, the median time-to-decision, how many were answered in under three seconds, and how often the advisor was followed; a median under three seconds is a warning that names the habit, because a prompt answered faster than it can be read is theater (reflex approvals are measured at about two seconds).

#### The question ladder (where a permission question is asked)

- **R-PERM.3**: **Surfaces are tried in a fixed order, and the harness is preferred**: when a permission question must be asked, ahma **must** try, in order:
  1. **The initiating MCP client (harness)**, via `elicitation/create` (R5.3.1) — *iff* it advertised `elicitation` at `initialize` **and** has not been demoted this session. The user is already looking at it, and it carries the context of the work that triggered the denial.
  2. **An attached ahma TUI**, via the grant modal (R-WEB.6 semantics: the modal renders over both chat and monitor modes; **Enter and Esc both deny**; the persist option names the settings file and the exact line).
  3. **Nobody can be asked → fail closed** (never open): the operation fails with the R5.4.7 `sandbox_denial` payload **and** a copy-pasteable remediation command (`ahma sandbox grant <path> --ro|--rw`). This rung works in every harness, including clients that render only tool-result text, and is the *only* rung guaranteed to exist.
- **R-PERM.3.1**: **Demotion is for broken surfaces, not for "no" answers**: an elicitation **timeout or transport error** demotes that client's elicitation channel for the rest of the session ("one strike"), and later questions skip to rung 2. A **decline is an answer**: it resolves the question as Deny and the client stays trusted as an asking surface.
- **R-PERM.3.2**: **The user is told where the question went**: whenever a fallback occurs (rung 1 unavailable or demoted, or rung 2 absent), the resulting message **must** state that the preferred surface could not be asked.
- **R-PERM.3.3**: **Multiple live surfaces are coordinated, not raced**: when rungs 1 and 2 are both live for one question, the server owns the decision under one `decision_id` and applies R5.3.3 (first answer wins; the other surface is dismissed via `notifications/cancelled`).
- **R-PERM.3.4**: **One prompt body, every section, at every surface**: a scope-grant question **must** be rendered from one structure (`ahma_common::grant_prompt::render`) so the human reads the same complete question wherever it is asked; a prompt without *what for* is answered by reflex.
  - **Sections, in order**: who is asking (client, workspace, session); what was blocked (the command, the path the kernel named, the evidence line, whether the path is exact or read from output, how many times it has been asked); what the agent says it needs (its own words, labelled as its claim); the minimum that would work (read-only unless a write was refused); what a grant lets every later command in the workspace do; the risk class with its warnings and observed facts (names and counts, never file contents); and the exact settings line an `always` answer writes — then the choices, deny first.
  - **Per surface**: the elicitation form sends the sections as its message and the choices as a titled single-select (`oneOf` const/title pairs, `deny` first, never a free-text box); the TUI modal, the hook's terminal text and the `sandbox_grant` result render the same body (a client that cannot show a prompt is told to relay it unchanged). Only the TUI shows key letters.
  - **Context is required**: the sender attaches what it knows (`GrantContext`), and a sender that knows nothing still yields a complete body saying "unknown", never a shorter one. Carrying the context is the notifier's **required** method (`ScopeGrantNotifier::notify_violation_with`), with no default, so no implementation can silently drop it. The terminal hook fills the same sections from what it knows (the harness its environment names, the harness session, its scopes and the command), with risk from the same function as the server's.
  - The full body is pinned by a golden fixture (`ahma_common/src/testdata/grant_prompt_full.txt`), so any change to what the human reads is a reviewed diff.
- **R-PERM.3.5**: **A key-driven surface never takes a grant before it can be read**: questions arriving while one is open wait in order and never replace it, so a key answers the question on screen. A question that takes the screen, first or after another left, ignores granting keys for its first moment (`GRANT_ARMING_DELAY`), however long it waited; deny keys act at once. A key that saves a grant to settings first shows the exact line it writes, and only a second press saves.
- **R-PERM.3.6**: **"Attached" means someone is watching**: rung 2 is used only while the hub reports at least one TUI subscribed (`HubMsg::Viewers`, sent to an instance when it registers and whenever the count changes; zero while the instance is disconnected). A hub connection alone is not a surface. With nobody watching, the question falls to rung 3 and is not left in flight.
- **R-PERM.3.7**: **A waiting question outlives the TUI that showed it, not the session that asked it**: the hub keeps every unanswered scope-grant and web-approval question, oldest first, and replays it to a TUI that subscribes later (a TUI that already holds it ignores the repeat). When the asking instance disconnects, the hub dismisses its waiting questions at every TUI.
- **R-PERM.4**: **Ask at most once per `(subject, access)` per session**, across all surfaces and concurrent operations (R5.4.7, generalized). A `session`-tier answer in **either** direction suppresses further questions for that subject for the life of the instance; a denied subject **must not** re-prompt. The one exception is an explicit user re-raise (R-PERM.7.1).
- **R-PERM.4.1**: **When an answer applies**: on the MCP server path, an offline grant (`ahma sandbox grant` CLI or manual config edit) is persisted and takes effect at the next server start for its workspace (R5.4.6, R5.4.11). A grant a human approves at a prompt raised by the broker, the TUI modal or `sandbox_grant` takes effect **immediately** for that session; at the `always` tier it is also persisted, at the `session` tier (`read-only-session` / `read-write-session` in the elicitation, `[o]` / `[s]` in the TUI) it is applied live, audited, and never written. On the **terminal hooks** path the sandbox is re-derived per command, so a grant takes effect on the next command with no restart, and the confirmation message **must** say which applies (R-PERM.9).
- **R-PERM.4.3**: **Every live widening passes the hard denylist**: the `session` tier never reaches `persist_grant`, so the denylist **must** also run where a live grant is applied (`Sandbox::add_live_grant`), and a request for a path it refuses **must never be raised as a question** at any surface (`GrantCoordinator::begin` returns nothing and logs why). A denial on a file directly under `$HOME` offers the *file*, never the directory, and a file inside a credential directory is refused outright like the directory. A prompt whose right answer is always "no" trains the reflex click.
- **R-PERM.4.4**: **The `session` tier reaches terminal hooks and native edits**: a session answer given at a harness prompt, the TUI modal, or `ahma sandbox grant --session` **must** be recorded under `runtime_dir()/session-grants/` (owner-only; one file per grant naming the workspace, the owning pid and the time) and honoured by `ahma hooks run-shell` and the edit guard for commands in that workspace until the owning process ends or 12 hours pass, whichever is first; it is never written to `settings.toml`. `ahma sandbox list` shows the session grants in force.
- **R-PERM.4.5**: **A session has a prompt budget**: ahma raises at most five automatic grant questions per ten minutes per session (`GrantCoordinator::PROMPT_BUDGET`); past that nothing is raised, the agent's `sandbox_grant` result says the budget is spent and tells it to ask the human in conversation and continue with what it has, and the ask-once memo still applies. An explicit human re-raise (R-PERM.7.1) is not counted.

#### Sandbox profiles (no app-specific exceptions in code)

- **R-PERM.5**: **Toolchain carve-outs are shipped data, not compiled-in special cases**: the sandbox backends **must not** hard-code application or toolchain paths (`~/.cargo`, `~/.rustup`, `~/.nvm`, `~/.npm`, `~/.go`, the cargo package-cache write set). Each carve-out **must** be a declarative **profile** — a data file naming scopes, their access, and any `never` exclusions within them (e.g. cargo's `bin/`, `config.toml`, `credentials.toml`) — folded into the effective scope through the **same code path** as a user grant, with provenance `builtin-profile(<name>)`. A profile is a **pre-answered bundle of grant questions**, so it can be shipped, contributed, inspected and disabled.
  - Profiles **must** be visible in `ahma permissions list` and the R5.4 scope displays with their provenance, and individually disableable (`[sandbox] profiles`). Default is **opt-out**: builtin profiles ship enabled, visible and refusable.
  - Platform-invariant rules (`/usr`, `/bin`, `/etc` read/execute; device-path denials; credential-directory denials) are **not** profiles and stay in the backends. The test is *app-specific*, not *platform-specific*.
- **R-PERM.5.1**: **Every platform states what its kernel does not enforce**: a platform limitation is not a grant and cannot be a profile, so it **must** be disclosed on every R5.4 surface — startup banner, `ahma status`, TUI scope panel — in the honest register R7.5 requires. One function, `sandbox::profiles::platform_enforcement()`, returns the notes for the running platform (the §4 table): macOS reads (R6.2.2, compensated by R6.2.3), Windows in **both** directions while AppContainer is off (R6.3.9), and Linux's trust-handoff deny tier, bypassable from `run_terminal_command` (R6.1.7, R-HANDOFF.4). Each note **must** say what is not enforced *and* what to do instead. `notifications/sandbox/configured` carries the machine-readable form as `reads_unrestricted`, `writes_unrestricted` and `platform_notes`, add-only per R24.5.
- **R-PERM.5.2**: **A profile's cost is disclosed with the profile**: wherever a profile is listed, a profile that grants `rw` on a path shared across every project on the machine **must** show its cross-project consequence and its opt-out (R-HANDOFF.8); a grant whose consequence is invisible is not meaningfully refusable.
- **R-PERM.5.3**: **A profile declares hostnames, not only paths**: a toolchain carve-out that grants `~/.cargo` but not `index.crates.io` breaks `cargo build` the moment `--restrict-network` is on. A profile **must** therefore be able to declare the hosts its toolchain needs, each with a mandatory human-readable `reason`, resolved through the same shipped-data path as its scopes.
  - The reachable set is the **union** of the operator's `[network] allow` and the enabled profiles' hosts; neither suppresses the other.
  - Host grants **must** be refusable **independently of path grants**: `[network] profile_hosts = false` drops all profile-contributed hosts while keeping the filesystem carve-outs, and `[network] deny_profile_hosts` does it per profile.
  - A **shipped** profile **must not** declare `*`; that is meaningful only as an operator's explicit choice in `[network] allow`.
  - Network restriction stays **opt-in** (`[network] restrict` defaults to `false`); profile hosts make it usable when chosen.
- **R-PERM.5.4**: **Every reachable host names its grantor**: wherever the effective allowlist is displayed — startup disclosure, `ahma permissions list` — each host **must** carry the source that granted it (`builtin-profile(<name>)` or `[network] allow`) and, for a profile host, the profile's reason, so an operator can find the single line that removes it (R-PERM.5.2 applied to egress).
- **R-PERM.5.5**: **A profile may set variables, never over yours**: a profile may declare environment variables (`[[env]]`: name, value, mandatory `reason`) set on every sandboxed command, with `${WORKSPACE}` and `${WORKSPACE_PORT}` (a port in 20000–59999 derived from the workspace path by a fixed hash, so it survives upgrades) besides the environment. A variable already in the environment **must not** be overridden: the user's choice wins. Each variable **must** be shown with its value and reason wherever the profile is listed (R-PERM.5.2), and goes when the profile is disabled. The motivating case is `sccache`: a compile-cache server started by a sandboxed build can write only that checkout, so one shared server breaks every other checkout (R-DOCTOR.7). The `sccache` profile gives each workspace its own cache and port, so a server born confined only ever serves the checkout it can write; such a server is confined by design and is neither restarted nor reported as a problem.

#### Hooks gating

- **R-PERM.6**: **Hooks are enabled per client, gated on the ladder — not on perfect classification**: `ahma setup` **must** enable hooks for a client only once it satisfies:
  1. **The loop closes in that client**: a denial round-trips deny → question (on whichever rung applies) → grant → the *next* command succeeds (R-PERM.4.1).
  2. **The fail-closed message is legible in that client**: the R-PERM.3 rung-3 message is surfaced where the user will see it — never *only* as a bare `Operation not permitted` line inside a build log.
  3. **Nested sandboxes defer only on kernel proof**: inside a host whose sandbox genuinely confines the command, the kernel refuses to nest ahma's profile and `run-shell` defers with a disclosure (R7.2, R7.6); a host *marker* alone never makes a hook stand down.
- **R-PERM.6.1**: A hook denial has no MCP session of its own. Rung 1 is available **only** when a live MCP session for the same workspace can be asked; otherwise the ladder starts at rung 2 (attached TUI) and falls to rung 3 (a remediation block printed to the terminal the command ran in, *after* the command's own output, so it is not lost in scrollback).

#### Making the question findable

- **R-PERM.7**: **A denial is a first-class, visible event**, not just an error string: every denial **must** appear in the operation stream with the operation identity of the command that caused it (R24.7), so it is visible in the TUI monitor and chat views and in replay after late attach.
- **R-PERM.7.1**: **A denied operation is selectable and re-raisable**: in the TUI, selecting a denied operation and confirming **must** re-raise the grant question through the same broker, with the same preview. As an explicit human action it bypasses the R-PERM.4 ask-once memo.
  - *Implementation*: the denial travels the hub wire on `OpFinished.denial` (`{path, access}`, add-only per R24.5; `status` stays `"Failed"` for pre-upgrade readers); the TUI promotes it to `OpStatus::Denied` and renders `denied: <path> · [a] ask`; `a` sends `ClientMsg::ReRaiseScopeGrant`, which the hub routes to the owning instance, where `GrantCoordinator::reopen` clears the ask-once memo for that `(path, access)` and raises a normal `ScopeGrantRequested` — same broker, modal and persistence path.

#### The advisor (a model recommends; it never answers)

- **R-PERM.8**: **A model may recommend an answer to a grant question, and never give one.** Only the TUI's selected model advises; a sampling-based advisor (the asking client's own model over MCP sampling) is out of scope, because the asking agent's model judging its own request is circular. The prompt **may** carry one labelled line: `advisor: <deny|once|session|always> · <access> — <reasons> (confidence N%)`. The rules:
  1. The advisor sees the request **minus the agent's claim** — who asked, what the kernel refused, the command line, the observed facts about the target (names and counts, never file contents), the risk class — and never the agent's transcript, because a judge shown the actor's own summary approves most of what it is shown.
  2. It **never** changes the pre-selected default (deny), never answers, and runs only after the hard denylist (R-PERM.4.3).
  3. It is time-boxed (`[permissions] advisor_timeout_secs`, default 6) and fail-quiet — the prompt is complete without it.
  4. Path names and command lines are declared untrusted in its instructions.
  5. Every answer records the advice and whether it was followed in the audit line (`advice`, `advice_followed`), which makes rubber-stamping measurable (R-DOCTOR.8).
  6. Its tier guidance is the policy's: `once` for a single command outside the project, `session` for caches and build outputs, `always` only for a path asked in two or more sessions and read-only unless a write was refused.

  `[permissions] advisor = false` turns it off. The provider is pluggable (`ahma_core::advisor::advise` takes any client).

#### What the human is told

- **R-PERM.9**: **Every user-facing permission message leads with whether the user must do anything.** The first line after a grant, a denial or a disclosure **must** be one of `Nothing more to do…`, `One thing to do: <the exact command or key>…`, or `Blocked until <who> <does what>…`, computed from the surface it is printed on (terminal hooks and the edit guard re-read grants per command; a running MCP session, until it watches the ledger, needs its connection restarted or the prompt it raised answered) — never hedged with "may" or "the next time a server starts", and never two answers in one message. When the user must act, the message names the exact action — the full command to paste, or the key to press and where — never "restart your IDE" or "grant it in the TUI".
- **R-PERM.9.1**: **A refusal is reported once, in proportion.** A terminal-hook command that failed with a refusal outside the workspace gets the full prompt body once (it was printed twice: to stderr and again as the returned error), led by what was refused rather than "blocked", since the command may have failed for another reason. A command that **succeeded** although one of its accesses was refused (a lock-holder file, a cache it can do without) gets one line: what was refused and the `ahma sandbox grant` that would allow it.

---

## 4.5 File System Contracts and Features

### R-LOG: Project Logging (`.ahma/logs` directory)

- **R-LOG.1**: All ahma and execution logs **must** be placed in `.ahma/logs/` at the root of the (primary) sandbox scope, not in a global user cache directory (`~/.cache`). Nesting under `.ahma/` puts the ignore rule of R-LOG.3 in `.ahma/.gitignore`, never in the project's own top-level `.gitignore`.
- **R-LOG.1.1**: **One project resolves to one log directory, on every execution path.** Where no scope has been locked yet, the log directory is anchored on the enclosing **repository root**, never on the process's current working directory. This binds the paths with no `roots/list` of their own — above all the terminal-hook path (R5.5), where each hooked command is its own short-lived process whose cwd is the command's directory. Build tooling that scans a tree by convention (an Android `res/`, an asset pipeline) picks up plaintext logs regardless of `.gitignore`, so a stray `.ahma/logs/` in a subdirectory is a disclosure hazard.
- **R-LOG.1.2**: Resolution order: `--log-dir` flag → `[logging] dir` in `settings.toml` → primary sandbox scope → repository root (R-LOG.1.1) → a per-project namespaced directory under `~/.ahma/logs`. A directory the user wrote down outranks one ahma discovered. A flag- or settings-chosen directory need not live under `.ahma/`; R-LOG.3's automatic gitignore management applies only to the default `.ahma/logs` location.
- **R-LOG.2**: At startup the log directory is created if missing, and ahma's managed rolling logs in it (`ahma.log*`, `ahma_bridge.*`) older than 24 h (`LOG_RETENTION_SECS`) are deleted. The execution audit log (`audit.jsonl`) is never swept.
- **R-LOG.3**: ahma **must** disclose the active log directory once at startup. For the default `.ahma/logs`, ahma **must** silently ensure `.ahma/.gitignore` covers it. For a custom log directory outside `.ahma/`, ahma **must** warn when it writes plaintext operational logs (which include full tool-call transcripts) into a git working tree not covered by an ignore rule, naming the remedy (`ahma logs gitignore`, or `--log-dir` / `[logging] dir` to move them out of the tree).

### R9: Safe Live Log Monitoring (`--log-monitor`)

- **R9.1**: With `--log-monitor` (or `[logging] log_monitor = true`), ahma gives read-only access to specific log files outside the sandbox scope without widening the sandbox contract.
- **R9.2**: **Mechanism**: at initialization only, ahma scans `.ahma/logs/` of every sandbox root for `*.log` symlinks and resolves their targets. A target inside a scope is accepted; one outside is accepted only if that exact file has a `log-target` grant for this workspace in the ledger (R-PERM.1), and is otherwise blocked with a warning naming `logs_approve`. `logs_approve` asks a human through the permission ladder (R-PERM.3, `GrantReason::LogTarget`) after refusing any target on the hard denylist (R-PERM.4.3). It offers deny, read-only for this session, or read-only always; either grant applies to the running session at once, and only an `always` answer writes a `log-target` row, through the one audited write path (`permissions::persist_log_target_as`). With nobody to ask nothing is recorded. Pressing `a` on a blocked log in `ahma tui` is the human's own approval and records it directly; no MCP tool can reach that path.
- **R9.3**: **Enforcement**: accepted targets are added to the sandbox profile as **read-only scopes** on Linux, macOS and Windows.
- **R9.4**: **Abuse prevention**: because symlinks are resolved only at startup, a symlink created later (e.g. to `/etc/passwd`) grants nothing.
- **R9.5**: **LLM-based detection** (`tool_type: livelog`): the `source_command` runs inside the kernel-enforced sandbox scope; the LLM endpoint is an outbound connection from the ahma process, not subject to the subprocess sandbox. Pipeline: §5.5.

---

## 4.6 Web Egress Sandboxing (R-WEB)

### Design rationale

The filesystem sandbox (R5/R6) governs what the agent can read and write locally, not what it can send outward. R-WEB guards agent-driven HTTP from the ahma process against **exfiltration** (a prompt-injected agent sending workspace data to an attacker's server), **SSRF** (the ahma process can reach `localhost`, the home router and cloud metadata at `169.254.169.254`), **DNS rebinding** after approval, and runaway API spend. A web grant trusts an operator with the data you send, so the model is deny-safe and offers three human-only tiers (R-WEB.5) — once, session, always — so a one-off fetch need not become permanent trust. Approval is per domain, not per path (R-WEB.13) or per method; a tool that adds a method shows it in the prompt. Response-content filtering and per-domain rate limiting are out of scope.

### R-WEB.1: Scope

This section governs **tool-level outbound HTTP made by the ahma process itself** — today `fetch_webpage`, and any tool that fetches through the guarded path (R-WEB.14). It does **not** govern subprocess HTTP traffic (the `--restrict-network` proxy, R-WEB.16), the ahma process's own operator-configured MCP/LLM client connections (`base_url`), or inbound connections.

### R-WEB.2: Default policy

- **R-WEB.2.1**: The default policy is `"allow"`: requests to domains in neither `always_allow` nor `never_allow` pass through without prompting.
- **R-WEB.2.2**: `default_policy = "deny"` in `[web]` is **strict mode**: a domain not in the session grant list or `always_allow` holds the request and raises a prompt (R-WEB.6). The setup wizard and documentation **must** recommend `"deny"` for any workspace handling sensitive data, credentials or proprietary code.
- **R-WEB.2.3**: Regardless of `default_policy`, `never_allow` entries **always block** and `block_private_ranges` (R-WEB.3) **always enforces**; neither can be overridden by session grants or `always_allow`.
- **R-WEB.2.4**: Regardless of `default_policy`, `always_allow` entries **always permit** without a prompt.

### R-WEB.3: Private-range blocking (always-on)

- **R-WEB.3.1**: At least these ranges are **always blocked**, whatever the policy, grants or patterns:

  | Range | Description |
  |-------|-------------|
  | `127.0.0.0/8` | IPv4 loopback |
  | `::1/128` | IPv6 loopback |
  | `10.0.0.0/8` | RFC-1918 private |
  | `172.16.0.0/12` | RFC-1918 private |
  | `192.168.0.0/16` | RFC-1918 private |
  | `169.254.0.0/16` | Link-local / cloud metadata (AWS IMDS, GCP metadata server) |
  | `fc00::/7` | IPv6 unique-local |
  | `fe80::/10` | IPv6 link-local |
  | Symbolic names: `localhost` | Resolved before check |

- **R-WEB.3.2**: The check **must** run on the **resolved IP address(es)** at connect time, not on the domain string, so an approved domain whose DNS later points at a private IP is still blocked. A pattern whose literal text is a private IP **must** be rejected at parse time.
- **R-WEB.3.3**: `block_private_ranges = false` in `[web]` disables the check, for development against a local server. It **must** produce a loud startup warning and a persistent TUI banner as prominent as the `--no-sandbox` banner.
- **R-WEB.3.4**: The resolved-IP check is done by the resolver in `ahma_harness_tools::egress_guard` on the guarded fetch path (R-WEB.14); a bare `reqwest::Client` bypasses it, which is why every agent-driven request must use that path.

### R-WEB.4: Domain pattern syntax

Patterns are **domain-only** — no path or query. Scheme and port are optional qualifiers.

```
pattern  = [scheme "://"] domain [":" port]
scheme   = "https" | "http"
domain   = exact | wildcard
exact    = <hostname>        # matches this hostname only
wildcard = "*." <hostname>   # matches any one-level subdomain only
port     = <decimal>
```

| Pattern | Matches | Does NOT match |
|---------|---------|----------------|
| `api.github.com` | `api.github.com` | `github.com`, `raw.github.com`, `deep.api.github.com` |
| `*.github.com` | `api.github.com`, `raw.github.com` | `github.com`, `deep.api.github.com` |
| `github.com` | `github.com` | `api.github.com` (exact only) |
| `https://api.github.com` | `api.github.com` over HTTPS | `api.github.com` over HTTP |
| `api.github.com:8080` | `api.github.com` on port 8080 | port 443 |
| `*` (bare) | — | **Rejected at parse time** |
| `*.com` | — | **Rejected at parse time** (TLD-level wildcard) |

`github.com` does not match `api.github.com` (CSP semantics); to allow a domain and its subdomains add both `github.com` and `*.github.com`. The modal (R-WEB.6) suggests both when the URL is a subdomain with no matching root-domain entry.

- **R-WEB.4.1**: Patterns containing a private IP or `localhost` **must** be rejected at parse time.
- **R-WEB.4.2**: `http://`-scheme entries in `always_allow` **must** produce a parse-time warning and a visible caution in the modal. They are not blocked, but never silently persisted.
- **R-WEB.4.3**: Matching is **case-insensitive** for the hostname (RFC 4343) and **case-sensitive** for the scheme.

### R-WEB.5: Three-tier approval model

Parallel to the filesystem persistent-scope grants (R5.4.4–R5.4.7):

| Tier | Lifetime | Storage | Agent can self-grant? |
|------|---------|---------|----------------------|
| **Allow once** | This request only | None | No — requires human key press |
| **Allow session** | Until server restart | In-process `HashSet` | No — requires human key press |
| **Allow always** | Permanent | `[web] always_allow` in `~/.ahma/settings.toml` | No — settings file is outside sandbox |

- **R-WEB.5.1**: Session grants are cleared when the server exits and are **never** serialized.
- **R-WEB.5.2**: A persistent grant is written **only** to `~/.ahma/settings.toml`, which is kernel-unwritable from inside the sandbox (R5.4.5), so an agent cannot grant itself permanent web access.
- **R-WEB.5.3**: After a persistent grant is written, the server **must** confirm it as R-WEB.12.2 specifies (file path, line, content).
- **R-WEB.5.4**: A session deny suppresses re-prompting for that domain for the rest of the session. There is no interactive "deny always"; permanent blocks use `never_allow` or `ahma web deny <pattern>`.
- **R-WEB.5.5**: An `always_allow` entry added at runtime takes effect immediately in the current session (the policy is hot-reloaded from the updated settings), as human-approved filesystem grants do (R5.4.6, R-PERM.2). Only a human action can write `settings.toml`.

### R-WEB.6: TUI approval modal

When a request is blocked under `default_policy = "deny"` and a TUI is attached:

- **R-WEB.6.1**: The modal **must** show the tool name, the full URL truncated at 256 characters (against prompt injection via crafted URLs), and the domain each tier would approve.
- **R-WEB.6.2**: Key layout:
  ```
  Web Request · allow access?

  Tool:    fetch_webpage
  URL:     https://api.github.com/repos/owner/repo/issues
  Domain:  api.github.com

  [n] Deny (default)    [o] Allow once
  [s] Allow session: api.github.com
  [p] Persist:       api.github.com  →  ~/.ahma/settings.toml
  Enter / Esc = Deny
  ```
- **R-WEB.6.3**: **Enter and Esc must deny**; approving at any tier needs an explicit non-default key (as R5.3.1: Enter never widens).
- **R-WEB.6.4**: For `http://` URLs the `[p]` line **must** carry a caution marker: `[p] Persist (⚠ cleartext HTTP): api.github.com`.
- **R-WEB.6.5**: After `[p]`, the modal shows the file and line added before dismissing.
- **R-WEB.6.6**: The modal is drawn last in the render pass, over all other content.
- **R-WEB.6.7**: With no TUI attached, `elicitation/create` is used (R5.3.1). With neither surface, the request is **denied** and the error gives the exact `ahma web allow <pattern>` command to pre-approve the domain.

### R-WEB.7: Dedup / debounce coordinator

`WebApprovalCoordinator` (parallel to `GrantCoordinator` in `ahma_common::scope_grant`):

- **R-WEB.7.1**: A domain is **asked at most once per session**; later requests get the cached answer.
- **R-WEB.7.2**: Concurrent requests for a domain with a pending prompt are **queued**, not dropped, and inherit its decision.
- **R-WEB.7.3**: A denied domain joins the session deny-list and is denied immediately from then on, preventing prompt storms from an agent that retries.
- **R-WEB.7.4**: First answer wins when a prompt is fanned to several surfaces.

### R-WEB.8: Redirect chain validation

- **R-WEB.8.1**: A 3xx redirect to a **different** host **must** be checked against the policy independently; redirects do not inherit the source's approval.
- **R-WEB.8.2**: `[web] on_redirect_to_new_domain` decides a redirect to another host. `"policy"` (the default): followed only if the live policy (R-WEB.2, with session grants and denies) allows that host, otherwise the request fails with an error naming `ahma web allow <host>`; never prompts. `"block"`: never followed, even to an allowed host; the error names the target and the setting. `"prompt"`: treated exactly like a fresh request to that host, so an allowed host is followed, a denied one refused, and an unknown one goes through the approval flow (R-WEB.5–R-WEB.7) and is followed only if approved. Redirects on the guarded path are followed by hand, one hop at a time, so every hop is decided and re-checked (R-WEB.3, R-WEB.8.4). An unknown value aborts startup (R-CFG6.1).
- **R-WEB.8.3**: Same-host redirects (including HTTP→HTTPS for the same host) are always followed.
- **R-WEB.8.4**: The redirect target's resolved IP is always checked against R-WEB.3.

### R-WEB.9: Audit log

- **R-WEB.9.1**: Every outbound tool request (approved, denied, or passed through in `allow` mode) **must** be written to the session audit log with timestamp, tool, HTTP method, full URL, resolved domain, decision and matched pattern.
- **R-WEB.9.2**: Structured JSONL, in the same session log as filesystem scope-grant events:
  ```json
  {"ts":"2026-06-24T12:00:00Z","kind":"web_request","tool":"fetch_webpage",
   "method":"GET","url":"https://api.github.com/repos/…","domain":"api.github.com",
   "decision":"approved-session","matched_pattern":"api.github.com"}
  ```
- **R-WEB.9.3**: Audit writes are best-effort and **must not** block the request.

### R-WEB.10: CLI management commands

Parallel to `ahma sandbox grant|list|revoke`:

| Command | Effect |
|---------|--------|
| `ahma web allow <pattern>` | Add to `[web] always_allow`; show file path + line added |
| `ahma web deny <pattern>` | Add to `[web] never_allow`; show file path + line added |
| `ahma web list` | Show `default_policy`, `block_private_ranges`, all entries with provenance |
| `ahma web revoke <pattern>` | Remove from `always_allow` or `never_allow`; show file path + line removed |
| `ahma web check <url>` | Dry-run: report what decision the policy would make for this URL |

- **R-WEB.10.1**: `ahma web allow` / `deny` **must** reject invalid patterns (bare `*`, TLD-level wildcards, private IPs) and warn on `http://` patterns.
- **R-WEB.10.2**: Every mutation command **must** print the settings file path and the exact line changed or added.

### R-WEB.11: TOML configuration schema

The `[web]` section of `~/.ahma/settings.toml`:

```toml
[web]
# "allow" (default) or "deny" (strict mode: prompt for unknown domains).
# Recommendation: use "deny" for any workspace handling sensitive data or credentials.
default_policy = "allow"

# Block loopback, RFC-1918, link-local, and cloud-metadata IP ranges.
# Enforced at DNS resolution time (not just pattern matching) to resist DNS rebinding.
# STRONGLY recommended: keep true. Setting false enables SSRF attacks against local services.
block_private_ranges = true

# "policy" (default), "block" or "prompt" (R-WEB.8.2).
on_redirect_to_new_domain = "policy"

# Domains always permitted without a runtime prompt.
# Syntax: exact ("api.github.com"), single-level wildcard ("*.github.com"),
#         scheme-qualified ("https://api.github.com"), port-qualified ("api.github.com:8080").
# Note: "github.com" matches github.com only — NOT api.github.com.
#       Add both "github.com" and "*.github.com" to allow all of GitHub.
always_allow = []

# Domains always blocked regardless of default_policy, always_allow, or session grants.
never_allow = []
```

- **R-WEB.11.1**: `[web]` uses `#[serde(deny_unknown_fields)]`, so a typo is a hard error.
- **R-WEB.11.2**: `ahma settings show` **must** validate every `always_allow` / `never_allow` pattern and reject the config with a clear error if any is invalid.
- **R-WEB.11.3**: The settings file is in the out-of-scope control plane `~/.ahma` (R5.4.8), so no sandboxed tool can read or modify it.

### R-WEB.12: Provenance and file confirmation

- **R-WEB.12.1**: `always_allow` / `never_allow` entries accept optional inline-table provenance; plain strings are also accepted and round-trip as plain strings:
  ```toml
  always_allow = [
    { pattern = "api.github.com", granted_at = "2026-06-24", note = "GitHub API for PR tooling" },
    "*.stackoverflow.com",
  ]
  ```
- **R-WEB.12.2**: A persistent grant made via TUI or CLI **must** confirm with the settings file's absolute path, the zero-based line number, and the full text of the line written:
  ```
  Persisted: ~/.ahma/settings.toml +47
    "api.github.com"
  Takes effect immediately for this session.
  ```

### R-WEB.13: No path-based restrictions (by design)

Path-based approval (`github.com/api/*` allowed, `github.com/login/*` denied) is **not supported**: query strings and request bodies carry data as well as paths do, redirects change the path after approval, and a path allowlist gives false confidence that other paths are blocked. The boundary is the domain operator. Path- or header-level control belongs in an HTTP proxy (the subprocess egress proxy, R-WEB.16).

### R-WEB.14: Implementation map

- **`WebPolicy`**, **`WebPattern`**, **`WebDecision`** (`ahma_common::web_policy`): the `[web]` settings, the validated pattern (R-WEB.4), and the verdict for a URL.
- **`WebApprovalCoordinator`**, **`WebApprovalRequest`**, **`WebApprovalDecision`** (`ahma_common::web_approval`): per-domain prompt dedup (R-WEB.7), session grants and denies; `persist_web_allow` writes an "always" answer.
- **Fetching**: `fetch_webpage` uses `ahma_harness_tools::fetch_webpage_with_redirect_guard`, whose resolver blocks private addresses on every hop (R-WEB.3) and whose `RedirectDomainGuard` decides each redirect to a new host by `on_redirect_to_new_domain` against the live policy (R-WEB.8). Every ahma-originated outbound HTTP request made on an agent's behalf **must** use this path, never a bare `reqwest::Client`.
- The TUI's `draw_web_approval_modal` follows `draw_scope_grant_modal`: drawn last, `[n]` the default, Enter/Esc deny.

### R-WEB.15: Interaction with other approval systems

- **R-WEB.15.1**: Web domain approval is **independent** of tool-level approval (`ahma_core::approvals`): approving `fetch_webpage` approves no domain.
- **R-WEB.15.2**: Web domain approval and filesystem scope grants are independent; the two coordinators are not coupled.
- **R-WEB.15.3**: When one call needs both, the prompts are queued and shown in sequence; each decision is independent.

### R-WEB.16: Subprocess egress sandbox (`--restrict-network` proxy)

> User guide: `docs/network-egress.md`.

Covers HTTP traffic from **sandboxed subprocesses** when `--restrict-network` / `[network] restrict = true` is set; the ahma process itself is governed by R-WEB.1–R-WEB.15. Off by default (R-PERM.5.3 explains why).

- **R-WEB.16.1**: With restriction on, `ahma serve` **must** bind an HTTP proxy to a random localhost port and inject `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY=127.0.0.1,::1,localhost` into the subprocess environment.
- **R-WEB.16.2**: Requests to domains **not** on the effective allowlist (`[network] allow` ∪ enabled profiles' hosts, R-PERM.5.3) **must** get `407 Proxy Authentication Required` (CONNECT / HTTPS) or `403 Forbidden` (plain HTTP), indistinguishable from a real network failure so the agent cannot detect the proxy from error content.
- **R-WEB.16.3**: Allowlist entries use the host-pattern syntax of R-WEB.16.9. An empty effective allowlist means deny all.
- **R-WEB.16.4**: The proxy **must not** decrypt HTTPS (no MITM): CONNECT tunnels are forwarded for approved domains and rejected otherwise.
- **R-WEB.16.5**: The proxy applies the private-range block (R-WEB.3.1) regardless of allowlist.
- **R-WEB.16.6**: QUIC (HTTP/3) is not intercepted by an HTTP proxy. On macOS the Seatbelt rule confining outbound IP to the proxy also stops direct QUIC; on Linux (Landlock filters TCP only) and Windows it does not, so tools that speak HTTP/3 **should** have it disabled in their own configuration.
- **R-WEB.16.7**: `ahma_mcp::egress::EgressGrants` computes the effective allowlist, `EgressAllowlist` holds it, and `HostPattern` is the single matcher. No second matcher may be introduced.
- **R-WEB.16.8** (interactive approval, R-NET): When a subprocess reaches a domain not in `[network] allow`, the proxy **must** raise an MCP `elicitation/create` prompt at the attached peer before denying, offering R-WEB.5's three tiers (`once` / `session` / `always`, persisted to `[network].allow`) plus `deny`. Concurrent connections to an in-flight domain are not double-prompted (R-WEB.5's dedup). With no peer, no elicitation capability, a timeout or a decline, the connection **must** fail exactly as R-WEB.16.2. A denied or unanswerable prompt is cancelled, not left in flight, so a later connection (e.g. once a capable client attaches) may re-ask.
- **R-WEB.16.9** (host matching): Matching **must** be **label-boundary-anchored**, never a substring or suffix test. Both sides are ASCII-lowercased and one trailing root dot is stripped; `crates.io` matches only itself, and `*.crates.io` matches exactly one more non-empty label (not the apex, not `a.b.crates.io`). A suffix test would let `evilcrates.io` match `crates.io`.
  - Non-ASCII hostnames **must** be **rejected**, not folded to punycode: `сrates.io` with a Cyrillic `с` looks identical to the real entry.
  - A malformed entry **must** be dropped with a warning, never coerced into something that matches.
  - "With `restrict = true` and an empty `allow`, all egress is denied" holds only when there are **also** no profile-contributed hosts (R-PERM.5.3); any statement of the deny-all condition **must** name both halves.
- **R-WEB.16.10** (session precedence): A per-session decision from R-WEB.16.8 (grant or deny) **must** be consulted before the static allowlist. A session deny **must** block a domain even if the allowlist covers it, and is answered from the coordinator's in-memory state without a DNS lookup.
- **R-WEB.16.11** (`network_grant` MCP tool): the agent-facing tool for proposing grants to `[network].allow` in `~/.ahma/settings.toml`, with `sandbox_grant`'s two-phase model: preview-only when `confirm: false` (default); a hard denylist refusing blanket `*`, localhost/local domains and private/loopback/cloud-metadata IPs (`169.254.169.254`) even with confirmation. `confirm: true` **must never** persist on its own for any client: when the client declared the MCP `elicitation` capability it prompts there and persists only on explicit human approval; otherwise (the in-process agent, a client with no prompt, a headless harness) it returns the `ahma network allow <host>` instruction for the human to run, and is **not** assumed to have been gated by the client. Human-approved grants apply immediately to the live session and are audit-logged under kind `net-host` with the chosen tier; only `always` is appended to `[network].allow` — `session` (or an out-of-spec `once`, which has no connection to bind to) lasts until the session ends and writes nothing. A timeout or broken prompt is reported as unanswered, never as a decline, and a decline tells the agent not to ask again.

### R-LISTEN: Listening sockets

> User guide: `docs/network-egress.md` ("Listening for connections").

- **R-LISTEN.1**: **Listening is not restricted, and every scope surface says so.** A sandboxed command may listen on any address. A server it starts on every interface (`0.0.0.0`, the default for Next.js, Spring Boot, a Go `:8080` and many others) can be reached by any device on the networks this machine is on. This **must** be disclosed on every R5.4 scope surface (startup, `status`, TUI; R-PERM.5.1), with what to do instead: bind `127.0.0.1`.
- **R-LISTEN.2**: **Why not a grant.** "Loopback only, every interface on request" was specified (0.22.1) and disproved on macOS CI runners before it shipped:
  - `(deny network-bind (local tcp "*:*"))` refuses every TCP bind, but no rule re-allows loopback: `(local tcp "localhost:*")` never matches `127.0.0.1` or `::1`, a literal IP is a syntax error ("host must be * or localhost"), and the `ip`/`tcp4` deny forms refuse nothing.
  - `(deny network-inbound (remote tcp "*:*"))`, with or without a localhost allow, has no effect: a server inside the profile served a LAN address exactly as it served loopback.
  - Landlock filters binds by port only, and Windows has no filter.

  So the only enforceable rule refuses all listening, localhost included, which would break every local test server. A real boundary needs a privileged network filter (a macOS Network Extension or pf anchor, Linux nftables), which is a separate design.

---

## 4.7 Outbound HTTP (R-HTTP)

All outbound HTTP — to model providers, the per-user hub, external MCP servers, GitHub and web
pages — goes through `ahma_common::http_retry`, which owns retry (R-HTTP.1–2) and failure wording
(R-HTTP.3).

- **R-HTTP.1 — Retry with backoff.** Every outbound HTTP request retries transient failures with
  capped exponential backoff and jitter (default: 3 retries, 500 ms doubling to 8 s), through
  `send_with_retry`. A server's `Retry-After` is honoured as a floor, capped at 60 s so a
  confused server cannot park a request indefinitely.
- **R-HTTP.2 — Retry only what is safe, and decide it from types.** A failure is classified once,
  from the `reqwest` error kind or the status code, never from rendered text:
  - *Not delivered* (refused connection, DNS, TLS handshake) and *throttled* (429, 503): always
    retried — the server did nothing, or asked to be asked again.
  - *Interrupted* (timeout, dropped connection, 408/500/502/504): retried only for idempotent
    requests — GETs, `tools/list`, downloads, LLM completions. **Never** for `tools/call`,
    `initialize` or `sampling/createMessage`: a tool may already have run, and each
    `initialize` that arrives creates a session.
  - Any other 4xx, a decode error, or a policy refusal (the `fetch_webpage` SSRF guard): never
    retried.
  - A model on this machine gets no timeout retries: re-sending makes a slow local model start
    reading its prompt again (`ahma_tui` R24.10.8).
  - Whether a failure is transient travels as a typed flag to whoever decides on a further
    retry (the TUI's one automatic chat retry reads `HubRelay::AgentError.transient`).
- **R-HTTP.3 — A failure leads with a plain summary.** What a person reads first names the
  service that is not working and its state, in plain words ("Couldn't reach your local model
  server at localhost:11434."); then, where the call site knows, what to do; then `Details:`
  with the full technical cause chain and the number of attempts. `ServiceError` renders this
  shape and rides inside `anyhow` chains, so every surface — CLI, TUI, MCP tool errors, agent
  events — finds it with `find_service_error` / `user_message` and shows the same thing. Errors
  that are not a service failure render their full chain (`{:#}`), never only the outermost
  context.
- **R-HTTP.4 — Not covered.** Re-opening a dropped SSE stream (stream resumption is its own
  design, R8.6.4), the `--restrict-network` egress proxy (a proxy must not retry on its client's
  behalf), best-effort health probes that already poll, and `xtask` (developer tooling).

## 5. Tool Definition (MTDF Schema)

### 5.1 Basic Structure

```json
{
  "name": "cargo",
  "description": "Rust's build tool and package manager",
  "command": "cargo",
  "enabled": true,
  "timeout_seconds": 600,
  "subcommand": [
    {
      "name": "build",
      "description": "Compile the current package.",
      "options": [
        { "name": "release", "type": "boolean", "description": "Build in release mode" }
      ]
    },
    { "name": "add", "description": "Add dependencies to Cargo.toml" }
  ]
}
```

### 5.2 Key Fields

| Field | Description |
|-------|-------------|
| `command` | Base executable (e.g., `git`, `cargo`) |
| `subcommand` | Array of subcommands; final tool name is `{command}_{name}` |
| `synchronous` | Deprecated (R2.3); omit it — `tools.execution_mode` decides |
| `options` | Command-line flags (e.g., `--release`) |
| `positional_args` | Positional arguments |
| `format: "path"` | **CRITICAL**: Any path argument **must** include this for security validation |

### 5.3 Sequence Tools

Sequence tools chain several commands into one workflow:

```json
{
  "name": "rust_quality_check",
  "description": "Format, lint, test, build",
  "command": "sequence",
  "step_delay_ms": 100,
  "sequence": [
    { "tool": "cargo_fmt", "subcommand": "default", "args": {} },
    { "tool": "cargo_clippy", "subcommand": "clippy", "args": {} },
    { "tool": "cargo_nextest", "subcommand": "nextest_run", "args": {} },
    { "tool": "cargo", "subcommand": "build", "args": {} }
  ]
}
```

### 5.4 Tool Availability Checks

```json
{
  "availability_check": { "command": "which cargo-nextest" },
  "install_instructions": "Install with: cargo install cargo-nextest"
}
```

### 5.5 Livelog Tool Type

`"tool_type": "livelog"` turns a long-running log-streaming command into an LLM-powered monitor. Usage guide: [docs/live-log-monitoring.md](docs/live-log-monitoring.md); a ready-to-use example is [`.ahma/android-logcat.json`](.ahma/android-logcat.json).

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `tool_type` | No | `"command"` | Set to `"livelog"` to activate the LLM pipeline |
| `livelog` | Yes (when `tool_type=livelog`) | — | `LivelogConfig` block (see below) |

**`LivelogConfig` fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `source_command` | Yes | — | Executable to run as the log source (e.g. `"adb"`, `"tail"`, `"ssh"`) |
| `source_args` | No | `[]` | Arguments for the source command |
| `detection_prompt` | Yes | — | Plain-English description of what to look for (passed verbatim to the LLM) |
| `llm_provider` | Yes | — | `LlmProviderConfig` block |
| `chunk_max_lines` | No | `50` | Flush chunk to LLM after this many lines |
| `chunk_max_seconds` | No | `30` | Flush chunk to LLM after this many seconds even if not full |
| `cooldown_seconds` | No | `60` | Minimum seconds between consecutive alerts (prevents alert storms) |
| `llm_timeout_seconds` | No | `30` | Maximum seconds to wait for the LLM to respond |

**`LlmProviderConfig` fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `base_url` | Yes | OpenAI-compatible API base URL (e.g. `http://localhost:11434/v1` for Ollama, `https://api.openai.com/v1`) |
| `model` | Yes | Model identifier (e.g. `llama3.2`, `gpt-4o-mini`) |
| `api_key` | No | Bearer token. Omit for local models |

**Pipeline:**

1. `tools/call` creates an operation and returns its `operation_id` immediately.
2. A background task spawns `source_command source_args` inside the sandbox scope (R9).
3. stdout and stderr lines accumulate into a chunk, sent to the LLM with the `detection_prompt` at `chunk_max_lines` lines or `chunk_max_seconds` seconds, whichever comes first.
4. The LLM answers `"CLEAN"` (case-insensitive) or a brief summary of the issue.
5. On an issue, if the cooldown has elapsed since the last alert, an `Alert` event on the operation is pushed to the client as `notifications/progress`.
6. The pipeline runs until the source exits or the client calls `cancel <operation_id>`.

**Example** (Android logcat via Ollama):

```json
{
    "name": "android-logcat",
    "description": "Monitor Android logs with LLM-powered crash detection.",
    "command": "adb",
    "tool_type": "livelog",
    "enabled": true,
    "livelog": {
        "source_command": "adb",
        "source_args": ["-d", "logcat", "-v", "threadtime"],
        "detection_prompt": "Look for crashes (FATAL EXCEPTION, NullPointerException), ANR errors, or any ERROR/FATAL log line indicating a real problem.",
        "llm_provider": { "base_url": "http://localhost:11434/v1", "model": "llama3.2" },
        "chunk_max_lines": 50,
        "chunk_max_seconds": 30,
        "cooldown_seconds": 60
    }
}
```

**Security:** `llm_provider.base_url` is an outbound call from the ahma process carrying log data; use a localhost endpoint (e.g. Ollama) unless sending logs to an external service is intended.

### 5.6 Extension tool types

Beyond the built-in `tool_type`s, a runtime-registered handler can serve a tool whose
`tool_type` names it (`register_extension_handler` / `get_extension_key`). No handler ships
in the product.

### 5.8 Task Vault

`--task-vault <dir>` runs a session inside a task vault, which sets that session's sandbox scopes. Requirements: [ahma_vault/SPEC.md](ahma_vault/SPEC.md); user guide: [docs/task-vault.md](docs/task-vault.md).

---

## 6. Usage Modes

### 6.1 STDIO Mode (Default)

`ahma serve stdio` is the IDE-facing MCP frontend; it proxies to the per-user hub (R-HUB.1), which runs the session in its own sandboxed worker (R-HUB.4).

```bash
ahma serve stdio
ahma serve stdio --tools python,git,github,fileutils,simplify   # add bundled tool definitions
```

Built-in tools (`BuiltinTool::ALL`) are always available. **Tool loading priority**: when an `.ahma/` directory exists (auto-detected or `--tools-dir`), **all** its definitions are loaded regardless of bundle flags. Bundle flags (`--tools …`) additionally activate definitions compiled into the binary as **fallbacks**; a local definition overrides a bundled one of the same name. With no `.ahma/` and no `--tools-dir`, only bundle-flag tools and built-ins are available.

### 6.2 HTTP Bridge Mode

`ahma serve http` starts an operator-owned Streamable HTTP server, separate from the per-user hub (R-HUB.1); each session gets its own sandboxed worker (R-HUB.4, R10). Transport details: §7.

```bash
ahma serve http                                   # 127.0.0.1:3000
ahma serve http --sandbox-scope /path/to/project  # explicit scope for every session
ahma serve http --port 8080
```

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/mcp` | JSON-RPC requests |
| GET | `/mcp` | SSE stream for notifications |
| GET | `/health` | Health check |
| DELETE | `/mcp` | Terminate session (with `Mcp-Session-Id`) |

### 6.3 CLI Mode

```bash
ahma tool run cargo_build -- --release   # execute a single tool command
```

### 6.4 List Tools Mode

```bash
ahma tool list -- /path/to/ahma serve stdio --tools-dir ./tools
ahma tool list --http http://localhost:3000
ahma tool validate .ahma/
```

---

## 6.5 Installation, Setup, and Uninstall

### R-SETUP: `ahma setup` Wizard

`ahma setup` installs all integrations (MCP server entries, terminal hooks, agent skills, TLS certificates) into the user's AI tool configurations.  Without flags it runs an interactive wizard; with `--auto` it installs everything silently.

**R-SETUP.1 — Hooks and the MCP server are complementary.** Terminal hooks and the ahma MCP server are **designed to run together** and sandbox different command streams: the MCP server provides the named async tools the agent calls explicitly (`run_terminal_command`, `file-tools`, `git`, …) with output capture and monitoring, while terminal hooks transparently sandbox the shell commands the agent runs through its **native** terminal/Bash tool (which never pass through MCP and would otherwise run unsandboxed). In `auto` mode hooks activate precisely **because** an ahma MCP server is configured. A command is wrapped at most once (already-wrapped commands and MCP tool calls are passed through untouched), so there is no double-execution. `ahma hooks status` therefore reports both being active as an informational note (not a warning), with the only tradeoff being a small per-command sandbox cold-start for the hooks path. Dropping hooks is optional and only advisable when the agent never uses its native terminal.

**R-SETUP.2 — Command timeout.** The default tool/command execution timeout is `1800` seconds (`tools.timeout_secs`, 30 minutes). It is configurable via the `--timeout` CLI flag (highest priority) or `tools.timeout_secs` in `settings.toml`, and applies uniformly to MCP tool calls and hook-wrapped shell commands; individual tools may shorten it via `timeout_seconds` in their JSON definition.

### R-UNINSTALL: `ahma uninstall` (Symmetric Teardown)

`ahma uninstall` **mirrors `ahma setup`**: same interactive "what / which platforms" prompt sequence (default: all), same flag surface (`--auto`, `--mcp`, `--hooks`, `--skills`, `--binary`, `--platform`, `--purge`, `--dry-run`).

**Invariants:**
- Only Ahma-managed keys and files are removed; other user content in the same config files is always preserved.
- `~/.ahma` data directory (TLS, prompts, settings, logs) is **never** removed unless `--purge` is explicitly passed.
- On Unix, the binary can self-delete; on Windows, manual instructions are printed instead.
- After uninstall, restart instructions are printed for all affected platforms.

### R-LIFECYCLE: Process Lifetime of Frontends and Operations

How long an `ahma serve stdio` frontend lives, and what its operation ids mean across restarts.
The hub's own lifetime is R-HUB.3; an explicitly started `ahma serve http|unix` has no idle
exit unless given `--idle-timeout`.

#### R-LIFECYCLE.2: Frontend (proxy) Orphan Prevention

The IDE-facing `ahma serve stdio` **frontend** process (which proxies stdin/stdout to the per-user hub, R-HUB.1) MUST self-terminate when its client connection is abandoned, so that editors that repeatedly spawn MCP servers without reaping them cannot accumulate orphaned processes:

1. **Stdin EOF**: when the client closes the pipe, the proxy loop exits.
2. **Parent-death watchdog**: the frontend polls `getppid()`; when it is reparented (parent IDE died) it `process::exit(0)`s within a few seconds. This covers the case where the client is hard-killed without closing stdin. (Unix; the detached hub is deliberately **not** watched, since it outlives its spawner by design.)
3. **Handshake deadline**: if the client never sends its first message (the `initialize` handshake) within `FRONTEND_HANDSHAKE_DEADLINE_SECS` (default `30`; debug builds let tests override it with `AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS`), the connection was spawned-and-abandoned and the frontend `process::exit(0)`s. The deadline is disarmed once the first message is forwarded, so a live but idle session is never killed.

These three mechanisms together bound how long any abandoned `ahma serve stdio` can live; none of them affect a healthy, actively-used session.

#### R-LIFECYCLE.3: The Frontend Outlives Its Backend

Conversely, a frontend whose client is still attached MUST NOT exit because the per-user
process behind it went away — it exited on idle, crashed, was killed, or handed over to a
newer version. Those are the only three exits above; a lost backend is not one of them.

1. Once a handshake has been observed, a closed or unreachable backend puts the frontend in a
   *disconnected* state rather than ending it. Each request that arrives while disconnected
   first runs a reconnect burst (respawning the backend when its endpoint is gone, and
   replaying the cached handshake); if the burst succeeds the request is forwarded, otherwise
   it is answered with a JSON-RPC error saying ahma is restarting and to retry.
2. The outage is disclosed once, as `reconnect_failed` (R8.8.3), and its end as `reconnected`.
3. If the backend cannot be reached or started at all when the frontend starts (for example a
   host sandbox forbids the detached spawn), the frontend serves the session **in-process**
   instead, and says so loudly (R7): ahma works, only unshared.

#### R-LIFECYCLE.4: Operation Ids Survive Their Process Honestly

Operation ids are counters, and counters restart with the process that issues them.

1. Every id carries its process's **generation**: `op_<generation>_<n>[_<details>]`, where
   `<generation>` is four random lowercase letters minted once per process. Without it,
   `op_3_cargo_build` issued after a restart or update could name a *different* operation
   than the one an agent is still awaiting, and `await` would return that one's result.
2. `await`, `status` and `cancel` on an id the process does not know say what happened to it,
   derived from the tag: another generation means ahma restarted or was updated since (or the
   id belongs to another session), so the work most likely finished and the agent should check
   its effect; the current generation means it was evicted from the bounded history; an id
   that is not an operation id is called that.
3. For an id from another process the answer says what ahma **recorded**, when it can. The hub
   writes `<socket stem>.last-exit.json` beside its socket on every exit — its version and
   build, why it went (`upgrade`, `idle`, `drain-timeout`, `socket-removed`, `stopped`,
   `failed`), when, and the operations still running, which that exit interrupted. The answer
   then names how the operation ended from `history.jsonl` (status, exit code, summary, how
   long ago), or that it never finished, instead of "most likely finished"; and when, why and
   from which version to which ahma last restarted. Keyed by the socket because hub and
   worker already agree on it (R-HUB.2), and that keeps a test's record its own (R-HUB.10).

### R-HUB: The Single Per-User Hub

One hub gives every surface — editors, TUIs, hooked commands — one place to meet, with one
lifetime.

- **R-HUB.1 — One hub per user.** Exactly one ahma hub per user hosts **both** the MCP endpoint
  and the observability hub. Every entry point that needs either — an MCP stdio frontend,
  `ahma tui`, a hooked command — rendezvouses on it and none hosts one itself. An explicitly
  started `ahma serve http` / `ahma serve unix` is a separate, operator-owned server and is not
  the hub.
  - It is a **control plane**: it executes nothing itself. Tools run in one kernel-sandboxed
    worker subprocess per MCP session (R5.1, R10.3), because a Landlock ruleset restricts the
    process that applies it, irreversibly — one process cannot hold two workspace scopes.

- **R-HUB.2 — Rendezvous.** A per-user runtime directory (`$XDG_RUNTIME_DIR/ahma`, else
  `~/.ahma`; `%LOCALAPPDATA%\ahma\run` on Windows), created `0700` and verified to be owned by
  the caller with no group or other bits before use. It holds `hub.lock` and `hub.sock`, the
  socket `0600`. There is no machine-global socket. The directory is checked, not merely the
  socket, because a `0600` socket inside a lax directory is still squattable.
  - **The lock is the mutex, on every OS.** `hub.lock` beside the socket (generally
    `<socket>.lock`) is a kernel advisory lock, released when its holder dies. The hub takes it
    before anything else; a loser connects to the winner. Only the holder touches the socket
    file: it removes a stale one before binding and unlinks its own before letting go of the
    lock, so a crash leaves nothing to clean up by probing and two starters cannot unlink each
    other's sockets. A socket that still answers is never removed, even by the holder
    (R-ISO.2): it belongs to a hub too old to take the lock.
  - **One socket carries both halves.** `hub.sock` serves the MCP endpoint (`/mcp`, `/health`)
    as HTTP, and the event stream as an HTTP/1.1 upgrade: `GET /events` with
    `Upgrade: ahma-hub`, after which the connection carries the NDJSON hub protocol. One path,
    set by `--unix-socket-path` or `[http] unix_socket_path`, names the hub for every process —
    the hub, the frontends that start it, the workers that report to it (told it explicitly)
    and the TUI; there is no separate `--hub-socket`. An operator's own `ahma serve unix`
    defaults to `mcp.sock` beside it, never onto it: a server there without the hub's lock
    would squat on the rendezvous.
  - **The socket is `AF_UNIX` on every OS**, Windows 10 1803+ included, through
    `ahma_common::local_socket`, and the stdio proxy speaks Streamable HTTP to it
    (`ahma_http_mcp_client::local_socket_client`). The hub binds no TCP port, and the frontend
    has no HTTP fallback: a probe of the `serve http` port only ever finds some *other* server
    (R-ISO.1).
  - **The ownership check binds the directory ahma chose, not one it was handed.** An
    operator-named `--unix-socket-path` (or `[http] unix_socket_path`) is a deliberate
    placement decision and is honoured; where its directory is writable by others *and* lacks
    the sticky bit that stops them unlinking our socket, that is disclosed rather than refused
    (R7).
  - Nothing else is written to the runtime directory to say who the hub is. `ahma doctor` asks
    the hub itself: `/health` on its socket reports its version and build id. A descriptor
    file outlives a crash, and probing the lock instead could make a starting hub lose it and
    stand down.

- **R-HUB.3 — Lifetime.** The first comer starts it, detached (R-PROC.3), never from a confined
  process or a test binary (R-HUB.12, R-ISO.1).
  - It exits when MCP sessions **and** hub connections have both been zero for
    `[hub] idle_timeout_secs` (3600; 10 under a test harness; `0` never; the legacy table name
    `[daemon]` is still read). Counting only sessions would exit while a TUI sat watching an
    idle project; counting only hub connections would exit mid-build.
  - Idle exit closes its listeners **first**, re-checks emptiness (a connection accepted in
    between re-arms it), unlinks its sockets while it still holds the lock (R-ISO.3), and
    exits. There is one exit path: sessions terminated, history flushed, sockets removed.
  - Idle time is **wall-clock** time, because the monotonic clock stops while a Mac sleeps. A
    clock stepped backwards restarts the window rather than ending it early.
  - It watches its socket. Clients find the hub by that path alone, so once the file is
    removed or replaced — `$XDG_RUNTIME_DIR` cleared at logout, a stray `rm` — nobody can reach
    it. It then **relinquishes** the rendezvous (releases the lock and forgets the path, so the
    next client can start a successor at once and this hub's exit never unlinks that
    successor's socket) and drains without a successor of its own: whoever connects next
    starts one.

- **R-HUB.4 — Sessions and per-session options.** One kernel-sandboxed worker per MCP session,
  owned by the hub. Ending a session never affects another. A client's own options (`--tools`,
  `--sandbox-scope`, `--no-sandbox`, a task vault) travel **with its session** — encoded in the
  MCP URL's query and applied to that session's worker alone, never to the hub or to other
  sessions. The option list is an allowlist and an unknown name is refused, not ignored.
  Settings that govern the hub as a whole — bearer tokens, rate limits, handshake and idle
  timeouts — are deliberately not settable per session.

- **R-HUB.5 — Upgrade by draining.** A **strictly newer** build asks the hub to **drain**: hand
  over to the new build without ending anyone's work. It never tears down sessions that belong
  to other windows.
  - **Strictly newer** means a newer version, or the same version whose file is newer. Each
    side knows its **identity** — version, build id, and the size and mtime of the executable
    it started from — and `/health` says the hub's, so a dirty rebuild counts as newer while two
    installed copies of one version (`target/release/ahma`, `~/.cargo/bin/ahma`) do not replace
    each other's hub. A hub too old to report an identity is compared by version string.
  - The hub **notices an install itself**: every 30 s, and whenever a session arrives, it
    compares the file at its own executable path with the identity it started with, and drains
    when they differ. `cargo install`, brew, `ahma update` and the install scripts only write
    the file. `ahma update` also asks it to drain once the new file is in place, which only
    makes it prompt; it does not stop the hub before installing. A path briefly missing
    mid-install is not yet a replacement.
  - A draining hub **keeps serving**, new sessions included, and says `draining` in `/health`.
  - It pre-spawns its **successor** from its own executable path (which an install has just
    filled with the new build), at its own spawn depth so upgrades never nest. The successor
    waits on the rendezvous lock and binds the moment the old hub lets go; it stands down if a
    hub that is not draining answers first.
  - It goes at the first moment **no work is in flight** — no operation running in a session
    worker and no request unanswered — on two consecutive looks, since a tool call answered a
    moment ago may not have reported its operation yet. Open sessions and subscribers are not
    work: their clients reconnect to the successor when the socket goes.
  - At `[hub] drain_timeout_secs` (3600; `0` waits for as long as the work takes) it ends what
    is left: every session is terminated, which answers each request still in flight with an
    error, and the operations it interrupts are replayed `interrupted` (R-HUB.7). The timeout
    exists because work that never goes quiet would otherwise run the old build forever.
  - After a reconnect lands on a different ahma version, the frontend tells its client
    `notifications/tools/list_changed`, once per change: the client cached `tools/list` from
    the old build and would otherwise keep its schemas until it restarted.
  - A client that finds the hub already draining does not ask again or wait; it proxies and
    **discloses** the skew. An **older** client neither drains a newer hub nor restarts itself:
    it proxies, because the hub runs every session's worker from its own binary, so the stale
    client is served by the newer build.

- **R-HUB.6 — Registration and routing.** An instance registers with its `session_id`,
  `client_pid`, MCP client identity, mode (`stdio` | `hook` | `tui`) and its **committed**
  sandbox scope, re-registering whenever any of them becomes known. The scope is not knowable
  until `roots/list` has been answered and the sandbox committed, and only a committed scope is
  advertised, so a project filter (R24.3) sees every roots-driven instance. The hub keeps one
  instance id per `session_id`, so re-registering is not a departure and an arrival.
  - A decision (tool approval, scope grant, web approval) is routed back to the instance that
    **raised** it. An untargeted request is routed only when exactly one non-hook, non-tui
    session is attached; otherwise the hub refuses rather than guessing.

- **R-HUB.7 — Retention, with bounds.** In memory: ≤ 500 operations per instance (oldest
  *finished* evicted first, running never), ≤ `MAX_TAIL_LINES` output lines per operation,
  ≤ 2000 operations across all instances, and a one-hour window. History is **retained when an
  instance disconnects** — a hook is an instance for the length of one command — and the
  instance stays listed with `ended_epoch_ms` so its operations have a section to belong to.
  - On disk: `history.jsonl` (`0600`) **in the R-HUB.2 runtime directory, beside the sockets**
    — the one directory whose ownership and mode the hub verifies, since the file names every
    command every client ran. One record per operation edge, the output window written once at
    completion, rotated by rename at 8 MiB keeping one predecessor. The last hour is replayed
    at start. A torn final line — the normal result of a crash mid-write — and a record from a
    newer ahma are skipped with a warning, never fatal.
  - An operation still running when its hub went away is replayed `interrupted`, not failed:
    its exit is genuinely unknown. An operation whose start record was never seen is
    reconstructed from its terminal event and flagged `partial`, because the outcome is real
    even when the preamble is gone.

- **R-HUB.8 — Hooks are visible.** A hooked command registers as an instance with
  `mode: "hook"` and streams its operation like any other. It never spawns a hub — that would
  put a process launch in front of a user's command — and it waits at most 300 ms for its
  terminal event to reach the hub before exiting: without the wait the report races process
  teardown, and with an unbounded one a wedged hub would hold up a shell. Registration happens
  after the R5.5.3 consent decision and cannot change it; the unsandboxed fallback is not
  reported.

- **R-HUB.9 — The TUI is a subscriber.** `ahma tui` never binds the hub and never starts a
  server — above all not one scoped to its launch directory, which would become the default
  scope of every editor session that attached afterwards. It subscribes, registers itself as
  `mode: "tui"` for its own `!` commands, and opens its chat session like any other client
  (scope from its own `roots/list`, R5.2.1.1). Quitting sends nothing but EOF.
  - **A `!` command is reported like any other work, and marked as unconfined.** The TUI opens
    a second, outgoing connection under a session id stable for its lifetime, and reports
    `OpStarted` / `OpOutput` / `OpFinished` for every command typed behind `!` — which is what
    puts them in the history file, in a second TUI, and in the view after a restart.
    `OpStarted.unsandboxed` is set on exactly these, and every surface that renders an
    operation **must** say so: the row carries a mark and the detail pane names it.
  - The reporter **must not** start a hub (the subscriber already ensures one) and **must not**
    block the UI: a command runs, and shows its output locally, whether or not the report
    lands.

- **R-HUB.10 — Test isolation.** Every path in R-HUB.2, and the history file, resolves under one
  per-run private location when `spawned_under_test_harness()`, as R-ISO.1 requires.

- **R-HUB.11 — What this deliberately does not do.** The hub holds no scope state of its own:
  the per-session `ScopeLock` is the single commit door (R5.1.1), and no scope decision is held
  for a later session (R5.3.6).

- **R-HUB.12 — The hub and its workers belong to no checkout.** The hub, and every worker it
  spawns, is started in the R-HUB.2 runtime directory — never in the directory the first
  frontend happened to be launched from. A worker's sandbox scope comes from its own client's
  `roots/list` (R5.1), and everything else it would otherwise resolve from its working
  directory **must** follow that scope or a neutral per-process location: the tools directory
  (the client root's `.ahma/` is layered as the *untrusted* overlay after commit; no inherited
  checkout may become the trusted operator tool set of another project), the execution audit
  log and operation output (under the committed scope's `.ahma/logs`), and the process log
  (under `~/.ahma/logs/<namespace>` until then; never `<runtime dir>/.ahma`).
  - A hub is never started from a process that is itself inside a sandbox: it would inherit
    that sandbox for life and serve every session on the machine confined to one checkout (the
    shape R-DOCTOR.7 hunts). A confined ahma serves its own session in-process and says so; the
    next unsandboxed ahma starts the hub. The one exception is a test-isolated process, whose
    hub is its run's private one (R7.6, R-ISO.1).
  - When the runtime directory cannot be entered, the hub starts in the temp directory instead
    of failing with a bare ENOENT.

### R-ISO: Test/Live Endpoint Isolation

The hub socket, the default `serve unix` socket and the history file are per-user singletons;
a test that reaches them can tear down the developer's live session, or pass on the strength of
a live server it borrowed. Isolation is therefore fail-closed, not opt-in per spawn site.

- **R-ISO.1 (fail-closed test detection).** Any ahma process spawned directly or transitively under a test harness MUST resolve private, test-scoped endpoints **and state** instead of the shared ones: the hub socket, the default `serve unix` socket and the history file (R-HUB.10). A test that wrote the developer's history would also read it back into its own assertions. Under a test harness a hub MUST be spawned only from the real `ahma` binary (whose hub is the run's private one), never from a test binary: `current_exe` there is the test harness, so spawning it re-runs the tests, each copy spawning again — a fork bomb. Detection is `ahma_common::test_isolation::spawned_under_test_harness()`: the explicit `AHMA_TEST_ISOLATION` plumbing variable OR the `NEXTEST` variable that `cargo nextest` exports to every test process (inherited by all children), so a spawn site that forgets the explicit variable cannot reach live endpoints. Per-run endpoint names that parent and child processes must agree on use `NEXTEST_RUN_ID` (not the PID). Test harnesses that spawn the binary SHOULD still set `AHMA_TEST_ISOLATION=1` explicitly (plain `cargo test` sets no distinctive variable).
  - **Isolate every endpoint, including the last resort.** A discovery path that isolates some endpoints and not others lets a test whose own server failed reach a live one and report success; the hub therefore has no discovery fallback at all (R-HUB.2).
- **R-ISO.2 (never steal a live socket).** A Unix-socket listener MUST NOT unlink an existing socket file without first probe-connecting it: a successful connection means a live server owns the path and binding MUST fail loudly (naming the conflict and the `--socket-path` remedy); only a refused/absent connection marks the file stale and safe to remove. (The hub probes before removing even when it holds the rendezvous lock; the HTTP bridge's Unix listener must probe too.)
- **R-ISO.3 (remove only what you own).** On shutdown a server MUST remove its socket file only if the path still refers to the socket it bound (device+inode match). The hub meets this with its rendezvous lock instead (R-HUB.2): every would-be owner must take the lock before touching the path, and the hub unlinks before releasing it. If another process has since replaced the path, deleting it would orphan *that* server's live socket.
- **R-ISO.4 (regression tests).** Unit tests MUST pin: harness detection via both variables; refusal to bind over a live socket; stale-socket cleanup; and identity-checked shutdown removal.
- **R-ISO.5 (test-launched servers die with their launcher).** An operator-started `ahma serve http|unix` outlives whoever launched it, by design — except when a test harness launched it (R-ISO.1 detection). Then it arms the parent-death watchdog, so a test run that is killed (a nextest timeout, Ctrl-C) cannot leave its servers running for good. A test's `Drop` guard never runs on SIGKILL; this is the backstop.

### R-SIGN: Binary Code Signing

On macOS / Apple Silicon a linker-ad-hoc-signed `ahma` (the cargo default) is `SIGKILL`ed by the
kernel (`Code Signature Invalid`, `namespace=CODESIGNING, "Invalid Page"`) when its mapped code
pages are invalidated — by a rebuild overwriting the running binary, or by page eviction under
memory pressure, where ad-hoc re-validation fails on fault-in. The on-disk file still verifies;
the client sees only `Connection closed`, and SIGKILL leaves no panic and no flushed log.

- **R-SIGN.1 (macOS — required).** Release binaries MUST be signed with a stable Developer ID identity (hardened runtime, secure timestamp) and notarized, **before** they are packaged, hashed or attested, so `SHA256SUMS` and the provenance attestation describe the signed bytes. The release workflow does this whenever the Apple signing secrets are configured and emits a notice when they are not (open: §11). Locally built/installed binaries SHOULD be re-signed with a stable signature (`codesign --force --sign - --options runtime`) instead of left linker-ad-hoc. On macOS this is a **runtime-stability** requirement, not merely a Gatekeeper/distribution one.
- **R-SIGN.2 (atomic install — all platforms).** `ahma setup` / `update` / install flows MUST install the binary out-of-place (write a new file, then atomic rename) and never overwrite the inode of a running `ahma`. This removes the "rebuild kills the running server" trigger everywhere. They MUST NOT stop a running `ahma` either: the hub hands over to the new build itself (R-HUB.5). The install scripts stage and verify the new binary beside the old one before it takes the path, so a failed verification leaves the previous install untouched. On Windows, which will not replace a running `.exe` but will rename one, the running binary is moved aside to `<name>.old` (or `<name>.<secs>.old` when an earlier one is still running) and the hub removes those on its next start.
- **R-SIGN.3 (Windows — distribution-only).** Windows is **not** expected to share the macOS runtime kill: a running `.exe` is locked against in-place replacement (R-SIGN.2's trigger cannot occur), and Authenticode is validated at image load, not re-validated on page fault. Authenticode signing is still wanted for **distribution trust** (SmartScreen/Defender reputation), not runtime stability. Assumption: no WDAC / Smart App Control policy kills a running page-evicted process; if one is observed, this becomes a runtime requirement like R-SIGN.1.
- **R-SIGN.4 (Linux — not applicable).** The kernel does not validate ELF code-page signatures, and replacing a running binary keeps the original inode mapped, so neither trigger exists. No signing is required for stability or load. (IMA/EVM appraisal is out of scope unless a specific deployment target enables it.)
- **R-SIGN.5 (fail loud — all platforms).** Independent of signing: when the bridge/proxy observes its server peer die by signal, it MUST surface a specific MCP error (naming the likely code-signing / memory-pressure cause and remediation) instead of a bare `Connection closed`, and the logger MUST flush on abnormal exit (signal/panic hook or synchronous writer) so the final lines survive a SIGKILL.

---

## 7. HTTP Bridge & Session Isolation

The Streamable HTTP transport (R8.1–R8.8), session isolation (R10.1–R10.6) and the bridge's
pipeline invariants (RB) are specified in [ahma_http_bridge/SPEC.md](ahma_http_bridge/SPEC.md).
Chat-agent MCP routing (R10.7, R10.8) is in [ahma_core/SPEC.md](ahma_core/SPEC.md).

## 8. Implementation Constraints

### 8.1 Meta-Parameters

These are per-call arguments of the MCP request, never forwarded to the command itself:
`working_directory` (where the command executes), `execution_mode` (sync vs async) and
`timeout_seconds` (operation timeout).

### 8.2 Process Lifetime Hygiene

#### R-PROC: Child Process Lifetime

- **R-PROC.1**: **Child process leaks**: Every `tokio::process::Command` spawn of a child the parent **owns must** set `.kill_on_drop(true)`. By default, dropping a tokio child-process future (e.g. from a timeout) orphans the process, leaving it running in the background. `status()` and `output()` spawn internally, so they are covered by this rule exactly as `spawn()` is.
- **R-PROC.2**: **Owning a child means owning its descendants.** An owned child **must** additionally be spawned as a process-group leader (`process_group(0)`) and torn down with a group kill (`kill(-pgid)` on Unix, the Job Object on Windows), never with `child.kill()` alone. A signal to a single pid reaps the direct child only: killing the `sh` of `sh -c "cargo build"` leaves `cargo` and `rustc` running, detached from any surface that could show or stop them. `kill_on_drop` does **not** cover this — it too signals only the direct child.
- **R-PROC.3**: **Deliberately detached hubs are exempt, and must say so.** A spawn whose entire purpose is to *outlive* its parent — the hub — **must not** set `kill_on_drop`, and uses `process_group(0)` for the opposite reason (to survive the terminal's process group, not to be reaped with it). Such a spawn **must** carry a comment stating that it is intentionally detached, so the exemption is visibly deliberate and not mistaken for an R-PROC.1 violation.
- **R-PROC.4**: **Group-kill is not graceful, and that's accepted.** SIGKILL (the group kill mandated by R-PROC.2) cannot be caught, so a killed child never runs its own signal handlers or cleanup. Git registers removal of `.git/index.lock` against SIGINT/SIGTERM/SIGHUP, not SIGKILL, so a `git` process ahma kills via timeout, cancel, or sandbox denial can leave `.git/index.lock` orphaned, breaking every later git command in that workspace. `kill_process_tree` stays SIGKILL-only because a SIGTERM grace period would add latency to every timeout/cancel across the whole tool surface. The mitigation is detection: `collect_lock_file_suggestions` (`ahma_mcp/src/mcp_service/handlers/await_tool.rs`) scans `.git/` alongside `target`/`node_modules`/`.cargo`/`tmp`/`temp` for stale lock files after an await timeout and surfaces `rm`-style remediation steps, as it does for cargo/npm lock files.

### 8.3 Unified Shell Output

- **R12.1**: All shell commands **must** redirect stderr to stdout (`2>&1`), for the **whole** script. A POSIX shell script is run as one group, `{ script` + newline + `} 2>&1`: appending ` 2>&1` to the text redirected only the last command, was swallowed by a trailing comment, and broke a heredoc whose terminator ended the script (`EOF 2>&1` terminates nothing). fish, PowerShell and cmd keep the plain suffix.
- **R12.2**: AI clients receive single, chronologically ordered stream.

### 8.4 Cancellation Handling

- **R13.1**: Distinguish MCP protocol cancellations from process cancellations.
- **R13.2**: Only cancel actual background operations, not synchronous MCP tool calls (`await`, `status`, `cancel`).

### 8.5 Concurrency Architecture Principles

#### R18: No-Wait State Transitions

- **R18.1**: State transitions **must never require wait loops or polling**. Code that needs to
  "wait for" another component is a design error.
- **R18.2**: Use state machines with explicit transitions. When a state change occurs, notify listeners immediately through channels or callbacks.
- **R18.3**: Anti-pattern:

```rust
// WRONG: Polling for state change
while !session.is_sandbox_ready() {
    sleep(Duration::from_millis(100)).await;
}
```

- **R18.4**: Correct pattern:

```rust
// CORRECT: watch-channel notification (Operation::completion_watch)
let mut rx = op.subscribe_completion(); // tokio::sync::watch::Receiver<bool>
rx.wait_for(|done| *done).await.ok();   // returns immediately if already true
```

#### R19: RAII for Spawned Tasks

- **R19.1**: When spawning async tasks, the caller **must not** return until the spawn is confirmed live.
- **R19.2**: Use barriers or oneshot channels to confirm task startup:

```rust
let (started_tx, started_rx) = oneshot::channel();
tokio::spawn(async move {
    started_tx.send(()).ok();  // Confirm we're running
    // ... do work ...
});
started_rx.await.ok();  // Don't return until spawn is live
```

- **R19.3**: For tasks that manage lifecycle resources (like sandbox configuration), prefer synchronous execution over spawn unless there's a specific reason for concurrent execution.

#### R20: Single Source of Truth for State

- **R20.1**: Every piece of state **must** have exactly one authoritative location.
- **R20.2**: State observed from multiple components uses watch channels (`tokio::sync::watch`)
  or event listeners with guaranteed delivery — never multiple copies kept in sync.

#### R22: Visual Minimalism

- **R22.1**: Public communications, including user messages, error logs, and documentation, **must** minimize the use of icons and emojis.
- **R22.2**: Standard ASCII text **should** be used for all status indications and visual cues.
- **R22.3**: Emojis are **forbidden** in source code logs and terminal output unless explicitly required for a specific standardized protocol.

#### R23: State Machine Standard

Any non-trivial lifecycle — three or more states, or one where an invalid combination of flags
is representable — **must** be an explicit state machine, not ad-hoc `bool`/`Option` fields
mutated in place. This is the mechanism behind R18.2 and R20.

- **R23.1 — Hand-written, no FSM crate.** Ahma does **not** depend on a third-party
  state-machine crate: none fits (transition-table DSLs such as `rust-fsm` model states as unit
  variants, while ours carry data; `statig` is an event-dispatch framework that fights the
  `watch` observability R18/R20 require; compile-time typestate cannot be stored in a field
  shared behind an `Arc`), and each would enlarge a security product's supply-chain surface.
- **R23.2 — Shared building blocks** in `ahma_common::state_machine`:
  - `FsmState` — implemented by every state enum (`name()` for logs/metrics, `is_terminal()`
    for guards).
  - `InvalidTransition` — the typed error a rejected guarded transition returns.
  - `Observable<S>` — a `tokio::sync::watch`-backed single source of truth shared across tasks:
    `current()`/`read()`, `subscribe()`, guarded `modify()`, event-driven `wait_until()`. It is
    the engine behind `sandbox_state::SandboxStateMachine`.
  - `StateMachine<S>` — a `Mutex`-plus-closure wrapper for **local** state not observed across
    tasks (e.g. OAuth `AuthState`).
- **R23.3 — Shape of a machine.** States are a data-carrying `enum`. Transitions are **named,
  guarded methods** on the owning type (`to_active`, `to_failed`, …) that encode their legal
  predecessors and return `Result<_, InvalidTransition>` (or a domain `Result`). Callers never
  mutate the state field directly. A transition out of a terminal state is rejected, not
  silently applied.
- **R23.4 — Observability.** Cross-task lifecycles use `Observable` (or another `watch`-based
  channel) so observers never poll (R18). Do **not** keep a shadow copy of any field the machine
  owns (R20).
- **R23.5 — Tests.** Every machine tests the happy-path sequence and that each illegal
  transition is rejected and leaves state unchanged. Reference implementation:
  `ahma_common::sandbox_state`.
- **R23.6 — Exemptions.** Enums used purely as **classifiers** or **strategy selectors** (e.g.
  `GrantStatus`, `ReduceMode`, `TransportMode`) have no transitions and are exempt.

#### R24, R25: TUI

Specified in [ahma_tui/SPEC.md](ahma_tui/SPEC.md); two rules there bind every surface: wire
types evolve by `#[serde(default)]` fields only (R24.5), and operation identity is computed
where the operation starts and carried on the wire (R24.7).

#### R26: Built-in file tools

The file tools ahma serves itself (`read_file`, `write_file`, `replace_in_file`, `multi_edit`,
`apply_patch`, `list_dir`, `file_search`, `grep_search`, `fetch_webpage`) follow one contract.
User guide: [docs/file-tools.md](docs/file-tools.md). The harness file tools — all of these
except `fetch_webpage`, plus `todo_write` — are withheld from clients that ship native
equivalents (Claude Desktop, Claude Code, Cursor, VS Code: `BuiltinTool::is_harness_file_tool`,
`McpClientType::has_native_file_tools`).

- **R26.1 — Read before change.** Overwriting or editing an existing file **must** be refused
  unless this session read it and its modification time and length are unchanged since; a
  successful write re-records it. New files need no read.
- **R26.2 — An edit names one place.** `old_str` **must** match exactly once unless
  `replace_all`; a miss reports the nearest match (whitespace-insensitive, or the first line's
  position).
- **R26.3 — All or nothing, atomically.** `multi_edit` and `apply_patch` **must** compute every
  change before writing any; each file is replaced via a temp file and rename, keeping
  permissions. A file's CRLF line endings are preserved.
- **R26.4 — Bounded, and says so.** `read_file` (2000 numbered lines, 2000 chars a line, binary
  refused), `grep_search` (200), `file_search` (1000), `fetch_webpage` (50,000 chars) **must**
  state when they cut and how to narrow. Search **must** respect `.gitignore`.
- **R26.5 — Same guard, same audit.** Every path any of them writes, including an `apply_patch`
  move target, passes the exec-config write guard and is audited (R-HANDOFF), and is
  scope-checked including not-yet-existing paths (`..` refused).
- **R26.6 — Other harnesses' vocabulary.** Tool and argument names models are trained on
  (`Edit`, `str_replace`, `apply_patch`, `file_path`, `old_string`…) are mapped to these tools
  by the name/argument healer; a name that is itself a known tool is never remapped.

---

## 9. Requirement index

Where each requirement family is defined. Ids never move between numbers; they move between
files with the code that implements them.

| Ids | Topic | Defined in |
|---|---|---|
| R1–R4 | Tools, operations, execution mode, schema | this file §3 |
| R-CFG | Configuration sources, tiers, retirement of `AHMA_*` | this file §3.5 |
| R5–R7 | Sandbox scope, platform enforcement, nested sandboxes | this file §4 |
| R-HANDOFF, R-PERM, R-DOCTOR | Trust handoff, permissions, doctor | this file §4 |
| R-LOG, R9 | Project logging, safe live-log access | this file §4.5 |
| R-WEB (R-NET: R-WEB.16.8's name for subprocess egress approval) | Web and subprocess egress | this file §4.6 |
| R-HTTP | Outbound HTTP | this file §4.7 |
| R-SETUP, R-UNINSTALL, R-LIFECYCLE, R-HUB, R-ISO, R-SIGN | Install, lifetime, hub, test isolation, signing | this file §6.5 |
| R-PROC, R12, R13, R18–R20, R22, R23, R26 | Process lifetime, shell output, cancellation, concurrency, state machines, file tools | this file §8 |
| R-SK | Agent skills (R-SK6, R-SK7: [AGENTS.md](AGENTS.md)) | this file §10 |
| R8, R10, RB | Streamable HTTP, session isolation, bridge invariants | [ahma_http_bridge/SPEC.md](ahma_http_bridge/SPEC.md) (R10.7–R10.8: [ahma_core/SPEC.md](ahma_core/SPEC.md)) |
| R24, R25 | TUI work view and chat | [ahma_tui/SPEC.md](ahma_tui/SPEC.md) |
| R15, R16, R-TIMEOUT, R-GUARD, R-DOC, R-HELPER, R-HARNESS, R-TEST-PATH | Testing and contribution rules | [AGENTS.md](AGENTS.md) |

Each crate's `SPEC.md` states what that crate must guarantee; start with
[ahma_mcp/SPEC.md](ahma_mcp/SPEC.md) (the engine) and
[ahma_common/SPEC.md](ahma_common/SPEC.md) (the shared contracts).

---

## 10. Agent Skills (R-SK)

Requirements for the agent skill files (`SKILL.md`) bundled with Ahma: machine-readable guides
that help AI coding assistants use Ahma effectively.

### R-SK1 — Canonical skill location

The primary `/ahma` skill MUST be maintained at `skills/ahma/SKILL.md` and symlinked from
`.agents/skills/ahma/SKILL.md`. The symlink must resolve correctly from the workspace root. The
`ahma` skill incorporates all sub-workflows including code complexity analysis (`/ahma simplify`).

### R-SK2 — YAML frontmatter

Every `SKILL.md` MUST have a YAML frontmatter block with:
- `name`: short identifier matching the `/` invocation name
- `description`: rich trigger-phrase description for skill dispatcher routing
- `user-invocable`: `true` if the user can invoke it directly with `/name`

### R-SK3 — Size limit

Skills MUST NOT exceed **500 lines**. Keep content dense: use tables, bullet lists, and code
snippets rather than prose paragraphs. Link to `docs/` for deep dives. Enforced by
`scripts/check-guardrails.sh` (AGENTS.md §4 R-GUARD.6).

### R-SK4 — Required sections (ahma skill)

`skills/ahma/SKILL.md` MUST include these sections (any order):

| Section | Content |
|---------|---------|
| Quick Start | mcp.json setup for VS Code, Cursor, Claude Code |
| Tool Bundles | Table of all bundles, how to activate, when to use |
| Built-in Tools | The always-available tools, with examples |
| Sync and Async Modes | What each mode returns; the operation-id pattern with `await`/`status`/`cancel` |
| Sandbox | Scope rules, `--tmp`, nested sandbox note |
| Key CLI Flags and Settings | Quick-reference table; the few live env vars; link to the full reference |
| CLI Reference | `serve`/`tool` subcommand synopsis |
| Troubleshooting | Common errors and fixes |

Keeping the skill current, validating its symlink, and its division of labour with `AGENTS.md`
are contributor process: AGENTS.md §1 and §3 (R-SK6, R-SK7).

### R-SK8 — Running standard skills

Ahma can also *run* any skill that follows the
[Agent Skills open standard](https://agentskills.io/specification), via `ahma_common::skills`
(discovery/parsing) and the ahma TUI chat dispatch.

- **R-SK8.1 Discovery**: skills are discovered from, in precedence order:
  `<workspace>/.agents/skills/`, `<workspace>/.claude/skills/`, `~/.agents/skills/`,
  `~/.claude/skills/`. The first skill found under a given name shadows later roots (workspace
  beats user-global); roots that resolve to the same directory (symlinks) are scanned once.
- **R-SK8.2 Validation**: `SKILL.md` frontmatter is validated per the standard — required `name`
  (1–64 chars; lowercase alphanumerics and hyphens; no leading/trailing/consecutive hyphens;
  must match the skill directory name) and required non-empty `description` (≤1024 chars).
  Unknown fields and nested maps (`metadata:` etc.) are tolerated and ignored. Invalid skill
  directories MUST be disclosed with the reason (in `/skills` output), never silently hidden.
- **R-SK8.3 User invocation (TUI)**: in the TUI chat, `/<name> [args]` invokes a discovered
  skill. Built-in commands are matched first, so a built-in always shadows a same-named skill.
  `/skill <name> [args]` is the explicit form (a missing name is reported, not treated as an
  unknown command); `/skills` and bare `/skill` list the discovered skills. Discovered skills
  also appear in the `/` command navigator.
- **R-SK8.4 Injection**: the chat pane displays the typed command; the LLM receives the full
  `SKILL.md` instruction body plus the user's arguments — on the invoking turn **and every later
  turn** of the conversation, so the skill stays in effect. Context-size accounting counts the
  injected payload, not the short displayed text.
- **R-SK8.5 `user-invocable` gate**: the `user-invocable` frontmatter extension (R-SK2) gates
  slash invocation. Absent means `true`, so third-party standard skills (which do not know the
  field) remain invocable. `user-invocable: false` skills are listed with a marker but cannot be
  slash-invoked and are not offered in the navigator.

---

## 11. Known gaps

Every requirement not yet met is listed here and nowhere else as a status; the body marks it
`(open: §11)`.

- **Windows filesystem boundary** (R6.3.3, R6.3.9, R-HANDOFF.4): AppContainer spawn isolation holds both ways on `windows-latest` but is disabled: ordinary tools need `NUL` (denied to application packages; fixing it needs an administrator) and the scope's ancestors (traverse and stat denied). Enabling it needs a design for granting both.
- **Linux trust-handoff deny tier** (R6.1.7): prevention is open. Landlock has no deny rule, and per-command user and mount namespaces are blocked for unprivileged users on Ubuntu 24.04 and later. Writes are detected and reported (`handoff_write`).
- **Developer-ID signing and notarization** (R-SIGN.1): wired in the release workflow and activates when the maintainer adds the six Apple secrets (docs/release-signing.md); until then releases are ad-hoc signed. `scripts/install.sh` and `ahma update` still re-sign the installed copy ad hoc, which would discard a Developer ID signature.
- **Explicit hook allow on Cursor and Antigravity** (R5.5.5): their PreToolUse allow contract is unverified, so the shell hook sends a plain `allow`.
