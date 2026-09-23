# ahma_core Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As an embedder of the Ahma execution engine, I want to use a permissive-licensed (MIT/Apache-2.0) core library so that I can build custom secure command execution wrappers and MCP servers without adopting copyleft (AGPL) constraints.*

## 2. Acceptance Criteria

- **Crate License**: Must be dual-licensed under MIT OR Apache-2.0.
- **Embedded Sandbox**: Re-exports `ahma_mcp::sandbox::Sandbox` and `ahma_mcp::sandbox::SandboxMode` to expose Landlock (Linux) and Seatbelt (macOS) kernel-level filesystem sandboxing.
- **Operation Monitoring**: Re-exports `ahma_mcp::operation_monitor::{OperationMonitor, OperationStatus, MonitorConfig}` for tracking background running tasks.
- **MCP Service Adapter**: Re-exports `ahma_mcp::{Adapter, AhmaMcpService}` for running declarative tool executions.
- **LLM Support**: Re-exports `ahma_llm_monitor::LlmClient` for OpenAI-compatible logging and monitoring integrations.

### Chat-agent MCP routing

- **R10.7**: **Daemon Chat MCP Base URL Resolution**: When the background daemon (`ahma serve` hub) runs an agent task, the `McpChatConfig` base URL (`base_url`) MUST be resolved to the local MCP bridge server's endpoint (HTTP host/port or Unix socket path as configured in the active service's `AppConfig`) rather than being set to the LLM provider's base URL. This ensures local tool calls (e.g. `read_file`) are routed back to the local MCP bridge.
- **R10.8**: **TUI Window Chat MCP Resolution**: TUI window/subtask LLM tasks MUST be provided with the resolved `McpChatConfig` when running chat tasks to allow proper tool routing and sampling capabilities when requested.

## 3. Non-Functional Requirements

- **Performance**: Must introduce zero overhead on top of raw `ahma_mcp` executions.
- **Dependency Isolation**: Must not transitively drag in copyleft AGPL workspace dependencies (like `ahma_tui`).

## 4. Out of Scope

- Implementing TUI control planes directly (these remain in separate AGPL-licensed crates).
