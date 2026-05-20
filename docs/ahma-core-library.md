# ahma_core — Embedding Ahma in Rust Applications

> **Experimental** — introduced in v0.7. The public API surface may change before stabilisation.

`ahma_core` is a re-export crate that exposes Ahma's secure execution primitives as an embeddable Rust library. Use it to add kernel-enforced sandboxing, per-task vaults, and local-LLM orchestration to your own Rust application without depending on the full `ahma_mcp` crate.

## Adding to your project

```toml
[dependencies]
ahma_core = { git = "https://github.com/paulirotta/ahma.git" }
```

## Available primitives

| Type | Purpose |
|------|---------|
| `TaskVault` | Create, open, and manage per-task vaults |
| `AuditWriter` | Append events to a vault's `audit.jsonl` |
| `TrashManager` | Two-phase delete within a vault |
| `EgressAllowlist` | Per-vault outbound domain allowlist |
| `EgressProxy` | Localhost HTTP proxy that enforces the allowlist |
| `DecomposeOrchestrator` | Split questions → sub-tasks → local LLM → aggregate |
| `Reducer` / `ReduceMode` | Deterministic result aggregation strategies |
| `WorkerRunner` | Compile and run ephemeral Rust or Python workers |
| `WorkerConfig` / `WorkerLanguage` | Worker configuration |
| `RenewalWatcher` | Automatic halt for unattended long-running tasks |
| `ClusterScheduler` | Route sub-tasks to peer machines |
| `WorkerRegistry` | Thread-safe registry of cluster peers |
| `ArtifactBuilder` | Generate self-contained HTML artifacts |
| `ArtifactServer` | Per-task localhost API server for artifact chat |
| `BundleSigner` / `BundleVerifier` | Bundle content manifest signing and verification |
| `BundleIndex` | First-party bundle index |
| `audit_bundle` | Supply-chain security scan of an MTDF bundle |
| `Sandbox` / `SandboxMode` | Kernel-level filesystem sandbox |
| `OperationMonitor` | Async operation tracking and cancellation |
| `AhmaMcpService` | Full MCP server service |
| `LlmClient` | OpenAI-compatible LLM client |

## Minimal example: vault + audit

```rust
use ahma_core::{TaskVault, AuditWriter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create a new vault
    let vault = TaskVault::create("my-analysis")?;
    println!("Vault: {}", vault.path().display());

    // Audit vault creation
    let audit = vault.audit_writer();
    audit.vault_created(
        &vault.path().display().to_string(),
        "my-analysis",
    ).await?;

    // Use workdir as the sandbox scope for any subprocess
    println!("Sandbox scope: {}", vault.sandbox_scope().display());

    Ok(())
}
```

## Example: local-LLM question decomposition

```rust
use ahma_core::{DecomposeOrchestrator, DecomposeConfig};
use ahma_mcp::config::LlmProviderConfig;

let cfg = DecomposeConfig {
    llm_provider: LlmProviderConfig {
        base_url: "http://localhost:11434/v1".into(),
        model: "gemma4".into(),
        api_key: None,
    },
    max_subtasks: Some(4),
    max_concurrent: Some(2),
    reduce_mode: None,
    answer_prompt: None,
    llm_timeout_seconds: Some(30),
};

let orchestrator = DecomposeOrchestrator::new(cfg);
let answer = orchestrator.run("What are the main business risks?").await?;
println!("{answer}");
```

## Example: egress-controlled subprocess

```rust
use ahma_core::{EgressAllowlist, EgressProxy, EgressProxyConfig};

let mut allowlist = EgressAllowlist::deny_all();
allowlist.add("api.example.com");

let proxy = EgressProxy::start(EgressProxyConfig { allowlist }).await?;

// Inject into subprocess environment
for (k, v) in proxy.env_vars() {
    std::env::set_var(k, v);
}
// Any HTTP call from this point goes through the proxy
```

## Dependency notes

`ahma_core` depends on `ahma_mcp` for its implementation. The workspace builds both crates together. `ahma_mcp` itself requires:

- Rust 1.95+
- Tokio async runtime
- Platform-specific sandbox prerequisites (see [docs/security-sandbox.md](security-sandbox.md))

## See also

- [docs/task-vault.md](task-vault.md)
- [docs/decompose.md](decompose.md)
- [docs/egress-sandbox.md](egress-sandbox.md)
- [docs/renewal-contract.md](renewal-contract.md)
