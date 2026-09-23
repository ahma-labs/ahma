# Ahma Tool Configurations

This directory contains tool configuration files for the Ahma server (ahma). These configurations define how AI agents can interact with various command-line tools in a safe and structured way.

## How Tool Loading Works

AHMA has a three-tier tool model:

### 1. Core Built-in Tools (always available, no configuration needed)

Implemented directly in Rust. **All twenty names below are reserved**: a tool
config that takes one is refused at load, naming the conflict and asking you to
rename. (Before v0.20.1 only fifteen were on the reserved list, so a config
named `sandbox_grant`, `agent`, `todo_write`, `log_monitor` or `logs_approve`
loaded without complaint and was then silently dropped from `tools/list` — no
error, no tool. It is now refused properly.)

| Tool | What it does |
|---|---|
| `run_terminal_command` | Execute shell commands in the security sandbox |
| `await` | Block until an async operation completes |
| `status` | Query operation status without blocking |
| `cancel` | Cancel running operations |
| `restart` | Reload tool configs by replacing the server process |
| `sandbox_grant` | Ask the user to widen the sandbox scope |
| `logs_list` / `logs_read` / `logs_search` / `logs_approve` | Inspect and approve access to the project log directory |
| `read_file` / `write_file` / `replace_in_file` / `list_dir` / `file_search` / `grep_search` | Harness file tools — withheld from clients that ship native equivalents |
| `fetch_webpage` | Fetch a URL through the egress sandbox |
| `agent` | Run a sub-agent task |
| `todo_write` | Record a task list |
| `log_monitor` | Start LLM-backed live log monitoring |

The list is generated from one place in the source (`BuiltinTool::ALL` in
`ahma_mcp/src/builtin_tool.rs`), so it cannot drift from what the server
actually answers.

### 2. Bundled Tool Configs (opt-in via the `--tools` flag)
Standard tool configurations are compiled into the `ahma` binary. They are only offered to MCP clients when explicitly enabled via `--tools <bundle>` (repeat or comma-separate):

| Bundle | Tool Name | Description |
|------|-----------|-------------|
| `--tools rust` | `cargo` | Rust build, test, clippy, fmt, etc. |
| `--tools fileutils` | `file-tools` | Unix file operations (ls, cp, mv, rm, grep, etc.) |
| `--tools git` | `git` | Git version control |
| `--tools github` | `gh` | GitHub CLI (PRs, issues, releases) |
| `--tools python` | `python` | Python interpreter and pip |
| `--tools simplify` | `simplify` | Code complexity metrics |

Example: `ahma serve stdio --tools rust,git,fileutils`

### 3. Local `.ahma/` Overrides (automatic)
If a `.ahma/` directory exists in the current working directory, all `*.json` files in it are loaded automatically at startup — no CLI flag needed.

**Override rule:** If a local `.ahma/*.json` file defines a tool with the same `name` as a bundled tool, the local version **replaces** the bundled one entirely. This lets you customize tool descriptions, options, and subcommands for your project.

Example: placing a `.ahma/rust.json` with `"name": "cargo"` will override the bundled cargo tool definition when `--rust` is also passed.

## Available Tool Configurations

Run the validation tool to ensure your configuration is correct:

```bash
# Validate a specific configuration using the example runners
cargo run --example cargo_tool
cargo run --example file-tools
cargo run --example gh_tool
cargo run --example git_tool
cargo run --example python_tool

# Or run schema validation tests
cargo test --test tool_config_schema_validation_test

# Or run all tests including execution tests
cargo nextest run --package ahma --test tool_config_schema_validation_test
cargo nextest run --package ahma --test tool_examples_execution_test
```

### 4. Verify Your Configuration Works

After copying and enabling a configuration in `.ahma/`, restart the ahma server to load the new tool. Tool definitions are read once at startup and are never re-read from disk while the server runs — there is no watch mode. While iterating on a definition, call the `restart` tool (or restart the server) to pick up the edit:

```bash
# Configs in .ahma/ are loaded once, at startup
ahma serve stdio --tools-dir .ahma
```

The tools directory lives inside the sandbox scope, so a running agent can write it; re-reading it at runtime would let that agent repoint an already-approved tool name at any command. `restart` keeps the reload explicit and auditable.

## Configuration Format

All tool configurations follow the MCP Tool Definition Format (MTDF) schema. Only `name`, `description`, and `command` are required. Here's a minimal example:

```json
{
    "name": "mytool",
    "description": "Description of what the tool does",
    "command": "command-to-execute",
    "enabled": true,
    "timeout_seconds": 300,
    "subcommand": [
        {
            "name": "subcommand_name",
            "description": "What this subcommand does",
            "options": [
                {
                    "name": "option-name",
                    "type": "string",
                    "description": "What this option does",
                    "required": false
                }
            ]
        }
    ]
}
```

By default a tool call waits for its result (`tools.execution_mode = "sync"`); with
`--async` (or `execution_mode = "async"`) a command that is still running after a
few seconds returns an operation ID to collect with `await`. Choose the mode for
the server rather than with the per-subcommand `synchronous` field, which is
deprecated. See [docs/settings.md](../docs/settings.md#sync-or-async-toolsexecution_mode).

## Validation Tools

### Command-Line Validation

```bash
# Validate all example configs
cargo nextest run -p ahma_mcp tool_config_schema_validation

# Run a specific example to see detailed output
cargo run --example cargo_tool

# Check if configuration is parseable
jq . .ahma/cargo.json
```

### Programmatic Validation

Use the `MtdfValidator` from `ahma`:

```rust
use ahma::schema_validation::MtdfValidator;
use std::path::Path;

let validator = MtdfValidator::new();
let config_path = Path::new(".ahma/mytool.json");
let content = std::fs::read_to_string(config_path)?;

match validator.validate_tool_config(config_path, &content) {
    Ok(config) => println!("OK Valid configuration"),
    Err(errors) => {
        eprintln!("FAIL Validation errors:");
        for error in errors {
            eprintln!("  - {}: {}", error.field_path, error.message);
        }
    }
}
```

## Security Considerations

- **Path Security**: All file paths are automatically validated and scoped to the current working directory
- **Sandbox Mode**: Commands run in isolated environments with restricted permissions
- **Timeout Protection**: All operations have configurable timeouts to prevent hanging
- **Command Whitelisting**: Only explicitly configured commands and subcommands are available

## Troubleshooting

### Configuration Not Loading

1. Ensure the JSON file is valid: `jq . .ahma/mytool.json`
2. Check that `"enabled": true` is set
3. Verify file permissions: `ls -la .ahma/`
4. Check server logs for parsing errors

### Validation Fails

1. Run the corresponding example: `cargo run --example mytool`
2. Check for schema violations in the error output
3. Compare with working examples in `ahma/examples/configs/`
4. Verify all required fields are present: `name`, `description`, `command`, `enabled`

### Tool Not Available in AI

1. Restart the `ahma` server
2. Verify tool is enabled: `grep enabled .ahma/mytool.json`
3. Check server initialization logs
4. Ensure the underlying command is installed: `which command-name`

## Schema Documentation

Full MTDF schema documentation is available at:
- `docs/mtdf-schema.json` - JSON Schema definition
- `ahma/docs/mtdf-schema.json` - Core library schema

## Contributing

To add a new tool configuration:

1. Create the JSON file in `ahma/examples/configs/`
2. Add a corresponding example in `ahma/examples/toolname.rs`
3. Add tests in `ahma_mcp/tests/unit/tool_config_schema_validation_test.rs`
4. Add execution tests in `ahma_mcp/tests/unit/tool_examples_execution_test.rs`
5. Update `ahma/Cargo.toml` with example declaration
6. Run all tests: `cargo nextest run --workspace`

## License

Tool configurations in this directory follow the project's dual MIT/Apache-2.0 license.
