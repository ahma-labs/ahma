# Custom Tools

Custom tool definitions are an advanced ahma workflow. They are useful when you want to expose a project-specific command or restrict the arguments available to an existing CLI tool, but they are not required for normal day-to-day use.

If your main goal is to use ahma with existing tools such as cargo, git, Python, shell commands, or file utilities, start with [README.md](../README.md) instead.

## When custom tools help

- you want to expose a project-specific script through MCP
- you want tighter argument-level control than direct `run_terminal_command` access
- you want local `.ahma/*.json` overrides for a particular repository

## Rules to know before you start

- **A workspace's `.ahma/` directory is not agent-writable.** It defines the commands ahma will itself run, so an agent that could write it could define a tool and then call it. Writes there are denied (SPEC R-HANDOFF.7); you author these files yourself.
- **The names ahma answers itself are reserved.** (`run_terminal_command`, `await`,
  `status`, `cancel`, `restart`, `sandbox_grant`, `network_grant`, the four
  `logs_*`, the file tools, `fetch_webpage`, `agent`, `todo_write`,
  `log_monitor`, `ahma_help`). A config that takes one is refused at
  load with a message naming the conflict — see
  [.ahma/README.md](../.ahma/README.md) for the full table.
- **Say whether a tool writes.** A tool (or subcommand) that only reads can declare `"concurrency": "read_only"` and then runs at once even while a build holds the workspace — under a sandbox that cannot write the workspace, so a wrong declaration fails rather than writes. `"service"` marks a long-lived process that must not hold the workspace write queue for its whole life; the default is `"exclusive"`. A declaration on the tool applies to every subcommand that does not declare its own. See [workspace-queue.md](workspace-queue.md).
- **Say whether a tool changes anything.** `"mutates": false` on a tool or a subcommand says it changes nothing (the default is `true`, so an undeclared tool is assumed to). ahma's own agent loop then runs it without asking for approval, and `tools/list` marks it read-only (`readOnlyHint`) so a client such as Claude Code may skip its prompt too. A declared `"concurrency": "read_only"` earns the same hint only where the kernel enforces that lane (Linux and macOS); on Windows it does not. Declare `mutates: false` only for a tool that writes nothing and sends nothing anywhere you would not want unasked (SPEC R1.5.6).
- **`"synchronous"` only chooses how long a call waits.** `"synchronous": true` (deprecated) makes a call to that tool wait for its result, as the server's `sync` mode does, and `false` makes it answer after the short adaptive window even in `sync` mode; a caller can ask the same per call with `"blocking": true`/`false`. Either way the command runs as a tracked operation — it shows in `status` and the TUI, and cancelling the MCP request cancels it as an operation. A waiting call that outlasts what the client can hold one request open for returns the operation id to collect with `await`. Leave it out and let `tools.execution_mode` decide ([settings.md](settings.md#sync-or-async-toolsexecution_mode), SPEC R2.1, R2.3).
- **Tool types beyond plain commands:** `"tool_type": "livelog"` ([live-log-monitoring.md](live-log-monitoring.md)) and `"command": "sequence"` (SPEC §5.3).
- **Changes are not picked up automatically.** ahma does not watch the tool-config directory — a watcher would turn writing a tool definition into *running* it with no user action in between. Reload with the explicit `restart` tool (or restart the server).

## Where to start

The canonical tool-definition guide lives in [.ahma/README.md](../.ahma/README.md).

That guide covers:

- built-in tools versus bundled tools versus local `.ahma/` overrides
- validation and testing of JSON tool definitions
- override rules and security considerations

For the MTDF schema itself, see [docs/mtdf-schema.json](mtdf-schema.json).