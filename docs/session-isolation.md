# Session Isolation

Session isolation is how several clients share one ahma without sharing a
sandbox. Each session runs in its own subprocess with its own kernel-enforced
boundary and its own lifecycle — in the per-user hub
([docs/hub.md](hub.md)) as much as in an explicitly started
`ahma serve http`.

## Why Session Isolation is Needed

In multi-tenant or multi-client scenarios, a single shared MCP server process would present significant security risks:
- **Sandbox Pollution**: One client could modify or access files belonging to another client's workspace.
- **State Leakage**: Operations, command history, and environment variables could leak across sessions.
- **Teardown Conflicts**: Cancelling an operation in one session could accidentally terminate operations in another.

Session isolation resolves these issues by spinning up a dedicated, isolated `ahma serve stdio` subprocess for each unique session.

## Architecture

```
                 ┌─────────────────────────────────┐
                 │   Per-user hub, or              │
                 │   ahma serve http | unix        │
                 └────────────────┬────────────────┘
                                  │
         ┌────────────────────────┼────────────────────────┐
         │ (Session UUID-1)       │ (Session UUID-2)       │ (Session UUID-3)
         ▼                        ▼                        ▼
┌──────────────────┐    ┌──────────────────┐    ┌──────────────────┐
│   Subprocess 1   │    │   Subprocess 2   │    │   Subprocess 3   │
│ (ahma serve stdio)│   │ (ahma serve stdio)│   │ (ahma serve stdio)│
└────────┬─────────┘    └────────┬─────────┘    └────────┬─────────┘
         │                       │                       │
         ▼                       ▼                       ▼
 ┌───────────────┐       ┌───────────────┐       ┌───────────────┐
 │ Sandbox Scope │       │ Sandbox Scope │       │ Sandbox Scope │
 │   (locked)    │       │   (locked)    │       │   (locked)    │
 └───────────────┘       └───────────────┘       └───────────────┘
```

1. **Initialization**: When a client initializes a connection, the HTTP bridge generates a unique Session ID (UUID) and returns it in the `Mcp-Session-Id` header.
2. **Dedicated Subprocess**: The bridge spawns a fresh `ahma serve stdio` subprocess dedicated exclusively to that session.
3. **Roots exchange**: the client answers `roots/list`, during the handshake (2025-11-25 clients) or with its first tool call (2026-07-28 clients such as Claude Code, SPEC R-HUB.4.3).
4. **Sandbox Locking**: The subprocess uses the first roots list to configure and lock its kernel-level sandbox scope. Once locked, this scope cannot be changed (a security invariant).
5. **Teardown**: a session ends on `DELETE /mcp`, when its client misses consecutive liveness pings, or when it is the oldest inactive session and `--max-sessions` (default 50) is reached. Its worker is stopped with it.

## Configuration and Usage

Session isolation is not optional and has no flag: every HTTP-served MCP session — the
per-user hub's and an explicit `ahma serve http`'s alike — gets its own subprocess.

## Security Invariants

- **Strict sandbox derivation**: scope comes, in order, from a task vault, an explicit `--sandbox-scope` (locked, never replaced by client roots, SPEC R5.2.2), the client's first `roots/list` answer, elicitation, or the auto-narrowed `[sandbox] container_root`.
- **Authenticated sessions**: opening a session needs the server's bearer token (SPEC R-HUB.13, R-HUB.14). A sandboxed command could otherwise open its own session with roots, or `--no-sandbox`, of its choosing.
- **Zero Scope Widening**: Once locked, the scope is immutable (SPEC R5.1.1). A later `roots/list_changed` is a **tolerated no-op** (SPEC R10.5): the bridge acknowledges it with HTTP 202 and keeps the session alive without forwarding it — real clients re-emit the notification routinely, and terminating on it caused pointless session churn. Sandbox escape is prevented by the immutability of the commit, not by tearing the session down.
- **Process Cleanup**: When a subprocess crashes, the bridge terminates the associated session and marks it for cleanup. If the session expires or is closed via `DELETE /mcp`, the subprocess is forcefully killed (`kill_on_drop`).

## Bridge Lifecycle

The per-user hub's lifetime is described in [docs/hub.md](hub.md): it
exits when no MCP sessions **and** no hub subscribers have been attached for
`[hub] idle_timeout_secs`. What follows describes an explicitly started
bridge given `--idle-timeout`:

- With `--idle-timeout N`, once `active_sessions` drops to zero and stays there for N seconds, the bridge calls `terminate_all` and exits cleanly. Without it (the default) there is no idle exit.
- The bridge also handles SIGINT/SIGTERM gracefully: it terminates all sessions, removes the Unix socket file (if applicable), and exits.
- Clients that disconnect send `DELETE /mcp` (HTTP proxy) or call `transport.close()` (Unix proxy, TUI) to decrement `active_sessions` immediately.
