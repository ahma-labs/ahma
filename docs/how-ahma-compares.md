# How ahma compares

Where ahma sits among the ways to run an AI coding agent, and where it is weaker than the
alternatives. This describes ahma v0.22 and the other tools as documented in November 2026;
the other tools change quickly, so each claim links to its source.

## The problem

An agent that holds **private data**, reads **untrusted content** (web pages, issues,
dependencies, MCP tool descriptions) and can **send data out** can be steered by that content
into leaking the data. Simon Willison calls this
[the lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/). Prompt
injection is not solved in the model, so the useful question is not whether an agent will be
misled but what it can do when it is:

- what it can **write**, and which of those writes something else later **executes**;
- what it can **read**;
- where it can **send** data;
- which **credentials** it can use.

## Ahma's answer

**The kernel enforces, you decide, a model only advises.** Every command runs under the
operating system's own sandbox (Seatbelt on macOS, Landlock on Linux), confined to the
workspace. When it needs more, the kernel's refusal becomes one question to you, answered once
and remembered at the tier you choose (this once, this session, this repo, always). A local
model can explain the question and recommend an answer; it never gives one. Nothing widens
silently, and when nobody can be asked the answer is no.

## Compared

| | Boundary | Credentials | Network | Who decides | Cost |
|---|---|---|---|---|---|
| **Nothing / "YOLO"** | Your user account | Everything you can read | Open | The model | None, until it goes wrong. "Ask every time" decays into clicking yes |
| **Probabilistic guardrails** (e.g. [Claude Code auto mode](https://www.anthropic.com/engineering/claude-code-auto-mode)) | None: a classifier judges each action | Not separated | Not separated | A model. Anthropic measures 17 % false negatives on real overeager actions | Little friction; a useful layer, not a boundary |
| **Docker / Podman container** | Namespaces and cgroups on the shared host kernel | Whatever you mount or pass in (env tokens, `~/.ssh`, often the Docker socket) | Open by default | You, at `run` time | An image and a second toolchain; rebuilds |
| **[Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)** | A microVM per agent with its own kernel ([why](https://www.docker.com/blog/why-microvms-the-architecture-behind-docker-sandboxes/)); workspace mounted read-write | A host proxy injects API credentials; raw values never enter the VM | Deny by default, host allowlist | Docker defaults; org-level Cedar policy for MCP on the governance tier | A VM per agent, a hypervisor, a toolchain inside the VM |
| **Other kernel sandboxes** (below) | Seatbelt, Landlock, bubblewrap or seccomp on the host kernel | Varies | Proxy allowlists, varies | Config files, per harness | Little; each covers one harness |
| **ahma** | Seatbelt or Landlock per command; writes confined to the workspace plus your grants; reads too on Linux, plus a seccomp baseline | SSH keys never enter: a broker signs per host after consent. Secret-looking environment variables are removed | Opt-in deny-by-default proxy (`--restrict-network`), kernel-pinned on macOS and Linux 6.7+ | You, once per question; a local model may recommend | None per command: your own tools and caches at native speed |

Docker says plainly that containers remain the right tool for applications and CI, and
argues that an *agent* needs a stronger boundary than a container
([Docker Sandboxes vs. containers](https://www.docker.com/blog/docker-sandboxes-vs-containers/)).

### Other kernel sandboxes

The agent vendors themselves have settled on in-place kernel sandboxing for their own
agents. Anthropic uses it for Claude Code, OpenAI for Codex, and Cursor for its agent.
Cursor reports that sandboxed agents stop 40% less often
([Cursor](https://cursor.com/blog/agent-sandboxing)). Ahma uses the same mechanisms; what
differs is the scope: one boundary and one permission ledger across all of them.

- **[Anthropic sandbox-runtime](https://github.com/anthropics/sandbox-runtime) and [Claude Code's sandboxed Bash](https://code.claude.com/docs/en/sandboxing)**: Seatbelt on macOS, bubblewrap on Linux, a proxy with domain allowlists; reads allowed everywhere by default. It covers Claude Code's Bash tool, not MCP servers, and is off unless you turn it on.
- **[OpenAI Codex CLI](https://developers.openai.com/codex/agent-approvals-security)**: Seatbelt on macOS, bubblewrap on Linux, a native Windows sandbox; network off in `workspace-write` by default.
- **[Cursor](https://cursor.com/blog/agent-sandboxing)**: Seatbelt on macOS; Landlock and seccomp on Linux; WSL2 on Windows.
- **[Gemini CLI](https://geminicli.com/docs/cli/sandbox/)**: Seatbelt profiles, or Docker, Podman or gVisor containers.
- **[nono](https://nono.sh/os-sandbox)**: a Landlock and Seatbelt sandbox for any agent, with domain filtering and a keychain-backed credential proxy. Closest to ahma in scope.
- **[landrun](https://github.com/zouuup/landrun)**: a minimal Landlock wrapper for any command.
- **[NVIDIA OpenShell](https://docs.nvidia.com/openshell/latest/about/overview.html)**: Landlock, seccomp and an egress policy, with sandbox policy expressible in Cedar.

What ahma adds to the same kernel mechanisms:
- **One sandbox and one permission ledger for every agent.** It works under any MCP client, and through hooks it covers the native terminals of Claude Code, Codex, Copilot CLI, Cursor and Antigravity.
- **Everything in one place.** A per-user hub shows every session in one [TUI](tui.md) or [browser view](web.md).
- **Operations.** Commands are tracked and cancellable operations, behind a [write queue](workspace-queue.md) that keeps two writers off one tree.
- **Writes something else later runs.** [Trust-handoff](security-sandbox.md) denials cover writes that the host executes afterwards.
- **Credentials and links.** [SSH](ssh-agent-broker.md) and [link](open-links.md) brokers keep keys and the browser outside the sandbox.
- **A record.** An [audit log](execution-audit-log.md) is written before each command starts.

## Where a microVM is stronger

1. **The kernel is shared.** A kernel privilege-escalation bug escapes any Landlock or Seatbelt sandbox; a VM also needs a hypervisor escape. Such bugs are not rare: in 2026 alone, [CVE-2026-31431 "Copy Fail"](https://www.microsoft.com/en-us/security/blog/2026/05/01/cve-2026-31431-copy-fail-vulnerability-enables-linux-root-privilege-escalation/) and [CVE-2026-46300 "Fragnesia"](https://access.redhat.com/security/cve/cve-2026-46300). On Linux ahma shrinks that surface with a seccomp filter: `AF_ALG` sockets (Copy Fail's entry point), `io_uring`, `bpf` and keyrings are refused in every command ([security-sandbox.md](security-sandbox.md#linux-landlock), SPEC R-HARDEN.3).
2. **Commands run as you.** They run next to your other processes. Ahma confines signals on macOS. On Linux, Landlock's ptrace scoping keeps other processes' memory and environment out of reach. On macOS a sandboxed command can read other same-user processes' arguments and environment, and no sandbox rule can stop it; ahma keeps secrets out of its own processes, but not out of your editor's (R-HARDEN.2). Host IPC is only partly mediated: D-Bus, Mach services, and on Linux a container daemon socket ([known gaps](../SPEC.md#11-known-gaps)).
3. **macOS reads are open.** Seatbelt confines writes. Reads are open apart from a credential denylist, and a denylist only stops what someone named. A VM sees only what is mounted.
4. **Windows has no path boundary yet.** AppContainer isolation is written but switched off. On Windows, run ahma inside WSL2 for a Landlock boundary.
5. **Linux kernel versions matter.** Landlock filters TCP only from Linux 6.7, and on Linux ahma filters no UDP (macOS refuses all direct egress).
6. **Seatbelt is undocumented.** Apple does not support it for third parties. Chrome and every macOS agent sandbox above depend on it, which lowers the risk but does not remove it.

## Where in-place kernel sandboxing is stronger

- **No VM, no image, no second toolchain.** Commands run with your installed tools and caches at native speed, each with its own scope and egress.
- **MCP tools and native-terminal commands are both confined.** Docker Sandboxes runs local stdio MCP servers on the host, with host permissions ([docs](https://docs.docker.com/ai/sandboxes/mcp-gateway/)).
- **Writes that something else runs later.** A microVM mounts the workspace read-write, so a planted git hook or editor task runs on the host the next time you commit or open the folder. Docker's own [security notes](https://docs.docker.com/ai/sandboxes/security) warn about this. Ahma denies those paths on macOS, and on Linux detects them, or with an opt-in prevents them ([R-HANDOFF](../SPEC.md#r-handoff-trust-handoff--legitimate-writes-that-something-trusted-later-executes)). This route is the most common real-world escape from agent sandboxes ([Pillar Security, 2026](https://www.bleepingcomputer.com/news/security/cursor-codex-gemini-cli-antigravity-hit-by-sandbox-escapes/)).
- **Your endpoint security still sees what runs.**
- **It works where there is no hypervisor:** inside a VM, on CI, on a Raspberry Pi.

## They compose

For hard isolation, run ahma *inside* a VM or container: the VM bounds the kernel, and ahma
bounds what the agent does inside it.

## Further reading

- Simon Willison, [The lethal trifecta for AI agents](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/): the clearest statement of the risk.
- Anthropic, [How we contain Claude across products](https://www.anthropic.com/engineering/how-we-contain-claude): process sandbox versus VM trade-offs, and real failures.
- [OWASP Top 10 for Agentic Applications 2026](https://genai.owasp.org/resource/owasp-top-10-for-agentic-applications-for-2026/).

## See also

- [security-sandbox.md](security-sandbox.md): what each platform enforces.
- [permissions.md](permissions.md): how a question is asked and remembered.
- [network-egress.md](network-egress.md): `--restrict-network`.
- SPEC [§4 Security](../SPEC.md#4-security---kernel-enforced-sandboxing) and [§11 Known gaps](../SPEC.md#11-known-gaps).
