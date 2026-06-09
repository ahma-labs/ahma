# ahma_bin Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a CLI user, I want a single unified `ahma` executable so that I can serve MCP stdio/HTTP endpoints, manage task vaults, run terminal commands, and launch the terminal UI from my command line.*

## 2. Acceptance Criteria

- **Subcommand Dispatch**: Implements clap-based parser CLI dispatching to:
  - `serve stdio` — standard stdio MCP server mode
  - `serve http` — HTTP bridge proxy mode
  - `tool run/validate/list/info` — run or query tools directly from CLI
  - `vault create/list` — manage task vaults
  - `tui` — launch terminal user interface
  - `tls init/rotate/status` — manage local self-signed TLS certificates
  - `llm list/add/remove/test` — manage LLM providers in configuration
  - `cluster list/add-peer/remove/ping/status/discover/announce/cert` — cluster management
- **PowerShell Check (Windows)**: Emits a startup warning and exits if PowerShell (pwsh) is not present on Windows systems.
- **Markdown Help**: Emits the full CLI command reference as Markdown when invoked with `--markdown-help`.
- **Settings Loader**: Reads configurations from `~/.ahma/settings.toml` unless overridden by `--no-settings` or `--settings-path`.

## 3. Non-Functional Requirements

- **Binary Portability**: Compiles cleanly into a single static binary.
- **License Compliance**: Subject to the AGPL-3.0-or-later license, enforcing source disclosure for distributed changes.

## 4. Out of Scope

- Implementing core logic; `ahma_bin` is the thin CLI wiring layer.
