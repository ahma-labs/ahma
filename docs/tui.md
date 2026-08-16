# TUI Control Plane

`ahma tui` opens a terminal dashboard for watching and controlling everything ahma is doing on your behalf — the operations your MCP client (Claude Code, Cursor, Antigravity, …) is running, and the commands you run yourself. It works over SSH, requires no graphical runtime, and is the primary interface for reviewing approval gates.

It is **chat-first**: the default view is a chat/agent interface with operation cards. Everything else is a toggleable sub-window stacked above the chat: `/tasks` opens the live **task tree**, `/log` the log pane, and `/scope` the **sandbox scope panel**. Running the same command again (with focus on that pane, for the focusable ones) closes it.

## Quickstart

```bash
# In your project root — attaches to everything already running for this project
cd ~/my-project
ahma tui

# Connect to a custom address
ahma tui --connect http://localhost:8080
```

## The live task tree — current at startup

Open `ahma tui` in a project directory while your IDE agent is working and the ongoing tasks are **already there** (SPEC R24): every ahma instance reports its operations to a per-user hub daemon, which replays recent history (with true start/end timestamps) to the TUI the moment it subscribes. If live project work is found at startup, the TUI opens straight into the task view; press any key to take over.

```
┌ Tasks · this project — [f] all ────────────────────────────┐
│ ▾ claude-code · stdio · …/github/ahma      2⟳ 1◷ 14✓       │
│    ⟳ cargo nextest run          [op_41]  1m12s   [P] [X]   │
│    │ Compiling ahma_core v0.15.4                            │
│    │ Compiling ahma_mcp v0.15.4                             │
│    ▾ session build-loop                                     │
│      ✓ cargo fmt --all          [op_39]  0.3s               │
│      ⟳ cargo clippy             [op_40]  12s     [P] [X]   │
│ ▸ cursor · stdio · …/github/ahma           3✓               │
│ ▾ this terminal (you)                      1⟳               │
│    ⟳ tail -f logs/ahma.log      [op_7]   4m02s   [P] [X]   │
└─────────────────────────────────────────────────────────────┘
```

- **One line per task.** Instance headers show *who* is driving (the MCP client identity from the `initialize` handshake), the transport, the sandbox scope, and at-a-glance tallies of how much is running / queued / done / failed in parallel.
- **Children indent under what spawned them** — persistent-session commands under their session, subtasks under their parent operation, to any depth.
- **Tasks resolve in place** when they finish: the spinner becomes ✓/✗ with the duration.
- **Accordion drill-in:** `Space` (or click) on a task expands it inline into its live output tail — or its historic output/result if already finished — and collapses whichever task was expanded before; on an instance or session header it folds that subtree. `Enter` opens the full-screen operation detail overlay instead.
- **Project-scoped by default:** only instances whose sandbox scope covers the directory you started in are shown; `f` shows all projects.
- Finished tasks stay visible for an hour, so a TUI opened mid-session shows what *was* done, not just what is running.

## Operation names and exit status

Every operation row shows **what actually ran**, not an internal id:

```
⟳ cargo nextest run -p ahma_core   [op_41]  exit 0 · 41s
✗ touch /etc/foo                   [op_42]  denied: outside sandbox scope
```

The name is computed **by the server**, where the command is known, and sent on
the wire (SPEC R24.7) — the TUI renders it rather than guessing. That is why rows
are meaningful even when you open `ahma tui` *after* your IDE has already been
working: the replayed history carries the same names and exit codes as the live
events, so a late-attached TUI shows what those commands were, not what their ids
looked like.

A finished row shows `exit 0` / `exit 101` where the command was a process. An
operation that was cancelled or timed out has no exit code, and says so, rather
than showing a fabricated `exit 0`.

## Key bindings

| Key | Action |
|-----|--------|
| Ctrl-C (`q` in a pane) | Quit |
| `↑`/`↓` (`j`/`k`) | Navigate rows |
| `Space` / click | Expand task into live/historic output (accordion); fold headers |
| `Enter` | Open the full-screen operation detail overlay |
| `f` | Toggle this-project / all-projects |
| `c` | Cancel selected operation |
| `p` | Pin selected operation |
| `a` | Ask for access again (on a denied operation) |
| `Tab` | Cycle panes |
| `y` / `n` | Approve / reject pending gate |
| `/` | Command navigator (from empty input) |
| `?` | Help (from empty input, or any pane) |

## Chat input prefixes

| Prefix | What it does |
|---|---|
| `/` | Command navigator (from an empty input) |
| `#` | Ask the LLM to decompose a goal into steps |
| `!` | **Run a command completely outside the sandbox**, at your full user privilege |

`!` is the deliberate escape hatch, and it is deliberately narrow: it fires only
from a keystroke you type, never from an LLM turn or a tool call. Every use is
disclosed twice — a warning in the log pane naming the command, and an
`UNSANDBOXED` label on the output window. It exists because the alternative is
worse: a user who cannot run one unconfined command from inside ahma runs it in
another terminal, where ahma can neither disclose nor record it.

## The sandbox scope panel

`/scope` opens a panel showing the sandbox **as the server actually locked it** — not what the TUI was launched with:

```
┌ Sandbox · scope ──────────────────────────── /scope closes ┐
│ ENFORCED · authority: ahma (kernel)                         │
│ write : ~/github/ahma                                       │
│ read  : (none beyond write roots)                           │
│ tmp   : OFF · source: roots/list                            │
└─────────────────────────────────────────────────────────────┘
```

- **`authority`** states who is protecting the session (SPEC R7.5): ahma's own kernel sandbox, ahma nested inside a detected host sandbox (Cursor, Docker, …), deferred to the host (ahma not enforcing), or none at all (`--no-sandbox`).
- **`source`** is the scope's provenance: `explicit` (a flag or settings), `roots/list` (your IDE's workspace), `elicited` (you answered a prompt), or `container` (the configured container root).
- **`grants`** lists the persistent scopes you granted yourself (`[sandbox] persistent_scopes`), so a writable path outside your workspace is explicable — and names `ahma sandbox list` / `ahma sandbox revoke <path>` to review or remove them.
- Platform limitations that cannot be expressed as scope — e.g. macOS scoping writes but not reads — are shown here too.
- If sandbox configuration **failed**, the panel shows the reason and the fix instead of a bare red chip.

The header's `sandbox:` label follows the same honesty rule: it shows the server-locked primary write root once reported. Until then it shows the launch directory with a trailing `?` — a guess, not the boundary.

## Approval gates

The TUI is the primary surface for approving or rejecting actions that require user sign-off:

- **Tool approval** — a chat tool call awaiting your `y`/`n` in the banner above the input.
- **Sandbox scope grants** — a sandboxed command was blocked on an out-of-scope path and the "grant access?" modal asks whether to record a persistent grant (Enter/Esc always deny; grants land in `~/.ahma/settings.toml` and apply on the next server start — ask the agent to run the `restart` tool to apply one immediately).
- **Web egress** — a blocked outbound domain raises the "allow egress?" modal, with three tiers: `[o]` once, `[s]` this session, `[a]` always. Cleartext `http://` is flagged rather than blocked.

When a command is blocked by the sandbox, the operation row says so — `denied: /etc (rw) · [a] ask` — rather than a bare "failed". Pressing `a` on it re-raises the grant question for exactly that path, even if you declined it earlier in the session: that suppression exists so ahma does not nag you, and deliberately choosing the row is not ahma nagging.

Persistent grants and revocations are appended to `~/.ahma/permissions-audit.jsonl`; see [docs/permissions.md](permissions.md).

## Text-mode fallback

When the full ratatui interface is not compiled in, `ahma tui` runs a simple polling loop that prints one status line per server check:

```
Ahma TUI (text mode)
Connecting to: http://localhost:3000
[12:01:00] Server http://localhost:3000 — HEALTHY
[12:01:02] Server http://localhost:3000 — HEALTHY
```

Press Ctrl-C to exit.

## Transport auto-detection

`ahma tui` automatically picks the best available transport in order:

1. **Unix socket** (`/tmp/ahma.sock`, or `[http] unix_socket_path` in `~/.ahma/settings.toml`) — lowest latency, local only. `$AHMA_UNIX_SOCKET` is retired (R-CFG1.2) and ignored by the TUI as it is by `ahma serve`.
2. **HTTP/3 (QUIC)** — when the server advertises `Alt-Svc: h3=…` _and_ local TLS material exists at `~/.ahma/tls/`. See [TLS provisioning](#tls-provisioning-for-quic) below.
3. **HTTP/1.1 / HTTP/2** — plain TCP, always available as a fallback.

The transport in use is shown in the TUI header (e.g. `transport: HTTP/3 (QUIC)` or `transport: Unix socket`).

Use `--connect` to bypass detection and force a specific endpoint:

```bash
# Force a specific HTTP address (skips Unix socket probe)
ahma tui --connect http://localhost:8080

# Force the Unix socket path
ahma tui --connect unix:///run/ahma/mcp.sock
```

Start the server with `--disable-quic` (or set `[http] disable_quic = true` in `~/.ahma/settings.toml`) to prevent the HTTP/3 upgrade even when it would otherwise be advertised. `AHMA_DISABLE_QUIC` is retired and ignored.

## TLS provisioning for QUIC

HTTP/3 transport requires TLS. `ahma` manages a persistent self-signed certificate at `~/.ahma/tls/` (override with the `--tls-dir` flag; `AHMA_TLS_DIR` is retired and ignored):

```bash
# Generate certificate on first use (safe to re-run — idempotent)
ahma tls init

# Replace the certificate (e.g. after 30 days or key compromise)
ahma tls rotate

# Check certificate age and rotation status
ahma tls status
```

The `ahma tls init` step is offered automatically during `install.sh`. The private key is stored with 0600 permissions.

## See also

- [docs/permissions.md](permissions.md) — grants, the question ladder, and the permissions ledger
- [docs/connection-modes.md](connection-modes.md) — HTTP bridge setup
