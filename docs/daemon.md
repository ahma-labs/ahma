# The ahma daemon

There is **one ahma daemon per user**, and it hosts both halves of ahma's
background presence: the MCP endpoint your editors connect to, and the
observability hub that `ahma tui` watches. Whoever needs it first starts it —
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

One daemon fixes those by construction, and the trade is deliberate: several
clients share one process. The **sandbox** is not shared — see below.

## What it does and does not do

The daemon is a control plane. It runs no commands itself. Every tool call runs
in a kernel-sandboxed worker subprocess, **one per MCP session**, each locking
its own scope from its own client's `roots/list` (SPEC R5.1). That is not a
design preference: on Linux a Landlock ruleset restricts the process that
applies it, irreversibly, so one process cannot hold two workspace scopes.

```
editor 1 ─┐
editor 2 ─┼─ ahma serve stdio (a pipe) ─┐
editor 3 ─┘                             │  mcp.sock
ahma tui ───────────────────────────────┤  daemon.sock
hooked command ─────────────────────────┘
                                        ▼
                            ┌───────────────────────┐
                            │   ahma daemon         │
                            │   hub + MCP endpoint  │
                            └───┬────┬────┬────┬────┘
                                ▼    ▼    ▼    ▼      one per session,
                               W1   W2   W3   W4      one locked scope each
```

## Where it lives

A per-user runtime directory — `$XDG_RUNTIME_DIR/ahma`, else `~/.ahma` —
created `0700` and refused if it is owned by someone else or reachable by group
or others. It holds:

| File | What it is |
|---|---|
| `daemon.sock` | The hub. Binding it is the mutex: whoever binds it is the daemon. `0600`. |
| `mcp.sock` | The MCP endpoint editors proxy to. `0600`. |
| `history.jsonl` | The last hour of operations, so recent work survives a restart. `0600`. |

The old machine-global `/tmp/ahma.sock` is retired: every local user could see
it, and since nothing owned the path, pre-create it. A `0600` socket inside a
world-writable directory is still squattable, which is why the directory is
checked and not only the socket.

On **Windows** there are no filesystem sockets, so `daemon.lock` (a kernel
advisory lock, released automatically when its holder dies) is the mutex, both
listeners bind ephemeral ports, and `daemon.json` publishes them with a random
bearer token that stands in for the mode bits.

## Lifetime

It exits when nothing has been attached for a while — no MCP sessions **and** no
TUI or other hub subscribers:

```toml
[daemon]
# Seconds with nothing attached before the daemon exits. 0 keeps it running.
idle_timeout_secs = 60
```

Counting only sessions would exit while a TUI sat watching an idle project;
counting only subscribers would exit mid-build. Restarting is cheap and the
daemon holds nothing you depend on — history is on disk.

## Upgrades

When you install a new ahma while one is running, the new binary asks the old
daemon to **drain**: stop accepting new sessions, finish the ones it has, then
exit, at which point the next client starts the new one. Your other editors'
sessions are not torn down mid-command to install a binary one of them asked
for. Until the handover happens, the mismatch is disclosed rather than hidden.

`ahma daemon` in a terminal runs one in the foreground, which is the way to see
what it is doing.

## The TUI's own commands

`ahma tui` is a subscriber, but it is also a place work happens: a `!` command
typed into its input runs right there, outside the sandbox, at your full
privilege. So the TUI registers a connection of its own (`mode: "tui"`) and
reports those commands like any other client — which is what puts them in the
history file and in front of a second TUI. They are flagged `unsandboxed` on
the wire and every surface says so. If the daemon is down the command still
runs and still shows its output; only the report is lost.

## Hooked commands

A command wrapped by ahma's shell hook registers as an instance of its own for
the length of that command, so hooked work appears in the TUI beside everything
else. It never starts a daemon (that would put a process launch in front of your
command) and waits at most 300 ms for its report to land before exiting. With no
daemon running, the command runs exactly as it otherwise would.

## See also

- [docs/tui.md](tui.md) — the work view that watches all of this
- [docs/connection-modes.md](connection-modes.md) — how editors connect
- [docs/session-isolation.md](session-isolation.md) — per-session workers and scopes
- SPEC.md `R-DAEMON` — the requirements this implements
