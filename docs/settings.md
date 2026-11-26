# Ahma Settings File (`~/.ahma/settings.toml`)

The settings file is the primary way to configure Ahma's behaviour.  
`AHMA_*` environment variables are **not** a configuration source: they are retired and
ignored with a warning ([environment-variables.md](environment-variables.md)).

## Getting started

```bash
ahma settings init        # create with all defaults commented out
ahma settings show        # print effective configuration (with source annotations)
ahma settings show --origin  # print exact per-key provenance (cli / user / default + file path)
ahma settings set tools.timeout_secs 3600   # change one key, in its own table, checked
ahma settings unset tools.timeout_secs      # back to the compiled-in default
ahma settings set --project tools.timeout_secs 3600   # this repository's .ahma/settings.toml
ahma settings explain sandbox.allow_gpu     # what a key does, its default, who may set it
ahma settings explain                       # list every key by table
ahma --no-settings serve stdio  # ignore settings file for one invocation
```

`--origin` reports the *true* source of each setting: `cli` when a flag passed in
the same invocation overrides the key (e.g. `ahma --timeout 30 settings show --origin`),
`user (<path>)` when the settings file explicitly sets the key — even if it sets it
to the default value — and `default` otherwise. Both `--settings-path` and
`--no-settings` are honored.

### Asking what a setting means

`ahma settings explain <table>.<key>` prints the key's description (the same
words as the comment above it in the settings file), its default, who may set
it, and the command that changes it. "Who may set it" is the trust tier
(SPEC R-CFG2): a *preference* may also come from a repository's
`.ahma/settings.toml`; a *security* key only from your own file or a flag. A
key that is a grant (a directory, a host, a tool approval) points at its own
command (`ahma sandbox grant`, `ahma network allow`, …) rather than `settings
set`, because those check the path or host and write an audit record. With no
key it lists every setting, marking the ones a repository may also set. It reads
only text built into ahma, so an agent can run it inside the sandbox
(SPEC R-CFG11).

### What `settings set` prints

The values before and after, then what the change means, in sentences:

```text
tools.long_running changed in /Users/you/.ahma/settings.toml
  was: [{ command = "make serve" }]
  now: [{ command = "scripts/review", timeout_secs = 86400 }]

What it does: Commands that run for hours or wait for a person. A command line that starts with one of these, in any repository, runs beside builds and tests instead of waiting its turn, with its own timeout; the sandbox is unchanged (SPEC R2.7.10).
Where it applies: every repository you work in on this machine. This is your own settings file, and no repository's own file can change it.
No longer in the list: { command = "make serve" }. `settings set` replaces the whole list, so to keep an entry, set the list again with every entry you want.
To undo: ahma settings set tools.long_running '[{ command = "make serve" }]'
```

- **Where it applies** depends on the file written. Your own
  `~/.ahma/settings.toml` reaches every repository on this machine; for a
  preference, a repository's own file can still override it there. `--project`
  reaches this repository only. A file named with `--path` reaches nothing until
  ahma is started with `--settings-path` naming it.
- **A list is replaced, not added to.** Entries the new value no longer holds
  are named, so one dropped by accident is visible at once. To keep them, set
  the list again with every entry.
- **To undo** is the one command that restores the previous value (`unset` when
  that was the default). (SPEC R-CFG10.8.)

### What `settings set` will and will not write

- **`--project`** edits this repository's `.ahma/settings.toml`. Only preferences
  go there; a security-tier key (everything in `[sandbox]`, `[network]`, `[web]`, `[auth]`, `[permissions]`, `[open_url]`, `[log_targets]`, plus `tools.long_running` and `http.unix_socket_path`) is refused with the command
  that sets it for you, because the file could not set it and ahma would only
  name it as ignored at every startup. The file gets its own header and does not
  list the security tables. With no `.ahma` directory it refuses; run it from
  the repository root.
- **Grants are not settings.** `sandbox.persistent_scopes`, `network.allow`,
  `web.always_allow` / `never_allow`, `permissions.tool_approvals`,
  `sandbox.ssh_sign` and `log_targets.approvals` are refused by `settings set`
  and name their own command (`ahma sandbox grant`, `ahma network allow`,
  `ahma egress allow|deny`, `ahma permissions revoke`). Those check a path against
  the hard denylist, or a host against the host patterns, and write the audit
  record; `settings set` does neither.
- **Every change is recorded.** `settings set|unset` and the TUI settings panel
  append one line per key to `~/.ahma/permissions-audit.jsonl` (`kind: setting`,
  the key, `user: false -> true`, and `cli` or `tui`). A secret's values are
  redacted. (SPEC R-CFG10.5-7.)

### What a server logs about its settings at startup

Every server process (`ahma serve …` and the per-user hub) logs one line for each
setting whose effective value differs from the compiled-in default, naming the
key, the value, the default and the source — the same source `--origin` reports:

```
INFO  setting tools.timeout_secs = 600 (default 1800) from user (/home/me/.ahma/settings.toml) (SPEC R-CFG5.2)
WARN  security-tier setting sandbox.disable_temp = true (default false) from user (/home/me/.ahma/settings.toml) (SPEC R-CFG5.2)
```

Security-tier settings (everything in `[sandbox]`, `[network]`, `[web]`, `[auth]`, `[permissions]`, `[open_url]`, `[log_targets]`, plus `tools.long_running` and `http.unix_socket_path`) log at `warn`, so anything that
shapes the sandbox is visible in the log without asking. A setting left at its
default logs nothing. On Unix, a settings file that group or others can write
also logs a `warn` with the fix (`chmod go-w <file>`): whoever can edit that
file can change how ahma confines your agent (SPEC R-CFG6.3).

## A settings file that does not parse

ahma will not guess. A settings file it can read but cannot parse (a typo, or a
table such as `[tools]` written twice) stops every command that would use it
(SPEC R-CFG6.1). For terminal hooks that means **every hooked command is
refused**, not run unsandboxed: the agent gets one line naming the file, the
line, the reason and the fix, for example

```
ahma refused this call: the settings file does not parse — /home/me/.ahma/settings.toml line 254 (`[tools]`): duplicate key. … Fix: [tools] appears twice: delete line 254 and move the keys under it into the first [tools], then run `ahma doctor` to confirm.
```

`ahma doctor` and `ahma settings` still run, on compiled-in defaults, so you
can diagnose and repair the file (SPEC R-CFG6.4).

## File location

| Platform | Default path |
|----------|--------------|
| macOS / Linux | `~/.ahma/settings.toml` |
| Windows | `%USERPROFILE%\.ahma\settings.toml` |

Override the path for a single invocation:
```bash
ahma --settings-path /path/to/my.toml serve stdio
```

---

## Priority order (highest wins)

1. **CLI flags** (`--timeout 600`, `--disable-temp-files`, …)
2. **`<workspace>/.ahma/settings.toml`** — preference keys only ([below](#project-settings-workspaceahmasettingstoml))
3. **`~/.ahma/settings.toml`**
4. **Compiled-in defaults**

Two switches are CLI-flag-only and cannot be set from any file, so they are always
visible where ahma is launched: `--no-sandbox` and `--insecure-skip-verify` (SPEC R-CFG2.3).

---

## Every key

ahma keeps `~/.ahma/settings.toml` up to date on startup: every option is listed with its
default, and only values you changed are uncommented. `AHMA_*` variables are ignored.
`ahma settings init` resets the file. `ahma settings explain` lists every key by table, and
`ahma settings explain <table>.<key>` gives its default and tier (SPEC R-CFG11).

---

## Sync or async (`tools.execution_mode`)

Every tool call runs as a tracked operation — it shows in `status` and the TUI,
can be cancelled, and writes its complete output to an `output_file`. The mode
only decides how long the call waits before answering:

| Mode | What a call returns | Use it when |
|---|---|---|
| `async` (default) | The result if the command finishes within a short adaptive window (≈10 s idle, ≈1 s when other work is running), otherwise an operation id to collect with `await` — the model keeps thinking while a long build or test run goes on. | Almost always. |
| `sync` | The command's result, once it finishes. If it outlasts what the client can hold one request open for, the call returns the operation id and says to `await` it — the result is never lost to a closed connection. | A client or model that handles operation ids badly. |

Async is safe as the default because of the **workspace write queue**: commands
that may write a workspace run one at a time, in the order they were sent,
across every ahma session on the machine; read-only commands skip the queue under
a sandbox that forbids them to write; a result nobody collected is delivered with
the next tool result. See [workspace-queue.md](workspace-queue.md).

Set it any of these ways (highest priority first):

- `--sync` / `--async` on the command line (the last one given wins);
- `execution_mode = "sync"` under `[tools]` in a project's `.ahma/settings.toml`;
- the same in `~/.ahma/settings.toml`;
- in `ahma tui`: `/sync` or `/async`, or **Settings → Tools → Execution** (Space
  cycles, `s` saves). Both write `~/.ahma/settings.toml`.

A running ahma session read its mode when it started: a change reaches new
sessions, and a running one after it restarts (the agent's `restart` tool).

One call can wait regardless of the mode: an MTDF tool marked `"synchronous": true`
(deprecated), a call with `"blocking": true`, or `run_terminal_command`'s
unadvertised `execution_mode: "Synchronous"` waits exactly as `sync` mode does —
still a tracked operation, still bounded by the client's request budget.
`"synchronous": false` or `"blocking": false` returns after the adaptive window even
in `sync` mode. `ahma tool run` (the CLI one-shot) and a terminal hook run their
command as an operation too, and wait for it with no budget: they have nothing to
`await` with later.

| Key (`[tools]`) | Default | Effect |
|---|---|---|
| `workspace_queue` | `true` | Writers run one at a time per workspace, in arrival order (SPEC R2.7). Off restores unordered async — only sensible with `execution_mode = "sync"`. |
| `edit_guard` | `true` | ahma's own `write_file`/`replace_in_file`/`multi_edit`/`apply_patch` refuse an edit while a command that may rewrite sources runs in that workspace (SPEC R2.7.8). Builds and tests never block an edit. |
| `source_readers` | `[]` | This project's own commands that only read sources, matched as a prefix of the command line in whole words (`["scripts/heavy", "make test"]`). Edits proceed while one runs. |
| `long_running` | `[]` | Commands that wait for a person or run for hours (`[{ command = "scripts/audition ask", timeout_secs = 14400 }]`). A line that is exactly one of them runs without the write-queue lease, with its own timeout; leading `cd <dir> &&` steps and a longer path to the same program still match. Security tier: your own settings file only, never a repository's (SPEC R2.7.10). |

Details: SPEC R2.1, R2.4, R2.7.

---

## Small models and token budgets (`ahma tui` chat)

These shape what the TUI's chat agent sends to its model; they do not change what
tools run or return to other MCP clients.

| Flag | Settings key | Effect |
|------|--------------|--------|
| `--context-length <tokens>` | — | Model window size. Caps one tool result at ¼ of it and the whole conversation at ¾ (≈4 chars/token), and enables proactive compaction |
| `--small-model-harness` / `--no-…` | `tools.small_model_harness` | Without `--context-length`: tool results capped at 8 000 chars and the conversation at 24 000 (default 60 000 / 240 000); adds error and read hints after tool calls |
| `--minimize-tokens` / `--no-…` | `tools.minimize_tokens` | Appends a conciseness rule to the system prompt. Toggle live with `/minimize` |

Truncated results keep the head and tail; the complete output is always in the
operation's `output_file`.

---

## Relationship with `~/.ahma/config.toml`

`~/.ahma/config.toml` is the **provider registry** file — it stores named LLM providers.  
`~/.ahma/settings.toml` is the **behaviour configuration** file — it stores runtime options.

Both files coexist independently.

| File | Purpose | Managed with |
|------|---------|-------------|
| `~/.ahma/config.toml` | LLM providers | `ahma llm add/remove` |
| `~/.ahma/settings.toml` | Runtime behaviour defaults | `ahma settings set <table>.<key> <value>` (or `ahma settings init` + a text editor) |

---

## Permissions

Everything ahma has been granted — filesystem scopes, web domains, per-workspace
tool approvals — lives in this one file, and is managed with one command:

```bash
ahma permissions list           # every grant, with where it came from
ahma permissions revoke ...     # previews the change; --yes applies it
```

## Project settings (`<workspace>/.ahma/settings.toml`)

A repository can carry its own settings file next to its tool definitions. It is
read whenever ahma finds that workspace's `.ahma` directory, and it overrides
`~/.ahma/settings.toml` per key — scalars replace scalars, and **lists replace
lists rather than concatenating**, so any effective value is attributable to
exactly one file.

**It may set preference-tier keys only.** This file travels with the repository,
so anyone who can send you a clone can propose values for it — and a cloned
repository must not be able to weaken the sandbox that is about to contain it.
Security-tier keys (everything in `[sandbox]`, `[network]`, `[web]`, `[auth]`, `[permissions]`, `[open_url]`, `[log_targets]`, plus `tools.long_running` and `http.unix_socket_path`) are refused there and reported by name at startup.
Set those in `~/.ahma/settings.toml` or on the command line, where they are yours.

`ahma settings show --origin` labels each key `cli`, `project (<path>)`,
`user (<path>)` or `default`, and lists whatever the project file asked for and
did not get:

```
# Project settings file: /path/to/repo/.ahma/settings.toml (1 preference key(s); …)
#   refused (security-tier, R-CFG2.2): auth.require_token, sandbox.disable
#   refused (unrecognised): tools.not_a_key
tools.timeout_secs                            = 4242  # project (/path/to/repo/.ahma/settings.toml)
sandbox.disable                               = false  # default (ignored; --no-sandbox is CLI-only)
```

`--no-settings` ignores **both** files for the invocation.

Three `[sandbox]` keys are worth knowing:

- **`profiles`** — the shipped toolchain carve-outs (`rust`, `node`, `go`,
  `python`, `ruby`, `android`, `apple`, `common`, `gh`, `sccache`). These used to be hard-coded in the sandbox backends, invisible and
  un-refusable; they are now data you can inspect and disable. Set to `[]` to
  enable none of them.
- **`package_cache_write`** (default `true`) — whether package-manager caches
  (the cargo registry and git caches, and their equivalents) are writable. Set it
  to `false`, or pass `--no-package-cache-write`, and they drop to read-only while
  the toolchain stays runnable.

  This is not a general hardening dial; it is the mitigation for one named risk.
  Those caches are shared by every project on the machine, so an agent working in
  one project can edit a cached crate's extracted source, and that code then runs
  — as a build script or proc macro — the next time you build an unrelated
  project. No sandbox rule is broken at any step. `ahma permissions list` states
  this cost beside the `rust` profile that creates it (SPEC R-HANDOFF.8).
- **`persistent_scopes`** — the directories you have granted, surviving every
  `roots/list` update. Each is bound to one `workspace` (the project it was
  granted for) and applies only to sessions working in that project (SPEC
  R5.4.11); an entry without a `workspace` is a legacy global grant that `ahma
  doctor` flags. Written by `ahma sandbox grant` or by an approved prompt, never
  by a sandboxed command (this file is outside every sandbox scope, by design —
  SPEC R5.4.8).

See [permissions.md](permissions.md) for the full model.

---

## Trust-handoff escape hatches

Some paths inside your workspace are writable by you but *executed by something
outside ahma's sandbox*. ahma denies writes to those by default (SPEC R-HANDOFF),
kernel-enforced on macOS. Two of them have real, legitimate uses, so each has a
narrow opt-in. Both are **off by default** — the opposite polarity from
`allow_keychain`, which is on by default.

| Key | Flag | Default | What turning it on permits |
|---|---|---|---|
| `allow_git_hooks` | `--allow-git-hooks` | `false` | Writes to `<git dir>/hooks/**`, for every *resolved* git directory (worktrees and `git init --separate-git-dir` included). |
| `allow_project_tool_config` | `--allow-project-tool-config` | `false` | Writes to `<workspace>/.ahma/**`, this project's MTDF tool definitions. |

```toml
[sandbox]
allow_git_hooks = true              # e.g. installing a repo's own pre-push guard
allow_project_tool_config = true    # e.g. developing the tool configs a repo ships
```

Read the consequence before you enable either:

- **`allow_git_hooks`** — a hook file is discovered by `git` *by convention* and
  runs with your full user privileges, outside the sandbox, on your next commit,
  checkout, push, or merge. Nothing prompts you at that point. Enable it for the
  session in which you are installing a hook you have read, not permanently.
- **`allow_project_tool_config`** — `.ahma/` defines the commands ahma will
  itself run, so a tool written under this setting can be invoked by the agent
  that wrote it. Tool configs still never hot-reload; an edit takes effect only
  through the explicit `restart` tool.

Either source is enough — the flag widens for one session, the settings key
widens permanently. There is no `--no-…` counterpart, because the default is
already "denied". Whenever one is on, ahma logs a warning at startup naming what
became writable and what executes it (SPEC R7: enforcement is never weakened
silently), and the write-denial error names the flag and the key so you never
have to go looking.

Both toggles remove the **kernel** rule as well as the write-tool check, so an
enabled hatch genuinely works rather than failing later with a bare
`Operation not permitted`. Turning one on is strictly narrower than
`--no-sandbox`, which is the outcome these hatches exist to prevent.

### Linux: prevent, not only detect (`linux_deny_tier`)

On Linux the kernel does not hold that deny list by default — Landlock cannot
subtract a path from a directory it allows — so a command run through
`run_terminal_command` can write `.git/hooks/pre-commit`, and ahma reports it as
`TRUST-HANDOFF WRITE` afterwards (SPEC R6.1.7). One key changes that where the
host allows it:

| Key | Values | Default | Effect |
|---|---|---|---|
| `linux_deny_tier` | `"detect"`, `"namespace"` | `"detect"` | `"namespace"` runs each command in its own user and mount namespace with every existing git hooks directory and the workspace's `.ahma/` mounted read-only, so a write to them fails with `Read-only file system`. Needs unprivileged user namespaces; where they are refused (stock Ubuntu 23.10+, Docker's default seccomp profile, an ahma nested inside another) ahma logs why at startup, shows it in `ahma status`, and keeps detecting. Detection stays on in both modes. Ignored off Linux. |

```toml
[sandbox]
linux_deny_tier = "namespace"
```

It is a `[sandbox]` key, so a repository's own `.ahma/settings.toml` cannot
turn it off (R-CFG2.2). What it needs and what it does not cover:
[security-sandbox.md](security-sandbox.md#prevented-where-the-host-allows-it-linux-opt-in).

### macOS: home reads by allowlist (`home_reads`)

| Key | Values | Default | Effect |
|---|---|---|---|
| `home_reads` | `"denylist"`, `"allowlist"` | `"denylist"` | `"allowlist"` makes file contents under your home unreadable, apart from the workspace, enabled profiles, your read-only grants, `~/.gitconfig`, `~/.config/git`, the ssh client files and the keychain (when `allow_keychain` is on). Metadata stays readable. A tool that reads elsewhere in home fails with `Operation not permitted` until you grant the path. Ignored off macOS. |

What it covers and what breaks:
[security-sandbox.md](security-sandbox.md#home-reads-by-allowlist-opt-in) (SPEC R-HARDEN.5).

### Linux: the system-call baseline (`syscall_filter`)

| Key | Values | Default | Effect |
|---|---|---|---|
| `syscall_filter` | `true`, `false` | `true` | Every command runs under a seccomp filter refusing `io_uring`, `bpf`, `perf_event_open`, `userfaultfd`, keyrings and `AF_ALG` sockets, and with `--restrict-network` UDP and raw sockets. `false` turns it off, for a tool that genuinely needs one (a profiler), and is logged at startup. Ignored off Linux. |

What each refusal returns: [security-sandbox.md](security-sandbox.md#linux-landlock)
(SPEC R-HARDEN.3).

## See also

- [environment-variables.md](environment-variables.md) — remaining env vars reference
- [llm-providers.md](llm-providers.md) — LLM provider configuration
- [security-sandbox.md](security-sandbox.md) — sandbox security details
- [live-log-monitoring.md](live-log-monitoring.md) — log monitor setup
