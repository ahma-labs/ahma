# ahma_common Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: no workspace crate (foundation layer)
* **Used by**: every other workspace crate

## 1. User Story / Problem Statement

*As a crate anywhere in the ahma workspace, I want one authoritative implementation of each
contract that more than one surface must honour — configuration, permissions, the event
stream, the daemon's wire format, egress policy, timeouts — so that no two surfaces can
implement the same rule differently.*

A rule that binds two surfaces lives here (or in the SPEC it cites), never as a copy in each.

## 2. Acceptance Criteria

**Configuration and permissions**
- `config`: the `~/.ahma/settings.toml` schema (`AhmaSettings`), its project-file overlay,
  per-key trust tiers (`settings_tier`), the `~/.ahma/config.toml` LLM provider registry,
  and `${VAR}` interpolation for tool files. Settings resolve once (R-CFG4). Retired
  `AHMA_*` variables go through `warn_retired_env`, which reports only *whether* a variable
  is set, never its value (R-CFG1.2).
- `permissions`: the unified permission ledger — filesystem grants, web domains, tool
  approvals — stored under `~/.ahma`, outside every sandbox scope (R-PERM.1, R-PERM.2).
- `workspace_scope`, `scope_decision`, `scope_grant`, `sandbox_state`, `elicitation`: scope
  ownership with a single commit point (R5.1.1), downgrade classification and the
  elicitation coordinator (R5.3), and the sandbox lifecycle state machine.
- `hook_consent`, `net_approval`, `web_approval`, `web_policy`: the terminal-hook fall-open
  consent ledger (R5.5.3), subprocess-egress and `fetch_webpage` session approvals, and the
  web domain policy (R-WEB).
- `doctor`: the read-only checks shared by `ahma doctor` and the TUI's `/doctor`
  (R-DOCTOR); fixes are applied by callers, only with consent.

**Operations and the daemon**
- `event_dispatcher`: the single `OperationEvent` stream. Ordering invariant: history
  write → watch signal → event (R15.3 in AGENTS.md).
- `op_identity`: one operation identity (title, cwd, command, origin, exit code), computed
  where the operation starts and carried on the wire (R24.7).
- `daemon_hub`, `daemon_endpoint`, `daemon_history`: the per-user daemon's hub state, its
  Windows rendezvous, and its bounded on-disk history (R-DAEMON).
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
- `BUILD_ID`: the git hash of the build, used to detect a stale same-version bridge.

**Shared definitions**
- `prompts`, `skills`, `simplify_args`, `peer_factory`, `state_machine`: prompt templates,
  Agent Skills discovery (R-SK8), the `ahma simplify` arguments (here so the CLI parser and
  `ahma_simplify` need not depend on each other), the transport-agnostic MCP peer factory,
  and the workspace state-machine convention (R23).

**Features**
- `otel` (off by default) compiles the OpenTelemetry SDK into `observability`. Without it
  every `observability` function keeps its signature and is a no-op, so dependents never
  need their own `#[cfg(feature = "otel")]`.
- `coverage` builds the `ahma_coverage` helper binary used by `scripts/coverage.sh`.

## 3. Non-Functional Requirements

- **No upward dependencies**: never depends on another workspace crate.
- **Cross-platform parity**: every primitive behaves the same on Linux, macOS and Windows,
  or states the difference.
- **Wire stability**: types that cross a process boundary (events, heartbeats, daemon
  records) only gain fields, each `#[serde(default)]` (R24.5).

## 4. Out of Scope

- MCP protocol handling (`ahma_mcp`), HTTP hosting (`ahma_http_bridge`), and sandbox
  enforcement itself (`ahma_mcp::sandbox`). This crate holds shared state and contracts,
  not enforcement mechanisms.
