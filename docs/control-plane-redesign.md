# Ahma Control Plane Redesign Plan

This document tracks the step-by-step execution of turning `ahma tui` into a thin-client control plane and moving the agent harness/MCP client management to the `ahma serve` daemon.

## Execution Checklist

### [x] Phase 1: Extract Agent Harness to Core
- [x] Scaffold new module/crate or use `ahma_core` for the extracted logic
- [x] Move `execute_agent_turn` loop and LLM communication out of `ahma_tui/src/llm_bridge.rs`
- [x] Move context trimming and budget calculations
- [x] Move small-model hint injection (`push_tool_message_with_hints`, etc.)
- [x] Add unit tests for the extracted agent loop in isolation (no UI dependency)

### [x] Phase 2: Move MCP Client Management to Daemon
- [x] Move `McpConnectionManager` out of `ahma_tui/src/mcp_connections.rs` into `ahma_mcp` or `ahma_core`
- [x] Move `mcp.json` IDE client config discovery to the daemon startup/serve logic
- [x] Wire background stdio/HTTP MCP client routing directly into `ahma serve`
- [x] Add tests for daemon-level tool routing

### [x] Phase 3: Expand Daemon Hub Protocol
- [x] Extend `ClientMsg` and `DaemonMsg` protocol enums in `ahma_common::daemon_hub` to support:
  - Prompt submission from client
  - Chat tokens streamed from daemon
  - Security approval requests (elevation, renewal) sent to TUI
  - Approval decisions returned to daemon
- [x] Update daemon to execute the agent loop asynchronously when a prompt is received
- [x] Write integration tests for the new protocol stream

### [x] Phase 4: Refactor TUI to Thin Client
- [x] Strip out LLM/agent code from `ahma_tui`
- [x] Strip out direct process management of external MCP stdio servers from `ahma_tui`
- [x] Implement event-based UI rendering that subscribes to the daemon event stream
- [x] Render tokens dynamically, pop up approval gates, and send user keypresses back
- [x] Verify that exiting the TUI does not terminate the background agent loop or external MCP processes

---

## Log of Completed Work
*(Add dated logs here as items are completed.)*
