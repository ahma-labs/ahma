# Ahma

<img src="./assets/ahma.png" alt="Ahma logo" align="right" width="120">

[![CI](https://github.com/ahma-labs/ahma/actions/workflows/build.yml/badge.svg)](https://github.com/ahma-labs/ahma/actions/workflows/build.yml) [![Coverage Report](https://img.shields.io/badge/Coverage-Report-blue)](https://ahma-labs.github.io/ahma/html/) [![Rust Docs](https://img.shields.io/badge/Rust-Docs-blue)](https://ahma-labs.github.io/ahma/doc/) [![Code Simplicity](https://img.shields.io/badge/Code-Simplicity-green)](https://ahma-labs.github.io/ahma/CODE_SIMPLICITY.html) [![License: Per Crate](https://img.shields.io/badge/License-Per--Crate-6f42c1)](docs/licensing.md) [![Rust](https://img.shields.io/badge/Rust-1.95%2B-B7410E.svg)](https://www.rust-lang.org/)

Ahma runs an AI agent's shell commands inside your operating system's own kernel sandbox,
confined to the project. Builds, tests, git, formatters and log tails run as tracked,
cancellable operations. It works through MCP in any client, and through hooks in the native
terminals of Claude Code, Codex, Copilot CLI, Cursor and Antigravity.

## Stance

**The kernel enforces, you decide, a model only advises.**

- **Defaults.** Ahma ships default policies: the workspace, temp and your toolchain's caches.
- **Questions.** When a command needs more, the kernel's refusal becomes one question to you, answered once and remembered at the tier you pick: once, this session, this repo, or always.
- **The model.** A local model you trust can explain the question and recommend an answer. It never answers.
- **Nothing silent.** Nothing widens silently, every gap is disclosed, and when nobody can be asked the answer is no.

The aim is to make the safe path the easy one, so nobody is pushed into "allow everything".

## Quickstart

```bash
# macOS / Linux
curl -sSf https://raw.githubusercontent.com/ahma-labs/ahma/main/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell 5.1+)
irm https://raw.githubusercontent.com/ahma-labs/ahma/main/scripts/install.ps1 | iex
```

The installer verifies the release's build provenance, installs `ahma` in `~/.local/bin`
(put it on your `PATH`), and runs `ahma setup`. That adds ahma to the MCP config of every
editor it finds (Cursor, VS Code, Claude Code, …). For Cursor and Claude Code it also
installs terminal hooks, so the agent's own terminal and file edits are sandboxed too. Other
clients get hooks only with `--hooks`, and setup says why. Restart the editor afterwards.

```bash
ahma hooks status    # what is actually sandboxed, and why
ahma setup           # see what is allowed, where it comes from, and change it
ahma doctor          # check ahma's health; fixes only with your OK
ahma update          # update to the latest release
```

Other install routes (`cargo binstall`, source builds, Intel Macs, branches):
[docs/installation.md](docs/installation.md). Writing `mcp.json` by hand:
[docs/connection-modes.md](docs/connection-modes.md).

## What is enforced, per platform

| | Writes | Reads | Network, with `--restrict-network` |
|---|---|---|---|
| **macOS** (Seatbelt; Apple Silicon binaries, Intel from source) | Workspace and your grants | Open, apart from a credential denylist | Kernel: only ahma's proxy is reachable |
| **Linux** (Landlock, kernel 5.13+; x86_64 and arm64, incl. Raspberry Pi) | Workspace and your grants | Workspace, system and toolchain directories, and your grants | Kernel pins TCP to the proxy on 6.7+; advisory before; UDP and raw sockets refused by seccomp |
| **Windows** (Job Objects) | **Not confined** | **Not confined** | Advisory proxy only |

Windows has no OS path boundary yet: AppContainer isolation is written but switched off.
Use ahma inside WSL2 for a Landlock boundary. Details:
[docs/security-sandbox.md](docs/security-sandbox.md).

**Not covered** ([details](docs/security-sandbox.md), [all known gaps](SPEC.md#11-known-gaps)):

- **Kernel exploits.** The kernel is shared. A kernel bug escapes, and ahma does not filter system calls yet.
- **Network is open unless you restrict it.** Even with `--restrict-network`, an allowed host such as `github.com` can carry data out.
- **Writes that something trusted runs later** (git hooks, editor tasks, harness settings).
  - macOS denies them.
  - Linux detects them after the fact (`TRUST-HANDOFF WRITE`), unless you set `linux_deny_tier = "namespace"`.
  - The shared package cache is a cross-project channel (`--no-package-cache-write`).
- **On macOS, other processes' environments are readable.** A sandboxed command can read the arguments and environment of your other processes, and no macOS sandbox rule can stop it. ahma keeps secrets out of its own long-lived processes, but an API key in your editor's or agent's environment is exposed. Keep keys in the keychain, and use `--restrict-network` (SPEC R-HARDEN.2).
- **No CPU, memory or process limits.**
- **Commands that bypass ahma.** A native terminal without hooks is not sandboxed. Check with `ahma hooks status`.

## How it compares

Ahma isn't alone in this approach. Anthropic sandboxes Claude Code's own shell with the same
operating-system mechanisms (Seatbelt on macOS, bubblewrap on Linux); in its auto-allow mode a
command that runs inside the sandbox needs no prompt at all
([Claude Code sandboxing](https://code.claude.com/docs/en/sandboxing),
[how Anthropic contains Claude](https://www.anthropic.com/engineering/how-we-contain-claude)).
OpenAI Codex and Cursor do the same. Ahma brings that boundary to every agent you use, under
one set of permissions.

| Approach | What bounds the agent |
|---|---|
| Nothing / "YOLO" | Your whole user account; the model decides |
| Probabilistic guardrails (e.g. Claude Code auto mode) | A classifier's judgement: a useful layer, not a boundary |
| Docker / Podman containers | The shared kernel plus whatever you mounted (often tokens, `~/.ssh`, the Docker socket) |
| Docker Sandboxes | A microVM with its own kernel; stronger against kernel bugs, but a VM per agent, and the workspace is still mounted read-write |
| Other kernel sandboxes (Claude Code sandbox, Codex, Cursor, nono, …) | The same kernel mechanisms, each covering one harness |
| **ahma** | The kernel, per command, across every agent, with one permission ledger and one view of all sessions |

Sources, details and an honest list of where a microVM is stronger:
[docs/how-ahma-compares.md](docs/how-ahma-compares.md). The two compose: run ahma inside a VM
for hard isolation.

## Features

| | |
|---|---|
| [Kernel sandbox](docs/security-sandbox.md) | Per-command Seatbelt or Landlock scope; nested-sandbox detection; temp access; trust-handoff denials |
| [Permissions](docs/permissions.md) | One ledger in `~/.ahma`; grants for one command, a session, a lease, a repo or always; asked in your client, the TUI or the harness's own dialog |
| [`ahma setup`](docs/setup.md) | What is allowed and why, and a plan of proposed changes applied one `y` at a time (`--plan` to print it) |
| [`ahma doctor`](docs/doctor.md) | Health checks and fixes, each with your OK (`/doctor` in the TUI) |
| [`ahma_help`](docs/ahma-help.md) | Guidance for an agent that hits `Operation not permitted`, so it stops retrying and asks well |
| [Network egress](docs/network-egress.md) | `--restrict-network`: a deny-by-default proxy, toolchain hosts from profiles, private and metadata addresses blocked |
| [Audit log](docs/execution-audit-log.md) | `audit.jsonl`: every execution and every denial, written before the process starts |
| [Operations and write queue](docs/workspace-queue.md) | Long commands return an operation id. Writers to one workspace run one at a time, in order, across every session; reads skip the queue |
| [Settings](docs/settings.md) | `~/.ahma/settings.toml`, sync or async mode, provenance of every value; `AHMA_*` variables are retired ([env vars](docs/environment-variables.md)) |
| [Terminal hooks](docs/installation.md#terminal-hooks) | The same sandbox for the agent's native terminal, and an edit guard for its file edits |
| [Connection modes](docs/connection-modes.md) | stdio, Unix socket and HTTP; token auth by default; MCP 2025-11-25 and 2026-07-28 |
| [Per-user hub](docs/hub.md) | One hub per user; every session runs in its own sandboxed worker; updates without dropping sessions |
| [Session isolation](docs/session-isolation.md) | Each HTTP client session gets its own scope and worker |
| [Session-health notifications](docs/session-health-notifications.md) | The client is told when its session was rebuilt, or a grant is waiting on a human |
| [TUI](docs/tui.md) | `ahma tui`: every session's work live, approvals, chat with a connected model |
| [LLM providers](docs/llm-providers.md) | `ahma llm add`: local or cloud models for chat, the advisor and log monitoring |
| [Live log monitoring](docs/live-log-monitoring.md) | `livelog` and `monitor_level`: a model watches a stream and pushes alerts |
| [Custom tools](docs/custom-tools.md) | MTDF JSON definitions: typed arguments for any CLI |
| [File tools](docs/file-tools.md) | Read, edit, `multi_edit`, `apply_patch`, search, for clients without their own |
| [`ahma simplify`](docs/simplify.md) | Code complexity analysis with fix instructions for an agent |
| [Agent skills](docs/agent-skills.md) | The `ahma` skill: teaches an agent to use ahma well |
| [`ahma_core`](docs/ahma-core-library.md) | Embed the sandbox and MCP service in a Rust application |
| [Release signing](docs/release-signing.md) | Sigstore build-provenance on every release; `ahma verify` |

## v0.22 Experimental Features

| | |
|---|---|
| [`ahma web`](docs/web.md) | The TUI's view in a browser tab, loopback only, paired with a one-time code |
| [`ahma desktop`](docs/desktop.md) | The same view in a native window (macOS, Windows) |
| [SSH key broker](docs/ssh-agent-broker.md) | `git push` over SSH with no key in the sandbox: signs per host after consent (macOS, Linux; in progress) |
| [Opening web links](docs/open-links.md) | `gh … --web` from the sandbox opens your browser, for sites you allowed (macOS, Linux; in progress) |
| [Task vaults](docs/task-vault.md) | One directory per task as the whole scope; `rm` moves to the vault's trash |
| [Team policy in Cedar](docs/cedar-policy.md) | Forbid-only Cedar rules that narrow what ahma allows on every surface; `ahma policy validate`, `list`, `explain`, `export`, `verify` |
| [Bundle audit](docs/bundle-audit.md) | Checks MTDF bundles for secrets, missing path validation and injection payloads |

## Getting an agent to use ahma

Strongly imperative tool descriptions read to Claude as prompt injection, so put a decision
rule in your project's `AGENTS.md` or `CLAUDE.md` instead:

```markdown
## When to use ahma vs the native terminal

Prefer ahma's `run_terminal_command` when the command writes to disk (the sandbox keeps
writes in the workspace), runs more than a few seconds (it returns an operation_id; `await`
it later), should be watched for errors (`monitor_level`), or shares the repo with other
commands (writers run in order; a queued one says `NOT started`, don't resend it).
Keep reading, searching and editing files on the native tools.
```

## Further reading

- Simon Willison, [The lethal trifecta for AI agents](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/)
- Anthropic, [How we contain Claude across products](https://www.anthropic.com/engineering/how-we-contain-claude)

## Contributing

Issues and pull requests are welcome.
- [AGENTS.md](AGENTS.md) is how to work in this repo, for people and agents alike.
- [SPEC.md](SPEC.md) and each crate's `SPEC.md` say what ahma must do.
- [docs/build-and-test-performance.md](docs/build-and-test-performance.md) explains the build layout.

## License

Per crate: the embeddable engine is MIT OR Apache-2.0, and the shipped `ahma` binary and its
interfaces are AGPL-3.0. See [docs/licensing.md](docs/licensing.md).
