# ahma_mcp Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As an AI coding agent or client developer, I want to expose declarative CLI tools as MCP tools and run them asynchronously in pre-warmed, sandboxed shells so that execution is secure, fast, and non-blocking.*

## 2. Acceptance Criteria

- **Configuration-Driven**: Loads and validates MTDF JSON configurations from a `tools/` directory (default: `.ahma/`) against the MTDF schema at startup.
- **Async-First Execution**: Spawns tasks asynchronously in the background and returns a session-unique operation ID immediately.
- **Shell Pooling**: Maintains a pre-warmed shell pool (bash on Unix, pwsh/powershell on Windows) to keep command startup latency under 20ms.
- **Kernel Sandbox**: Implements path-validation rules and platform-specific kernel sandboxing (Landlock on Linux, Seatbelt on macOS, Job Objects on Windows).
- **Built-in tools**: Provides core internal tools `status`, `await`, `cancel`, and `run_terminal_command` regardless of external configuration.
- **Hot-Reloading**: Opt-in watching of the tools directory to reload definitions on the fly.
- **Supply Chain Audit**: Implements `ahma bundle audit/sign/verify` commands to scan tool definitions for security risks.

## 3. Non-Functional Requirements

- **Latency**: Under 20ms command startup via pre-warmed shells.
- **Security**: Strict path validation preventing directory traversal or symlink escapes (`dunce::canonicalize`).
- **Hygiene**: Strict async I/O hygiene; child process spawns must enforce `kill_on_drop(true)`.

## 4. Out of Scope

- Exposing the server over network sockets (TCP/HTTP) directly; network transport is handled by `ahma_http_bridge`.
