# ahma_http_bridge Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common` (dev: `ahma_mcp`, `ahma_test_support`)
* **Used by**: `ahma_mcp` (`ahma serve http|unix` and the per-user daemon embed it); the
  standalone `ahma_http_bridge` binary exists for end-to-end tests

## 1. User Story / Problem Statement

*As an MCP client that speaks Streamable HTTP — or as several clients sharing one per-user
daemon — I want each session to get its own sandboxed ahma worker, so that one client's
scope, operations and failures can never reach another's.*

The bridge terminates HTTP/SSE and multiplexes sessions. For each `Mcp-Session-Id` it runs
one `ahma serve stdio` subprocess, and that subprocess — not the bridge — executes commands
and commits the sandbox scope. The bridge's job is protocol fidelity, session lifecycle and
honest gating.

## 2. Acceptance Criteria

### R8: Streamable HTTP transport

- **R8.1**: HTTP bridge mode via `ahma serve http`. `GET /health` answers liveness without authentication.
- **R8.2**: SSE at `/mcp` (GET) for server-to-client notifications.
- **R8.3**: JSON-RPC via POST at `/mcp`. Protocol conformance details that bind both POST transports:
  - **R8.3.1**: Notifications (id-less messages) are answered **HTTP 202** with no JSON-RPC body.
  - **R8.3.2**: An unknown or terminated `Mcp-Session-Id` is answered **HTTP 404**, which is the signal a spec-conforming client (rmcp included) uses to drop the stale session and re-`initialize`. It must not be 403 — clients treat that as terminal.
  - **R8.3.3**: JSON-RPC batch arrays are rejected with **HTTP 400** (batching was removed from the MCP spec in 2025-06-18; forwarding a raw array to the subprocess is undefined behavior).
  - **R8.3.4**: Requests carrying a non-loopback `Origin` header are rejected (server-side DNS-rebinding validation per the MCP transport security requirements; CORS headers alone only gate what a browser lets a page *read*, not what the server *executes*). Non-browser clients send no `Origin` and are unaffected.
  - **R8.3.5**: **`MCP-Protocol-Version` header** (2025-06-18 Streamable HTTP): the bridge validates the header when present — an unsupported value gets HTTP 400 naming the supported set — and assumes `2025-03-26` when absent (the spec's backwards-compatibility rule, which is also what keeps every pre-2025-06-18 client working). ahma's own first-party HTTP clients (the stdio proxy, the TUI, the external-tool client) negotiate the current revision at `initialize` and echo the **server-answered** version in the header on every subsequent request, via the one shared implementation in `ahma_common::mcp_protocol` — protocol currency must not be re-implemented per client.
  - **R8.3.6**: HTTP `DELETE /mcp` with a valid `Mcp-Session-Id` terminates that session (its subprocess is stopped) and is answered **HTTP 204**; a missing header gets 400, an unknown id gets the R8.3.2 404.
  - **R8.3.7**: **A JSON-RPC id already in flight on a session is refused, never allowed to orphan the earlier request.** Several HTTP clients may share one `Mcp-Session-Id` (the TUI's status source and its agent's tool calls, for instance), so the bridge cannot trust ids to be unique per session. A second request reusing an in-flight id is answered **HTTP 400** / JSON-RPC `-32600` naming the id; the first request is untouched. Replacing the first request's response slot instead dropped its channel, so a healthy request failed instantly with HTTP 500 "Response channel closed" — which is how every parallel pair of agent tool calls used to fail. ahma's own clients mint ids from one process-wide counter, so they never trip this.
- **R8.4**: **Subprocess death fails the session loudly; there is no auto-restart.** A crashed subprocess answers its session's in-flight requests with a classified error (R-SIGN.5) and the session terminates; the client re-initializes into a fresh session via R8.3.2's 404.
- **R8.4.1**: **Stateful by design — stateless mode is out of scope.** Every session is bound to a live subprocess holding a kernel sandbox lock (R5.1); a scope commit cannot be stateless, so the Streamable HTTP spec's optional stateless-server mode is deliberately not implemented. Conformance effort goes into the *stateful* session lifecycle instead: session header, DELETE termination, 202/404 semantics above.
- **R8.5**: Content negotiation via `Accept` header (`text/event-stream` → SSE, `application/json` → JSON).
- **R8.6**: **HTTP Streaming (MCP Streamable HTTP)**: POST requests support SSE response streaming for full multiplexing and reconnection resilience.
  - **R8.6.1**: POST with `Accept: text/event-stream` returns SSE-formatted response and interleaved server notifications within a single stream.
  - **R8.6.2**: Per-session SSE event IDs (`id:` field) enable ordering and deduplication. Each JSON-RPC response and notification receives a monotonically-increasing session-unique ID.
  - **R8.6.3**: Event history buffer maintains recent events (bounded to 1000 events per session) for `Last-Event-Id` replay support.
  - **R8.6.4**: GET requests with `Last-Event-Id: N` replay all events with ID > N from the per-session history buffer, enabling seamless reconnection after temporary network loss.
  - **R8.6.5**: Event IDs are independent per session and start at 1. Event history is cleared when the session ends.
- **R8.7**: **HTTP/3 (QUIC) Client Preference**: All HTTP clients built with `reqwest` use the `http3` feature to prefer HTTP/3 (QUIC) transport when the server advertises support via Alt-Svc headers.
  - **R8.7.1**: HTTP/3 uses QUIC (UDP-based) for reduced connection latency and improved multiplexing compared to HTTP/2 over TCP.
  - **R8.7.2**: Transparent fallback to HTTP/2 or HTTP/1.1 when the server does not support HTTP/3.
  - **R8.7.3**: **Known limitation**: the GET SSE stream is **not served over HTTP/3** — the QUIC endpoint answers it with 406 Not Acceptable and an explanatory body, and clients fall back to HTTP/2 or HTTP/1.1 for the push channel. POST request/response works over HTTP/3.
- **R8.8**: **Session-Health Disclosure** (client reference: `docs/session-health-notifications.md`): structured server→client disclosure of session-health changes the client cannot otherwise observe. Events are **information only** — they never demand a response, never gate server progress, and emission failure must never fail or block the operation that triggered the event.
  - **R8.8.1**: Canonical event notification `notifications/ahma/session_event` with envelope `{kind, timestamp, seq, detail}`; `seq` is per-emitter monotonic so a client can detect gaps. Kinds: `reconnected`, `reconnect_failed`, `grant_pending`, `grant_decided`, `health`.
  - **R8.8.2**: Every event is mirrored as a standard `notifications/message` logging notification (`data` = the event params; level `error` for `reconnect_failed`, `warning` for reconnect/grant kinds, `info` for `health`) so foreign clients surface the disclosure with zero ahma-specific code. The mirror uses the standard wire shape directly, because MCP deprecates the typed logging API (SEP-2577).
  - **R8.8.3**: The stdio proxy — the only party that knows a transparent reconnect happened — synthesizes `reconnected` after a successful rebuild and a terminal `reconnect_failed` before exiting on exhaustion, **downstream only**: session events must never reach the (fresh) bridge session, mirroring how the replayed handshake never reaches stdio.
  - **R8.8.4**: The `notifications/ahma/heartbeat` payload carries `pending_grants` (grants awaiting a human decision, filled by the server from the `GrantCoordinator`) and `reconnects` (overlaid by the proxy — the server behind it cannot know). Both fields are `#[serde(default)]` and wire-compatible in both directions with pre-R8.8 peers.
  - **R8.8.5**: The permission broker emits `grant_pending` (with `grant_id` = the coordinator's `decision_id`) once per deduped `(path, access)` before the question ladder asks, and `grant_decided` (`granted`/`declined`) on resolution — **beside**, never instead of, the human asking surfaces. The R5.3/R5.4 grant security gates and session scope-immutability are unaffected: disclosure carries no approval authority.

### R10: Session isolation

- **R10.1**: Every HTTP-served MCP session gets its own `ahma serve stdio` subprocess with its own sandbox scope. This is unconditional — there is no flag and no shared-process mode.
- **R10.2**: Session ID (UUID) generated on `initialize`, returned via `Mcp-Session-Id` header.
- **R10.3**: Sandbox scope is resolved per the R5.2 source precedence (explicit → `roots/list` → elicitation → auto-narrowed container root) and committed through the single atomic compare-and-swap of R5.1.1. Each session's dedicated subprocess owns and commits its own scope (R5.1): an explicit bridge-level `--sandbox-scope` locks every session to the same operator-chosen value; otherwise each session derives its scope from its own client's `roots/list` answer, in full isolation from every other session. A client that declares no `roots` capability at `initialize`, on a bridge with no fallback scope, has its sandbox failed at once, so `tools/call` gets a deterministic, remediated refusal instead of a handshake timeout.
- **R10.4**: Once committed, the instance sandbox scope **cannot** be changed (security invariant; R5.1).
- **R10.5**: `roots/list_changed` after sandbox lock is a **tolerated no-op**: the committed instance scope is immutable and can never be widened (R5.1 / R5.1.1 / R5.2.2), so the notification is acknowledged with success, **not** forwarded to the subprocess, and the session is **kept alive**. The server **must not** widen, narrow, or re-derive scope from it, and **must not** terminate the session. Real clients re-emit it routinely; sandbox escape is prevented by the immutability of the commit, not by tearing down the session. Any actual scope-widening is rejected at the single commit point (R5.1.1) — there is no second path to widen scope after lock.
- **R10.6**: **Client Response Mapping**: The HTTP bridge MUST keep track of server-to-client JSON-RPC requests (such as `roots/list` and `sampling/createMessage`) by recording their request IDs. When a client sends a JSON-RPC response with a `result` field, the bridge MUST only process it as a `roots/list` response (and lock the sandbox) if its request ID matches an outstanding `roots/list` request. Client responses to other methods (e.g. keepalive pings or sampling) MUST NOT trigger roots-parsing or sandbox-locking logic, and MUST NOT generate invalid roots warnings or errors.

### Transport security

- **Bearer authentication** is on when a token is configured (`--require-token <token>`,
  `--require-token-path <file>`, or `[auth]` in settings). Every route except `/health`
  then needs `Authorization: Bearer <token>` (scheme case-insensitive); the comparison is
  constant-time; `SIGHUP` re-reads the token file without a restart. Without a token there
  is no authentication, and binding a non-loopback address warns loudly.
- **Per-IP rate limiting** (`--rate-limit-rps`, `--rate-limit-burst`) answers excess
  requests with HTTP 429 and `Retry-After`; `/health` is exempt.
- **Listeners**: TCP (`ahma serve http`), a Unix socket (`ahma serve unix`, Unix only), and
  the per-user daemon's endpoint (R-DAEMON). An explicitly started `serve http|unix` is
  operator-owned and has no idle exit unless `--idle-timeout` is given.

### RB: Pipeline and state invariants

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
  indefinitely otherwise. A session with **no** SSE subscriber at all is the
  idle-eviction sweep's responsibility (`SessionManager::evict_oldest_inactive_session`), not
  this probe's — pinging it would have nothing to reach.

## 3. Non-Functional Requirements

- **Dual-transport parity**: every behaviour of `POST /mcp` holds identically for
  `application/json` and `text/event-stream` (RB.1); tests cover both (AGENTS.md R15.5).
- **Session cap**: at most `--max-sessions` concurrent sessions (default 50); the oldest
  inactive session is evicted first.

## 4. Out of Scope

- Executing commands or enforcing the sandbox — both belong to the `ahma serve stdio`
  subprocess (`ahma_mcp`).
- Stateless Streamable HTTP (R8.4.1).
