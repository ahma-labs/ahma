# TUI Control Plane

> **Experimental** — introduced in v0.7. The full `ratatui`-based rendering layer is implemented as the default user interface, with a text-mode fallback available if the library features are omitted at compile-time.

`ahma tui` opens a terminal dashboard for monitoring and controlling active tasks. It works over SSH, requires no graphical runtime, and is the primary interface for reviewing approval gates raised by the [renewal contract](renewal-contract.md).

## Quickstart

```bash
# Connect to the default ahma HTTP bridge on localhost:3000
ahma tui

# Connect to a custom address
ahma tui --connect http://localhost:8080
```

The TUI polls the server every two seconds and renders the live dashboard interface until you press Ctrl-C.

## Panels

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
