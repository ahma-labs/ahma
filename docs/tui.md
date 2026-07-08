# TUI Control Plane

`ahma tui` opens a terminal dashboard for watching and controlling everything ahma is doing on your behalf — the operations your MCP client (Claude Code, Cursor, Antigravity, …) is running, and the commands you run yourself. It works over SSH, requires no graphical runtime, and is the primary interface for reviewing approval gates.

It is **chat-first**: the default view is a chat/agent interface with operation cards; `/mode monitor` switches to the monitor dashboard with the live **task tree**. Switch back with `/mode chat`.

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
- **Accordion drill-in:** `Enter` (or click) on a task expands it inline into its live output tail — or its historic output/result if already finished — and collapses whichever task was expanded before. `Enter` on an instance or session header folds that subtree.
- **Project-scoped by default:** only instances whose sandbox scope covers the directory you started in are shown; `f` shows all projects.
- Finished tasks stay visible for an hour, so a TUI opened mid-session shows what *was* done, not just what is running.

## Key bindings

| Key | Action |
|-----|--------|
| `q` / Ctrl-C | Quit |
| `↑`/`↓` (`j`/`k`) | Navigate rows |
| `Enter` / click | Expand task into live/historic output (accordion); fold headers |
| `f` | Toggle this-project / all-projects |
| `c` | Cancel selected operation |
| `p` | Pin selected operation |
| `a` | Await selected operation |
| `Tab` | Cycle panes |
| `y` / `n` | Approve / reject pending gate |
| `/` | Command navigator |
| `?` | Help |

## Approval gates

The TUI is the primary surface for approving or rejecting operations that require user sign-off:

- **Renewal checkpoints** — a task that has run unattended beyond `T_renew` seconds halts and requires re-approval before continuing. See [docs/renewal-contract.md](renewal-contract.md).
- **Elevation requests** — a task requesting write access outside its vault scope.
- **Trash purge confirmation** — permanently deleting staged files.

Approval decisions are recorded in the vault's `audit.jsonl`.

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

1. **Unix socket** (`/tmp/ahma.sock` or `$AHMA_UNIX_SOCKET`) — lowest latency, local only.
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

Set `AHMA_DISABLE_QUIC=1` to prevent the HTTP/3 upgrade even when the server advertises it.

## TLS provisioning for QUIC

HTTP/3 transport requires TLS. `ahma` manages a persistent self-signed certificate at `~/.ahma/tls/` (override with `$AHMA_TLS_DIR`):

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

- [docs/renewal-contract.md](renewal-contract.md) — task renewal and approval gates
- [docs/task-vault.md](task-vault.md) — vaults and audit logs
- [docs/connection-modes.md](connection-modes.md) — HTTP bridge setup
