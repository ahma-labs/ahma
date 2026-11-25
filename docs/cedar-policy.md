# Team policy in Cedar

**Status: experimental** (v0.22). SPEC: [R-POLICY](../SPEC.md#r-policy-one-policy-decision-point-and-team-policy-in-cedar),
[`ahma_policy/SPEC.md`](../ahma_policy/SPEC.md).

Write rules in [Cedar](https://www.cedarpolicy.com/) that **forbid** what ahma would otherwise
allow: a path no grant may open, a host no command may reach, a tool no agent may call. ahma
reads them once at startup and checks them before every permission it decides. They can only
narrow: a file containing a `permit` is refused.

## Why

ahma's own defaults, your grants and `ahma setup` are how one person decides what an agent may
do. A team needs something else: a few rules that hold on every developer's machine, that no
prompt can talk past, and that a reviewer can read in a pull request. Cedar is a policy
language built for that, already used for agent permissions by AWS (Bedrock AgentCore) and
Docker (MCP governance), with a formally specified evaluator.

It is not a replacement for the kernel sandbox. A rule here is only as strong as the place it
is enforced; ahma enforces it at its own decisions, and the kernel enforces the scope those
decisions build. See [What reaches the kernel](#what-reaches-the-kernel).

## Quickstart

```bash
mkdir -p ~/.ahma/policy
cat > ~/.ahma/policy/team.cedar <<'EOF'
@reason("cloud credentials are never granted to an agent")
forbid(principal, action in [Ahma::Action::"read", Ahma::Action::"write"],
       resource in Ahma::Path::"/Users/me/.aws");

@reason("paste sites are an exfiltration channel")
forbid(principal, action in [Ahma::Action::"connect", Ahma::Action::"fetch", Ahma::Action::"open_link"],
       resource in Ahma::Host::"pastebin.com");

@reason("no sub-agents in production checkouts")
forbid(principal, action == Ahma::Action::"call_tool", resource == Ahma::Tool::"agent")
  when { principal has workspace && principal.workspace like "/srv/prod/*" };
EOF

ahma policy validate                                # parse and check against ahma's schema
ahma policy list                                    # what is loaded, from where
ahma policy explain fetch https://pastebin.com/x    # which rule decides, and why
```

Policy is read once when each ahma process starts: a hooked command picks up a change at once, an MCP session when it next starts (quit the editor's ahma sessions, or the hub).

## Where policy files live

| Directory | For |
|---|---|
| `~/.ahma/policy/*.cedar` | You. Inside `~/.ahma`, which no sandboxed command can write and no grant can open. |
| `/etc/ahma/policy/` (Linux), `/Library/Application Support/ahma/policy/` (macOS), `%ProgramData%\ahma\policy\` (Windows) | An administrator or MDM, for every user of the machine. |

A project's `.ahma/` is never read: an agent can write the repository it works in.

**A broken file fails closed.** If any file does not parse, contains a `permit`, or does not
match the schema, ahma refuses every permission it decides and every tool call, saying so,
until the file is fixed. `ahma policy validate` shows the problem.

## The schema

| Action | Resource | Asked when |
|---|---|---|
| `read`, `write` | `Ahma::Path` | a grant would open a path (any surface, any tier) |
| `connect` | `Ahma::Host` | a sandboxed command reaches a host through the egress proxy |
| `fetch` | `Ahma::Host` | ahma's `fetch_webpage` reads a URL |
| `open_link` | `Ahma::Host` | the link broker would open a link in your browser |
| `ssh_sign` | `Ahma::Host`, `Ahma::Namespace` | the SSH key broker would sign |
| `call_tool` | `Ahma::Tool` | an MCP tool call, a hooked shell command (`run_terminal_command`), or the TUI agent's tool |

- **Paths** are absolute and canonical (`/private/var/…`, not `/var/…`, on macOS). A path's parent is its parent directory, so `resource in Ahma::Path::"/a"` covers `/a` and everything under it.
- **Hosts** are lower case. A host's parent is the name with its first label removed, so `resource in Ahma::Host::"github.com"` covers `api.github.com`.
- **The principal** is `Ahma::Agent`, with an optional `workspace` attribute (the workspace's canonical path). Test it with `principal has workspace && …`, as Cedar's strict validation requires.
- `@reason("…")` is shown to whoever is refused. `@id("…")` names the rule; otherwise it is `<file stem>#<n>`.

The full schema: [`ahma_policy/ahma.cedarschema`](../ahma_policy/ahma.cedarschema).

## What reaches the kernel

Cedar cannot be evaluated per system call, so a rule reaches the kernel only through what ahma
builds:

| Rule | Held by |
|---|---|
| Forbids a grant (`read`/`write` on a path outside the scope) | ahma refuses the grant; the path never enters the kernel scope |
| Forbids a path *inside* the workspace | **Not enforced yet.** The workspace is in the kernel scope already, so such a rule refuses only grants. Enforcement (a Seatbelt deny on macOS, the namespace deny tier on Linux, and ahma's file tools) is planned (SPEC R-POLICY.4) |
| Forbids a host | the egress proxy (kernel-pinned to it on macOS and Linux 6.7+) and `fetch_webpage` |
| Forbids a tool | the MCP server and terminal hooks refuse the call |

A grant is refused when a rule forbids reading **or** writing its path, whatever access was
asked for; finer handling is planned.

## See also

- [permissions.md](permissions.md): grants, the question ladder, and the one ledger.
- [how-ahma-compares.md](how-ahma-compares.md): why the kernel stays the boundary.
- SPEC [R-POLICY](../SPEC.md#r-policy-one-policy-decision-point-and-team-policy-in-cedar).
