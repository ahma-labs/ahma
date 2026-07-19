# Ahma Requirements

> **For AI Assistants:** This is the **single source of truth** for the project. Always read this before making changes. Update this file when requirements change, bugs are discovered, or implementation status changes.

## Quick Status

| Component | Status | Notes |
|-----------|--------|-------|
| Core Tool Execution | tests-pass | `ahma` adapter executes CLI tools via MTDF JSON |
| Async-First Operations | tests-pass | Operations return `id`, push results via MCP notifications |
| Shell Pool | removed | Dead prewarmed pool removed — commands spawn directly (~6ms median measured by `latency_guard_test`); `shell_pool` module retains platform shell selection + command timeout config |
| Unified Operation Event Stream | tests-pass | Single `OperationEvent` stream (`ahma_common::event_dispatcher`); `OperationMonitor` is the sole lifecycle emitter; subscribers: MCP progress push, daemon hub, vault audit, TUI |
| Output Spill Files | tests-pass | Complete per-operation output at `<log dir>/operations/<id>.log`; advertised as `output_file` in results; retention-cleaned |
| Small-Model Context Harness | tests-pass | `ahma tui` budgets tool results + trims conversation for limited-context local models; `--context-length`, `--small-model-harness`/`--no-small-model-harness` |
| Feature-Gated Incubating Crates | tests-pass | vault/cluster/simplify/decompose/worker/renewal behind non-default cargo features; graceful `feature_not_compiled` CLI errors |
| Latency Regression Guards | tests-pass | Ignored benchmarks guard end-to-end dispatch latency and per-line streaming cost (`latency_guard_test`) |
| Linux Sandbox (Landlock) | tests-pass | Kernel-level FS sandboxing on Linux 5.13+ |
| macOS Sandbox (Seatbelt) | tests-pass | Kernel-level FS sandboxing via `sandbox-exec` |
| Nested Sandbox Detection | tests-pass | Detects Cursor/VS Code/Docker outer sandboxes; hooks defer to host, MCP stays authoritative, active sandbox always disclosed (R7) |
| Windows Runtime (PowerShell) | in-progress | Built-in PowerShell (5.1+) runtime; cross-platform path security + file URI; parity tests green |
| Windows Sandbox backend | in-progress | Job Object enforcement done; AppContainer spawn isolation pending Windows CI proof |
| Windows Pre-built Releases | in-progress | `x86_64-pc-windows-msvc`; `.zip` CI artifacts; `install.ps1` |
| STDIO Mode | tests-pass | Direct MCP server over stdio for IDE integration |
| HTTP Bridge Mode | tests-pass | HTTP/SSE proxy for web clients |
| HTTP Streaming (Streamable HTTP) | tests-pass | POST SSE with event IDs, event history, Last-Event-Id replay, full multiplexing |
| HTTP/3 (QUIC) Client Preference | tests-pass | All HTTP clients prefer HTTP/3 (QUIC) when server supports it; transparent fallback to HTTP/2 and HTTP/1.1 |
| Session Isolation (HTTP) | tests-pass | Per-workspace sandbox scope (R5.1), shared by attached sessions, sourced via MCP `roots/list` |
| Built-in `status` Tool | tests-pass | Non-blocking progress check for async operations |
| Built-in `await` Tool | tests-pass | Blocking wait for operation completion |
| Built-in `cancel` Tool | tests-pass | Cancel running operations |
| Built-in `run_terminal_command` | tests-pass | Execute arbitrary shell commands within sandbox |
| Batteries-Included Tools | tests-pass | Built-in MTDF setups activated via CLI flags (e.g. `--python`, `--git`) |
| MTDF Schema Validation | tests-pass | JSON schema validation at startup |
| Sequence Tools | tests-pass | Chain multiple commands into workflows |
| Tool Hot-Reload | tests-pass | Opt-in `--hot-reload-tools` watches `tools/` directory and reloads on changes |
| MCP Progress Push | tests-pass | Event-stream subscriber pushes `notifications/progress` per registered operation (replaces legacy callback chain) |
| HTTP MCP Client | tests-pass | Connect to external HTTP MCP servers |
| OAuth 2.0 + PKCE | tests-pass | Authentication for HTTP MCP servers |
| `ahma --validate` | tests-pass | Validate tool configs against MTDF schema |
| `generate-tool-schema` CLI | tests-pass | Generate MTDF JSON schema |
| Graceful Shutdown | tests-pass | 10-second grace period for operation completion |
| Unified Shell Output | tests-pass | stderr redirected to stdout (`2>&1`) |
| Logging (File + Stderr) | tests-pass | Daily rolling logs, `--log-to-stderr` for debug |
| Live Log Monitoring (LLM) | tests-pass | `tool_type: livelog` routes to LLM analysis pipeline; `ahma_llm_monitor` crate; OpenAI-compatible providers |
| TUI Dashboard | tests-pass | Terminal user interface for operation monitoring and approvals |
| Live Task Tree (R24) | tests-pass | Project-scoped caller → subtask tree, current at TUI startup via hub replay with true timestamps; accordion drill-in to live/historic output; client identity via reconnect-to-relabel; operation identity (title/cwd/command/origin/exit_code) computed server-side and carried on the wire (R24.7) |
| Local Cluster Scheduler | tests-pass | mDNS discovery and signed task dispatch to remote worker peers |
| Configuration Standard (R-CFG) | in-progress | Flag/settings-file configuration with trust tiers; `AHMA_*` env vars retired as a config source (§3.5). Done: Security-tier `AHMA_*` retirement (warn-and-ignore, R-CFG1.2/R-CFG7.1), settings-file/`--no-settings` resolution, and settings provenance (`ahma settings show --origin`, R-CFG5.1). Pending: project-tier settings file (R-CFG3) |
| Unified Permissions (R-PERM) | tests-pass | One ledger under `~/.ahma` (fs scopes, web domains, tool approvals; legacy `approvals.json` migrated); question ladder (harness elicitation → TUI modal → fail-closed with paste-able remediation); sandbox profiles replace the hard-coded toolchain carve-outs; hooks enabled per client. User guide: `docs/permissions.md` |
| `ahma cluster remove` | tests-pass | Subcommand to remove worker peers from peers configuration |
| `ahma setup` / `ahma uninstall` | tests-pass | Interactive wizard installs / removes MCP entries, hooks, skills, binary; symmetric teardown leaves other user config intact |
| Auto-spawned Bridge Lifecycle | tests-pass | Bridges started by `ahma serve stdio` or `ahma tui` self-terminate after `--idle-timeout` seconds with no connected client; explicitly-started `ahma serve http/unix` remain persistent by default |
| Binary Code Signing (R-SIGN) | in-progress | macOS ad-hoc binary gets `SIGKILL (Code Signature Invalid)` under heavy-build memory pressure / in-place rebuild → opaque `Connection closed`. Done: atomic out-of-place install + local re-sign in `ahma update` (R-SIGN.2, R-SIGN.1-local); signal-death classification surfaced in the client's JSON-RPC error + panic log-flush (R-SIGN.5). Pending: Developer-ID release signing (R-SIGN.1, blocked on Apple Developer credentials), Windows WDAC/SAC verify (R-SIGN.3) |

---

## Removed: orphaned incubating crates (`ahma_decompose`, `ahma_worker`, `ahma_renewal`)

These three AGPL-3.0-or-later crates were removed from the workspace because nothing in the
shipped product invoked them:

- **`ahma_renewal`** — renewal contract for long-running tasks. Had zero dependents and no SPEC.
- **`ahma_worker`** — ephemeral worker code synthesis. Declared only as an (unused) dependency of `ahma_cluster`.
- **`ahma_decompose`** — local-LLM decompose orchestration. Declared only as an (unused) dependency of `ahma_cluster`; self-flagged for deprecation in its own `lib.rs`.

The related `tool_type: decompose`/`worker` handler stubs inside `ahma_mcp` and the
`.ahma/decompose.json` example are tracked separately. The sources remain in git history if
these roadmap features are revived; recover them from the commit that deleted the crate
directories.

---

## TODO: Python bindings (former `ahma_py` crate)

The `ahma_py` crate has been removed from the workspace and the source files deleted. Before removal it served as the project's Python bindings (PyO3) to expose `ahma_core` to Python consumers and provided build notes for producing a wheel. Key points captured from the crate's source before deletion:

- Purpose: Python bindings for `ahma_core` via PyO3; intended to publish an `ahma-py` wheel for Jupyter/FastAPI/Streamlit use-cases.
- AGPL separation: planned separate `ahma_py_agpl` distribution for bindings that expose AGPL-licensed crates (e.g., `ahma_vault`, `ahma_decompose`, `ahma_worker`).
- Build hints (from removed crate): use `maturin` to build/develop the wheel; example commands were included in the crate docs.
- Implementation notes: some APIs were stubs that returned errors when AGPL dependencies were not present (explicitly instructing the integrator to add the AGPL crate if they accept those terms).

Deferred action items (documented TODO):

1. Re-evaluate packaging and licensing approach for Python bindings (single wheel vs. split permissive/AGPL wheels).
2. If re-introducing bindings: add `pyo3` and `maturin` build guidance to workspace docs, update `workspace.dependencies` or document build-time requirements, and gate AGPL features behind a separate crate/package.
3. Preserve a record of the removed crate in the git history and reference the commit that deleted `ahma_py` for future restoration.

The deleted crate files were under `ahma_py/` prior to removal. Check the git history if you need the original sources.

## 1. Project Overview

**Ahma** (Finnish for "wolverine") is a universal, high-performance **Model Context Protocol (MCP) server** designed to dynamically adapt any command-line tool for use by AI agents. Its purpose is to provide a consistent, powerful, and non-blocking bridge between AI and the vast ecosystem of command-line utilities.

_"Create agents from your command line tools with one JSON file, then watch them complete your work faster with **true multi-threaded tool-use agentic AI workflows**."_

### Technology Stack

| Tech | Version | Purpose |
|------|---------|---------|
| Rust | 2024 Edition (1.93+) | Core language |
| rmcp | 1.5 | MCP protocol implementation |
| Tokio | 1.x | Async runtime |
| Landlock | 0.4.4 | Linux kernel sandboxing |
| reqwest | 0.13.2 (http3) | HTTP client with HTTP/3 (QUIC) preference |
| schemars | 1.2.0 | JSON Schema generation |

---

## 2. Architecture

### 2.1 Core Modules

| Module | Purpose |
|--------|---------|
| `adapter` | Primary engine for executing external CLI tools (sync/async) |
| `mcp_service` | Implements `rmcp::ServerHandler` - handles `tools/list`, `tools/call`, etc. |
| `operation_monitor` | Tracks background operations (progress, timeout, cancellation) |
| `shell_pool` | Platform shell selection (bash/PowerShell 5.1+) and default command timeout config |
| `sandbox` | Kernel-level sandboxing (Landlock on Linux, Seatbelt on macOS) |
| `config` | MTDF (Multi-Tool Definition Format) configuration models |
| `ahma_common::event_dispatcher` | Unified `OperationEvent` broadcast stream (Started/OutputLine/Progress/Alert/terminal) |
| `mcp_service::progress_push` | Event-stream subscriber that pushes `notifications/progress` to the registered MCP client |
| `adapter::spill` | Complete per-operation output spill files (queryable with file tools) |
| `path_security` | Path validation for sandbox enforcement |

### 2.2 Built-in Internal Tools

These tools are always available regardless of JSON configuration:

| Tool | Description |
|------|-------------|
| `status` | Non-blocking progress check for async operations |
| `await` | Blocking wait for operation completion (use sparingly) |
| `cancel` | Cancel running operations |
| `run_terminal_command` | Execute arbitrary shell commands within sandbox scope (promoted from file-based to internal) |

**Note**: These internal tools are hardcoded into the `AhmaMcpService` and are guaranteed to be available even when no `.ahma` directory exists or when all external tool configurations fail to load.

### 2.3 Async-First Architecture

```text
┌─────────────────┐         ┌──────────────────┐
│  AI Agent (IDE) │ ──MCP─▶ │  AhmaMcpService  │
└─────────────────┘         └────────┬─────────┘
                                     │
                    ┌────────────────┼────────────────┐
                    ▼                ▼                ▼
            ┌───────────┐    ┌───────────────┐  ┌─────────┐
            │  Adapter  │    │ OperationMon. │  │ Sandbox │
            └─────┬─────┘    └───────────────┘  └─────────┘
                  │
                  ▼
            ┌───────────────┐
            │ Direct spawn /│ ──▶ Sandboxed bash/PowerShell processes
            │ PTY sessions  │     (persistent sessions via `session_id`)
            └───────────────┘
```

**Workflow:**

1. AI invokes tool → Server immediately returns `id`
2. Command executes in background (direct sandboxed spawn; output streamed line-by-line)
3. Every state transition is emitted on the unified operation event stream; the
   MCP progress-push subscriber forwards events for registered operations as
   `notifications/progress`
4. AI processes the notification when it arrives, or retrieves the stored
   result via `await`/`status` (the store of record)

### 2.3.1 Unified Operation Event Stream

All operation lifecycle data flows through ONE broadcast stream of
`ahma_common::event_dispatcher::OperationEvent` values
(`Started` / `OutputLine` / `Progress` / `Alert` /
`Completed` / `Failed` / `Cancelled` / `TimedOut`):

- **Single emission point**: the `OperationMonitor` emits events at each state
  transition (`add_operation` → `Started`, `append_output_line` →
  `OutputLine`, `append_alert` → `Alert`, terminal `update_status` → exactly
  one terminal event). No other component emits lifecycle events.
- **Ordering invariant**: on terminal transitions the monitor writes
  completion history, signals the completion watch, then emits the terminal
  event — readers woken by the watch always observe complete history.
- **Lagged subscribers**: the broadcast is a live feed, not the store of
  record. A subscriber that lags reconciles from monitor state
  (`status`/`await`); awaiting a result never depends on the broadcast.
- **Subscribers**: MCP progress push (`mcp_service::progress_push`), the
  daemon hub reporter (feeds `ahma tui` and remote dashboards), the vault
  audit subscriber, and tests.
- **Full output**: the bounded `stdout_tail` (100 lines) and result window are
  for token economy; the complete redacted output of every async operation is
  spilled to `<project log dir>/operations/<operation_id>.log` and advertised
  as `output_file` in results, so agents query big outputs with file tools
  instead of re-running commands.

### 2.4 Synchronous Setting Inheritance

```text
┌─────────────────────────────────────────────────────────────────┐
│                    EXECUTION MODE RESOLUTION                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  1. CLI Flag (highest priority)                                  │
│     └── --sync flag forces ALL tools to run synchronously        │
│                                                                  │
│  2. Subcommand Config                                            │
│     └── "synchronous": true/false in subcommand definition       │
│                                                                  │
│  3. Tool Config                                                  │
│     └── "synchronous": true/false at tool level                  │
│                                                                  │
│  4. Default (lowest priority)                                    │
│     └── ASYNC - operations run in background by default          │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## 3. Core Requirements

### R1: Configuration-Driven Tools

- **R1.1**: The system **must** adapt any CLI tool for use as MCP tools based on declarative JSON configuration files.
- **R1.2**: All tool definitions **must** be stored in `.json` files within a `tools/` directory (default: `.ahma/`).
- **R1.2.1**: **Auto-Detection**: When `--tools-dir` is not explicitly provided, the system **must** check for a `.ahma` directory in the current working directory. If found, it **must** be used as the tools directory. If not found, the system **must** log a warning and operate with only the built-in internal tools (`await`, `status`, `run_terminal_command`).
- **R1.2.2**: When `--tools-dir` is explicitly provided via CLI argument, that path **must** take precedence over auto-detection.
- **R1.3**: The system **must not** be recompiled to add, remove, or modify a tool.
- **R1.4**: **Hot-Reloading**: The system **must** watch the `tools/` directory and send `notifications/tools/list_changed` when files change.
- **R1.5**: [REMOVED] Progressive disclosure and the `activate_tools` meta-tool have been removed from the server.
- **R1.5.1**: [REMOVED]
- **R1.5.2**: [REMOVED]
- **R1.5.3**: [REMOVED]
- **R1.5.4**: The `instructions` field in the MCP `initialize` response contains sandbox routing directives instructing the model to use `run_terminal_command` for all command execution.
- **R1.5.5**: [REMOVED]
- **R1.5.6**: [REMOVED]

### R2: Async-First Architecture

- **R2.1**: Operations **must** execute asynchronously by default, returning an `id` immediately.
- **R2.2**: On completion, the system **must** store results reliably in `OperationMonitor` (pull channel) and **should** push a best-effort MCP progress notification. Clients rely on the `await` tool for guaranteed result delivery; the push notification is an optimistic shortcut to avoid a round-trip.
- **R2.3**: **Static Synchronous Flag (DEPRECATED)**: The static `"synchronous": true/false` configuration in tool and subcommand JSON definitions is deprecated. Code calling tools should not rely on static config.
- **R2.4**: **Dynamic Resolution**: Execution mode (blocking/synchronous vs non-blocking/asynchronous) is resolved dynamically per-invocation using the `blocking` boolean parameter in the MCP `tools/call` arguments. If not specified, tool execution defaults to asynchronous.

### R3: Performance

- **R3.1**: Command dispatch **must** stay low-latency via direct sandboxed spawns (~6ms median, guarded by the `latency_guard_test` benchmarks). The former prewarmed shell pool was removed as dead code.
- **R3.2**: Persistent shell sessions (opt-in via `session_id`) are tracked per session and automatically cleaned up on shutdown.

### R4: JSON Schema Validation

- **R4.1**: All tool configurations **must** be validated against the MTDF schema at server startup.
- **R4.2**: Invalid configurations **must** be rejected with clear error messages.
- **R4.3**: Schema supports: `string`, `boolean`, `integer`, `array`, required fields, and `"format": "path"` for security.

---

## 3.5 Configuration Standard (R-CFG)

Server configuration (everything except MTDF tool definitions) **must** be deterministic, inspectable, and tamper-resistant. Environment variables are ambient, persistent state: they leak across sessions, are settable by any process sharing the user's environment, and are invisible at the invocation site. They are therefore being removed as a configuration source. This section is the single source of truth for configuration resolution; where older sections (R5.3, R21.4, `docs/environment-variables.md`) conflict, R-CFG wins.

### R-CFG1: Configuration Sources and Precedence

- **R-CFG1.1**: There are exactly four configuration sources, resolved highest-precedence first:
  1. **CLI flags** — including flags passed via the `args` array in an IDE's `mcp.json`. This is the canonical way to configure ahma per-project from an MCP client.
  2. **Project settings** — `<workspace>/.ahma/settings.toml` (Preference-tier keys only, see R-CFG2).
  3. **User settings** — `~/.ahma/settings.toml` (or `--settings-path <file>`).
  4. **Compiled-in defaults**.
- **R-CFG1.2**: `AHMA_*` environment variables are **not** a configuration source. During the migration window (R-CFG7) a set `AHMA_*` variable produces a startup `warn` naming the replacement flag/key; after the window it is ignored with the same warning. Security-tier variables (R-CFG2.1) are ignored **immediately** — there is no migration honoring for them.
- **R-CFG1.3**: The only environment variables production code may read are: (a) platform/ecosystem standards (`HOME`, `PATH`, `RUST_LOG`, `NO_COLOR`, `TERM`, `XDG_*`, `APPDATA`, `USERPROFILE`, `OTEL_*`, `TRACEPARENT`), and (b) **internal plumbing** variables used for parent→child process communication (e.g. `AHMA_MCP_ARGS` bridge→subprocess). Internal plumbing variables **must** be listed in a single table in `docs/environment-variables.md` marked `INTERNAL`, **must** be set only by ahma itself, and **must never** widen security scope relative to the parent's resolved configuration.
- **R-CFG1.4**: Every boolean setting **must** be expressible as on *and* off at every source level (`--x` / `--no-x` flag pairs; `Option<bool>` settings keys). OR-combining sources (where any source can enable but none can disable) is forbidden — a higher-precedence source **must** be able to turn a lower-precedence setting off.

### R-CFG2: Trust Tiers

- **R-CFG2.1**: Every setting is classified into one of two tiers:
  - **Security tier (S)**: anything that weakens or shapes the security boundary — sandbox disable/defer, sandbox scopes, working dirs, temp access, package-cache write, task vault, auth token and token path, rate limits, TLS directory, session isolation, update signature verification.
  - **Preference tier (P)**: everything else — timeouts, tool bundles, tools dir, hot reload, logging, token minimization, instance label, transport tuning.
- **R-CFG2.2**: Security-tier settings **must not** be honored from the project settings file. A cloned repository must not be able to weaken the sandbox that is about to contain it (the `.vscode/tasks.json` attack class). Security-tier keys found in `<workspace>/.ahma/settings.toml` **must** be ignored and reported at `warn` with the key names.
- **R-CFG2.3**: The two most dangerous switches — disabling the sandbox entirely and skipping update signature verification — **must** be CLI-flag-only (`--no-sandbox`, `--insecure-skip-verify`). They may not be set from any settings file, so that they are always visible at the invocation site (process listing, `mcp.json` args) and never persist invisibly.
- **R-CFG2.4**: Nested-sandbox auto-detection (R7) remains the only non-CLI path to a disabled internal sandbox, and **must** log why it triggered.

### R-CFG3: Project Settings File

- **R-CFG3.1**: `<workspace>/.ahma/settings.toml` is loaded when the tools directory auto-detection (R1.2.1) or `--tools-dir` identifies a `.ahma` directory. Same schema as user settings; Security-tier keys rejected per R-CFG2.2.
- **R-CFG3.2**: Merge semantics are per-key scalar override (project over user). List-valued keys replace, never concatenate, so the effective value is always attributable to one source.
- **R-CFG3.3**: `--no-settings` disables **both** settings files for the invocation.

### R-CFG4: Resolve Once, Then Immutable

- **R-CFG4.1**: All configuration **must** be resolved exactly once at startup into an immutable resolved-config structure passed down by constructor argument (extends R21.4). No production code may read configuration (env, settings files) after startup; runtime re-reads are a tamper channel.
- **R-CFG4.2**: Only the configuration-resolution module may call `std::env::var*` for `AHMA_*` names. This **must** be enforced by a CI check (grep test or clippy `disallowed-methods`) with an explicit allowlist for R-CFG1.3 reads.
- **R-CFG4.3**: The sandbox scope derived from resolved configuration remains subject to R5.1: set once, never mutated.

### R-CFG5: Provenance and Observability

- **R-CFG5.1**: `ahma settings show --origin` **must** print every effective setting with its value, source (`cli` / `project` / `user` / `default`), and for file sources the file path.
- **R-CFG5.2**: At startup the server **must** log one `info` line per setting whose effective value differs from the compiled-in default, including its source. Security-tier deviations **must** log at `warn`.
- **R-CFG5.3**: The documented precedence and the implemented precedence **must** be the same and **must** be covered by a matrix test (every source pair, at least one Preference and one Security key).

### R-CFG6: Strict Parsing (Fail Closed)

- **R-CFG6.1**: A settings file that exists **and can be read** but fails to parse **must** abort startup with a clear error. Silently falling back to defaults is forbidden — a tampered or corrupted file must not silently change behavior. This applies only to a *read-but-unparseable* file: a settings file that cannot be *read* at all — missing, or permission-denied because it lives in the out-of-scope control-plane directory `~/.ahma` (R5.4.8) — is **not** a parse failure and **must** fall back to compiled-in defaults (with a `warn` for the permission-denied case), never abort.
- **R-CFG6.2**: Unknown keys in the `[sandbox]` and `[auth]` tables **must** abort startup (a typo in a security key must not be silently ignored). Unknown keys elsewhere **must** produce a `warn` listing each key (forward compatibility).
- **R-CFG6.3**: On Unix, a settings file that is group- or world-writable **should** produce a startup `warn`.

### R-CFG7: Migration Schedule

- **R-CFG7.1**: Next minor release: project settings file, trust tiers, `--origin`, strict parsing, flag pairs; Security-tier `AHMA_*` variables (`AHMA_DISABLE_SANDBOX`, `AHMA_SANDBOX_SCOPE`, `AHMA_SANDBOX_DEFER`, `AHMA_WORKING_DIRS`, `AHMA_TMP_ACCESS`, `AHMA_DISABLE_TEMP`, `AHMA_NO_PACKAGE_CACHE_WRITE`, `AHMA_TASK_VAULT`, `AHMA_REQUIRE_TOKEN`, `AHMA_REQUIRE_TOKEN_PATH`, `AHMA_TLS_DIR`, `AHMA_INSECURE_SKIP_VERIFY`) ignored with `warn`. Preference-tier variables demoted below settings files and warned.
- **R-CFG7.2**: The following minor release: all remaining `AHMA_*` configuration variables ignored. Only R-CFG1.3 allowlisted reads survive.
- **R-CFG7.3**: `docs/environment-variables.md`, `docs/connection-modes.md` (the Antigravity example currently sets `AHMA_SANDBOX_SCOPE`; it must use `--sandbox-scope` in `args`), `skills/ahma/SKILL.md`, and README **must** be updated in the same PR as each migration step (R-DOC, R-SK6).

### R-CFG8: Required Tests

- **R-CFG8.1**: Red team: with `AHMA_DISABLE_SANDBOX=1` in the environment, the sandbox **must** still be enforced (write outside scope blocked).
- **R-CFG8.2**: Red team: a project `<workspace>/.ahma/settings.toml` containing `sandbox.disable = true`, widened `sandbox.scopes`, or `auth` keys **must not** affect behavior, and the ignored keys **must** appear in startup warnings.
- **R-CFG8.3**: Precedence matrix per R-CFG5.3; parse-failure abort per R-CFG6.1; `--no-x` overriding a settings-file `x = true` per R-CFG1.4.

### R-CFG9: Test-Only Configuration

Production and test code must be separated so that test harness machinery can never alter production security decisions.

- **R-CFG9.1**: Test-only behavior **must** be controlled exclusively through:
  1. `#[cfg(test)]` compile-time gates (preferred — zero runtime cost in production builds).
  2. The `AppConfig.is_server_child` field (set only via the `--server-child` CLI flag or the `AHMA_SERVER_CHILD` internal plumbing variable, which is set by the parent ahma process before spawning a subprocess).
  3. Constructor/function parameters (dependency injection).
- **R-CFG9.2**: Production code **must not** read `NEXTEST`, `CARGO_MANIFEST_DIR`, `CARGO_LLVM_COV`, `CARGO_TARGET_DIR`, or any other cargo-set environment variable. These variables are set by the build/test toolchain and must not influence runtime security decisions (R21.3). The `--server-child` flag is the exclusive mechanism for subprocess detection in production. **Single carve-out (R-ISO.1):** `NEXTEST` / `NEXTEST_RUN_ID` may be read for exactly one purpose — forcing test isolation of endpoint rendezvous (private socket/port instead of the machine-global ones), via `ahma_common::test_isolation` only. This influence is fail-closed by construction: the variable can only *restrict* the process to private endpoints; it can never widen filesystem/network access, restart shared services, or weaken a sandbox decision. (Production already reads `AHMA_TEST_ISOLATION` to the same effect, so this adds no new attacker capability.)
- **R-CFG9.3**: Test helper code inside `#[cfg(test)]` blocks or `test_utils` modules **may** read `AHMA_TEST_BINARY`, `CARGO_TARGET_DIR`, `NEXTEST`, and `CARGO_LLVM_COV` to locate test fixtures and adjust timeouts. These reads are acceptable because they are gated behind compile-time test flags and do not run in production binaries.
- **R-CFG9.4**: The `AHMA_DAEMON_PORT` and `AHMA_DAEMON_SOCK` variables are test-isolation helpers set by `init_test_daemon_isolation()`. They **must** only be read inside `#[cfg(test)]`-gated code paths or in functions that are explicitly documented as test-only. They are INTERNAL plumbing (not user-facing) and **must** be listed in `docs/environment-variables.md` as `INTERNAL/TEST`.

---

## 4. Security - Kernel-Enforced Sandboxing

The sandbox scope defines the root directory boundary. AI has **full read/write access** within the sandbox but **zero read/write access** outside it. Read access outside the sandbox is strictly limited to necessary system binaries across all platforms (Linux, macOS, Windows) and explicitly granted feature scopes (see `--livelog`).

### R5: Sandbox Scope

**Design principles (govern all of R5):** scope is never inferred from spoofable signals; the complete scope is always visible with its provenance; the user is prompted *only* on a genuine security downgrade (never on routine establishment or narrowing); and when the user cannot be asked, ahma fails to a clear, shown default rather than silently widening or running unsandboxed. "No surprises" is the controlling invariant.

#### Scope ownership and lifetime

- **R5.1**: **Per-workspace instance ownership**: A sandbox scope is owned by a **per-workspace server instance**, not by an individual MCP session. All sessions (IDE, TUI, CLI) that attach to a workspace instance **share and gate on** that single scope. The scope is set once per instance and **cannot** be mutated for the life of the instance (the lock-once invariant). Sessions do not carry their own scope.
- **R5.1.1**: **Single commit point**: Every scope commit — derived from `roots/list`, from an explicit flag, from a user elicitation answer, or from the default — **must** go through one atomic compare-and-swap on the instance scope state machine. There is exactly one door to "scope locked"; there is no second path that can set or widen scope after lock. This holds on **both** transports: the HTTP bridge swallows a post-lock `roots/list_changed` (R10.5), and the direct-stdio configuration path (`configure_sandbox_from_roots`, used when a client speaks to `ahma serve stdio` without the bridge) latches the commit once and treats any later `roots/list` / `roots/list_changed` as a tolerated no-op — it does **not** re-request `roots/list` or re-derive scope.

#### Scope source (no spoofable inference)

- **R5.2**: **Scope source precedence**: The locked scope **must** be derived from exactly one of the following, in order; the chosen source **must** be recorded for display (R5.4):
  1. **Explicit** `--sandbox-scope` / `--working-directories` (CLI, user settings file, or task vault) — locked immediately; `roots/list` is **not** requested (R5.2.2).
  2. **Client `roots/list`** — the workspace roots reported by the MCP client.
  3. **User elicitation answer** — only when reaching the scope requires a downgrade decision (R5.3).
  4. **Declared default** `~/sandbox` — used when no client roots arrive and no explicit scope is set (R5.2.3).
- **R5.2.1**: **No marker-based inference**: The server **must not** infer or accept a sandbox scope from the presence of project-marker files (`.git`, `Cargo.toml`, `package.json`, etc.) or any other spoofable, ambient signal in the current working directory. The launch CWD is **not** trusted as a scope on its own; it may only become the scope by being reported through `roots/list` (R5.2 step 2) or named explicitly (step 1). Marker-file "plausible workspace" heuristics are prohibited.
- **R5.2.2**: **Explicit scope is locked and never widened**: When the scope is provided explicitly (R5.2 step 1), the server **must not** request or apply `roots/list` and **must not** widen the scope by any means. This blocks a compromised or buggy client from widening an operator-chosen scope, and gives roots-less clients a stable scope.
- **R5.2.3**: **Default `~/sandbox`, shown loudly**: When the client supplies no usable roots and no explicit scope is configured, the server **must** lock to the configured `sandbox_directory` (default `~/sandbox`, created if absent) and surface it prominently with `source: default` (R5.4). The server **must not** lock to the launch CWD, the system temp directory, the home directory, or a filesystem root. `~/sandbox` is the only implicit fallback.
- **R5.2.4**: **Hard rejections**: The system temp directory, the home directory, and any filesystem root (`/`, `C:\`, UNC root) **must never** be a locked scope, even after symlink resolution (R5.7). These are non-negotiable invariants, not heuristics.
- **R5.2.5**: **Temp dir is opt-in and auxiliary only**: The system temp directory **must not** be in scope except when explicitly enabled via `--tmp` or `[sandbox] tmp_access = true`, in which case it is an auxiliary scope appended after the primary scope, never the sole or primary scope. Enabling `--tmp` is a downgrade (R5.3).

#### Visibility (nothing silent)

- **R5.4**: **Scope is always visible with provenance**: The complete locked scope — every writable root, every read-only root, `--tmp` status, and whether kernel enforcement is on or off — together with its **`source:`** attribution (`explicit` | `roots/list` | `elicited` | `default`) **must** be rendered through one canonical representation and surfaced at: (a) the startup banner and `ahma status`; (b) the persistent TUI scope panel; (c) the `notifications/sandbox/configured` payload (R5.6); and (d) the body of every scope-related error (e.g. the 409 returned before lock). No scope decision may be communicated only via an internal log line.

#### Downgrade prompts (ask only when it matters)

- **R5.3**: **Prompt only on a genuine downgrade**: The server **must** prompt the user **only** when an action would reduce the security posture: widening the writable set beyond the established scope, accepting client roots broader than an already-established scope, disabling kernel enforcement, adding the system temp directory (`--tmp`), or a terminal hook about to run unsandboxed (R5.5.3). First-time scope **establishment** and any **narrowing** are not downgrades: they are applied and shown (R5.4), never prompted. Prompts **must** be rare enough to remain meaningful; routine operation **must not** generate confirmation prompts ("no security theater").
- **R5.3.1**: **Elicitation channel**: Downgrade prompts are delivered via the MCP `elicitation/create` request to every attached session whose client advertised the `elicitation` capability at `initialize`. The prompt **must** show the literal paths affected (never a vague "Allow workspace?"). The default-focused choice **must** be the narrowest/safest option; a *widening* choice **must** require an explicit, non-default selection (Enter alone **must not** widen).
- **R5.3.2**: **Cannot-ask fallback**: When no attached client can be asked (none advertises `elicitation`) and no explicit scope is configured, the server **must not** silently widen or run unsandboxed. It locks to the default `~/sandbox` (R5.2.3) shown loudly; an out-of-band confirmation (TUI, or an explicit CLI command) is the only path to a broader scope.
- **R5.3.3**: **Dual-modal coordination**: When multiple sessions are attached to one workspace instance, a single downgrade decision is fanned to all capable sessions under one `decision_id`. The server (not any client) owns the decision. When any session answers, the server **must** dismiss the prompt on the others via `notifications/cancelled` for that `decision_id`. When a session that holds an open prompt terminates (e.g. the IDE is closed), the server **must** resolve that prompt as cancelled-not-decided and dismiss any twin.
- **R5.3.4**: **Conflict resolution — most-restrictive-wins, then re-confirm**: If two sessions answer the same `decision_id` within a short debounce window, the **narrowest** answer wins regardless of arrival order; a widening answer can never win over a narrowing one by timing. When answers conflicted, the committed (narrowest) scope **must** be shown for re-confirmation before lock; because the narrowest option is always the safe choice, this re-confirmation may auto-accept after a brief visible window.
- **R5.3.5**: **Decision freshness**: A `decision_id` **must** bind to the session generation that created it. An answer that arrives after the handshake deadline (R10) or after the session was recycled **must** be rejected, never applied to a new session.
- **R5.3.6**: **TUI-only establishment is pending**: An answer given in the TUI when no IDE session is live **must** establish the scope as **pending** (shown as such), applied when the next IDE session attaches to the workspace instance; it **must not** silently lock a scope that no live session is using as if it were active. _Status_: the pending-state semantics are implemented and unit-tested at the single commit point (`WorkspaceScope::commit_pending`/`promote_pending`, with `ScopeSource::Pending` reserved for display); wiring the TUI establishment-answer transport through it lands together with the broader `WorkspaceScope`/`ElicitationDecision` integration (R5.3.3–R5.3.5), which is not yet connected to a live server.

#### Subprocess propagation and defaults

- **R5.4.1**: **Scope propagation to subprocesses**: When the stdio MCP server spawns a background bridge or per-session subprocesses, it **must** forward only genuinely explicit `--sandbox-scope` values (never the provisional CWD or temp). The `--sandbox` and `--tmp` boolean flags are forwarded separately so each subprocess derives the default secondary and auxiliary scopes itself.
- **R5.4.2**: **Default install uses `--sandbox`**: The default MCP server configuration installed by `ahma setup` for Cursor, VSCode, Claude, Antigravity, Codex, and LM Studio **must** include `--sandbox` (not `--tmp`). For clients known not to support `roots/list` (e.g. Antigravity, LM Studio), `ahma setup` **should** additionally inject an explicit `--sandbox-scope` (or rely on the `~/sandbox` default) so the client works without a stall.
- **R5.4.3**: **Write Protection**: The system **must** block any attempt to write outside the locked scope, including via command arguments (e.g. `touch /outside/file`).

#### Persistent scope grants (external tool directories)

- **R5.4.4**: **User-granted persistent scopes survive `roots/list`**: Directories listed in `[sandbox] persistent_scopes` (`~/.ahma/settings.toml`) are external locations a trusted tool legitimately needs outside the workspace — e.g. a build cache (sccache/ccache) or a shared toolchain. Each entry carries an `access` (`rw` default, or `ro`) and optional `granted_by` / `granted_at` / `note` provenance. Unlike the provisional `scopes` list (replaced when the client sends workspace roots), every persistent scope **must** be re-appended on each `roots/list` update — `rw` into the writable set, `ro` into the read-only set — so the grant remains in effect for the whole session regardless of which workspace the client opens. They are folded into the initial scope set before first enforcement.
- **R5.4.5**: **Grants are authored only by the unsandboxed control plane, never by a sandboxed command**: `persistent_scopes` lives in `~/.ahma/settings.toml`, which is outside every workspace scope and therefore kernel-unwritable from inside the sandbox — so no sandboxed *command* (a `run_terminal_command` child, a build script) can ever author one. A grant is written only by the trusted control plane, from one of: explicit human action (`ahma sandbox grant`, or a hand edit); an approved elicitation/TUI prompt; or the `sandbox_grant` MCP tool. The `sandbox_grant` tool **must** gate every write behind **two independent** controls: (a) a **two-phase human confirmation** — the first call only *previews*, returning the absolute settings-file path and the exact line it would add, and **must** write nothing without an explicit `confirm: true` (default Deny); and (b) a **hard denylist** that **must** refuse outright — even when confirmed — the filesystem root, the exact `$HOME`, any parent of a live workspace scope, credential directories (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.docker`, `~/.config/gh|gcloud`), `~/.ahma` itself, and OS system directories. No model-supplied input can override the denylist. The AI may *propose* a scope and form the exact line; the human still *approves* it, and the catastrophic-path denylist holds regardless. This is the persistence layer the elicitation downgrade flow (R5.3) and the `sandbox_grant` tool both write through once a request is approved.
- **R5.4.6**: **Grants are inspectable and reversible by name**: `ahma sandbox list` **must** show every persistent scope with its access level and provenance, and **must** name the absolute settings file that holds them, since that file — edited by hand, or by `ahma sandbox revoke`, never by a sandboxed command — is where the user reviews and removes grants. `ahma sandbox grant`/`revoke` and the `sandbox_grant` tool **must** confirm the change, name that same file, and state that it takes effect on the next server start.
- **R5.4.7**: **Auto-detection raises the grant question, never the grant**: when a sandboxed command is blocked by an out-of-scope path — either rejected up front by path validation (the path is known exactly) or surfaced by a stderr denial signature (a heuristic *candidate* path) — the server **must** surface the denial as actionable signal: a structured `sandbox_denial` payload on the tool error (path, access, current scopes, remediation) and/or a "grant access to X?" prompt. Remediation flows only through R5.4.5 — a human-confirmed `ahma sandbox grant`, an approved elicitation prompt, or the gated `sandbox_grant` tool — so detection itself **must not** widen the live session: an approved grant is persisted for the next start, identical to `ahma sandbox grant`. The same offending `(path, access)` **must** be asked at most once per session (a denied or already-granted path **must not** re-prompt), and a stderr-extracted path **must** be canonicalized and shown literally — a command that prints a forged denial line can at worst raise a human-gated prompt, never escalate on its own.
- **R5.4.8**: **The control-plane directory `~/.ahma` is out of scope for read *and* write, and ahma degrades gracefully when it cannot read its own config**: `~/.ahma` holds ahma's own control-plane state — its settings file, persistent scope grants (R5.4.4/R5.4.5), task vault, and credentials. It is **never** part of any workspace scope: not writable (R5.4.5 establishes it is kernel-unwritable from inside the sandbox) and **not readable** either. Widening scope to `~/.ahma` is on the hard denylist and requires explicit human review (R5.4.5); this is a deliberate security measure so a compromised or curious sandboxed tool cannot read ahma's secrets. A direct consequence: whenever ahma **itself** runs inside its own sandbox — its test suite, a nested invocation, `run_terminal_command` re-entrantly building/testing ahma, or an ahma spawned as a subprocess of another ahma — the read of `~/.ahma/settings.toml` is denied by the kernel and returns a permission error (`EPERM` → Rust `ErrorKind::PermissionDenied`). The settings loader **must** treat an *unreadable* settings file — whether missing (`NotFound`) or permission-denied — as "no user settings present" and fall back to the compiled-in defaults, emitting a loud `warn` for the permission-denied case. It **must not** abort: aborting would make ahma unusable in every sandboxed context (the historical bug: `fatal: failed to read ~/.ahma/settings.toml: Operation not permitted`, cascading to ~150 test failures). This graceful degradation is fail-*safe* because the compiled-in defaults are the secure baseline (sandbox enabled). It does **not** relax R-CFG6.1, whose fail-closed rule is narrower and still holds: a settings file that *can* be read but fails to **parse** must still abort, so a tampered or corrupt file can never silently change behavior.

#### Terminal hooks (one-time consent, never silent)

- **R5.5.3**: **Hook fall-open requires one-time, session-scoped consent**: A terminal hook that can sandbox the command runs normally. When the hook binary **executes** but cannot sandbox (it is stale/incapable, or the kernel sandbox is unavailable), it **must not** silently run the command unsandboxed. Instead:
  - The **first** such invocation in a session **fails closed**: it returns a `deny` decision to the IDE and emits an actionable message (the reason plus `ahma hooks doctor` / repair guidance and how to approve unsandboxed mode). `ahma hooks doctor`, `ahma hooks approve-unsandboxed`, and `ahma hooks revoke` manage and diagnose this state.
  - **Residual (binary cannot execute at all)**: if the hook binary is entirely missing or crashes/times out before producing a decision, ahma cannot emit `deny` and the IDE-level `failClosed` setting governs. The default install keeps `failClosed: false` as a deliberate anti-wedge valve, because the consent mechanism itself requires the binary to run; this residual is loud (the IDE surfaces the hook failure) and is repaired with `ahma hooks doctor` / reinstall, not silent in the steady-state sense R5.5.3 targets.
  - Consent is collected **out-of-band** (an `elicitation/create` to an attached client/TUI, or an explicit `ahma hooks approve-unsandboxed` command) — never mid-command. Approving unsandboxed execution is maximal widening and **must** be an explicit, deliberate action, never an Enter-default.
  - Consent is scoped to **workspace + session generation** and **must not** persist across restarts (persisting would silently re-downgrade the next session).
  - While consent is active, every surface (R5.4) **must** continuously display a prominent banner stating that hooks are running unsandboxed and how many commands have done so.
- **R5.5.4**: **Hook default-enablement is gated on the permission ladder, per client**: hooks are not installed by default until the client in question can carry a denial through to a user decision. The gate is defined in **R-PERM.6**; until a client passes it, `ahma setup` **must not** install hooks for that client, and the reason **must** be stated rather than left as an unexplained omission.
- **R5.6**: **Lifecycle Notifications**: The system **must** emit JSON-RPC notifications for sandbox lifecycle events:
  - `notifications/sandbox/configured`: When sandbox is successfully initialized from roots.
  - `notifications/sandbox/failed`: When sandbox initialization fails (payload: `{"error": "message"}`).
  - `notifications/sandbox/terminated`: When the session ends (payload: `{"reason": "reason"}`).
- **R5.6.1**: **Best-Effort Delivery over Pipes**: In HTTP bridge mode, lifecycle notifications are written as raw JSON-RPC to the subprocess's stdout so the bridge can intercept them. Delivery is **best-effort**: a broken-pipe error (Unix `EPIPE`, Windows OS error 232 "The pipe is being closed") during the write **must not** panic the process. This condition is expected when the bridge closes the pipe during session teardown. All stdout notification writes **must** use `utils::stdio::emit_stdout_notification`, which classifies errors as follows:
  - **Broken pipe**: logged at `debug` level, treated as non-fatal (the bridge is already shutting down).
  - **Other I/O errors**: logged at `warn` level and returned to the caller, which may choose to abort or continue.
  - Code **must not** use `println!` or `print!` for protocol data on stdout; these macros panic unconditionally on write errors.
- **R5.7**: **Path Canonicalization**: All paths **must** be canonicalized using `dunce::canonicalize` before validation to prevent symlink escape attacks. This resolves symlinks to their real targets and normalizes paths, ensuring that a symlink pointing outside the sandbox cannot be used to bypass security. The `dunce` crate is used instead of `std::fs::canonicalize` to avoid the Windows `\\?\` extended-length path prefix that can cause compatibility issues with some APIs.

### R6: Platform-Specific Enforcement

#### R6.1: Linux (Landlock)

- **R6.1.1**: Uses Landlock (kernel 5.13+) for kernel-level FS sandboxing.
- **R6.1.2**: If Landlock is unavailable and sandbox is not explicitly disabled, server **must** refuse to start with upgrade instructions.
- **R6.1.3**: If user explicitly opts into compatibility mode (`--disable-sandbox` or `AHMA_DISABLE_SANDBOX=1`), server **must** start in unsandboxed mode and emit a clear warning that Ahma sandboxing is disabled until the kernel is upgraded.
- **R6.1.4**: **Spawn-time enforcement (per command)**: `landlock_restrict_self(2)` restricts only the calling thread and threads/processes created after it, so process-level enforcement performed inside an already-running async runtime does **not** cover commands spawned from pre-existing worker threads. Every child process created through `Sandbox::create_command` (including PTY execution) **must** have the Landlock ruleset — built from the sandbox's current scopes — applied in `pre_exec`, between `fork` and `exec`, where the child is single-threaded. Process-level enforcement at startup remains as defense-in-depth for the server itself.
- **R6.1.5**: **Availability probing**: Landlock availability **must** be determined by calling `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` — never by kernel version or the `/sys/kernel/security/lsm` list, both of which report false positives in containers (seccomp-blocked syscall, unmounted securityfs, LSM compiled out).

#### R6.2: macOS (Seatbelt)

- **R6.2.1**: Uses `sandbox-exec` with Seatbelt profiles (SBPL).
- **R6.2.2**: Profile uses `(deny default)` with allowed reads and writes **strictly limited** to the sandbox scope, necessary system paths, and necessary temp paths.
- **R6.2.3**: **Read/Write Limitation**: The security guarantee is **read and write isolation**. By default, it operates identical to Landlock: standard system binaries (`/usr`, `/etc`, `~/.cargo`) are whitelisted for read/execute, and all other paths outside the scope are denied.  `~/.cargo/registry` and `~/.cargo/git` receive **additional write access** by default so agents can fetch new dependency versions (see R6.2.5).
- **R6.2.4**: **CRITICAL**: `/var` is symlink to `/private/var` on macOS; profiles **must** use real paths.
- **R6.2.5**: **Package Cache Write** (default on): `~/.cargo/registry/` and `~/.cargo/git/` (and cargo's root lock files) are writable by default so that `cargo add` / `cargo update` work inside the sandbox without manual `--sandbox-scope ~/.cargo` which would grant write to the entire cargo home including binaries and credentials. The writable set is computed from `$CARGO_HOME` (or `~/.cargo`) and excludes `bin/`, `config.toml`, and `credentials.toml`.  Disable with `--no-package-cache-write` / `AHMA_NO_PACKAGE_CACHE_WRITE=1` / `[sandbox] package_cache_write = false` for the strictest isolation.

#### R6.3: Windows (AppContainer / Job Objects) — _in-progress_

> **Security gate**: Windows GA release requires this section to reach `tests-pass` status.
> Until it does, strict mode **must** fail closed (`SandboxError::PrerequisiteFailed`) so the
> server never runs unsandboxed without explicit `--disable-sandbox` opt-out.
>
> **Current status**: Job Object enforcement is implemented in `sandbox/windows.rs`.
> AppContainer availability probing is present, but per-command AppContainer spawn
> isolation and scoped access grants are not active yet. Final validation (R6.3.1,
> R6.3.3, R6.3.7) requires a `windows-latest` CI run.

##### Architecture decision

The planned implementation uses two mechanisms in order of preference:

1. **AppContainer** (Windows 8+) — lowest-privilege user-space sandbox.
   An `AppContainer` SID will be given read+execute on the Windows system directory and
   full access only to the workspace root. This is the primary containment mechanism.
2. **Job Objects with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`** — applied unconditionally at
   server startup via `enforce_windows_sandbox`.  Ensures all child processes are killed
   when the server exits.  Does **not** restrict file-system access by path; AppContainer
   is required for R6.3.3.

##### Acceptance criteria (required before GA)

- **R6.3.1**: `check_windows_sandbox_available()` returns `Ok(())` when the AppContainer
  API is available on Windows 8+. _Status: implemented (probes `CreateAppContainerProfile`
  with an invalid name; returns `Ok(())` on Win8+, `PrerequisiteFailed` on older OS)._
- **R6.3.2**: `enforce_windows_sandbox(roots)` applies Job Object containment at server
  startup, ensuring child processes are killed on server exit. Signature mirrors
  `enforce_landlock_sandbox` (`&[PathBuf]`). _Status: **done** — Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` applied; non-fatal if already inside a job._
- **R6.3.3**: Write attempts outside the sandbox scope **must** be blocked at the OS level.
  Proof: a test must show that `tools/call` inside the scope succeeds while a write to a
  path outside the scope fails with a permission error.
- **R6.3.4**: `tools/call` issued before sandbox lock (state != `Locked`) **must** return
  HTTP 409 / JSON-RPC `-32001` on Windows, identical to Linux/macOS behavior.
- **R6.3.5**: Filesystem root scopes (`C:\`, `D:\`, UNC `\\server\share`) **must** be
  rejected by `canonicalize_scopes` with `SandboxError::PrerequisiteFailed`, identical to
  Unix `/` rejection.
- **R6.3.6**: PowerShell (built into Windows 10/11) **must** be documented as a runtime requirement;
  the server should emit a clear startup error if `powershell` is absent.
- **R6.3.7**: All existing integration tests that exercise sandbox gating logic **must**
  pass on Windows CI with no `#[ignore]` waivers.
- **R6.3.8**: **Cross-Platform Test Scripts**: When tests dynamically generate and execute scripts (e.g., to verify log monitoring or stdout capture), they **must** provide equivalent logic for both `bash` (Unix) and `PowerShell` (Windows). Tests **must not** rely on `bash.exe` or `sh.exe` being present on Windows (avoids WSL dependencies). All such tests **must** use a uniform helper method (e.g., `write_cross_platform_script`) to ensure consistency and prevent platform-specific leaks.

##### Windows path model

- Sandbox scope paths use native Windows absolute paths (e.g., `C:\Users\name\project`).
- File URIs from MCP clients are parsed by `SessionManager::parse_file_uri_to_path` which
  handles `file:///C:/...` (drive letter) and `file://server/share/path` (UNC) forms.
- `normalize_path_lexically` never pops a `Prefix` or `RootDir` component (enforced by
  `scopes.rs`).

### R7: Nested Sandbox Detection and Deferral

A "host sandbox" is an outer kernel sandbox ahma is running inside (Cursor, VS Code, Docker). ahma detects it from environment markers (`CURSOR_SANDBOX`/`CURSOR_AGENT`, `VSCODE_*`, `/.dockerenv`/`container`) and, as an unnamed fallback, the platform nesting probe. ahma **must not** chase a host's private internals (e.g. its injected build-cache env vars) to coexist; instead it chooses **one authoritative sandbox per execution path** and **always discloses which one is active** (R5.4 "nothing silent").

- **R7.1**: System **must** detect when running inside a host sandbox and, where possible, name it (Cursor/VS Code/Docker); otherwise report it as an unidentified outer sandbox.
- **R7.2 (terminal hooks — defer to host)**: When an ahma terminal hook fires inside a detected host sandbox, the command already runs under the host's kernel sandbox, so ahma **must** defer: it allows the command **unchanged** (it runs in the host sandbox) and **must not** re-wrap it in ahma's own sandbox. This deferral **must** be disclosed loudly (hook `systemMessage`/`userMessage`), stating that protection comes from the host and that ahma is not re-enforcing — and that if the host's sandbox is disabled the command is unsandboxed. Deferral is **not** an "unsandboxed bypass" and is not counted as one. Users who want ahma's own (tighter) sandbox instead **may** set `AHMA_PREFER_OWN_SANDBOX=1`, accepting the redundant double-sandbox and the host's build-cache friction.
- **R7.3 (MCP / standalone — stay authoritative)**: When ahma itself executes commands (the MCP `run_terminal_command` path, or standalone), the host sandbox does **not** wrap those executions, so ahma **remains authoritative** and applies its own sandbox. If ahma cannot apply its own sandbox, it **must** fail loudly with instructions (use `--disable-sandbox` to defer to the host explicitly) — it **must never** silently run unsandboxed.
- **R7.4**: When `--disable-sandbox` is used, the outer sandbox provides security and ahma's internal sandbox is disabled; the active-sandbox disclosure **must** reflect this (deferred-to-host when a host is detected, otherwise disabled).
- **R7.5 (honesty limit)**: Detecting a host does **not** prove the host's sandbox is *enabled* (it may be configured off). Disclosure copy **must** therefore state that protection now depends on the host, so a user who disabled the host sandbox is informed rather than surprised.

### R-PERM: Unified Permissions Model

**Problem this family solves.** ahma already has every *mechanism* needed to let a user grant an exception when the sandbox blocks something they legitimately want: kernel denial detection (R5.4.7), persistent grants (R5.4.4–R5.4.8), elicitation prompts (R5.3.1), a TUI modal (R-WEB.6), and a dedup coordinator (R-WEB.7). What it lacks is **convergence**: those mechanisms live in separate code paths, persist to two different config trees, and are asked through whichever surface each subsystem happened to wire up. The consequence is that terminal hooks cannot be enabled by default — a hook denial has no realistic path to a user decision — and that the sandbox needs hard-coded, app-specific carve-outs to be usable at all.

**Design principles (govern all of R-PERM), inherited from R5:** the kernel denial *is* the discovery mechanism — ahma cannot predict what the thousands of applications it will never see need, but the kernel reports the exact path at the exact moment of need. The generic loop is therefore **deny → detect → ask once, with context → remember at a chosen tier → apply**. Nothing is silent; nothing self-widens; the user is asked only on a genuine downgrade; when nobody can be asked, ahma fails closed to a shown default.

#### One ledger

- **R-PERM.1**: **All persistent permissions live in `~/.ahma/`, and nowhere else**: filesystem scope grants (R5.4.4), web-domain grants (R-WEB.5), per-workspace tool approvals, and hook unsandboxed consent (R5.5.3) **must** share a single control-plane directory. `~/.config/ahma/` is retired as a permission store; an existing `approvals.json` there **must** be migrated once, non-destructively, and the legacy file left in place with a `.migrated` suffix. The ledger directory inherits R5.4.8 unchanged: it is never part of any workspace scope, is kernel-unreadable and kernel-unwritable from inside the sandbox, and therefore **cannot** be authored by a sandboxed command.
- **R-PERM.2**: **One record shape, one preview, one confirmation**: every grant, of every kind, is representable as `{kind: fs-scope | web-domain | tool | hook-unsandboxed, subject, access, tier, granted_by, granted_at, surface, note}`. `tier` is one of `once` | `session` | `always`. A `once` grant is **never** stored. A `session` grant lives **only** in memory and dies with the instance. Only `always` is written to disk, and only after the preview-and-approve exchange R5.4.5 already mandates for `sandbox_grant`, generalized to every kind: the user is shown the **absolute file path** and the **exact line(s)** that would be written, and nothing is written without explicit approval. The R5.4.5 hard denylist gates **every** write path into the ledger — the MCP tool, the CLI, and any elicitation/TUI answer — not just the `sandbox_grant` tool.
- **R-PERM.2.1**: **One CLI, one audit trail**: `ahma permissions list | grant | revoke` **must** manage every kind through the same preview-and-confirm path, showing provenance (`granted_by`, `surface`) for each record. Kind-scoped aliases (`ahma sandbox grant|list|revoke`, `ahma web allow|list|revoke`) **must** continue to work, because they are the strings ahma itself emits as remediation. Every persist and revoke **must** append one record to an append-only audit log in `~/.ahma/`.

#### The question ladder (where a permission question is asked)

- **R-PERM.3**: **Surfaces are tried in a fixed order, and the harness is preferred**: when a permission question must be asked, ahma **must** try, in order:
  1. **The initiating MCP client (harness)**, via `elicitation/create` (R5.3.1) — *iff* that client advertised the `elicitation` capability at `initialize` **and** has not been demoted this session. This is the preferred surface whenever it works: the user is already looking at it, and it carries the context of the work that triggered the denial.
  2. **An attached ahma TUI**, via the grant modal (R-WEB.6 semantics: the modal renders over both chat and monitor modes; **Enter and Esc both deny**; the persist option names the settings file and the exact line).
  3. **Nobody can be asked → fail closed** (never fail open): the operation fails with the structured `sandbox_denial` payload of R5.4.7 **and** a copy-pasteable remediation command (`ahma sandbox grant <path> --ro|--rw`). This rung is the universal fallback: it works in every harness, including clients that render only tool-result text, and it is the *only* rung guaranteed to exist.
- **R-PERM.3.1**: **Demotion is for broken surfaces, not for "no" answers**: an elicitation **timeout or transport error** demotes that client's elicitation channel for the remainder of the session ("one strike") and subsequent questions skip to rung 2. A **decline is an answer**, not a failure: it resolves the question as Deny and the client stays trusted as an asking surface. Demoting on decline would train the system to abandon a working surface the moment a user says no.
- **R-PERM.3.2**: **The user is told where the question went**: whenever a fallback occurs (rung 1 unavailable or demoted, or rung 2 absent), the resulting message **must** state that the preferred surface could not be asked. A question that silently relocates is indistinguishable from a question that was never asked.
- **R-PERM.3.3**: **Multiple live surfaces are coordinated, not raced**: rungs 1 and 2 may both be live for the same question. The server owns the decision under one `decision_id` and applies R5.3.3 (fan-out; first answer wins; the losing surface is dismissed via `notifications/cancelled`) and R5.3.4 (most-restrictive-wins on a tie).
- **R-PERM.4**: **Ask at most once per `(subject, access)` per session**, across all surfaces and all concurrent operations (R5.4.7, generalized). A `session`-tier answer — in **either** direction — suppresses further questions for that subject for the life of the instance. A denied subject **must not** re-prompt. The one deliberate exception is an explicit user re-raise (R-PERM.7.1), which is a fresh human action, not a repeat prompt.
- **R-PERM.4.1**: **When an answer applies**: on the MCP server path an `always` or `session` grant **must not** widen the live locked scope (R5.1); it is persisted/recorded and reported as taking effect at the next server start, exactly as R5.4.6 already requires. On the **terminal hooks** path the sandbox is re-derived per command, so a grant **must** take effect on the very next command with no restart — this difference is a feature of hooks and **must** be stated in the confirmation message rather than papered over.

#### Sandbox profiles (no app-specific exceptions in code)

- **R-PERM.5**: **Toolchain carve-outs are shipped data, not compiled-in special cases**: the sandbox backends **must not** contain hard-coded application or toolchain paths (`~/.cargo`, `~/.rustup`, `~/.nvm`, `~/.npm`, `~/.go`, and the cargo package-cache write set). Each such carve-out **must** be expressed as a declarative **profile** — a data file naming scopes, their access, and any `never` exclusions within them (e.g. cargo's `bin/`, `config.toml`, `credentials.toml`, which stay denied) — and folded into the effective scope through the **same code path** as a user grant, with provenance `builtin-profile(<name>)`. A profile is nothing more than a **pre-answered bundle of grant questions**, which is why it can be shipped, community-contributed, inspected, and disabled.
  - Profiles **must** be visible in `ahma permissions list` and in the R5.4 scope displays with their provenance, and individually disableable (`[sandbox] profiles`). Default is **opt-out**: the builtin profiles ship enabled, so existing behavior is preserved — but it becomes *visible* and *refusable* rather than invisible and mandatory.
  - Platform-invariant rules (`/usr`, `/bin`, `/etc` read/execute; device-path denials; credential-directory denials) are **not** profiles and remain in the backends. The test is *app-specific*, not *platform-specific*.
- **R-PERM.5.1**: **What cannot be expressed as a profile must be disclosed**: the macOS Seatbelt backend currently grants blanket read access (`(allow file-read*)`) as a workaround for APFS firmlinks/cryptexes, so on macOS **writes are kernel-scoped but reads are not**. This is a platform limitation, not a grant, and therefore cannot be represented as a profile. It **must** be disclosed in every scope surface (R5.4) — the startup banner, `ahma status`, and the TUI scope panel — in the same honest register R7.5 requires of host-sandbox deferral. A limitation the user cannot see is a limitation the user cannot compensate for.

#### Hooks gating

- **R-PERM.6**: **Hooks are enabled per client, gated on the ladder — not on perfect classification**: terminal hooks were disabled by default because a denial had no realistic path to a user decision, not because their sandbox classification is inadequate. A client **must** therefore be enabled for hooks by `ahma setup` only once it satisfies:
  1. **The loop closes in that client**: a denial round-trips deny → question (on whichever rung applies) → grant → the *next* command succeeds (R-PERM.4.1).
  2. **The fail-closed message is legible in that client**: the R-PERM.3 rung-3 message must be surfaced where the user will see it — never *only* as a bare `Operation not permitted` line buried inside a build log (the historical failure mode: a host build-cache denial inside a dependency's build script).
  3. **Nested sandboxes still defer**: R7.2 defer-to-host remains the default inside a detected host sandbox, which removes most of the surface where hooks "get in the way" in the first place.
- **R-PERM.6.1**: A hook denial has no MCP session of its own. Rung 1 is available **only** when a live MCP session for the same workspace can be asked; otherwise the ladder starts at rung 2 (attached TUI) and falls to rung 3 (a remediation block printed to the terminal the command ran in, *after* the command's own output, so it is not lost in scrollback).

#### Making the question findable

- **R-PERM.7**: **A denial is a first-class, visible event**, not just an error string: every denial **must** appear in the operation stream with the operation identity of the command that caused it (R24.7), so it is visible in the TUI monitor and chat views and in replay after late attach.
- **R-PERM.7.1**: **A denied operation is selectable and re-raisable**: in the TUI, selecting a denied operation and confirming **must** re-raise the grant question through the same broker, with the same preview. This is an explicit human action and therefore bypasses the R-PERM.4 ask-once memo (it is not an unsolicited re-prompt). This is the "escape hatch with context" that hooks have never had.

---

## 4.5 File System Contracts and Features

### R8: Project Logging (`/logs` directory)

- **R8.1**: All ahma and execution logs **must** be placed in the `logs/` directory at the root of the (primary) configured sandbox scope, rather than global user cache directories (`~/.cache`).
- **R8.2**: When the project is built or the server initialized, the `logs/` directory is created if it does not exist, and old `.log` files are deleted to wipe previous logs.

### R9: Safe Live Log Monitoring (`--livelog`)

- **R9.1**: The `--livelog` feature flag enables safe read-only access to specific log files located outside the sandbox scope without compromising the sandbox contract.
- **R9.2**: **Mechanisms**: During initialization (and ONLY at initialization), the system scans the `logs/` directories of all configured sandbox roots for symbolic links. The targets of these symlinks are evaluated.
- **R9.3**: **Enforcement**: The resolved physical paths of those symlinks are dynamically added to the sandbox profile (across Linux, macOS, and Windows) as **read-only scopes**.
- **R9.4**: **Abuse Prevention**: Since symlinks are only resolved and granted access at startup, hostile entities or rogue AI cannot abuse this later by creating new symlinks to sensitive files (e.g. `/etc/passwd`). Existing files placed in read-only scopes are tightly controlled by the system operator running `ahma --livelog`.
- **R9.5**: **LLM-Based Detection** (`tool_type: livelog`): Tools with `tool_type: livelog` spawn their `source_command` inside the kernel-enforced sandbox scope. The LLM endpoint is an outbound connection from the ahma process and is not subject to the inbound sandbox policy. See Section 5.5 for the full pipeline specification.

---

## 4.6 Web Egress Sandboxing (R-WEB)

### Design rationale and critical analysis

The filesystem sandbox (R5/R6) governs what the agent can *read and write locally*. It says nothing about what the agent can *send outward*. This is a distinct and under-addressed threat:

- **Exfiltration**: A prompt-injection payload in a fetched file can instruct the agent to POST vault contents to an attacker-controlled server. The filesystem sandbox prevents the file from being read outside the scope, but if `fetch_webpage` is also used to POST, the data leaves via HTTP.
- **SSRF (Server-Side Request Forgery)**: Unrestricted outbound HTTP from the ahma process — which runs with the user's full network privileges — can reach `http://localhost:8080/admin`, `http://192.168.1.1/` (home router), or `http://169.254.169.254/latest/meta-data/` (cloud instance metadata). None of these are reachable from inside a browser's origin model; they are reachable from a local process.
- **Uncontrolled API spend / rate abuse**: An agent running in a loop can burn API quotas or trigger account suspension on any service whose domain is reachable.
- **DNS rebinding after approval**: A domain approved at time T can have its DNS entry changed to resolve to `169.254.169.254` at time T+1, routing subsequent traffic to the metadata service under the cover of an approved pattern.

**Why "just copy the filesystem grant model" is insufficient:**

The filesystem grant says: *this directory is structurally safe to access.* The filesystem is passive — a path grants access to bytes that sit still.

A web domain grant says: *I trust bidirectional communication with this domain, including data I send to it.* The internet is active — a domain is an operator who receives your data, can redirect you, changes DNS, and may share data downstream.

This matters for design:
1. Filesystem approval is about *access*; web approval is also about *disclosure*.
2. The "deny is safe" invariant is even more important for web than filesystem: a denied web request leaks nothing; a denied file read blocks the agent, but data stays local.
3. Domain-level approval is necessarily coarse. Approving `github.com` approves every GitHub API endpoint — the repos API, the Gist upload API, the OAuth token exchange endpoint. There is no path-level approval; see R-WEB.13.

**Established patterns this design draws from:**

| System | Pattern used | What ahma adapts |
|--------|-------------|-----------------|
| Browser Content Security Policy | `api.example.com`, `*.example.com` (single-level) | Pattern syntax |
| macOS AppSandbox entitlements | `com.apple.security.network.client` enable/disable | Process-level `default_policy` |
| Little Snitch / LuLu | Per-connection prompts; once / always; domain patterns | Three-tier approval + TUI modal |
| Burp Suite intercept | Every request, show full URL, user decides | "Allow once" option |
| `docs/egress-sandbox.md` (subprocess proxy) | Per-vault allowlist, deny-by-default | Persistent allowlist format and pattern syntax |
| DNS-based blocklists (Pi-hole) | Domain block/allow with wildcard | Pattern matching rules |

**What this design explicitly does NOT do, and why:**

- **No path-based approval** (`github.com/api/*` vs `github.com/login/*`): URL paths and query strings are not meaningful security boundaries — the same data can be sent via POST body to any path, and redirects can change the path after approval. Path patterns create false confidence. See R-WEB.13.
- **No response content filtering**: Scanning response bodies for sensitive data is expensive, unreliable, and privacy-invasive. The right control is at the request level, not the response level.
- **No per-HTTP-method distinction in patterns**: The current `fetch_webpage` tool is GET-only. Future tools may add POST. When they do, the method should be *shown in the approval prompt* but the approved pattern covers all methods for that domain — restricting by method in a pattern creates false confidence (any GET can include query parameters that effectively write data).
- **No rate limiting in this module**: Rate limiting per approved domain is important but orthogonal; it belongs in a separate rate-limit layer, not in the domain-approval flow.

---

### R-WEB.1: Scope

This section governs **tool-level outbound HTTP requests made by the ahma process itself** — currently `fetch_webpage`, and any future tool that uses `EgressClient` (R-WEB.14). It does **not** govern:
- Subprocess HTTP traffic inside task vaults (that is the HTTP proxy described in `docs/egress-sandbox.md`, formalized in R-WEB.16).
- The ahma process's own MCP client connections (LLM provider `base_url`) — those are operator-configured endpoints, not agent-driven requests.
- Inbound connections to the ahma MCP server.

---

### R-WEB.2: Default policy

- **R-WEB.2.1**: The default policy is `"allow"`. In `allow` mode, requests to domains not in `always_allow` or `never_allow` are **passed through without prompting** — preserving backward compatibility with existing `fetch_webpage` usage.
- **R-WEB.2.2**: Setting `default_policy = "deny"` in `[web]` switches to **strict mode**: any domain not in the session grant list or `always_allow` causes the request to be held and a prompt raised (R-WEB.6). Strict mode is the recommended production posture.
- **R-WEB.2.3**: Regardless of `default_policy`, `never_allow` entries **always block** and `block_private_ranges` (R-WEB.3) **always enforces**. Neither can be overridden by session grants or `always_allow` patterns.
- **R-WEB.2.4**: Regardless of `default_policy`, `always_allow` entries **always permit** without a prompt.

> **Recommendation:** New ahma installations default to `"allow"` for backward compatibility, but the setup wizard and documentation **must** prominently recommend `"deny"` for any workspace handling sensitive data, credentials, or proprietary code. A future major version may flip the default.

---

### R-WEB.3: Private-range blocking (always-on)

- **R-WEB.3.1**: The following address ranges are **always blocked**, regardless of `default_policy`, session grants, `always_allow`, or any other setting:

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

- **R-WEB.3.2**: The private-range check **must** run at **DNS resolution time** — on the resolved IP address(es), not on the domain name string. A domain pattern entry resolving to a private IP is blocked at connection time, catching DNS rebinding attacks where a whitelisted domain's IP changes after approval. A pattern entry whose literal text is a private IP address **must** be rejected at parse time.
- **R-WEB.3.3**: `block_private_ranges = false` (in `[web]`) disables the private-range check. This opt-out is permitted for development environments where the agent legitimately needs to reach a local dev server. Setting it to `false` **must** produce a loud startup warning and a persistent TUI banner, identical in prominence to the `--disable-sandbox` unsandboxed-mode banner.
- **R-WEB.3.4**: The resolved-IP check is the responsibility of a custom `reqwest` connector configured in `EgressClient` (R-WEB.14). Raw `reqwest::Client::new()` callers bypass this check — this is why all tools are required to use `EgressClient`.

---

### R-WEB.4: Domain pattern syntax

Patterns are **domain-only** — no path or query components. Scheme and port are optional qualifiers.

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

> **User expectation gap**: Most users expect `github.com` to match all of GitHub including `api.github.com`. It does **not** — this is intentional and consistent with CSP semantics. To allow all of a domain including subdomains, add both `github.com` and `*.github.com`. The TUI modal (R-WEB.6) suggests both entries when the matched URL is a subdomain with no matching root-domain entry.

- **R-WEB.4.1**: Patterns containing a private IP or `localhost` **must** be rejected at parse time.
- **R-WEB.4.2**: `http://`-scheme entries in `always_allow` **must** produce a warning at parse time and a visible caution in the TUI modal. They are not blocked — some dev environments legitimately use HTTP — but they are never silently persisted.
- **R-WEB.4.3**: Pattern matching is **case-insensitive** for the hostname component (RFC 4343) and **case-sensitive** for scheme.

---

### R-WEB.5: Three-tier approval model

Parallel to the filesystem persistent-scope grant model (R5.4.4–R5.4.7):

| Tier | Lifetime | Storage | Agent can self-grant? |
|------|---------|---------|----------------------|
| **Allow once** | This request only | None | No — requires human key press |
| **Allow session** | Until server restart | In-process `HashSet` | No — requires human key press |
| **Allow always** | Permanent | `[web] always_allow` in `~/.ahma/settings.toml` | No — settings file is outside sandbox |

- **R-WEB.5.1**: Session grants are cleared when the server process exits. They are **never** serialized to disk.
- **R-WEB.5.2**: A persistent grant (`allow always`) is written **only** to `~/.ahma/settings.toml` — outside every workspace scope and therefore kernel-unwritable from inside the sandbox (same guarantee as filesystem persistent scopes, R5.4.5). An agent cannot grant itself permanent web access.
- **R-WEB.5.3**: After a persistent grant is written, the server **must** display the full settings file path, the exact line number, and the content added (e.g. `~/.ahma/settings.toml +47: "api.github.com"`).
- **R-WEB.5.4**: A session deny suppresses re-prompting for that domain for the remainder of the session. There is no interactive "deny always"; use `never_allow` in settings or `ahma web deny <pattern>` for permanent blocks.
- **R-WEB.5.5**: Unlike filesystem scope grants (which take effect on next server start — the R5 invariant), web `always_allow` entries added at runtime **may** take effect immediately within the current session: the session policy is hot-reloaded from the updated settings struct. This is safe because the agent cannot write `settings.toml` — only the human action did.

---

### R-WEB.6: TUI approval modal

When a request is blocked under `default_policy = "deny"` and a TUI surface is attached:

- **R-WEB.6.1**: The modal **must** display: the tool name, the full URL truncated at 256 characters (to prevent prompt-injection via crafted URLs), and the domain string that would be approved for each tier.
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
- **R-WEB.6.3**: **Enter and Esc must deny.** Approving at any tier requires an explicit non-default key. This is the same invariant as the scope-grant modal (R5.3.1: Enter must never widen).
- **R-WEB.6.4**: For `http://` URLs, the `[p]` line **must** carry a visible caution marker: `[p] Persist (⚠ cleartext HTTP): api.github.com`.
- **R-WEB.6.5**: After `[p]` is pressed, the modal area shows the file and line added before dismissing — no separate notification required.
- **R-WEB.6.6**: The modal is drawn last in the TUI render pass, overlaying all other content (same as the scope-grant modal).
- **R-WEB.6.7**: When no TUI is attached, the `elicitation/create` MCP mechanism is used (R5.3.1 pattern). When neither surface is available, the request is **denied** and the tool call returns an error with an actionable message: the exact `ahma web allow <pattern>` command to pre-approve the domain.

---

### R-WEB.7: Dedup / debounce coordinator

A `WebApprovalCoordinator` struct parallel to `GrantCoordinator` (`ahma_common::scope_grant`):

- **R-WEB.7.1**: The same domain is **asked at most once per session**. After the first prompt resolves (deny or grant), subsequent requests for the same domain return the cached answer immediately.
- **R-WEB.7.2**: Concurrent requests to the same domain that arrive while a prompt is pending are **queued**, not dropped. When the prompt resolves, queued requests inherit the decision.
- **R-WEB.7.3**: A denied domain is added to the session deny-list. Further requests in the session are denied immediately (no re-prompt), preventing prompt storms if the agent retries aggressively.
- **R-WEB.7.4**: First-answer-wins when a prompt is fanned to multiple surfaces simultaneously.

---

### R-WEB.8: Redirect chain validation

- **R-WEB.8.1**: When the HTTP client follows a 3xx redirect from an approved domain to a **different** domain, the redirect target **must** be independently checked against the policy. Redirects do not inherit the source domain's approval.
- **R-WEB.8.2**: Default: **block cross-domain redirects** (fail the request with a clear error). The config key `on_redirect_to_new_domain = "block" | "prompt"` governs; `"prompt"` raises a new approval modal for the redirect target.
- **R-WEB.8.3**: Same-domain redirects (including HTTP→HTTPS scheme upgrades for the same host) are permitted without a new prompt.
- **R-WEB.8.4**: The redirect target's resolved IP is also checked against the private-range block (R-WEB.3) regardless of redirect-approval setting.

---

### R-WEB.9: Audit log

- **R-WEB.9.1**: Every outbound HTTP request from a tool (approved, denied, or passed through in `allow` mode) **must** be written to the session audit log with: timestamp, tool name, HTTP method, full URL, resolved domain, decision, and matched pattern.
- **R-WEB.9.2**: Structured JSONL format, appended to the same session log used by filesystem scope-grant events:
  ```json
  {"ts":"2026-06-24T12:00:00Z","kind":"web_request","tool":"fetch_webpage",
   "method":"GET","url":"https://api.github.com/repos/…","domain":"api.github.com",
   "decision":"approved-session","matched_pattern":"api.github.com"}
  ```
- **R-WEB.9.3**: Audit log writes are best-effort (non-fatal on error) and **must not** block the HTTP request.

---

### R-WEB.10: CLI management commands

Parallel to `ahma sandbox grant|list|revoke`:

| Command | Effect |
|---------|--------|
| `ahma web allow <pattern>` | Add to `[web] always_allow`; show file path + line added |
| `ahma web deny <pattern>` | Add to `[web] never_allow`; show file path + line added |
| `ahma web list` | Show `default_policy`, `block_private_ranges`, all entries with provenance |
| `ahma web revoke <pattern>` | Remove from `always_allow` or `never_allow`; show file path + line removed |
| `ahma web check <url>` | Dry-run: report what decision the policy would make for this URL |

- **R-WEB.10.1**: `ahma web allow` and `ahma web deny` **must** reject invalid patterns (bare `*`, TLD-level wildcards, private IP addresses) and warn on `http://`-scheme patterns.
- **R-WEB.10.2**: Every mutation command **must** print the settings file path and the exact line changed or added.
- **R-WEB.10.3**: `ahma web list` includes a `last_used` timestamp column (tracked in-session, cleared on restart) to encourage pruning stale entries.

---

### R-WEB.11: TOML configuration schema

The `[web]` section in `~/.ahma/settings.toml` (parallel to `[sandbox]`):

```toml
[web]
# "allow" (default, backward-compatible) or "deny" (strict mode: prompt for unknown domains).
# Recommendation: use "deny" for any workspace handling sensitive data or credentials.
default_policy = "allow"

# Block loopback, RFC-1918, link-local, and cloud-metadata IP ranges.
# Enforced at DNS resolution time (not just pattern matching) to resist DNS rebinding.
# STRONGLY recommended: keep true. Setting false enables SSRF attacks against local services.
block_private_ranges = true

# "block" (default): cross-domain redirects fail the request.
# "prompt": raise a new approval modal for the redirect target domain.
on_redirect_to_new_domain = "block"

# Domains always permitted without a runtime prompt.
# Syntax: exact ("api.github.com"), single-level wildcard ("*.github.com"),
#         scheme-qualified ("https://api.github.com"), port-qualified ("api.github.com:8080").
# Note: "github.com" matches github.com only — NOT api.github.com.
#       Add both "github.com" and "*.github.com" to allow all of GitHub.
always_allow = []

# Domains always blocked regardless of default_policy, always_allow, or session grants.
never_allow = []
```

- **R-WEB.11.1**: The `[web]` section uses `#[serde(deny_unknown_fields)]` so a typo is a hard error rather than a silent no-op.
- **R-WEB.11.2**: `ahma config validate` **must** parse and validate every pattern in `always_allow` and `never_allow`, rejecting the config with a clear error if any pattern is invalid.
- **R-WEB.11.3**: The settings file lives in `~/.ahma/settings.toml` — outside every workspace scope — so no sandboxed tool can read or modify it (same guarantee as `persistent_scopes`, R5.4.5).

---

### R-WEB.12: Provenance and file confirmation

- **R-WEB.12.1**: `always_allow` and `never_allow` entries support optional inline-table provenance (plain strings are also accepted and round-trip as plain strings):
  ```toml
  always_allow = [
    { pattern = "api.github.com", granted_at = "2026-06-24", note = "GitHub API for PR tooling" },
    "*.stackoverflow.com",
  ]
  ```
- **R-WEB.12.2**: When a persistent grant is made via TUI or CLI, the confirmation message **must** include the settings file absolute path, the zero-based line number, and the full text of the line as written:
  ```
  Persisted: ~/.ahma/settings.toml +47
    "api.github.com"
  Takes effect immediately for this session.
  ```

---

### R-WEB.13: No path-based restrictions (by design)

Path-based domain approval (`github.com/api/*` permitted, `github.com/login/*` denied) is **explicitly not supported**. This is a deliberate design choice:

1. **Query parameters carry as much data as paths**: `github.com/search?q=secret` and a POST to `github.com/submit` with a body containing `secret` are equivalent exfiltration vectors. Path filtering addresses neither.
2. **False confidence**: a user who sees `github.com/api/*` in the allowlist believes `github.com/upload` is blocked, when in fact the path component is not inspected.
3. **Practical coverage**: the meaningful security boundary is the domain operator, not the URL path. Trusting `github.com` means trusting GitHub's access controls.

Users who need path-level or header-level egress control should route traffic through a dedicated HTTP proxy (the subprocess egress proxy, R-WEB.16). That is the right tool for that job.

---

### R-WEB.14: Internal implementation architecture

- **`WebPolicy`** (`ahma_common::config`): the `[web]` config struct. `#[serde(deny_unknown_fields, default)]`. Parsed at startup, stored in `AhmaSettings`.
- **`WebDomainPattern`** (`ahma_common::web_egress`): validated parsed pattern. `parse(s) -> Result<Self, PatternError>`. `matches(url: &Url) -> bool` (case-insensitive hostname, optional scheme/port filter).
- **`WebApprovalRequest`** (`ahma_common::web_egress`): `{ request_id: Uuid, url: Url, domain: String, method: HttpMethod, tool: String }`.
- **`WebDecision`** enum: `Deny | AllowOnce | AllowSession(WebDomainPattern) | AllowPersist(WebDomainPattern)`.
- **`WebApprovalCoordinator`** (`ahma_common::web_egress`): holds a `Mutex<HashMap<String, PendingSlot>>` keyed by domain (R-WEB.7), a `HashSet<WebDomainPattern>` for session grants, a `HashSet<String>` for the session deny-list.
- **`EgressClient`** (`ahma_common::web_egress`): wraps `reqwest::Client`. Pre-request async check sequence: (1) `never_allow` → immediate error; (2) private-range block on URL host → immediate error; (3) `always_allow` → pass; (4) session grant set → pass; (5) `default_policy = "allow"` and no match → pass; (6) otherwise → call `WebApprovalCoordinator::request_decision()` and await. On redirect, re-run the full sequence for the new URL (R-WEB.8). On connect, re-check resolved IP (R-WEB.3.4).
- **All tools making outbound HTTP calls must use `EgressClient`**, not `reqwest::Client::new()`. A Clippy deny lint **should** be added to prevent bare `Client::new()` in tool-handler code.
- The TUI `draw_web_approval_modal` function follows the same pattern as `draw_scope_grant_modal`: drawn last, `[n]` highlighted as default, Enter/Esc deny.

---

### R-WEB.15: Interaction with existing approval systems

- **R-WEB.15.1**: Web domain approval is **orthogonal** to tool-level approval (`ahma_core::approvals`). Approving `fetch_webpage` as a tool does not automatically approve any domain; domain approval is a separate, independent control.
- **R-WEB.15.2**: Web domain approval and filesystem scope-grant decisions are independent; the two coordinators operate without cross-coupling.
- **R-WEB.15.3**: When both systems require approval simultaneously (a tool that trips both a filesystem scope violation and a web domain block), the modals are queued and presented in sequence; each decision is independent.

---

### R-WEB.16: Subprocess egress sandbox (task vault HTTP proxy)

> Design narrative: `docs/egress-sandbox.md`. This section provides the SPEC-level requirements that were previously missing.

The subprocess egress sandbox is a complementary mechanism that covers HTTP traffic from **subprocesses spawned inside a task vault** — not the ahma process itself (which is governed by R-WEB.1–R-WEB.15).

- **R-WEB.16.1**: When `ahma serve` starts with a `--task-vault <path>`, it **must** bind an HTTP proxy to a random localhost port and inject `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY=127.0.0.1,::1,localhost` into the subprocess environment.
- **R-WEB.16.2**: Requests from subprocesses to domains **not** in `egress.allowlist` **must** receive `407 Proxy Authentication Required` (CONNECT / HTTPS) or `403 Forbidden` (plain HTTP). The response **must** be indistinguishable from a real network failure, preventing the agent from detecting the proxy's presence via error content.
- **R-WEB.16.3**: `egress.allowlist` pattern syntax matches R-WEB.4. An empty or absent file means deny all.
- **R-WEB.16.4**: The proxy **must not** decrypt HTTPS traffic (no MITM). CONNECT tunnels are forwarded for approved domains and rejected for unapproved ones.
- **R-WEB.16.5**: The private-range block (R-WEB.3.1) is applied by the proxy regardless of `egress.allowlist` entries.
- **R-WEB.16.6**: QUIC (HTTP/3) connections are not intercepted by an HTTP proxy. For strict subprocess egress, HTTP/3 **should** be disabled in the subprocess environment (`AHMA_NO_HTTP3=1` or equivalent).
- **R-WEB.16.7**: `EgressAllowlist` (Rust API in `ahma_core`) is the canonical type for managing the allowlist file. `EgressClient` (in `ahma_common`) is the canonical HTTP client for enforced requests from the ahma process itself.
- **R-WEB.16.8** (interactive approval, R-NET): When `--restrict-network` (or `[network] restrict`) is on and a subprocess reaches a domain not in `[network] allow`, the proxy **must** raise an MCP `elicitation/create` prompt at the attached peer before denying, offering the same three-tier answer as R-WEB.5 (`once` / `session` / `always`, persisted to `[network].allow`) plus `deny`. Concurrent connections to the same in-flight domain are **not** double-prompted (R-WEB.5's dedup applies identically). When no peer is attached, the client lacks the elicitation capability, the prompt times out, or the human declines, the connection **must** fail exactly as R-WEB.16.2 specifies — indistinguishable from a real network failure. A denied or unanswerable prompt is cancelled rather than left in flight, so a later connection (e.g. once a capable client attaches) may re-ask.

---

### Security invariants summary (R-WEB)

| Invariant | Requirement |
|-----------|------------|
| Private ranges blocked at DNS resolution time, not just pattern match | R-WEB.3.2 |
| Enter/Esc is always Deny in every approval modal | R-WEB.6.3 |
| Persistent grants written only to out-of-sandbox settings file | R-WEB.5.2 |
| Session grants are never serialized to disk | R-WEB.5.1 |
| Redirect targets are independently checked (no inherited approval) | R-WEB.8.1 |
| No path-based filtering (explicitly excluded to avoid false safety) | R-WEB.13 |
| All outbound HTTP tools must use `EgressClient`, not bare `reqwest` | R-WEB.14 |
| Every request is audit-logged regardless of policy | R-WEB.9.1 |
| `never_allow` cannot be overridden by session grants or `always_allow` | R-WEB.2.3 |
| `block_private_ranges` cannot be overridden by any domain pattern | R-WEB.3.1 |

---

## 5. Tool Definition (MTDF Schema)

### 5.1 Basic Structure

```json
{
  "name": "cargo",
  "description": "Rust's build tool and package manager",
  "command": "cargo",
  "enabled": true,
  "timeout_seconds": 600,
  "synchronous": false,
  "subcommand": [
    {
      "name": "build",
      "description": "Compile the current package.",
      "options": [
        { "name": "release", "type": "boolean", "description": "Build in release mode" }
      ]
    },
    {
      "name": "add",
      "description": "Add dependencies to Cargo.toml",
      "synchronous": true
    }
  ]
}
```

### 5.2 Key Fields

| Field | Description |
|-------|-------------|
| `command` | Base executable (e.g., `git`, `cargo`) |
| `subcommand` | Array of subcommands; final tool name is `{command}_{name}` |
| `synchronous` | `true` for blocking, `false`/omit for async (default) |
| `options` | Command-line flags (e.g., `--release`) |
| `positional_args` | Positional arguments |
| `format: "path"` | **CRITICAL**: Any path argument **must** include this for security validation |

### 5.3 Sequence Tools

Sequence tools chain multiple commands into a single workflow:

```json
{
  "name": "rust_quality_check",
  "description": "Format, lint, test, build",
  "command": "sequence",
  "synchronous": true,
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

Set `"tool_type": "livelog"` to turn any long-running log-streaming command into an LLM-powered monitoring tool.

#### Fields

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

#### Pipeline

1. `tools/call` on a livelog tool creates an operation and returns an `operation_id` immediately.
2. A background task spawns `source_command source_args` inside the sandbox (same kernel-enforced scope as normal tools — see R9).
3. Lines from stdout and stderr are accumulated into a chunk.
4. When the chunk reaches `chunk_max_lines` lines **or** `chunk_max_seconds` seconds elapse, the chunk is sent to the LLM with the `detection_prompt`.
5. The LLM responds with `"CLEAN"` (case-insensitive) if no issue is found, or a brief human-readable summary if an issue is detected.
6. On an issue: a `ProgressUpdate::LogAlert` notification is pushed to the MCP client **if** the cooldown window has elapsed since the last alert.
7. The pipeline continues until the source process exits or the client calls `cancel <operation_id>`.

#### Example (Android logcat via Ollama)

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
        "llm_provider": {
            "base_url": "http://localhost:11434/v1",
            "model": "llama3.2"
        },
        "chunk_max_lines": 50,
        "chunk_max_seconds": 30,
        "cooldown_seconds": 60
    }
}
```

A ready-to-use copy is in [`.ahma/android-logcat.json`](.ahma/android-logcat.json).  Usage guide: [docs/live-log-monitoring.md](docs/live-log-monitoring.md).

#### Security note

The `source_command` executes inside the same sandbox scope as all other tools (R9). The LLM endpoint (`llm_provider.base_url`) is an outbound HTTP call originating from the ahma process — use a localhost endpoint (e.g. Ollama) to avoid sending log data to external services, unless that is explicitly intended.

---

### 5.6 Removed tool types (`decompose`, `worker`, `task_tree`)

The `decompose`, `worker`, and `task_tree` MTDF tool types were removed. Their
implementing crates (`ahma_decompose`, `ahma_worker`, and the `ahma_task_tree`
orchestrator) were deleted because nothing in the shipped product dispatched
them — no handler was registered and no example config shipped. The generic
`Extension` tool-type mechanism (a runtime-registered handler resolved from a
tool's `tool_type` string; see `register_extension_handler` /
`get_extension_key`) remains available for out-of-tree handlers. Recover the
removed crates from git history if these roadmap features are revived.

---

### 5.8 Task Vault

Each `ahma vault create <slug>` call produces a directory tree at
`~/.ahma/tasks/<utc-date>-<slug>-<hex>/`:

```
inputs/       — copies of user-provided files (read intent)
workdir/      — kernel sandbox scope root
outputs/      — tool artifacts (HTML reports, CSV exports)
trash/        — staged deletions (two-phase delete)
audit.jsonl   — append-only event log
egress.allowlist — per-task outbound domain allowlist
```

Use `--task-vault <path>` on `ahma serve http` or `ahma serve stdio` to set
the sandbox scope to `<vault>/workdir/` and wire the audit log and trash.

See [docs/security-sandbox.md](docs/security-sandbox.md) for full documentation.

---

## 6. Usage Modes

### 6.1 STDIO Mode (Default)

Direct MCP server over stdio for IDE integration:

```bash
ahma serve stdio
```

Alternatively, standard tool configurations are bundled directly inside the binary. Enable them using the `--tools` flag to activate built-in fallback definitions:
```bash
ahma serve stdio --tools python,git,github,fileutils,simplify,kotlin
```

Note: Core tools (`run_terminal_command`, `await`, `status`, `cancel`) are always available without any flags.

**Tool loading priority**: When an `.ahma/` directory exists (auto-detected or via explicit `--tools-dir`), **all** tool definitions in it are always loaded regardless of bundle flags. Bundle flags (`--tools python`, `--tools simplify`, etc.) additionally activate built-in tool definitions compiled into the binary, serving as **fallbacks** for tools not defined locally. Local `.ahma/` definitions override bundled defaults with the same name. If *no* `.ahma/` directory exists and no `--tools-dir` is given, only bundle-flag tools plus core built-ins are available.

### 6.2 HTTP Bridge Mode

HTTP server proxying to stdio MCP server:

```bash
# Start on default port (3000)
cd /path/to/project
ahma serve http

# Explicit sandbox scope
ahma serve http --sandbox-scope /path/to/project

# Custom port
ahma serve http --port 8080
```

**Endpoints:**

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/mcp` | JSON-RPC requests |
| GET | `/mcp` | SSE stream for notifications |
| GET | `/health` | Health check |
| DELETE | `/mcp` | Terminate session (with `Mcp-Session-Id`) |

### 6.3 CLI Mode

Execute a single tool command:

```bash
ahma --tool_name cargo --tool_args '{"subcommand": "build"}'
```

### 6.4 List Tools Mode

```bash
ahma --list-tools -- /path/to/ahma --tools-dir ./tools
ahma --list-tools --http http://localhost:3000
```

---

## 6.5 Installation, Setup, and Uninstall

### R-SETUP: `ahma setup` Wizard

`ahma setup` installs all integrations (MCP server entries, terminal hooks, agent skills, TLS certificates) into the user's AI tool configurations.  Without flags it runs an interactive wizard; with `--auto` it installs everything silently.

**R-SETUP.1 — Hooks and the MCP server are complementary.** Terminal hooks and the ahma MCP server are **designed to run together** and sandbox different command streams: the MCP server provides the named async tools the agent calls explicitly (`run_terminal_command`, `file-tools`, `git`, …) with output capture and monitoring, while terminal hooks transparently sandbox the shell commands the agent runs through its **native** terminal/Bash tool (which never pass through MCP and would otherwise run unsandboxed). In `auto` mode hooks activate precisely **because** an ahma MCP server is configured. A command is wrapped at most once (already-wrapped commands and MCP tool calls are passed through untouched), so there is no double-execution. `ahma hooks status` therefore reports both being active as an informational note (not a warning), with the only tradeoff being a small per-command sandbox cold-start for the hooks path. Dropping hooks is optional and only advisable when the agent never uses its native terminal.

**R-SETUP.2 — Command timeout.** The default tool/command execution timeout is `600` seconds (`tools.timeout_secs`). It is configurable via the `--timeout` CLI flag (highest priority) or `tools.timeout_secs` in `settings.toml`, and applies uniformly to MCP tool calls and hook-wrapped shell commands; individual tools may shorten it via `timeout_seconds` in their JSON definition.

### R-UNINSTALL: `ahma uninstall` (Symmetric Teardown)

`ahma uninstall` **mirrors `ahma setup`**: same interactive "what / which platforms" prompt sequence (default: all), same flag surface (`--auto`, `--mcp`, `--hooks`, `--skills`, `--binary`, `--platform`, `--purge`, `--dry-run`).

**Invariants:**
- Only Ahma-managed keys and files are removed; other user content in the same config files is always preserved.
- `~/.ahma` data directory (TLS, prompts, settings, logs) is **never** removed unless `--purge` is explicitly passed.
- On Unix, the binary can self-delete; on Windows, manual instructions are printed instead.
- After uninstall, restart instructions are printed for all affected platforms.

### R-LIFECYCLE: Auto-Spawned Bridge Self-Termination

Bridges **auto-spawned** by `ahma serve stdio` (proxy mode) or `ahma tui` automatically self-terminate once no MCP client remains connected:

1. Each auto-spawned bridge is started with `--idle-timeout <N>` (default: `AUTO_SPAWNED_BRIDGE_IDLE_TIMEOUT_SECS = 10`).
2. The idle-timeout checker polls `active_sessions` every second; when the counter is zero for `N` seconds, the bridge calls `terminate_all` and `process::exit(0)`.
3. The bridge also installs a SIGINT/SIGTERM handler that runs `terminate_all`, removes the Unix socket, and exits cleanly.
4. The TUI sends `DELETE /mcp` for its session on quit; the Unix stdio proxy calls `transport.close()` on EOF — both signal the bridge promptly rather than waiting for SSE-drop detection.

**Explicitly-started bridges** (`ahma serve http`, `ahma serve unix`) have no idle timeout by default and remain running until stopped by the user.

#### R-LIFECYCLE.2: Frontend (proxy) Orphan Prevention

The IDE-facing `ahma serve stdio` **frontend** process (which proxies stdin/stdout to the auto-spawned bridge) MUST self-terminate when its client connection is abandoned, so that editors that repeatedly spawn MCP servers without reaping them cannot accumulate orphaned processes:

1. **Stdin EOF** (existing): when the client closes the pipe, the proxy loop exits.
2. **Parent-death watchdog**: the frontend polls `getppid()`; when it is reparented (parent IDE died) it `process::exit(0)`s within a few seconds. This covers the case where the client is hard-killed without closing stdin. (Unix; the detached bridge/daemon are deliberately **not** watched, since they outlive their spawner by design.)
3. **Handshake deadline**: if the client never sends its first message (the `initialize` handshake) within `FRONTEND_HANDSHAKE_DEADLINE_SECS` (default `30`; overridable for tests via `AHMA_FRONTEND_HANDSHAKE_DEADLINE_SECS`), the connection was spawned-and-abandoned and the frontend `process::exit(0)`s. The deadline is disarmed once the first message is forwarded, so a live but idle session is never killed.

These three mechanisms together bound how long any abandoned `ahma serve stdio` can live; none of them affect a healthy, actively-used session.

### R-ISO: Test/Live Endpoint Isolation

> **Problem (confirmed live failure, 2026-07-14).** The proxy, bridge, and daemon rendezvous on machine-global singleton endpoints (`/tmp/ahma.sock`, `~/.ahma/daemon.sock`, the Windows daemon TCP port). Test isolation existed but was opt-in per spawn site (`AHMA_TEST_ISOLATION`, set only by `test_utils::cli::test_command`); harnesses in other crates spawned the real binary without it. A full `cargo nextest run` therefore unlinked the live `/tmp/ahma.sock` while binding test bridges and dispatched a `RunPrompt` to the live daemon hub — tearing down the developer's active MCP session mid-conversation (surfaced to the client as `-32002` then a full server disconnect).

- **R-ISO.1 (fail-closed test detection).** Any ahma process spawned directly or transitively under a test harness MUST resolve private, test-scoped endpoints instead of the machine-global ones. Detection is `ahma_common::test_isolation::spawned_under_test_harness()`: the explicit `AHMA_TEST_ISOLATION` plumbing variable OR the `NEXTEST` variable that `cargo nextest` exports to every test process (inherited by all children), so a spawn site that forgets the explicit variable can no longer reach live endpoints. Per-run endpoint names that parent and child processes must agree on use `NEXTEST_RUN_ID` (not the PID). Test harnesses that spawn the binary SHOULD still set `AHMA_TEST_ISOLATION=1` explicitly (plain `cargo test` sets no distinctive variable).
- **R-ISO.2 (never steal a live socket).** A Unix-socket listener MUST NOT unlink an existing socket file without first probe-connecting it: a successful connection means a live server owns the path and binding MUST fail loudly (naming the conflict and the `--socket-path` remedy); only a refused/absent connection marks the file stale and safe to remove. (The daemon hub's bind-is-the-mutex protocol already satisfies this; the HTTP bridge's Unix listener must too.)
- **R-ISO.3 (remove only what you own).** On shutdown a server MUST remove its socket file only if the path still refers to the socket it bound (device+inode match). If another process has since replaced the path, deleting it would orphan *that* server's live socket.
- **R-ISO.4 (regression tests).** Unit tests MUST pin: harness detection via both variables; refusal to bind over a live socket; stale-socket cleanup; and identity-checked shutdown removal.

### R-SIGN: Binary Code Signing — in-progress (macOS runtime stability; Windows/Linux distribution-only)

> **Status:** `in-progress`. Tracks a confirmed macOS failure mode plus the cross-platform signing posture.
> Done: R-SIGN.2 (atomic staged-rename install in `ahma update`, inode-pinned by regression test), the local-build half of R-SIGN.1 (`codesign --force --sign - --options runtime` on the staged binary during install, best-effort), and R-SIGN.5 in full (bridge classifies a peer's signal death — SIGKILL gets the code-signing/memory-pressure cause and remediation logged at ERROR — the classified cause travels from the exit monitor into the JSON-RPC error answered to in-flight client requests, and the file logger flushes from a panic hook instead of leaking its guard).
> Pending: Developer-ID signing + notarization of release binaries (R-SIGN.1 — blocked on Apple Developer credentials), and the Windows WDAC/SAC verification (R-SIGN.3).

**Problem (macOS / Apple Silicon).** The installed `ahma` binary is `Signature=adhoc, linker-signed` (the cargo default; `TeamIdentifier=not set`). The long-lived MCP server gets `SIGKILL`ed by the kernel with `EXC_BAD_ACCESS · SIGKILL (Code Signature Invalid)` / `termination namespace=CODESIGNING, "Invalid Page"` when its mapped code pages are invalidated — either by a dev rebuild overwriting the in-use binary, or by code-page eviction under the memory pressure of a heavy in-workspace build (e.g. `cargo clippy --all-targets && cargo nextest run`) where ad-hoc page re-validation fails on fault-in. To the MCP client this surfaces as an opaque `Connection closed` mid-operation; SIGKILL leaves no panic, an empty (buffered) `~/.ahma/logs` for the dead session, and often no fresh `.ips` (`ReportCrash` throttles repeats). `codesign --verify --strict` on the file passes — it is the running mapping, not the on-disk file, that is invalidated.

- **R-SIGN.1 (macOS — required).** Release binaries MUST be signed with a stable Developer ID identity and notarized (hardened runtime). Locally built/installed binaries SHOULD be re-signed with a stable signature (`codesign --force --sign - --options runtime`) instead of left linker-ad-hoc. On macOS this is a **runtime-stability** requirement, not merely a Gatekeeper/distribution one.
- **R-SIGN.2 (atomic install — all platforms).** `ahma setup` / `update` / install flows MUST install the binary out-of-place (write a new file, then atomic rename) and never overwrite the inode of a running `ahma`. This removes the "rebuild kills the running server" trigger everywhere.
- **R-SIGN.3 (Windows — distribution-only, verify).** Windows is **not** expected to share the macOS runtime kill: a running `.exe` is locked against in-place replacement (R-SIGN.2's trigger cannot occur), and Authenticode is validated at image load, not re-validated on page fault. Authenticode signing is still wanted for **distribution trust** (SmartScreen/Defender reputation), not runtime stability. **TODO:** confirm no WDAC / Smart App Control / CI-enforcement policy can kill a running, page-evicted process; if one can, promote to a runtime requirement.
- **R-SIGN.4 (Linux — not applicable).** The kernel does not validate ELF code-page signatures, and replacing a running binary keeps the original inode mapped, so neither trigger exists. No signing is required for stability or load. (IMA/EVM appraisal is out of scope unless a specific deployment target enables it.)
- **R-SIGN.5 (fail loud — all platforms).** Independent of signing: when the bridge/proxy observes its server peer die by signal, it MUST surface a specific MCP error (naming the likely code-signing / memory-pressure cause and remediation) instead of a bare `Connection closed`, and the logger MUST flush on abnormal exit (signal/panic hook or synchronous writer) so the final lines survive a SIGKILL.

---

## 7. HTTP Bridge & Session Isolation

### R8: HTTP Bridge & Streamable HTTP

- **R8.1**: HTTP bridge mode via `ahma serve http`.
- **R8.2**: SSE at `/mcp` (GET) for server-to-client notifications.
- **R8.3**: JSON-RPC via POST at `/mcp`.
- **R8.4**: Auto-restart stdio subprocess if it crashes.
- **R8.5**: Content negotiation via `Accept` header (`text/event-stream` → SSE, `application/json` → JSON).
- **R8.6**: **HTTP Streaming (MCP Streamable HTTP)**: POST requests support SSE response streaming for full multiplexing and reconnection resilience.
  - **R8.6.1**: POST with `Accept: text/event-stream` returns SSE-formatted response and interleaved server notifications within a single stream.
  - **R8.6.2**: Per-session SSE event IDs (`id:` field) enable ordering and deduplication. Each JSON-RPC response and notification receives a monotonically-increasing session-unique ID.
  - **R8.6.3**: Event history buffer maintains recent events (bounded to 1000 events per session) for `Last-Event-Id` replay support.
  - **R8.6.4**: GET requests with `Last-Event-Id: N` replay all events with ID > N from the per-session history buffer, enabling seamless reconnection after temporary network loss.
  - **R8.6.5**: Event IDs are independent per session and start at 1. Event history is cleared when the session ends.
- **R8.7**: **HTTP/3 (QUIC) Client Preference**: All HTTP clients built with `reqwest` use the `http3` feature to prefer HTTP/3 (QUIC) transport when the server advertises support via Alt-Svc headers.
  - **R8.7.1**: HTTP/3 uses QUIC (UDP-based) for reduced connection latency and improved multiplexing compared to HTTP/2 over TCP.
  - **R8.7.2**: Transparent fallback to HTTP/2 or HTTP/1.1 when the server does not support HTTP/3.
  - **R8.7.3**: Both SSE and HTTP streaming endpoints work correctly with HTTP/3-capable clients.

### R10: Session Isolation

- **R10.1**: `--session-isolation` flag enables per-session subprocess with own sandbox scope.
- **R10.2**: Session ID (UUID) generated on `initialize`, returned via `Mcp-Session-Id` header.
- **R10.3**: Sandbox scope is resolved per the R5.2 source precedence (explicit → `roots/list` → elicitation → `~/sandbox` default) and committed through the single atomic compare-and-swap of R5.1.1. Scope is owned by the per-workspace instance and shared by all attached sessions (R5.1), not derived independently per session.
- **R10.4**: Once committed, the instance sandbox scope **cannot** be changed (security invariant; R5.1).
- **R10.5**: `roots/list_changed` after sandbox lock is a **tolerated no-op**: the committed instance scope is immutable and can never be widened (R5.1 / R5.1.1 / R5.2.2), so the notification is acknowledged with success, **not** forwarded to the subprocess, and the session is **kept alive**. The server **must not** widen, narrow, or re-derive scope from it, and **must not** terminate the session. (Real clients re-emit `roots/list_changed` routinely; terminating on it caused 403 → stdio-proxy respawn churn. Sandbox escape is prevented by the immutability of the commit, not by tearing down the session.) Any actual scope-widening is rejected at the single commit point (R5.1.1) — there is no second path to widen scope after lock.
- **R10.6**: **Client Response Mapping**: The HTTP bridge MUST keep track of server-to-client JSON-RPC requests (such as `roots/list` and `sampling/createMessage`) by recording their request IDs. When a client sends a JSON-RPC response with a `result` field, the bridge MUST only process it as a `roots/list` response (and lock the sandbox) if its request ID matches an outstanding `roots/list` request. Client responses to other methods (e.g. keepalive pings or sampling) MUST NOT trigger roots-parsing or sandbox-locking logic, and MUST NOT generate invalid roots warnings or errors.
- **R10.7**: **Daemon Chat MCP Base URL Resolution**: When the background daemon (`ahma serve` hub) runs an agent task, the `McpChatConfig` base URL (`base_url`) MUST be resolved to the local MCP bridge server's endpoint (HTTP host/port or Unix socket path as configured in the active service's `AppConfig`) rather than being set to the LLM provider's base URL. This ensures local tool calls (e.g. `read_file`) are routed back to the local MCP bridge.
- **R10.8**: **TUI Window Chat MCP Resolution**: TUI window/subtask LLM tasks MUST be provided with the resolved `McpChatConfig` when running chat tasks to allow proper tool routing and sampling capabilities when requested.


---

## 8. Development Workflow

### 8.0 Documentation Requirements

#### R-DOC: Feature Documentation Contract

Every major feature in ahma **must** have a corresponding page in `docs/` and an entry in `README.md`. This applies to both stable and experimental features.

**R-DOC.1 — Dedicated doc page**: Each major feature **must** have its own `docs/<feature>.md` file with:
- A clear statement of whether the feature is stable or **Experimental** (version introduced).
- A motivating "Why" paragraph explaining the security or usability rationale.
- A practical quickstart with runnable commands or code.
- A reference table of configuration options where applicable.
- A "See also" section linking to related docs and the relevant SPEC.md section.

**R-DOC.2 — README entry**: Each major feature **must** have a brief entry in `README.md` under the appropriate section (stable features) or the "vX.Y Experimental Features" section (new/unstable features). The entry **must** link to the dedicated doc page.

**R-DOC.3 — SPEC.md accuracy**: When a feature's behaviour is changed, the corresponding SPEC.md section and its `docs/<feature>.md` page **must** be updated in the same commit or PR.

**R-DOC.4 — Experimental graduation**: When an experimental feature is stabilised, its doc page **must** remove the "Experimental" notice, update SPEC.md status to `tests-pass`, and move its README entry from the "Experimental" section to the appropriate stable section.

**R-DOC.5 — Removal**: When a feature is removed, its `docs/<feature>.md` **must** be deleted and all README and SPEC.md references **must** be removed in the same commit.

**R-DOC.6 — No orphan docs**: Every file in `docs/` **must** be referenced from at least one of: `README.md`, `SPEC.md`, or another `docs/*.md` file. Orphan documentation is misleading and should not accumulate.

**R-DOC.7 — CLI Help Text Guidelines**: Command-line interface help descriptions **must** follow two strict guidelines:
- **Contiguous Layout**: Descriptions for arguments, flags, and subcommands **must** be written as contiguous blocks of text without blank lines (double carriage returns). Since Clap outputs help text inside lists, internal blank lines disrupt the alignment and layout.
- **Educational Context**: Help text for complex or non-obvious features (e.g., `--task-vault`) **must** be educational. It must explain what the feature is and why/when a user or tool would use it, while remaining concise and precise.

| Feature area | Stable doc | SPEC.md section |
|---|---|---|
| Kernel sandbox | [docs/security-sandbox.md](docs/security-sandbox.md) | R5, R6 |
| Connection modes | [docs/connection-modes.md](docs/connection-modes.md) | §6 |
| Custom tools / MTDF | [docs/custom-tools.md](docs/custom-tools.md) | §5 |
| Live log monitoring | [docs/live-log-monitoring.md](docs/live-log-monitoring.md) | §5.5 |
| Environment variables | [docs/environment-variables.md](docs/environment-variables.md) | — |
| Installation | [docs/installation.md](docs/installation.md) | — |
| Session isolation | [docs/session-isolation.md](docs/session-isolation.md) | R10 |
| Task vaults | [docs/task-vault.md](docs/task-vault.md) | §5.8 |
| Decompose | [docs/decompose.md](docs/decompose.md) | §5.6 |
| TUI | [docs/tui.md](docs/tui.md) | — |
| Egress sandbox | [docs/egress-sandbox.md](docs/egress-sandbox.md) | — |
| Artifacts | [docs/artifacts.md](docs/artifacts.md) | — |
| Worker synthesis | [docs/worker-synthesis.md](docs/worker-synthesis.md) | §5.7 |
| Bundle audit | [docs/bundle-audit.md](docs/bundle-audit.md) | — |
| Cluster scheduler | [docs/cluster-scheduler.md](docs/cluster-scheduler.md) | — |
| Renewal contract | [docs/renewal-contract.md](docs/renewal-contract.md) | — |
| ahma_core library | [docs/ahma-core-library.md](docs/ahma-core-library.md) | — |

### 8.1 Core Principle: Use Ahma

**Always use Ahma** instead of terminal commands:

| Instead of... | Use Ahma tool... |
|---------------|---------------------|
| `run_in_terminal("cargo build")` | `cargo` with `{"subcommand": "build"}` |
| `run_in_terminal("any command")` | `run_terminal_command` with `{"command": "any command"}` |

**Why**: We dogfood our own product. Using Ahma catches bugs immediately, runs faster (no GUI prompts), and enforces sandbox security.

### 8.2 Quality Checks

Before committing, run (via Ahma):

1. `cargo fmt` — format code
2. `cargo nextest run` — run tests
3. `cargo clippy --fix --allow-dirty` — fix lint warnings
4. `cargo doc --no-deps` — verify docs build

### 8.3 Terminal Fallback (Rare)

Only use terminal directly when:

1. **Coverage**: `cargo llvm-cov` — instrumentation incompatible with sandboxing
2. **Ahma completely broken** — fix immediately after recovery

---

## 9. Implementation Constraints

### 9.1 Meta-Parameters

These control execution environment but **must not** be passed as CLI arguments:

- `working_directory`: Where command executes
- `execution_mode`: Sync vs async
- `timeout_seconds`: Operation timeout

### 9.2 Async I/O Hygiene

- **R10.1**: Blocking I/O (`std::fs`) **must not** be used in async functions. Use `tokio::fs` instead.
- **R10.2**: Test code is exempt (blocking acceptable in `#[tokio::test]`).
- **R10.3**: **Child Process Leaks**: All `tokio::process::Command` spawns **must** implement `.kill_on_drop(true)`. By default, dropping a tokio child process future (e.g. from a timeout) orphans the process, leaving it running in the background. This has historically caused catastrophic CLI test hangs in CI. Always explicitly enforce `kill_on_drop`.

### 9.3 Error Handling

- **R11.1**: Use `anyhow::Result` for internal error propagation.
- **R11.2**: Convert to `McpError` at MCP service boundary.
- **R11.3**: Include actionable context in error messages.

### 9.4 Unified Shell Output

- **R12.1**: All shell commands **must** redirect stderr to stdout (`2>&1`).
- **R12.2**: AI clients receive single, chronologically ordered stream.

### 9.5 Cancellation Handling

- **R13.1**: Distinguish MCP protocol cancellations from process cancellations.
- **R13.2**: Only cancel actual background operations, not synchronous MCP tool calls (`await`, `status`, `cancel`).

### 9.6 Concurrency Architecture Principles

#### R18: No-Wait State Transitions

- **R18.1**: State transitions **must never require wait loops or polling**. If code needs to "wait for" another component, the design is fundamentally broken.
- **R18.2**: Use state machines with explicit transitions. When a state change occurs, notify listeners immediately through channels or callbacks.
- **R18.3**: Example anti-pattern:

```rust
// WRONG: Polling for state change
while !session.is_sandbox_ready() {
    sleep(Duration::from_millis(100)).await;
}
```

- **R18.4**: Correct pattern:

```rust
// CORRECT: Explicit state transition notification via watch channel
// In Operation:
let mut rx = op.subscribe_completion(); // tokio::sync::watch::Receiver<bool>
rx.wait_for(|done| *done).await.ok();   // returns immediately if already true
// where subscribe_completion() creates a receiver from watch::Sender<bool>
// stored in Operation::completion_watch
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
- **R20.2**: When state needs to be observed from multiple components, use:
  - Watch channels (`tokio::sync::watch`)
  - Event listeners with guaranteed delivery
  - NOT: multiple copies of state with synchronization attempts

#### R21: Security Against Environment Pollution

- **R21.1**: Production behavior **must not** be controllable via environment variables that an attacker or malicious process could set.
- **R21.2**: Test-only behavior **should** be controlled via:
  - Compile-time features (`#[cfg(test)]`)
  - Explicit CLI parameters (e.g., `--disable-sandbox`)
  - Constructor parameters passed at initialization
- **R21.3**: The following patterns are **FORBIDDEN**:
  - Any different behavior based on automatic "test mode" detection from environment variables like `NEXTEST`, `CARGO_TARGET_DIR`, etc.
  - Any environment variable that bypasses security checks
- **R21.4**: **Environment Variable Minimization**: The system **must** minimize configuration via environment variables to prevent security side-channel attacks and configuration clutter. Configuration parameters **must** be declared on the command line or in explicit configuration structures (`AppConfig`) and passed down through constructor arguments rather than being queried directly from the environment at execution time. The full resolution standard, trust tiers, and env-var retirement schedule are specified in §3.5 (R-CFG), which supersedes any older text that honors `AHMA_*` variables.

#### R22: Visual Minimalism

- **R22.1**: Public communications, including user messages, error logs, and documentation, **must** minimize the use of icons and emojis.
- **R22.2**: Standard ASCII text **should** be used for all status indications and visual cues.
- **R22.3**: Emojis are **forbidden** in source code logs and terminal output unless explicitly required for a specific standardized protocol.

#### R23: State Machine Standard

Any non-trivial lifecycle — anything with three or more states, or where an
invalid combination of flags is currently representable — **must** be modeled as
an explicit state machine rather than ad-hoc `bool`/`Option` fields mutated in
place. This is the concrete mechanism behind R18.2 and R20.

- **R23.1 — Hand-written, no FSM crate.** Ahma deliberately does **not** depend
  on a third-party state-machine crate. The popular options do not fit this
  codebase and adding one would enlarge the audit/supply-chain surface of a
  security product for no benefit:
  - `rust-fsm` (and similar transition-table DSLs) model states as **unit
    variants**; our states carry data (`Active { scopes }`,
    `Completed { output }`, `Configuring { scopes }`) and cannot be expressed.
  - `statig` is an opinionated **event-dispatch framework** that would fight the
    `tokio::sync::watch` observability model R18/R20 require; no FSM crate
    integrates with no-poll watch observation.
  - compile-time `typestate` (state-as-type) cannot be stored in a struct field
    and shared/observed across async tasks behind an `Arc`, which every one of
    our machines needs.

- **R23.2 — Shared building blocks.** State machines are built from the
  primitives in `ahma_common::state_machine`:
  - `FsmState` — every state enum implements it (`name()` for logs/metrics,
    `is_terminal()` for guards), giving one vocabulary across crates.
  - `InvalidTransition` — the typed error a rejected guarded transition returns.
  - `Observable<S>` — a `tokio::sync::watch`-backed single source of truth for
    state shared across tasks. Exposes `current()`/`read()` (non-blocking reads),
    `subscribe()`, guarded `modify()`, and event-driven `wait_until()`. This is
    the generalized engine behind `sandbox_state::SandboxStateMachine`.
  - `StateMachine<S>` — a `Mutex`-plus-closure wrapper for **local** state that
    is not observed across tasks (e.g. OAuth `AuthState`).

- **R23.3 — Shape of a machine.** States are a data-carrying `enum`. Transitions
  are **named, guarded methods** on the owning type (`to_active`,
  `to_failed`, …) that encode their legal predecessor states and return
  `Result<_, InvalidTransition>` (or a domain `Result`). Callers never mutate the
  state field directly. Terminal states are preserved — a transition out of a
  terminal state is rejected, not silently applied.

- **R23.4 — Observability.** Cross-task lifecycles use `Observable` (or another
  `watch`-based channel) so observers react immediately and never poll (R18).
  State has exactly one authoritative location (R20); do **not** keep a shadow
  copy of any field that the machine already owns.

- **R23.5 — Tests.** Every machine tests both the happy-path transition sequence
  and that each illegal transition is rejected (and leaves state unchanged). The
  reference implementation is `ahma_common::sandbox_state`.

- **R23.6 — Exemptions.** Enums used purely as **classifiers** or **strategy
  selectors** (e.g. `GrantStatus`, `ReduceMode`, `TransportMode`) are not
  lifecycles and are exempt; they have no transitions to guard.

#### R24: Live Task Tree (TUI observability)

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
  subscribes to the hub daemon, which replays each instance's retained
  operation history (`OpStarted` + terminal `OpFinished`, bounded per
  instance) before live events. Replayed events carry wall-clock timestamps
  (`started_epoch_ms` / `ended_epoch_ms`) so elapsed/duration displays are
  **true times, not time-since-receipt**. When the replay reveals live work
  for the current project from an attached client, the TUI switches to the
  task view automatically; any user keystroke disarms this auto-switch.

- **R24.3 — Project-scoped by default.** The tree shows instances whose
  sandbox scope covers (or lives inside) the directory the TUI was started in;
  `f` toggles all projects. Matching is component-boundary path containment in
  either direction. Instances with no operations still render (an idle,
  attached client is information, not noise).

- **R24.4 — Compact tree with accordion drill-in.** One line per task:
  instance headers (client identity, transport, scope, and parallel-work
  tallies: running / queued / succeeded / failed), operations beneath them,
  children indented under their parent (session groups, spawned subtasks —
  arbitrary depth). Finished tasks resolve in place to a terminal glyph +
  duration. Enter or click on a task expands it inline into its live output
  tail (running) or historic output/result summary (finished); expanding one
  task collapses the previously expanded one (single-expand accordion).
  Instance and session headers fold/unfold their subtree instead.

- **R24.5 — Field-only wire evolution.** The task-tree protocol additions
  (`parent_id`, `started_epoch_ms`, `ended_epoch_ms` on `DaemonEvent`;
  `client` on `Register`/`InstanceInfo`) are `#[serde(default)]` **field**
  additions — never new message variants — so mixed-version daemon / instance
  / TUI combinations keep interoperating. The MCP client identity
  (`clientInfo.name`, learned at `initialize` — after hub registration) is
  conveyed by the reporter **reconnecting and re-registering**
  (reconnect-to-relabel), which also re-replays state, rather than by a new
  `UpdateInstance` message.

- **R24.6 — One task, one row.** An operation visible both through the hub
  (instance-tagged) and through the TUI's direct MCP status poll (untagged)
  renders once; the hub copy wins because it carries instance grouping.

- **R24.7 — One operation identity, computed at the source.** An operation's
  human-meaningful name is **data on the wire**, not a string an observer
  reverse-engineers. Observers **must not** derive an operation's name from its
  id, its description prose, or any other naming convention — the historical
  failure (rows reading `op_41_echo_hello`, or a bare tool name) is a *data*
  defect, and no formatter can repair data that was never sent.
  - `DaemonEvent::OpStarted` carries `title` (a human command summary computed
    **server-side**, which is the only place that knows the command), plus
    `cwd`, the full `command`, and `origin` — which attached session initiated
    the work (`cursor` | `claude-code` | `tui` | `cli` | `hook` | …).
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

---

## 10. Testing Philosophy

### 10.1 Core Principles

- **R14.1**: All new functionality **must** have tests.
- **R14.2**: Tests should be: Fast (<100ms), Isolated, Deterministic, Documented.
- **R14.3**: Bug fixes **must** include a regression test.
- **R14.4**: Prefer in-memory unit tests over subprocess E2E tests. A test that validates static configuration, schema generation, path security, tool dispatch, argument parsing, async operation lifecycle, or any pure logic **must not** spawn an OS process. Only use `ClientBuilder`/`spawn_http_bridge` when the test specifically validates binary wiring or CLI flag behaviour that cannot be exercised via the in-process API.
- **R14.5**: Tests **must not** depend on an external Python runtime (`python3`, `pip`, or any `.py` script). Python is a supported _execution target_ for worker synthesis, but CI test suites assume only a Rust toolchain is present. Use Rust-native equivalents in tests; if a feature requires Python at runtime, make the test conditional and document the external prerequisite explicitly.

### 10.1.1 The Test Pyramid

This project follows a strict test pyramid to keep CI stable on 2-core GitHub Actions runners. Every subprocess spawned by a test consumes an OS thread *and* process-scheduler slots. When 20+ tests run concurrently, IPC pipe back-pressure causes handshake timeouts — these look like logic bugs but are infrastructure failures.

| Layer | Tool | When to use | Execution time |
|-------|------|-------------|----------------|
| **Unit** (preferred) | Direct API calls, `#[cfg(test)]` modules | Logic, schema generation, config parsing, state machines | <5 ms |
| **Integration (in-process)** | `create_in_process_mcp_from_dir()` / `create_in_process_mcp_with_scope()` | MCP protocol logic, tool dispatch, path security, argument parsing, async operations | <50 ms |
| **E2E (subprocess)** | `ClientBuilder`, `spawn_http_bridge` | Binary wiring, CLI flags, cross-binary IPC | 1–5 s |

**Decision rule**: _Can this test be written without spawning a process?_ If yes, write it that way. `ClientBuilder` and `spawn_http_bridge` are reserved for the E2E layer.

**⚠️ Warning**: `setup_mcp_service_with_client()` is a **subprocess wrapper** (it calls `start_process_with_args`), not an in-process helper. Using it for integration tests causes CI timeouts on 2-CPU runners.

### 10.1.2 Choosing the Right In-Process Helper

Both helpers live in `ahma_mcp::test_utils::in_process`:

| Helper | Sandbox | Use when |
|--------|---------|----------|
| `create_in_process_mcp_from_dir(tools_dir)` | `Sandbox::new(Test)` — **path validation ENFORCED** | Tool dispatch, arg parsing, async lifecycle, schema tests |
| `create_in_process_mcp_with_scope(tools_dir, scopes)` | `Sandbox::new(Strict)` — **path validation ENFORCED** | Tests that assert a path or symlink is **rejected** |

Both helpers strictly enforce path validation since `new_test` and validation bypasses have been removed. Tests must ensure that input files and working directories are correctly scoped.


### 10.2 Test File Isolation (CRITICAL)

- **ALL tests MUST use temporary directories** via `tempfile` crate.
- **NEVER** create test files directly in repository structure.
- `TempDir` automatically cleans up on drop.

```rust
use tempfile::tempdir;

let temp_dir = tempdir().unwrap();
let test_file = temp_dir.path().join("test.txt");
fs::write(&test_file, "test content").unwrap();
```

### 10.3 CLI Binary Integration Tests

- All binaries (`ahma`, `generate-tool-schema`) **must** have integration tests.
- Tests in `ahma/tests/cli_binary_integration_test.rs`.
- Cover: `--help`, `--version`, basic functionality.

### 10.4 Test Utilities - Prevent Code Duplication

**R-TEST-PATH**: All binary path resolution in tests **MUST** use centralized helpers:

- **R-TEST-PATH.1**: Use `ahma_mcp::test_utils::cli::get_binary_path(package, binary)` to get binary paths
- **R-TEST-PATH.2**: Use `ahma_mcp::test_utils::cli::build_binary_cached(package, binary)` for builds with caching
- **R-TEST-PATH.3**: **NEVER** manually access `std::env::var("CARGO_TARGET_DIR")` outside of `test_utils::cli`

**Why**: CI environments may set `CARGO_TARGET_DIR` to relative paths (e.g., `target`). The centralized helpers correctly resolve these relative to the workspace root. Manual path resolution duplicates this logic and inevitably introduces bugs.

**Enforcement**: See `scripts/lint_test_paths.sh` for automated detection of violations.

### 10.5 CI-Resilient Testing Patterns

**R15**: Tests must pass reliably in CI environments with concurrent test execution.

#### R15.1: Avoid Race Conditions in Async Testresults, prefer either **synchronous tool execution** (`synchronous: true`) or the `await` tool. Notifications are best-effort; use the `await` tool for reliable result retrieval.
- **R15.1.3**: Use generous timeouts (10+ seconds) for async
- **R15.1.1**: Never use `tokio::select!` to race response completion against notification reception. When the response branch wins, the transport may already be closing.
- **R15.1.2**: For stdio MCP tests that verify results, prefer either **synchronous tool execution** (`synchronous: true`) or the `await` tool. Notifications are best-effort; use the `await` tool for reliable result retrieval.
- **R15.1.3**: Use generous timeouts (10+ seconds) for async waiting. CI environments are slower and more variable than local development.

#### R15.2: Test Timeout and Polling Guidelines

- **R15.2.1**: Never use fixed `sleep()` to wait for async conditions. Use `wait_for_condition()` from `test_utils`.
- **R15.2.2**: For health checks and server readiness, poll with increasing backoff instead of fixed delays.
- **R15.2.3**: When testing notifications or async events, use channel-based communication with explicit timeouts.

#### R15.3: Stdio Transport Gotchas

- **R15.3.1**: Async operation **results** no longer rely on transport delivery. `OperationMonitor` stores results via `tokio::sync::watch` channel; `wait_for_operation()` is race-free (watch stores the current value, so a late subscriber sees `true` immediately). The old `Arc<Notify>` + `wait_for_history_propagation_pub` polling hack has been removed. Push notifications remain best-effort for progress updates
- **R15.3.2**: The `handle_notification` callback is only invoked when rmcp's internal reader successfully parses and delivers the notification. Transport teardown can prevent this.
- **R15.3.3**: For notification tests, consider using HTTP mode with SSE instead of stdio - SSE keeps the notification stream open independently.
- **R15.3.4**: Async operation **results** no longer rely on transport delivery. `OperationMonitor` stores results via `tokio::sync::watch` channel; `wait_for_operation()` is race-free (watch stores the current value, so a late subscriber sees `true` immediately). The old `Arc<Notify>` + `wait_for_history_propagation_pub` polling hack has been removed. Push notifications remain best-effort for progress updates.

#### R15.4: Coverage Overhead Mitigation

- **R15.4.1**: `llvm-cov` instrumentation significantly slows down execution (10x-20x), especially for process-heavy tests like stdio integration.
- **R15.4.2**: Integration tests involving child processes or networks **must** use generous timeouts (30s+). A 10s timeout that works in `release` mode will reliably fail in `coverage` mode.
- **R15.4.3**: Flaky failures that occur ONLY in coverage CI jobs almost always indicate timeouts being too tight for the instrumented binary overhead.

#### R15.5: Dual-Transport Test Coverage (HTTP Bridge)

The HTTP bridge exposes a single `/mcp` POST endpoint whose response format is content-negotiated via the `Accept` header:

| `Accept` value         | Handler                                  | Response                                    |
|------------------------|------------------------------------------|---------------------------------------------|
| `application/json`     | `handle_session_isolated_request`        | Single JSON-RPC response body               |
| `text/event-stream`    | `handle_session_isolated_request_sse`    | SSE stream: notifications + response event  |

**Requirement**: Every test that exercises tool execution (i.e. calls `tools/call` or `tools/list`) MUST cover BOTH response modes.

**Implementation pattern** — extract the test body into a shared `async fn run_<case>(mode: TransportMode)`, then add two `#[tokio::test]` entry points:

```rust
async fn run_my_tool_test(mode: TransportMode) {
    let Some((_server, mcp)) = setup_test_mcp(mode).await else { return; };
    // ... assertions ...
}

#[tokio::test]
async fn test_my_tool_json() { run_my_tool_test(TransportMode::Json).await; }

#[tokio::test]
async fn test_my_tool_sse()  { run_my_tool_test(TransportMode::Sse).await; }
```

**Naming convention** — append `_json` / `_sse` suffix to every test entry point that covers a specific transport mode.  Do NOT use these suffixes for tests that are transport-agnostic (e.g. pure protocol handshake tests, session lifecycle tests, or SSE-specific protocol tests such as event-ID replay).

**Infrastructure** — use `common::setup_test_mcp(mode)` (defined in `tests/common/mod.rs`).  This spawns a fresh server, completes the full MCP handshake including roots exchange, and returns an `McpTestClient` configured with the requested `TransportMode`. The client's `send_request()` / `call_tool()` / `list_tools()` methods automatically use the correct `Accept` header.

**Exemptions** — the following test files are transport-specific by design and do NOT need `_json` / `_sse` variants:
- `sse_streaming_test.rs` — validates POST SSE content-negotiation, event IDs, Last-Event-Id replay
- `sse_endpoint_test.rs`  — validates GET `/mcp` SSE notification stream and event structure
- `handshake_*.rs`       — validates session handshake protocol invariants
- `sandbox_*.rs`         — validates sandbox gating rules

**Concurrency limits** — all test files using `setup_test_mcp` spawn one server per test function.  They MUST be listed in the `threads-required = 2` override filter in the `[profile.ci.overrides]` section of `.config/nextest.toml` to prevent resource storms on GitHub Actions' 2-CPU runners (where `test-threads = "num-cpus"` = 2 and `threads-required = 2` together allow only one such test to run at a time).  The `[profile.default]` section intentionally omits `threads-required` so that local developer machines (e.g. an M4 Ultra with many cores) run tests with full parallelism.  The CI profile is activated explicitly via `cargo nextest run --profile ci` in `build.yml`; plain `cargo nextest run` always uses the default profile.

### 10.6 Testing Patterns and Helpers

> [!IMPORTANT]
> **ALL** integration tests MUST use the centralized helpers in `ahma/src/test_utils.rs`. Do NOT reinvent spawn logic, HTTP clients, or project scaffolding.

#### R16.1: Project Scaffolding (`test_utils::test_project`)
Use `create_rust_test_project` for all tests that need a filesystem. This ensures isolated unique directories via `tempfile` and no repository pollution.

#### R16.2: MCP Service Helpers
- **In-process (preferred)**: Use `create_in_process_mcp_from_dir(tools_dir)` for MCP protocol logic, tool dispatch, and argument-parsing tests — no subprocess, full MCP handshake, runs in <50 ms. Use `create_in_process_mcp_with_scope(tools_dir, scopes)` when the test must assert that a path or symlink is **rejected** (strict sandbox mode).
- **HTTP**: Use `spawn_http_bridge()` and `HttpMcpTestClient` for HTTP/SSE integration testing.
- **Subprocess (E2E only)**: `setup_mcp_service_with_client()` spawns a real subprocess; reserve it for tests that specifically validate binary wiring or CLI flags.

#### R16.3: Binary Resolution
Always use `cli::build_binary_cached()` to avoid redundant `cargo build` calls and ensure tests are fast and CI-friendly.

#### R16.4: Concurrent Test Helpers (`test_utils::concurrent_test_helpers`)

**Purpose**: Safe patterns for testing concurrent operations.

```rust
use ahma_mcp::test_utils::concurrent_test_helpers::*;

// Spawn tasks that start simultaneously
let results = spawn_tasks_with_barrier(5, |task_id| async move {
    // All tasks start at the exact same instant
    perform_operation(task_id).await
}).await;

// Verify no duplicates
assert_all_unique(&results);

// Bounded concurrency for resource-limited CI
let results = spawn_bounded_concurrent(items, 4, |item| async move {
    process(item).await
}).await;
```

**Why**: AI-generated concurrent tests often have subtle race conditions. Barriers ensure deterministic starts; bounded spawning prevents OOM.

#### R16.4: Timeout and Polling (`test_utils::concurrent_test_helpers`)

**Purpose**: CI-resilient waiting patterns.

```rust
use ahma_mcp::test_utils::concurrent_test_helpers::*;

// Wrap operations with clear timeout errors
let result = with_ci_timeout(
    "operation completion",
    CI_DEFAULT_TIMEOUT,
    async { monitor.wait_for_operation("op-1").await }
).await?;

// Wait with exponential backoff (more efficient)
wait_with_backoff("server ready", Duration::from_secs(10), || async {
    health_check().await.is_ok()
}).await?;
```

**Why**: Fixed `sleep()` is flaky on variable CI. Timeouts provide clear diagnostics when things hang.

#### R16.5: Async Assertions (`test_utils::async_assertions`)

**Purpose**: Assert timing behavior in async tests.

```rust
use ahma_mcp::test_utils::async_assertions::*;

// Assert operation completes in time
let result = assert_completes_within(
    Duration::from_secs(5),
    "quick operation",
    async { fetch_data().await }
).await;

// Assert condition becomes true
assert_eventually(
    Duration::from_secs(10),
    Duration::from_millis(100),
    "operation becomes complete",
    || async { monitor.is_complete("op-1").await }
).await;
```

**Why**: Standard assertions don't work with async conditions. These provide clear failure messages.

### 10.7 CI Anti-Patterns to Avoid

**R17**: Avoid these patterns that reliably cause CI failures but may work locally.

| Anti-Pattern | Problem | Solution |
|-------------|---------|----------|
| `tokio::time::sleep(Duration::from_secs(1))` | Flaky on slow CI runners | Use `wait_for_condition()` or `wait_with_backoff()` |
| `tokio::select!` racing response vs notification | Transport teardown wins | Use synchronous mode for notification tests |
| `std::fs::create_dir("./test_dir")` | Pollutes repo, conflicts between tests | Use `tempdir()` or `test_project::create_rust_test_project()` |
| `Command::new("cargo").arg("build")` | Slow, skips cached binaries | Use `cli::build_binary_cached()` |
| Spawning 100+ concurrent tasks | OOM on CI, thread exhaustion | Use `spawn_bounded_concurrent()` |
| Expecting notification order | Async execution order is undefined | Collect notifications, assert set membership |
| Hard-coded ports | Port conflicts with parallel tests | Use port 0 for auto-assignment |
| Shared mutable state without locks | Data races under concurrent tests | Use `Arc<Mutex<_>>` or channels |

#### R17.1: Example Anti-Pattern vs Correct Pattern

FAIL **WRONG**: Fixed sleep for operation completion
```rust
async fn test_operation_completes() {
    let op_id = start_operation().await;
    tokio::time::sleep(Duration::from_secs(2)).await;  // Flaky!
    assert!(is_complete(&op_id));
}
```

OK **CORRECT**: Condition-based waiting
```rust
async fn test_operation_completes() {
    let op_id = start_operation().await;
    wait_with_backoff("operation complete", Duration::from_secs(10), || async {
        is_complete(&op_id).await
    }).await?;
    // Now we know it's complete
}
```

FAIL **WRONG**: Creating files in repo directory
```rust
let f = File::create("test.txt"); // WRONG
```

OK **CORRECT**: Using temp directory
```rust
let t = tempdir();
let f = File::create(t.path().join("test.txt")); // OK
```

### 10.8 Platform-Aware Timeouts

**R18**: All test timeouts **must** use the `ahma_common::timeouts` module for platform-aware scaling.

#### R18.1: Problem Statement

Windows CI runners are 3-5x slower than Linux/macOS for:
- Process spawning and stdio communication
- File system operations (especially temp directories)
- Network socket operations
- PowerShell startup (vs bash)

Hardcoded timeouts that work locally on macOS/Linux will reliably fail on Windows CI, leading to "whack-a-mole" fixes across the codebase.

#### R18.2: Solution - Centralized Timeout Utility

The `ahma_common::timeouts` module provides:

```rust
use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};

// Use semantic categories with platform-appropriate defaults
let timeout = TestTimeouts::get(TimeoutCategory::Handshake);  // 60s base, 4x on Windows

// Scale custom durations
let custom = TestTimeouts::scale_secs(5);  // 5s base, 20s on Windows

// Platform-appropriate polling interval
let interval = TestTimeouts::poll_interval();  // 100ms on Unix, 500ms on Windows
```

#### R18.3: Timeout Categories

| Category | Base (Unix) | Windows | Coverage Mode | Purpose |
|----------|-------------|---------|---------------|---------|
| `ProcessSpawn` | 30s | 120s | 240s | Binary loading, process startup |
| `Handshake` | 60s | 240s | 480s | MCP initialize + roots exchange |
| `ToolCall` | 30s | 120s | 240s | Individual tool execution |
| `SandboxReady` | 60s | 240s | 480s | Post-roots sandbox activation |
| `HttpRequest` | 30s | 120s | 240s | HTTP request/response cycle |
| `SseStream` | 120s | 480s | 960s | SSE stream operations |
| `HealthCheck` | 15s | 60s | 120s | Server health polling |
| `Cleanup` | 10s | 40s | 80s | Test cleanup operations |
| `Quick` | 5s | 20s | 40s | Sub-second operations |

#### R18.4: Migration Requirements

- **R18.4.1**: New tests **must** use `TestTimeouts` instead of hardcoded `Duration::from_secs()`.
- **R18.4.2**: Existing tests with Windows CI failures **should** be migrated to `TestTimeouts`.
- **R18.4.3**: When adding delays after async operations (e.g., post-SSE exchange), use `TestTimeouts::short_delay()`.
- **R18.4.4**: Polling loops **must** use `TestTimeouts::poll_interval()` instead of hardcoded intervals.

#### R18.5: Why Platform Multipliers

The 4x multiplier for Windows is based on empirical CI data:
- Windows GitHub Actions runners have ~4x slower process spawn times
- PowerShell startup is ~3x slower than bash
- Windows temp directories have higher latency than Linux tmpfs
- Coverage mode (`llvm-cov`) adds another 2x overhead

The multipliers stack: Windows + Coverage = 8x base timeout.

### 11.1 Canonical Reuse Patterns

These rules codify the architecture simplification strategy: isolate repetitive protocol/setup
details behind shared helpers so core execution algorithms remain easy to read.

#### R19: Production Helper Patterns

- **R19.1**: MCP handlers that return a single text response **should** use
  `mcp_service::handlers::common::text_result(...)` instead of inlining
  `CallToolResult::success(vec![Content::text(...)])`.
- **R19.2**: Common MCP error constructors without extra data **should** use
  `mcp_service::handlers::common::{mcp_internal, mcp_invalid_params}`.
- **R19.3**: JSON argument extraction in MCP handlers **should** use
  `mcp_service::handlers::common::{require_str, opt_str}` where applicable.
- **R19.4**: Tool-call readiness checks **must** use
  `sandbox::Sandbox::is_ready_for_tool_calls()` instead of duplicating
  `scopes().is_empty() && !is_test_mode()` checks.
- **R19.5**: Built-in tool input schemas (`await`, `status`, `run_terminal_command`)
  **must** be generated with `mcp_service::schema` helper builders
  (`string_property`, `path_property`, enum helpers, `object_input_schema`).

#### R20: Test Harness Reuse Patterns

- **R20.1**: HTTP bridge tool tests **should** use `tests/common/setup_test_mcp_for_tools(...)`
  for setup + required-tool gating, rather than open-coding availability checks.
- **R20.2**: Reusable assertions in HTTP bridge tests **should** use
  `tests/common/assert_tool_success_with_output(...)` where output is required.
- **R20.3**: Tests that need tempdir + `.ahma` tools dir + MCP client **should** use
  `ahma_mcp::test_utils::client::McpClientFixture`.
- **R20.4**: Integration tests with custom bridge startup parameters **should** use
  `tests/common/server::spawn_server_guard_with_config(...)` instead of duplicating
  process startup/port/health polling code.
- **R20.5**: Timeout values in integration tests **must** use `TestTimeouts` categories or
  scaling helpers; numeric `Duration::from_secs(<literal>)` / `from_millis(<literal>)`
  should only be used in narrowly justified micro-timing helpers.

#### R21: Guardrail Enforcement

- **R21.1**: Guardrail scripts **must** reject deprecated HTTP bridge test helper usage
  (`ensure_server_available` outside `common/sse_test_helpers.rs`).
- **R21.2**: Guardrail scripts **must** reject newly added literal `Duration::from_secs(...)`
  / `Duration::from_millis(...)` patterns in timeout-sensitive handshake/bridge integration tests.
- **R21.3**: Guardrail scripts **should** verify that custom HTTP bridge integration tests
  use shared startup helpers from `tests/common/server.rs`.

### 11.2 Recurring Failure Mode Detection

This repo has a recurring failure mode: tests can pass while real-world usage is broken.

---

## 12. Feature Requirements by Module

### 12.1 ahma

| Feature | Status | Description |
|---------|--------|-------------|
| Adapter execution | PASS | Sync/async CLI tool execution |
| MCP ServerHandler | PASS | Complete MCP protocol implementation |
| Shell sessions | PASS | Persistent PTY sessions (`session_id`); prewarmed pool removed as dead code |
| Linux sandbox | PASS | Landlock enforcement |
| macOS sandbox | PASS | Seatbelt/sandbox-exec enforcement |
| Nested sandbox detection | PASS | Detect outer sandboxes |
| Operation monitor | PASS | Track async operations |
| Callback system | PASS | Push completion notifications |
| Config loading | PASS | MTDF JSON parsing |
| Schema validation | PASS | Validate at startup |
| Sequence tools | PASS | Multi-command workflows |
| Hot-reload | PASS | Watch tools directory |

### 12.2 ahma-http-bridge

| Feature | Status | Description |
|---------|--------|-------------|
| HTTP-to-stdio bridge | PASS | Proxy JSON-RPC to subprocess |
| SSE streaming | PASS | Server-sent events for notifications |
| Session isolation | PASS | Per-session sandbox scope |
| Auto-restart | PASS | Restart crashed subprocess |
| Health endpoint | PASS | `/health` monitoring |
| Session termination | PASS | DELETE with `Mcp-Session-Id` |

### 12.3 ahma-http-mcp-client

| Feature | Status | Description |
|---------|--------|-------------|
| HTTP transport | PASS | POST requests with Bearer auth |
| SSE receiving | PASS | Background task for server messages |
| OAuth 2.0 + PKCE | PASS | Browser-based auth flow |
| Token storage | PASS | Persist to temp directory |
| Token refresh | PLANNED | Auto-refresh expired tokens |

### 12.4 ahma --validate

| Feature | Status | Description |
|---------|--------|-------------|
| MTDF Validation | PASS | Validate tool configs against JSON schema via `ahma --validate` |
| Error reporting | PASS | Concise, actionable error messages |

---

## 13. CI Caching Strategy

To maintain high performance and avoid cache bloat, the following strategies are employed in GitHub Actions:

### 13.1 Daily Rotation
- **R13.1.1**: All caches **must** use a daily rotating key (e.g., `...-day${{ steps.day-number.outputs.day }}`) to ensure they contain only current files and do not grow indefinitely.
- **R13.1.2**: `restore-keys` **must** be used to fall back to the most recent previous cache (from earlier in the day or a previous day).

### 13.2 Distributed Caching (sccache)
- **R13.2.1**: **sccache** **must** be used as the compiler wrapper across all macOS and Linux CI jobs. Windows CI is exempt from sccache and instead relies on plain Cargo target caching.
- **R13.2.2**: The **GitHub Actions Backend** (`SCCACHE_GHA_ENABLED: "true"`) **must** be used for `sccache` on macOS/Linux to allow atomic uploads of object files directly to the GHA cache API.
- **R13.2.3**: Windows CI is exempt from sccache requirements, and compiles without a compiler wrapper.
- **R13.2.4**: Each CI job **must** use unique `SCCACHE_GHA_CACHE_TO` keys to prevent concurrent write conflicts. Key format: `sccache-{OS}-{ARCH}-{JOB}-day{DAY}`.
- **R13.2.5**: Each CI job **must** use `SCCACHE_GHA_CACHE_FROM` with comma-separated fallbacks to enable cache sharing between related jobs on the same platform.
- **R13.2.6**: Debug-profile jobs on the same platform (clippy, nextest, android, coverage) **should** include each other in their `CACHE_FROM` lists since they produce compatible cache entries.
- **R13.2.7**: Release-profile jobs **must not** include debug caches in `CACHE_FROM` since `--release` flag produces incompatible cache entries.

### 13.3 Cargo Registry Caching
- **R13.3.1**: The Cargo registry (`~/.cargo/registry`) and git database (`~/.cargo/git`) **must** be cached using `actions/cache` or specialized actions, adhering to the Daily Rotation rule.

### 13.4 GitHub Actions Versioning
- **R13.4.1**: GitHub Actions **must** be referenced by version tags (e.g. `@v6`, `@v5`) rather than full commit hashes, to ensure readability, maintainability, and automatic receipt of minor version updates and security patches.
- **R13.4.2**: Workflows **must** be updated to target the latest available major versions of each respective action.

---

## 13. Build & Development

### 13.1 Prerequisites

```bash
# Rust 1.93+ required
rustup update stable

# Build
cargo build --release

# The binary will be at target/release/ahma
```

### 13.2 mcp.json Configuration

```json
{
  "servers": {
    "Ahma": {
      "type": "stdio",
      "cwd": "${workspaceFolder}",
      "command": "/path/to/ahma/target/release/ahma",
      "args": []
    }
  }
}
```

### 13.3 Quality Checks

> **CRITICAL for AI Assistants:** Run all checks and ensure they pass **before stopping work**.

```bash
cargo fmt                           # Format code
cargo clippy --all-targets          # Check for lints (must pass)
cargo build --release               # Verify build succeeds
cargo nextest run                   # Run all tests (must pass)
```

### 13.4 Test-First Development (TDD)

> **MANDATORY for all new features and bug fixes:**

**R13.4.1**: **ALL** functional requirements and bug fixes **MUST** follow test-first development:

1. **Write the test first** - Write a test that expresses the desired behavior or exposes the bug
2. **See it fail** - Run the test and verify it fails for the expected reason
3. **Implement the fix** - Write the minimal code to make the test pass
4. **See it pass** - Run the test and verify it passes
5. **Refactor** - Clean up the code while keeping tests green

**R13.4.2**: This workflow is **non-negotiable** and applies to:
- New features (e.g., auto-detection of `.ahma` directory)
- Bug fixes (any deviation from expected behavior)
- Performance improvements (when testable)
- Security enhancements (when testable)

**R13.4.3**: Tests are **part of the functional requirements**, not an afterthought.

**R13.4.4**: Code changes without corresponding tests **MUST NOT** be merged unless:
- The change is purely documentation
- The change is a trivial typo fix in comments
- Tests are genuinely impossible (must be justified in code review)

**R13.4.5**: Before considering any work complete, you **MUST** run these quality checks in order:
1. `cargo clippy` - Verify no warnings or errors
2. `cargo nextest run` (preferred) or `cargo test` - Verify all tests pass
3. Only after both pass can work be considered complete

This ensures:
- Code quality and idiomatic Rust patterns (clippy)
- No regressions in functionality (nextest)
- Early detection of issues before they are merged

**Failing to run these checks results in broken builds and wasted time.**

---

## 14. Maintenance Notes

> **AI Assistants:** When you modify code or discover issues:
>
> 1. Update the "Quick Status" table
> 2. Add to "Known Issues" if new bugs found
> 3. Update feature tables with status changes
> 4. **BEFORE stopping work: Run `cargo clippy` then `cargo nextest run` to verify quality`
> 5. If you change CLI flags, env vars, tool bundles, connection modes, or sandbox behavior,
>    update `skills/ahma/SKILL.md` to keep the agent skill current.

**Last Updated**: 2026-01-18

**Status**: Living Document - Update with every architectural decision or significant change

---

## 15. Agent Skills

This section specifies requirements for the AI agent skill files (`SKILL.md`) bundled with
Ahma. Skills are machine-readable guides that help AI coding assistants use Ahma effectively.

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
snippets rather than prose paragraphs. Link to `docs/` for deep dives.

### R-SK4 — Required sections (ahma skill)

`skills/ahma/SKILL.md` MUST include these sections (any order):

| Section | Content |
|---------|---------|
| Quick Start | mcp.json setup for VS Code, Cursor, Claude Code |
| Tool Bundles | Table of all bundles, how to activate, when to use |
| Built-in Tools | `run_terminal_command`, `status`, `await`, `cancel` with examples |
| Async Workflow | Operation ID pattern, status/await/cancel usage |
| Sandbox | Scope rules, `--tmp`, env vars, nested sandbox note |
| Key Env Vars | Quick-reference table with link to full reference |
| CLI Reference | `serve`/`tool` subcommand synopsis |
| Troubleshooting | Common errors and fixes |

### R-SK5 — Currency requirement

Skills are **living documents**. When any of the following change, the relevant skill MUST be
updated in the same PR or commit:

- CLI flags or subcommands (`ahma_mcp/src/shell/cli.rs`)
- Environment variables (`ahma_mcp/src/config/`)
- Tool bundle names or contents (`ahma_mcp/src/mcp_service/bundle_registry.rs`)
- Built-in tool signatures (`run_terminal_command`, `status`, `await`, `cancel`)
- Connection modes or HTTP endpoints (`ahma_http_bridge/`)
- Sandbox scope semantics (`ahma_core/src/sandbox/`)
- Live-log monitoring configuration

### R-SK6 — CI / pre-commit validation

The skill symlink MUST resolve at the repo root. A CI or pre-push check MUST assert:

```bash
# Cross-platform (macOS readlink does not support -f)
test -L .agents/skills/ahma/SKILL.md && cat .agents/skills/ahma/SKILL.md > /dev/null
```

If the symlink does not exist or does not resolve, the check fails.

### R-SK7 — No duplication with AGENTS.md

`skills/ahma/SKILL.md` targets **AI agents using Ahma**. `AGENTS.md` targets **AI contributors
developing Ahma**. Do not copy developer-only content (testing rules, cross-platform checklist,
commit format) into the skill, and do not copy agent usage recipes into AGENTS.md.

---

## 16. Future Work

### v0.8 — Discovery, Observability, Economics

| Area | Item | Notes |
|------|------|-------|
| Cluster | **mDNS peer discovery** (`mdns-sd` crate, `_ahma-worker._tcp.local`) | ✓ Completed |
| Cluster | **Named provider refs in tool files** (`llm_provider_ref: "ollama-local"`) | Avoids duplicating connection details across tool definitions |
| UX | **`ratatui` TUI** — real-time task dashboard | ✓ Completed (full ratatui TUI implemented) |
| Economics | **Cost metering** — track token counts + estimated cost per tool call | Aggregate by provider; expose via `ahma tool info --cost-summary` |
| Security | **Signed bundle index** (`bundle-index.json` with HMAC-SHA256 over manifest) | Prevent silent tampering with downloaded bundles |
| Security | **`--require-token` key rotation** — reload token from file on SIGHUP | Zero-downtime key rotation for long-running HTTP bridge instances |

### v0.9 — Scheduling Refinement, Keyring

| Area | Item | Notes |
|------|------|-------|
| Cluster | **Weighted scheduling** — factor GPU model, RAM, historical latency into `load_score_for` | Better affinity for large models |
| Cluster | **`cluster remove` subcommand** — remove a peer from `peers.json` by ID | ✓ Completed |
| Security | **OS keyring integration** (`keyring` crate) — store API keys in system credential store instead of env vars | macOS Keychain, GNOME Secrets, Windows Credential Manager |
| Config | **Encrypted secrets at rest** in `~/.ahma/config.toml` (age encryption) | Fallback when OS keyring is unavailable |

