# Workspace write queue — safe async by default

**Status:** stable — introduced in the first release after v0.21.8 · SPEC [R2.7](../SPEC.md#r27-the-workspace-write-queue--safe-async)

## Why

Async execution lets a long command — `cargo nextest run`, a full build — overlap the
model's own thinking instead of stalling it. Without ordering it also lets two commands
change one workspace at the same time: a build racing a `sed -i`, a test run racing a
`git checkout`, a second `cargo` racing the first over `target/`. The result is a
workspace — and a test verdict — that matches neither the old code nor the new.

ahma's answer is to keep the overlap and remove the interleaving. The **model** thinks,
reads and plans concurrently. The **workspace** is written by one ahma command at a time,
in the order the model asked, no matter how many sessions (two Claude Code windows, the
TUI, a terminal hook) are working in it. That is why `tools.execution_mode = "async"` is
the default again.

## What the model sees

```text
run_terminal_command("cargo nextest run")   → AHMA ID: op_12 … (still running)
run_terminal_command("sed -i s/a/b/ lib.rs") → AHMA ID: op_13
                                               NOT started — queued behind op `op_12`
                                               (`cargo nextest run`, 41s). … Do not send it again.
run_terminal_command("git status")           → answers at once (read-only lane)
status()                                     → ── Finished since your last call ──
                                               cargo nextest run — exit 0 in 3m12s …
```

- **Writers run one at a time, in arrival order.** A command that may write the workspace
  waits for every earlier one. If its call returns before it starts, the result says
  `NOT started — queued behind …` and names who it waits for; it runs by itself.
- **Readers never wait.** `git status/diff/log/show`, `rg`, `grep`, `ls`, `cat`, `ps`, `sed -n`, `gh pr view`, `curl` and similar — and **pipelines and lists of them** (`grep … | head`, `cd src && ls`, `2>&1`, `>/dev/null`) —
  plain reads skip the queue — under a sandbox that grants the workspace **no write access**,
  so a misclassified command fails instead of writing.
- **Nothing is lost to a forgotten `await`.** A result the model never collected is put at
  the top of its next tool result, once.
- **Drift is reported.** Edits made by a harness's own editor never pass through ahma, so
  they cannot be queued. A command that ran for two seconds or more lists the files that
  changed in its workspace while it ran (`changed_during_run`), so a test verdict that
  raced an edit says so.
- **Edits wait for writers.** ahma's own file tools refuse an edit while a writer runs in
  that workspace, naming it; the opt-in edit guard does the same for the client's own file
  tools in Claude Code, Codex, Copilot CLI, Cursor, Antigravity and VS Code.

## Quickstart

Nothing to do: the queue is on by default. To see it, run a long command and then a
short writer in the same repository:

```bash
ahma tui          # watch the second command sit in the queue behind the first
```

The client's own file edits wait too, through the **edit guard** that `ahma hooks install`
writes alongside every terminal hook (decline it with `--no-edit-guard`). The same hook also
keeps those edits inside the sandbox scope — see [security-sandbox.md](security-sandbox.md):

```bash
ahma hooks install                                   # user scope, every supported client, shell hook + edit guard
ahma hooks install --platform codex --scope project
ahma hooks status                                    # "installed+guard" per client
ahma hooks uninstall                                 # removes shell hook and guard
```

It adds one pre-edit hook per client, under its own managed id:

| Client | Hook file (user / project) | Edit tools it guards |
|---|---|---|
| Claude Code | `~/.claude/settings.json` / `.claude/settings.json` | `Edit`, `Write`, `MultiEdit`, `NotebookEdit` |
| Codex | `~/.codex/hooks.json` / `.codex/hooks.json` | `apply_patch` (paths read from the patch) |
| GitHub Copilot CLI | `~/.copilot/hooks/ahma.json` / `.github/hooks/ahma.json` | `create`, `edit`, `str_replace_editor`, `apply_patch` |
| Cursor | `~/.cursor/hooks.json` / `.cursor/hooks.json` | `Write`, `Delete` |
| Antigravity | `~/.gemini/config/hooks.json` / `.agents/hooks.json` | `write_to_file`, `replace_file_content`, `multi_replace_file_content` |
| VS Code | — (no file of its own) | Its Claude, Codex and Copilot agents run those clients' hooks above; its *Local* agent reads the Copilot files and is recognised by its payload |

While the workspace is free the hook takes no position and the client decides as usual
(Cursor and Antigravity get the same plain `allow` ahma's shell hook already sends them).
While an ahma writer runs, it denies the edit with a reason naming that command. It never
waits, never approves an edit the client would otherwise question, and always exits 0 — a
missing or broken ahma cannot block editing.

## How it works

| Piece | What it does |
|---|---|
| **Workspace** | The repository containing the working directory — the nearest ancestor with `.git` (each git worktree is its own workspace, so worktrees are how you get parallel writers). Without one, the sandbox scope. Unrelated repositories never wait for each other. |
| **Lease** | An exclusive operation takes its workspace's lease before it spawns and holds it until its whole process tree exits. |
| **Order** | A place in line is taken the moment the call arrives. Within one ahma process the order is strict FIFO; between processes the kernel lock guarantees one writer at a time. |
| **Kernel lock** | `flock` / `LockFileEx` on a file in the per-user runtime directory (`$XDG_RUNTIME_DIR/ahma/locks`, else `~/.ahma/locks`; `%LOCALAPPDATA%\ahma\run\locks` on Windows). Released by the kernel when the holder exits for any reason — no stale locks, nothing to clean up, and the file is outside every workspace so no command can delete it while it is held. |
| **Nesting** | A command run under a lease carries `AHMA_HELD_WORKSPACE_LEASE`; an ahma it starts (ahma's own test suite, run through ahma) does not wait for the lease its ancestor holds. The per-user hub never inherits it, since it outlives that command. |
| **Timeouts** | A queued command's wait counts against its own timeout, and it is cancellable while queued. Once it starts, its duration and timeout measure the command, not the wait; the wait is stated in its result. |

### Lanes

| Lane | Takes the lease | Used for |
|---|---|---|
| `exclusive` (default) | yes | Anything that may write: builds, tests, formatters, `git commit`, any shell command line the classifier cannot prove is a plain read |
| `read_only` | no | Plain reads; spawned with the workspace read-only (Landlock on Linux, a write-free Seatbelt profile on macOS) and `GIT_OPTIONAL_LOCKS=0` |
| `service` | no | Long-lived processes such as `livelog` monitors or a declared dev server, which must not hold the workspace for their whole life |

The read-only lane exists only where the kernel can enforce it. On **Windows** (no
filesystem boundary yet, R6.3.3), in Test-mode sandboxes, and in a macOS ahma nested inside
another Seatbelt profile, every command is exclusive.

The bundled tools declare theirs: `file-tools` reads (`ls`, `cat`, `grep`, `find`, `head`,
`tail`, `diff`, `pwd`, `cd`), `git` `status` and `log`, and the `gh` list/view commands are
`read_only`; `gh run_watch`, which follows a CI run for minutes, is `service`. Everything
else — `sed`, `rm`, `git commit`, `gh run_download` — stays `exclusive`.

A custom tool declares its lane in MTDF. A declaration on the tool applies to every
subcommand; the nearest one wins:

```json
{ "name": "status", "description": "git status", "concurrency": "read_only" }
```

## Seeing who holds a workspace

```
ahma queue
```

lists every workspace lease right now: the holder's command, pid, age and whether that
process is still alive, with a first line that says whether anything is blocked at all. It
takes no lock and is never queued, so it works while every other command of yours is
waiting (SPEC R2.7.9). A dead holder is shown as dead: its OS lock went with it, and only
its record remains. To stop a live one, cancel it in the TUI or with the `cancel` tool of
the session that started it.

## Configuration

| `[tools]` key | Default | Effect |
|---|---|---|
| `execution_mode` | `"async"` | `"sync"` makes every call wait for its result |
| `workspace_queue` | `true` | `false` lets writers overlap again (the pre-R2.7 behaviour); only sensible with `execution_mode = "sync"` |
| `edit_guard` | `true` | ahma's own file tools refuse edits while a writer runs; also gates the edit guard installed by `ahma hooks install --edit-guard` |
| `mutex_groups` | `cargo` | Extra per-workspace serialisation for tools that contend on a shared directory, matched on the command line you wrote |

## Limits

- **Harness-native edits are held back only through the edit guard.** With
  `--no-edit-guard`, such an edit during a run is *reported* (`changed_during_run`), not
  prevented. Some Codex releases fire `PreToolUse` for `apply_patch` without enforcing its
  deny ([openai/codex#27833](https://github.com/openai/codex/issues/27833)); there the drift
  report is the remaining signal. Edits made through a shell command are ordered by the
  terminal hook and the queue, not by the guard.
- **Cross-process order is mutual exclusion, not FIFO.** Two sessions queued on the same
  workspace never overlap, but the kernel lock does not promise which goes first.
- **The drift report is by modification time**, skips `.gitignore`d paths and `.git/`,
  never walks outside the sandbox scope, names at most 20 files, and cannot tell who wrote a file — a writer that also edits
  sources (`cargo fmt`) is listed too.
- **A synchronous call waits at most its own timeout.** Terminal hooks, CLI one-shots and
  `synchronous: true` tools have no operation id to hand back while they wait, so they say
  once that they are waiting (a hook writes it to stderr, which the client shows) and, if the
  workspace stays busy for the whole timeout, fail with `Not run: …` naming the holder.
- **A long-running command in the exclusive lane holds the workspace** until it ends. Run
  servers and log followers through `livelog` (or declare them `service`), or `cancel` the
  holder the queued result names.

## See also

- [settings.md](settings.md#sync-or-async-toolsexecution_mode) — execution mode
- [security-sandbox.md](security-sandbox.md) — the kernel sandbox the read-only lane relies on
- [file-tools.md](file-tools.md) — ahma's own edit tools and the edit guard
- [installation.md](installation.md#terminal-hooks) — terminal hooks
- [custom-tools.md](custom-tools.md) — MTDF `concurrency`
- SPEC [R2.7](../SPEC.md#r27-the-workspace-write-queue--safe-async)
