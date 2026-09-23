# ahma_bin Crate Specification

* **Status**: Approved
* **License**: AGPL-3.0
* **Depends on**: `ahma_mcp`, `ahma_core`, `ahma_tui`, `ahma_common`; `ahma_simplify` (optional, feature `simplify`)
* **Used by**: end users (the `ahma` binary)

## 1. User Story / Problem Statement

*As a CLI user, I want a single unified `ahma` executable so that I can serve MCP stdio/HTTP endpoints, run and inspect tools, manage permissions and settings, and launch the terminal UI from my command line.*

## 2. Acceptance Criteria

- **Thin entry point**: `main` parses `ahma_mcp::shell::cli::Cli` and dispatches. The
  subcommand set is the `Subcommands` enum in `ahma_mcp/src/shell/cli/mod.rs` (run
  `ahma --help`); this crate adds only the handlers that need AGPL crates — `tui`,
  `simplify`, `llm` — and never duplicates the list.
- **AppContainer re-entry first**: on Windows, `ahma.exe` re-executes itself to launch each
  sandboxed command, so `appcontainer_launcher_hook()` runs before clap parses anything.
- **Hooks fail open on a parse error**: a malformed `ahma hooks exec …` (for example a hook
  file written by another ahma version) emits a concise `allow` decision and exits 0, rather
  than a usage banner the editor would show as "Hook blocked". `--help`/`--version` still
  print normally.
- **Markdown help**: `ahma --markdown-help` prints the whole CLI reference as Markdown.
- **PowerShell check (Windows)**: warns and exits if Windows PowerShell 5.1+ is missing
  (R6.3.6; the requirement is `powershell`, not `pwsh`).
- **Features**: `simplify` (default) links `ahma_simplify`; without it `ahma simplify` fails
  with an error naming the feature. `otel` enables OpenTelemetry export; `full` = both.

## 3. Non-Functional Requirements

- **License Compliance**: Subject to the AGPL-3.0 license, enforcing source disclosure for distributed changes.

## 4. Out of Scope

- Implementing core logic; `ahma_bin` is the thin CLI wiring layer.
