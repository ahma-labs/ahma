# ahma_http_bridge Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a web-based client or remote integration, I want to communicate with Ahma over HTTP and SSE so that I can invoke MCP tools and receive live notifications using standard web protocols.*

## 2. Acceptance Criteria

- **HTTP/SSE Transport**: Exposes MCP protocol over POST `/mcp` (JSON-RPC) and GET `/mcp` (Server-Sent Events stream for notifications).
- **Streamable HTTP**: Supports multiplexed response streaming and Event ID ordering on POST requests with `Accept: text/event-stream`.
- **Reconnection Resilience**: Implements an event history buffer to replay missed events using the `Last-Event-Id` header.
- **Session Isolation**: Spawns a dedicated, isolated `ahma serve stdio` subprocess per `Mcp-Session-Id`.
- **Sandbox Derivation**: Derives the sandbox scope from the client's first `roots/list` response and locks it. Rejects any scope widening attempts with HTTP 403.
- **Auto-Restart**: Automatically restarts the stdio subprocess if it crashes.
- **Health check**: Exposes a `/health` endpoint to monitor server status.

## 3. Non-Functional Requirements

- **Protocol Parity**: Must fully implement all JSON-RPC methods defined by the Model Context Protocol.
- **HTTP/3 Preference**: Prefers HTTP/3 (QUIC) transport when the client supports it, falling back to HTTP/2 and HTTP/1.1 transparently.

## 4. Protocol & State Invariants (RB requirements)

- **RB.1 — Single POST pipeline**: `POST /mcp` requests for the JSON
  (`Accept: application/json`) and SSE (`Accept: text/event-stream`) transports flow through
  **one shared pipeline** (validate → gate → dispatch → encode). The response *encoding* is the
  only permitted difference between the transports; gate order and error/timeout semantics are
  identical by construction, never duplicated per transport.
  - **RB.1.1 — Observable sandbox gate**: `tools/call` before the sandbox lock returns
    HTTP 409 with JSON-RPC error `-32001`, on both transports.
  - **RB.1.2 — Recoverable wait-window timeout**: when the bridge's wait window elapses before
    the subprocess answers a forwarded request, both transports return **HTTP 200** carrying a
    JSON-RPC `-32002` error with the original request id, and the session survives. A non-2xx
    here is transport-fatal to rmcp clients and tears the whole MCP session down.
- **RB.2 — Sandbox notifications are the authoritative state input**: the subprocess's
  `notifications/sandbox/configured` / `notifications/sandbox/failed` are the **sole inputs
  that open** (`Active`) **or fail** (`Failed`) the `tools/call` gate. Bridge-side validation
  of client roots may *stage* provisional scopes (`Configuring`) — used only as the fallback
  scope record when the notification omits its scope summary — and may fail fast when a scope
  is provably unresolvable (no roots and no fallback scope), but must never itself transition
  a session to `Active`.
- **RB.3 — Deterministic POST-SSE interleave**: the SSE response stream for a POST request
  contains exactly the broadcast events already queued when the response became available,
  followed by the response event, then closes. No wall-clock windows: an event that arrives
  later is delivered on the live `GET /mcp` stream and retained for `Last-Event-Id` replay,
  never raced against a timer.

## 5. Out of Scope

- Implementing the command execution or sandboxing logic directly (delegated to the `ahma serve stdio` subprocess).
