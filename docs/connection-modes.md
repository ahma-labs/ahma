# Connection Modes

`ahma` supports:
1. **STDIO Mode** (default): IDE spawns `ahma` as a subprocess and communicates via standard I/O. Recommended for development.
2. **HTTP Mode**: Start `ahma --mode http` for HTTP/3 (QUIC) support.

## 1. STDIO Mode (Default)

The IDE spawns `ahma` as a subprocess and communicates via standard I/O. This is the recommended mode for development because:

- The IDE sets `cwd` to `${workspaceFolder}`, so the sandbox scope is automatic.
- Each workspace gets its own sandboxed server instance.
- No network exposure.

```bash
ahma --mode stdio
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
            "args": ["--tmp", "--livelog", "--simplify"]
        }
    }
}
```

Alternatively, in a terminal run `ahma --mode http` for visibility of all actions, and use:

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
            "args": ["--tmp", "--livelog", "--simplify"]
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
            "args": ["--tmp", "--livelog", "--simplify"]
        }
    }
}
```

**Antigravity** (sets `AHMA_SANDBOX_SCOPE` explicitly — Antigravity doesn't send `roots/list`):

```json
{
  "mcpServers": {
    "Ahma": {
      "command": "ahma",
      "args": [
        "serve",
        "stdio",
        "--tools",
        "rust,simplify",
        "--tmp",
        "--log-monitor"
      ],
      "env": {
        "AHMA_SANDBOX_SCOPE": "/Users/username"
      }
    }
  }
}

> [!NOTE]
> Replace `/Users/username` with your actual absolute home or project directory. Tilde expansion may not be supported depending on your MCP client's execution environment.
```

## 2. HTTP Mode (EXPERIMENTAL)

**IMPORTANT SECURITY NOTE**: HTTP mode is for local development only. Do not expose to untrusted networks. OAuth pass through is not yet fully implemented, so all tools are available to any client that can connect. Use firewall rules or `--http-host` to restrict access.

First start the server in a terminal with your preferred flags, defaulting to port 3000:

```bash
ahma --mode http --tmp --livelog --simplify
```

The HTTP server requires **HTTP/2 or HTTP/3**. HTTP/1.1 connections are explicitly rejected.

- **HTTP/2** (h2c — cleartext, no TLS required): the default transport for all HTTP clients.
- **HTTP/3** (QUIC): clients that advertise Alt-Svc support (including `ahma tui`) will automatically upgrade to QUIC when local TLS material is present at `~/.ahma/tls/`. Run `ahma tls init` to provision the certificate. See [TLS management](#tls-management-for-quic) below.

Default endpoint: `http://localhost:3000/mcp`

- `POST /mcp` with `Accept: application/json`: JSON-RPC, preferred for speed and low overhead.
- `POST /mcp` with `Accept: text/event-stream`: Streamable HTTP (SSE fallback for some networks).
- `GET /mcp` with `Accept: text/event-stream`: SSE stream for server-to-client events.

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

HTTP server that proxies MCP protocol to a stdio subprocess. Used for web clients, remote agents, debugging, or multi-client scenarios.

#  Used for increased visibility, web clients, remote agents, debugging, or multi-client scenarios. While sandbox scope is automatic in stdio mode, HTTP mode requires your MCP client to support `roots/list` responses. VSCode and Cursor do this automatically. For clients that don't, you can set a fixed sandbox scope (e.g. `--sandbox-scope /path/to/your/project`).

```bash
# Start on default port 3000 (sandbox scope from roots/list)
ahma --mode http

# Explicit sandbox scope (for clients that don't send roots/list)
ahma --mode http --sandbox-scope /path/to/your/project

# Via environment variable
export AHMA_SANDBOX_SCOPE=/path/to/your/project
ahma --mode http

# Custom port and host
ahma --mode http --http-port 8080 --http-host 127.0.0.1
```

| Feature | STDIO Mode | HTTP Mode |
|---------|-----------|----------|
| Sandbox scope | Set by IDE via `cwd` | Per session via `roots/list` |
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

In HTTP mode, each MCP session gets its own sandbox scope derived from the `roots/list` response. See [docs/session-isolation.md](session-isolation.md) for details.

## 3. Unix Socket Mode

Serves MCP Streamable HTTP over a Unix domain socket instead of TCP. Lower latency than HTTP mode, no port conflicts, and access-controlled by filesystem permissions.

```bash
# Start on default socket path /tmp/ahma.sock
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
            "url": "unix:///tmp/ahma.sock#/mcp"
        }
    }
}
```

**Why `#/mcp` in the URL?** VS Code uses the URL fragment (`#/subpath`) as the documented way to specify the HTTP endpoint path when connecting over a Unix socket. This is VS Code-specific syntax — the socket path is `/tmp/ahma.sock` and `/mcp` is the HTTP path to request on the socket. See the [VS Code MCP configuration reference](https://code.visualstudio.com/docs/copilot/reference/mcp-configuration) for details.

> Note: Unix socket mode is not available on Windows. Use `ahma serve http` instead.

## HTTP/3 (QUIC)

Ahma HTTP clients built on `reqwest` prefer HTTP/3 (QUIC) when the remote server advertises support via `Alt-Svc`, with transparent fallback to HTTP/2 and HTTP/1.1.

For this local HTTP bridge endpoint, clients should expect HTTP/2 or HTTP/1.1.

## TLS management for QUIC

`ahma tui` upgrades to HTTP/3 (QUIC) when:
1. The server returns an `Alt-Svc: h3=…` header, **and**
2. Local TLS material exists at `~/.ahma/tls/` (or `$AHMA_TLS_DIR`).

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

Set `AHMA_DISABLE_QUIC=1` to prevent the HTTP/3 upgrade globally.
