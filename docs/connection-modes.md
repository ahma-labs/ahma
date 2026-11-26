# Connection Modes

`ahma` has three transports:

1. **STDIO** (default, recommended): your editor spawns `ahma serve stdio`, a pipe to the per-user hub.
2. **HTTP** (`ahma serve http`): a Streamable HTTP server you start, on `127.0.0.1:3000` (HTTP/1.1 and HTTP/2; HTTP/3 when a certificate exists).
3. **Unix socket** (`ahma serve unix`, also what a bare `ahma serve` starts): the same server on a Unix domain socket.

`serve http` and `serve unix` require a bearer token by default.

> **Editor sessions go through one hub.** The `ahma serve stdio` an editor spawns is a pipe
> to the single per-user hub, and tools run in a kernel-sandboxed worker per session
> ([hub.md](hub.md)). `ahma serve http` and `ahma serve unix` are separate servers you start
> yourself; they too run one sandboxed worker per session.

## 1. STDIO Mode (Default)

Your editor spawns `ahma` as a subprocess and communicates via standard I/O.
That subprocess is a **pipe**: it ensures the per-user hub is running and
forwards to it (SPEC R-HUB.1). This is the recommended mode because:

- The sandbox scope comes from the workspace roots your editor reports (`roots/list`): VS Code and Cursor answer during the handshake, Claude Code with its first tool call ([below](#claude-code-and-the-2026-07-28-protocol)). The subprocess's working directory is **never** trusted as a scope on its own (SPEC R5.2.1): it is client-config-controlled and spoofable.
- Each session gets its own kernel-sandboxed worker, locking its own scope. Three windows on three projects are three workers with three scopes.
- Flags in *your* `mcp.json` (`--tools`, `--sandbox-scope`, a task vault) apply to *your* session and no one else's (SPEC R-HUB.4).
- No network exposure.

```bash
ahma serve stdio
```

### mcp.json examples

`ahma setup` writes the stdio entry for the editors it finds (it never writes an HTTP or
Unix-socket entry: those need the server's token, below). By hand:

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
> - **Antigravity answers `roots/list` with an empty array.** It is roots-*empty*, not roots-*less* (SPEC R5.4.2 has the wire evidence). An empty answer is not a workspace, so ahma falls through to the next scope source rather than treating it as one.
> - **`ahma setup` writes no sandbox scope into this file.** It is client-owned (anyone configuring the client can edit it), so a scope written here is exactly the over-broad path ahma is meant to distrust. Point ahma at the directory that holds your projects instead, in your own `~/.ahma/settings.toml`:
>   ```toml
>   [sandbox]
>   container_root = "~/github"
>   ```
>   ahma narrows the writable scope from there to the one project you are working in, keeping the rest of the container readable but not writable.
> - **You do not need a "sync" flag.** By default (`tools.execution_mode = "async"`) a command that finishes within a short window returns its output in the same response; a longer one returns an operation id to collect with `await`, while writers stay ordered by the workspace write queue ([workspace-queue.md](workspace-queue.md)). Set `sync` ([settings.md](settings.md#sync-or-async-toolsexecution_mode)) to make every call wait for its result, as long as your client tolerates one long-open request (SPEC R2.6). Sending `"sync": true` to `run_terminal_command` does nothing; the result will say so.

> **Git worktrees.** Inside a linked worktree the repository's git storage sits outside the
> workspace, so ahma grants `<main>/.git` and `<main>/.git/worktrees/<name>` read/write, but only
> when the git dir names the worktree back through the `gitdir` back-reference git itself writes.
> An unverifiable `gitdir:` pointer is refused and logged. Writes to `<git_dir>/hooks` remain
> denied, kernel-enforced on macOS and application-layer only on Linux
> ([security sandbox](security-sandbox.md)).

### Claude Code and the 2026-07-28 protocol

Claude Code opens with the 2026-07-28 `server/discover` handshake. ahma's stdio frontend
answers it, so the session runs on 2026-07-28. That protocol forbids a server writing requests
to stdout, so for such a client (SPEC R-HUB.4.2–4.6):

- **Roots arrive with the first tool call.** The first call that needs a scope is answered with a request for the client's roots, and the retry that carries them is the call that runs. `status` and `ahma_help` answer before then.
- **Approval prompts ride tool calls.** A grant, network, link or web question is attached to the running call or, when none is running (an async command), to the client's next call. A question the retry leaves unanswered counts as cancelled.
- **Only tool-list changes are pushed**, on the `subscriptions/listen` stream Claude Code opens. Log messages and ahma's session events are not sent ([session-health-notifications.md](session-health-notifications.md)).

The hub behind the frontend still runs a 2025-11-25 session and never sees the newer
protocol. `ahma serve http` answers `subscriptions/listen` with `{}` and does not hold it open.

### Cursor shared-process and empty roots

Cursor runs all `stdio` MCP servers in a **shared process** when the window has no workspace
folder open (a fresh install, or a single file opened rather than a folder). Its MCP client
then answers `roots/list` with `{"roots":[]}`.

An empty answer is not a usable scope (SPEC R5.2.7). With `[sandbox] container_root`
configured, ahma falls back to it (narrowed to the first project a tool call touches, R5.2.6).
Without one, it fails the session at once rather than silently scoping every command to an
empty or wrong directory: `tools/call` returns HTTP 403 with the remediation below. While roots
have not arrived yet, `tools/call` returns HTTP 409 / JSON-RPC `-32001` ("Sandbox
initializing..."). The built-ins that change nothing (`status`, `ahma_help`, `await`, …) still
answer, so `ahma_help topic=roots` can explain the wait.

**Resolutions (pick one):**

1. **Open a workspace folder**: in Cursor, `File → Open Folder...`, so Cursor advertises it as a root.
2. **Configure an explicit scope**: pass `--sandbox-scope /path/to/your/project` in your `mcp.json` `args`:
   ```json
   "args": ["serve", "stdio", "--log-monitor", "--sandbox-scope", "/path/to/project"]
   ```
3. **Set a container root**: `container_root = "~/github"` under `[sandbox]` in `~/.ahma/settings.toml`. ahma uses it only when the client reports no usable roots, and narrows the writable scope to the project in use.

`--scratch` (deprecated alias `--sandbox`) is *not* a resolution: it adds an auxiliary scratch
directory alongside the workspace scope, and does nothing unless you also set
`[sandbox] scratch_directory`.

## 2. HTTP Mode

**Security.** `ahma serve http` binds `127.0.0.1` and requires a bearer token on every route
but `/health` (see *Authentication* below). `--host 0.0.0.0` exposes it to that network; put a
firewall or an authenticating reverse proxy in front.

```bash
ahma serve http --log-monitor                                # port 3000, scope from roots/list
ahma serve http --sandbox-scope /path/to/your/project        # for clients that send no roots
ahma serve http --port 8080 --host 127.0.0.1
```

Each session's scope comes from that client's `roots/list` answer. An explicit
`--sandbox-scope` is locked and never replaced by client roots (SPEC R5.2.2). A client that
declares no `roots` capability on a server with no `--sandbox-scope` gets an immediate,
actionable error instead of a handshake timeout.

The server accepts **HTTP/1.1 and HTTP/2** (`--disable-http1-1` requires HTTP/2+), and
advertises **HTTP/3** when a certificate exists ([below](#http3-and-tls)).

> Over cleartext, HTTP/2 needs prior knowledge (`curl --http2-prior-knowledge`, reqwest
> `http2_prior_knowledge()`). A plain HTTP/1.1 client works unless the server runs with
> `--disable-http1-1`.

Endpoint: `http://localhost:3000/mcp`

- `POST /mcp` with `Accept: application/json`: one JSON-RPC response; preferred.
- `POST /mcp` with `Accept: text/event-stream`: the response as an SSE stream, with notifications first.
- `GET /mcp` with `Accept: text/event-stream`: the server-to-client event stream.

Protocol notes:

- Notifications (id-less JSON-RPC messages) are answered `202 Accepted` with no body, on both POST transports.
- An unknown or expired `Mcp-Session-Id` is answered `404 Not Found`: re-send `initialize` to start a fresh session.
- JSON-RPC **batch arrays are rejected** with `400` (batching was removed from the MCP spec in 2025-06-18).
- A request whose `id` is `null`, or an `initialize` sent without an `id`, is rejected with `400` and JSON-RPC `-32600`: MCP requires a string or number id. Nothing runs and no session starts. The error's `data.remediation` says what to do; if it keeps happening, run [`ahma doctor`](doctor.md).
- Browser requests are accepted only from loopback origins; a non-loopback `Origin` is rejected with `403` (DNS-rebinding guard). Non-browser clients send no `Origin` and are unaffected.
- The deprecated 2024-11-05 two-endpoint HTTP+SSE transport (`/sse` + `/messages`) is **not** implemented; speak Streamable HTTP on `/mcp`.

### Authentication (on by default)

A command running inside ahma's sandbox can reach a loopback port, and a session locks the
scope its client's `roots/list` names, so `ahma serve http` makes a token and requires it on
every route but `/health` (SPEC R-HUB.14). The token is 64 hex characters in
`serve-http-<port>.token` in your runtime directory (`$XDG_RUNTIME_DIR/ahma`, else `~/.ahma`;
`%LOCALAPPDATA%\ahma\run` on Windows), readable only by you and outside what a sandboxed
command can read. ahma's own clients (`ahma tui --connect`, the proxies) find it themselves.
Any other client must send it:

```json
{
  "servers": {
    "Ahma-http": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer ${input:ahma-token}" }
    }
  },
  "inputs": [{ "id": "ahma-token", "type": "promptString", "password": true,
               "description": "Contents of the ahma serve http token file" }]
}
```

The generated token changes on every start. For a config that survives restarts, start the
server with `--require-token-path <file>` (a file outside every sandbox scope) and use that
token. `--allow-unauthenticated` runs with none; ahma says so on every start, because any local
process, including a sandboxed command, can then use the server.

### Streamable HTTP with curl

```bash
TOKEN=$(cat ~/.ahma/serve-http-3000.token)   # or $XDG_RUNTIME_DIR/ahma/…

# JSON response
curl -X POST http://localhost:3000/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" -H "Accept: application/json" \
  -H "Mcp-Session-Id: <session-uuid>" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'

# SSE response
curl -X POST http://localhost:3000/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" -H "Accept: text/event-stream" \
  -H "Mcp-Session-Id: <session-uuid>" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'

# Reconnect and replay every event after id 42
curl -X GET http://localhost:3000/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H "Accept: text/event-stream" \
  -H "Mcp-Session-Id: <session-uuid>" -H "Last-Event-Id: 42"
```

| | STDIO | HTTP / Unix |
|---|---|---|
| Server | Per-user hub, started on demand | One you start and stop |
| Sandbox scope | Per session from roots (or `--sandbox-scope`) | Same |
| Authentication | Hub token, handled for you | Bearer token; third-party clients must send it |
| Configuration | `mcp.json` args | CLI flags or `~/.ahma/settings.toml` |

Every session, in every mode, gets its own sandbox scope and worker
([session-isolation.md](session-isolation.md)). A client that wants to show reconnects and
pending sandbox grants can listen for ahma's
[session-health notifications](session-health-notifications.md).

## 3. Unix Socket Mode

Serves MCP Streamable HTTP over a Unix domain socket instead of TCP: no port to collide on.
File permissions keep other users out but not your own sandboxed commands, so it requires a
token too (below).

The per-user hub uses this transport; running `ahma serve unix` yourself starts a
**separate**, operator-owned server (SPEC R-HUB.1), for a fixed scope or a custom path. You
don't need it for ordinary editor use.

```bash
# Default socket: mcp.sock in your runtime directory, beside the hub's hub.sock
ahma serve unix

# Custom socket path
ahma serve unix --socket-path /run/ahma/mcp.sock

# Linux abstract socket (@ prefix, no filesystem entry)
ahma serve unix --socket-path @ahma
```

**Authentication (on by default).** The token is written beside the socket (`mcp.sock` →
`mcp.token`; for an abstract `@name` socket, `abstract-name.token` in the runtime directory),
readable only by you, and removed when the server stops (SPEC R-HUB.14). ahma's own clients
read it from there; any other client is refused with a 401 until it sends the token.
`--require-token-path` chooses your own, and `--allow-unauthenticated` runs without one,
disclosed on every start.

### mcp.json configuration (VS Code)

```json
{
  "servers": {
    "ahma-unix": {
      "type": "http",
      "url": "unix:///run/user/1000/ahma/mcp.sock#/mcp",
      "headers": { "Authorization": "Bearer ${input:ahma-token}" }
    }
  },
  "inputs": [{ "id": "ahma-token", "type": "promptString", "password": true,
               "description": "Contents of the ahma serve unix token file (mcp.token)" }]
}
```

VS Code takes the HTTP path from the URL fragment: the part before `#` is the socket path and
`/mcp` the path requested on it ([VS Code MCP reference](https://code.visualstudio.com/docs/copilot/reference/mcp-configuration)).

> On Windows (10 1803 or later) this is the same `AF_UNIX` socket file, in
> `%LOCALAPPDATA%\ahma\run`. Abstract `@` sockets are Linux-only. Whether a Windows MCP client
> can dial a Unix socket is up to that client; `ahma serve http` works with any of them.

## HTTP/3 and TLS

`ahma serve http` advertises HTTP/3 with `Alt-Svc` when a certificate exists in `~/.ahma/tls/`
and `--disable-quic` is not set. Clients that honour it (`ahma tui`, ahma's own clients)
upgrade; others stay on HTTP/2 or HTTP/1.1. The `GET /mcp` push stream is not served over
HTTP/3 (406), so clients keep it on HTTP/2 or 1.1 (ahma_http_bridge R8.7.3).

```bash
ahma tls init     # create the certificate (idempotent; `ahma setup` does this for you)
ahma tls rotate   # replace it (e.g. every 30 days, or after a key compromise)
ahma tls status   # path, age, and whether rotation is recommended
```

The private key (`~/.ahma/tls/key.der`) is mode `0600` on Unix. The certificate is self-signed
and used only for local loopback QUIC sessions.
