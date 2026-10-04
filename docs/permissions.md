# Permissions

ahma runs your commands inside a kernel-enforced sandbox. Sooner or later it will
block something you actually wanted — a build cache outside the workspace, a
toolchain in an unusual place, a domain a tool needs to reach. This page is about
what happens then.

The short version: **the block is the question**. ahma cannot predict what the
thousands of tools it has never seen will need, so it doesn't try. It waits for
the kernel to tell it exactly what was needed, at the exact moment it was needed,
and then asks you — once, clearly, with the option to remember your answer.

## The one rule worth knowing

Every permission you grant lives in **`~/.ahma/settings.toml`**, and that
directory is the one place the sandbox *never* includes. A sandboxed command
cannot write it — kernel-enforced on Linux and macOS — and cannot read it either
(on macOS via the credential denylist, since reads there are not scoped; see
[Limitations](#limitations-stated-plainly)). So no matter what a command does — no
matter how thoroughly it is compromised or how confused an AI agent gets — **it
cannot grant itself anything**. Only you can, and only after seeing the exact line
that would be written.

```bash
ahma permissions list                      # everything ahma has been granted
ahma permissions revoke fs-scope ~/cache   # previews; add --yes to apply
```

## What happens when something is blocked

ahma asks you in the best place available, in this order:

1. **Your IDE / agent** (Cursor, Claude Code, …). If it can show a prompt, that's
   where the question appears — you're already looking at it, and it arrives with
   the context of whatever you were doing.
2. **The ahma TUI**, if one is open: a modal, over whichever view you're in. A question
   goes there only while a TUI is actually open; with none open it falls to the next rung
   rather than waiting for a TUI nobody is looking at.
3. **Nowhere left to ask** → the command **fails**, and tells you exactly what to do:

   ```
   Blocked until a human grants it: ahma's kernel sandbox refused an
   out-of-scope write to '/opt/ext/sccache/0/object.o'.
   ...the same who / what / minimum / allows / risk body as the prompt...
   One thing to do (pick a tier), then re-run the command:
     ahma sandbox grant /opt/ext/sccache/0 --session   # this terminal session only
     ahma sandbox grant /opt/ext/sccache/0             # until revoked, this workspace
   ```

Wherever the question lands, it carries the same body: **who** is asking (client,
workspace, session), **what was blocked** (the command, the path the kernel named,
the evidence line, how many times it has been asked), **what the agent says it
needs** (its own words, shown as its claim), the **minimum** that would work
(read-only unless a write was refused), **what a grant allows** every later command
in that workspace to do, the **risk** (with observed facts, never file contents),
and the exact settings line an "always" answer writes. In your IDE the choices are
buttons with those labels, deny first; in the TUI they are keys; in a terminal they
are the two commands above. A client that cannot show a prompt is told to relay the
body to you unchanged. Ahma raises at most five such questions per ten minutes per
session; past that the agent is told to ask you in conversation instead.

**Commands run through Claude Code's Bash tool** (the terminal hook) are refused
mid-command, where no prompt is possible. ahma remembers the refusal, and before
the **next** command in that workspace Claude Code shows its own permission dialog
with ahma's question: which directory, which refused paths it covers, and what yes
and no mean. **Yes** grants it for this Claude Code session and runs the command;
**no** runs nothing, and ahma does not ask about that directory again this session.
Related paths are asked as one directory when that is safe (`~/.cache/neubit`, never
`~/.cache`, your home folder, a folder of projects, or anything holding credentials).
For "always", the dialog names the `ahma sandbox grant` command (SPEC R-PERM.10).
If `ahma tui` is open, the same question appears there as soon as the refusal is
recorded, and answering it in the TUI (deny, this session, 24 hours or always) means
Claude Code never shows the dialog for it.

That last rung is the important one: ahma never fails *open*. If nobody can be
asked, the answer is no — and you get a command you can paste rather than a
mystery.

A few deliberate details:

- **Enter and Esc always mean no.** A stray keypress can never widen your sandbox.
- **You're asked once.** The kernel will trip on the same path dozens of times;
  you hear about it once per session. Declining counts as an answer — saying no
  does not get you nagged.
- **If your IDE doesn't answer** (times out, or can't show prompts), ahma stops
  trying it for the session and tells you where the question went instead. But a
  *decline* is an answer, not a failure — declining doesn't cost you the prompt.

## Three ways to say yes

| Tier | Lives | Use it when |
|---|---|---|
| **once** | The next command only. Never written down. | You're not sure yet. |
| **session** | Until the session that approved it ends (your MCP server, or the terminal you ran `ahma sandbox grant --session` in), at most 12 hours. Never written to the file; terminal hooks and the edit guard honour it too. | A one-off task. |
| **always** | `~/.ahma/settings.toml`, until you revoke it. | A cache your builds always need. |

Only **always** touches disk, and only after you've seen the exact file and the
exact line.

## Kinds of permission

`ahma permissions list` groups every grant by kind; `--kind` and `revoke` take the
same names.

| Kind | What it allows | Granted by | Lives in |
|---|---|---|---|
| `fs-scope` | a directory outside the workspace, read-only or read-write | a prompt, `ahma sandbox grant` | `[sandbox].persistent_scopes` |
| `web-domain` | a domain ahma's own HTTP tools may fetch | a prompt, `ahma web allow` | `[web]` |
| `net-host` | a host sandboxed commands may reach under `--restrict-network` | a prompt, `ahma network allow` | `[network].allow` |
| `tool` | a tool the agent may run in a workspace without asking again | a tool-approval prompt | `[permissions].tool_approvals` |
| `log-target` | a file outside the workspace that a `.ahma/logs/*.log` symlink points at, readable (never writable) by live-log monitoring | a human's `always` answer to the question `logs_approve` raises, or `a` on the log in `ahma tui` | `[log_targets].approvals` |

`tool` and `log-target` grants are bound to the workspace they were granted in, and so
is an `fs-scope` grant unless it was made `--global`; `web-domain` and `net-host` grants
apply everywhere. A `log-target` grant answered at a prompt applies to the session that
asked at once; one made with `a` in `ahma tui` reaches sandboxed log monitors from the
next session; see
[Live log monitoring](live-log-monitoring.md#log-files-that-link-outside-the-workspace).

## When a grant takes effect

- **Scope**: a filesystem grant is bound to the workspace it was approved for
  (the session's project root; `ahma sandbox grant` uses the repository enclosing
  your current directory, or `--workspace <DIR>`). Agents in other projects never
  see it. `--global` is the explicit opt-out, and `ahma doctor` flags grants that
  have no workspace.
- **Tier**: at a prompt you can answer for this session only (`read-write-session`
  / `read-only-session`, or `[s]` / `[o]` in the TUI): applied now, audited, never
  written to the file. The answer is also recorded for terminal hooks and the edit
  guard in the same workspace (under ahma's runtime directory, ending with the
  session or after 12 hours), so a hooked command gets the same session tier an
  MCP session does. From a terminal, `ahma sandbox grant <dir> --session` does the
  same for the shell you typed it in. Every tier, session included, passes the
  denylist below: a session answer on `~/.ssh` is refused, not applied.
- **Terminal hooks**: on your **next command**. Hooks re-derive the sandbox each
  time, so there's nothing to restart.
- **The MCP server** (your IDE's connection): when a human approves a request the
  `sandbox_grant` or `network_grant` MCP tool raised (at your client's prompt, or in the
  ahma TUI), it takes effect **immediately** for the active session, and an `always`
  answer is also saved. The agent is told the answer you gave: approved and for how
  long, declined, still waiting in the TUI, or that no surface could ask you. The
  tools never grant on their own: `confirm: true` only raises the
  question, for every client — a client that cannot show a prompt cannot approve, and
  ahma never assumes it asked you before the call. A filesystem grant made offline
  (`ahma sandbox grant`, `revoke` or `renew`, or a direct `~/.ahma/settings.toml` edit)
  takes effect from the next command, in a running server too: it re-reads the file
  whenever it changes. Network allow-list edits (`ahma network allow`) take effect on
  the next server start (or after using the `restart` tool).

## Web domains and redirects

`fetch_webpage` asks about a domain only in strict mode (`[web] default_policy =
"deny"`); under the default `allow` policy only `never_allow` domains, and ones you
denied this session, are refused. Approving `api.github.com` approves that host and
nothing else, so when a page **redirects to a different host**, the new host is
decided on its own (SPEC R-WEB.8). `[web] on_redirect_to_new_domain` says how:

| Value | A redirect to another host is… |
|---|---|
| `"policy"` (default) | followed if the `[web]` policy — `always_allow`, `never_allow`, this session's answers, `default_policy` — allows the new host; otherwise the fetch fails with the `ahma web allow <host>` command to approve it. Never prompts. |
| `"block"` | never followed, even to a host the policy allows. The fetch fails with an error naming the target and this setting; fetch the target URL directly if you want it. |
| `"prompt"` | treated exactly like a new request to that host: allowed or refused by the policy as usual, and, if the policy would ask (strict mode, a host you have not answered for), you get the same once / session / always question, deduplicated the same way. The redirect is followed only if you say yes. |

Whatever the value, a redirect to the **same** host (including `http` → `https`)
is always followed, a chain stops after 10 hops, and every hop is checked against
the private-address block (loopback, RFC-1918, cloud metadata) before it connects.

```toml
# ~/.ahma/settings.toml
[web]
on_redirect_to_new_domain = "prompt"   # or "policy" (default), "block"
```

`ahma web list` shows the value in effect. An unrecognised value is a parse error:
`[web]` is security-tier, so ahma refuses to start rather than guess.

> **Upgrading:** before this setting was enforced, every redirect behaved as
> `"policy"`, whatever the file said, and the documented default was `"block"`.
> The default is now `"policy"`, so nothing changes unless your settings file
> sets the key — but a file that explicitly says `"block"` now really blocks.

## Git authentication (SSH and HTTPS)

The sandbox denies reads of your private keys (`~/.ssh/id_*`) and forwards the SSH
agent socket instead, so a sandboxed `git fetch` or `git push` over SSH can
authenticate only through the agent. On the host, ssh reads the key file directly,
so an **empty agent is invisible until the first sandboxed push** fails with
`Permission denied (publickey)`. One thing to do: `ssh-add --apple-use-keychain
~/.ssh/id_ed25519` (macOS) or `ssh-add ~/.ssh/id_ed25519`.

HTTPS is the more common transport and ahma does not block it: the login keychain
is allowed (`[sandbox] allow_keychain`, on by default), so `osxkeychain` and `gh`
credential helpers work, and `~/.config/gh` is readable unless you added it to
`deny_credential_reads`. If a sandboxed push over HTTPS fails with `could not read
Username` or `Authentication failed`, no helper answered: `gh auth login` then `gh
auth setup-git` on the host fixes it. `ahma doctor` checks both transports and
names the exact command or settings key when one cannot work.

## The advisor

In the TUI, the model you have selected can add one line to a grant prompt:

```
advisor: session · read+write — a project cache; a write was refused (confidence 80%)
```

It is advice, labelled as such, and nothing more: it never changes the default
(deny), never answers, and the prompt is complete without it if the model is slow.
The model sees the evidence — who asked, what the kernel refused, the command, what
the directory contains — and deliberately **not** the agent's own justification: a
judge shown the actor's summary approves most of what it is shown. Every answer records
the advice and whether you followed it in the audit log, and `ahma doctor` reports how
long you take to answer prompts; a median under three seconds is a warning, because a
prompt answered faster than it can be read is theater. `[permissions] advisor = false`
turns the advisor off; `advisor_timeout_secs` (default 6) bounds the wait. (SPEC
R-PERM.8, R-DOCTOR.8.)

## What ahma will never grant

Some paths and network destinations are refused outright, with no override flag, no matter who asks — you,
the AI, or a confirmed prompt:

- your home directory itself, or a filesystem root
- any parent of your workspace (that would widen the sandbox above your project;
  the enclosing git repository root of a worktree or subdirectory workspace is allowed)
- credential directories and everything inside them: `~/.ssh`, `~/.aws`, `~/.gnupg`,
  `~/.kube`, `~/.docker`, `~/.config/gh`, `~/.config/gcloud` (a key file is refused
  like the directory that holds it; git and ssh still work through your SSH agent)
- `~/.ahma` itself and everything in it — the ledger cannot authorize access to the ledger
- OS system directories
- a grant for any of these that is already in `settings.toml` (written by hand, or by an
  older ahma) is skipped at startup with a warning naming the revoke command; recorded
  is not the same as allowed

Some paths are allowed but flagged **high risk**, with a sentence saying what will run
what a grant lets the agent write there: shell startup files (`~/.zshrc`, `~/.bashrc`,
`~/.profile`, …) run in every new shell, `~/Library/LaunchAgents` and
`~/.config/autostart` start programs at login, and a `.git/hooks` directory is run by
git on commit, checkout and push. None of them asks first.

- for network egress: blanket `*` wildcard (refused via AI grant tool; only human editing or explicit CLI can author), `localhost`, private RFC 1918 IP addresses, and link-local or cloud-metadata IPs (`169.254.169.254`)

If a tool genuinely needs something under one of these, grant the *specific
subdirectory* it needs. There's no flag to override this, deliberately: every
legitimate case is better served by being specific.

## Sandbox profiles

Some toolchains need paths outside your workspace just to function — the cargo
registry cache, the rustup toolchains, the npm cache. ahma ships these as
**profiles**: pre-answered bundles of the grant questions you'd otherwise have to
answer one denial at a time.

```bash
ahma permissions list      # shows the profiles in effect, and every path each grants
```

They're data, not hard-coded exceptions, which means you can see exactly what
they give away and turn any of them off:

```toml
# ~/.ahma/settings.toml
[sandbox]
profiles = ["rust"]   # only rust; drop node, go, android, apple, common, gh, sccache
# profiles = []       # nothing — grant every toolchain path explicitly
```

Built-in: `rust`, `node`, `go`, `android`, `apple`, `common`, `gh`, `sccache`. All enabled by default.
A profile whose toolchain is not installed grants paths that do not exist, which is
harmless; `android` (Gradle, Maven, the SDK, Kotlin/Native) and `apple` (Xcode
DerivedData, SwiftPM and CocoaPods caches, simulators read-only) exist so an iOS or
Android build does not have to stop and ask a human for five cache directories one
at a time. Both carry a `cost` line in `ahma permissions list`: like the cargo
caches, Gradle and SwiftPM *execute* what they find in those shared directories.

### Profiles that set environment variables

A profile may also set variables on every sandboxed command, each with a stated
reason. The one that does today is `sccache`. sccache runs a long-lived server that
every compiler call talks to, and a server started by a sandboxed build can write
only the checkout it started in, so every other checkout's builds failed through it.
The `sccache` profile gives each workspace its own server instead:

| Variable | Value | Why |
|---|---|---|
| `SCCACHE_DIR` | `<workspace>/.sccache` | the cache lives where a sandboxed server can write |
| `SCCACHE_CACHE_SIZE` | `4G` | each workspace's cache is bounded, rather than sccache's default 10G per checkout |
| `SCCACHE_SERVER_PORT` | a port derived from the workspace path, 20000–59999, the same every time | that workspace's builds only ever reach its own server |

A variable that names a cache directory is marked as one in the profile
(`cache_dir = true`). Before a command runs, ahma creates that directory with a
`.gitignore` of `*` and a `CACHEDIR.TAG`. So it never shows up in `git status`,
whatever your repository's own ignore rules, and backup tools that honour the tag
(Time Machine via `tmutil`, restic, borg, tar `--exclude-caches`) skip it. Delete it
whenever you like; it is only a cache. It is only ever created inside the
workspace. (Before this, the cache was `<workspace>/target/sccache`: in a
repository whose Rust code is in a subdirectory, that root `target/` was not
ignored. An old `target/sccache` can be deleted.)

The cost is a cold cache per checkout (the first build in each is slower) and up
to 4G of disk each. Nothing built for one project is ever served to another. Rules for every
profile variable:
- it never overrides a variable you have already set (a shared `SCCACHE_DIR` of
  your own wins);
- `ahma permissions list` shows each one, with its value for the current folder
  and its reason;
- it goes when you remove the profile from `[sandbox] profiles`.

`ahma doctor` and ahma's startup check leave these per-workspace servers alone;
they still restart a sccache server stuck confined on the shared default port.

Note what the `rust` profile deliberately does **not** do: it never makes
`~/.cargo/bin`, `~/.cargo/config.toml`, or `~/.cargo/credentials.toml` writable.
Granting all of `~/.cargo` would be simpler and would hand a sandboxed command
your crates.io token and write access to every binary on your PATH.

## Limitations, stated plainly

**On macOS, ahma scopes writes but not reads** (SPEC R6.2.2). Apple's sandbox
cannot reliably match read paths under APFS firmlinks, so ahma grants blanket read
access rather than pretend to a protection it doesn't have. Writes are still
kernel-enforced. What keeps your secrets out of reach there is a **denylist** of
credential paths and key material (SPEC R6.2.3) — and a denylist is a weaker thing
than a scope: a scope denies everything it doesn't name, a denylist denies only
what it does. Anything nobody thought to enumerate is readable.

Practically: on macOS, treat anything *you* can read as readable by a sandboxed
command unless it is on that list. Linux (Landlock) scopes both directions
properly (SPEC R6.1.6). **On Windows, neither** — a Job Object bounds process
lifetime, not filesystem paths, and AppContainer is not wired up yet (SPEC
R6.3.9). ahma shows all of this in `ahma status` and in the TUI scope panel rather
than burying it here.

**Some paths inside your workspace are not writable, and the protection is not
uniform.** Files that something outside the sandbox later executes by convention —
git hook directories, ahma's own `.ahma/` tool definitions, container daemon
sockets — are denied outright; editor and harness configuration is allowed but
disclosed loudly when written. That deny tier is kernel-enforced on macOS,
**application-layer only on Linux** (so a shell command through
`run_terminal_command` can still write those paths), and unenforced on Windows.
Where the kernel does not hold it, ahma compares those paths before and after every
command and reports a change as `TRUST-HANDOFF WRITE` in the result and the audit
log — detection, not prevention.
See [`docs/security-sandbox.md`](security-sandbox.md#writable-but-not-everything-trust-handoff)
and SPEC R-HANDOFF.

**SSH credentials and agent authentication**: On macOS, Seatbelt denies direct disk reads to private keys (`~/.ssh/id_*`) from sandboxed commands. Ahma forwards `$SSH_AUTH_SOCK` into the sandbox, so SSH operations (such as `git fetch` or `git push` over SSH) authenticate seamlessly via the SSH agent. If a command fails with `Permission denied (publickey)`, run `ssh-add` on the host to load your key into the agent (e.g. `ssh-add ~/.ssh/id_ed25519`).

## Command reference

```bash
ahma permissions list                          # every grant, of every kind
ahma permissions list --kind fs-scope          # just filesystem scopes
ahma permissions list --kind net-host          # just network hosts
ahma permissions revoke fs-scope ~/cache --yes # revoke this workspace's grant (previews without --yes)
ahma permissions revoke net-host crates.io --yes # revoke network host
ahma permissions revoke tool cargo_build       # per-workspace tool approval
ahma permissions list --kind log-target        # log files outside the workspace that live-log may read
ahma permissions revoke log-target /var/log/app.log --workspace ~/code/proj

ahma sandbox grant ~/cache [--read-only]       # kind-scoped shortcut, bound to this workspace
ahma sandbox grant ~/cache --session           # this terminal session only; never written to the file
ahma sandbox grant ~/cache --for 8h            # a lease: saved, and stops applying after 8h
                                               # (every prompt also offers "for 24 hours")
ahma sandbox renew ~/cache --for 24h           # extend a lease (denylisted and audited like a grant)
ahma permissions list --expiring 12h           # the leases to renew before a long unattended run
ahma sandbox list
ahma sandbox revoke ~/cache                    # this workspace's grant; --workspace <DIR> or --global for another

ahma network allow crates.io                   # subprocess egress hosts
ahma network list
ahma network revoke crates.io

ahma web allow api.github.com                  # outbound domains (HTTP fetch tools)
ahma web list
```

Every grant and revoke is appended to `~/.ahma/permissions-audit.jsonl`, with the
surface that answered (`cli`, `tui`, `harness`). Filesystem grants are written through
one function — the CLI, the TUI modal and an elicitation answer all share it — and that
function applies the denylist above and writes the audit record, so no surface can skip
either.

## See also

- [`docs/security-sandbox.md`](security-sandbox.md) — how the sandbox itself works
- [SPEC.md](../SPEC.md) — R-PERM (this model), R5 (sandbox scope), R-WEB (egress; R-WEB.8 for redirects)
