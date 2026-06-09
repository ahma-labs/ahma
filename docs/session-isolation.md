# Session Isolation

Session isolation is a key security feature of the Ahma HTTP Bridge (`ahma serve http`). It ensures that each client connection or session runs in a completely isolated environment with its own sandbox boundaries and lifecycle.

## Why Session Isolation is Needed

In multi-tenant or multi-client scenarios, a single shared MCP server process would present significant security risks:
- **Sandbox Pollution**: One client could modify or access files belonging to another client's workspace.
- **State Leakage**: Operations, command history, and environment variables could leak across sessions.
- **Teardown Conflicts**: Cancelling an operation in one session could accidentally terminate operations in another.

Session isolation resolves these issues by spinning up a dedicated, isolated `ahma serve stdio` subprocess for each unique session.

## Architecture

```
                 ┌─────────────────────────────────┐
                 │        HTTP Bridge Server       │
                 │        (ahma serve http)        │
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
3. **Roots Exchange**: During the MCP handshake, the client sends its workspace roots via the `roots/list` protocol.
4. **Sandbox Locking**: The subprocess uses the first roots list to configure and lock its kernel-level sandbox scope. Once locked, this scope cannot be changed (a security invariant).
5. **Teardown**: When the session is closed via `DELETE /mcp` or terminates due to inactivity, the dedicated subprocess is cleanly shut down and its sandbox is dismantled.

## Configuration and Usage

To enable session isolation, pass the `--session-isolation` flag when starting the HTTP bridge:

```bash
ahma serve http --session-isolation
```

Alternatively, you can set the `AHMA_SESSION_ISOLATION=1` environment variable.

## Security Invariants

- **Strict Sandbox Derivation**: The sandbox scope is derived *strictly* from the client's first `roots/list` response.
- **Zero Scope Widening**: Once locked, any subsequent attempt by the client to alter or expand the roots list will be rejected, and the HTTP bridge will terminate the session immediately (HTTP 403 Forbidden).
- **Process Cleanup**: The HTTP bridge implements auto-restart and strict subprocess tracking. If a subprocess crashes, it is automatically restarted with the same credentials and sandbox constraints. If the session expires, the process is forcefully killed (`kill_on_drop`).
