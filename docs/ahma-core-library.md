# ahma_core — Embedding Ahma in Rust Applications

> **License**: `ahma_core` is dual-licensed under **MIT OR Apache-2.0**.

`ahma_core` exposes Ahma's permissive secure execution primitives, and the chat agent
that drives ahma's tools, as an embeddable Rust library.

## Adding to your project

```toml
[dependencies]
ahma_core = { git = "https://github.com/ahma-labs/ahma.git" }
```

ahma's HTTP stack needs `--cfg reqwest_unstable`. Add to your project's `.cargo/config.toml`:

```toml
[build]
rustflags = ["--cfg", "reqwest_unstable"]
```

## Available primitives (MIT OR Apache-2.0)

| Type | Purpose |
|------|---------|
| `Sandbox` / `SandboxMode` / `SandboxOptions` | Kernel-level filesystem sandbox |
| `OperationMonitor` | Async operation tracking and cancellation |
| `MonitorConfig` / `OperationStatus` | Monitor configuration and status |
| `AhmaMcpService` | Full MCP server service |
| `Adapter` | CLI tool execution adapter |
| `LlmClient` | OpenAI- and Anthropic-compatible LLM client |
| `agent::{execute_agent_turn, spawn_agent_task}`, `McpChatConfig`, `AgentEvent`, `AgentApprovalGate` | One chat turn: model reply plus the ahma tool calls it makes, over MCP; every tool call passes the approval gate |
| `advisor` | A model that recommends (never gives) an answer to a scope-grant question (R-PERM.8) |
| `approvals` | Per-workspace "always allow" tool grants in the permission ledger |
| `tool_menu` | Which tools a model is offered; how a small model asks for more |

## AGPL-licensed sibling crates

The following primitives are available in separate AGPL-licensed crates. Add
them to your `Cargo.toml` only if you accept the applicable license terms for
those crates.

| Crate | License | Key types |
|-------|---------|----------|
| `ahma_tui` | AGPL-3.0 | `run_tui` and the terminal UI |
| `ahma_gui` | AGPL-3.0 | The `ahma web` / `ahma desktop` surface |
| `ahma_bin` | AGPL-3.0 | The `ahma` binary |

Embedding any of these AGPL crates means any modified version offered to remote users
over a network must publish its modified source code (AGPL-3.0 §13).

Each crate's `Cargo.toml` is the authoritative license declaration.

## Minimal example: sandbox + monitor

```rust
use ahma_core::{Sandbox, SandboxMode, OperationMonitor, MonitorConfig};
use std::time::Duration;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sandbox = Arc::new(Sandbox::new(vec![], SandboxMode::Strict, Default::default())?);
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(300),
    )));
    println!("Sandbox and monitor ready.");
    Ok(())
}
```

## See also

- [docs/task-vault.md](task-vault.md)
- [docs/network-egress.md](network-egress.md)
