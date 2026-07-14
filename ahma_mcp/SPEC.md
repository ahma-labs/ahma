# ahma_mcp Crate Specification

* **Status**: Approved
* **Date**: 2026-06-12

## 1. User Story / Problem Statement

*As an AI coding agent or client developer, I want to expose declarative CLI tools as MCP tools and run them asynchronously in a kernel-enforced sandbox so that execution is secure, fast, and non-blocking.*

## 2. Acceptance Criteria

- **Configuration-Driven**: Loads and validates MTDF JSON configurations from a `tools/` directory (default: `.ahma/`) against the MTDF schema at startup.
- **Async-First Execution**: Spawns tasks asynchronously in the background and returns a session-unique operation ID immediately.
- **Unified Event Stream**: All operation lifecycle data (Started / OutputLine / Progress / Alert / terminal) flows through one `OperationEvent` broadcast; the `OperationMonitor` is the single lifecycle emitter, and exactly one terminal event is emitted per operation.
- **MCP Progress Push**: An event-stream subscriber (`mcp_service::progress_push`) forwards events for operations with a registered client `progressToken` as `notifications/progress`. Push is best-effort; results are always retrievable via `await`/`status`.
- **Output Spill**: The complete redacted output of every async operation is written to `<project log dir>/operations/<operation_id>.log` and advertised as `output_file` in results; the inline result window is bounded for token economy.
- **Kernel Sandbox**: Implements path-validation rules and platform-specific kernel sandboxing (Landlock on Linux, Seatbelt on macOS, Job Objects on Windows).
- **Built-in tools**: Provides core internal tools `status`, `await`, `cancel`, and `run_terminal_command` regardless of external configuration.
- **Hot-Reloading**: Opt-in watching of the tools directory to reload definitions on the fly.
- **Supply Chain Audit**: Implements `ahma bundle audit/sign/verify` commands to scan tool definitions for security risks.

## 3. Non-Functional Requirements

- **Latency**: End-to-end async dispatch (start → terminal event) for a trivial command must stay well under 1s (guarded by the ignored `latency_guard_test` benchmarks; currently ~6ms median via direct sandboxed spawn). Note: the former pre-warmed shell pool was removed as dead code; execution runs via direct sandboxed spawns and `ShellSessionManager` PTY sessions.
- **Streaming Cost**: Per-line streaming (redaction, bounded collection, spill, tail/event emission) must stay non-pathological — 5000 lines under 10s (currently ~37ms).
- **Security**: Strict path validation preventing directory traversal or symlink escapes (`dunce::canonicalize`).
- **Hygiene**: Strict async I/O hygiene; child process spawns must enforce `kill_on_drop(true)`.

## 4. Out of Scope

- Exposing the server over network sockets (TCP/HTTP) directly; network transport is handled by `ahma_http_bridge`.
