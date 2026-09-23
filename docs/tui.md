# TUI Control Plane

`ahma tui` opens a terminal dashboard showing everything ahma is doing on your
behalf — every editor session's work, every hooked shell command, and the
commands you run yourself — in one view. It works over SSH, needs no graphical
runtime, and is where approval gates are answered.

It is **work-first**: the view you open into is what is being done for you.
Chat is a thing you then choose to do (`i`, or `/chat`), and the log pane
(`/log`) and sandbox scope panel (`/scope`) are toggles.

## Quickstart

```bash
# In your project root — attaches to everything already running for this project
cd ~/my-project
ahma tui

# Connect to a custom address
ahma tui --connect http://localhost:8080
```

## One section per client session

Open `ahma tui` in a project directory while your editors are working and their
work is **already there** (SPEC R24.2): every ahma instance reports to the
per-user daemon, which replays recent history — with true start and end times,
and the output each command was printing — the moment the TUI subscribes. If
there is live work for this project, its section opens by itself; any keystroke
takes over.

```
 ahma · work · this project [f] 3 clients 2⟳ 1◷ 14✓   sync ● ahma ● daemon · Unix socket · sandbox: …/github/ahma [ENFORCED]
▶─ claude-code (1) · …/github/ahma · qwen2.5-coder · ctx 38% (49k/128k) · ↑52k ↓6.1k ── ⢷⡪ ─── 2⟳ 1◷ 14✓ ──
   ⟳ cargo nextest run              [op_41]  1m12s        [P] [X]
   │ Compiling ahma_core v0.15.4
   │ Compiling ahma_mcp v0.15.4
   ▾ session build-loop
     ✓ cargo fmt --all              [op_39]  exit 0 · 0.3s
── claude-code (2) · …/github/ahma ── ✓ cargo build ───────────── 8✓ ──
── cursor · …/proj-b ── ⟳ npm test ──────────────────────── 1⟳ 3✓ ──
── hooks · …/github/ahma ── ✓ pre-commit lint ───────────── 1⟳ 2✓ ──
── this terminal (you) ── ! rm -rf build ───────────────────── 1✓ ──
 ↑↓ move  Enter open  Space tail  f all projects  i chat  ? help  q quit
```

- **One status header, in every layout.** Left: the project filter, clients and
  task tallies. Right: the execution mode (`sync`/`async`, see
  [settings](settings.md#sync-or-async-toolsexecution_mode)), whether the ahma
  server and the per-user daemon are reachable — spelled out with how long, e.g.
  `○ daemon OFFLINE 12s`, when one is not — the transport, and the sandbox the
  server actually locked.
- **Each window shows its own LLM and spend.** A section's rule names the model
  Enter will chat with there and, once it has been used, its context fill and
  tokens in/out. The chat input's title carries the same meter for the window
  you are typing to, plus the live tokens/second and elapsed time while a turn
  streams. Context fill needs the model's window size (`--context-length`, or
  `num_ctx` for the provider in `~/.ahma/config.toml`); without one it is left
  out rather than guessed. Providers that report no usage get a `~N tok est`.
- **A section per client session.** Two windows of the same editor on the same
  project are numbered, so you can tell them apart. Hooked commands fold into
  one `hooks` section — a hook is one instance per command — and your own `!`
  commands and chat tool calls are *this terminal (you)*.
- **`!` commands are marked.** Anything you run with `!` runs outside the
  sandbox, at your full privilege, and its row carries a `!` and its detail
  pane says `UNSANDBOXED`. It is reported to the daemon like any other work, so
  it is in the history, and a second TUI sees it too.
- **A closed section still tells you something**: what it is running now, or
  what it last ran. You should not have to open each one to find the one you
  want.
- **One section is open at a time.** Click a header (or press Enter on it) and
  it opens while the previous one closes, over about a third of a second, so you
  can see which line went where.
- **Inside the open section**, children indent under whatever spawned them, and
  one task at a time expands into its output: the live tail if it is running,
  the retained tail or result if it has finished. Enter opens the full-screen
  detail view instead.
- **Project-scoped by default**; `f` shows every project. A session that has not
  established its scope yet reads *no scope yet* rather than disappearing.
- **Recent work survives.** Finished work stays for an hour — including work
  from a session that has since closed, and from a daemon that has since exited,
  because the daemon writes a bounded history beside its sockets, in the
  per-user runtime directory. An
  operation that was still running when its daemon went away is shown
  `interrupted`, not failed: nobody established that it failed.

## Operation names and exit status

Every operation row shows **what actually ran**, not an internal id:

```
⟳ cargo nextest run -p ahma_core   [op_41]  exit 0 · 41s
✗ touch /etc/foo                   [op_42]  denied: outside sandbox scope
```

The name is computed **by the server**, where the command is known, and sent on
the wire (SPEC R24.7) — the TUI renders it rather than guessing. That is why rows
are meaningful even when you open `ahma tui` *after* your IDE has already been
working: the replayed history carries the same names and exit codes as the live
events, so a late-attached TUI shows what those commands were, not what their ids
looked like.

A finished row shows `exit 0` / `exit 101` where the command was a process. An
operation that was cancelled or timed out has no exit code, and says so, rather
than showing a fabricated `exit 0`.

## Key bindings

The in-app help (`?`) and the footer are generated from the same table the key
bindings are tested against, so what they say is what the keys do. The most
useful ones:

| Key | Action |
|-----|--------|
| Ctrl-C | Cancel the running chat turn; press again (within 2 s) to quit |
| `q` (in a pane) | Quit — asks you to press it again while operations or a turn are still running |
| `Esc` (chat input) | Clear the input; on an empty input, cancel the running turn |
| `↑`/`↓` (`j`/`k`) | Move the selection |
| `Enter` / click a header | Open that section and chat with that window |
| `Space` / click a task | Expand it into its output (one at a time) |
| `Enter` on a task | Full-screen operation detail |
| Wheel | Scroll the view |
| `i` or `/chat` | Open or close the chat pane |
| `f` | This project / all projects |
| `c` | Cancel the selected operation |
| `p` | Pin the selected operation |
| `a` | Ask for access again (on a denied operation) |
| `Tab` | Cycle panes |
| `y` / `a` / `n` | Approve / always allow / reject a pending gate. While you are typing in the chat input these keys type; press `Esc` to clear the input first |
| `/` | Command navigator (from an empty input) |
| `?` | Help |
| `↑`/`↓` (chat input) | Earlier / later message, from the first / last line |
| Click a tool call in chat | Open its full arguments and result |

## Chatting

- **Connect an LLM with `/setup`** (also offered when you first chat with a
  window). It checks the provider before saving anything: the model list is
  fetched from the provider — that is the connection test — and the models you
  pick from are the ones it actually has. Cloud providers read their key from
  the environment (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`); the key is never typed
  into the TUI or stored — the provider is registered in `~/.ahma/config.toml`
  with a `${VAR}` reference. A window whose client supports MCP sampling can also
  answer with the client's own model. Any other OpenAI-compatible server:
  `/provider add`.
- **Each window has its own conversation.** Switching windows switches
  transcript, so a question for one client's model is never sent to another.
  After each turn the conversation is saved under `~/.ahma/transcripts/` (not in
  the project, so it cannot be committed by accident); `/resume` brings it back
  in a later session.
- **Replies are rendered Markdown**: headings, lists, emphasis, and code blocks
  that read as code.
- **A paste lands where you can see it**: in the log filter when that is open,
  otherwise in the chat input, which opens and takes focus.
- **Window commands need their slash**: `/x3` closes window 3; a message that
  happens to start `x3` is sent as a message.
- `/compact` keeps the last four turns so the model sees less; `/sync` and
  `/async` switch how tool calls return (see the status header).

## Chat input prefixes

| Prefix | What it does |
|---|---|
| `/` | Command navigator (from an empty input) |
| `#` | Ask the LLM to decompose a goal into steps |
| `!` | **Run a command completely outside the sandbox**, at your full user privilege |

`!` is the deliberate escape hatch, and it is deliberately narrow: it fires only
from a keystroke you type, never from an LLM turn or a tool call. Every use is
disclosed twice — a warning in the log pane naming the command, and an
`UNSANDBOXED` label on the output window. It exists because the alternative is
worse: a user who cannot run one unconfined command from inside ahma runs it in
another terminal, where ahma can neither disclose nor record it.

## The sandbox scope panel

`/scope` opens a panel showing the sandbox **as the server actually locked it** — not what the TUI was launched with:

```
┌ Sandbox · scope ──────────────────────────── /scope closes ┐
│ ENFORCED · authority: ahma (kernel)                         │
│ write : ~/github/ahma                                       │
│ read  : (none beyond write roots)                           │
│ tmp   : OFF · source: roots/list                            │
└─────────────────────────────────────────────────────────────┘
```

- **`authority`** states who is protecting the session (SPEC R7.5): ahma's own kernel sandbox, ahma nested inside a detected host sandbox (Cursor, Docker, …), deferred to the host (ahma not enforcing), or none at all (`--no-sandbox`).
- **`source`** is the scope's provenance: `explicit` (a flag or settings), `roots/list` (your IDE's workspace), `elicited` (you answered a prompt), or `container` (the configured container root).
- **`grants`** lists the persistent scopes you granted yourself (`[sandbox] persistent_scopes`), so a writable path outside your workspace is explicable — and names `ahma sandbox list` / `ahma sandbox revoke <path>` to review or remove them.
- Platform limitations that cannot be expressed as scope — e.g. macOS scoping writes but not reads — are shown here too.
- If sandbox configuration **failed**, the panel shows the reason and the fix instead of a bare red chip.

The header's `sandbox:` label follows the same honesty rule: it shows the server-locked primary write root once reported. Until then it shows the launch directory with a trailing `?` — a guess, not the boundary.

## What the model is doing

While a chat turn runs, a status line pinned under the transcript says what is happening, in words, from real events only:

| You see | Meaning |
|---|---|
| `qwen3.8 is reading 37k tokens of context · 1m12s · ~2m left` | The model is reading its whole prompt before answering. Local models are silent while they do this; the time left appears once ahma has measured this model's reading speed in the session |
| `qwen3.8 is thinking · 5s` / `is writing · 18 tok/s` | Reasoning / answer tokens are arriving |
| `running cargo build · 42s` | A tool the model asked for is running |
| `waiting for your answer above` | An approval is open. The panel stops animating: this is your time, not the model's |

When a large prompt on a slow model is why you are waiting, a dim hint suggests `/compact` or `/model`. A turn over 10 s leaves a one-line summary — time taken, tokens read, reading and writing speed — so switching model or shrinking context is an informed choice.

If the connection drops before any answer arrives, ahma sends your message again once and says so; a second failure tells you what to do next. Models on this machine are given 30 minutes to read a prompt and are never re-sent it on a timeout, because re-sending makes a slow model start reading again from the beginning.

If chat is using an MCP client's own model and that client disconnects, chat switches to the model you used most recently that ahma runs itself, and says so; it switches back when the client returns, unless you chose another model meanwhile.

## Approval gates

The TUI is the primary surface for approving or rejecting actions that require user sign-off:

- **Trust this folder?** — asked once, the first time you open a folder. **Yes** lets every tool that runs *inside* the folder's kernel sandbox — reads, edits, builds, tests — run without asking. Still asked every time: paths outside the folder, network access, `!` commands, tools on other MCP servers, and settings changes. **No** (or Enter/Esc) keeps per-tool questions. Never offered for your home directory or a filesystem root. Undo with `ahma permissions revoke tool '*' --workspace <dir>`.
- **Tool approval** — a chat tool call awaiting your `y`/`a`/`n` in the banner above the input. Several calls to the same tool share one question, and `a` (always allow) covers the ones already queued.
- **Sandbox scope grants** — a sandboxed command was blocked on an out-of-scope path and the "grant access?" modal asks whether to record a persistent grant (Enter/Esc always deny; grants land in `~/.ahma/settings.toml` and apply on the next server start — ask the agent to run the `restart` tool to apply one immediately).
- **Web egress** — a blocked outbound domain raises the "allow egress?" modal, with three tiers: `[o]` once, `[s]` this session, `[a]` always. Cleartext `http://` is flagged rather than blocked.

When a command is blocked by the sandbox, the operation row says so — `denied: /etc (rw) · [a] ask` — rather than a bare "failed". Pressing `a` on it re-raises the grant question for exactly that path, even if you declined it earlier in the session: that suppression exists so ahma does not nag you, and deliberately choosing the row is not ahma nagging.

Persistent grants and revocations are appended to `~/.ahma/permissions-audit.jsonl`; see [docs/permissions.md](permissions.md).

## Transport auto-detection

`ahma tui` attaches to the per-user daemon; it never starts a server of its own,
and in particular never one scoped to the directory you happened to open it in
(SPEC R-DAEMON.9). It picks the best available transport in order:

1. **Unix socket** — the daemon's `mcp.sock` in your per-user runtime directory
   (`$XDG_RUNTIME_DIR/ahma`, else `~/.ahma`), or `[http] unix_socket_path` in
   `~/.ahma/settings.toml`. Lowest latency, local only. `$AHMA_UNIX_SOCKET` is retired (R-CFG1.2) and ignored by the TUI as it is by `ahma serve`.
2. **HTTP/3 (QUIC)** — when the server advertises `Alt-Svc: h3=…` _and_ local TLS material exists at `~/.ahma/tls/`. See [TLS provisioning](#tls-provisioning-for-quic) below.
3. **HTTP/1.1 / HTTP/2** — plain TCP, always available as a fallback.

The transport in use is shown in the TUI's status header (e.g. `HTTP/3 (QUIC)` or `Unix socket`).

Use `--connect` to bypass detection and force a specific endpoint:

```bash
# Force a specific HTTP address (skips Unix socket probe)
ahma tui --connect http://localhost:8080

# Force the Unix socket path
ahma tui --connect unix:///run/ahma/mcp.sock
```

Start the server with `--disable-quic` (or set `[http] disable_quic = true` in `~/.ahma/settings.toml`) to prevent the HTTP/3 upgrade even when it would otherwise be advertised. `AHMA_DISABLE_QUIC` is retired and ignored.

## TLS provisioning for QUIC

HTTP/3 transport requires TLS. `ahma` manages a persistent self-signed certificate at `~/.ahma/tls/` (override with the `--tls-dir` flag; `AHMA_TLS_DIR` is retired and ignored):

```bash
# Generate certificate on first use (safe to re-run — idempotent)
ahma tls init

# Replace the certificate (e.g. after 30 days or key compromise)
ahma tls rotate

# Check certificate age and rotation status
ahma tls status
```

The `ahma tls init` step is offered automatically during `install.sh`. The private key is stored with 0600 permissions.

## See also

- [docs/permissions.md](permissions.md) — grants, the question ladder, and the permissions ledger
- [docs/connection-modes.md](connection-modes.md) — HTTP bridge setup
