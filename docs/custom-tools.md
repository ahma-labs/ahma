# Custom Tools

Custom tool definitions are an advanced ahma workflow. They are useful when you want to expose a project-specific command or restrict the arguments available to an existing CLI tool, but they are not required for normal day-to-day use.

If your main goal is to use ahma with existing tools such as cargo, git, Python, shell commands, or file utilities, start with [README.md](../README.md) instead.

## When custom tools help

- you want to expose a project-specific script through MCP
- you want tighter argument-level control than direct `run_terminal_command` access
- you want local `.ahma/*.json` overrides for a particular repository

## Two rules to know before you start

- **A workspace's `.ahma/` directory is not agent-writable.** It defines the commands ahma will itself run, so an agent that could write it could define a tool and then call it. Writes there are denied (SPEC R-HANDOFF.7); you author these files yourself.
- **Changes are not picked up automatically.** ahma does not watch the tool-config directory — a watcher would turn writing a tool definition into *running* it with no user action in between. Reload with the explicit `restart` tool (or restart the server).

## Where to start

The canonical tool-definition guide lives in [.ahma/README.md](../.ahma/README.md).

That guide covers:

- built-in tools versus bundled tools versus local `.ahma/` overrides
- validation and testing of JSON tool definitions
- override rules and security considerations

For the MTDF schema itself, see [docs/mtdf-schema.json](mtdf-schema.json).