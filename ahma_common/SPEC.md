# ahma_common Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: no workspace crate (foundation layer)
* **Used by**: every other workspace crate

## 1. User Story / Problem Statement

*As a crate anywhere in the ahma workspace, I want one authoritative implementation of each
contract that more than one surface must honour — configuration, permissions, the event
stream, the hub's wire format, egress policy, timeouts — so that no two surfaces can
implement the same rule differently.*

A rule that binds two surfaces lives here (or in the SPEC it cites), never as a copy in each.

## 2. Acceptance Criteria

**Configuration and permissions**
- `config`: the `~/.ahma/settings.toml` schema (`AhmaSettings`), its project-file overlay,
  per-key trust tiers (`settings_tier`), the `~/.ahma/config.toml` LLM provider registry,
  and `${VAR}` interpolation for tool files. Settings resolve once (R-CFG4). Retired
  `AHMA_*` variables go through `warn_retired_env`, which reports only *whether* a variable
  is set, never its value (R-CFG1.2).
- `settings_origin`: per-key provenance of the effective settings (cli / project / user /
  default) and the startup report built from it: one line per non-default setting
  (R-CFG5.2) and a warning per group- or world-writable settings file (R-CFG6.3). Both
  `ahma settings show --origin` and the startup log resolve sources through it (R-CFG5.1).
- `permissions`: the unified permission ledger — filesystem grants, web domains, tool
  approvals — stored under `~/.ahma`, outside every sandbox scope (R-PERM.1, R-PERM.2).
- `approver`: `ApproverSurface`, who approved — the one type every audit-record writer
  takes for its `surface`, rendered to the names the audit file has always held (R-GUI.7).
  Distinct from `hub::Surface`, the interface a session runs on.
- `scope_grant`, `sandbox_state`: the grant coordinator (ask once per path, R-PERM.3) and
  the sandbox lifecycle state machine.
- `grant`: one grant as every consumer sees it, `ResolvedGrant` (folder, access, `Binding`,
  `GrantedBy`, `GrantLife`), built from a saved grant or lease of the settings file or from a
  session grant (`session_grants`); neither store's file format changes. Whether a grant is
  for the session is its `GrantLife`, never its text (R-PERM.4.4), and which sessions it
  applies to is `ResolvedGrant::applies_to`, the sandbox's one workspace rule (R5.4.11).
- `hook_consent`, `net_approval`, `web_approval`, `web_policy`: the terminal-hook fall-open
  consent ledger (R5.5.3), subprocess-egress and `fetch_webpage` session approvals, and the
  web domain policy (R-WEB).
- `doctor`: the read-only checks shared by `ahma doctor` and the TUI's `/doctor`
  (R-DOCTOR); fixes are applied by callers, only with consent.

**Time**
- `timestamp`: the one place timestamps are spelled and the local zone is chosen. Every
  surface (CLI, MCP server, hooks, TUI, vault) goes through it.
  - **R-TIME.1**: An RFC 3339 timestamp ahma writes — to an audit log, a settings file,
    operation JSON — **must** keep the spelling every earlier version wrote: a numeric
    `±HH:MM` offset (UTC is `+00:00`, never `Z`), and a fraction that is absent when zero
    and otherwise exactly 3, 6 or 9 digits. Logs mix lines from both sides of an upgrade
    and are compared as text. Readers **must** accept any RFC 3339 offset, `Z` included.
  - **R-TIME.2**: Local time is `TZ` when it resolves, else the OS zone (`/etc/localtime`;
    on Windows the system zone), else UTC. On Unix an unresolvable `TZ` **must not** turn
    every local time into UTC.

**Operations and the hub**
- `event_dispatcher`: the single `OperationEvent` stream. Ordering invariant: history
  write → watch signal → event (R15.3 in AGENTS.md).
- `op_identity`: one operation identity (title, cwd, command, origin, exit code), computed
  where the operation starts and carried on the wire (R24.7).
- `hub`, `hub_history`: the per-user hub state and its bounded on-disk
  history (R-HUB).
- `exe_identity`: which ahma binary a process runs and the file it started from
  (version, build id, path, size, mtime), so a rebuild or an overwritten binary is
  detectable where a version string is not; "strictly newer" is decided here once (R-HUB.5).
- `session_event`, `keepalive`: session-health events and heartbeat payloads (R8.8).
  Added fields are `#[serde(default)]`, so old and new peers interoperate both ways.
- `mcp_methods`, `mcp_protocol`, `sse`: MCP method names, protocol-version negotiation for
  every first-party HTTP client (R8.3.5), and SSE framing.

**Process and platform**
- `test_isolation`: a process spawned under a test harness (`AHMA_TEST_ISOLATION`, or
  `NEXTEST` inherited from `cargo nextest`) must never resolve the machine-global endpoints,
  so a test cannot tear down a developer's live session (R-ISO.1). This is the single
  permitted read of a cargo-set variable (R-CFG9.2).
- `process_guard`: a spawn-depth ceiling that stops any frontend → bridge → peer recursion
  bug from exhausting the process table.
- `timeouts`: semantic test-timeout categories with platform multipliers; tests never
  hardcode durations (R-TIMEOUT in AGENTS.md).
- `file_uri`, `hostname`, `fs_lock`, `local_tls`, `digest`: `file://` parsing and encoding,
  host naming, a cross-process advisory lock, local TLS certificates, and the one SHA-256
  hex encoder.
- `git`, `seatbelt`: the one reader of a `.git` pointer file (`gitdir: <path>`: git's own grammar for
  grants, and every reading for the exec-config deny rules, R-HANDOFF.2.1),
  and the one declaration of macOS `sandbox_check` with its flags (is a process sandboxed;
  may it register a Mach name).
- `pid_alive`: the one probe for whether a pid is held by a running process, on every
  platform (`kill(pid, 0)` on Unix, `OpenProcess` on Windows). Every grant bounded by its
  owner's life (root SPEC R-PERM.4.4) and the Seatbelt confinement check use it.
- `BUILD_ID`: the git hash of the build, used to detect a stale same-version bridge.

**Shared definitions**
- `shell_words`: how a shell line is read as words, once. `split_strict` fails closed (the
  workspace-queue lane, R2.7.4, and the edit guard, R2.7.8, treat a refusal as "cannot
  tell"); `segments` never fails and names what runs, `$(…)` and backticks included
  (R-PERM.10.1, R-PERM.11, R-ESCAPE.4, R2.7.11); `split_literal` keeps `\` for Windows
  paths. One assignment rule (the shell's: `NAME=value`), one keyword list, one framing
  rule, and two readings of wrappers: `harmless_wrapper_len` (writes nothing itself, never
  `nohup`; decides a lane) and `command_chain` over `RUNS_THE_NEXT` (names the program).
  Where `split_strict` reads a line, `segments` reads the same words.
- `prompts`, `skills`, `simplify_args`, `peer_factory`, `state_machine`: prompt templates,
  Agent Skills discovery (R-SK8), the `ahma simplify` arguments (here so the CLI parser and
  `ahma_simplify` need not depend on each other), the transport-agnostic MCP peer factory,
  and the workspace state-machine convention (R23).
- `harness`: every AI harness ahma recognises, configures or hooks, listed once with its
  names (`label`, `--platform` value, client display name) and capabilities (progress,
  native file tools, elicitation budget, `roots/list`, terminal hook). `ahma_mcp`'s
  MCP-client type, `ahma setup` targets and hook platforms map their variants onto it and
  read those facts from it, so no two of them can name or treat a harness differently.

**Features**
- `otel` (off by default) compiles the OpenTelemetry SDK into `observability`. Without it
  every `observability` function keeps its signature and is a no-op, so dependents never
  need their own `#[cfg(feature = "otel")]`.
- `coverage` builds the `ahma_coverage` helper binary used by `scripts/coverage.sh`.

## 3. Non-Functional Requirements

- **No upward dependencies**: never depends on another workspace crate.
- **Cross-platform parity**: every primitive behaves the same on Linux, macOS and Windows,
  or states the difference.
- **Wire stability**: types that cross a process boundary (events, heartbeats, hub
  records) only gain fields, each `#[serde(default)]` (R24.5).

## 4. Out of Scope

- MCP protocol handling (`ahma_mcp`), HTTP hosting (`ahma_http_bridge`), and sandbox
  enforcement itself (`ahma_mcp::sandbox`). This crate holds shared state and contracts,
  not enforcement mechanisms.
