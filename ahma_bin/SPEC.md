# ahma_bin Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a CLI user, I want a single unified `ahma` executable so that I can serve MCP stdio/HTTP endpoints, run and inspect tools, manage permissions and settings, and launch the terminal UI from my command line.*

## 2. Acceptance Criteria

- **Subcommand Dispatch**: Implements clap-based parser CLI dispatching to:
  - `serve stdio` — standard stdio MCP server mode
  - `serve http` — HTTP bridge proxy mode
  - `tool run/validate/list/info` — run or query tools directly from CLI
  - `tui` — launch terminal user interface
  - `tls init/rotate/status` — manage local self-signed TLS certificates
  - `llm list/add/remove/test` — manage LLM providers in configuration
  - `settings init/show` — write and inspect the settings file, with provenance
  - `permissions list/grant/revoke` — the unified permission ledger (root SPEC R-PERM)
  - `bundle audit/checksum/verify` — supply-chain audit and a SHA-256 content manifest
- **PowerShell Check (Windows)**: Emits a startup warning and exits if Windows PowerShell (5.1+, built into Windows 10/11) is not present. Root SPEC R6.3.6 governs; the runtime requirement is `powershell`, not `pwsh`.
- **Markdown Help**: Emits the full CLI command reference as Markdown when invoked with `--markdown-help`.
- **Settings Loader**: Reads `~/.ahma/settings.toml`, then layers `<workspace>/.ahma/settings.toml` over it for preference-tier keys only (root SPEC R-CFG3). `--no-settings` ignores both; `--settings-path` replaces the user file.

## 3. Non-Functional Requirements

- **Binary Portability**: Compiles cleanly into a single static binary.
- **License Compliance**: Subject to the AGPL-3.0-or-later license, enforcing source disclosure for distributed changes.

## 4. Out of Scope

- Implementing core logic; `ahma_bin` is the thin CLI wiring layer.
