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

## 3. Non-Functional Requirements

- **Performance**: Must introduce zero overhead on top of raw `ahma_mcp` executions.
- **Dependency Isolation**: Must not transitively drag in copyleft AGPL workspace dependencies (like `ahma_vault`, `ahma_tui`, or `ahma_cluster`).

## 4. Out of Scope

- Implementing task vaults, TUI control planes, cluster scheduling, or worker code synthesis directly (these remain in separate AGPL-licensed crates).
