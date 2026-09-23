---
name: ahma
version: 0.21.3
author: Paul Houghton
description: >
  Comprehensive guide for using Ahma (ahma) as an AI agent. USE THIS SKILL when you need
  to understand how to run tools, activate bundles, use the sandbox, monitor logs, author custom
  tools, or configure ahma. Also handles code complexity analysis via `/ahma simplify` and
  installation updates via `/ahma update`.
  Trigger phrases: "use ahma", "run with ahma", "ahma tool", "activate bundle",
  "run_terminal_command", "ahma async", "ahma serve", "mcp.json ahma", "ahma sandbox",
  "ahma livelog", "ahma monitor", "custom tool .ahma", "ahma", "await tool",
  "cancel operation", "tool bundle", "progressive disclosure", "activate_tools",
  "simplify", "reduce complexity", "too complex", "hard to read", "refactor",
  "maintainability", "cognitive complexity", "cyclomatic complexity", "simplicity score",
  "code quality metrics", "hotspot", "ahma simplify", "ahma help", "ahma ?",
  "ahma update", "ahma tui", "/ahma tui".
user-invocable: true
---

<!-- version: 0.21.3 | author: Paul Houghton -->

# Ahma Skill — Comprehensive AI Usage Guide

**Ahma** (`ahma`) is a kernel-sandboxed MCP server that wraps command-line tools for AI
agents. It exposes shell tools (cargo, git, python, file utilities, etc.) as MCP tools with
kernel-level filesystem sandboxing, async execution, and live log monitoring.

---

## Quick Start: mcp.json Setup

MCP stdio servers auto-start when the IDE needs tools — the only step is getting config in
place. Full walkthrough of every mode (stdio/HTTP/Unix socket) and client:
[docs/connection-modes.md](https://github.com/ahma-labs/ahma/blob/main/docs/connection-modes.md);
first-time install: [docs/installation.md](https://github.com/ahma-labs/ahma/blob/main/docs/installation.md).

**Recommended — commit `.vscode/mcp.json` to the repo** so every teammate gets it automatically:

```json
{
  "servers": {
    "ahma": {
      "type": "stdio",
      "command": "ahma",
      "args": ["serve", "stdio", "--tools", "git,fileutils", "--log-monitor"]
    }
  }
}
```

| Approach | Where |
|---|---|
| User-level (all workspaces) | VS Code `~/.config/Code/User/mcp.json`, Cursor `~/.cursor/mcp.json`, Claude Code `~/.claude.json` → `mcpServers`, Claude Desktop `claude_desktop_config.json` |
| VS Code auto-start | `{ "chat.mcp.autoStart": true }` in settings |
| VS Code sandbox integration | add `"sandboxEnabled": true` + `"sandbox": {...}` to the server entry — auto-approves tool calls |
| Multi-IDE install script | `curl -fsSL https://raw.githubusercontent.com/ahma-labs/ahma/main/scripts/install.sh \| bash` |
| Dev containers | run the install script from `postCreateCommand`, combine with a committed `.vscode/mcp.json` |
| One-step setup (all of the above) | `ahma setup` (interactive) or `ahma setup -y` (non-interactive) |

**Helping a user set up Ahma:** check for `.vscode/mcp.json`, offer to create it, ask which
bundles they need, then tell them to reload the window.

## Tool Bundles

Ahma groups command-line tools into logical bundles loaded at startup via `--tools`.

| Bundle | Activate with | Key tools | When to use |
|--------|--------------|-----------|-------------|
| `fileutils` | `--tools fileutils` | ls, cp, mv, rm, grep, find, diff | File operations |
| `github` | `--tools github` | gh pr/issue/run/release | GitHub CLI operations |
| `git` | `--tools git` | git status/commit/push/log/diff | Version control |
| `python` | `--tools python` | python script execution | Python projects |
| `simplify` | `--tools simplify` | Code complexity analysis | Code quality work |

```json
"args": ["serve", "stdio", "--tools", "git,fileutils"]
```

---

## Built-in Tools (Always Available)

### `run_terminal_command` — Run any shell command

```
run_terminal_command(command="cargo build --release", working_directory="/path/to/project", timeout_seconds=300)
```

Runs inside the kernel sandbox. Supports pipes, redirects, multi-command strings.
`monitor_level` (`error`/`warn`/`info`) + `monitor_stream` (`stderr`/`stdout`/`both`) trigger
LLM log alerts.

### `status` — Check async operation progress

```
status(id="op_abc123")
```

Returns `running`/`complete`/`failed`/`cancelled`/`timeout`. Non-blocking, safe to poll.

### `await` — Wait for an async operation to finish

```
await(id="op_abc123", timeout_seconds=60)
```

Blocks until completion or timeout (default 540s / `tools.await_timeout_secs`). The timeout is
**soft** — it ends your wait, not the operation; call `await` again with the same `id`, or
`cancel` to stop it. A wait can also end early if the liveness probe of your connection goes
unanswered — treat that the same as a timeout and `await` again.

### `cancel` — Cancel a running operation

```
cancel(id="op_abc123")
```

Terminates the process and frees resources.

---

Clients without native file tools also get `read_file`, `replace_in_file`, `multi_edit`, `apply_patch`, `grep_search`… — read before editing; one exact match per edit ([docs/file-tools.md](https://github.com/ahma-labs/ahma/blob/main/docs/file-tools.md)).

## Sync and Async Modes

Every call is a tracked operation (`status`, `ahma tui`, cancellable, full output in
`output_file`). `tools.execution_mode` decides how long a call waits (the server
`instructions` say which); humans switch it with `--sync`/`--async` or `/sync`/`/async`:

- **`sync` (default)** — a call returns the command's result when it finishes. If it outlasts
  what your client can hold one request open for, you get an `operation_id` and a line saying
  it is still running: `await` that id to collect it.
- **`async`** (`--async`) — a call returns inline only if it finishes within a few seconds,
  otherwise an `operation_id`; start several, then `await` them:

```
result = cargo_build(subcommand="build")        # → "AHMA ID: op_abc123 … running in the background"
status(id="op_abc123")                          # → { "status": "running", ... }  (non-blocking)
await(id="op_abc123", timeout_seconds=120)      # → { "status": "complete", "exit_code": 0, ... }
```

> **The completion push needs you to still be listening.** It rides the same live connection
> that started the operation — nothing is queued or replayed for a caller who has disconnected.
> If there's any chance you'll end your turn before an operation finishes, `await` it (blocking)
> rather than relying on the notification — this matters most for subagents, which aren't woken
> back up by an MCP push the way a top-level session is.

> **Before declaring a task done, confirm every operation you started actually finished.** A
> soft `await` timeout is not completion. `status`/`await` every `operation_id` from this turn
> and confirm each reached a terminal state (`Completed`/`Failed`/`Cancelled`), not `InProgress`.

---

## Sandbox — Filesystem Security

Ahma enforces **kernel-level** filesystem boundaries set once at startup. Full detail (scope
narrowing, trust-handoff writes, network egress, platform internals):
[docs/security-sandbox.md](https://github.com/ahma-labs/ahma/blob/main/docs/security-sandbox.md).

| Rule | Detail |
|---|---|
| Scope (STDIO) | `cwd` from mcp.json (usually `${workspaceFolder}`) |
| Scope (HTTP) | Workspace roots from MCP `roots/list` |
| Override | `--sandbox-scope /path/a` (repeat for multiple paths) |
| Temp dir | `--tmp` adds `/tmp` (`%TEMP%` on Windows); needed for compilers/build tools |
| Nested sandbox | Ahma detects an outer sandbox (Cursor/VS Code/Docker/another ahma) and discloses which one is actually protecting you; on macOS, Seatbelt can't nest so ahma running inside another Seatbelt defers (`deferred_to_host`). `--no-sandbox` is the explicit opt-out. |
| Platform | Linux: Landlock (kernel 5.13+) · macOS: Seatbelt · Windows: Job Objects (+ AppContainer, in progress) |

---

## Terminal Hooks — Shell Interception & Security

For agents that run shell commands natively (Claude Code, Cursor, Codex, Copilot CLI) rather
than via MCP, **terminal hooks** extend the kernel sandbox to those commands by wrapping them
with `ahma hooks run-shell`. Full setup, fail-safe semantics, and per-client config paths:
[docs/installation.md#terminal-hooks](https://github.com/ahma-labs/ahma/blob/main/docs/installation.md#terminal-hooks).

```bash
ahma hooks status                                    # effective ACTIVE/INACTIVE + why
ahma hooks install --scope user                      # all supported clients
ahma hooks install --platform copilot --scope project
ahma hooks uninstall --platform copilot --scope user
```

Key points: **installed ≠ active** (only active when an ahma MCP server is detected for that
client, unless forced with `AHMA_HOOKS=on|off`); **fail-safe, not silent** (a command ahma can't
sandbox is blocked, not run unsandboxed — `ahma hooks doctor` diagnoses it; `ahma doctor` checks ahma overall and fixes only on `y`); hooks and the MCP
server are complementary, not redundant (they sandbox different command streams, so running
both is safe).

---

## Live Log Monitoring

Full reference: [docs/live-log-monitoring.md](https://github.com/ahma-labs/ahma/blob/main/docs/live-log-monitoring.md).

| Flavor | Enable | What it does |
|---|---|---|
| Server logs | `--log-monitor` (rate limit: `--monitor-rate-limit 60`) | Tails `.ahma/logs/ahma.log.*`, pushes `LogAlert` notifications on LLM-detected anomalies |
| `livelog` tool type | Define in `.ahma/*.json` with `"tool_type": "livelog"` | Monitors any streaming command (e.g. `adb logcat`) via a `source_command` + `detection_prompt`; built-in example: `android-logcat` |

---

## Custom Tools — `.ahma/` Directory

Place `*.json` files in `.ahma/` at the project root to define project-local tools (override
path: `--tools-dir`). Ahma loads them once at startup — no watch mode; call the `restart` tool
after editing one. Canonical guide with the full config format, subcommand/sequence-tool
examples, reserved names, and validation:
[.ahma/README.md](https://github.com/ahma-labs/ahma/blob/main/.ahma/README.md) ·
[docs/custom-tools.md](https://github.com/ahma-labs/ahma/blob/main/docs/custom-tools.md).

Minimal example:

```json
{
  "name": "deploy",
  "description": "Deploy the application to staging",
  "command": "scripts/deploy.sh",
  "enabled": true,
  "synchronous": true
}
```

Validate configs: `ahma tool validate .ahma/`

---

## Key CLI Flags and Settings

> `AHMA_*` **configuration** env vars are retired and ignored — use CLI flags or
> `~/.ahma/settings.toml`. Terminal-hook vars (`AHMA_HOOKS`, `AHMA_DISABLE_HOOKS`,
> `AHMA_PREFER_OWN_SANDBOX`) remain live. Full reference:
> [docs/environment-variables.md](https://github.com/ahma-labs/ahma/blob/main/docs/environment-variables.md).

| CLI flag / Settings key | Default | Purpose |
|----------|---------|---------|
| `--tools-dir` / `tools.tools_dir` | `.ahma/` | Custom tools directory path |
| `--timeout` / `tools.timeout_secs` | `600` | Default tool timeout (seconds) |
| `--sync` / `--async` / `tools.execution_mode` | `sync` | `sync`: calls return results; `async`: calls return ids to `await` |
| `--no-sandbox` / `sandbox.disable` | off | Disable kernel sandbox (UNSAFE) |
| `--sandbox-scope` / `sandbox.scopes` | cwd | Sandbox scope paths |
| `sandbox.container_root` | unset | Directory holding your projects; scope fallback when the client reports no roots |
| `--scratch` / `sandbox.use_scratch_directory` | off | Add a persistent secondary scope |
| `--tmp` / `sandbox.tmp_access` | off | Add temp dir to sandbox scope (opt-in) |
| `--disable-temp-files` / `sandbox.disable_temp` | off | Block all temp dir access |
| `--no-package-cache-write` | off | Disable cargo cache writes (strictest isolation) |
| `--log-to-stderr` / `logging.target` | file | Log to stderr |
| `--log-monitor` / `logging.log_monitor` | off | Enable live log monitoring |
| `--monitor-rate-limit` / `logging.monitor_rate_limit_secs` | `60` | Min seconds between log alerts |
| `RUST_LOG` (env) | `info` | Log verbosity (e.g. `ahma_mcp=debug`) |

---

## CLI Reference

```bash
# Start MCP server (stdio — for IDE integration)
ahma serve stdio [--tools git,fileutils] [--sandbox] [--log-monitor]

# Start HTTP server (local development, multiple clients)
ahma serve http [--port 3000] [--host 0.0.0.0] [--disable-quic]

# Start Unix socket server (IPC / Kubernetes sidecars)
ahma serve unix [--socket-path <path>]   # default: the per-user runtime dir

# Run a single tool from the CLI
ahma tool run run_terminal_command -- "echo hello"

# Validate .ahma/ tool configs
ahma tool validate [.ahma/]

# Show locally configured tools with descriptions (use this, not `ahma tool list`, for local configs)
ahma tool info [--tools git,fileutils]

# Local TLS certificate management (required for QUIC/HTTP3 transport)
ahma tls init      # Generate cert at ~/.ahma/tls/ (idempotent)
ahma tls rotate    # Replace the certificate with a new one
ahma tls status    # Show cert path, age, and rotation recommendation
```

---

## Troubleshooting

**Tool not found**: check the tool's bundle is in `--tools` at startup (e.g. `--tools git,fileutils`).

**Timeout**: `--timeout 600` in mcp.json args, or `tools.timeout_secs = 600` in `~/.ahma/settings.toml`.

**Permission denied / sandbox error**: the path is outside the sandbox scope — check
`--sandbox-scope`, set `[sandbox] container_root`, or add `--tmp` for temp-file access.

**"sandbox scope is your container root"**: the session scope spans every project; pass
`working_directory` naming the project subdirectory so ahma knows which subtree to narrow to.

**Cargo/tool-install permission errors** (`cargo add`, `cargo install`, `npm i -g`, …): do
**not** add `--sandbox-scope ~/.cargo` — that grants write to the whole cargo home including
credentials. The built-in `package_cache_write` feature (on by default) already handles
`cargo add`/`update`. For installs into `~/.cargo/bin`, use the `sandbox_grant` tool (preview,
then `confirm: true`) + `restart`, or install into the workspace instead
(`cargo install --root <workspace>/.tools`). Full rationale:
[docs/security-sandbox.md#cargo-install--cargo-binstall-and-other-tool-installs](https://github.com/ahma-labs/ahma/blob/main/docs/security-sandbox.md#cargo-install--cargo-binstall-and-other-tool-installs).

**"ahma is DEFERRING to … sandbox"**: ahma is inside an outer sandbox it can't nest inside
(macOS Seatbelt refuses nesting). Commands still run, confined by the outer sandbox — expected
when running ahma's own test suite or a nested `ahma serve`. For ahma's own enforcement, start
it from a plain terminal.

**Tool still running**: `status(operation_id)` to check, or `cancel(operation_id)`.

**Linux old kernel**: Landlock needs kernel 5.13+; use `--no-sandbox` on older systems.

---

## User-Invocable Subcommands

| Command | Alias | Purpose |
|---------|-------|---------|
| `/ahma help` | `/ahma ?` | List all available subcommands and their usage |
| `/ahma tool list` | `/ahma tools` | Show all available tools in the current project |
| `/ahma simplify` | — | Auto-fix top 10 complexity issues concurrently via subagents |
| `/ahma simplify top N` | — | Auto-fix top N complexity issues concurrently |
| `/ahma simplify N` | — | Get fix instructions for issue #N only (manual mode) |
| `/ahma tui` | — | Start the terminal user interface (TUI) control plane |
| `/ahma update` | — | Update ahma to the latest version |
| `/ahma uninstall` | — | Remove integrations installed by `ahma setup` (MCP entries, hooks, skills, binary) |

`/ahma help` / `/ahma ?` just reprints this table plus the key config flags (`--tools`,
`--sandbox`, `--log-monitor`).

---

## `/ahma tool list` — List Configured Tools

`/ahma tool list` / `/ahma tools`: list all configured tools (built-in bundles + local
`.ahma/` configs). Load them with `ahma tool info` (**not** `ahma tool list`, which expects a
running server connection and fails on the bare CLI) and present as a markdown table of name,
description, subcommands.

---

## `/ahma tui` — Start the TUI Dashboard

`/ahma tui` runs `ahma tui` in the user's terminal: a work view with one section per client
session (attached editors, hooked shell commands, the user's own commands), history replayed
from the per-user daemon, and `i` to toggle the chat pane where approval gates are answered.
Inside that chat, `/skills` lists Agent Skills from the standard locations and `/<name>
[args]` runs one. Full detail: [docs/tui.md](https://github.com/ahma-labs/ahma/blob/main/docs/tui.md).

---

## `/ahma update [ref]` — Update the Installed Binary

```
/ahma update                  # latest published GitHub release
/ahma update 0.15.2           # a specific release tag (semver, with or without 'v')
/ahma update main             # build and install from the main branch
/ahma update <branch-name>    # build and install from a named feature branch
```

**Workflow — always prefer the built-in subcommand**, which handles platform detection,
version comparison, `RUSTFLAGS`, and PATH hints automatically:

1. `run_terminal_command("ahma update")` (or `ahma update <branch-name>` — branch installs
   compile from source and take several minutes; watch for `Installed /path/to/ahma`)
2. `run_terminal_command("ahma --version")` — confirm it reports the expected version
3. Ask the user to reload the IDE (MCP clients cache the binary path)

Manual `cargo install` fallback (only if `ahma update` is absent or broken), the
`RUSTFLAGS='--cfg reqwest_unstable'` requirement, and anti-patterns (never target the retired
`ahma_mcp` package): [docs/installation.md](https://github.com/ahma-labs/ahma/blob/main/docs/installation.md).

---

## `/ahma uninstall` — Remove Installed Integrations

Symmetrically reverses `ahma setup`: removes MCP server entries, terminal hooks, agent skills
and/or the ahma binary. Only ahma-managed keys/files are touched.

```
/ahma uninstall                                 # Interactive wizard
/ahma uninstall --auto                          # Non-interactive: remove everything
/ahma uninstall --mcp --platform cursor,claude  # Remove only specific MCP entries
/ahma uninstall --auto --dry-run                # Preview without writing
/ahma uninstall --auto --purge                  # Also delete ~/.ahma data directory
```

| Flag | Description |
|------|-------------|
| `-y` / `--auto` | Skip prompts, remove everything |
| `--mcp` / `--hooks` / `--skills` / `--binary` | Remove only that category |
| `--platform <list>` | Comma-separated platforms to target |
| `--purge` | Also remove `~/.ahma` data dir (TLS, settings, logs) |
| `--dry-run` | Print planned changes without modifying files |

Background ahma servers self-terminate once no IDE/TUI client is connected.

---

## `/ahma simplify` — Automatic Code Simplification

When the user types `/ahma simplify`, automatically analyze the codebase, identify the top
complexity issues, and spawn concurrent subagents to fix them — **no additional prompting
required**. Full reference (all lenses' caveats, supported languages, CLI flags, MCP args,
score formula, fail-closed rule):
[docs/simplify.md](https://github.com/ahma-labs/ahma/blob/main/docs/simplify.md).

### Syntax

```
/ahma simplify                  # Auto-fix top 10 issues concurrently (DEFAULT)
/ahma simplify top 5            # Auto-fix top 5 issues concurrently
/ahma simplify rust             # Auto-fix top 10 Rust issues concurrently
/ahma simplify rust top 3       # Auto-fix top 3 Rust issues concurrently
/ahma simplify 3                # Manual mode: get fix prompt for issue #3 only
/ahma simplify kotlin 2         # Manual mode: Kotlin issue #2 only
/ahma simplify --lens reuse     # Reuse lens only — duplicate-code candidates
/ahma simplify --lens dead-code # Dead-code lens only — unreferenced exports
/ahma simplify --lens altitude  # Altitude lens only — delegation chains
/ahma simplify --diff           # Only files changed in git, instead of the whole tree
```

**Mode selection:** `top N`, or no trailing integer → **auto mode** (concurrent subagents). A
bare trailing integer without `top` → **manual mode** (single-file, sequential). `--lens` and
`--diff` narrow *what* gets analyzed and combine with either mode.

**Lenses** (`--lens`/`lens`, default `all`): `complexity` (metrics/hotspots); `reuse`
(duplicate-code candidates — evaluate each before extracting, don't auto-spawn a fix per
finding); `dead-code` (unreferenced exports, Rust/TS/JS/Python/Java only — a candidate, never a
verdict, verify before deleting); `altitude` (thin-wrapper delegation chains, same 5 languages —
a forwarding layer is often intentional). Full per-lens blind spots and mitigations are in the
docs page linked above.

### Auto Mode — Concurrent Simplification (DEFAULT)

**Phase 1 — Analyze (parent agent):** run `simplify(directory="<root>", ai_fix=1)` (or `ahma
simplify <root> --ai-fix 1`); parse the ranked file list and set `N =
min(requested_count, total_issues)` (default `requested_count` 10). Tell the user how many
issues were found and that N subagents are being spawned.

**Phase 2 — Spawn subagents (concurrent):** spawn **one subagent per issue, in the same
response**, so they run concurrently — each edits a different file, so there are no conflicts.
No subagent tool? Launch N background tasks, or run sequentially as a last resort.

> [!IMPORTANT]
> **Antigravity**: no general-purpose subagent tool exists (only `browser_subagent`, for
> browser tasks). Run issues **sequentially**, default to **N = 1** unless the user asked for
> more, verify each file with `--verify` before the next, and get user approval between files.

Each subagent's prompt:

```
Fix complexity issue #<N> in project root <PROJECT_ROOT>.

1. Run `ahma simplify <PROJECT_ROOT> --ai-fix <N>` (or simplify(directory=<PROJECT_ROOT>,
   ai_fix=<N>)) and read the fix prompt: file path, hotspot functions, evaluation, constraints.
2. Evaluate critically — if complexity is volume-driven (many match arms, config fields)
   rather than genuinely hard to follow, report "No changes needed" and STOP.
3. Otherwise edit ONLY the listed hotspot functions: no signature/API/behavior changes, no
   surrounding refactors, no cargo fmt/clippy/test (the parent runs those once at the end).
   Prefer guard clauses, helper extraction, named predicates. Skip test files unless one test
   function is individually complex.
4. Report what changed (or why nothing did).
```

**Phase 3 — Verify (parent agent, after ALL subagents complete):**
1. `cargo fmt --all && cargo clippy --all-targets`
2. `cargo nextest run` — on failure, identify and revert/fix the responsible subagent's change
3. Re-run `ahma simplify <project-root> --ai-fix 1`; report the before/after project score
4. Summarize per-issue results in a table (file, action, result)

### Manual Mode — Single-Issue Workflow

Triggered by a bare trailing integer (e.g. `/ahma simplify 3`):

1. `simplify(directory=".", ai_fix=<N>)` — read the structured fix prompt
2. Edit only the listed hotspot functions (no signature/API/behavior changes, no whole-file refactor)
3. `simplify(directory=".", verify="<edited-file>")` — see the verdict table in
   [docs/simplify.md#verification](https://github.com/ahma-labs/ahma/blob/main/docs/simplify.md#verification)
4. Iterate with `ai_fix=<N+1>` until the project score is satisfactory

### Anti-Patterns to Avoid

1. Don't refactor the whole file — follow the hotspot list exactly.
2. Don't add comments to improve scores — structural change is needed.
3. Don't inline complex logic — fewer, denser functions score worse.
4. Don't run `--ai-fix` without reading the structured prompt.
5. Don't skip verification.
6. Don't have subagents run cargo fmt/clippy/test — the parent runs them once, after all
   subagents finish, to avoid build lock contention.

---

**See also**: [security-sandbox.md](https://github.com/ahma-labs/ahma/blob/main/docs/security-sandbox.md) ·
[live-log-monitoring.md](https://github.com/ahma-labs/ahma/blob/main/docs/live-log-monitoring.md) ·
[connection-modes.md](https://github.com/ahma-labs/ahma/blob/main/docs/connection-modes.md) ·
[environment-variables.md](https://github.com/ahma-labs/ahma/blob/main/docs/environment-variables.md) ·
[mtdf-schema.json](https://github.com/ahma-labs/ahma/blob/main/docs/mtdf-schema.json) ·
[simplify.md](https://github.com/ahma-labs/ahma/blob/main/docs/simplify.md)
