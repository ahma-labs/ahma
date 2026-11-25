# Tool configuration examples

Each program loads one of the tool definitions ahma bundles (the repository's
[`.ahma/`](../../.ahma/), compiled into the `ahma` binary and offered with `--tools`) and
validates it against the MTDF schema, printing the tool and its subcommands:

| Example | Validates |
|---|---|
| [git_tool.rs](git_tool.rs) | `.ahma/git.json` (the `git` bundle) |
| [gh_tool.rs](gh_tool.rs) | `.ahma/gh.json` (the `github` bundle) |
| [python_tool.rs](python_tool.rs) | `.ahma/python.json` (the `python` bundle) |
| [file-tools.rs](file-tools.rs) | `.ahma/file-tools.json` (the `fileutils` bundle) |

```bash
cargo run --example git_tool
```

Each exits non-zero when the file is missing or invalid. To check your own definitions, use
`ahma tool validate` instead.

## See also

- [docs/custom-tools.md](../../docs/custom-tools.md): writing tool definitions
- [docs/mtdf-schema.json](../../docs/mtdf-schema.json): the JSON Schema
