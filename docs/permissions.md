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
cannot read it and cannot write it. So no matter what a command does — no matter
how thoroughly it is compromised or how confused an AI agent gets — **it cannot
grant itself anything**. Only you can, and only after seeing the exact line that
would be written.

```bash
ahma permissions list                      # everything ahma has been granted
ahma permissions revoke fs-scope ~/cache   # previews; add --yes to apply
```

## What happens when something is blocked

ahma asks you in the best place available, in this order:

1. **Your IDE / agent** (Cursor, Claude Code, …). If it can show a prompt, that's
   where the question appears — you're already looking at it, and it arrives with
   the context of whatever you were doing.
2. **The ahma TUI**, if one is attached: a modal, over whichever view you're in.
3. **Nowhere left to ask** → the command **fails**, and tells you exactly how to
   fix it:

   ```
   ahma's kernel sandbox blocked an out-of-scope write to
   '/opt/ext/sccache/0/object.o'. To allow it, run
   `ahma sandbox grant /opt/ext/sccache/0` and re-run the command.
   ```

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
| **once** | This command only. Never written down. | You're not sure yet. |
| **session** | Until ahma restarts. Memory only. | A one-off task. |
| **always** | `~/.ahma/settings.toml`, until you revoke it. | A cache your builds always need. |

Only **always** touches disk, and only after you've seen the exact file and the
exact line.

## When a grant takes effect

- **Terminal hooks**: on your **next command**. Hooks re-derive the sandbox each
  time, so there's nothing to restart.
- **The MCP server** (your IDE's connection): on the **next server start**. The
  sandbox scope is locked for the life of a session on purpose — a running session
  can never have a hole punched in it. Use the `restart` tool to apply it now.

## What ahma will never grant

Some paths are refused outright, with no override flag, no matter who asks — you,
the AI, or a confirmed prompt:

- your home directory itself, or a filesystem root
- any parent of your workspace (that would widen the sandbox above your project)
- credential directories: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.docker`,
  `~/.config/gh`, `~/.config/gcloud`
- `~/.ahma` itself — the ledger cannot authorize access to the ledger
- OS system directories

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
profiles = ["rust"]   # only rust; drop node, go, common
# profiles = []       # nothing — grant every toolchain path explicitly
```

Built-in: `rust`, `node`, `go`, `common`. All enabled by default.

Note what the `rust` profile deliberately does **not** do: it never makes
`~/.cargo/bin`, `~/.cargo/config.toml`, or `~/.cargo/credentials.toml` writable.
Granting all of `~/.cargo` would be simpler and would hand a sandboxed command
your crates.io token and write access to every binary on your PATH.

## A limitation, stated plainly

**On macOS, ahma scopes writes but not reads.** Apple's sandbox cannot reliably
match read paths under APFS firmlinks, so ahma grants blanket read access rather
than pretend to a protection it doesn't have. Writes are still kernel-enforced.

Practically: on macOS, treat anything *you* can read as readable by a sandboxed
command. Linux (Landlock) scopes both. ahma shows this in `ahma status` and in the
TUI scope panel rather than burying it here.

## Command reference

```bash
ahma permissions list                          # every grant, of every kind
ahma permissions list --kind fs-scope          # just filesystem scopes
ahma permissions revoke fs-scope ~/cache --yes # revoke (previews without --yes)
ahma permissions revoke tool cargo_build       # per-workspace tool approval

ahma sandbox grant ~/cache [--read-only]       # kind-scoped shortcut
ahma sandbox list
ahma sandbox revoke ~/cache

ahma web allow api.github.com                  # outbound domains
ahma web list
```

Every grant and revoke is appended to `~/.ahma/permissions-audit.jsonl`.

## See also

- [`docs/security-sandbox.md`](security-sandbox.md) — how the sandbox itself works
- [SPEC.md](../SPEC.md) — R-PERM (this model), R5 (sandbox scope), R-WEB (egress)
