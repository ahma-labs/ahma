# Security Sandbox

Ahma enforces **kernel-level filesystem sandboxing** by default. The sandbox scope is set once at server startup and cannot be changed. What that buys you is **not** one guarantee across all three platforms, and it must not be stated as one (SPEC R6.1.6, R6.2.2, R6.3.9):

| | Writes outside the scope | Reads outside the scope |
|---|---|---|
| **Linux** (Landlock) | denied by the kernel | denied by the kernel |
| **macOS** (Seatbelt) | denied by the kernel | **not confined** — the profile grants blanket read; a credential denylist compensates ([below](#macos-seatbelt)) |
| **Windows** (Job Objects) | **not confined by path** — process-lifetime containment only; AppContainer is pending | **not confined** |

Within the scope the agent has full access, with deliberate exceptions — see [Writable, but not everything](#writable-but-not-everything-trust-handoff).

## Why Kernel-Level Sandboxing?

Trust-based security ("do you trust this tool?") doesn't protect against mistakes or manipulation at speed. Kernel-enforced boundaries do:

- **String filters can be bypassed** via path traversal, symlinks, or creative shell expansion. A kernel policy is applied to the syscall, not to the string.
- **Blast radius is bounded** — on Linux and macOS, an AI agent that tries `rm -rf ~` is stopped by the kernel, not by a pattern match. (On Windows there is no path boundary yet; see [below](#windows-job-objects-appcontainer-pending).)
- **No runtime overhead** — the policy is applied once at sandbox lock and enforced by the OS.

## Sandbox Scope

The sandbox scope is the root directory boundary — for writes everywhere it is enforced, and for reads on Linux (see the table above):

- **STDIO mode**: the IDE reports the workspace through `roots/list`, so setting `"cwd": "${workspaceFolder}"` in `mcp.json` makes the sandbox "just work". The launch directory is not itself trusted as a scope: it becomes the scope only by being *reported* through `roots/list` or *named* explicitly, never by being inferred from where the process started or from project-marker files (SPEC R5.2.1).
- **HTTP mode**: Set once when the server starts. Configure via:
  1. `--sandbox-scope <path>` CLI flag (highest priority)
  2. `scopes = [...]` in `~/.ahma/settings.toml`
  3. MCP client `roots/list` (an **empty** answer is not a workspace and falls through)
  4. A user elicitation answer
  5. `container_root` from settings, narrowed to the project in use

There is **no** sixth source and no invented directory. With none of the five available, ahma refuses tool calls and says how to fix it — it does not pick somewhere to run. (It used to: `sandbox_directory` defaulted to an auto-created `~/sandbox`, so a client reporting no roots silently locked there and every command failed with an ordinary-looking shell error such as `fatal: not a git repository`.)

### Container root and auto-narrowing

The container root is the directory that holds the projects you work on. It is deliberately settable **only** in your own `~/.ahma/settings.toml` — never in a client-owned `mcp.json`, which is precisely where an over-broad path would be planted.

```toml
[sandbox]
container_root = "~/github"     # no default; unset means "refuse rather than guess"
```

ahma never locks the container whole. The first tool call that names a path — a command's `working_directory`, or the target of `write_file`/`replace_in_file` — selects the project, and the writable scope narrows to that one immediate child for the rest of the session. The rest of the container stays **readable but not writable**, so cross-project lookups keep working while an injected prompt cannot drop a `.git/hooks/post-checkout` into an unrelated repository. That is persistence, not merely data loss, which is why the container is not left whole.

Consequences worth knowing:

- **A command with no `working_directory` is refused, not guessed.** The container spans every project, so there is nothing safe to substitute — and the working directory is also the signal that selects what to narrow to.
- **Narrowing happens once.** A later call naming a sibling project is denied rather than re-scoped. To reach a second project, restart, or grant it explicitly with `ahma sandbox grant <path>`.
- **Reads never narrow.** Only write-capable surfaces select the project, so an incidental lookup cannot spend the session's one narrowing.

**Security invariant**: Once the sandbox scope is set, it cannot be *widened* for the lifetime of the server process. Any attempt to change it after lock is rejected at the single commit point. Auto-narrowing is the one sanctioned exception in the other direction, and only ever within a container the user already authorized.

## Platform-Specific Enforcement

### Linux (Landlock)

On Linux, Ahma uses [Landlock](https://docs.kernel.org/userspace-api/landlock.html) — a kernel LSM that applies fine-grained filesystem access rules in-process with no daemon or capability escalation required.

**Requirements**: Linux kernel 5.13 or newer (released June 2021). The server refuses to start on older kernels unless sandbox is explicitly disabled.

```bash
uname -r                            # check kernel version
cat /sys/kernel/security/lsm        # verify landlock is active
```

A Landlock rule is an allow-list of file descriptors, so the read-only set is expressed as explicitly as the writable one and anything unnamed is unreadable. This is the position the phrase "outside the scope is denied" actually describes, and it holds **only** here (SPEC R6.1.6).

**Older kernels / Raspberry Pi**: Landlock requires kernel ≥ 5.13. On older Pi OS kernels, run with:

```bash
ahma serve stdio --no-sandbox
```

or add `"--no-sandbox"` to `mcp.json` args. Disabling enforcement is deliberately CLI-only — `AHMA_DISABLE_SANDBOX` is retired and ignored, because a client-owned config file can carry an environment variable (SPEC R-CFG2.3).

### macOS (Seatbelt)

On macOS, Ahma uses Apple's built-in `sandbox-exec` with a generated Seatbelt profile (SBPL) that restricts **write** access to the sandbox scope. No additional installation required.

**Requirements**: Any modern macOS version. `sandbox-exec` is built into macOS.

**Writes are kernel-scoped; reads are not** (SPEC R6.2.2). On Apple Silicon and macOS 26+, the APFS firmlink / cryptex volume layout means `bash` and `dyld` resolve paths to vnodes that match no traditional `/usr`, `/System`, … subpath prefix — read rules written as subpaths simply never fire, so a profile that tried to scope reads would deny the very commands it exists to protect. The profile therefore emits a bare, unqualified file-*read* allow. This is a platform limitation rather than a grant, which is why it cannot be expressed as a profile and is instead disclosed on every scope surface: the startup banner, `ahma status`, and the TUI scope panel (SPEC R-PERM.5.1).

What keeps secrets unreadable on macOS is therefore an explicit **denylist**, not the scope (SPEC R6.2.3). Each entry is emitted as a `(deny file-read* …)` placed *after* the blanket allow and *before* the workspace-scope allows, so SBPL's last-match-wins ordering keeps them denied by default while an explicit scope grant still wins. The set covers plaintext credential directories, ahma's own control plane, private key material, and container daemon sockets; it is tuned so no common build/test/VCS tool breaks — notably `~/.ssh` as a whole stays readable (git-over-ssh needs `config` and `known_hosts`) while the `~/.ssh/id_*` key files themselves are denied. The effective set is owned by `sandbox/credential_reads.rs`, not by this page: inspect it with `ahma permissions list`, extend it via `[sandbox] deny_credential_reads`, or re-allow a default via `[sandbox] allow_credential_reads`.

> **A denylist is a weaker guarantee than a scope, and is worth reading as one.** A scope denies everything it does not name; a denylist denies only what it *does* name, so any secret nobody thought to enumerate is readable. It is the best available answer on this platform — not an equivalent of the Linux position above.

#### Keychain access (`gh auth` / `git-credential-osxkeychain`)

The login **keychain** (`~/Library/Keychains`) is **allowed by default** (read + write, plus the `com.apple.security*` preference plists). This is what lets `gh`, `git-credential-osxkeychain`, and other Keychain-backed credential helpers work under the sandbox.

Why it's on by default (unlike the plaintext credential dirs above): the keychain is **encrypted at rest**, so blocking file access to it only guards against offline theft of the encrypted database — not against secret extraction, which goes through the `securityd` daemon and is gated by each item's ACL (and a GUI prompt) regardless of the sandbox. Blocking it mostly just breaks tools: `gh auth login` appears to succeed but writes the OAuth token where `gh` can't read it back, so every later `gh` call falls back to unauthenticated (HTTP 401 / the anonymous IP rate limit).

For maximum defense-in-depth on high-security machines, turn it off:

```toml
[sandbox]
allow_keychain = false   # blocks keychain read+write; breaks gh and similar tools
```

or per-invocation with `--no-allow-keychain` (and `--allow-keychain` to force it on when settings disable it). When off, `~/Library/Keychains` is added to the credential-read deny set and keychain writes are blocked, so `security add-generic-password` fails with *"The authorization was denied"*.

> **Note on nesting:** if you launch ahma from *inside* another sandbox (e.g. an editor's Bash sandbox), tool subprocesses run under the **intersection** of both profiles — so keychain access ahma grants can still be blocked by the outer sandbox. Start ahma outside that shell, or see [Nested Sandbox Environments](#nested-sandbox-environments-cursor-vs-code-docker).

### Windows (Job Objects; AppContainer pending)

On Windows, Ahma applies a Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) at startup, which guarantees child processes are killed when the server exits. A Job Object does **not** restrict filesystem access by path in either direction — not writes, not reads. Path confinement would arrive with per-command AppContainer isolation. That is written — per-session container SID, scoped ACL grants, launcher re-entry — and a `windows-latest` CI run has **executed and disproved** it: the scoped grant does not take effect, so a write *inside* the locked scope is denied along with one outside it. A boundary that denies everything proves nothing, so it is switched off rather than shipped broken, and the honest position today is **process-lifetime containment and no kernel filesystem boundary** (SPEC R6.3.9). The `appcontainer_dacl_diagnostics` test dumps the ACL and token evidence needed to find out why; it runs on every Windows CI leg. ahma's own path validation still binds the paths ahma resolves, but it cannot bind what a spawned command does with its own syscalls — so the scope shown on Windows is a scope ahma honours, not one the OS enforces.

PowerShell (5.1+) is the shell. See [SPEC.md R6.3](../SPEC.md) for the GA gate this section has to clear.

## Writable, but not everything (trust handoff)

Confining writes is necessary but not sufficient. A file the agent is fully entitled to write — inside the workspace, through an ordinary tool call — can be executed later by something that was never sandboxed at all: your `git` on the next `commit` or `checkout`, your editor's extension host the next time the folder opens, your agent harness's hook engine at the end of a turn. Nothing breaks the sandbox in this class of attack; the boundary is crossed by a component that was never inside it. SPEC **R-HANDOFF** owns the response. The short version is two tiers, deliberately not collapsed into one.

**Deny-write — no question asked.** Some paths inside your own workspace are not writable, because no legitimate agent task writes them:

- **hook directories under the resolved git directory.** Resolved, not spelled: `git worktree add` and `git init --separate-git-dir` move the real git directory somewhere a `.git/hooks` pattern would never match, and a rule that describes a *spelling* rather than the boundary is no rule at all (SPEC R-HANDOFF.2).
- **the workspace's own `.ahma/` directory** — ahma's MTDF tool definitions. An agent that can author a tool definition can define the command it is then allowed to run. This is also why ahma no longer watches that directory: a watcher would turn writing the file into *running* it with no user action in between, so **tool configuration reloads only through the explicit `restart` tool** (SPEC R-HANDOFF.7).
- **container daemon sockets**, denied for read and write alike — a `--privileged` container with a host bind mount converts socket access into unrestricted host write access, performed by a daemon entirely outside the sandbox (SPEC R-HANDOFF.6).
- **the virtualenv-shaped fake-interpreter vector** — a `pyvenv.cfg`, or an executable named `python*` directly under `bin/`/`Scripts/`, which an editor's interpreter discovery runs from an unsandboxed extension host.

A denied write fails with the structured `sandbox_denial` payload naming the path and the reason; nothing is prompted, because there is no judgement call to delegate.

**Two of those denies have an escape hatch, and the error names it.** "No legitimate agent task writes them" is true of the general case and false of two specific, common ones: installing a repository's own pre-push guard (`cp scripts/check-guardrails.sh .git/hooks/pre-push`), and editing the `.ahma/` tool definitions a project ships. A default with no documented way out does not make anyone safer — it makes them disable the sandbox wholesale, which is far worse than a narrow opt-in. So `--allow-git-hooks` / `[sandbox] allow_git_hooks` and `--allow-project-tool-config` / `[sandbox] allow_project_tool_config` exist, both **off by default**, each removing exactly its own path from the deny set *and* from the macOS kernel rules. Enabling either is disclosed with a startup warning naming what became writable and what will execute it (SPEC R7), and the write-denial message names the flag and the settings key so you never have to go looking. See [settings.md](settings.md#trust-handoff-escape-hatches).

**Allow, but say so loudly.** Editor and harness configuration is genuinely something you ask an agent to edit — `.vscode/tasks.json`, `.vscode/launch.json`, `.vscode/settings.json`, `.vscode/mcp.json` and `.cursor/mcp.json`, `.cursor/hooks.json` and the other harness `hooks.json` files, `.cursor/rules/**`, `.claude/settings.json` / `.claude/settings.local.json`, and a repository's own `.git/config`. Blocking these would break "set up my editor for this project"; prompting on each would be exactly the permission fatigue ahma exists to prevent. So the write **succeeds**, and is surfaced as a first-class warning that names the file *and the trigger that will execute it* — "this runs the next time you open this folder" is the load-bearing half, because a filename alone does not tell you a write became a future execution. Membership of either tier lives in `sandbox/exec_config.rs`, not on this page.

**Enforcement is uneven, and you should know where** (SPEC R-HANDOFF.4). The deny-write tier is a hole *inside* an allowed subtree, and platforms differ on whether that is expressible to the kernel at all:

| Platform | Deny-write tier |
|---|---|
| **macOS** | kernel-enforced for the fixed-subpath rules — SBPL is last-match-wins, so a `(deny file-write* …)` emitted after the workspace allow genuinely subtracts |
| **Linux** | **application-layer only.** Landlock's ABI is additive-allow with no deny rule and no ordering, so the hole cannot be expressed to the kernel (SPEC R6.1.7). ahma enforces it in its own file tools, which means it is **bypassable from `run_terminal_command`**: a shell child inherits the workspace-wide write right and can create a hook script directly |
| **Windows** | no filesystem enforcement yet (SPEC R6.3.9); the application-layer check is the only control |

The shape-matched rules are application-layer on *every* platform by construction: a kernel deny on every `bin/python*` would break a legitimate `python -m venv`.

**The child's environment is part of the same surface.** Variables that cause an unrelated process to load code of the agent's choosing (`BASH_ENV`, `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES` and family) or that re-point a trusted client at an attacker-chosen endpoint (`DOCKER_HOST`) are stripped from every sandboxed child. `SSH_AUTH_SOCK` is deliberately **kept**: it is a capability to *use* keys, not to read them, and it is what lets git-over-ssh keep working while the key files stay denied (SPEC R-HANDOFF.5).

## Network egress

The filesystem sandbox says nothing about the network, and **egress is unrestricted by default**. Pass `--restrict-network` (or set `[network] restrict = true`) to route sandboxed subprocesses through a guarded local proxy that forwards only the domains in `[network] allow` — deny-all when that list is empty — and refuses private, loopback and cloud-metadata addresses. The README's *What the sandbox does not cover* section states what that restriction is and is not on each platform. The per-vault `egress.allowlist` described under [Task Vaults](#task-vaults) is a separate, vault-only mechanism and does not apply to an ordinary workspace session.

## Nested Sandbox Environments (Cursor, VS Code, Docker)

When ahma runs inside a host that already provides its own kernel sandbox (Cursor's agent sandbox, VS Code, Docker), ahma does **not** try to mimic or coexist with the host's internals (such as the build-cache environment variables Cursor injects). Instead it picks exactly **one authoritative sandbox per execution path** and **always tells you which one is active**.

ahma detects a host from environment markers (`CURSOR_SANDBOX`/`CURSOR_AGENT`, `CLAUDECODE`/`CLAUDE_CODE_ENTRYPOINT`, `VSCODE_*`, `/.dockerenv`/`container`).

**Terminal hooks → defer to the host.** When an ahma terminal hook fires inside a detected host sandbox, the command already runs under the host's kernel sandbox, so ahma defers: it lets the command run unchanged in the host sandbox and does **not** re-wrap it in a second sandbox. This removes the "double sandbox" friction (e.g. builds failing because the host redirected `CARGO_TARGET_DIR` outside the workspace) without ahma chasing each host's private cache variables. The hook discloses this loudly:

> Sandbox: ahma is DEFERRING to Cursor's sandbox and is NOT applying its own. Protection now depends on Cursor. If you have disabled Cursor's sandbox, this command runs UNSANDBOXED.

To force ahma's own (tighter) sandbox instead — accepting the redundant double-sandbox and the host's build-cache friction — set `AHMA_PREFER_OWN_SANDBOX=1`.

**MCP server (`run_terminal_command`) → ahma stays authoritative.** Commands the agent runs through ahma's MCP tools execute in ahma's own process, which the host's terminal sandbox does **not** wrap, so ahma applies its own sandbox and remains the authority.

Two nested cases are handled loudly at server startup (SPEC R5.4 "nothing silent"):

- **ahma cannot nest its own sandbox** (macOS `sandbox-exec` is *denied* — positive proof ahma is inside a restrictive outer sandbox): instead of hard-failing, ahma **defers to that host** and discloses it loudly, with host-specific remediation. This is fail-closed — a blocked nesting attempt proves an outer sandbox is enforcing.

  This is a platform rule, not a configuration: macOS refuses to apply a Seatbelt profile inside a process that is already confined by one whenever the outer profile denies *anything* (measured on macOS 26 — `(allow default)` plus a single `deny` of a nonexistent path is enough). Every real sandbox forbids nesting, ahma's own included, and no profile ahma could generate changes that; `AHMA_PREFER_OWN_SANDBOX` has no effect here because the kernel, not ahma, refuses. Every child of a confined process inherits the confinement, so a command ahma runs *without* its own wrapper is still kernel-sandboxed — by the outer boundary. That is why the deferral is decided when a `Sandbox` is **constructed**, on every execution path — `ahma serve` startup *and* the in-process library used by ahma's own tests and by embedders — from the kernel's own answer (`sandbox_check` on ahma's pid) confirmed by a refused nesting probe, never from environment markers alone. Before this, an ahma built in-process inside an outer sandbox found out at its first spawn, as the child's opaque `sandbox-exec: sandbox_apply: Operation not permitted`.

  The common way to hit it is ahma running ahma: `cargo nextest run` executed through `run_terminal_command`, or a nested `ahma serve`. Every command ahma sandboxes carries the marker `AHMA_OUTER_SANDBOX_PID=<pid>` (set by ahma, never read as a setting), so the nested ahma names the outer one:

  > Sandbox: ahma is DEFERRING to an outer ahma's sandbox and is NOT applying its own. Protection now depends on an outer ahma. … This process was started by an outer ahma `run_terminal_command`, whose kernel sandbox already confines every write it makes …

  The nested ahma's own scope is still validated in-process (path checks on file tools) but is not kernel-enforced — the outer sandbox's boundary is. For ahma's own enforcement, start the process from a plain terminal instead (SPEC R7.6).
- **ahma is enforcing *on top of* a host sandbox** (the report's classic case: ahma launched from inside an IDE's Bash sandbox): ahma keeps enforcing, but an **active confinement probe** — a write attempt outside every scope — confirms it is genuinely nested, and ahma discloses that the effective policy is the **intersection** of both sandboxes (so access ahma grants, e.g. the keychain, may still be blocked by the outer one). The probe is why this never false-positives on a normal IDE-launched-but-unconfined MCP server: an IDE sets `CURSOR_SANDBOX`/`CLAUDECODE` in the server's environment without wrapping its executions, so env presence alone is not trusted — only a *blocked* out-of-scope write triggers the disclosure.

Every one of these disclosures includes **actionable remediation** — how to make ahma the single authoritative sandbox for that specific host:

| Host | How to make ahma authoritative |
|------|--------------------------------|
| **Claude Code** | Run ahma as a configured **MCP server** (Claude Code does not sandbox MCP servers — only its Bash tool), rather than from inside its Bash tool; or disable Claude Code's Bash sandbox; or start ahma from a plain terminal outside Claude Code. |
| **Cursor** | Set Cursor's sandbox to `"insecure_none"` in `sandbox.json` (or enable the Legacy Terminal Tool). For terminal hooks specifically, `AHMA_PREFER_OWN_SANDBOX=1`. |
| **VS Code** | Run ahma as its MCP server (VS Code has no execution sandbox of its own to disable). |
| **Docker** | The container is a deliberate outer boundary; run ahma directly on the host if you did not intend the double layer. |

If ahma cannot apply its own sandbox and this is not a recognized nesting case, it still fails loudly (use `--no-sandbox` to defer explicitly) — it never silently runs unsandboxed.

**Choosing your model:**
- Default (hooks): let the host sandbox protect; ahma defers and tells you.
- Want ahma to be the single sandbox? Disable the host's sandbox (e.g. Cursor `sandbox.json` `"type": "insecure_none"`, or the Legacy Terminal Tool) so only ahma sandboxes.
- Want belt-and-suspenders (both)? `AHMA_PREFER_OWN_SANDBOX=1` for hooks (re-introduces the host cache friction).

**Honesty limit:** detecting a host does not prove its sandbox is *enabled*. If you have turned the host sandbox off, deferral means the command is unsandboxed — which is why the disclosure says so explicitly.

## HTTP Transport Authentication

When running in HTTP mode (`ahma serve http`), all `/mcp` endpoints are protected by **bearer token authentication**:

```jsonc
// mcp.json
{
  "mcpServers": {
    "ahma": {
      "url": "http://localhost:4000/mcp",
      "headers": { "Authorization": "Bearer YOUR_TOKEN" }
    }
  }
}
```

- Start the server with `--require-token <token>` (or `auth.require_token` in `~/.ahma/settings.toml`; `AHMA_REQUIRE_TOKEN` is retired and ignored).
- The `Authorization: Bearer` scheme is **case-insensitive** (RFC 7235 §2.1).
- The `/health` endpoint is explicitly **exempt** from authentication so orchestrators can probe liveness without credentials.
- Bearer tokens are compared in **constant time** to prevent timing attacks.
- **Hot-reload**: Send `SIGHUP` to swap the token without restarting; the new token is read from config and applied atomically.

**Manual override** (when you know the outer environment is safe):

```bash
ahma --no-sandbox
```

Common `mcp.json` for nested environments (VS Code with workspace scoping):

```json
{
    "servers": {
        "Ahma": {
            "type": "stdio",
            "command": "ahma",
            "args": ["--tmp", "--livelog", "--simplify"]
        }
    }
}
```

## Package Manager Cache Write (`--no-package-cache-write`)

By default, ahma grants **write access** to the package-manager fetch directories so that agents can autonomously upgrade dependencies (e.g. `cargo add sqlx@0.9`, `cargo update`) without requiring `--sandbox-scope ~/.cargo`:

| Path | Access |
|------|--------|
| `~/.cargo/registry/` | Read + Write |
| `~/.cargo/git/` | Read + Write |
| `~/.cargo/.package-cache` | Read + Write |
| `~/.cargo/.package-cache-mutate` | Read + Write |
| `~/.cargo/bin/` | Read only |
| `~/.cargo/config.toml` | Read only |
| `~/.cargo/credentials.toml` | Read only |

> **Do not use `--sandbox-scope ~/.cargo`**: that flag grants **read-write to the entire cargo home**, including installed binaries and credentials. The built-in `package_cache_write` feature is narrower and safer.

### What that write access costs you

This is a **disclosed residual risk, not a bug** (SPEC R-HANDOFF.8), and it is the reason the opt-out exists. The package cache is shared by *every* project on the machine. An agent working in project X can edit the extracted source of a cached crate, and that edited code is compiled and executed — as a build script or a proc macro — when you later build an unrelated project Y. No sandbox rule is violated at any point: the write is legitimate, and the execution happens in another session, in another project, possibly weeks later, in a build the agent has no part in.

Auto-narrowing (above) does **not** help here, and reasoning by analogy from it is the trap: narrowing bounds an injected write to one subtree of a container you already authorized, but the cache was never inside the container to begin with.

Related, and worth stating plainly: build scripts and proc macros are ordinary programs that run at build time with the **full write set of the sandbox** — the workspace, the temp scopes, and every writable path an enabled profile granted. Adding a dependency is adding code that runs locally; the sandbox bounds *where* that code can write, never *whether* it runs (SPEC R-HANDOFF.9).

To turn the cross-project channel off — this is the mitigation for the risk above, not a generic hardening knob:

```bash
ahma serve stdio --no-package-cache-write
# or in ~/.ahma/settings.toml:
# [sandbox]
# package_cache_write = false
```

It downgrades those `rw` rules to read-execute rather than dropping them, so the toolchain stays runnable and only `cargo add` / `cargo update` stop working inside the sandbox. `$CARGO_HOME` is respected; defaults to `~/.cargo`.

> **These paths are a *profile*, not a hard-coded exception.** The cargo carve-out
> above ships as `rust` in `[sandbox] profiles` — data, not code — so you can see
> exactly what it grants (`ahma permissions list`) and switch it off
> (`profiles = []`). See [permissions.md](permissions.md#sandbox-profiles).

### `cargo install` / `cargo binstall` and other tool installs

`cargo install`, `cargo binstall`, `rustup component add`, `npm i -g`, etc. write a
binary into `~/.cargo/bin` (or the equivalent) **and** update an install manifest
such as `~/.cargo/.crates.toml`. Those paths are intentionally **read-only** (see
the table above), so the install fails with a low-level kernel error — on macOS:

```
error: failed to open: /Users/<you>/.cargo/.crates.toml

Caused by:
  Operation not permitted (os error 1)
```

This is expected and is **not** a bug: installing global binaries is denied by
default. There is no special flag for it — the supported remedy is the standard
**runtime-denial grant loop**:

1. ahma detects the denied path from the command's stderr and returns a structured
   `sandbox_denial` error (over MCP) or prints an `ahma sandbox grant …` hint (in a
   hooked native terminal).
2. **Grant the path** — via the `sandbox_grant` MCP tool (preview, then `confirm: true`),
   or on the CLI: `ahma sandbox grant ~/.cargo/bin` (and `~/.cargo` for the
   manifest). The grant is written to `~/.ahma/settings.toml`, which lives outside
   every sandbox scope. Credential/config files are never auto-granted and the
   path is risk-classified before it is offered.
3. **Apply it** — run the `restart` MCP tool (or restart the server) so the new
   scope takes effect; scopes are immutable for the lifetime of a running session.
4. **Re-run** the original command.

If a maintenance script bootstraps tools (e.g. `cargo install cargo-binstall`),
expect the first run to surface a grant prompt; once granted and applied, the
script proceeds. Prefer scripts that install into a workspace-local directory
(`cargo install --root <workspace>/.tools`) when you want installs to land
in-scope without any grant.

## Temp Directory Access (`--tmp`)

By default, the system temp directory is accessible only via platform-implicit rules. Use `--tmp` (or `[sandbox] tmp_access = true`) to add it as an explicit read/write scope — useful for compilers and build tools.

| Flag combination | Behavior |
|-----------------|----------|
| (default) | Temp access via platform rules |
| `--tmp` | Temp dir added as explicit scope |
| `--disable-temp-files` | Temp access blocked entirely |
| `--tmp --disable-temp-files` | `--disable-temp-files` wins (blocked) |

**Security considerations**: `/tmp` is shared by all users and processes. Use `mktemp` with random suffixes to avoid TOCTOU attacks. Clean up sensitive temp files after use.

## Live Log Monitoring (`--livelog`)

The `--livelog` flag grants additional read-only access to specific log files via symlinks in the `log/` directory at server startup — see [live-log-monitoring.md](live-log-monitoring.md) and [SPEC.md R9](../SPEC.md).

---

## Task Vaults

A **Task Vault** is a per-question isolated working directory that promotes the "dedicated folder per task" security principle from user discipline to a kernel-enforced architectural guarantee.

### Why task vaults?

Cowork's user guidance says: _"Create a per-task working folder. Copy in inputs."_ This is good advice, but it relies on users remembering to follow it. In Ahma, a vault is the only way to start a task — there is no "grant my whole Documents folder" option.

### Directory layout

```
~/.ahma/tasks/<utc-date>-<slug>-<hex>/
  inputs/       — copies of user-provided files (read intent; never modified in-place)
  workdir/      — kernel sandbox scope root; all agent commands run here
  outputs/      — artifacts produced by tools (HTML reports, CSV exports, etc.)
  trash/        — staged-deletion holding area (two-phase delete)
  audit.jsonl   — append-only JSONL audit log of all operations
```

### Creating a vault

```bash
# Create a vault and print its root path
VAULT=$(ahma vault create summarise-q4-report)
echo $VAULT
# ~/.ahma/tasks/20260520T120000Z-summarise-q4-report-a1b2c3d4e5f60001/

# Start an ahma HTTP bridge scoped to that vault's workdir
ahma serve http --task-vault "$VAULT"
```

Or in a single `mcp.json` entry:

```json
{
    "servers": {
        "Ahma (vault)": {
            "type": "stdio",
            "command": "sh",
            "args": ["-c", "ahma serve stdio --task-vault \"$(ahma vault create $SLUG)\""]
        }
    }
}
```

### Security properties

| Property | Detail |
|----------|--------|
| **Kernel-enforced scope** | The sandbox scope is `<vault>/workdir/` — writes outside it are rejected by the kernel on Linux and macOS (see the platform table at the top of this page for what *reads* do, and for Windows) |
| **Inputs are copies** | The agent never touches original files — only copies placed in `inputs/` |
| **Two-phase delete** | `trash/` holds staged deletions; `purge` requires explicit confirmation |
| **Append-only audit** | `audit.jsonl` records every tool call, artifact write, and elevation grant |
| **Egress allowlist** | `egress.allowlist` in the vault root controls outbound network access |

### Two-phase delete

The AI can never permanently delete a file in one step. The `TrashManager` enforces:

1. **Stage** — the file is moved to `trash/<timestamp>_<filename>`; the original location is empty immediately.
2. **Review** — `ahma vault list-staged <vault>` shows what is waiting.
3. **Purge** — only after explicit per-batch confirmation does `purge()` permanently remove staged entries.

This limits the blast radius of a confused `rm -rf` to a recoverable staging operation.

### Audit log format

Each line in `audit.jsonl` is a JSON object:

```json
{"timestamp":"2026-05-20T12:00:01Z","type":"tool_call","operation_id":"op_1","tool_name":"cargo_build","args_summary":"--release"}
{"timestamp":"2026-05-20T12:00:04Z","type":"tool_complete","operation_id":"op_1","success":true,"duration_ms":3200}
{"timestamp":"2026-05-20T12:00:05Z","type":"artifact_written","path":"outputs/result.html","size_bytes":4096}
```

### Renewal contract

A task vault session that runs unattended for more than `T_renew` seconds (default 5 minutes) is automatically halted with a checkpoint written to the vault. The user must re-approve before the operation continues. This closes Cowork's "scheduled task drift" vulnerability where a re-injection can steer a long unattended run without the user knowing.

---

## Egress Sandbox

The **egress sandbox** closes the network egress carve-out present in Cowork (web-fetch and MCP connections bypass org egress policies).

Each task vault can have an `egress.allowlist` file:

```
# ahma egress allowlist — one domain pattern per line
api.openai.com          # explicit allow
*.anthropic.com         # wildcard subdomain
# (empty = deny all outbound connections)
```

When `ahma serve http --task-vault <vault>` starts, an egress proxy is bound to a random localhost port and injected into the subprocess environment:

```
HTTP_PROXY=http://127.0.0.1:<port>
HTTPS_PROXY=http://127.0.0.1:<port>
NO_PROXY=127.0.0.1,::1,localhost
```

Requests to domains not in the allowlist receive `407 Proxy Authentication Required` (CONNECT) or `403 Forbidden` (plain HTTP), indistinguishable from a real network failure — the subprocess learns nothing about which domains are blocked.

On Linux and macOS the kernel FS sandbox blocks the subprocess from modifying `/etc/hosts` or `/etc/resolv.conf`, closing the DNS-rebinding route around the proxy. On Windows that write is not OS-blocked yet (SPEC R6.3.9), so treat the proxy there as the only control.
