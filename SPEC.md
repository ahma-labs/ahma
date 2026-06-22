# Ahma Requirements

> **For AI Assistants:** This is the **single source of truth** for the project. Always read this before making changes. Update this file when requirements change, bugs are discovered, or implementation status changes.

## Quick Status

| Component | Status | Notes |
|-----------|--------|-------|
| Core Tool Execution | tests-pass | `ahma` adapter executes CLI tools via MTDF JSON |
| Async-First Operations | tests-pass | Operations return `id`, push results via MCP notifications |
| Shell Pool | in-progress | Pool prewarms shells but is NOT wired into the async hot path — commands spawn directly (~6ms median measured by `latency_guard_test`); decide: wire in or remove |
| Unified Operation Event Stream | tests-pass | Single `OperationEvent` stream (`ahma_common::event_dispatcher`); `OperationMonitor` is the sole lifecycle emitter; subscribers: MCP progress push, daemon hub, vault audit, TUI |
| Output Spill Files | tests-pass | Complete per-operation output at `<log dir>/operations/<id>.log`; advertised as `output_file` in results; retention-cleaned |
| Small-Model Context Harness | tests-pass | `ahma tui` budgets tool results + trims conversation for limited-context local models; `--context-length`, `--small-model-harness`/`--no-small-model-harness` |
| Feature-Gated Incubating Crates | tests-pass | vault/cluster/simplify/decompose/worker/renewal behind non-default cargo features; graceful `feature_not_compiled` CLI errors |
| Latency Regression Guards | tests-pass | Ignored benchmarks guard end-to-end dispatch latency and per-line streaming cost (`latency_guard_test`) |
| Linux Sandbox (Landlock) | tests-pass | Kernel-level FS sandboxing on Linux 5.13+ |
| macOS Sandbox (Seatbelt) | tests-pass | Kernel-level FS sandboxing via `sandbox-exec` |
| Nested Sandbox Detection | tests-pass | Detects Cursor/VS Code/Docker outer sandboxes |
| Windows Runtime (PowerShell) | in-progress | Built-in PowerShell (5.1+) shell pool; cross-platform path security + file URI; parity tests green |
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
| Local Cluster Scheduler | tests-pass | mDNS discovery and signed task dispatch to remote worker peers |
| Configuration Standard (R-CFG) | PLANNED | Flag/settings-file configuration with trust tiers; `AHMA_*` env vars retired as a config source (§3.5) |
| `ahma cluster remove` | tests-pass | Subcommand to remove worker peers from peers configuration |
| `ahma setup` / `ahma uninstall` | tests-pass | Interactive wizard installs / removes MCP entries, hooks, skills, binary; symmetric teardown leaves other user config intact |
| Auto-spawned Bridge Lifecycle | tests-pass | Bridges started by `ahma serve stdio` or `ahma tui` self-terminate after `--idle-timeout` seconds with no connected client; explicitly-started `ahma serve http/unix` remain persistent by default |

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
| `shell_pool` | Pre-warmed bash/PowerShell (5.1+) shells for 5-20ms command startup latency |
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
            │  ShellPool    │ ──▶ Pre-warmed bash/PowerShell shells
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

- **R3.1**: The system **must** use a pre-warmed shell pool for 5-20ms command startup latency.
- **R3.2**: Shell processes are pooled per working directory and automatically cleaned up.

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

- **R-CFG6.1**: A settings file that exists but fails to parse **must** abort startup with a clear error. Silently falling back to defaults is forbidden — a tampered or corrupted file must not silently change behavior.
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
- **R-CFG9.2**: Production code **must not** read `NEXTEST`, `CARGO_MANIFEST_DIR`, `CARGO_LLVM_COV`, `CARGO_TARGET_DIR`, or any other cargo-set environment variable. These variables are set by the build/test toolchain and must not influence runtime security decisions (R21.3). The `--server-child` flag is the exclusive mechanism for subprocess detection in production.
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
- **R5.3.6**: **TUI-only establishment is pending**: An answer given in the TUI when no IDE session is live **must** establish the scope as **pending** (shown as such), applied when the next IDE session attaches to the workspace instance; it **must not** silently lock a scope that no live session is using as if it were active.

#### Subprocess propagation and defaults

- **R5.4.1**: **Scope propagation to subprocesses**: When the stdio MCP server spawns a background bridge or per-session subprocesses, it **must** forward only genuinely explicit `--sandbox-scope` values (never the provisional CWD or temp). The `--sandbox` and `--tmp` boolean flags are forwarded separately so each subprocess derives the default secondary and auxiliary scopes itself.
- **R5.4.2**: **Default install uses `--sandbox`**: The default MCP server configuration installed by `ahma setup` for Cursor, VSCode, Claude, Antigravity, Codex, and LM Studio **must** include `--sandbox` (not `--tmp`). For clients known not to support `roots/list` (e.g. Antigravity, LM Studio), `ahma setup` **should** additionally inject an explicit `--sandbox-scope` (or rely on the `~/sandbox` default) so the client works without a stall.
- **R5.4.3**: **Write Protection**: The system **must** block any attempt to write outside the locked scope, including via command arguments (e.g. `touch /outside/file`).

#### Persistent scope grants (external tool directories)

- **R5.4.4**: **User-granted persistent scopes survive `roots/list`**: Directories listed in `[sandbox] persistent_scopes` (`~/.ahma/settings.toml`) are external locations a trusted tool legitimately needs outside the workspace — e.g. a build cache (sccache/ccache) or a shared toolchain. Each entry carries an `access` (`rw` default, or `ro`) and optional `granted_by` / `granted_at` / `note` provenance. Unlike the provisional `scopes` list (replaced when the client sends workspace roots), every persistent scope **must** be re-appended on each `roots/list` update — `rw` into the writable set, `ro` into the read-only set — so the grant remains in effect for the whole session regardless of which workspace the client opens. They are folded into the initial scope set before first enforcement.
- **R5.4.5**: **Grants are authored only out-of-band, never by a sandboxed actor**: `persistent_scopes` lives in `~/.ahma/settings.toml`, which is outside every workspace scope and therefore kernel-unwritable from inside the sandbox. A scope **must** be added only by the trusted control plane acting on explicit human action — the `ahma sandbox grant` command, or a hand edit — never by a tool call. The AI may *request* a scope; only the human *grants* one. This is the persistence layer the elicitation downgrade flow (R5.3) writes through once a request is approved.
- **R5.4.6**: **Grants are inspectable and reversible by name**: `ahma sandbox list` **must** show every persistent scope with its access level and provenance, and **must** name the absolute settings file that holds them, since that file — not any AI action — is where the user edits or revokes them. `ahma sandbox grant`/`revoke` **must** confirm the change, name that same file, and state that it takes effect on the next server start.
- **R5.4.7**: **Auto-detection raises the grant question, never the grant**: when a sandboxed command is blocked by an out-of-scope path — either rejected up front by path validation (the path is known exactly) or surfaced by a stderr denial signature (a heuristic *candidate* path) — the server **may** raise a "grant access to X?" prompt that, on explicit human approval, writes a persistent grant via R5.4.5. Detection **must not** widen the live session: an approved grant is persisted for the next start, identical to `ahma sandbox grant`. The same offending `(path, access)` **must** be asked at most once per session (a denied or already-granted path **must not** re-prompt), and a stderr-extracted path **must** be canonicalized and shown literally — a command that prints a forged denial line can at worst raise a human-gated prompt, never escalate on its own.

#### Terminal hooks (one-time consent, never silent)

- **R5.5.3**: **Hook fall-open requires one-time, session-scoped consent**: A terminal hook that can sandbox the command runs normally. When the hook binary **executes** but cannot sandbox (it is stale/incapable, or the kernel sandbox is unavailable), it **must not** silently run the command unsandboxed. Instead:
  - The **first** such invocation in a session **fails closed**: it returns a `deny` decision to the IDE and emits an actionable message (the reason plus `ahma hooks doctor` / repair guidance and how to approve unsandboxed mode). `ahma hooks doctor`, `ahma hooks approve-unsandboxed`, and `ahma hooks revoke` manage and diagnose this state.
  - **Residual (binary cannot execute at all)**: if the hook binary is entirely missing or crashes/times out before producing a decision, ahma cannot emit `deny` and the IDE-level `failClosed` setting governs. The default install keeps `failClosed: false` as a deliberate anti-wedge valve, because the consent mechanism itself requires the binary to run; this residual is loud (the IDE surfaces the hook failure) and is repaired with `ahma hooks doctor` / reinstall, not silent in the steady-state sense R5.5.3 targets.
  - Consent is collected **out-of-band** (an `elicitation/create` to an attached client/TUI, or an explicit `ahma hooks approve-unsandboxed` command) — never mid-command. Approving unsandboxed execution is maximal widening and **must** be an explicit, deliberate action, never an Enter-default.
  - Consent is scoped to **workspace + session generation** and **must not** persist across restarts (persisting would silently re-downgrade the next session).
  - While consent is active, every surface (R5.4) **must** continuously display a prominent banner stating that hooks are running unsandboxed and how many commands have done so.
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

### R7: Nested Sandbox Detection

- **R7.1**: System **must** detect when running inside another sandbox (Cursor, VS Code, Docker).
- **R7.2**: Upon detection, system **must** exit with instructions to use `--disable-sandbox` or `AHMA_DISABLE_SANDBOX=1`.
- **R7.3**: When `--disable-sandbox` is used, outer sandbox provides security; Ahma's internal sandbox is disabled.

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

### 5.6 Decompose Tool Type

Set `"tool_type": "decompose"` to split a complex business question into smaller sub-questions, dispatch each to a local LLM, and aggregate the results with a deterministic Rust reducer.  **No cloud egress required** — uses the same `LlmProviderConfig` as `livelog`.

#### Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `tool_type` | No | `"command"` | Set to `"decompose"` to activate |
| `decompose` | Yes (when `tool_type=decompose`) | — | `DecomposeConfig` block |

**`DecomposeConfig` fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `llm_provider` | Yes | — | `LlmProviderConfig` — prefer small local models (`gemma3:4b`, `llama3.2:3b`) |
| `max_subtasks` | No | `5` | Maximum sub-questions to generate |
| `max_concurrent` | No | `3` | Sub-questions to run concurrently (keep low for single-machine Ollama) |
| `reduce_mode` | No | `"summarize"` | How to combine results: `summarize`, `extract_fields`, `classify`, `concat`, `first` |
| `answer_prompt` | No | `"Answer concisely"` | System prompt for each sub-question LLM call |
| `llm_timeout_seconds` | No | `30` | Timeout per LLM call |

#### Pipeline

1. `tools/call` returns an `operation_id` immediately.
2. The orchestrator asks the LLM to split the question into up to `max_subtasks` sub-questions.
3. Sub-questions are dispatched in batches of `max_concurrent` to the LLM.
4. Results are aggregated by the deterministic `Reducer` (no additional LLM call).
5. The aggregated answer is pushed as a `ProgressUpdate` notification.

#### Example (`.ahma/decompose.json`)

See the ready-to-use config in [`.ahma/decompose.json`](.ahma/decompose.json).

---

### 5.7 Worker Tool Type

Set `"tool_type": "worker"` to compile and run synthesized Rust or Python code inside a sub-vault.  The synthesized program is deterministic code — it cannot be re-injected mid-run.

#### Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `tool_type` | No | `"command"` | Set to `"worker"` to activate |
| `worker` | Yes (when `tool_type=worker`) | — | `WorkerConfig` block |

**`WorkerConfig` fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `language` | No | `"rust"` | `"rust"` (requires `rustc`) or `"python"` (requires `python3`) |
| `extra_args` | No | `[]` | Additional compiler / interpreter arguments |
| `keep_source` | No | `false` | Retain the synthesized source file after execution |
| `timeout_seconds` | No | `60` | Execution timeout |

#### Security properties

- Worker executes inside the vault's `workdir/` kernel sandbox scope.
- Source hash (SHA-256-like digest) is recorded in `audit.jsonl`.
- Source is deleted after execution unless `keep_source: true`.

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
| Recursive task tree | [docs/recursive-task-tree.md](docs/recursive-task-tree.md) | — |

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
| `ProcessSpawn` | 30s | 120s | 240s | Binary loading, shell pool init |
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
| Shell pool | PASS | Pre-warmed processes, per-directory pooling |
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

