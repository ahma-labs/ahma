---
name: ahma
version: 0.16.6
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

<!-- version: 0.16.6 | author: Paul Houghton -->

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
- `ahma-http` — shared HTTP server on port 3000 (run `ahma serve http --tools git,fileutils --sandbox --log-monitor`)
- `ahma-unix` — shared HTTP server over Unix socket (run `ahma serve unix --socket-path /tmp/ahma.sock --tools git,fileutils --sandbox --log-monitor`)

You can copy or customize this for your own projects. Create `.vscode/mcp.json` in your project root and commit it. Every VS Code user
who opens the project gets Ahma configured automatically (prompted to trust once):

```json
{
  "servers": {
    "ahma": {
      "type": "stdio",
      "command": "ahma",
      "args": ["serve", "stdio", "--tools", "git,fileutils", "--sandbox", "--log-monitor"]
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
      "args": ["serve", "stdio", "--tools", "git,fileutils", "--sandbox"],
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
  "postCreateCommand": "curl -fsSL https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.sh | bash",
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
2. Ask which bundles they need (git, python, etc.)
3. Create the config and tell them to reload the window (`Developer: Reload Window`)

## Tool Bundles

Ahma groups command-line tools into logical bundles that can be loaded at startup using the `--tools` parameter.

### Available Bundles

| Bundle | Activate with | Key tools | When to use |
|--------|--------------|-----------|-------------|
| `fileutils` | `--tools fileutils` | ls, cp, mv, rm, grep, find, diff | File operations |
| `github` | `--tools github` | gh pr/issue/run/release | GitHub CLI operations |
| `git` | `--tools git` | git status/commit/push/log/diff | Version control |
| `python` | `--tools python` | python script execution | Python projects |
| `simplify` | `--tools simplify` | Code complexity analysis | Code quality work |

To enable bundles at startup:
```json
"args": ["serve", "stdio", "--tools", "git,fileutils"]
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
- Start server with `--sync` flag

---

## Sandbox — Filesystem Security

Ahma enforces **kernel-level** filesystem boundaries set once at startup.

### Scope Rules
- **STDIO mode**: Scope = `cwd` from mcp.json (usually `${workspaceFolder}`)
- **HTTP mode**: Scope = workspace roots from MCP `roots/list` response
- **Override**: `--sandbox-scope /path/a` CLI flag (repeat for multiple paths)

### Temp Directory
```json
"args": ["serve", "stdio", "--sandbox"]
```
Adds `/tmp` (or `%TEMP%` on Windows) to the scope. Required for compilers, build tools.

### Nested Sandbox Detection
If running inside Cursor, VS Code, or Docker, Ahma auto-disables its internal sandbox
(outer sandbox already provides protection). Use `--no-sandbox` flag to suppress
the warning message.

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

# Install user-scoped hooks for all supported tools (including Cursor)
ahma hooks install --scope user

# Install project-scoped hooks for GitHub Copilot specifically
ahma hooks install --platform copilot --scope project

# Uninstall hooks
ahma hooks uninstall --platform copilot --scope user
```

Supported Hook Platforms:
- **Cursor**: Configures `${HOME}/.cursor/hooks.json` (user) or `<repo>/.cursor/hooks.json` (project)
- **Claude Code**: Configures `${HOME}/.claude/settings.json` (user or `<repo>/.claude/settings.json` for project)
- **Codex**: Configures `${HOME}/.codex/hooks.json`
- **GitHub Copilot / Copilot CLI**: Configures `${HOME}/.copilot/hooks/ahma.json` (user) and `.github/hooks/ahma.json` (project)
- **Antigravity**: Configures `${HOME}/.gemini/config/hooks.json` (user) and `<repo>/.agents/hooks.json` (project)

> [!IMPORTANT]
> **Installed ≠ active.** `ahma hooks install` only writes the hook file. In the default `auto` mode the hook is *active* only when an ahma MCP server is detected for that client; otherwise commands pass through UNSANDBOXED. `ahma hooks status` prints the effective ACTIVE/INACTIVE verdict and why — always check it. Force with `AHMA_HOOKS=on|off`.

> [!NOTE]
> **Fail-safe, not silent.** If ahma is *active* but cannot sandbox a command (missing binary, no kernel support, unknown working directory), the command is **blocked**, not run unsandboxed — with an actionable message. `ahma hooks doctor` diagnoses it; `ahma hooks approve-unsandboxed` grants a loud, session-only override (cleared on reboot), and `ahma hooks revoke` clears it. The only silent pass-through is when ahma is *inactive* (off, or `auto` with no MCP server detected). Cursor hooks use `failClosed: false` so a crashed/absent hook binary never wedges your terminal.

> [!IMPORTANT]
> **Turning off ahma without breaking your terminal**: When you intentionally disable ahma (remove it from `mcp.json`, set `AHMA_HOOKS=off`, or run `ahma hooks uninstall`), the hook automatically passes commands to the default terminal. It never bricks your workflow. The three safe off-switches:
> 1. Remove/comment out the `ahma` entry in `~/.cursor/mcp.json`
> 2. Set `AHMA_HOOKS=off` in your shell environment (also accepts `AHMA_DISABLE_HOOKS=1`)
> 3. Run `ahma hooks uninstall --platform cursor`
>
> The hook reads `AHMA_HOOKS` and the MCP config at invocation time — no Cursor restart needed for options 1 and 2.

> [!NOTE]
> **Cursor and VS Code Hook Support**: Cursor supports shell execution hooks as of June 2026 via `~/.cursor/hooks.json` or `<project>/.cursor/hooks.json`. The installer configures them automatically. VS Code does not have hook support and is not configured.

> [!NOTE]
> **Hooks + MCP are complementary, not redundant.** Running both the terminal hooks and the ahma MCP server for the same client is **supported and safe** — they sandbox different command streams. The MCP server sandboxes the named tools the agent calls explicitly (`run_terminal_command`, file-tools, git, …); the hooks sandbox the shell commands the agent runs through its *native* terminal/Bash tool, which never pass through MCP. A command is only ever wrapped once (already-wrapped and MCP tool calls pass through untouched). The only tradeoff is a small per-command sandbox cold-start from hooks — if your agent only ever uses ahma's MCP tools and never its native terminal, you can drop hooks with `ahma hooks uninstall --scope user`.

---

## Live Log Monitoring

Two flavors of log monitoring:

### 1. `--log-monitor` flag — Monitor Ahma's own server logs

```json
"args": ["serve", "stdio", "--log-monitor"]
```

Tails Ahma's rolling log files (`./logs/ahma.log.*`), analyzes chunks with an LLM, and
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

Built-in examples (activate with `--tools`): `android-logcat`.

---

## Custom Tools — `.ahma/` Directory

Place `*.json` files in `.ahma/` at the project root to define project-local tools.
Ahma auto-detects and loads them at startup. Override path via `--tools-dir /path/to/dir`.

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

Hot-reload while authoring (dev only): `ahma serve stdio --hot-reload`

---

## Key CLI Flags and Settings

> [!IMPORTANT]
> `AHMA_*` **configuration** environment variables are **retired** (R-CFG1.2) and ignored by the
> `ahma` binary. Use CLI flags (in `mcp.json` `args`) or `~/.ahma/settings.toml` instead.
> This does not cover terminal-hook variables (`AHMA_HOOKS`, `AHMA_DISABLE_HOOKS`,
> `AHMA_PREFER_OWN_SANDBOX`), which remain live, or the `scripts/install.sh`/`install.ps1`
> bootstrap installers, which run before any `ahma` binary exists. See
> [docs/environment-variables.md](../../docs/environment-variables.md) for the full picture.

| CLI flag / Settings key | Default | Purpose |
|----------|---------|---------|
| `--tools-dir` / `tools.tools_dir` | `.ahma/` | Custom tools directory path |
| `--timeout` / `tools.timeout_secs` | `360` | Default tool timeout (seconds) |
| `--sync` / `tools.force_sync` | off | Force all tools synchronous |
| `--hot-reload` / `tools.hot_reload` | off | Reload tool JSON on file change (dev only) |
| `--no-sandbox` / `sandbox.disable` | off | Disable kernel sandbox (UNSAFE) |
| `--sandbox-scope` / `sandbox.scopes` | cwd | Sandbox scope paths |
| `--sandbox` / `sandbox.use_sandbox_directory` | off | Add ~/sandbox as persistent secondary scope |
| `--tmp` / `sandbox.tmp_access` | off | Add temp dir to sandbox scope (opt-in) |
| `--disable-temp-files` / `sandbox.disable_temp` | off | Block all temp dir access |
| `--no-package-cache-write` | off | Disable cargo cache writes (strictest isolation) |
| `--log-to-stderr` / `logging.target` | file | Log to stderr |
| `--log-monitor` / `logging.log_monitor` | off | Enable live log monitoring |
| `--monitor-rate-limit` / `logging.monitor_rate_limit_secs` | `60` | Min seconds between log alerts |
| `RUST_LOG` (env, PLATFORM) | `info` | Log verbosity (e.g., `ahma_mcp=debug`) |

Full reference: [environment-variables.md](https://github.com/paulirotta/ahma/blob/main/docs/environment-variables.md)

---

## CLI Reference

```bash
# Start MCP server (stdio — for IDE integration)
ahma serve stdio [--tools git,fileutils] [--sandbox] [--log-monitor]

# Start HTTP server (local development, multiple clients)
ahma serve http [--port 3000] [--host 0.0.0.0] [--disable-quic]

# Start Unix socket server (IPC / Kubernetes sidecars)
ahma serve unix [--socket-path /tmp/ahma.sock]

# Run a single tool from the CLI
ahma tool run run_terminal_command -- "echo hello"

# Validate .ahma/ tool configs
ahma tool validate [.ahma/]

# List all configured tools
ahma tool list [--http http://localhost:3000] [--format json]

# Show locally configured tools with descriptions
ahma tool info [--tools git,fileutils]

# Local TLS certificate management (required for QUIC/HTTP3 transport)
ahma tls init      # Generate cert at ~/.ahma/tls/ (idempotent)
ahma tls rotate    # Replace the certificate with a new one
ahma tls status    # Show cert path, age, and rotation recommendation
```

---

## Common Recipes

### Git project — full version control pipeline

```
git_status(subcommand="status")
git_commit(subcommand="commit", message="Update docs")
git_push(subcommand="push")
```

### Run arbitrary shell commands

```
run_terminal_command(command="npm ci && npm run build", working_directory="/project")
run_terminal_command(command="docker compose up -d", timeout_seconds=60)
```

### Monitor Android app logs

```
android_logcat(...)   # if defined in .ahma/android-logcat.json
```

---

## Troubleshooting

**Tool not found**: Make sure the bundle is specified in the `--tools` parameter at startup (e.g., `--tools git,fileutils`).

**Timeout**: Increase via `--timeout 600` in mcp.json args, or set `tools.timeout_secs = 600` in `~/.ahma/settings.toml`.

**Permission denied / sandbox error**: The file is outside the sandbox scope.
Check `--sandbox-scope` CLI flag or add `--sandbox` to include ~/sandbox as a persistent scratch space, or `--tmp` if temp file access is needed.

> **Cargo dependency errors**: If `cargo add` or `cargo update` fail with permission errors, do **not** add `--sandbox-scope ~/.cargo` to your `mcp.json` — that grants write to the entire cargo home including binaries and credentials.  Instead, the built-in `package_cache_write` feature (on by default) handles this correctly, granting write only to `registry/`, `git/`, and the cargo lock files.  If you previously had `--sandbox-scope ~/.cargo` in your config, remove it — it is no longer needed.

> **Tool installs (`cargo install` / `cargo binstall`, `npm i -g`, …)**: These write into `~/.cargo/bin` and update an install manifest (`~/.cargo/.crates.toml`), which are read-only by default, so they fail with `Operation not permitted (os error 1)`.  This is expected — there is no special flag.  ahma detects the denied path and returns a `sandbox_denial` error: call the `sandbox_grant` tool with the named path (preview, then `confirm: true`), run the `restart` tool to apply, then re-run the command.  In a hooked native terminal the recovery is the CLI equivalent: `ahma sandbox grant <path>`, then re-run.  To avoid grants entirely, install into the workspace: `cargo install --root <workspace>/.tools`.

**Nested sandbox warning**: Ahma detected an outer sandbox (Cursor, VS Code, Docker).
Internal sandbox auto-disabled. Use `--no-sandbox` flag to suppress the warning.

**Tool still running**: Use `status(operation_id)` to check, or `cancel(operation_id)`.

**Linux old kernel**: Landlock requires kernel 5.13+. Use `--no-sandbox` on
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
| `/ahma uninstall` | — | Remove integrations installed by `ahma setup` (MCP entries, hooks, skills, binary) |

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
/ahma uninstall         — Remove integrations installed by ahma setup
```

Also mention the key flags for configure, e.g., `--tools`, `--sandbox`, `--log-monitor`.

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
1. Loads the tool configurations using `ahma tool info`. (Note: Do NOT run `ahma tool list` directly on the CLI as it expects a connection to a running server and will fail. Always use `ahma tool info` to list local configurations).
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
/ahma update 0.15.2           # Install a specific release tag (semver, with or without 'v')
/ahma update main             # Build and install from the main branch
/ahma update <branch-name>    # Build and install from a named feature branch
```

### What the ref means

| ref | Behaviour |
|-----|-----------|
| *(omitted)* | Downloads the latest pre-built release asset for the current platform |
| semver (e.g. `0.15.2`) | Downloads that specific release asset |
| branch name (e.g. `main`) | Runs `cargo install --git ... ahma_bin --branch <ref>` from GitHub source |

### Primary workflow — use the built-in `ahma update` subcommand

The `ahma update` subcommand handles platform detection, version comparison, `RUSTFLAGS`,
PATH hints, and the restart reminder automatically.  Always prefer it.

**Step 1 — run the update:**

```
run_terminal_command("ahma update")
```

Or to build from a specific branch (replace `<branch-name>` with the real branch, e.g. `main`):

```
run_terminal_command("ahma update <branch-name>")
```

Branch installs compile from source and take several minutes.  Watch for the
`Installed /path/to/ahma` line to confirm success.

**Step 2 — verify the version:**

```
run_terminal_command("ahma --version")
```

The output must show the expected version (e.g. `ahma 0.15.2`).

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

## `/ahma uninstall` — Remove Installed Integrations

Symmetrically reverses `ahma setup`: removes MCP server entries, terminal hooks, agent skills
and/or the ahma binary.  Only Ahma-managed keys and files are touched; other user config is
preserved.

### Syntax

```
/ahma uninstall                  # Interactive wizard (prompts for what and which platforms)
/ahma uninstall --auto           # Non-interactive: remove everything from all platforms
/ahma uninstall --mcp --platform cursor,claude  # Remove only Cursor + Claude Code MCP entries
/ahma uninstall --auto --dry-run  # Preview removals without writing files
/ahma uninstall --auto --purge    # Also delete ~/.ahma data directory (TLS, prompts, logs)
```

### Flags

| Flag | Description |
|------|-------------|
| `-y` / `--auto` | Skip prompts, remove everything |
| `--mcp` | Remove only MCP server entries |
| `--hooks` | Remove only terminal hooks |
| `--skills` | Remove only agent skills and Claude Code plugin |
| `--binary` | Remove the ahma binary from the install dir |
| `--platform <list>` | Comma-separated platforms to target |
| `--purge` | Also remove `~/.ahma` data dir (TLS, settings, logs) |
| `--dry-run` | Print planned changes without modifying files |

After uninstall, the wizard prints a list of tools to restart.  Background ahma servers
will self-terminate once no IDE or TUI client is connected.

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
or `--tools git,simplify`.

**Via CLI:** `ahma simplify` is the subcommand. Run `ahma simplify --help` to verify.

### CRITICAL: Fail-Closed Rule

**If the `simplify` MCP tool is not available:**
1. Ensure `simplify` is listed in `--tools` at startup (e.g. `--tools git,simplify`), OR
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

> [!IMPORTANT]
> **Antigravity Environment**: In Antigravity, there is no general-purpose code subagent spawning tool (the only subagent tool is `browser_subagent` which is for browser tasks only).
> Therefore, you **MUST** run the N issues **sequentially, one at a time** yourself.
> 
> To ensure reliability and prevent token exhaustion:
> 1. **Default to N = 1** (the single worst file). Only proceed to N > 1 if the user explicitly requested it (e.g., `/ahma simplify top 3`).
> 2. **Implement one file at a time**. Edit the target file, verify the improvement using `ahma simplify <project-root> --verify <file>`, and check/test compilation.
> 3. **Obtain user approval before proceeding to the next file**. Do NOT attempt to refactor multiple files in a single turn without stopping. Always pause, report progress, run tests, and ask the user before editing subsequent files.

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
