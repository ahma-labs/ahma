# Session-Health and Pending-Grant Notifications — Design (#485)

**Status:** P1 (proxy reconnect disclosure) and P2 (grant events + heartbeat
fields) implemented; P3 (client integration guidance, SPEC rows) pending
**Issue:** [#485](https://github.com/paulirotta/ahma/issues/485)
**Related:** #479 (transparent proxy reconnect), SPEC R5.3/R5.4 (grant flow), R8.4 (bridge sessions)

## 1. Problem

#479 made bridge-session death invisible to the MCP client: the stdio proxy
(`ahma_mcp/src/shell/modes/proxy_client.rs`) caches the client's `initialize`,
`notifications/initialized`, and `roots/list` answer, and on a dead transport it
rebuilds the bridge connection and replays the handshake without the downstream
client (Claude Code, Cursor, …) ever seeing it. That is the right default — but it
leaves the client with exactly two health signals, both bad:

- a generic JSON-RPC error (`-32002` and friends) on a single request, or
- a dead pipe, if reconnection exhausts its attempts.

There is no structured way for ahma to tell a connected client:

1. "your session was rebuilt transparently, and here's why" (reconnects);
2. "a sandbox grant is awaiting a human decision" (the `sandbox_grant`
   preview/confirm flow can park a request on a human surface — TUI modal,
   CLI hint — with the MCP client completely blind to it);
3. general session-health telemetry a capable client could react to.

## 2. What already exists (build on it, don't invent beside it)

The codebase already has a **custom notification family** and three transport
paths that matter:

| Mechanism | Where | Notes |
|---|---|---|
| `notifications/ahma/heartbeat` | `mcp_service/mod.rs` (`KeepAlive::send_enhanced_heartbeat`) | Periodic; payload is `{version, hash, timestamp}` (`ahma_common::keepalive::HeartbeatPayload`). Ahma peers consume it (`is_ahma_peer`); foreign clients ignore it. |
| `notifications/sandbox/configured` / `failed` | `mcp_service/config_watcher.rs` | Emitted on stdout at sandbox lock/failure, with a structured scope summary (`sandbox/display.rs`). |
| `notifications/sandbox/terminated` | `shell/modes/server.rs` | Emitted on session teardown. |
| `notifications/progress` push | `mcp_service/progress_push.rs` | Long-running op output relay. |
| MCP `elicitation/create` | `handlers/sandbox_grant_tool.rs` | Already used **on the tool-call path** to put a grant decision in front of an external client that supports elicitation. |
| `ScopeGrantNotifier` / `GrantCoordinator` | `sandbox/grant_channel.rs`, `ahma_common::scope_grant` | The denial→approval-surface channel, with per-session dedup/debounce. Surfaces plug in as trait impls (TUI modal, logging stub today). |

Two structural facts drive the design:

- **Only the proxy knows a reconnect happened.** The rebuilt bridge session is
  brand-new and unaware; the client was deliberately kept unaware. Any
  reconnect disclosure must therefore be *synthesized by the proxy itself*.
- **A pending grant lives server-side** (in `GrantCoordinator`'s dedup state and
  whatever surface the notifier delivered to). The MCP client is only informed
  today if *it* was the elicitation surface. When the human surface is the TUI
  or a CLI hint, the client that triggered the denial learns nothing.

## 3. Design questions from #485, answered

### 3.1 Which MCP primitive?

**Recommendation: a layered fan-out from one internal event type — no protocol
extension, no capability negotiation, no version bump.**

MCP already permits server→client notifications with custom methods; unknown
methods are ignored by compliant clients (JSON-RPC notification semantics), so
custom notifications degrade safely. The repo already relies on this for
`notifications/ahma/heartbeat` and `notifications/sandbox/*`. The design adds
**one** new method and reuses two existing channels:

1. **`notifications/ahma/session_event`** (new, canonical, structured) — for
   ahma-aware peers (the ahma TUI, the proxy, future ahma-aware clients).
   Payload:

   ```json
   {
     "kind": "reconnected | reconnect_failed |
              grant_pending | grant_decided | health",
     "timestamp": 1789000000000,
     "seq": 42,
     "detail": { }
   }
   ```

   `seq` is a per-emitter monotonic counter so a client can detect gaps after
   its own outages. `detail` is kind-specific (below).

2. **`notifications/message` mirror** (existing MCP logging primitive) — every
   `session_event` is also emitted as a standard logging notification (level
   `warning` for `reconnected`/`grant_pending`, `error` for
   `reconnect_failed`) with the same JSON in `data`. This is the passive
   channel for foreign clients: Claude Code and Cursor surface logging
   notifications today with zero ahma-specific code. `daemon_reporter.rs`
   already emits `notifications/message`; reuse its serialization.

3. **Heartbeat piggyback** (existing) — `HeartbeatPayload` gains optional
   fields so slow-polling ahma peers converge even if they missed a
   notification:

   ```rust
   pub struct HeartbeatPayload {
       pub version: String,
       pub hash: String,
       pub timestamp: u64,
       // new, all optional/defaulted for wire compatibility:
       pub pending_grants: u32,      // grants awaiting a human decision
       pub reconnects: u32,          // proxy reconnects this session
   }
   ```

   Serde defaults keep old/new peers wire-compatible in both directions.

**Rejected alternatives:**

- *A subscribable resource (`ahma://session/health`) with
  `notifications/resources/updated`.* Cleanest "state, not events" model, but
  client support for resource subscription is spotty, it forces a
  read-back round trip on every change, and ahma's server currently exposes
  tools, not resources. Revisit only if a state-snapshot consumer appears
  (deferred, not precluded — the internal event bus below leaves room).
- *A new bidirectional request (server→client)*. Requires capability
  negotiation and blocks on client response; nothing here needs an answer.
  Where an answer *is* needed (grant decisions) MCP already has the right
  primitive — elicitation — and `sandbox_grant` already uses it.
- *Overloading `notifications/progress`.* Progress is scoped to an in-flight
  operation token; session health is not operation-scoped.

### 3.2 What should a client *do* with it?

**Passive disclosure is the contract; actionability is opt-in where a stable
handle exists.**

- `reconnected` — disclosure only (log line / toast). The event's `detail`
  carries `{cause, reconnects, message}`. Requests in flight when the
  transport died were each answered with a JSON-RPC error, and the client (or
  its user) can re-issue; the existing `status`/`await` tools already let a
  client check whether an `op_id` survived. No new actionable surface is
  required.
- `reconnect_failed` — terminal disclosure (level `error`) emitted *before*
  the proxy exits, so the pipe death that follows is at least explained. A
  capable client may prompt the user to restart the server; a passive one
  shows the log line.
- `grant_pending` — the actionable case. `detail` carries
  `{grant_id, path, access, risk, surface}`. A capable client can:
  - surface "a sandbox grant for `<path>` is awaiting approval in `<surface>`"
    to its user (minimum, passive), or
  - call the existing `sandbox_grant` tool with the same path to pull the
    decision into its own UI via elicitation (actionable, no new API — the
    `GrantCoordinator` dedup makes the two surfaces converge on one decision).
- `grant_decided` — `detail` `{grant_id, outcome: granted|declined|timeout}`;
  closes the loop so a client that displayed `grant_pending` can clear it.

The rule: **events never demand a response and never gate server progress.**
A client that ignores everything is exactly as well off as today.

### 3.3 Interaction with the sandbox grant flow

The grant flow is the concrete beneficiary. Changes are confined to the
existing seam — `ScopeGrantNotifier` / `GrantCoordinator`:

1. `GrantCoordinator` assigns each deduped pending request a `grant_id` and
   exposes a `pending()` snapshot (feeds both `grant_pending` events and the
   heartbeat `pending_grants` count).
2. A new `McpSessionEventNotifier` implements `ScopeGrantNotifier` by emitting
   `grant_pending` through the session-event emitter — *in addition to*, not
   instead of, the existing surface delivery (TUI modal, elicitation, log).
3. Decision paths (`persist_grant` on approval; decline/timeout in the
   notifier) emit `grant_decided`.
4. Security invariants are untouched: events are **information only**. The
   three gates in `sandbox_grant_tool.rs` (denylist, human decision, two-phase
   confirm) still stand; a grant still only takes effect on next server start
   (R5.4.7); scope immutability during a session (R5.4) is not weakened by
   telling a client that a grant is pending.

Privacy note: `grant_pending.detail.path` names a path *outside* the sandbox.
That path was already disclosed to the same client in the `sandbox_denial`
error payload that started the flow, so the event discloses nothing new — but
events must never carry file *contents*, environment values, or settings-file
text.

## 4. Emitter topology

One internal `SessionEvent` enum in `ahma_common` (serde-serializable, the
single source for `kind`/`detail` schemas), with two emitters:

- **Server-side emitter** (in `AhmaMcpService`, next to the keepalive): owns
  `seq`, fans out to `session_event` + `notifications/message` mirror, and
  feeds the heartbeat counters. Used by the grant notifier and (already
  structurally present) the sandbox lifecycle notifications, which stay
  as-is for compatibility but gain mirrored `session_event`s
  (`kind: "health"`).
- **Proxy-side emitter** (in `proxy_client.rs`): on successful replay,
  injects a `reconnected` event *downstream to the client only* (it must not
  reach the new bridge session, mirroring how the replayed handshake never
  reaches stdio); on final failure, injects `reconnect_failed` before exit.
  The proxy also increments a local reconnect counter that it *overlays* onto
  forwarded heartbeats (`reconnects` field) — the server behind it cannot know.

Stdout writes follow R5.6.1: all stdio emission goes through
`emit_stdout_notification` (broken pipe → debug, non-fatal).

## 5. Rollout

Three landable slices, each independently useful:

- **P1 — reconnect disclosure (proxy only).** `SessionEvent` type +
  proxy-side emitter + `notifications/message` mirror. Closes the original
  dogfooding gap: a rebuilt session announces itself. No server changes.
- **P2 — pending-grant events.** `grant_id` + `pending()` on
  `GrantCoordinator`, `McpSessionEventNotifier`, `grant_decided` on the
  decision paths, heartbeat payload fields. TUI shows pending count from
  heartbeats.
- **P3 — client guidance + SPEC.** Document the event schema for client
  integrators (Claude Code / Cursor config recipes), add the SPEC requirement
  rows (proposed as an R8.6 "session-health disclosure" block; final numbering
  at implementation time), and integration tests asserting: reconnect emits
  exactly one `reconnected` event downstream and zero upstream; `grant_pending`
  fires once per deduped path; foreign-client mode (no ahma peer) still sends
  the `notifications/message` mirror.

## 6. Test plan (per Hard Invariants)

- Proxy reconnect test extension: after a forced bridge kill + successful
  replay, assert the client-side transcript contains one
  `notifications/ahma/session_event` with `kind: "reconnected"` and that the
  bridge-side transcript contains none.
- Grant-flow test: trip a denial with the TUI surface active, assert
  `grant_pending` is emitted with the same `grant_id` that later appears in
  `grant_decided`, and that `pending_grants` in the next heartbeat reflects it.
- Wire-compat test: old-format `HeartbeatPayload` (three fields) deserializes
  under the new struct; new payload deserializes under a three-field struct.
- Negative test: `session_event` emission failure (closed pipe) must not fail
  the operation that triggered it.
