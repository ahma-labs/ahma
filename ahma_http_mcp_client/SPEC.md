# ahma_http_mcp_client Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`
* **Used by**: `ahma_mcp` (stdio proxy, `ahma tool list --http`, external HTTP MCP tools),
  `ahma_core` (chat agent), `ahma_tui` (live view)

## 1. User Story / Problem Statement

*As an in-workspace component that must talk MCP over HTTP — to the ahma bridge or to an
external server — I want one client implementation of the Streamable HTTP handshake, so that
no consumer can get the handshake order, the sandbox gate or protocol-version negotiation
subtly wrong on its own.*

## 2. Acceptance Criteria

**Shared Streamable HTTP client (`streamable`)**
- Implements the handshake in this order, and only this order: `initialize` (no session
  header) → open the `GET /mcp` SSE stream **before** `notifications/initialized` → send
  `notifications/initialized` → answer the server's `roots/list` over SSE with the same
  JSON-RPC id → only then `tools/call`. This is the AGENTS.md hard invariant; every
  in-workspace consumer uses this client rather than its own copy.
- A `tools/call` before the sandbox locks gets HTTP 409 / JSON-RPC `-32001`; the caller
  chooses to surface it (`ConflictRetryPolicy::NONE`, `ToolCallOutcome::SandboxInitializing`)
  or retry on a bounded policy.
- Negotiates the protocol version at `initialize` and echoes the server's answer in
  `MCP-Protocol-Version` on every later request (R8.3.5, via `ahma_common::mcp_protocol`).
- `clientInfo.name` is passed through verbatim — the server keys real behaviour
  (`supports_progress`, request budget) off it, so it is never normalized.
- Every timeout is supplied by the caller; nothing is hardcoded here.
- `delete_session` ends a session with `DELETE /mcp` (R8.3.6).
- Every POST retries per root SPEC R-HTTP (R-HTTP.2; `initialize` and `tools/call` are
  never re-sent after an interruption). A retried request keeps its JSON-RPC id. A final
  failure is a `ServiceError` naming "the MCP server at host:port"; callers that know better
  (the agent: "the ahma hub") re-attribute it with `ServiceError::for_service`.
  `with_retry_policy` overrides the default policy.
- JSON-RPC request ids are unique across **every** client in the process, not per client:
  callers `attach` a fresh client per call against one shared session and run calls
  concurrently, and the bridge refuses an id that is already in flight (R8.3.7).

**Other transports**
- `client::HttpMcpTransport`: an `rmcp` `Transport` over HTTP POST + SSE for external MCP
  servers, with optional OAuth 2.0 authorization-code + PKCE. The callback listens on
  `127.0.0.1` only.
- Tokens persist in `~/.ahma/mcp_http_token.json` (file `0600`, directory `0700` on Unix),
  outside every sandbox scope. `AHMA_HTTP_CLIENT_TOKEN_PATH` is ignored with a warning
  (R-CFG1.2).
- `local_socket_client`: the same Streamable HTTP transport over an `AF_UNIX` socket, on
  every OS including Windows (through `ahma_common::local_socket`), for the per-user hub's
  MCP endpoint (root SPEC R-HUB.2) and `ahma serve unix`. It is rmcp's
  `UnixSocketHttpClient` with only the connect replaced, since rmcp's uses
  `tokio::net::UnixStream`, which does not exist on Windows.
- `http_client::HttpClient`: how `streamable` sends. It builds every request with
  `reqwest` and sends it through `reqwest` over TCP, or through hyper over the local socket
  for a `unix://` base URL, returning an ordinary `reqwest::Response` with its body
  streaming either way; this is what lets the TUI and the agent reach the hub on Windows,
  where `reqwest` cannot reach an `AF_UNIX` socket. Both transports are retried by the same
  rules (R-HTTP.2): a failed connect never delivered the request; anything later may have.

## 3. Non-Functional Requirements

- **HTTP/3 preference**: outbound `reqwest` clients prefer HTTP/3 and fall back
  transparently (R8.7).

## 4. Out of Scope

- Hosting an MCP endpoint (`ahma_http_bridge`).
- Configurable OAuth endpoints and token refresh: the OAuth endpoints are fixed to
  Atlassian's (`auth.atlassian.com`), and there is no token refresh.
