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

On Linux, Ahma uses [Landlock](https://docs.kernel.org/userspace-api/landlock.html) — a kernel LSM that applies fine-grained filesystem access rules in-process with no hub or capability escalation required.

**Requirements**: Linux kernel 5.13 or newer (released June 2021). The server refuses to start on older kernels unless sandbox is explicitly disabled.

```bash
uname -r                            # check kernel version
cat /sys/kernel/security/lsm        # verify landlock is active
```

A Landlock rule is an allow-list of file descriptors, so the read-only set is expressed as explicitly as the writable one and anything unnamed is unreadable. This is the position the phrase "outside the scope is denied" actually describes, and it holds **only** here (SPEC R6.1.6).

Moving or hard-linking a file into another directory works wherever writes do. On kernels before 5.19, Landlock refuses any rename that changes a file's directory with "Invalid cross-device link"; `mv` still works there (it copies), but programs that call `rename(2)` directly fail (SPEC R6.1.8).

**Git hooks and `.ahma/` are watched, not protected — unless you opt in** (SPEC R6.1.7). Inside the workspace, Landlock cannot deny the trust-handoff paths — every resolved `<git dir>/hooks` and the project's `.ahma/` — so by default a command run through `run_terminal_command` can write `.git/hooks/pre-commit`, and your `git` will run it later outside any sandbox. ahma detects this instead: it inventories those paths before every command and compares afterwards, and a change leads the tool result with a `TRUST-HANDOFF WRITE` line. Nothing is reverted. See [Detected, not prevented](#detected-not-prevented-linux-and-windows). Where the host allows unprivileged user namespaces, `[sandbox] linux_deny_tier = "namespace"` makes the kernel refuse those writes: see [Prevented, where the host allows it](#prevented-where-the-host-allows-it-linux-opt-in).

**Older kernels / Raspberry Pi**: Landlock requires kernel ≥ 5.13. On older Pi OS kernels, run with:

```bash
ahma serve stdio --no-sandbox
```

or add `"--no-sandbox"` to `mcp.json` args. Disabling enforcement is deliberately CLI-only — `AHMA_DISABLE_SANDBOX` is retired and ignored, because a client-owned config file can carry an environment variable (SPEC R-CFG2.3).

### macOS (Seatbelt)

On macOS, Ahma uses Apple's built-in `sandbox-exec` with a generated Seatbelt profile (SBPL) that restricts **write** access to the sandbox scope. No additional installation required.

**Requirements**: Any modern macOS version. `sandbox-exec` is built into macOS.

**Writes are kernel-scoped; reads are not** (SPEC R6.2.2). On Apple Silicon and macOS 26+, the APFS firmlink / cryptex volume layout means `bash` and `dyld` resolve paths to vnodes that match no traditional `/usr`, `/System`, … subpath prefix — read rules written as subpaths simply never fire, so a profile that tried to scope reads would deny the very commands it exists to protect. The profile therefore emits a bare, unqualified file-*read* allow. This is a platform limitation rather than a grant, which is why it cannot be expressed as a profile and is instead disclosed on every scope surface: the startup banner, `ahma status`, and the TUI scope panel (SPEC R-PERM.5.1).

What keeps secrets unreadable on macOS is therefore an explicit **denylist**, not the scope (SPEC R6.2.3). Each entry is emitted as a `(deny file-read* …)` placed *after* the blanket allow and *before* the workspace-scope allows, so SBPL's last-match-wins ordering keeps them denied by default while an explicit scope grant still wins. The set covers plaintext credential directories, ahma's own control plane, private key material, and container daemon sockets; it is tuned so no common build/test/VCS tool breaks — notably `~/.ssh` is denied as a whole, so a private key is unreadable whatever it is called, while the files ssh needs that hold no secret (`config`, `config.d/`, `known_hosts*`, `*.pub`, `allowed_signers`, the `agent/` socket directory) are let back in. Every path on this list is also one no grant can open. On Linux, where the sandbox reads nothing in home by default, those same ssh files plus `~/.gitconfig` and `~/.config/git` are granted read-only, so git has its identity and credential helper. The effective set is owned by `sandbox/credential_reads.rs`, not by this page: inspect it with `ahma permissions list`, extend it via `[sandbox] deny_credential_reads`, or re-allow a default via `[sandbox] allow_credential_reads`.

> **A denylist is a weaker guarantee than a scope, and is worth reading as one.** A scope denies everything it does not name; a denylist denies only what it *does* name, so any secret nobody thought to enumerate is readable. It is the best available answer on this platform — not an equivalent of the Linux position above.

#### Git authentication: SSH through the agent, or HTTPS through a credential helper

Private keys in `~/.ssh`, whatever they are named, are unreadable inside the sandbox, by design, and no grant can change that. ssh can still use a key without reading it, through the SSH agent: load the key on the host with `ssh-add <key>` (once per login) and sandboxed `git push`/`fetch` sign through the forwarded `$SSH_AUTH_SOCK`. A new host key is added by connecting once from your own terminal. HTTPS works too: git's credential helper (for example `git-credential-osxkeychain`, or `gh auth setup-git`) reads the keychain, which the sandbox allows. ahma says this, in the same words, both when a command is refused a key file and when ssh reports `Permission denied (publickey)`. In terminal hooks, the [SSH key broker](ssh-agent-broker.md) signs for a server you allowed without the agent holding the key.

#### Keychain access (`gh auth` / `git-credential-osxkeychain`)

The login **keychain** (`~/Library/Keychains`) is **allowed by default** (read + write, plus the `com.apple.security*` preference plists). This is what lets `gh`, `git-credential-osxkeychain`, and other Keychain-backed credential helpers work under the sandbox.

Why it's on by default (unlike the plaintext credential dirs above): the keychain is **encrypted at rest**, so blocking file access to it only guards against offline theft of the encrypted database — not against secret extraction, which goes through the `securityd` hub and is gated by each item's ACL (and a GUI prompt) regardless of the sandbox. Blocking it mostly just breaks tools: `gh auth login` appears to succeed but writes the OAuth token where `gh` can't read it back, so every later `gh` call falls back to unauthenticated (HTTP 401 / the anonymous IP rate limit).

For maximum defense-in-depth on high-security machines, turn it off:

```toml
[sandbox]
allow_keychain = false   # blocks keychain read+write; breaks gh and similar tools
```

or per-invocation with `--no-allow-keychain` (and `--allow-keychain` to force it on when settings disable it). When off, `~/Library/Keychains` is added to the credential-read deny set and keychain writes are blocked, so `security add-generic-password` fails with *"The authorization was denied"*.

> **Note on nesting:** if you launch ahma from *inside* another sandbox (e.g. an editor's Bash sandbox), tool subprocesses run under the **intersection** of both profiles — so keychain access ahma grants can still be blocked by the outer sandbox. Start ahma outside that shell, or see [Nested Sandbox Environments](#nested-sandbox-environments-cursor-vs-code-docker).

### Windows (Job Objects; AppContainer pending)

On Windows, Ahma applies a Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) at startup, which guarantees child processes are killed when the server exits. A Job Object does **not** restrict filesystem access by path in either direction — not writes, not reads. Path confinement would arrive with per-command AppContainer isolation. That is written — per-session container SID, scoped ACL grants, launcher re-entry — and on `windows-latest` its boundary holds both ways: inside the container a command can write and read the scope and is denied outside it. It is still switched off, because ordinary tools cannot run inside it yet: writing to `NUL` is denied, and so is looking at any folder above the scope, which breaks path canonicalization and Windows PowerShell 5.1 started from a short (8.3) path. The honest position today is **process-lifetime containment and no kernel filesystem boundary** (SPEC R6.3.9). The `appcontainer_dacl_diagnostics` test measures the boundary without depending on a shell, through a probe mode of `ahma.exe` that reports raw OS error codes and the container token; it runs on every Windows CI leg. ahma's own path validation still binds the paths ahma resolves, but it cannot bind what a spawned command does with its own syscalls — so the scope shown on Windows is a scope ahma honours, not one the OS enforces.

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
| **Linux** | **application-layer only by default, and detected — not prevented — for shell commands.** Landlock's ABI is additive-allow with no deny rule and no ordering, so the hole cannot be expressed to it (SPEC R6.1.7). ahma enforces it in its own file tools, which means it is **bypassable from `run_terminal_command`**: a shell child inherits the workspace-wide write right and can create a hook script directly. Such a write is reported after the command (below). The opt-in `[sandbox] linux_deny_tier = "namespace"` holds the existing paths with a read-only mount instead, where the host allows it (below) |
| **Windows** | no filesystem enforcement yet (SPEC R6.3.9); the application-layer check is the only control, and shell-command writes are detected and reported as on Linux |

The shape-matched rules are application-layer on *every* platform by construction: a kernel deny on every `bin/python*` would break a legitimate `python -m venv`.

### Prevented, where the host allows it (Linux, opt-in)

```toml
# ~/.ahma/settings.toml
[sandbox]
linux_deny_tier = "namespace"   # default "detect"
```

With this set, each command ahma runs enters a user and mount namespace of its own before Landlock is applied, and every deny-tier path that exists when it starts — each resolved `<git dir>/hooks`, the workspace's `.ahma/` — is bind-mounted onto itself read-only. A write there fails in the command:

```text
$ echo x > .git/hooks/pre-commit
sh: 1: cannot create .git/hooks/pre-commit: Read-only file system
```

Everything else in the workspace stays writable, the hooks stay readable, and `git commit` run by the command still executes them (ahma never adds `noexec`). The child cannot undo the mount: Landlock, applied right after, forbids a sandboxed task to change its mounts.

**It needs unprivileged user namespaces, and many hosts refuse them.** ahma forks one probe at startup that performs the whole sequence on a temporary directory and checks that a write really fails; if it does not, nothing is attempted per command, a `warn` says why, and `ahma status` and the startup scope disclosure carry the reason (SPEC R-PERM.5.1). The common cases:

| Probe result | Typical cause | To get prevention |
|---|---|---|
| id maps refused | Ubuntu 23.10+ (`kernel.apparmor_restrict_unprivileged_userns = 1`): the namespace is created but holds no privilege | an administrator installs an AppArmor profile for the ahma binary that allows `userns` (below), or sets that sysctl to `0` |
| user namespaces refused | a container's seccomp profile (Docker's default), `user.max_user_namespaces = 0` | run the container with a profile that allows `unshare(CLONE_NEWUSER)` |
| already confined | this ahma runs inside another ahma's sandbox (or a command run through one) | run ahma outside the other sandbox |

An AppArmor profile in the form Ubuntu documents for programs that need user namespaces (adjust the path to where `ahma` is installed; `sudo apparmor_parser -r /etc/apparmor.d/ahma` loads it):

```text
# /etc/apparmor.d/ahma
abi <abi/4.0>,
include <tunables/global>

profile ahma /home/*/.local/bin/ahma flags=(unconfined) {
  userns,
  include if exists <local/ahma>
}
```

What it does **not** cover, which is why detection stays on in this mode too:

- **A path created during the command.** A bind needs an existing mount point, so a `git init`, a `git clone` or a first `.ahma/` is covered from the next command; ahma never creates the directory to protect it.
- **Renaming an ancestor.** `mv .git .git.old` and rebuilding `.git` sidesteps any path-based rule — on macOS too. Detection reports the new hook.
- **A child that cannot enter its namespace** (for instance one forked from a thread already inside a Landlock domain) runs without it rather than failing; its write is detected as in the default mode. So that ahma's own threads are never such a thread, in this mode the server does not apply Landlock to the thread that commits the scope — every command is still Landlock-confined at spawn, which is where command containment has always come from.

Inside the namespace, files owned by other users (root included) show as owned by `nobody`, because only your own uid and gid are mapped. Access checks are unchanged — they use the real ids — and setuid programs such as `sudo` already do not work under Landlock's `no_new_privs`.

### Detected, not prevented (Linux and Windows)

Kernel prevention on Linux needs a private mount namespace per command, which is the opt-in above, and stock Ubuntu 24.04 refuses unprivileged user namespaces. Until that is generally available or a Landlock "no-inherit" rule exists, ahma closes the *silent* half of the gap. Wherever the kernel does not hold the deny tier — Linux, Windows, and any session running with `--no-sandbox` — every command that may write is bracketed by two inventories of the deny-tier paths (the same set the macOS kernel rules deny: every resolved `<git dir>/hooks`, every `<scope>/.ahma`). An entry that was created, modified, removed or made executable while the command ran is reported three ways:

1. **At the top of the tool result**, after the identity line, one line per entry:

   ```text
   TRUST-HANDOFF WRITE: /home/me/project/.git/hooks/pre-commit (created) — git runs files in .git/hooks outside any sandbox; review before your next git command
   Detected after the command ran, not prevented: on this platform the kernel does not stop writes to these paths (SPEC R6.1.7). Another process, such as your editor, may have made the change. Nothing was reverted.
   ```

   A change under `.ahma/` names its own trigger instead: ahma loads those tool definitions on its next start or `restart`.
2. **At `warn` in ahma's log**, with the same wording.
3. **In the execution audit log** as a `handoff_write` record per entry — the durable half, because the hook runs later, when nobody is reading the transcript ([execution-audit-log.md](execution-audit-log.md#handoff_write)).

The hub and the TUI receive the alert as the operation's alert event. A PTY or persistent-session command (`pty: true`, `session_id`) publishes its own result, so for those the alert goes to the log and the audit trail only.

What this does **not** do, on purpose:

- **Revert.** The change may be your own edit made while the command ran; ahma says so rather than undoing it.
- **Watch a repository the command itself creates.** The paths are resolved when the command starts, the same floor as the macOS kernel rules (SPEC R-HANDOFF.2); a `git clone` is covered from the next command.
- **Walk anything else.** Only the deny-tier directories are inventoried, at most 500 entries each and four levels deep. A directory that holds more is reported as `TRUST-HANDOFF WATCH INCOMPLETE` on every command while it stays that way, so a planted pile of files cannot push a real hook out of view quietly.
- **Attribute the change.** Entries are compared by size, modification time and permission bits (on Unix also inode number and inode-change time, which `touch -r` cannot forge), not by who wrote them.
- **Report ahma's own writes.** ahma's log directory (by default `.ahma/logs`, which receives the operation output and this audit log while a command runs), the `.ahma/.gitignore` it maintains for it, and the user-level `~/.ahma` are left out of the inventory.
- **Run on macOS**, where the kernel already refuses these writes. An operator opt-in (`--allow-git-hooks`, `--allow-project-tool-config`) removes its path from the watched set too, exactly as it does from the deny set.

**The child's environment is part of the same surface.** Variables that cause an unrelated process to load code of the agent's choosing (`BASH_ENV`, `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES` and family) or that re-point a trusted client at an attacker-chosen endpoint (`DOCKER_HOST`) are stripped from every sandboxed child. `SSH_AUTH_SOCK` is deliberately **kept**: it is a capability to *use* keys, not to read them, and it is what lets git-over-ssh keep working while the key files stay denied (SPEC R-HANDOFF.5).

## Signals: a command may stop only what it started

The profile also grants `process-info*`, so `pgrep` and `lsof` work under it (SPEC R6.2.8). `/bin/ps` and `top` are setuid root, and no sandbox can execute a setuid binary (a kernel rule, not a missing allow), so `ahma ps [FILTER]` lists processes instead — pid, parent, start, whether the process is itself sandboxed, command line — and is read-only and never queued. Denying process information protected nothing and left agents unable to see what they were waiting for.

On macOS the Seatbelt profile grants `(allow signal (target same-sandbox))`, so a sandboxed command can `kill` the process tree it started under the same profile and nothing else. A `kill` aimed at another session's build, a server from an earlier command, or any other process of yours is refused by the kernel, and ahma explains the `kill: (N) - Operation not permitted` it sees as a boundary rather than a dead pid: the agent is told the pid belongs to something outside its sandbox and that a human must stop it (SPEC R6.2.6). `[sandbox] signal_other_processes = true` is the explicit opt-out, logged at startup. (Linux and Windows do not confine signals; the README's *What the sandbox does not cover* says so.)

## Tools that bring their own sandbox (SwiftPM, xcodebuild)

SwiftPM's manifest loader and `xcodebuild` package resolution call `sandbox-exec` themselves, and macOS allows one profile per process tree, so inside ahma's sandbox they fail with `sandbox_apply: Operation not permitted`. This is the nesting limit, not a path: no grant helps, and ahma says so instead of offering one (SPEC R7.7). Use the tool's own switch: `swift build --disable-sandbox` / `swift package resolve --disable-sandbox`, or for `xcodebuild` add `-IDEPackageSupportDisableManifestSandbox=YES` to the command (or run `defaults write com.apple.dt.Xcode IDEPackageSupportDisableManifestSandbox -bool YES` once to make it permanent). Either turns off only SwiftPM's manifest sandbox; ahma's sandbox still confines the build. Swift macros (`@Observable` and friends) hit the same limit through the macro plugin server, failing with "swift-plugin-server produced malformed response": add `'OTHER_SWIFT_FLAGS=$(inherited) -disable-sandbox'` to the `xcodebuild` command (or `-disable-sandbox` to `swiftc`). A workspace whose packages were resolved once outside ahma no longer loads manifests under `sandbox-exec`.

## Helpers left running inside a sandbox (sccache, Gradle and Kotlin daemons)

A long-lived helper started by a sandboxed command inherits the sandbox and outlives the command. A sccache server started that way serves every session on the machine but can write only the checkout it was born in, so every other checkout's Rust build fails under its own `target/` with a bare `Operation not permitted`. `ahma doctor` finds any such process by its shape — confined, reparented to launchd, your own executable — and names the restart (`sccache --stop-server && sccache --start-server` from a plain terminal); `ahma doctor --fix` runs it, and an unconfined ahma server restarts a confined sccache on its own at startup (SPEC R-DOCTOR.7).

## GPU (Metal): denied unless you opt in

The Seatbelt profile is `(deny default)` and grants no `iokit-open`, so Metal cannot open the GPU inside a sandboxed command: llama.cpp reports `failed to create command queue`, a Metal probe sees no device, and GPU-accelerated tests fall back to the CPU or fail. This is a **capability**, not a path, so no prompt can grant it and the `sandbox_grant` tool is the wrong tool; ahma recognises the failure and tells the agent so instead of letting it hunt for a directory to request. `[sandbox] allow_gpu = true` enables it by adding only the Metal user-client classes Apple's own profiles use (`AGX*` on Apple silicon, `IOAccel*`/`IGAccel*` on AMD and Intel, the IOGPUFamily client a paravirtualised GPU presents in a VM, and `IOSurface`), never a blanket IOKit allow, which would also hand over cameras and HID devices (SPEC R6.2.7). While the GPU is withheld, every scope display says so. With it on, Metal compute works in full (buffers, shaders compiled at runtime, dispatch), which is what whisper.cpp, llama.cpp and MLX need; CI proves it on every macOS run. A WebKit view needs no setting: it runs in the default sandbox, and its "Could not create a sandbox extension" messages are noise. A failed Metal buffer allocation (whisper.cpp and llama.cpp report `failed to allocate buffer` through ggml) is recognised the same way. A sandboxed command may allocate pseudo-terminals of its own (test runners and `script` need them), never another process's terminal (SPEC R6.2.11).

Launching an app through LaunchServices (`open App.app`, `open -a`, `open <URL>`) is never allowed: LaunchServices starts the app outside every sandbox. Run the app's binary directly (`App.app/Contents/MacOS/App`) to keep it confined; ahma says so when `open` is refused (SPEC R6.2.10).

The hub and the workers it spawns start in ahma's runtime directory, never in the checkout the hub happened to be launched from: a worker's scope comes from its own client's `roots/list`, and its tools directory and operation logs follow that scope. Before this, every worker inherited the first checkout's directory, logged into it, and loaded that checkout's `.ahma/` as the trusted tool set of every other project (SPEC R-HUB.12).

## Network egress

The filesystem sandbox says nothing about the network, and **egress is unrestricted by default**. Pass `--restrict-network` (or set `[network] restrict = true`) to route sandboxed subprocesses through a guarded local proxy that forwards only the domains in `[network] allow` — deny-all when that list is empty — and refuses private, loopback and cloud-metadata addresses. The README's *What the sandbox does not cover* section states what that restriction is and is not on each platform. How the allowlist is built, and the hosts each sandbox profile contributes: [network-egress.md](network-egress.md).

## Nested Sandbox Environments (Cursor, Claude Code, VS Code, Docker)

When ahma runs inside a host that may provide its own kernel sandbox (Cursor's agent sandbox, Claude Code's Bash sandbox, VS Code, Docker), ahma does **not** try to mimic or coexist with the host's internals (such as the build-cache environment variables Cursor injects), and it does **not** stand down because the host *might* be sandboxing. It applies **its own sandbox on every execution path**, defers only when the kernel proves an outer sandbox is enforcing, and **always tells you which one is active**.

ahma can *name* a host from environment markers (`CURSOR_SANDBOX`/`CURSOR_AGENT`, `CLAUDECODE`/`CLAUDE_CODE_ENTRYPOINT`, `VSCODE_*`, `/.dockerenv`/`container`). A marker only proves which harness launched the process. Claude Code, for example, sets `CLAUDECODE=1` for every process it starts whether or not its Bash sandbox is enabled — and it is off by default. An earlier release let terminal hooks pass commands through unchanged on that marker, so a Claude Code session ran every Bash command unsandboxed for days while `ahma hooks status` said ACTIVE and the disclosure was written to a hook field Claude Code does not render. That is why markers now name, and never decide (SPEC R7.2).

**A client's own terminal is outside unless hooked.** ahma confines what it runs. A client's own terminal (VS Code's, Claude Desktop's, Zed's, LM Studio's, or Claude Code's Bash tool without ahma's hook) runs outside every boundary ahma has. The `status` tool and the TUI session list say so for each connected client (SPEC R7.8).

**Terminal hooks → ahma's sandbox, always.** Every hooked shell command is rewritten into `ahma hooks run-shell` and runs under ahma's kernel sandbox, scoped to the enclosing repository (plus persistent grants). The first command of a session carries a scope disclosure the model and the user both see (SPEC R5.4.10), in the fields the harness renders (Claude Code: `additionalContext` and `systemMessage`):

> Sandbox: this shell command runs inside ahma's kernel sandbox (terminal hook). Writes are confined to: /Users/you/github/project, plus any persistent grants in ~/.ahma/settings.toml. Reads are NOT confined on macOS (except credential directories) — a platform limit. A write outside the scope fails; to allow one, ask the human — they approve it in the ahma TUI or run `ahma sandbox grant <dir>`. You cannot widen the scope yourself.

The same install writes an **edit guard** for the harness's native file tools (Claude Code `Edit`/`Write`, Codex `apply_patch`, Copilot `edit`/`create`, Cursor `Write`, Antigravity `write_to_file`), which never pass through the shell sandbox: an edit whose target is outside that same scope is refused with the same remediation (SPEC R5.5.6). A harness's own working set — Claude Code's plan files under `~/.claude/plans` and its per-session scratchpad under `/tmp/claude-<uid>/` — is writable without a grant and listed in the scope disclosure (SPEC R5.5.7); a `session` answer at a prompt reaches hooked commands and native edits too (SPEC R-PERM.4.4). Decline it with `ahma hooks install --no-edit-guard`; `ahma hooks status` shows `installed+guard` when it is in place.

`AHMA_PREFER_OWN_SANDBOX` is retired — own sandbox is the only behaviour.

**MCP server (`run_terminal_command`) → ahma stays authoritative.** Commands the agent runs through ahma's MCP tools execute in ahma's own process, which the host's terminal sandbox does **not** wrap, so ahma applies its own sandbox and remains the authority.

Two nested cases are handled loudly, at server startup and on every hooked command (SPEC R5.4 "nothing silent"):

- **ahma cannot nest its own sandbox** (macOS `sandbox-exec` is *denied* — positive proof ahma is inside a restrictive outer sandbox): instead of hard-failing, ahma **defers to that host** and discloses it loudly, with host-specific remediation. This is fail-closed — a blocked nesting attempt proves an outer sandbox is enforcing. A hooked command in this state prints the disclosure on stderr every time.

  This is a platform rule, not a configuration: macOS refuses to apply a Seatbelt profile inside a process that is already confined by one whenever the outer profile denies *anything* (measured on macOS 26 — `(allow default)` plus a single `deny` of a nonexistent path is enough). Every real sandbox forbids nesting, ahma's own included, and no profile ahma could generate changes that. Every child of a confined process inherits the confinement, so a command ahma runs *without* its own wrapper is still kernel-sandboxed — by the outer boundary. That is why the deferral is decided when a `Sandbox` is **constructed**, on every execution path — `ahma serve` startup, `ahma hooks run-shell`, *and* the in-process library used by ahma's own tests and by embedders — from the kernel's own answer (`sandbox_check` on ahma's pid) confirmed by a refused nesting probe, never from environment markers alone.

  The common way to hit it is ahma running ahma: `cargo nextest run` executed through `run_terminal_command`, or a nested `ahma serve`. Every command ahma sandboxes carries the marker `AHMA_OUTER_SANDBOX_PID=<pid>` (set by ahma, never read as a setting), so the nested ahma names the outer one:

  > Sandbox: ahma is DEFERRING to an outer ahma's sandbox and is NOT applying its own. Protection now depends on an outer ahma. … This process was started by an outer ahma `run_terminal_command`, whose kernel sandbox already confines every write it makes …

  The nested ahma's own scope is still validated in-process (path checks on file tools) but is not kernel-enforced — the outer sandbox's boundary is. For ahma's own enforcement, start the process from a plain terminal instead (SPEC R7.6).
- **ahma is enforcing *on top of* a host sandbox** (ahma launched from inside an IDE's Bash sandbox): ahma keeps enforcing, but an **active confinement probe** — a write attempt outside every scope — confirms it is genuinely nested, and ahma discloses that the effective policy is the **intersection** of both sandboxes (so access ahma grants, e.g. the keychain, may still be blocked by the outer one). The probe is why this never false-positives on a normal IDE-launched-but-unconfined MCP server: an IDE sets `CURSOR_SANDBOX`/`CLAUDECODE` in the server's environment without wrapping its executions, so env presence alone is not trusted — only a *blocked* out-of-scope write triggers the disclosure.

Every one of these disclosures includes **actionable remediation** — how to make ahma the single authoritative sandbox for that specific host:

| Host | How to make ahma authoritative |
|------|--------------------------------|
| **Claude Code** | Run ahma as a configured **MCP server** (Claude Code does not sandbox MCP servers — only its Bash tool), rather than from inside its Bash tool; or disable Claude Code's Bash sandbox; or start ahma from a plain terminal outside Claude Code. With Claude Code's sandbox off (the default) ahma's hook is the only sandbox a Bash command gets. |
| **Cursor** | Set Cursor's sandbox to `"insecure_none"` in `sandbox.json` (or enable the Legacy Terminal Tool) so only ahma sandboxes. |
| **VS Code** | Run ahma as its MCP server (VS Code has no execution sandbox of its own to disable). |
| **Docker** | The container is a deliberate outer boundary; run ahma directly on the host if you did not intend the double layer. |

If ahma cannot apply its own sandbox and this is not a recognized nesting case, it still fails loudly (use `--no-sandbox` to defer explicitly) — it never silently runs unsandboxed.

**Honesty limit:** detecting a host does not prove its sandbox is *enabled*. That is exactly why detection never decides enforcement; and whenever ahma does defer — on kernel proof, or because you passed `--no-sandbox` — the disclosure says that protection now depends on the host.

## HTTP Transport Authentication

When `ahma serve http` is started with `--require-token <token>` or `--require-token-path <file>`, every `/mcp` endpoint requires **bearer token authentication** (without either, there is none — bind to loopback):

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
            "args": ["--tmp", "--log-monitor"]
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
2. **A human grants the path** — at the prompt the `sandbox_grant` MCP tool raises
   (preview first; `confirm: true` asks you, in your client or the ahma TUI, and never
   grants by itself), or on the CLI: `ahma sandbox grant ~/.cargo/bin` (and `~/.cargo`
   for the manifest). The grant is written to `~/.ahma/settings.toml`, which lives
   outside every sandbox scope, bound to the workspace it was approved for — sessions in
   other projects never see it (SPEC R5.4.11). Credential/config files are never
   auto-granted and the path is risk-classified before it is offered. A grant you approve
   at the prompt takes effect **immediately** for the running session and persists; answer
   for this session only if that is all you want. (If granted via CLI or offline editing,
   restart the server for it to take effect.)
3. **Re-run** the original command.

If a maintenance script bootstraps tools (e.g. `cargo install cargo-binstall`),
expect the first run to surface a grant prompt; once granted and applied, the
script proceeds. Prefer scripts that install into a workspace-local directory
(`cargo install --root <workspace>/.tools`) when you want installs to land
in-scope without any grant.

## Temp Directory Access (`--tmp`)

By default, the system temp directory is accessible only via platform-implicit rules. `--tmp` (or `[sandbox] tmp_access = true`) asks for it as an explicit read/write scope — useful for compilers and build tools that take a temp path as a working directory or argument.

Under `ahma serve` that is a **request, not a grant** (SPEC R5.2.5, R5.3). The temp directory is shared by every program on the machine, and an MCP config file is client-owned, so a `--tmp` in it is nobody's consent. Once the workspace scope is committed — after `notifications/sandbox/configured`, never delaying it — ahma asks you once, through the same question ladder as every grant: your client's own prompt if it supports elicitation, else an attached `ahma tui`, else nobody. The question names the literal canonical temp path, says nothing was blocked, and offers **deny, next command only, or this session only** — never "always" or 24 hours. A yes adds the directory to this session's live scope; a deny, a dismissed prompt, or nobody to ask leaves it out (fail closed). A dismissed prompt (`cancel`) is not recorded as a denial. `ahma tool run --tmp` and terminal hooks keep the old behaviour: you typed the flag (or set it in your own settings), so it is the answer.

| Flag combination | Behavior |
|-----------------|----------|
| (default) | Temp access via platform rules |
| `ahma serve … --tmp` | Asked once per session; temp dir in scope only after a yes |
| `ahma tool run --tmp …` | Temp dir added as explicit scope |
| `--disable-temp-files` | Temp access blocked entirely |
| `--tmp --disable-temp-files` | `--disable-temp-files` wins (blocked) |

The scope summary shows where it stands: `tmp  : requested, awaiting consent`, `tmp  : granted (session)` or `tmp  : off` in text, and `tmp` (in scope), `tmp_requested` and `tmp_in_scope` in the `notifications/sandbox/configured` payload. With a workspace that itself lives inside the temp directory, the grant would widen the scope above the workspace, so it is refused and not asked.

**Security considerations**: `/tmp` is shared by all users and processes. Use `mktemp` with random suffixes to avoid TOCTOU attacks. Clean up sensitive temp files after use.

## Live Log Monitoring (`--log-monitor`)

With `--log-monitor` (or `[logging] log_monitor = true`) the sandbox also grants read-only
access to the targets of symlinks in `.ahma/logs/` at startup, so a `livelog` tool can
follow a log file that lives outside the workspace. A target outside the workspace is
added only once a human approves it — `logs_approve` asks, it never approves on the
agent's word — see
[live-log-monitoring.md](live-log-monitoring.md) and SPEC R9.

## Task Vaults

`--task-vault <dir>` makes a per-task directory's `workdir/` the whole sandbox scope and
turns `rm` into a recoverable move to `trash/`. See [task-vault.md](task-vault.md).
