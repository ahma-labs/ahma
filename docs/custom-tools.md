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
  `log_monitor`, `ahma_help`, `ahma_command`). A config that takes one is refused at
  load with a message naming the conflict ([the full list](#where-tools-come-from)).
- **Say whether a tool writes.** A tool (or subcommand) that only reads can declare `"concurrency": "read_only"` and then runs at once even while a build holds the workspace — under a sandbox that cannot write the workspace, so a wrong declaration fails rather than writes. `"service"` marks a long-lived process that must not hold the workspace write queue for its whole life; the default is `"exclusive"`. A declaration on the tool applies to every subcommand that does not declare its own. See [workspace-queue.md](workspace-queue.md).
- **Say whether a tool changes anything.** `"mutates": false` on a tool or a subcommand says it changes nothing (the default is `true`, so an undeclared tool is assumed to). ahma's own agent loop then runs it without asking for approval, and `tools/list` marks it read-only (`readOnlyHint`) so a client such as Claude Code may skip its prompt too. A declared `"concurrency": "read_only"` earns the same hint only where the kernel enforces that lane (Linux and macOS); on Windows it does not. Declare `mutates: false` only for a tool that writes nothing and sends nothing anywhere you would not want unasked (SPEC R1.5.6).
- **`"synchronous"` only chooses how long a call waits.** `"synchronous": true` (deprecated) makes a call to that tool wait for its result, as the server's `sync` mode does, and `false` makes it answer after the short adaptive window even in `sync` mode; a caller can ask the same per call with `"blocking": true`/`false`. Either way the command runs as a tracked operation — it shows in `status` and the TUI, and cancelling the MCP request cancels it as an operation. A waiting call that outlasts what the client can hold one request open for returns the operation id to collect with `await`. Leave it out and let `tools.execution_mode` decide ([settings.md](settings.md#sync-or-async-toolsexecution_mode), SPEC R2.1, R2.3).
- **Tool types beyond plain commands:** `"tool_type": "livelog"` ([live-log-monitoring.md](live-log-monitoring.md)) and `"command": "sequence"` (SPEC §5.3).
- **Changes are not picked up automatically.** ahma does not watch the tool-config directory — a watcher would turn writing a tool definition into *running* it with no user action in between. Reload with the explicit `restart` tool (or restart the server).

## Where tools come from

ahma offers tools from three places, loaded once at startup:

1. **Built-in tools**, always present. Their 25 names are reserved:

   | Tools | What |
   |---|---|
   | `run_terminal_command`, `await`, `status`, `cancel`, `restart` | Run commands as tracked operations, collect, inspect, cancel, reload |
   | `sandbox_grant`, `network_grant` | Ask the human for a scope or an egress host |
   | `logs_list`, `logs_read`, `logs_search`, `logs_approve`, `log_monitor` | Project logs and live monitoring |
   | `read_file`, `write_file`, `replace_in_file`, `multi_edit`, `apply_patch`, `list_dir`, `file_search`, `grep_search`, `todo_write` | File tools; withheld from clients with their own ([file-tools.md](file-tools.md)) |
   | `fetch_webpage`, `agent`, `ahma_help` | A web page under `[web]` policy, a sub-agent, sandbox guidance |
   | `ahma_command` | Run ahma's own `update`, `web`, `desktop` or `doctor` outside the sandbox ([installation.md](installation.md#update-recommended)) |

   The list is generated from one place, `BuiltinTool::ALL` in `ahma_mcp/src/builtin_tool.rs`.
2. **Bundles**, compiled in and offered only with `--tools` (repeat or comma-separate, e.g.
   `ahma serve stdio --tools git,fileutils`): `fileutils` (`file-tools`), `git`, `github`
   (`gh`), `python`, `simplify`. An unknown name is ignored with a warning.
3. **Your project's `.ahma/*.json`**, loaded automatically. A definition with a bundled tool's
   name replaces that tool.

## A minimal tool

Every definition follows the MTDF schema ([mtdf-schema.json](mtdf-schema.json)); only `name`,
`description` and `command` are required:

```json
{
  "name": "mytool",
  "description": "What the tool does",
  "command": "command-to-execute",
  "enabled": true,
  "timeout_seconds": 300,
  "subcommand": [
    {
      "name": "subcommand_name",
      "description": "What this subcommand does",
      "options": [
        { "name": "option-name", "type": "string", "description": "What it sets", "required": false }
      ]
    }
  ]
}
```

Mark a path argument `"format": "path"` so ahma validates it against the sandbox scope. Worked
definitions live in this repository's own [`.ahma/`](../.ahma/) and in
[`ahma_mcp/examples/`](../ahma_mcp/examples/).

## Validate

```bash
ahma tool validate              # every file in ./.ahma
ahma tool validate .ahma/mytool.json
```

From Rust: `ahma_mcp::schema_validation::MtdfValidator::validate_tool_config(path, &content)`.

## Troubleshooting

- **The tool does not appear.** Check the JSON parses (`ahma tool validate`) and has
  `"enabled": true`, then call `restart` or restart the server: definitions are read only at
  startup.
- **Refused at load.** The message names the field or the reserved name in conflict.
- **It appears but fails.** The command it runs must be installed and on `PATH` inside the
  sandbox; run it once with `run_terminal_command` to see the error.
