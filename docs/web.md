# ahma in the browser (`ahma web`)

> **Experimental** (introduced as `ahma gui`; renamed `ahma web` when it started returning to
> the prompt). Behaviour and flags may change.

`ahma web` shows what `ahma tui` shows — every editor session's work, hooked shell commands, a
chat with your model, and the permission questions — in a browser tab. It runs the same session
and the same rules as the terminal UI; only how a screen is drawn and how an answer arrives
differ.

## Why

A terminal is the right place for ahma's controls, but a browser window is often the one already
open beside your editor. A web page is also a larger attack surface than a terminal: anything on
your machine can try to talk to a local port, and a page you visit can try to talk to
`localhost`. So it is built around that: it listens on this machine only, it needs a single-use
code only the person who started it was given, it refuses requests that did not come from its
own page, it stops by itself when no tab is using it, and it is given less authority than the
terminal (see [What it will not do](#what-it-will-not-do)).

## Quickstart

```bash
# In your project root: start ahma for this directory, open your browser, return to the prompt
ahma web

# Print the address and the code only; type the code instead of pasting it
ahma web --no-open --no-copy

# Stay attached to this terminal, on a fixed port, for an SSH port-forward
ahma web --port 8765 --no-open --foreground
```

`ahma web` makes sure the per-user hub is running, starts its server in the background, opens
your browser and returns to the prompt. The terminal shows the address and a **pairing code**,
and the code is also **copied to your clipboard**: paste it into the page (⌘V or Ctrl+V) and the
tab pairs on its own. The chat then talks to an ahma session scoped to the directory you started
it in.

The code works once and for two minutes. Once used, it is cleared from the clipboard (unless you
have copied something else since). A reload, or another tab, means running `ahma web` again:
each run is its own server with its own code.

The server stops by itself a minute after its last tab closes (`--idle-timeout`), or a minute
after it started if no tab ever paired. Stopping closes its session, so the hub ends that
session's worker too.

To use it from another machine, forward the port over SSH
(`ssh -L 8765:127.0.0.1:8765 host`), run `ahma web --port 8765 --no-open --foreground` there,
and open `http://127.0.0.1:8765/` locally. There is no option to listen on another address.

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

Decisions made in the browser are recorded in the audit log under the surface `web` (records
written before the rename say `gui`, and are read as `web`).

## Security model

| Layer | What it does |
|---|---|
| Listener | `127.0.0.1` and `[::1]` only; a peer that is not loopback is refused. |
| Pairing | An 8-character code, single use, good for two minutes, retired after 5 wrong tries. It is shown on the controlling terminal and, unless `--no-copy`, put on the clipboard; never in a URL, argv, an environment variable or a file. A per-tab token is held in page memory only. |
| Clipboard | Other programs of yours can read the clipboard, so: ahma's macOS sandbox denies the pasteboard to every command it runs; the code is cleared from the clipboard once used and when the server stops; and if the code is used a second time — someone else got it first — every tab is disconnected and the server stops. On Linux and Windows, ahma's sandbox cannot keep a command from the clipboard: use `--no-copy` where that matters. |
| Lifetime | In the background, 15 wrong codes or an expired code stop the server (nobody is there to unlock it); with `--foreground`, Enter at the terminal unlocks and a new code is shown there. The server stops after `--idle-timeout` seconds with no tab connected. |
| Request guard | `Host` must be one of the server's own names (defeats DNS rebinding); `Origin` must equal the request's own origin; `Sec-Fetch-Site` must be `same-origin` or `none`. No CORS headers. |
| Page policy | CSP `default-src 'none'` with only `'self'` sources, Trusted Types required, `nosniff`, `no-referrer`, `no-store`. |
| Content | Markdown is parsed in the server into data, never HTML; control and bidirectional-override characters are escaped; links are not followed and images are not fetched. |
| Page code | First-party TypeScript with no runtime dependencies; the compiled files are committed and embedded in the binary. |
| Subscription | The page counts as an attached approval surface only while a paired tab is open. |

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
| `--no-copy` | Do not copy the pairing code to the clipboard; type it from the terminal. |
| `--foreground` | Stay attached to this terminal: stop on Ctrl-C, show a code for another tab here, unlock pairing with Enter. |
| `--idle-timeout SECONDS` | Stop after this long with no tab connected (default 60; `0` never). |
| `--connect URL` | Server to monitor, as for `ahma tui`; default is the per-user hub. |
| `--profile NAME` | Launch with a specific agent profile. |

`ahma web` needs a terminal or a clipboard to give you the code; `--foreground` needs a terminal.
`ahma gui` was this command's old name and now says so. The web-egress policy commands that
used to be `ahma web allow|deny|…` are `ahma egress …`.

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

- [docs/desktop.md](desktop.md): the same page in a window of its own, with no pairing.
- [docs/tui.md](tui.md): the terminal UI, which has more commands and the saved answers.
- [docs/permissions.md](permissions.md): how a question is routed and answered.
- [docs/hub.md](hub.md): the per-user hub both surfaces attach to.
- [docs/security-sandbox.md](security-sandbox.md): the macOS clipboard denial (R6.2.15).
- SPEC: root `R-GUI`, `R-UI`, R6.2.15; `ahma_gui/SPEC.md`.
