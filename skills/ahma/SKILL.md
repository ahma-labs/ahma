---
name: ahma
version: 0.11.0
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

<!-- version: 0.11.0 | author: Paul Houghton -->

# Ahma Skill — Comprehensive AI Usage Guide

**Ahma** (`ahma`) is a kernel-sandboxed MCP server that wraps command-line tools for AI
agents. It exposes shell tools (cargo, git, python, file utilities, etc.) as MCP tools with
kernel-level filesystem sandboxing, async execution, and live log monitoring.

---

## Quick Start: mcp.json Setup

MCP stdio servers auto-start when the IDE needs tools — the only step is getting
the config in place. There are several approaches, from zero-friction to global:

### 1. Commit to the repo (recommended — zero setup for teammates)

**The Ahma project already provides `.vscode/mcp.json` with three configurations to try:**

- `ahma` — stdio mode (recommended, automatic per-client instances)
- `ahma-http` — shared HTTP server on port 3000 (run `ahma serve http --tools rust,git,fileutils --tmp --log-monitor`)
- `ahma-unix` — shared HTTP server over Unix socket (run `ahma serve unix --socket-path /tmp/ahma.sock --tools rust,git,fileutils --tmp --log-monitor`)

You can copy or customize this for your own projects. Create `.vscode/mcp.json` in your project root and commit it. Every VS Code user
who opens the project gets Ahma configured automatically (prompted to trust once):

```json
{
  "servers": {
    "ahma": {
      "type": "stdio",
      "command": "ahma",
      "args": ["serve", "stdio", "--tools", "rust,git,fileutils", "--tmp", "--log-monitor"]
    }
  }
}
```

### 2. User-level config (available in all workspaces)

| IDE | Config file |
|-----|-------------|
| **VS Code** | `~/.config/Code/User/mcp.json` (or run `MCP: Open User Configuration`) |
| **Cursor** | `~/.cursor/mcp.json` |
| **Claude Code** | `~/.claude.json` → `"mcpServers"` key |
| **Claude Desktop** | `~/Library/Application Support/Claude/claude_desktop_config.json` |

Same JSON structure as above. The server starts automatically when chat is opened.

### 3. VS Code auto-start setting

Enable globally in VS Code settings:
```json
{ "chat.mcp.autoStart": true }
```
This auto-(re)starts MCP servers when configuration changes are detected, so
Ahma spins up as soon as VS Code sees the config — no need to open chat first.

### 4. VS Code sandbox integration (auto-approve tool calls)

VS Code provides its own sandbox for MCP stdio servers. When enabled, tool calls
are auto-approved (no confirmation dialogs). Pairs well with Ahma's kernel sandbox:

```json
{
  "servers": {
    "ahma": {
      "type": "stdio",
      "command": "ahma",
      "args": ["serve", "stdio", "--tools", "rust,git,fileutils", "--tmp"],
      "sandboxEnabled": true,
      "sandbox": {
        "filesystem": { "allowWrite": ["${workspaceFolder}"] }
      }
    }
  }
}
```

### 5. Install script (multi-IDE setup)

Run the install script to configure Ahma across all supported IDEs at once:
```bash
curl -fsSL https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.sh | bash
```

### 6. Dev containers

Add to `.devcontainer/devcontainer.json` for Codespaces / container users:
```json
{
  "features": {},
  "postCreateCommand": "cargo install ahma",
  "customizations": {
    "vscode": {
      "settings": { "chat.mcp.autoStart": true }
    }
  }
}
```
Combine with `.vscode/mcp.json` committed to the repo.

### AI Agent: Helping Users Set Up

If a user asks you to use Ahma but it isn't configured, help them by:
1. Check if `.vscode/mcp.json` exists — if not, offer to create it
2. Ask which bundles they need (rust, git, python, etc.)
3. Create the config and tell them to reload the window (`Developer: Reload Window`)

---

## Tool Bundles & Progressive Disclosure

By default, Ahma shows only built-in tools (`run_terminal_command`, `status`, `await`) plus
`activate_tools` (when bundles are loaded but not yet specified via `--tools`).
Bundles specified with `--tools` are **always revealed immediately** — no extra flag needed.
Bundles NOT in `--tools` remain hidden and can be unlocked on demand via `activate_tools`.

### Discovering and Activating Bundles

```
activate_tools(action="list")           # See available bundles
activate_tools(action="reveal", bundle="rust")   # Unlock Cargo tools
activate_tools(action="reveal", bundle="git")    # Unlock Git tools
```

### Available Bundles

| Bundle | Activate with | Key tools | When to use |
|--------|--------------|-----------|-------------|
| `rust` | `--tools rust` | cargo build/test/clippy/fmt/nextest/add | Rust/Cargo projects |
| `git` | `--tools git` | git status/commit/push/log/diff | Version control |
| `fileutils` | `--tools fileutils` | ls, cp, mv, rm, grep, find, diff | File operations |
| `python` | `--tools python` | python script execution | Python projects |
| `kotlin` | `--tools kotlin` | gradle build/test/lint | Android/Kotlin |
| `github` | `--tools github` | gh pr/issue/run/release | GitHub CLI operations |
| `simplify` | `--tools simplify` | Code complexity analysis | Code quality work |

**Specify bundles at startup** (tools visible immediately — no extra step required):
```json
"args": ["serve", "stdio", "--tools", "rust,git,fileutils"]
```

**Disable progressive disclosure entirely** (show all loaded tools, no `activate_tools`):
```json
"args": ["serve", "stdio", "--tools", "rust,git", "--disable-progressive-disclosure"]
```

---

## Built-in Tools (Always Available)

### `run_terminal_command` — Run any shell command

```
run_terminal_command(
  command="cargo build --release",
  working_directory="/path/to/project",
  timeout_seconds=300
)
```

- Runs inside the kernel sandbox (cannot write outside project scope)
- Supports pipes, redirects, variables, multi-command strings
- `monitor_level` ("error"/"warn"/"info") and `monitor_stream` ("stderr"/"stdout"/"both")
  trigger LLM log alerts when issues are detected

### `status` — Check async operation progress

```
status(operation_id="op_abc123")
```

Returns current state: `running`, `complete`, `failed`, `cancelled`, or `timeout`.
Non-blocking — safe to call repeatedly.

### `await` — Wait for an async operation to finish

```
await(operation_id="op_abc123", timeout_seconds=60)
```

Blocks until the operation completes or times out. Use sparingly — prefer `status` polling
when you want to continue other work in parallel.

### `cancel` — Cancel a running operation

```
cancel(operation_id="op_abc123")
```

Sends cancellation signal. The process is terminated and resources are freed.

---

## Async-First Workflow

Most tools run **asynchronously** by default — they return an `operation_id` immediately.

```
# 1. Start a long operation
result = cargo_build(subcommand="build")
# → { "operation_id": "op_abc123", "status": "started" }

# 2. Check progress (non-blocking)
status(operation_id="op_abc123")
# → { "status": "running", "output_so_far": "..." }

# 3. Wait for completion when needed
await(operation_id="op_abc123", timeout_seconds=120)
# → { "status": "complete", "exit_code": 0, "output": "..." }

# Or: cancel if taking too long
cancel(operation_id="op_abc123")
```

**Force synchronous** for state-modifying commands (e.g., `cargo add`):
- Set `"synchronous": true` in the tool's MTDF JSON, or
- Start server with `--sync` flag, or set `AHMA_SYNC=1`

---

## Sandbox — Filesystem Security

Ahma enforces **kernel-level** filesystem boundaries set once at startup.

### Scope Rules
- **STDIO mode**: Scope = `cwd` from mcp.json (usually `${workspaceFolder}`)
- **HTTP mode**: Scope = workspace roots from MCP `roots/list` response
- **Override**: `AHMA_SANDBOX_SCOPE=/path/a:/path/b` (colon-separated on Unix)

### Temp Directory
```json
"args": ["serve", "stdio", "--tmp"]   # or AHMA_TMP_ACCESS=1
```
Adds `/tmp` (or `%TEMP%` on Windows) to the scope. Required for compilers, build tools.

### Nested Sandbox Detection
If running inside Cursor, VS Code, or Docker, Ahma auto-disables its internal sandbox
(outer sandbox already provides protection). Override: `AHMA_DISABLE_SANDBOX=1` to
suppress the warning message.

### Platform Enforcement
- **Linux**: Landlock LSM (requires kernel 5.13+)
- **macOS**: `sandbox-exec` (Seatbelt, built-in)
- **Windows**: Job Objects + AppContainer (in progress)

---

## Terminal Hooks — Shell Interception & Security

For AI agents that run commands natively in your local terminal (like Claude Code, Cursor, Codex, or GitHub Copilot CLI), they execute commands directly in your shell rather than via an MCP server.

To extend Ahma's **kernel sandbox** to these native shell tools, you can install **managed terminal hooks**.

### How it works
1. **Intercept**: The hook intercepts bash/shell execution requests from the AI agent.
2. **Rewrite**: It wraps the command with `ahma hooks run-shell` and passes it to `ahma`'s sandboxed terminal runner.
3. **Execute**: The command runs inside the kernel sandbox, preventing escapes outside the allowed scope.

### Configuration Scopes
- **User scope** (machine-level): Configures the hook globally for all projects.
- **Project scope** (repo-level): Configures the hook only for the current project repository.

### Setup & Management

Use the `hooks` subcommand to configure and verify hooks:

```bash
# Check current hook installation status across all platforms
ahma hooks status

# Install user-scoped hooks for all supported tools (Cursor, Claude, Codex, Copilot)
ahma hooks install --scope user

# Install project-scoped hooks for GitHub Copilot specifically
ahma hooks install --platform copilot --scope project

# Uninstall hooks
ahma hooks uninstall --platform copilot --scope user
```

Supported Hook Platforms:
- **Cursor**: Configures `${HOME}/.cursor/hooks.json`
- **Claude Code**: Configures `${HOME}/.claude/settings.json`
- **Codex**: Configures `${HOME}/.codex/hooks.json`
- **GitHub Copilot / Copilot CLI**: Configures `${HOME}/.copilot/hooks/ahma.json` (user) and `.github/hooks/ahma.json` (project)

---

## Live Log Monitoring

Two flavors of log monitoring:

### 1. `--log-monitor` flag — Monitor Ahma's own server logs

```json
"args": ["serve", "stdio", "--log-monitor"]
```

Tails Ahma's rolling log files (`./log/ahma_mcp.log.*`), analyzes chunks with an LLM, and
pushes `LogAlert` MCP progress notifications when errors or anomalies are detected.

Configure minimum seconds between alerts: `--monitor-rate-limit 60` (default 60).

### 2. `livelog` tool type — Monitor any streaming command

For tools defined in `.ahma/` with `"tool_type": "livelog"`:
```json
{
  "name": "logcat",
  "tool_type": "livelog",
  "livelog": {
    "source_command": "adb",
    "source_args": ["-d", "logcat", "-v", "threadtime"],
    "detection_prompt": "Look for crashes, ANR errors, or exceptions.",
    "llm_provider": { "base_url": "http://localhost:11434/v1", "model": "llama3.2" },
    "chunk_max_lines": 50,
    "chunk_max_seconds": 30,
    "cooldown_seconds": 60
  }
}
```

Built-in examples (activate with `--tools`): `android-logcat`, `rust-log-monitor`.

---

## Custom Tools — `.ahma/` Directory

Place `*.json` files in `.ahma/` at the project root to define project-local tools.
Ahma auto-detects and loads them at startup. Override path: `AHMA_TOOLS_DIR=/path/to/dir`.

### Minimal MTDF tool definition

```json
{
  "name": "deploy",
  "description": "Deploy the application to staging",
  "command": "scripts/deploy.sh",
  "enabled": true,
  "synchronous": true
}
```

### With subcommands and options

```json
{
  "name": "myapp",
  "description": "Build and run the application",
  "command": "python",
  "subcommand": [
    {
      "name": "build",
      "description": "Build the app",
      "options": [
        { "name": "release", "type": "boolean", "description": "Optimized build" }
      ]
    },
    {
      "name": "run",
      "description": "Run the app",
      "options": [
        { "name": "port", "type": "integer", "description": "Port number", "default": 8080 }
      ]
    }
  ]
}
```

### Sequence tools (multi-step workflows)

```json
{
  "name": "check",
  "description": "Format, lint, and test in one command",
  "command": "sequence",
  "sequences": [
    { "tool": "cargo", "subcommand": "fmt", "args": { "all": true } },
    { "tool": "cargo", "subcommand": "clippy", "args": {} },
    { "tool": "cargo", "subcommand": "nextest_run", "args": {} }
  ]
}
```

Validate tool configs: `ahma tool validate .ahma/`

Hot-reload while authoring (dev only): `AHMA_HOT_RELOAD=1 ahma serve stdio`

---

## Key Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `AHMA_TOOLS_DIR` | `.ahma/` | Custom tools directory path |
| `AHMA_TIMEOUT` | `360` | Default tool timeout (seconds) |
| `AHMA_SYNC` | off | Force all tools synchronous |
| `AHMA_HOT_RELOAD` | off | Reload tool JSON on file change (dev only) |
| `AHMA_DISABLE_SANDBOX` | off | Disable kernel sandbox (UNSAFE) |
| `AHMA_SANDBOX_SCOPE` | cwd | Colon-separated scope paths |
| `AHMA_TMP_ACCESS` | off | Add temp dir to sandbox scope |
| `AHMA_DISABLE_TEMP` | off | Block all temp dir access |
| `AHMA_LOG_TARGET` | file | Set `stderr` to log to stderr |
| `AHMA_LOG_MONITOR` | off | Enable live log monitoring |
| `AHMA_MONITOR_RATE_LIMIT` | `60` | Min seconds between log alerts |
| `AHMA_PROGRESSIVE_DISCLOSURE_OFF` | off | Expose all tools immediately |
| `RUST_LOG` | `info` | Log verbosity (e.g., `ahma_mcp=debug`) |

Full reference: [environment-variables.md](https://github.com/paulirotta/ahma/blob/main/docs/environment-variables.md)

---

## CLI Reference

```bash
# Start MCP server (stdio — for IDE integration)
ahma serve stdio [--tools rust,git] [--tmp] [--log-monitor]

# Start HTTP server (local development, multiple clients)
ahma serve http [--port 3000] [--host 0.0.0.0] [--disable-quic]

# Start Unix socket server (IPC / Kubernetes sidecars)
ahma serve unix [--socket-path /tmp/ahma.sock]

# Run a single tool from the CLI
ahma tool run cargo_build -- --release
ahma tool run run_terminal_command -- "echo hello"

# Validate .ahma/ tool configs
ahma tool validate [.ahma/]

# List all configured tools
ahma tool list [--http http://localhost:3000] [--format json]

# Show locally configured tools with descriptions
ahma tool info [--tools rust,git]

# Local TLS certificate management (required for QUIC/HTTP3 transport)
ahma tls init      # Generate cert at ~/.ahma/tls/ (idempotent)
ahma tls rotate    # Replace the certificate with a new one
ahma tls status    # Show cert path, age, and rotation recommendation
```

---

## Common Recipes

### Rust project — full quality pipeline

```
activate_tools(action="reveal", bundle="rust")
activate_tools(action="reveal", bundle="git")
cargo_fmt(subcommand="fmt")
cargo_clippy(subcommand="clippy")
cargo_nextest_run(subcommand="nextest run")
```

### Run arbitrary shell commands

```
run_terminal_command(command="npm ci && npm run build", working_directory="/project")
run_terminal_command(command="docker compose up -d", timeout_seconds=60)
```

### Check what bundles are available

```
activate_tools(action="list")
```

Returns: bundle names, descriptions, AI hints for when each is useful.

### Monitor Android app logs

```
activate_tools(action="reveal", bundle="kotlin")
android_logcat(...)   # if defined in .ahma/android-logcat.json
```

---

## Troubleshooting

**Tool not found**: Call `activate_tools(action="list")` to see unrevealed bundles.
Then `activate_tools(action="reveal", bundle="<name>")`.

**Timeout**: Set `AHMA_TIMEOUT=600` in mcp.json env, or pass `timeout_seconds` per tool call.

**Permission denied / sandbox error**: The file is outside the sandbox scope.
Check `AHMA_SANDBOX_SCOPE` or add `--tmp` if needed for temp files.

**Nested sandbox warning**: Ahma detected an outer sandbox (Cursor, VS Code, Docker).
Internal sandbox auto-disabled. Set `AHMA_DISABLE_SANDBOX=1` to suppress the warning.

**Tool still running**: Use `status(operation_id)` to check, or `cancel(operation_id)`.

**Linux old kernel**: Landlock requires kernel 5.13+. Set `AHMA_DISABLE_SANDBOX=1` on
older systems (Raspberry Pi OS bullseye, etc.).

---

---

## User-Invocable Subcommands

The `/ahma` skill supports these user-invocable subcommands in chat:

| Command | Alias | Purpose |
|---------|-------|---------|
| `/ahma help` | `/ahma ?` | List all available subcommands and their usage |
| `/ahma tool list` | `/ahma tools` | Show all available tools in the current project |
| `/ahma simplify` | — | Auto-fix top 10 complexity issues concurrently via subagents |
| `/ahma simplify top N` | — | Auto-fix top N complexity issues concurrently |
| `/ahma simplify N` | — | Get fix instructions for issue #N only (manual mode) |
| `/ahma tui` | — | Start the terminal user interface (TUI) control plane |
| `/ahma update` | — | Update ahma to the latest version |

---

## `/ahma help` — List Subcommands

When the user types `/ahma help` or `/ahma ?`, respond with a concise list of all available
user-invocable subcommands and a one-line description of each:

```
/ahma help              — Show this help list
/ahma ?                 — Alias for /ahma help
/ahma tool list         — Show all available tools in the current project
/ahma simplify          — Auto-fix top 10 complexity issues concurrently via subagents
/ahma simplify top 5    — Auto-fix top 5 issues concurrently
/ahma simplify 3        — Manual mode: get fix instructions for issue #3 only
/ahma simplify rust     — Auto-fix top 10 Rust issues concurrently
/ahma tui               — Start the terminal user interface (TUI) control plane
/ahma update            — Update ahma to the latest version
```

Also mention the key flags for configure, e.g., `--tools`, `--tmp`, `--log-monitor`.

---

## `/ahma tool list` — List Configured Tools

### Syntax

```
/ahma tool list
/ahma tools
```

### Workflow

When the user runs `/ahma tool list` or `/ahma tools`, the agent lists all configured tools (both built-in bundles and local `.ahma/` configurations).

To list them, the agent:
1. Loads the tool configurations using `ahma tool info`.
2. Presents them in a clean markdown table, showing the tool name, description, and available subcommands.

---

## `/ahma tui` — Start the TUI Dashboard

### Syntax

```
/ahma tui
```

### Workflow

When the user runs `/ahma tui`, the agent starts the TUI in the user's terminal:

```bash
ahma tui
```

This opens the terminal dashboard for monitoring active operations, viewing logs, and approving gates.

---

## `/ahma update [ref]` — Update the Installed Binary

### Syntax

```
/ahma update                  # Install the latest published GitHub release
/ahma update 0.7.1            # Install a specific release tag (semver, with or without 'v')
/ahma update main             # Build and install from the main branch
/ahma update feature/update   # Build and install from a named feature branch
```

### What the ref means

| ref | Behaviour |
|-----|-----------|
| *(omitted)* | Downloads the latest pre-built release asset for the current platform |
| semver (e.g. `0.7.1`) | Downloads that specific release asset |
| branch name (e.g. `main`) | Runs `cargo install --git ... ahma_bin --branch <ref>` from GitHub source |

### Primary workflow — use the built-in `ahma update` subcommand

The `ahma update` subcommand handles platform detection, version comparison, `RUSTFLAGS`,
PATH hints, and the restart reminder automatically.  Always prefer it.

**Step 1 — run the update:**

```
run_terminal_command("ahma update feature/update")
```

Or for the latest release:

```
run_terminal_command("ahma update")
```

Branch installs compile from source and take several minutes.  Watch for the
`Installed /path/to/ahma` line to confirm success.

**Step 2 — verify the version:**

```
run_terminal_command("ahma --version")
```

The output must show the expected version (e.g. `ahma 0.7.0`).

**Step 3 — reload the IDE**

MCP clients cache the binary path. After a successful update, ask the user to:
- VS Code: run **Developer: Reload Window** (Cmd/Ctrl+Shift+P)
- Cursor: reload the window or restart the app

### Fallback — manual `cargo install` (only when `ahma update` is absent or broken)

Use this only if `ahma` is not yet installed or `ahma update` itself is broken:

**Linux / macOS:**

```bash
RUSTFLAGS='--cfg reqwest_unstable' \
  cargo install --git https://github.com/paulirotta/ahma \
    --branch feature/update ahma_bin --bin ahma --root ~/.local --locked --force
export PATH="$HOME/.local/bin:$PATH"
```

**Windows (PowerShell 5.1+):**

```powershell
$env:RUSTFLAGS='--cfg reqwest_unstable'
cargo install --git https://github.com/paulirotta/ahma `
  --branch feature/update ahma_bin --bin ahma --root $HOME\.local --locked --force
```

**Local checkout (iterating on ahma itself):**

```bash
RUSTFLAGS='--cfg reqwest_unstable' \
  cargo install --path ahma_bin --bin ahma --root ~/.local --locked --force
```

### Anti-patterns

- **Do NOT target the `ahma_mcp` package.** The `ahma` binary moved to `ahma_bin` in 0.7.0.
  `--path ahma_mcp` / `ahma_mcp` as the positional package will fail with
  `no bin target named ahma`.
- **Do NOT omit `RUSTFLAGS='--cfg reqwest_unstable'` for source builds.** The workspace uses
  `reqwest` with the `http3` feature, which refuses to compile without this flag.
  The `ahma update` subcommand sets it automatically; the manual snippets above show
  the exact form needed.
- **Do NOT skip the version check.** Run `ahma --version` and confirm it reports the
  expected value before declaring success.
- **Do NOT forget to reload the IDE.** An updated binary is not picked up by a running
  MCP session until the client restarts.

---

## `/ahma simplify` — Automatic Code Simplification

When the user types `/ahma simplify`, automatically analyze the codebase, identify the
top complexity issues, and spawn concurrent subagents to fix them — **no additional
prompting required**.

### Syntax

```
/ahma simplify                 # Auto-fix top 10 issues concurrently (DEFAULT)
/ahma simplify top 5           # Auto-fix top 5 issues concurrently
/ahma simplify rust            # Auto-fix top 10 Rust issues concurrently
/ahma simplify rust top 3      # Auto-fix top 3 Rust issues concurrently
/ahma simplify 3               # Manual mode: get fix prompt for issue #3 only
/ahma simplify kotlin 2        # Manual mode: Kotlin issue #2 only
```

**Mode selection rule:** If the command contains `top N` or has NO trailing integer,
use **auto mode** (concurrent subagents). If a bare trailing integer is given without
`top`, use **manual mode** (single-file sequential workflow).

Language names are case-insensitive and expand to their extensions automatically.

### Supported Languages

| Name | Extensions |
|------|------------|
| `rust` | `.rs` |
| `kotlin` | `.kt`, `.kts` |
| `swift` | `.swift` |
| `objc` / `objective-c` | `.m`, `.mm` |
| `python` | `.py` |
| `javascript` | `.js`, `.jsx` |
| `typescript` | `.ts`, `.tsx` |
| `java` | `.java` |
| `c++` / `cpp` | `.cpp`, `.cc`, `.hpp`, `.hh` |
| `c#` / `csharp` | `.cs` |
| `go` | `.go` |
| `html` | `.html`, `.htm` |
| `css` | `.css` |

**Kotlin analyzer cascade:** detekt-cli (standalone, preferred) → Gradle detekt (plugin required) → Lizard (universal fallback). Install `brew install detekt` or `pip install lizard` for zero-config Kotlin analysis.

**Swift analyzer cascade:** SwiftLint (cyclomatic + cognitive) → Lizard (cyclomatic only). Install `brew install swiftlint` for full Swift complexity analysis. `pip install lizard` as a lighter-weight alternative.

### Prerequisites

**Via MCP tool (preferred):** The `simplify` tool must be active — start Ahma with `--tools simplify`
or `--tools rust,simplify`.

**Via CLI:** `ahma simplify` is the subcommand. Run `ahma simplify --help` to verify.

### CRITICAL: Fail-Closed Rule

**If the `simplify` MCP tool is not available:**
1. Call `activate_tools(action="reveal", bundle="simplify")` to unlock it, OR
2. Run `ahma simplify <directory> --ai-fix 1` directly via the sandboxed shell.

**NEVER substitute shell heuristics** such as `find ... | wc -l` (line counts) or `wc -c` (file sizes) as a proxy for complexity. File length is not a complexity metric. Using it will produce incorrect rankings and mislead refactoring effort. If neither the tool nor the CLI is available, tell the user and stop — do not improvise.

---

### Auto Mode — Concurrent Simplification (DEFAULT)

This is the default when the user types `/ahma simplify` with no trailing integer.
The agent orchestrates the entire workflow automatically without additional prompting.

#### Phase 1 — Analyze (parent agent)

Run the complexity analysis once to get the full report:

**Via MCP tool:**
```
simplify(directory="<project-root>", ai_fix=1)
```

**Via CLI:**
```bash
ahma simplify <project-root> --ai-fix 1
```

If language filters were specified (e.g., `/ahma simplify rust`), add `--extensions rust`.

The output contains:
1. Overall project simplicity score (0–100%)
2. Ranked file list (worst first)
3. Function-level hotspots for the top issue
4. A structured fix prompt for issue #1

Parse the ranked file list to determine how many issues exist. Set `N` to
`min(requested_count, total_issues)` — default `requested_count` is 10.

Tell the user: "Analyzing codebase... Found N complexity issues. Spawning N
concurrent subagents to fix them."

#### Phase 2 — Spawn subagents (concurrent)

Spawn **one subagent per issue**, all concurrently. Each subagent is independent
and edits a different file, so there are no file conflicts.

**How to spawn depends on your environment.** Use the first strategy that works:

| If you have... | Then do... |
|----------------|------------|
| A subagent/agent spawning tool (e.g., `invoke_subagent`, `Agent` tool, `Task` tool) | Spawn N subagent tool calls **in the same response** so they run concurrently |
| Background task capability but no subagent tool | Launch N background tasks, one per issue |
| Neither | Run the N issues sequentially, one at a time |

**Each subagent receives this prompt** (fill in the template for each issue number):

```
You are simplifying a codebase. Your task is to fix complexity issue #<N>.

Project root: <PROJECT_ROOT>

## Step 1 — Get your fix instructions

Run this command to get the structured fix prompt for your assigned issue:

    ahma simplify <PROJECT_ROOT> --ai-fix <N>

Or via MCP tool:

    simplify(directory="<PROJECT_ROOT>", ai_fix=<N>)

Read the output. It contains:
- The exact file path to edit
- Hotspot functions (name, line range, metrics)
- A structured evaluation and fix prompt

## Step 2 — Read and evaluate the target file

Read the target file identified in the fix prompt. Evaluate critically:
- Are the hotspot functions genuinely hard to understand?
- Or is the complexity score driven by volume/enumeration (many match arms, config fields)?
- Would splitting them force readers to jump between more locations?

If the code is already clear and metrics are driven by volume rather than genuine
algorithmic complexity, report "No changes needed — complexity is structural, not
cognitive" and STOP.

## Step 3 — Apply focused changes (if warranted)

Constraints:
- Edit ONLY the hotspot functions listed in the fix prompt
- Do NOT refactor surrounding code
- Do NOT change function signatures, public APIs, or behavior
- Do NOT run cargo fmt, cargo clippy, or cargo test (the parent agent will do this)
- Prefer: early returns/guard clauses, helper extraction for self-contained logic,
  named predicates for complex boolean chains

For test files: skip unless a single test function is individually complex.

## Step 4 — Report

Report what you changed (file path, functions modified, patterns applied) or why
no changes were needed.
```

#### Phase 3 — Verify (parent agent, after ALL subagents complete)

After all subagents have finished, the parent agent runs a single verification pass:

1. **Format and lint:**
   ```bash
   cargo fmt --all && cargo clippy --all-targets
   ```

2. **Run tests:**
   ```bash
   cargo nextest run
   ```
   If tests fail, identify which subagent's changes caused the failure and revert
   or fix those specific changes.

3. **Re-analyze to show improvement:**
   ```bash
   ahma simplify <project-root> --ai-fix 1
   ```
   Report the before/after project simplicity score to the user.

4. **Summarize results** in a table:
   ```
   | Issue # | File | Action | Result |
   |---------|------|--------|--------|
   | 1 | src/foo.rs | Extracted 3 helpers | Improved |
   | 2 | src/bar.rs | No changes needed | Skipped |
   | ... | ... | ... | ... |
   ```

---

### Manual Mode — Single-Issue Workflow

Triggered when the user provides a bare trailing integer (e.g., `/ahma simplify 3`
or `/ahma simplify rust 2`). This follows the original sequential workflow for
targeted single-file work.

#### Step 1 — Run complexity analysis

**Via MCP tool:**
```
simplify(directory="<project-root>", ai_fix=<N>)
```

**Via CLI:**
```bash
ahma simplify <project-root> --ai-fix <N>
```

#### Step 2 — Read and follow the structured fix prompt

The `--ai-fix N` output ends with a structured prompt. It specifies:
- The exact file path to edit
- Hotspot functions (name, line range, metrics)
- Constraints on what to change

**Follow the prompt's constraints exactly:**
- Edit **only** the listed hotspot functions
- Do not refactor the whole file
- Do not change function signatures, public APIs, or behavior

#### Step 3 — Apply targeted changes

Common complexity-reduction patterns:
- Extract deeply nested logic into well-named helper functions
- Replace complex boolean chains with named predicates
- Replace long match/switch arms with lookup tables
- Flatten early-return cascades (guard clauses)

**For test files:** High test count is expected. Skip unless a single test function is individually complex.

#### Step 4 — Verify improvement

**Via MCP tool:**
```
simplify(directory="<project-root>", verify="<path-to-edited-file>")
```

**Via CLI:**
```bash
ahma simplify <project-root> --verify <path-to-edited-file>
```

| Verdict | Meaning |
|---------|---------|
| Significant improvement (≥10%) | Success — move to next issue |
| Modest improvement (1–9%) | Acceptable |
| No change | Hotspot functions may not have been modified |
| Regression | Revert and try a different approach |

#### Step 5 — Iterate

```
simplify(directory="<project-root>", ai_fix=<N+1>)
```

Continue until the project score is satisfactory.

---

### Score Interpretation

```
Score = 0.4 × MI + 0.3 × Cognitive Density + 0.2 × Peak Cognitive + 0.1 × Length Score
```

| Score | Status | Action |
|-------|--------|--------|
| 85–100% | Excellent | No action needed |
| 70–84% | Good | Fix only worst outliers |
| 55–69% | Fair | Plan a simplification sprint |
| 40–54% | Poor | Prioritize before new features |
| 0–39% | Critical | Address now |

### MCP Tool Reference (`simplify`)

| Argument | Type | Default | Purpose |
|----------|------|---------|---------|
| `directory` | path (required) | — | Project root to analyze |
| `ai_fix` | integer | — | Issue number for fix prompt (1 = worst file) |
| `limit` | integer | 50 | Issues to include in report |
| `verify` | path | — | Re-analyze a file vs. baseline |
| `extensions` | array | all | Restrict to file types (e.g. `["rs","py"]`) |
| `exclude` | array | — | Additional glob patterns to exclude |
| `output_path` | path | — | Write report to directory instead of stdout |
| `html` | boolean | false | Also generate HTML report |

### CLI Quick Reference

```bash
# Analyze and get fix prompt for worst file
ahma simplify . --ai-fix 1

# Rust files only
ahma simplify . --extensions rust --ai-fix 1

# Multiple languages
ahma simplify . --extensions rust,python --ai-fix 1

# 2nd worst file
ahma simplify . --ai-fix 2

# Verify improvement after editing
ahma simplify . --verify src/my_module.rs

# Full report to file
ahma simplify . --output-path ./reports

# HTML report
ahma simplify . --html

# Exclude generated code
ahma simplify . --exclude '**/generated/**,**/vendor/**' --ai-fix 1
```

### Anti-Patterns to Avoid

1. **Do not refactor the whole file** — follow the hotspot list exactly.
2. **Do not add comments to improve scores** — structural change is needed.
3. **Do not inline complex logic** — fewer functions with more complexity each makes scores worse.
4. **Do not run `--ai-fix` without reading the structured prompt.**
5. **Do not skip verification** — complexity improvements must be confirmed by metrics.
6. **Do not have subagents run cargo fmt/clippy/test** — the parent agent runs these once after all subagents complete to avoid build lock contention.

---

**See also**: [security-sandbox.md](https://github.com/paulirotta/ahma/blob/main/docs/security-sandbox.md) ·
[live-log-monitoring.md](https://github.com/paulirotta/ahma/blob/main/docs/live-log-monitoring.md) ·
[connection-modes.md](https://github.com/paulirotta/ahma/blob/main/docs/connection-modes.md) ·
[environment-variables.md](https://github.com/paulirotta/ahma/blob/main/docs/environment-variables.md) ·
[mtdf-schema.json](https://github.com/paulirotta/ahma/blob/main/docs/mtdf-schema.json) ·
[SIMPLIFY.md](https://github.com/paulirotta/ahma/blob/main/SIMPLIFY.md)
