# ahma_http_bridge Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a web-based client or remote integration, I want to communicate with Ahma over HTTP and SSE so that I can invoke MCP tools and receive live notifications using standard web protocols.*

## 2. Acceptance Criteria

- **HTTP/SSE Transport**: Exposes MCP protocol over POST `/mcp` (JSON-RPC) and GET `/mcp` (Server-Sent Events stream for notifications).
- **Streamable HTTP**: Supports multiplexed response streaming and Event ID ordering on POST requests with `Accept: text/event-stream`. Notifications (id-less messages) are answered with HTTP 202 on both POST transports; an unknown or terminated session ID is answered with HTTP 404 so a spec-conforming client re-initializes; JSON-RPC batch arrays are rejected with HTTP 400 (batching was removed from the MCP spec in 2025-06-18).
- **Stateful by design**: every session is bound to a live subprocess holding a kernel sandbox lock, so the spec's optional *stateless* server mode is deliberately out of scope — a scope commit cannot be stateless. The bridge instead implements the stateful session lifecycle in full (session header, DELETE termination, 404-driven re-initialize).
- **Origin validation**: requests carrying a non-loopback `Origin` header are rejected (DNS-rebinding guard, per the MCP transport security requirements). Non-browser clients send no `Origin` and are unaffected.
- **Reconnection Resilience**: Implements an event history buffer to replay missed events using the `Last-Event-Id` header.
- **Session Isolation**: Spawns a dedicated, isolated `ahma serve stdio` subprocess per `Mcp-Session-Id`.
- **Sandbox Derivation**: Derives the sandbox scope from the client's first `roots/list` response and locks it (unless an explicit `--sandbox-scope` was given, which is locked and never replaced — SPEC R5.2.2). Once locked, a later `roots/list_changed` is a tolerated no-op (SPEC R10.5): acknowledged with 202, not forwarded, session kept alive. A client that declares no `roots` capability at `initialize`, on a bridge with no fallback scope, has its sandbox failed immediately so `tools/call` gets a deterministic remediated 403 instead of a handshake timeout.
- **Subprocess death**: a crashed subprocess fails its session's in-flight requests with a classified error (R-SIGN.5) and the session terminates; the client re-initializes into a fresh session. (An earlier auto-restart-with-handshake-replay mechanism was intentionally removed.)
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
- **RB.4 — Active client liveness probe**: the bridge periodically pings every session that has
  a live SSE subscriber, over the same routed-request channel used for sampling
  (`Session::routed_requests`), and terminates a session — cascading to its sandboxed worker
  subprocess — once its client misses a fixed number of consecutive pings. This closes a gap the
  subprocess's own liveness ping cannot see: that ping (SPEC R2.6.5.3) is answered by the bridge
  on the client's behalf the moment an SSE subscriber is merely *attached*, so it proves only
  that the socket looks open, never that anything is actually reading it — a crashed client, a
  hung process, or a dead network path can leave a session (and its subprocess) alive
  indefinitely otherwise. A session with **no** SSE subscriber at all remains the pre-existing
  idle-eviction sweep's responsibility (`SessionManager::evict_oldest_inactive_session`), not
  this probe's — pinging it would have nothing to reach.

## 5. Out of Scope

- Implementing the command execution or sandboxing logic directly (delegated to the `ahma serve stdio` subprocess).
