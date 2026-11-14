# Browser GUI (`ahma gui`)

> **Experimental** (introduced in v0.23). Behaviour and flags may change.

`ahma gui` shows what `ahma tui` shows — every editor session's work, hooked shell commands, a
chat with your model, and the permission questions — in a browser tab. It runs the same session
and the same rules as the terminal UI; only how a screen is drawn and how an answer arrives
differ.

## Why

A terminal is the right place for ahma's controls, but a browser window is often the one already
open beside your editor. A web page is also a larger attack surface than a terminal: anything on
your machine can try to talk to a local port, and a page you visit can try to talk to
`localhost`. So the GUI is built around that: it listens on this machine only, it needs a code
you type from the terminal, it refuses requests that did not come from its own page, and it is
given less authority than the terminal (see [What it will not do](#what-it-will-not-do)).

## Quickstart

```bash
# In your project root: find the per-user hub, listen on a free local port, open a browser
ahma gui

# Print the address and the pairing code only
ahma gui --no-open

# A fixed port, for an SSH port-forward
ahma gui --port 8765 --no-open
```

The terminal prints an address and a **pairing code**. Open the address, type the code into the
page, and the tab is paired. The code works once; each further tab (up to four) needs a fresh
code, which the terminal prints when you open it. A reload pairs again.

To use it from another machine, forward the port over SSH
(`ssh -L 8765:127.0.0.1:8765 host`) and open `http://127.0.0.1:8765/` locally. There is no
option to listen on another address.

## What you see

- **Header**: connection, sync or async mode, sandbox state.
- **Work**: one section per client session, as in the terminal; open one to see its commands and
  their live output, cancel one, or ask for the question about a denied one again.
- **Chat**: with a window's model; stop a running reply.
- **Log**: the ahma log.
- **Permission questions**: sandbox grants, web egress and tool approvals, pinned where they
  cannot be missed. Deny is always available and acts at once. An answer that grants becomes
  available a moment after the question has actually been on screen in a focused tab, so a
  keystroke meant for something else cannot grant it.

Slash commands typed into the chat are the ones the controller allows a browser (see below);
the terminal UI is where settings, setup and the doctor are used.

## What it will not do

The terminal runs as you, outside the sandbox. A browser tab is reached over a socket, so it is
trusted with less. A browser session cannot:

- run a command in the host shell (`!`, `/mcp add stdio`);
- give an answer that is saved to settings or outlives the session (`always`, a lease,
  always-allow for a tool);
- change a setting or apply a proposal, or run a slash command that is not on its allow-list.

Each is refused by the controller where the text is parsed, not by hiding a button, and the
refusal says to use `ahma tui`.

Decisions made in the browser are recorded in the audit log under the surface `gui`.

## Security model

| Layer | What it does |
|---|---|
| Listener | `127.0.0.1` and `[::1]` only; a peer that is not loopback is refused. |
| Pairing | A code printed to the controlling terminal only, single use, retired after 5 wrong tries, pairing locked after 15 until you press Enter at the terminal. A per-tab token is held in page memory only. |
| Request guard | `Host` must be one of the server's own names (defeats DNS rebinding); `Origin` must equal the request's own origin; `Sec-Fetch-Site` must be `same-origin` or `none`. No CORS headers. |
| Page policy | CSP `default-src 'none'` with only `'self'` sources, Trusted Types required, `nosniff`, `no-referrer`, `no-store`. |
| Content | Markdown is parsed in the server into data, never HTML; control and bidirectional-override characters are escaped; links are not followed and images are not fetched. |
| Page code | First-party TypeScript with no runtime dependencies; the compiled files are committed and embedded in the binary. |
| Subscription | The GUI counts as an attached approval surface only while a paired tab is open. |

The browser, and any extension in it that can inject into the page, are inside what is trusted.
Pair from a profile whose extensions you trust. Where the OS has no filesystem boundary yet
(Windows), nothing stops a sandboxed command from reading the hub's token; see
[docs/security-sandbox.md](security-sandbox.md).

## Command reference

| Flag | Meaning |
|---|---|
| `[PATH]` | Sandbox scope for the session (default: the current directory). |
| `--port N` | Port on loopback. `0` (default) picks a free one. |
| `--no-open` | Do not open a browser. |
| `--connect URL` | Server to monitor, as for `ahma tui`; default is the per-user hub. |
| `--profile NAME` | Launch with a specific agent profile. |

`ahma gui` needs a terminal: the pairing code is written there and nowhere else.

## Building the page

The page lives in `ahma_gui/web/` and is plain TypeScript compiled with `tsc`, the only
development dependency. `cargo build` never runs Node. After changing the source:

```bash
cd ahma_gui/web
npm ci --ignore-scripts
npm run build
npm test
```

and commit the result in `ahma_gui/web/dist/`; CI fails if it differs from a fresh compile.

## See also

- [docs/tui.md](tui.md): the terminal UI, which has more commands and the saved answers.
- [docs/permissions.md](permissions.md): how a question is routed and answered.
- [docs/hub.md](hub.md): the per-user hub both surfaces attach to.
- SPEC: root `R-GUI`, `R-UI`; `ahma_gui/SPEC.md`.
