# ahma_gui Crate Specification

* **Status**: Approved (Experimental feature)
* **License**: AGPL-3.0
* **Depends on**: `ahma_tui` (the controller, session and `Frontend` trait), `ahma_core`, `ahma_mcp`, `ahma_common`
* **Used by**: `ahma_bin` (`ahma gui`)

A SPEC states what the crate does now and why. Open work in the whole product is listed once,
in the root SPEC's Known gaps (§11).

## 1. User Story / Problem Statement

*As someone supervising agents, I want the work view, the chat and the permission questions in a
browser tab so that I can watch and answer from a window I already have open, without giving
that window any more authority than the terminal has.*

The terminal UI is the one place a human watches agent work and answers approval questions.
`ahma gui` is a second surface on the same session and the same rules: it is a `Frontend`
(`ahma_tui::session`) over a loopback web server and a page. The rules that decide whether an
answer is taken live once in `ahma_tui::controller` (root R-UI); this crate decides only who
may talk to the server and how a session's state is drawn.

## 2. Acceptance Criteria

Criteria in the root SPEC are cited by id, not restated.

- **Loopback only (R-GUI.1)**: the sockets are `127.0.0.1` and `[::1]` on one port; a
  non-loopback peer is refused.
- **Pairing (R-GUI.2)**: the code is written to the controlling terminal only; a tab holds a
  per-tab token in memory; at most four tabs.
- **Same-origin guard (R-GUI.3)**: Host by name, exact Origin, `Sec-Fetch-Site`, method
  allow-list, all before the token is read.
- **Fixed policy on every response (R-GUI.4)**: CSP with Trusted Types, `nosniff`,
  `no-referrer`, `no-store`.
- **Nothing untrusted is markup (R-GUI.5)**: Markdown is a Rust-built tree; control and bidi
  characters are escaped in the server; the wire takes an allow-listed intent set.
- **Gate safety (R-GUI.6, R-PERM.3.5)**: a grant waits until the tab reports the question
  visible and focused and the arming delay has passed; a saving answer is not offered.
- **Its own audit name (R-GUI.7)**, **subscribes only while watched (R-GUI.8)**,
  **no host commands (R-GUI.9)**.
- **First-party page, committed build (R-GUI.10)**.
- **Disclosure on every start (R-GUI.11)**.
- `ahma gui [PATH] [--port N] [--no-open]` starts the server, prints the address and the
  pairing code on the terminal, opens the browser unless told not to, and stops on Ctrl-C:
  every open event stream is sent `bye` and ends (graceful shutdown would otherwise wait on
  an open tab forever), the session closes its hub MCP session so the hub stops that
  session's worker at once, and the whole stop is bounded. A second Ctrl-C exits at once.

## 3. Requirements Owned Here

The requirement ids are R-GUI in the root SPEC, because they bind any surface like this one.
What follows is how this crate meets them.

### Routes

- `GET /`, `/app.css`, `/favicon.svg`, `/dist/**/*.js`: the embedded first-party files
  (`assets.rs`). Nothing is read from disk at run time.
- `POST /api/pair`: the only route that needs no token; it takes the typed code and returns a
  tab's token (`auth.rs`).
- `GET /api/events`: Server-Sent Events, read by the page with `fetch` so the token travels in
  a header. A full snapshot on connect, then only the regions whose revision changed, and a
  `bye` event when the server stops.
- `POST /api/intent`: one `WireIntent` (`dto.rs`, `deny_unknown_fields`) becomes a controller
  `Intent`; the controller's refusal is returned as a typed error.
- `POST /api/report`: what this tab showed (`GateVisible`, `GateHidden`), which is what starts
  a grant's arming delay (R-GUI.6).

### Threads

The session's state is not `Send`, so it lives on its own thread with a current-thread
runtime; the server talks to it through bounded channels (`frontend.rs`). A slow tab skips
states instead of queueing them. A prompt names the session's own MCP session, so the hub
runs it on the worker scoped to the launch directory even when an editor session is
attached too (root SPEC R-HUB.6).

## 4. Non-Functional Requirements

- **Security**: the request guard, pairing and headers (R-GUI.1–R-GUI.4) are unit tested
  without a socket and again over a real loopback socket (`listen.rs`); a route inventory test
  asserts every `/api` route but `/pair` answers 401 without a token.
- **Page**: `npm test` (Node's built-in runner, no test dependency) covers the pure modules:
  the SSE parser, the reducer and the formatters. The DOM layer has no automated test; no
  browser driver is a dependency (Out of Scope).
- **Speed**: the server sends only changed regions; the page updates only regions whose
  revision advanced, batched per animation frame.

## 5. Out of Scope

- Listening on any non-loopback address, or serving more than one human.
- Anything R-GUI.9 refuses: host commands, settings or proposals, persistent answers.
- A built-in login, accounts or TLS: the person's own SSH forward is the remote story.
- A web framework, bundler, or any runtime package in the page.
- Automated tests in a real browser.
