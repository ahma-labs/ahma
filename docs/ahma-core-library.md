# ahma_core — Embedding Ahma in Rust Applications

> **License**: `ahma_core` is dual-licensed under **MIT OR Apache-2.0**.

`ahma_core` is a re-export crate that exposes Ahma's permissive secure execution
primitives as an embeddable Rust library.

## Adding to your project

```toml
[dependencies]
ahma_core = { git = "https://github.com/ahma-labs/ahma.git" }
```

## Available primitives (MIT OR Apache-2.0)

| Type | Purpose |
|------|---------|
| `Sandbox` / `SandboxMode` | Kernel-level filesystem sandbox |
| `OperationMonitor` | Async operation tracking and cancellation |
| `MonitorConfig` / `OperationStatus` | Monitor configuration and status |
| `AhmaMcpService` | Full MCP server service |
| `Adapter` | CLI tool execution adapter |
| `LlmClient` | OpenAI-compatible LLM client |

## AGPL-licensed sibling crates

The following primitives are available in separate AGPL-licensed crates. Add
them to your `Cargo.toml` only if you accept the applicable license terms for
those crates.

| Crate | License | Key types |
|-------|---------|----------|
| `ahma_tui` | AGPL-3.0-or-later | `TuiApp`, `TuiEvent`, `run_tui` |

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
    let sandbox = Arc::new(Sandbox::new(vec![], SandboxMode::Strict, false, false, false)?);
    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(300),
    )));
    println!("Sandbox and monitor ready.");
    Ok(())
}
```

## See also

- [docs/task-vault.md](task-vault.md)
- [docs/egress-sandbox.md](egress-sandbox.md)
