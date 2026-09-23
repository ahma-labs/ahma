# Connection Modes

`ahma` supports:
1. **STDIO Mode** (default): your editor spawns `ahma` as a subprocess and communicates via standard I/O. Recommended for development.
2. **HTTP Mode**: Start `ahma serve http` for HTTP/3 (QUIC) support.

> **All of them go through one daemon.** Whatever an editor spawns, the process
> that actually serves MCP is the single per-user daemon, and the tools run in a
> kernel-sandboxed worker per session. See [docs/daemon.md](daemon.md).

## 1. STDIO Mode (Default)

Your editor spawns `ahma` as a subprocess and communicates via standard I/O.
That subprocess is a **pipe**: it ensures the per-user daemon is running and
forwards to it (SPEC R-DAEMON.1). This is the recommended mode for development
because:

- The sandbox scope comes from the workspace roots your editor reports via `roots/list` (VS Code and Cursor do this automatically). The subprocess's working directory is **never** trusted as a scope on its own (SPEC R5.2.1) — it is client-config-controlled and spoofable.
- Each session gets its own kernel-sandboxed worker, locking its own scope. Three windows on three projects are three workers with three scopes.
- Flags in *your* `mcp.json` — `--tools`, `--sandbox-scope`, a task vault — apply to *your* session and no one else's (SPEC R-DAEMON.4).
- No network exposure.

```bash
ahma serve stdio
```

### mcp.json examples

**VS Code** (user profile `mcp.json` or `.vscode/mcp.json`):

> The user-level `mcp.json` lives in your VS Code profile folder:
> macOS `~/Library/Application Support/Code/User/mcp.json`,
> Linux `~/.config/Code/User/mcp.json`,
> Windows `%APPDATA%\Code\User\mcp.json`.
> Or run `MCP: Open User Configuration` from the Command Palette.

```json
{
    "servers": {
        "Ahma": {
            "type": "stdio",
            "command": "ahma",
            "args": ["serve", "stdio", "--log-monitor"]
        }
    }
}
```

Alternatively, in a terminal run `ahma serve http` for visibility of all actions, and use:

```json
{
    "servers": {
        "Ahma-http": {
            "type": "http",
            "url": "http://localhost:3000/mcp"
        }
    }
}
```

**Cursor** (`~/.cursor/mcp.json`):

```json
{
    "mcpServers": {
        "Ahma": {
            "type": "stdio",
            "command": "ahma",
            "args": ["serve", "stdio", "--log-monitor"]
        }
    }
}
```

**Claude Code** (`~/.claude.json`):

```json
{
    "mcpServers": {
        "Ahma": {
            "type": "stdio",
            "command": "ahma",
            "args": ["serve", "stdio", "--log-monitor"]
        }
    }
}
```

> **Git worktrees.** Inside a linked worktree the repository's git storage sits outside the
> workspace, so ahma grants `<main>/.git` and `<main>/.git/worktrees/<name>` read/write — but only
> when the git dir names the worktree back through the `gitdir` back-reference git itself writes.
> An unverifiable `gitdir:` pointer is refused and logged. Writes to `<git_dir>/hooks` remain
> denied, kernel-enforced on macOS and application-layer only on Linux
> ([security sandbox](security-sandbox.md)).

**Antigravity / LM Studio** (same entry, minus the `"type"` field these clients do not accept):

```json
{
  "mcpServers": {
    "Ahma": {
      "command": "ahma",
      "args": ["serve", "stdio", "--tools", "simplify", "--log-monitor"]
    }
  }
}
```

> [!NOTE]
> - **Antigravity answers `roots/list` — with an empty array.** It is roots-*empty*, not roots-*less* (an earlier revision of this document said otherwise; the correction is recorded in SPEC R5.4.2 with the wire evidence). An empty answer is not a workspace, so ahma falls through to the next scope source rather than treating it as one.
> - **`ahma setup` writes no sandbox scope into this file.** It is client-owned — anyone configuring the client can edit it — so a scope written here is exactly the over-broad path that ahma is meant to distrust. Point ahma at the directory that holds your projects instead, in your own `~/.ahma/settings.toml`:
>   ```toml
>   [sandbox]
>   container_root = "~/github"
>   ```
>   ahma narrows the writable scope from there to the one project you are actually working in, keeping the rest of the container readable but not writable.
> - **You do not need a "sync" flag.** By default (`tools.execution_mode = "sync"`) a command returns its output in the same response, waiting as long as your client tolerates on one open request (SPEC R2.6). Set `async` ([settings.md](settings.md#sync-or-async-toolsexecution_mode)) to get an operation id back after a short window and collect results with `await`. Sending `"sync": true` to `run_terminal_command` does nothing; the result will say so.

## 2. HTTP Mode (EXPERIMENTAL)

**IMPORTANT SECURITY NOTE**: HTTP mode is for local development only. Do not expose to untrusted networks. OAuth pass through is not yet fully implemented, so all tools are available to any client that can connect. Use firewall rules or `--host` to restrict access.

First start the server in a terminal with your preferred flags, defaulting to port 3000:

```bash
ahma serve http --scratch --log-monitor
```

The HTTP server accepts **HTTP/1.1 and HTTP/2** by default (pass `--disable-http1-1` to require HTTP/2+), and clients may upgrade to **HTTP/3** via Alt-Svc.

- **HTTP/2** (h2c — cleartext, no TLS required): the preferred transport for all HTTP clients.
- **HTTP/3** (QUIC): clients that advertise Alt-Svc support (including `ahma tui`) will automatically upgrade to QUIC when local TLS material is present at `~/.ahma/tls/`. Run `ahma tls init` to provision the certificate. See [TLS management](#tls-management-for-quic) below.

Default endpoint: `http://localhost:3000/mcp`

- `POST /mcp` with `Accept: application/json`: JSON-RPC, preferred for speed and low overhead.
- `POST /mcp` with `Accept: text/event-stream`: Streamable HTTP (SSE fallback for some networks).
- `GET /mcp` with `Accept: text/event-stream`: SSE stream for server-to-client events.

Protocol notes:

- Notifications (id-less JSON-RPC messages) are answered `202 Accepted` with no body, on both POST transports.
- An unknown or expired `Mcp-Session-Id` is answered `404 Not Found` — re-send `initialize` to start a fresh session.
- JSON-RPC **batch arrays are rejected** with `400` (batching was removed from the MCP spec in 2025-06-18).
- Browser requests are accepted only from loopback origins; a non-loopback `Origin` header is rejected with `403` (DNS-rebinding guard). Non-browser clients send no `Origin` and are unaffected.
- The **deprecated 2024-11-05 two-endpoint HTTP+SSE transport** (`/sse` + `/messages`) is **not** implemented; clients must speak Streamable HTTP on `/mcp`.

> **Client configuration**: Configure your HTTP client with HTTP/2 prior-knowledge (`--http2-prior-knowledge` in curl, `http2_prior_knowledge()` in reqwest) because the server does not negotiate via ALPN (no TLS).

Then configure your IDE to connect to for example `http://localhost:3000/mcp`:

```json
{
    "servers": {
        "Ahma-http": {
            "type": "http",
            "url": "http://localhost:3000/mcp"
        }
    }
}
```

HTTP server that proxies MCP protocol to a stdio subprocess. Used for increased visibility, web clients, remote agents, debugging, or multi-client scenarios. HTTP mode derives each session's sandbox scope from that client's `roots/list` answer (VS Code and Cursor answer automatically). For clients that don't support roots, set a fixed sandbox scope (e.g. `--sandbox-scope /path/to/your/project`) — an explicit scope is locked and is never replaced by client roots (SPEC R5.2.2). A client that declares no `roots` capability on a bridge with no `--sandbox-scope` gets an immediate, actionable error instead of a handshake timeout.

```bash
# Start on default port 3000 (sandbox scope from roots/list)
ahma serve http

# Explicit sandbox scope (for clients that don't send roots/list)
ahma serve http --sandbox-scope /path/to/your/project

# Custom port and host
ahma serve http --port 8080 --host 127.0.0.1
```

| Feature | STDIO Mode | HTTP Mode |
|---------|-----------|----------|
| Sandbox scope | Client `roots/list` (or explicit flag) | Per session via `roots/list` (or explicit flag) |
| Per-project isolation | Automatic | Automatic (per session) |
| Configuration | `mcp.json` in IDE | CLI args or env vars |
| Use case | Standard IDE integration | Debugging, advanced setups |

## 3. HTTP Streaming (MCP Streamable HTTP)

Ahma implements [MCP Streamable HTTP](https://spec.modelcontextprotocol.io/) for multiplexed, reconnection-resilient communication.

**POST with JSON response:**

```bash
curl -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -H "Mcp-Session-Id: <session-uuid>" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

**POST with SSE streaming:**

```bash
curl -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -H "Mcp-Session-Id: <session-uuid>" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

**Reconnect with event replay:**

```bash
curl -X GET http://localhost:3000/mcp \
  -H "Accept: text/event-stream" \
  -H "Mcp-Session-Id: <session-uuid>" \
  -H "Last-Event-Id: 42"
# Receives all events with ID > 42
```

| Feature | SSE | HTTP Streaming |
|---------|-----|----------------|
| Content negotiation | GET only | POST with `Accept: text/event-stream` |
| Event sequencing | None | SSE `id:` field for ordering and replay |
| Reconnection | Client guesses missed events | `Last-Event-Id` enables precise replay |
| Full-duplex | Separate streams | Single POST stream |
| Multiplexing | Limited | Native per-session |

## Session Isolation

In HTTP mode, each MCP session gets its own sandbox scope derived from the `roots/list` response. See [docs/session-isolation.md](session-isolation.md) for details. A client that wants to show reconnects and pending sandbox grants can listen for ahma's session-health notifications: [docs/session-health-notifications.md](session-health-notifications.md).

### Cursor shared-process and empty roots

Cursor runs all `stdio` MCP servers in a **shared process** context when the IDE window has no workspace folder open (e.g. after a fresh install or when opening a single file rather than a folder). In this case, Cursor's MCP client responds to `roots/list` with an empty array `{"roots":[]}`.

Ahma treats an empty `roots/list` response as "client has no workspace roots yet" and keeps the sandbox in a deferred state. Any `tools/call` request before a real workspace root is provided returns HTTP 409 / JSON-RPC error `-32001` ("Sandbox initializing...") instead of silently scoping every command to an empty or wrong directory.

**Resolutions (pick one):**

1. **Open a workspace folder** — in Cursor: `File → Open Folder...` — so that Cursor advertises the folder as a workspace root in its next `roots/list` response.
2. **Configure an explicit scope** — pass `--sandbox-scope /path/to/your/project` in your `mcp.json` `args` list:
   ```json
   "args": ["serve", "stdio", "--log-monitor", "--sandbox-scope", "/path/to/project"]
   ```
3. **Set a container root** — put `container_root = "~/github"` under `[sandbox]` in `~/.ahma/settings.toml`, naming the directory that holds your projects. ahma uses it only when the client reports no usable roots, and narrows the writable scope to the single project subtree in use.

`--scratch` (deprecated alias `--sandbox`) is *not* a resolution: it adds an auxiliary scratch directory alongside the real workspace scope, and does nothing unless you also set `[sandbox] scratch_directory`. It used to default to `~/sandbox` and double as the scope fallback, which is how sessions ended up silently locked to a directory nobody chose.

## 4. Unix Socket Mode

Serves MCP Streamable HTTP over a Unix domain socket instead of TCP. Lower latency than HTTP mode, no port conflicts, and access-controlled by filesystem permissions.

This is the transport the per-user daemon uses; running `ahma serve unix`
yourself starts a **separate**, operator-owned server (SPEC R-DAEMON.1), which
is what you want for a fixed scope or a custom path and not what you need for
ordinary editor use.

```bash
# Start on the default socket: mcp.sock in your per-user runtime directory
# ($XDG_RUNTIME_DIR/ahma, else ~/.ahma)
ahma serve unix

# Custom socket path
ahma serve unix --socket-path /run/ahma/mcp.sock

# Linux abstract socket (@ prefix, no filesystem entry)
ahma serve unix --socket-path @ahma
```

### mcp.json configuration (VS Code)

```json
{
    "servers": {
        "ahma-unix": {
            "type": "http",
            "url": "unix:///run/user/1000/ahma/mcp.sock#/mcp"
        }
    }
}
```

**Why `#/mcp` in the URL?** VS Code uses the URL fragment (`#/subpath`) as the documented way to specify the HTTP endpoint path when connecting over a Unix socket. This is VS Code-specific syntax — the part before `#` is the socket path (`ahma setup` writes the resolved per-user path for you) and `/mcp` is the HTTP path to request on it. See the [VS Code MCP configuration reference](https://code.visualstudio.com/docs/copilot/reference/mcp-configuration) for details.

> Note: Unix socket mode is not available on Windows. Use `ahma serve http` instead.

## HTTP/3 (QUIC)

Ahma HTTP clients built on `reqwest` prefer HTTP/3 (QUIC) when the remote server advertises support via `Alt-Svc`, with transparent fallback to HTTP/2 and HTTP/1.1.

For this local HTTP bridge endpoint, clients should expect HTTP/2 or HTTP/1.1.

## TLS management for QUIC

`ahma tui` upgrades to HTTP/3 (QUIC) when:
1. The server returns an `Alt-Svc: h3=…` header, **and**
2. Local TLS material exists at `~/.ahma/tls/` (or via `--tls-dir`).

Manage the local TLS certificate with the `ahma tls` subcommands:

```bash
# First-time provisioning (idempotent — safe to re-run)
ahma tls init

# Force certificate rotation (e.g. every 30 days or after key compromise)
ahma tls rotate

# Show certificate path, age, and whether rotation is recommended
ahma tls status
```

The private key (`~/.ahma/tls/key.der`) is stored with mode `0600` (Unix). The certificate is self-signed and used only for local loopback QUIC sessions — it is not exposed to the network.

Use `--disable-quic` flag to prevent the HTTP/3 upgrade.
