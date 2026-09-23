# Session-Health and Pending-Grant Notifications

**Status:** stable — SPEC R8.8. Everything below is observable behavior pinned by tests;
treat it as a contract.

## Why

The stdio proxy hides a dead bridge session from the MCP client: it rebuilds the
connection and replays the handshake, which is the right default but leaves the client
blind. Likewise, a `sandbox_grant` request can wait on a human surface (TUI modal, CLI
hint) that the client cannot see. These notifications tell a client what happened —
**information only**: they never demand a response, never gate server progress, and
carry no approval authority. A client that ignores them is no worse off.

## Event reference

Method: `notifications/ahma/session_event`. Params envelope:

```json
{ "kind": "<kind>", "timestamp": <unix ms>, "seq": <per-emitter monotonic>, "detail": { } }
```

| `kind` | Mirror level | `detail` fields | Emitted by |
|---|---|---|---|
| `reconnected` | `warning` | `cause` (`"transport_failure"`), `reconnects` (count so far), `message` | stdio proxy, after it transparently rebuilt the bridge session |
| `reconnect_failed` | `error` | `cause`, `attempts`, `message` | stdio proxy, right before it exits; the pipe dies next |
| `grant_pending` | `warning` | `grant_id`, `path`, `access` (`ro`/`rw`), `reason` | server broker, before the question ladder asks |
| `grant_decided` | `warning` | `grant_id`, `outcome` (`granted`/`declined`), `access` (granted only) | server broker, on resolution |
| `health` | `info` | reserved | reserved for future telemetry |

`seq` is monotonic **per emitter** (the proxy and the server count
independently); a gap means you missed events, and the heartbeat fields below
let you re-converge. Two emitters ⇒ do not assume a single global ordering.

## The `notifications/message` mirror

Every event is also sent as a standard MCP logging notification:

```json
{ "method": "notifications/message",
  "params": { "level": "warning", "logger": "ahma.session", "data": { ...same envelope... } } }
```

Note MCP has deprecated the logging primitive (SEP-2577), so the mirror is a
compatibility bridge for today's clients, not the long-term contract — the
canonical `session_event` is.

## Heartbeat convergence fields

`notifications/ahma/heartbeat` params include (both `0` when idle, both
absent from pre-R8.8 servers — treat missing as `0`):

- `pending_grants` — grants currently awaiting a human decision (server-filled)
- `reconnects` — transparent transport rebuilds this session (proxy-overlaid)

## Recipes

**Foreign client (Claude Code, Cursor, any MCP client):** nothing to do.
The mirror arrives as an ordinary logging notification and is surfaced by the
client's existing log/notification UI. To do better, parse `params.data.kind`
from notifications with `logger == "ahma.session"`.

**Ahma-aware client (TUI, custom integrations):** handle the
`notifications/ahma/session_event` method (unknown methods are safe to ignore,
so shipping the handler is backward-compatible with old servers). Suggested
reactions, all optional:

- `reconnected` → toast/log line; requests that were in flight got a JSON-RPC
  error and can simply be re-issued (`status`/`await` tell you whether an
  `op_id` survived).
- `reconnect_failed` → show the message; the connection is about to die, so
  offer a restart.
- `grant_pending` → display "a sandbox grant for `<path>` is awaiting approval".
  To pull the decision into your own UI instead, call the `sandbox_grant` tool
  with the same path — the coordinator's dedup makes the surfaces converge on
  one decision (first answer wins).
- `grant_decided` → clear the corresponding `grant_pending` display.

**What you may never do with events:** treat them as approval authority. A
`grant_decided` event does not grant anything by itself — persistence happened
(or didn't) server-side through the R5.3/R5.4-gated flow; events only report it.
