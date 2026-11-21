# Opening web links from inside the sandbox

**Status:** Experimental (v0.22.19). macOS and Linux, for commands run through the terminal
hooks (Claude Code's Bash tool); MCP `run_terminal_command` follows. On Windows commands open
links as they would without ahma: there is no sandbox boundary there yet. SPEC:
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

# Inside any hooked command, these now open in your browser:
gh pr view 12 --web
"$BROWSER" https://github.com/ahma-labs/ahma

# See and undo:
ahma permissions list --kind open-url
ahma permissions revoke open-url github.com --yes
ahma permissions revoke open-url --yes        # every site
```

A link to a site you have not allowed is not opened: the command fails with one line that
names the exact `ahma permissions grant open-url <site>` to run, and the address, so the agent
can show it to you.

## How it works

1. Every command ahma runs from a terminal hook gets `BROWSER` and `GH_BROWSER` set to
   `ahma-open-url` — ahma's own binary under that name — and a socket of its own to the ahma
   process that started it, which answers only that command.
2. The helper sends each address to that process, outside the sandbox. It checks it: `http`
   or `https`, a real host, no `user@` before the host, no spaces or backslashes, at most
   8 KB. It opens what it checked, in its plain ASCII form, never the text the command passed.
3. If you allowed the site, it runs `open` (macOS) or `xdg-open` (Linux) with the address.
   If not, the command gets the line above.
4. `ahma open-url <URL>…` is the same helper by name.

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
- Not yet asked: a link to a site you have not allowed is refused, never asked about. Asking
  in Claude Code's dialog and in MCP clients is in progress (SPEC R-LINK.6).
- `open https://…` itself is still refused by the sandbox on macOS; use `"$BROWSER" <URL>` or
  `ahma open-url <URL>`.
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
