# Ahma Settings File (`~/.ahma/settings.toml`)

The settings file is the primary way to configure Ahma's behaviour.  
`AHMA_*` environment variables are **not** a configuration source: they are retired and
ignored with a warning ([environment-variables.md](environment-variables.md)).

## Getting started

```bash
ahma settings init        # create with all defaults commented out
ahma settings show        # print effective configuration (with source annotations)
ahma settings show --origin  # print exact per-key provenance (cli / user / default + file path)
ahma --no-settings serve stdio  # ignore settings file for one invocation
```

`--origin` reports the *true* source of each setting: `cli` when a flag passed in
the same invocation overrides the key (e.g. `ahma --timeout 30 settings show --origin`),
`user (<path>)` when the settings file explicitly sets the key — even if it sets it
to the default value — and `default` otherwise. Both `--settings-path` and
`--no-settings` are honored.

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

1. **CLI flags** (`--timeout 600`, `--tmp`, …)
2. **`<workspace>/.ahma/settings.toml`** — preference keys only ([below](#project-settings-workspaceahmasettingstoml))
3. **`~/.ahma/settings.toml`**
4. **Compiled-in defaults**

Two switches are CLI-flag-only and cannot be set from any file, so they are always
visible where ahma is launched: `--no-sandbox` and `--insecure-skip-verify` (SPEC R-CFG2.3).

---

## Full schema

All options are commented out by default.  
Run `ahma settings init` to generate this file automatically.

```toml
# ~/.ahma/settings.toml — Ahma user settings
#
# All options are commented out.  Uncomment and edit any value to override
# the compiled-in default.  CLI flags always take highest priority, followed
# by this file, followed by deprecated AHMA_* environment variables.

# ── LM Studio (local OpenAI-compatible server) ──────────────────────────────
# Start the LM Studio Local Server (Developer tab), or headless: lms server start
#
# [lmstudio]
# base_url = "http://localhost:1234/v1"  # default: 1234 (LM Studio local server)
# model    = "openai/gpt-oss-20b"        # set to the model loaded in LM Studio

# ── Tool execution ───────────────────────────────────────────────────────────
# [tools]
# timeout_secs = 1800     # default tool timeout (seconds; 30 minutes)
# await_timeout_secs = 1800 # default `await` soft timeout (seconds); does not cancel the operation
# idle_timeout_secs = 1800  # tool execution idle timeout (seconds without output); 0 disables
# request_budget_override_secs = 0 # override the fallback single-request budget (SPEC R2.6.5); 0 = unset, use the built-in default
# force_progress_notifications = false # send progress to Cursor despite its client-side logging quirk
# execution_mode = "async" # "async": return an operation id after a short window,
#                          # collect with `await`; "sync": wait for each command's
#                          # result (within what the client can wait for). See below.
# workspace_queue = true   # writers run one at a time per workspace, in arrival order
# edit_guard = true        # refuse ahma's own file edits while a writer runs
# skip_probes  = false    # skip availability probes at startup
# minimize_tokens     = false # `ahma tui` chat: ask the model for terse answers
# small_model_harness = false # `ahma tui` chat: tighter result/conversation budgets

# ── Sandbox & filesystem security ────────────────────────────────────────────
# [sandbox]
# tmp_access   = false    # add system temp dir to sandbox scope
# disable_temp = false    # block all access to system temp dir (overrides tmp_access)
# defer        = false    # defer sandbox lock until client provides roots/list
# allow_git_hooks = false           # let tools write <git dir>/hooks/** (default: denied)
# allow_project_tool_config = false # let tools write this workspace's .ahma/ (default: denied)

# ── Logging ──────────────────────────────────────────────────────────────────
# [logging]
# target                  = "file"   # "file" (rolling) or "stderr"
# log_monitor             = false    # enable live log monitoring via LLM
# monitor_rate_limit_secs = 60       # min seconds between log-monitor alerts
# dir                     = ""       # log directory; "" resolves automatically

# ── HTTP server (ahma serve http only) ───────────────────────────────────────
# [http]
# handshake_timeout_secs = 45      # MCP handshake timeout
# disable_quic           = false   # disable HTTP/3 QUIC; fall back to HTTP/2 TCP
# disable_http1_1        = false   # reject HTTP/1.1; require HTTP/2+

# ── HTTP authentication & rate limiting ──────────────────────────────────────
# [auth]
# require_token_path = ""   # path to file containing required bearer token
# rate_limit_rps     = 0    # max requests/second (0 = no limit)
# rate_limit_burst   = 10   # burst allowance

# ── Instance identity ────────────────────────────────────────────────────────
# [instance]
# label = "ahma"   # instance name shown in TUI and hub event stream

# [hub]
# idle_timeout_secs = 3600 # seconds with nothing attached — no MCP sessions and
#                          # no TUI — before the per-user hub exits. 0 keeps
#                          # it running. `[daemon]` (its old name) is still
#                          # read. See docs/hub.md.
# drain_timeout_secs = 3600 # longest a hub replaced by a newer install waits
#                           # for running work before ending it and handing
#                           # over. 0 waits as long as the work takes.
```

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

| Key (`[tools]`) | Default | Effect |
|---|---|---|
| `workspace_queue` | `true` | Writers run one at a time per workspace, in arrival order (SPEC R2.7). Off restores unordered async — only sensible with `execution_mode = "sync"`. |
| `edit_guard` | `true` | ahma's own `write_file`/`replace_in_file`/`multi_edit`/`apply_patch` refuse an edit while a writer runs in that workspace (SPEC R2.7.8). |

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
| `~/.ahma/settings.toml` | Runtime behaviour defaults | `ahma settings init` + text editor |

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
Everything in `[sandbox]`, `[auth]`, `[web]`, `[network]` and `[permissions]`,
plus `http.unix_socket_path`, is refused there and reported by name at startup.
Set those in `~/.ahma/settings.toml` or on the command line, where they are yours.

`ahma settings show --origin` labels each key `cli`, `project (<path>)`,
`user (<path>)` or `default`, and lists whatever the project file asked for and
did not get:

```
# Project settings file: /path/to/repo/.ahma/settings.toml (1 preference key(s); …)
#   refused (security-tier, R-CFG2.2): auth.require_token, sandbox.disable
#   refused (unrecognised): tools.not_a_key
tools.timeout_secs                            = 4242  # project (/path/to/repo/.ahma/settings.toml)
sandbox.disable                               = false  # default
```

`--no-settings` ignores **both** files for the invocation.

Three `[sandbox]` keys are worth knowing:

- **`profiles`** — the shipped toolchain carve-outs (`rust`, `node`, `go`,
  `common`). These used to be hard-coded in the sandbox backends, invisible and
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

On macOS a sandboxed command may signal (`kill`) only the process tree it started
itself; `signal_other_processes = true` lets it signal any of your processes, which is
how one agent stopped another session's build — turn it on only for a workflow that
genuinely has to stop a pre-existing server (SPEC R6.2.6).

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
`[sandbox] disable = true`, which is the outcome these hatches exist to prevent.

## See also

- [environment-variables.md](environment-variables.md) — remaining env vars reference
- [llm-providers.md](llm-providers.md) — LLM provider configuration
- [security-sandbox.md](security-sandbox.md) — sandbox security details
- [live-log-monitoring.md](live-log-monitoring.md) — log monitor setup
