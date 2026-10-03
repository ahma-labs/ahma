# The ahma hub

> **Status:** stable. Called the *daemon* in 0.21 and earlier; `[daemon]` settings are
> still read.

There is **one ahma hub per user**, and it hosts both halves of ahma's
background presence: the MCP endpoint your editors connect to, and the
event stream that `ahma tui` watches. Whoever needs it first starts it —
an editor's `ahma serve stdio`, `ahma tui`, or a hooked command — and everyone
else attaches to the same one.

## Why one

Three Claude Code windows, a Cursor window and a TUI used to produce two
independent singletons with two rendezvous points and two lifetimes: an MCP
bridge on the machine-global `/tmp/ahma.sock`, and a hub that `ahma tui` would
*become* if it happened to start first. That had consequences nobody chose:

- Quitting the terminal window running the TUI took the event stream away from
  every attached editor.
- A TUI started in one directory made that directory the sandbox scope for
  every editor session that attached afterwards.
- The first client to start the bridge configured the rest: a second window's
  `--tools` was ignored, and a first window's `--no-sandbox` unsandboxed
  everybody.
- Hooked shell commands reported to neither and were invisible everywhere.

One hub fixes those by construction, and the trade is deliberate: several
clients share one process. The **sandbox** is not shared — see below.

## What it does and does not do

The hub is a control plane. It runs no commands itself. Every tool call runs
in a kernel-sandboxed worker subprocess, **one per MCP session**, each locking
its own scope from its own client's `roots/list` (SPEC R5.1). That is not a
design preference: on Linux a Landlock ruleset restricts the process that
applies it, irreversibly, so one process cannot hold two workspace scopes.

```
editor 1 ─┐
editor 2 ─┼─ ahma serve stdio (a pipe) ─┐
editor 3 ─┘                             │
ahma tui ───────────────────────────────┤  hub.sock
hooked command ─────────────────────────┘
                                        ▼
                            ┌───────────────────────┐
                            │       ahma hub        │
                            │ events + MCP endpoint │
                            └───┬────┬────┬────┬────┘
                                ▼    ▼    ▼    ▼      one per session,
                               W1   W2   W3   W4      one locked scope each
```

## Where it lives

A per-user runtime directory — `$XDG_RUNTIME_DIR/ahma`, else `~/.ahma`;
`%LOCALAPPDATA%\ahma\run` on Windows — created `0700` and refused if it is
owned by someone else or reachable by group or others. It holds:

| File | What it is |
|---|---|
| `hub.lock` | The mutex: whoever holds it is the hub. A kernel lock, so it is released the moment its holder dies, and nothing is left to clean up after a crash. |
| `hub.sock` | The one socket: the MCP endpoint editors proxy to, and the event stream (instances, operations and approvals). Only the lock holder binds or removes it. `0600`. |
| `history.jsonl` | The last hour of operations, so recent work survives a restart. `0600`. |

Both halves share the socket. `/mcp` and `/health` are ordinary HTTP; the
event stream is an HTTP upgrade (`GET /events` with `Upgrade: ahma-hub`), after
which the connection carries newline-delimited JSON. So there is one path to
configure and one file to secure. An `ahma serve unix` you start yourself
defaults to `mcp.sock` beside it, never onto it.

You can put the socket somewhere else with `--unix-socket-path` (or `[http]
unix_socket_path`), and ahma will honour it — every process that looks for
the hub reads the same setting, and the lock goes beside it. It checks ownership and mode only
on the directory it picked itself — the `0700` one above. A directory you named
is your decision; if it is writable by other users and has no sticky bit to stop
them unlinking your socket, ahma says so at startup and carries on.

The old machine-global `/tmp/ahma.sock` is retired: every local user could see
it, and since nothing owned the path, pre-create it. A `0600` socket inside a
world-writable directory is still squattable, which is why the directory is
checked and not only the socket.

On **Windows** (10 1803 or later) it is the same kind of `AF_UNIX` socket
file, with the same lock; access control comes from the per-user profile ACL
rather than mode bits. The hub opens no TCP port on any OS. Tokio cannot drive
`AF_UNIX` sockets on Windows, so each connection is served by two threads; closing a
connection still closes its socket at once, after the last reply and its end-of-stream
have been sent, even when the other side never answers (SPEC R-HUB.2).

## Lifetime

It exits when nothing has been attached for a while — no MCP sessions **and** no
TUI or other hub subscribers:

```toml
[hub]
# Seconds with nothing attached before the hub exits. 0 keeps it running.
idle_timeout_secs = 3600
```

| Setting | Default | What it does |
|---|---|---|
| `[hub] idle_timeout_secs` | `3600` | Seconds with no MCP sessions and no TUI before the hub exits; `0` never. |
| `[hub] drain_timeout_secs` | `3600` | Longest a hub replaced by a newer install waits for running work before ending it and handing over; `0` waits as long as the work takes. |
| `--unix-socket-path` / `[http] unix_socket_path` | `hub.sock` in the per-user runtime dir | Where the hub's one socket lives. |

Counting only sessions would exit while a TUI sat watching an idle project;
counting only subscribers would exit mid-build. Restarting is cheap and the
hub holds nothing you depend on — history is on disk.

The hour is wall-clock time, so a laptop that sleeps overnight does not wake
up to a hub that thinks it has only just gone idle. And if the hub's socket is
removed — `$XDG_RUNTIME_DIR` cleared at logout, say — the hub can no longer be
reached, so it hands the rendezvous over to whatever starts next, finishes
what it has, and exits rather than lingering beside its replacement.

Your editor never has to notice. The `ahma serve stdio` process it talks to
stays attached however the hub goes away (idle exit, crash, kill, upgrade):
the next request restarts it and resumes the session, and requests made while
it is unreachable are answered with "ahma is restarting, retry" rather than the
server going dead. If the hub cannot be started at all — a host sandbox that
forbids the detached spawn, say — the session runs in-process instead, and the
log says so. Operation ids name the process that issued them, so `await` on an
id from before a restart says what happened instead of just "not found"
(SPEC R-LIFECYCLE.3, R-LIFECYCLE.4).

## Upgrades

When you install a new ahma while one is running, the old hub **drains**: it
hands over to the new build without ending anyone's work. It notices the
install itself — it checks its own executable every 30 seconds and whenever a
session starts — and a newer client that connects asks it to as well. Only a
strictly newer build does that (a newer version, or the same version built
later), so two installed copies of ahma never take turns replacing each
other. `/health` says which binary, and which file, the hub is running.

- The old hub **keeps serving** meanwhile, new sessions included — opening
  another editor window during a drain works as usual.
- It starts the new build straight away, waiting in the wings for the
  rendezvous.
- It hands over the moment nothing is running: no command in any session and
  no request waiting for an answer. Your editors reconnect to the new hub on
  their next request, without you doing anything.
- Work that never goes quiet is ended after `[hub] drain_timeout_secs` (an
  hour by default), so an old build cannot run forever; the requests it
  interrupts get an error saying why.

When your editor reconnects to the new build it is told the tool list may
have changed, so it picks up new or changed tools without a restart. And if
you `await` an operation id from before the handover, the answer says how that
operation ended — from the hub's history — and when and why ahma restarted,
rather than just "not found".

Your other editors' sessions are not torn down mid-command to install a
binary one of them asked for. Until the handover happens, the mismatch is
disclosed rather than hidden. A TUI watching the old hub does not hold it
open: it reconnects to the new one on its own.

`ahma hub` in a terminal runs one in the foreground, which is the way to see
what it is doing.

## The TUI's own commands

`ahma tui` is a subscriber, but it is also a place work happens: a `!` command
typed into its input runs right there, outside the sandbox, at your full
privilege. So the TUI registers a connection of its own (`mode: "tui"`) and
reports those commands like any other client — which is what puts them in the
history file and in front of a second TUI. They are flagged `unsandboxed` on
the wire and every surface says so. If the hub is down the command still
runs and still shows its output; only the report is lost.

## Hooked commands

A command wrapped by ahma's shell hook registers as an instance of its own for
the length of that command, so hooked work appears in the TUI beside everything
else. It never starts a hub (that would put a process launch in front of your
command) and waits at most 300 ms for its report to land before exiting. With no
hub running, the command runs exactly as it otherwise would.

## See also

- [docs/tui.md](tui.md) — the work view that watches all of this
- [docs/connection-modes.md](connection-modes.md) — how editors connect
- [docs/session-isolation.md](session-isolation.md) — per-session workers and scopes
- SPEC.md `R-HUB` — the requirements this implements. R-HUB.12: the hub and its workers start in the runtime directory, so a worker's tools dir, logs and audit follow its own committed scope rather than whichever checkout the hub was first launched from.
