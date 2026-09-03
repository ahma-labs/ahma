# Ahma

_Use your existing command line workflows through MCP with a repo-scoped sandbox, async execution, and less pressure to fall back to insecure terminal access._

## Why Ahma helps

- **When the agent only needs the repo, broad terminal access is too much**: ahma starts inside a kernel-enforced workspace boundary, so normal project work does not require wider filesystem access.
- **When builds, tests, and checks take time, blocked agents waste time**: ahma runs commands async-first so long-running work can continue in the background while the agent keeps moving.
- **When independent tasks are forced through one terminal, work gets serialized**: ahma can start separate operations concurrently and track them cleanly.
- **When safety is noisy, people disable it**: ahma aims to make the safe path the practical path, reducing pressure to use broad or insecure override modes just to get work done.

|                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |                                     |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------: |
| [![CI](https://github.com/paulirotta/ahma/actions/workflows/build.yml/badge.svg)](https://github.com/paulirotta/ahma/actions/workflows/build.yml) [![Coverage Report](https://img.shields.io/badge/Coverage-Report-blue)](https://paulirotta.github.io/ahma/html/) [![Rust Docs](https://img.shields.io/badge/Rust-Docs-blue)](https://paulirotta.github.io/ahma/doc/) [![Code Simplicity](https://img.shields.io/badge/Code-Simplicity-green)](https://paulirotta.github.io/ahma/CODE_SIMPLICITY.html) [![Prebuilt Binaries](https://img.shields.io/badge/Prebuilt-Binaries-blueviolet)](https://github.com/paulirotta/ahma/actions/workflows/build.yml?query=branch%3Amain+event%3Apush+is%3Asuccess) [![License: Per Crate](https://img.shields.io/badge/License-Per--Crate-6f42c1)](#license) [![Rust](https://img.shields.io/badge/Rust-1.93%2B-B7410E.svg)](https://www.rust-lang.org/) | ![Ahma Logo](./assets/ahma.png) |

Ahma is an MCP server for running real project work through existing CLI tools with tighter filesystem boundaries and less blocking. It is aimed at the common case: builds, tests, formatters, git operations, log inspection, and other deterministic command-line tasks that agents already try to run.

## Quickstart

**Linux / macOS — first-time install**

```bash
curl -sSf https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.sh | bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc  # or ~/.bashrc — reload your shell after
```

The installer also runs `ahma setup`, which configures MCP entries and agent skills for the
editors it detects (Cursor, VS Code, Claude Code, …) — restart your editor afterward. [Terminal
hooks](#terminal-hooks) are opt-in and not part of this default (pass `--hooks`, or select them
at the prompt) since they're experimental and could interfere with your workflow until
sandbox-exception handling is fully hardened. Run `ahma setup` again any time to reconfigure, or
see [MCP Server Connection Modes](#mcp-server-connection-modes) below to wire up `mcp.json` by
hand.

**Windows (PowerShell 5.1+) — first-time install**

```powershell
irm https://raw.githubusercontent.com/paulirotta/ahma/main/scripts/install.ps1 | iex
```

**Update an existing install:**

```bash
ahma update              # latest release
ahma update main         # build from branch
```

<details>
<summary><strong>Advanced — install a specific branch (requires <a href="https://rustup.rs/">Rust</a>)</strong></summary>

Use this if you need to test an unreleased branch before the next binary release. Replace
`<branch-name>` below with the real branch you want (e.g. `main`).

The workspace uses `reqwest` with the `http3` feature, so source builds require `RUSTFLAGS='--cfg reqwest_unstable'`. The `ahma update <branch-name>` command sets this automatically; the snippets below are only needed if you are installing for the first time without an existing `ahma` binary.

**Linux / macOS**

```bash
# First time (no ahma yet)
RUSTFLAGS='--cfg reqwest_unstable' \
  cargo install --git https://github.com/paulirotta/ahma --branch <branch-name> ahma_bin --bin ahma --root ~/.local --locked --force
export PATH="$HOME/.local/bin:$PATH"

# After ahma is installed — the subcommand handles RUSTFLAGS automatically
ahma update <branch-name>
```

**Windows (PowerShell 5.1+)**

```powershell
$env:RUSTFLAGS='--cfg reqwest_unstable'
cargo install --git https://github.com/paulirotta/ahma --branch <branch-name> ahma_bin --bin ahma --root $HOME\.local --locked --force
```

</details>

<details>
<summary><strong>Alternative — install with Cargo (<code>cargo binstall</code> or <code>cargo install</code>)</strong></summary>

Prefer the Rust toolchain to the curl/irm installer? Both routes below install the same
`ahma` binary. **The interactive customization is identical** — it lives in the `ahma setup`
wizard (MCP entries, agent skills, optional terminal hooks/TLS), which the shell installer
simply runs for you at the end. Run it yourself after either command, and re-run it any time
to reconfigure:

```bash
ahma setup          # interactive wizard (same prompts as the curl installer)
ahma setup --auto   # non-interactive defaults
```

**Prebuilt, attested binary — no compile** (needs [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall); works on Linux, macOS, and Windows):

```bash
# cargo binstall downloads the CI-built, Sigstore-attested GitHub Release asset — no toolchain, no RUSTFLAGS.
cargo binstall --git https://github.com/paulirotta/ahma ahma_bin
ahma verify --self   # confirm the SLSA-3 build-provenance attestation (binstall skips the installer's auto-verify)
ahma setup
```

**From source** (needs [Rust](https://rustup.rs/)):

```bash
# http3/QUIC needs the reqwest_unstable cfg; the curl installer and `ahma update` set it for you,
# but a bare `cargo install --git` does not read the repo's .cargo/config.toml, so pass it here:
RUSTFLAGS='--cfg reqwest_unstable' \
  cargo install --git https://github.com/paulirotta/ahma ahma_bin --locked
ahma setup
```

Notes:
- `cargo binstall ahma` / `cargo install ahma` from **crates.io** are not yet available (ahma isn't published there) — use the `--git` forms above.
- Ensure `~/.cargo/bin` is on your `PATH` (rustup adds it during setup).
- **macOS:** a Cargo-installed binary is ad-hoc signed. If it is ever `SIGKILL`ed under memory pressure, re-sign it once with the hardened runtime (the curl installer does this automatically): `codesign --force --sign - --options runtime "$(command -v ahma)"`.

</details>

See [docs/installation.md](docs/installation.md) for platform details, source builds, and branch installs from local checkouts.

### Example workflow

Ask your agent to run a normal project task such as:

> Run formatters, linting, tests, and a build for this repo. Start independent steps concurrently where possible and keep me updated on failures.

With ahma, that workflow stays inside the repo boundary and the long-running steps can begin immediately as background operations. The agent can inspect results, continue other work, or start additional safe commands without waiting on one giant terminal session.

### Without ahma / with ahma

| Workflow detail | Without ahma | With ahma |
|---|---|---|
| **Filesystem access** | Often tied to a broad terminal with a larger blast radius | Kernel-enforced to the workspace scope |
| **Approval friction** | Repeated trust decisions or pressure to relax safety settings | Repo-scoped access is established up front |
| **Long-running work** | One blocked terminal session at a time | Async-first operations with status tracking |
| **Parallel tasks** | Often serialized | Independent tasks can start and run concurrently |
| **Operational visibility** | Raw terminal output | Operation IDs, progress notifications, and structured tool calls |

### What Ahma does

Ahma complements IDE and CLI MCP clients by making normal command-line work safer and less blocking. It is most useful where the client either exposes a broad terminal directly or has no terminal model at all.

| Capability | Native IDE/CLI terminal | Ahma `run_terminal_command` |
|---|---|---|
| **Write protection** | None — full filesystem access | Kernel-enforced to workspace only (Seatbelt on macOS, Landlock on Linux) |
| **Async execution** | Synchronous — AI blocks until done | Async-first — AI continues working while commands run in background |
| **Parallel operations** | Sequential tool calls | True concurrent operations with per-operation status tracking |
| **Structured tool schema** | Raw shell strings | Typed parameters, validation, subcommands via `.ahma/*.json` |
| **Progressive disclosure** | All tools always listed | Bundles revealed on demand — preserves AI context window |
| **Live log monitoring** | Raw output only | Pattern-matched alerts streamed to AI (error/warn/info levels) |
| **PoLP enforcement** | Any command, any argument | Call directly, or define a JSON file to restrict which arguments can be passed to a command line tool |

## OS Support

- **macOS** — Full support with kernel-level sandboxing (Seatbelt). Prebuilt binaries are Apple Silicon only; on Intel Macs use [Source Installation](#source-installation) below.
- **Linux (Ubuntu, RHEL)** — Intel and ARM. Full support with Landlock (kernel ≥ 5.13)
- **Raspberry Pi** — 64-bit and 32-bit. Use `--no-sandbox` until kernel-level sandboxing is supported (Landlock requires kernel ≥ 5.13)
- **Windows** — Full support. Uses the built-in PowerShell (5.1+) included with Windows 10/11

## Source Installation

If you prefer to build from source (required for Intel Macs, since prebuilt binaries are Apple Silicon only):

**Linux / macOS**

```bash
git clone https://github.com/paulirotta/ahma.git
cd ahma
cargo build --release -p ahma_bin
mkdir -p ~/.local/bin
mv target/release/ahma ~/.local/bin/
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc  # or ~/.bashrc — reload your shell after
```

**Windows (PowerShell)**

```powershell
git clone https://github.com/paulirotta/ahma.git
cd ahma
cargo build --release -p ahma_bin
Copy-Item target\release\ahma.exe "$HOME\.local\bin\"
```

See [docs/installation.md](docs/installation.md) for supported binary platforms and installer behavior.

## Security Sandbox

Ahma enforces **kernel-level filesystem sandboxing** by default — Landlock on Linux, Seatbelt on macOS, Job Objects on Windows. The sandbox scope is set once at startup and cannot be changed. The AI has full access within the workspace, and **writes** outside it are blocked unconditionally on Linux and macOS. *Reads* are a different story per platform — see [What the sandbox does *not* cover](#what-the-sandbox-does-not-cover) below.

**Network egress** is unrestricted by default. Pass `--restrict-network` (or set `[network] restrict = true`) to route every sandboxed subprocess through a guarded local proxy that forwards only the allowed hosts and refuses private/loopback/cloud-metadata addresses. When a subprocess reaches a host not on the list and an MCP client capable of `elicitation/create` is attached (e.g. an IDE), the proxy raises an interactive approval prompt instead of denying outright — the human can allow it once, for the session, or persist it to `[network] allow`; declining, a timeout, or no capable client all fail safe to a deny. Ahma's own web tool (`fetch_webpage`) is governed separately by the `[web]` policy.

The reachable set is the **union** of your `[network] allow` and the hosts each enabled sandbox profile declares its toolchain needs — `index.crates.io` and `static.crates.io` for Rust, `registry.npmjs.org` for Node, `proxy.golang.org` for Go, each shipped with a stated reason. Without that, turning restriction on broke `cargo build` on the very first command, which is why almost nobody turned it on. Every host is listed with the source that granted it (`ahma permissions list`, and at startup), so you can see which single line removes it: `[network] deny_profile_hosts` drops one profile's hosts, `[network] profile_hosts = false` drops all of them, and both keep the profile's *file* access — trusting a toolchain with a directory is not the same as trusting it with the internet. Everything is denied only when `allow` is empty **and** no profile contributes a host. See [docs/network-egress.md](docs/network-egress.md).

**When the sandbox blocks something you actually wanted**, ahma asks you — in your IDE if it can prompt, in the ahma TUI if one is attached, and otherwise by failing the command with the exact `ahma sandbox grant …` line that fixes it. It never fails *open*: if nobody can be asked, the answer is no. Grants live in `~/.ahma/settings.toml`, the one directory the sandbox never includes — so a sandboxed command can never grant itself anything.

See [docs/permissions.md](docs/permissions.md) for how permissions work, and [docs/security-sandbox.md](docs/security-sandbox.md) for platform details, nested sandbox detection, temp directory access, and example `mcp.json` configs.

### What the sandbox does *not* cover

Ahma sandboxes the **real host process in place** — there is no image, no rootfs, no VM. That is its strength (near-instant startup; per-session scope and egress that can be tailored per run) and the source of its limits. It is **not** a full container/VM isolation boundary:

- **Network restriction is kernel-enforced on macOS and modern Linux, advisory elsewhere.** `--restrict-network` routes HTTP(S) through the guarded proxy via `HTTP_PROXY`. On **macOS** the Seatbelt profile *denies all outbound IP egress except the proxy*, so even a tool that ignores `HTTP_PROXY` or opens a raw socket cannot reach the network directly. On **Linux** (kernel **6.7+**) Landlock restricts each sandboxed subprocess's outbound **TCP** to the proxy port — port-only (Landlock can't filter by address) and TCP-only, so **UDP** and a rogue service on the same port number are residual gaps; on kernels **< 6.7** it degrades to advisory. On other platforms it is advisory (a tool that bypasses the proxy env vars is not contained). On all, the proxy itself allow-lists domains and blocks private/loopback/cloud-metadata targets; for hard, complete confinement run ahma inside a container/VM.
- **No resource limits.** Unlike a container's cgroups, ahma does not cap CPU, memory, PIDs, or I/O — a runaway build can exhaust host resources. (A container *memory limit* is a cgroup ceiling with OOM-kill on breach, not a reservation; both a container and ahma allocate host memory dynamically and share the host kernel, so "fixed vs dynamic memory" is **not** a real difference — the difference is that a container *can* cap it and ahma does not.)
- **No process/namespace isolation.** A sandboxed tool shares the host PID, network, and user namespaces: it can see and signal other host processes and bind local ports. There is no seccomp syscall filtering and no UID remapping.
- **On macOS the sandbox is a write boundary, not a read boundary.** APFS firmlinks defeat read subpath matching, so the Seatbelt profile grants global file-*read* — writes stay scoped, reads are wide open. A denylist compensates: credential directories and private key material are explicitly denied, but a denylist only denies what someone thought to name, so treat it as narrower than a container's mount namespace, not equivalent to it. On **Linux** reads *are* scoped (Landlock); on **Windows**, treat neither reads nor writes as OS-confined. Job Objects give process containment, not a path boundary. AppContainer spawn isolation is *written* — per-command container, scoped ACL grants, launcher re-entry — and a `windows-latest` CI run has now executed it and **disproved** it: the scoped grant does not take effect, so writes were denied *inside* the locked scope as well as outside it. A boundary that blocks everything proves nothing, so it is switched off rather than shipped broken, and Windows currently has no OS-enforced path boundary at all. Two consequences to know for the day it is proven and switched back on: `--restrict-network` becomes **mutually exclusive** with it (an AppContainer blocks loopback, so the egress proxy would be unreachable; ahma refuses the combination loudly rather than pretending egress is gated), and `%TEMP%` is redirected into the container's own folder. While it is off, `--restrict-network` works on Windows exactly as it does elsewhere. The login **keychain** is *allowed* by default (read+write) so `gh` and other Keychain-backed tools work — it is encrypted at rest and secret extraction is still gated by `securityd`; set `[sandbox] allow_keychain = false` (or `--no-allow-keychain`) to block it. See SPEC R6.2.2/R6.2.3.
- **Files the agent legitimately writes can be run later by something outside the sandbox.** This is the interesting one, and it does not require breaking the sandbox at all: a git hook, an editor task set to run on folder-open, a harness settings hook, a planted `bin/python` a language-extension picks up. The agent writes a file it is entitled to write; your `git`, your IDE's extension host, or your harness executes it afterwards. Ahma's answer is two-tier — **deny writes** to paths no legitimate task touches (hook directories under the real git dir, container daemon sockets, key material, ahma's own config), and **allow but disclose loudly** for paths you genuinely ask an agent to edit (editor and harness configuration), naming the file *and* the trigger that will execute it. Blocking the second set would break "set up my editor for this project"; prompting on every one of them is the permission fatigue ahma exists to avoid. Two deny-tier paths *do* have legitimate authors — installing a repo's own git hook, and editing a project's `.ahma/` tool definitions — so each has a narrow opt-in, off by default and announced at startup when on: `--allow-git-hooks` and `--allow-project-tool-config` (or the matching `[sandbox]` keys). The denial message names the flag, so you find it when you hit it rather than by searching these docs. **Enforcement is uneven and you should know where:** the deny tier is kernel-enforced on macOS, but on Linux it is application-layer only (Landlock can't carve a deny hole inside a directory it has allowed), so a shell command run through `run_terminal_command` can still write those paths; on Windows there is no proven filesystem enforcement yet. Whichever tier applies, the write is recorded: every execution — and every sandbox denial — appends to `audit.jsonl` next to the operation logs, written *before* the process starts, so the record survives a crash, a kill, or a session you have long since closed. Output tells you what a command printed; this is what tells you it happened. See SPEC R-HANDOFF and [docs/execution-audit-log.md](docs/execution-audit-log.md).
- **The shared package cache is a cross-project channel.** Cargo needs write access to `$CARGO_HOME/registry` and `$CARGO_HOME/git` for `cargo add`/`cargo update` to work at all, and the shipped Rust profile grants it by default. That cache is shared by every project on your machine, so an agent working in one project can edit an extracted crate's source and have it compiled and run — as a build script or proc macro — the next time you build a *different* project. Narrowing the workspace scope does not help, because the cache was never inside it. Pass `--no-package-cache-write` (or `[sandbox] package_cache_write = false`) to downgrade those caches to read-only; the toolchain still runs, only `cargo add`/`cargo update` stop working inside the sandbox. Related: build scripts and proc macros are ordinary programs that run with the sandbox's full write set — adding a dependency is adding code that executes locally. See SPEC R-HANDOFF.8.

For hard multi-tenant isolation or resource governance, run ahma **inside** a container/VM — the two compose. Ahma's job is a fast, in-place, scope- and egress-tailored guard for an agent working on your own machine, not a substitute for full virtualization.

## Terminal Hooks

The ahma MCP server only sandboxes the tools an agent calls explicitly. **Terminal hooks** extend the same kernel sandbox to the shell commands an agent runs through its *native* terminal/Bash tool — which never pass through MCP. Supported clients: **Cursor, Claude Code, Codex, GitHub Copilot CLI, and Antigravity** (VS Code and Claude Desktop have no execution-hook mechanism).

```bash
ahma hooks install      # user-scoped hooks for all supported clients
ahma hooks status       # shows the EFFECTIVE state (active vs installed-but-inactive)
ahma hooks uninstall    # remove them
```

`install` only writes the hook file. In the default `auto` mode a hook is **active** only when an ahma MCP server is detected for that client; otherwise commands pass through **unsandboxed**. Always confirm with `ahma hooks status`, which prints the effective verdict and why. Force the behaviour with `AHMA_HOOKS=on|off` (alias `AHMA_DISABLE_HOOKS=1`).

**Fail-safe behaviour:** if ahma is *active* but cannot sandbox a command (broken install, missing kernel support), the command is **blocked**, not run unsandboxed — `ahma hooks doctor` diagnoses it and `ahma hooks approve-unsandboxed` grants a loud, session-only override. ahma never silently runs a command unsandboxed while it believes hooks are active.

See [docs/installation.md](docs/installation.md#terminal-hooks) for the per-client config paths and full details.

## Configuration Reference

Sandbox scope, logging, execution behaviour, and HTTP transport options are configured with **CLI flags** or `~/.ahma/settings.toml`. `AHMA_*` variables are **not** a configuration source — a client-owned config file can carry an environment variable, so security settings must not be reachable that way. See **[docs/environment-variables.md](docs/environment-variables.md)** for the retirement status of each variable and the handful that are still live.

## Live Log Monitoring

Ahma can run any streaming command (e.g. `adb logcat`, `tail -f`, `docker logs -f`) through an LLM to detect issues in real time. The tool returns an operation ID immediately; alerts are pushed as MCP progress notifications whenever the LLM finds a problem matching your description.

See [docs/live-log-monitoring.md](docs/live-log-monitoring.md) for setup, the Android logcat example, and how to use cloud or local LLM providers.

## Optional advanced topics

- **Custom tools**: If you want to expose your own command-line tools through ahma, start with [docs/custom-tools.md](docs/custom-tools.md).
- **Agent skills**: Optional agent-specific setup is documented in [docs/agent-skills.md](docs/agent-skills.md).
- **Code complexity analysis**: `ahma simplify` analyzes source files and returns structured AI fix instructions. See [SIMPLIFY.md](SIMPLIFY.md).

---

## v0.7 Experimental Features

The following capabilities were introduced in v0.7. They are functional and tested but their APIs and configuration formats may change before stabilisation. Each is opt-in — existing workflows are unaffected.

### Security rationale

Every v0.7 feature was designed around the principle that **the kernel sandbox is the trust boundary, not a classifier or a user-discipline rule**. The design was informed by documented weaknesses in cloud agent tools:

- Prompt injection can bypass any filter with non-zero probability. Ahma's response is to make the *consequences* of a successful injection bounded by the kernel sandbox scope, not to prevent injection entirely.
- Folder-level permission grants that survive a whole session give too much access for too long. Task vaults enforce the per-task folder discipline that responsible users already practice — but make it the only option.
- Network egress from agent subprocesses is not controlled by filesystem sandboxing alone. The egress sandbox adds a deny-by-default HTTP proxy layer.

### Task Vaults — isolated per-question working directories

```bash
VAULT=$(ahma vault create my-question)
ahma serve stdio --task-vault "$VAULT"
ahma vault list
```

Each vault gets its own kernel sandbox scope (`workdir/`), input copies, output directory, two-phase delete staging (`trash/`), and append-only audit log. There is no "grant my whole Documents folder" option — the vault is the only scope.

See [docs/task-vault.md](docs/task-vault.md).

### TUI — terminal dashboard and approval gates

```bash
ahma tui
ahma tui --connect http://localhost:8080
```

A terminal dashboard for monitoring active operations and handling approval gates (elevation requests, deletion confirmations, egress approvals).

- **Redesigned Monitor Mode (`/mode monitor`)**: Features a unified operations list with clickable/touchable `[Pin]` and `[Cancel]` buttons, a detailed operation inspector with a clickable `[Analyze]` button for AI analysis of outputs/logs, and inline log viewing.
- **Log Monitor Integration**: Type `/monitor file <path> [prompt]` in the chat input area to start a background log-monitoring operation using the built-in process-free tailing engine.
- **AI Analysis**: Type `/analyze [op_id]` or click `[Analyze]` on any operation to ask the AI for analysis of the operation's stdout and alerts.

See [docs/tui.md](docs/tui.md).

### Egress Sandbox — per-task outbound network control

Every vault has an `egress.allowlist` file. An HTTP proxy enforces it for all subprocess traffic. Default: deny all outbound connections. Local Ollama (localhost) is always excluded from the proxy.

See [docs/egress-sandbox.md](docs/egress-sandbox.md).

### Interactive HTML Artifacts

Tools can emit `outputs/result.html` — a self-contained artifact with embedded data, rendered tables, and a local-LLM chat widget. The user opens it in a browser and keeps iterating without re-engaging the agent.

See [docs/artifacts.md](docs/artifacts.md).

### Bundle Audit — supply-chain security for MTDF bundles

```bash
ahma bundle audit    /path/to/bundle
ahma bundle checksum /path/to/bundle
ahma bundle verify   /path/to/bundle
```

`audit` scans for embedded secrets, missing path validation, and prompt-injection payloads in tool JSON files before they are loaded. `checksum`/`verify` record and re-check a SHA-256 manifest — a **corruption** check, not a signature: the manifest is unsigned and sits inside the bundle, so anyone who can edit a bundle file can rewrite it too. Real tamper-evidence is the v0.8 signed bundle index, which is not implemented; `audit` is the control that exists today.

See [docs/bundle-audit.md](docs/bundle-audit.md).

### ahma_core — embedding Ahma in Rust applications

The `ahma_core` crate exposes the sandbox, MCP service, and local-LLM agent runtime as a library for embedding in other Rust applications.

See [docs/ahma-core-library.md](docs/ahma-core-library.md).

## MCP Server Connection Modes

`ahma` supports **STDIO** (default — IDE spawns a subprocess per workspace), **HTTP Bridge** (proxy for web clients and debugging), and **HTTP Streaming** (MCP Streamable HTTP with event replay and full-duplex).

See [docs/connection-modes.md](docs/connection-modes.md) for `mcp.json` examples for VS Code, Cursor, Claude Code, and Antigravity, plus HTTP streaming usage.

## Contributing

Issues and pull requests are welcome. This project is AI friendly and provides the following:

- **`AGENTS.md`/`CLAUDE.md`**: Instructions for AI agents to use the MCP server to contribute to the project.
- **`SPEC.md`**: This is the **single source of truth** for the project requirements. AI keeps it up to date as you work on the project.
- **[docs/build-and-test-performance.md](docs/build-and-test-performance.md)**: why `target/` used to grow to tens of GB, what the build/test layout does about it, and the measurements behind the rules in `AGENTS.md`.

## Working well with Claude (Sonnet / Opus)

Claude models treat strongly imperative language in tool descriptions ("MANDATORY", "do NOT use any other pathway", "under any circumstances") as a prompt-injection signal and downweight it. The recommended way to get Claude to route work through ahma is to add a decision-rule snippet to your workspace `AGENTS.md` or `CLAUDE.md`:

```markdown
## When to use ahma vs the native terminal

For commands run during this project, prefer ahma's `run_terminal_command` (via `CallMcpTool` on Cursor) when any of these apply:

- the command writes to disk — the kernel-enforced sandbox keeps writes inside the workspace
- the command runs for more than a few seconds — `run_terminal_command` is async, returns an operation_id, and lets the agent continue other work while it runs
- the output should be watched for errors mid-run — set `monitor_level` to get pushed alerts
- multiple independent commands should run concurrently — each gets its own operation_id

For reading, searching, and editing files (read, grep, glob, find, edit) keep using the IDE's native file tools — they are faster and cheaper than going through MCP. Clients with native file tools (Claude Code, Cursor, VS Code) don't see ahma's `read_file`/`write_file`/`replace_in_file`/`list_dir`/`file_search`/`grep_search` in `tools/list` at all; those stay available only to clients without native equivalents (ahma's own agent loop, the TUI).

The downstream effect: `cargo`, `git`, `pytest`, build scripts, formatters, and long log tails go through ahma; file reads and edits stay on native tooling.
```

Workspace-level rules in `AGENTS.md` reach Claude as operator-trusted content (higher weight than tool descriptions), and the capability-led bullets give it a clear decision rule rather than a mandate to override.

## License

Ahma is licensed **per crate**, not under a single repository-wide license.
The root `Cargo.toml` groups crates in one workspace, but each member crate's
`Cargo.toml` is the authoritative declaration for that crate.

## Licensing Architecture

Ahma uses a dual-tier licensing model to keep the core library reusable while ensuring the end-user product stays open-source.

| Crate | License | Why it is here |
|---|---|---|
| `ahma_mcp` | MIT OR Apache-2.0 | Core MCP service, sandbox, and command execution |
| `ahma_bundle` | MIT OR Apache-2.0 | Tool-bundle supply-chain audit and content checksum (`ahma bundle`) |
| `ahma_common` | MIT OR Apache-2.0 | Shared runtime types and configuration |
| `ahma_llm_monitor` | MIT OR Apache-2.0 | Log-monitoring and LLM client support |
| `ahma_harness_guard` | MIT OR Apache-2.0 | Small-model harness guards (argument healing, loop detection, skill injection) |
| `ahma_log_monitor` | MIT OR Apache-2.0 | Live log monitor: level detection, redaction, context snapshots (`monitor_level`) |
| `ahma_output_optimizer` | MIT OR Apache-2.0 | Token-economy output optimizer (dedup, truncation, pressure governor) |
| `ahma_simplify` | MIT OR Apache-2.0 | Code complexity analysis (`ahma simplify`); optional in `ahma_bin`, feature `simplify` (default on) |
| `ahma_test_support` | MIT OR Apache-2.0 | Test helpers for workspace crates |
| `ahma_update` | MIT OR Apache-2.0 | Self-update: release download, Sigstore verification (`ahma verify`), git installs |
| `ahma_vault` | MIT OR Apache-2.0 | Task vault: isolated per-task directories, two-phase trash, audit log |
| `generate_tool_schema` | MIT OR Apache-2.0 | Schema generation utility |
| `ahma_tui` | AGPL-3.0-or-later | Terminal dashboard and approval flow |
| `ahma_bin` | AGPL-3.0-or-later | Shipped `ahma` binary |

`MIT OR Apache-2.0` is used for the embeddable libraries, transports, and
tooling crates so other Rust applications can adopt Ahma's protocol and secure
sandboxing without copyleft obligations on their surrounding application code.
`ahma_mcp`, `ahma_core`, `ahma_common`, and `ahma_http_mcp_client` carry this dual
license so developers can embed Ahma's MCP server and sandbox execution primitives directly. The Apache side of the dual license adds an
explicit patent grant, and the MIT side preserves the standard Rust dual-license
option used by many libraries.

`AGPL-3.0-or-later` is used for the end-user and network-exposed product crates
that define the shipped product surface and security-relevant runtime behavior.
That includes the shipped `ahma` binary and the user-facing TUI.

### AGPL + Build Verification: Supply Chain Defense


- **AGPL requires source disclosure**: anyone distributing a modified `ahma` binary or
  running a modified version over a network must publish the corresponding source.
  Closed-source backdoored forks cannot be legally distributed as "ahma".

- **Build verification closes the gap AGPL cannot**: source transparency is only useful if
  you can verify the binary you installed actually came from that source.
  Every prebuilt release is verified using GitHub Build Provenance Attestations (SLSA Level 3)
  backed by Sigstore, ensuring the binary was built directly from the official repository
  by the CI pipeline. The installer verifies this attestation before writing anything to disk.

`ahma update` and `ahma verify --self` check this attestation automatically, but that check
runs *in-band* — inside the very binary whose integrity is in question. For the higher standard,
verify **out-of-band** instead: an independent tool queries Sigstore's public transparency log
directly, so the result doesn't depend on trusting the artifact you're trying to verify.

```bash
gh attestation verify ahma-release-linux-x86_64.tar.gz \
  --repo paulirotta/ahma
```

Together they protect against:

| Attack | AGPL | Verification |
|--------|------|--------------|
| Backdoored binary from unofficial mirror | — | ✓ Verification fails |
| Closed-source fork distributed as "ahma" | ✓ AGPL violation | ✓ Verification fails |
| DNS/CDN hijack serving a tampered binary | — | ✓ Hash mismatch |
| Compromised GitHub release assets | — | ✓ Verification fails (cannot attest outside build workflow) |
| Modified binary without modified source | ✓ AGPL violation | ✓ Verification fails |

Building from source is always an option. AGPL means the source is always
public and auditable:
```bash
cargo install --git https://github.com/paulirotta/ahma ahma_bin --bin ahma --root ~/.local --locked
```

See [docs/release-signing.md](docs/release-signing.md) for the release verification
architecture, manual verification commands, and trust model details.

### Common uses

| If you want to... | Typical answer |
|---|---|
| Embed `ahma_mcp`, `ahma_core`, or `ahma_http_mcp_client` in your own application | Allowed under **MIT OR Apache-2.0** for those crates |
| Distribute a modified `ahma` binary | Allowed under **AGPL-3.0-or-later** — source must be published |
| Offer a modified `ahma` service to remote users | Allowed under AGPL — source-availability obligations apply |
| Use Ahma internally for local or private workflows | Allowed subject to the applicable crate terms |

### Security provenance

Trust in Ahma comes from three independently verifiable facts:

1. **Published source** — GitHub, auditable by anyone, required open by AGPL for any fork
2. **Signed binaries** — RSA-2048 signature from a key held only in GitHub Actions secrets
3. **Reproducible build** — `--locked` Cargo.lock, deterministic CI pipeline in `build.yml`

The license split supports a single published origin for the security-focused
product crates. The license does not by itself make a modified fork safe — but
combined with cryptographic signing it makes an unsigned or differently-signed
impostor immediately detectable.

The repository root includes `MIT_LICENSE.txt`, `APACHE_LICENSE.txt`, and
`AGPL_LICENSE.txt` because different workspace crates use different licenses.
When in doubt, check the target crate's `Cargo.toml` first.
