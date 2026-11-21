# ahma in a window (`ahma desktop`)

> **Experimental.** Behaviour and flags may change. macOS and Windows; elsewhere it opens
> [`ahma web`](web.md).

`ahma desktop` opens the same view as [`ahma web`](web.md) — every editor session's work, hooked
shell commands, a chat with your model for the current directory, and the permission
questions, a web link a hooked command was refused among them ([open-links.md](open-links.md))
— in a window of its own instead of a browser tab.

## Why

A browser tab is shared with everything else in the browser: its extensions, its other tabs,
its history. A window of ahma's own has none of that, and because the window and the server are
one process there is no pairing code to hand over: nothing else can pair with it at all. It
uses the web engine that is part of the operating system (WKWebView on macOS, WebView2 on
Windows), so it adds no browser engine to ahma's download.

## Quickstart

```bash
# In your project root: open a window for this directory, and return to the prompt
ahma desktop

# Stay attached to this terminal until the window closes
ahma desktop --foreground
```

Close the window to stop it: the server and its hub session go with it.

On Linux, where the operating system has no web engine of its own (WebKitGTK is a library that
servers and many desktops lack), and on a Windows machine without the WebView2 runtime,
`ahma desktop` says so and runs `ahma web` instead.

## How it is kept safe

| Layer | What it does |
|---|---|
| Pairing | None is offered. The server mints the window's token before it opens and refuses every pairing code, so a process that finds the port gets nothing. |
| Token | Handed to the page by a script the engine runs before the page's own code, only on the server's own origin, as a frozen property. Never in a URL, argv, a file or the environment. |
| Navigation | The window stays on its own server. Any other address, and any request for a new window, opens in your default browser instead. Developer tools are off. |
| Everything else | As for `ahma web`: loopback listener, same-origin guard, fixed page policy, server-side Markdown, the same limits on what the page may do. Decisions are audited under the surface `desktop`. |

## Command reference

| Flag | Meaning |
|---|---|
| `[PATH]` | Sandbox scope for the session (default: the current directory). |
| `--foreground` | Stay attached to this terminal until the window closes. |
| `--connect URL` | Server to monitor, as for `ahma tui`; default is the per-user hub. |
| `--profile NAME` | Launch with a specific agent profile. |

## See also

- [docs/web.md](web.md): the same view in your browser, and the security model they share.
- [docs/tui.md](tui.md): the terminal UI.
- SPEC: root `R-DESK`, `R-GUI`; `ahma_gui/SPEC.md`.
