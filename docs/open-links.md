# Opening web links from inside the sandbox

**Status:** Experimental (v0.22.19). macOS and Linux, for commands run through the terminal
hooks (Claude Code's Bash tool) and through MCP (`run_terminal_command`, with or without
`pty`). On Windows commands open links as they would without ahma: there is no sandbox
boundary there yet. SPEC:
[R-LINK](../SPEC.md), [R6.2.10](../SPEC.md).

## Why

ahma's sandbox never lets a command start an app through LaunchServices: an app started that
way runs outside every sandbox, and a planted one would run unconfined. A browser is such an
app, so `gh pr view --web`, `gh auth login --web` and `open https://…` used to fail. Opening a
link is useful, but it is also a way out: the whole address goes to the website, in the
browser where you are signed in, and an address can carry data from your project. So ahma
opens links for commands itself, outside the sandbox, only `http` and `https`, and only for
sites you allowed.

## Before you allow a site

Opening a link sends its whole address to that website and opens it in your everyday browser,
where you are signed in. An address can carry data from your project, so allow only sites you
trust. One site is safer than any site, and one repo is safer than all repos. ahma opens only
http and https links, in your default browser, never files or apps. Undo with
`ahma permissions revoke open-url`.

## Quickstart

```bash
# Let commands open GitHub links without asking (preview first, then --yes):
ahma permissions grant open-url github.com
ahma permissions grant open-url github.com --yes

# In this repo only:
ahma permissions grant open-url github.com --this-repo --yes

# Inside any command ahma runs (hooked or over MCP), these now open in your browser:
gh pr view 12 --web
"$BROWSER" https://github.com/ahma-labs/ahma
open https://github.com/ahma-labs/ahma        # runs as `ahma open-url`, and says so

# See and undo:
ahma permissions list --kind open-url
ahma permissions revoke open-url github.com --yes
ahma permissions revoke open-url --yes        # every site
```

A hooked command is never paused to ask: a link to a site you have not allowed is not opened,
and the command fails at once with one line that names the exact
`ahma permissions grant open-url <site>` and the address, so the agent can show it to you. A
command run over MCP waits while you are asked ([below](#asked-in-your-mcp-client)).

## Asked in your MCP client

A command run through ahma's MCP server (`run_terminal_command`, with or without `pty`) waits
while ahma asks you in the MCP client's own prompt (`elicitation/create`), for at most that
client's question budget (45–120 s). The command's operation carries an alert saying it is
waiting for you, and the question shows the site on its own line, the address, any note, and
the guidance above. The answers, deny first and the default:

| Answer | What it does |
|---|---|
| Deny | Not opened. ahma does not ask about that site again in this session. |
| This time | This link opens now. Nothing is remembered; the next link to the site is asked again. |
| Always for this site — all repos | Saved to `[open_url] allow`, as `ahma permissions grant open-url <site>` would. |
| Always for this site — this repo only | Saved for this repo, as `--this-repo` would. |
| Always (any site) | Any public site. Offered only where it would cover the link. |

Every yes is in `permissions-audit.jsonl`, with the surface `harness`. Two commands opening the
same site at once share one question. Link questions count toward the session's budget of five
questions in ten minutes.

When nobody can be asked — the client does not show questions, the question was closed or
timed out, an earlier question failed, or the budget is spent — the link is not opened, and the
line names the grant and says why you were not asked. A question closed without an answer
decides nothing: the next link to that site is asked again.

## Asked in Claude Code

When the agent runs the refused command again — or any `ahma open-url`, `"$BROWSER"` or
whole-line `open` of a link to the same site — Claude Code's own dialog asks you first. It
shows the site on its own line, the address (up to 300 characters), a note for a local site or
one outside your network allow list, and the guidance above. That dialog has only yes and no,
so it names every answer, in this order:

| Answer | How you give it | What it does |
|---|---|---|
| Deny | No | Not opened. ahma does not ask about that site again in this Claude Code session. |
| This time | Yes | The approved run may open that site's links. Nothing is remembered; the next refusal asks again. |
| Always for this site — all repos (the usual lasting answer) | `ahma permissions grant open-url <site>` | Saved for every repo. |
| Always for this site — this repo only | `ahma permissions grant open-url <site> --workspace '<repo>'` | Saved for this repo. |
| Always (any site) | `ahma permissions grant open-url` | Any public site; never a local one, and with network restriction on only sites on your allow list. Offered only where it would cover the link. |

Each yes is recorded in `permissions-audit.jsonl` as a one-time `open-url` grant from the
harness dialog.

## How it works

1. Every command ahma runs, from a terminal hook or over MCP, gets `BROWSER` and `GH_BROWSER` set to
   `ahma-open-url` — ahma's own binary under that name — and a socket of its own to the ahma
   process that started it, which answers only that command.
2. The helper sends each address to that process, outside the sandbox. It checks it: `http`
   or `https`, a real host, no `user@` before the host, no spaces or backslashes, at most
   8 KB. It opens what it checked, in its plain ASCII form, never the text the command passed.
3. If you allowed the site, it runs `open` (macOS) or `xdg-open` (Linux) with the address.
   If not, over MCP it asks you while the command waits; from a hook the command gets the line
   above, and the refusal is remembered for Claude Code's dialog.
4. `ahma open-url <URL>…` is the same helper by name. A command line that is exactly `open`
   or `xdg-open` and one `http`/`https` address (bare, or quoted as a literal) runs as it, and
   ahma says so: the result's first line from a hook, an alert on an MCP operation. Any other form (`open -a …`, two addresses, an unquoted
   `&`) runs as written; if the sandbox refuses it, ahma names the exact `ahma open-url '<URL>'`
   to run instead, and does not offer to run it outside the sandbox.
5. Only a command that has a link broker may reach the broker's socket directory; another
   command run by the same ahma process gets no such access.

## Local addresses

`localhost`, `*.local`, names without a dot, and private, loopback and link-local addresses
(including a public name that points at one) are never covered by "any site". A link there can
make a local service act for you; allow each by name, e.g.
`ahma permissions grant open-url localhost` (every port).

## With `--restrict-network`

A link is egress. While network restriction is on, "any site" covers only sites on your network
allow list ([network-egress.md](network-egress.md)); any other site needs its own grant. A link
grant never lets commands reach a site, and a network grant never opens links.

## Reference

| Setting / command | What |
|---|---|
| `[open_url] allow` | Sites whose links open without asking in every repo: `github.com`, `*.github.io` (one label), `*` (any public site). Written by `ahma permissions grant open-url`. |
| `[open_url] repos` | The same, for one repo (`--this-repo`, `--workspace <DIR>`). |
| `ahma permissions grant open-url [SITE]` | Preview, then `--yes` to save. No site = any public site. |
| `ahma permissions revoke open-url [SITE]` | Remove one site, or with no site every one. |
| `ahma open-url <URL>…` | The helper, by name, inside a command ahma runs. |
| `BROWSER`, `GH_BROWSER`, `AHMA_LINK_SOCK` | Set by ahma for each command; do not set them yourself. |

Every link opened or refused is in the execution audit log ([execution-audit-log.md](execution-audit-log.md))
as `link_opened` / `link_refused`, with the address's scheme, host and path and only the
lengths of its query and fragment.

## Limits

- At most 5 links per command, each at most 8 KB.
- Asked in Claude Code's dialog when a hooked command runs again, and in the MCP client's prompt
  while an MCP command waits. The TUI, `ahma web` and `ahma desktop` do not ask yet (SPEC
  R-LINK.6): a client that cannot show questions gets the grant line.
- Commands in a persistent shell session (`session_id`) share one long-lived shell, so they get
  no link broker of their own; a browser launch there fails as before.
- A site you allow can forward your browser elsewhere (an open redirect); allow sites you trust.
- A name that resolves to a public address when ahma checks it and to a local one later (DNS
  rebinding) is not caught: the browser looks the name up again.
- `cargo doc --open` passes a file; ahma never opens files, and tells the agent to give you its
  path.
- Linux: `xdg-open` run inside the sandbox may still reach a browser you already have open,
  over D-Bus, which the sandbox does not yet mediate.
- Windows: no link broker yet (no filesystem sandbox there either).

## See also

[security-sandbox.md](security-sandbox.md) (LaunchServices, R6.2.10) ·
[ssh-agent-broker.md](ssh-agent-broker.md) (the same broker shape for SSH keys) ·
[network-egress.md](network-egress.md) · [permissions.md](permissions.md) ·
[environment-variables.md](environment-variables.md) · [SPEC R-LINK](../SPEC.md)
