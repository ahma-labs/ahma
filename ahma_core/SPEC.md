# ahma_core Crate Specification

* **Status**: Approved
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_mcp`, `ahma_common`, `ahma_http_mcp_client`, `ahma_llm_monitor`
* **Used by**: `ahma_tui`, `ahma_bin`; external embedders

## 1. User Story / Problem Statement

*As an embedder, or as ahma's own TUI, I want a permissively licensed library that gives me
ahma's sandbox and operation tracking plus a working chat agent that drives ahma's tools, so
that I can build on it without adopting the AGPL.*

User guide: [docs/ahma-core-library.md](../docs/ahma-core-library.md).

## 2. Acceptance Criteria

**Embedding facade**
- Re-exports `Sandbox`, `SandboxMode`, `OperationMonitor`, `OperationStatus`,
  `MonitorConfig`, `Adapter`, `AhmaMcpService` (from `ahma_mcp`) and `LlmClient` (from
  `ahma_llm_monitor`).

**Chat agent (`agent`)**
- `execute_agent_turn` / `spawn_agent_task` run one chat turn: stream the model's reply,
  execute the tool calls it makes through ahma over MCP (the shared Streamable HTTP client,
  `ahma_http_mcp_client::streamable`), feed results back, and repeat until the model
  answers. Progress is reported as `AgentEvent`s.
- Every tool call passes an `AgentApprovalGate` before it runs.
- Context budgets: with `--context-length` a single tool result may use a quarter of the
  window and the conversation three quarters (≈4 chars/token), with proactive compaction;
  without it, `small_model_harness` selects tighter fixed budgets. Truncation keeps the
  head and tail and says what it cut; the full output stays in the operation's
  `output_file`.
- `minimize_tokens` appends a conciseness rule to the system prompt.
- `agent` itself is never offered to the agent loop (no recursion).

**Approvals (`approvals`)**
- "Always allow" grants are stored in the unified permission ledger
  (`[permissions].tool_approvals` in `~/.ahma/settings.toml`, R-PERM.1), keyed by workspace
  root: trusting a tool in one project does not trust it in another. `~/.ahma` is outside
  every sandbox scope (R5.4.8), so a sandboxed agent can neither read nor extend its grants.
  A legacy `approvals.json` is migrated once, non-destructively.

**Tool menu (`tool_menu`, R24.12.8)**
- With the small-model harness on (always, for a model on this machine) the model is offered
  the core tools plus `more_tools`, which opens further groups on request. Opening a group
  changes what is *offered*, never what is *permitted*. A tool that exists but was not
  offered is refused with the group that holds it — never silently run.

**Chat-agent MCP routing**

- **R10.7**: **Daemon Chat MCP Base URL Resolution**: When the background daemon (`ahma serve` hub) runs an agent task, the `McpChatConfig` base URL (`base_url`) MUST be resolved to the local MCP bridge server's endpoint (HTTP host/port or Unix socket path as configured in the active service's `AppConfig`) rather than being set to the LLM provider's base URL. This ensures local tool calls (e.g. `read_file`) are routed back to the local MCP bridge.
- **R10.8**: **TUI Window Chat MCP Resolution**: TUI window/subtask LLM tasks MUST be provided with the resolved `McpChatConfig` when running chat tasks to allow proper tool routing and sampling capabilities when requested.

## 3. Non-Functional Requirements

- **License boundary**: never depends on an AGPL crate (`ahma_tui`, `ahma_bin`);
  `scripts/check-license-boundaries.sh` enforces it.
- **Async hygiene**: approval reads inside a turn are async; no blocking I/O in the agent loop.

## 4. Out of Scope

- The terminal UI (`ahma_tui`).
