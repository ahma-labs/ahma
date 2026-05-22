# TUI Control Plane

> **Experimental** — introduced in v0.7. The full ratatui rendering layer is planned; the current release ships a text-mode fallback.

`ahma tui` opens a terminal dashboard for monitoring and controlling active tasks. It works over SSH, requires no graphical runtime, and is the primary interface for reviewing approval gates raised by the [renewal contract](renewal-contract.md).

## Quickstart

```bash
# Connect to the default ahma HTTP bridge on localhost:3000
ahma tui

# Connect to a custom address
ahma tui --connect http://localhost:8080
```

The TUI polls the server every two seconds and prints a live status table until you press Ctrl-C.

## Panels (planned full TUI)

```
┌──────────────────────────────────────────────────────┐
│ AHMA  Task Control Plane     server: HEALTHY  [q] quit │
├───────────────────────┬──────────────────────────────┤
│ Active Tasks          │ Task Detail                   │
│ ► op_001 [Running]   │ tool: cargo_build             │
│   op_002 [Pending]   │ status: InProgress            │
│                       │ elapsed: 12s                  │
├───────────────────────┴──────────────────────────────┤
│ Recent log                                            │
│ 12:01:03  INFO  sandbox configured                    │
│ 12:01:04  INFO  cargo_build started                   │
└──────────────────────────────────────────────────────┘
│ APPROVAL REQUIRED  op_003: renewal checkpoint        │
│  [y] approve  [n] reject                             │
└──────────────────────────────────────────────────────┘
```

## Key bindings

| Key | Action |
|-----|--------|
| `q` | Quit |
| `↑` / `↓` | Navigate task list |
| `Enter` | Show task detail |
| `y` | Approve pending gate |
| `n` | Reject pending gate |

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

## See also

- [docs/renewal-contract.md](renewal-contract.md) — task renewal and approval gates
- [docs/task-vault.md](task-vault.md) — vaults and audit logs
- [docs/connection-modes.md](connection-modes.md) — HTTP bridge setup
