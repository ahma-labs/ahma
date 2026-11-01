# `ahma_help` — guidance for the AI, from ahma

**Status:** Stable. SPEC: [R-HELP](../SPEC.md).

## Why

An AI agent that meets `Operation not permitted` has three bad options: retry the
same command, copy caches into your project to get around it, or stop and ask you.
Each costs a round trip, and the second leaves a mess. `ahma_help` tells the agent
what to try first, in its own terms (a flag, an environment variable, a folder to
put files in), and the one command to ask you for when nothing else works. The
text is generated from the live session, so what it says about temp space is true
for this machine and this configuration.

It is also the place a project's `AGENTS.md` or `CLAUDE.md` no longer has to
explain how to call commands inside a sandbox.

## Quickstart

An agent calls it like any other ahma tool:

```
ahma_help()                  # the index
ahma_help(topic="temp")      # is temp space available, where, and what it is good for
ahma_help(topic="blocked")   # a command failed with 'Operation not permitted'
```

It works before the sandbox scope has settled, because that is when an agent most
needs it, through the HTTP bridge and hub as well as a direct connection. The
server instructions every MCP client gives its model name it.

An agent that reaches ahma only through a terminal hook (Claude Code's or
Cursor's own shell) has no MCP tool. It runs the same text as a command:

```
ahma explain              # the index
ahma explain blocked      # what to try when a command fails with 'Operation not permitted'
ahma explain temp
```

`ahma explain` never waits behind a build and writes nothing. Inside a hooked
command it cannot read `~/.ahma`, so it reports temp as "probably available"
and names the one switch (`--no-temp-files`) that would turn it off.

## Topics

| Topic | What it covers |
|---|---|
| `start` | What ahma does to commands, the three places a command can write, and whether temp space is available |
| `temp` | Whether the temp directory is available to commands, its path, and recipes that make common tools work with it |
| `blocked` | The order to try things in when a write is denied: the tool's own option, a writable place, then a human grant |
| `slow` | Long-running commands, `await`, and why a command that waits for a person blocks others |
| `readonly` | Which commands skip the workspace write queue |
| `roots` | "Sandbox initializing from client roots" and "no sandbox scope", and the flag that fixes them |

## Temp space

The kernel sandbox lets commands write `/tmp` on Linux and `/private/tmp` and
`/private/var/folders` on macOS, unless the session was started with
`--no-temp-files`. `ahma_help topic=temp` reports which of these is true, the
real path, and a warning if `TMPDIR` points somewhere commands cannot write. It
shows how to make a folder of your own (`mktemp -d`) and how to point common tools
at it (`XDG_CACHE_HOME`, `pip --cache-dir`, `npm --cache`, `CARGO_TARGET_DIR`,
`GOCACHE`, `PYTHONPYCACHEPREFIX`, `GRADLE_USER_HOME`).

Temp is shared with every process on the machine and cleaned on the operating
system's schedule. ahma's own file tools and `working_directory` still treat it as
outside the workspace (R5.2.5). Caches worth keeping between sessions belong in a
gitignored folder in the workspace.

## When a human is needed

The last step of `blocked` is the only one that involves you: the agent asks for
the narrowest directory with `ahma sandbox grant <dir>`. Paths no grant can unlock
(credentials, ahma's own settings) keep their refusal text; `ahma_help` does not
suggest a way around them.

## See also

- [docs/security-sandbox.md](security-sandbox.md) — what the sandbox allows and why
- [docs/permissions.md](permissions.md) — grants and the permission ledger
- [docs/doctor.md](doctor.md) — checks ahma's own state
- SPEC: R-HELP, R5.2.5
