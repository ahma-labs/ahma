//! # Ahma Core Library
//!
//! This crate exposes Ahma's secure execution primitives as an embeddable Rust
//! library.  Use it to add kernel-enforced sandboxing, per-task vaults, and
//! local-LLM orchestration to your own Rust application.
//!
//! ## Key re-exports
//!
//! | Type | From | Purpose |
//! |------|------|---------|
//! | [`TaskVault`] | `ahma_mcp::vault` | Per-question isolated working directory |
//! | [`AuditWriter`] | `ahma_mcp::vault::audit` | Append-only task audit log |
//! | [`TrashManager`] | `ahma_mcp::vault::trash` | Two-phase delete |
//! | [`EgressAllowlist`] | `ahma_mcp::egress` | Per-task network allowlist |
//! | [`DecomposeOrchestrator`] | `ahma_mcp::decompose` | Local-LLM task decomposer |
//! | [`Reducer`] / [`ReduceMode`] | `ahma_mcp::decompose` | Aggregation strategies |
//! | [`WorkerRunner`] | `ahma_mcp::worker` | Ephemeral code synthesis & execution |
//! | [`RenewalWatcher`] | `ahma_mcp::renewal` | Long-task renewal contract |
//! | [`ClusterScheduler`] | `ahma_mcp::cluster` | Local peer scheduler |
//! | [`Sandbox`] | `ahma_mcp::sandbox` | Kernel-level FS sandbox |
//! | [`OperationMonitor`] | `ahma_mcp::operation_monitor` | Async operation tracking |
//! | [`AhmaMcpService`] | `ahma_mcp` | Full MCP server service |
//!
//! ## Quickstart
//!
//! ```no_run
//! use ahma_core::{TaskVault, AuditWriter, DecomposeOrchestrator};
//! use ahma_core::decompose_config;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // 1. Create an isolated task vault for this question.
//!     let vault = TaskVault::create("summarise-invoice")?;
//!
//!     // 2. Record vault creation in the audit log.
//!     let audit = vault.audit_writer();
//!     audit.vault_created(&vault.path().display().to_string(), "summarise-invoice").await?;
//!
//!     // 3. (Optionally) run a decompose task against a local LLM.
//!     // let orch = DecomposeOrchestrator::new(decompose_config!("http://localhost:11434/v1", "gemma:4b"));
//!     // let answer = orch.run("Summarise the key line items in this invoice.").await?;
//!
//!     Ok(())
//! }
//! ```

// Re-export vault primitives
pub use ahma_mcp::vault::{
    TaskVault,
    audit::{AuditEvent, AuditEventKind, AuditWriter},
    trash::{StagedEntry, TrashManager},
};

// Re-export egress sandbox
pub use ahma_mcp::egress::{EgressAllowlist, EgressProxy, EgressProxyConfig};

// Re-export decompose orchestration
pub use ahma_mcp::decompose::reducer::{ReduceMode, Reducer};
pub use ahma_mcp::decompose::{DecomposeOrchestrator, SubTaskResult};

// Re-export worker synthesis
pub use ahma_mcp::config::{WorkerConfig, WorkerLanguage};
pub use ahma_mcp::worker::WorkerRunner;

// Re-export renewal contract
pub use ahma_mcp::renewal::{RenewalConfig, RenewalHaltEvent, RenewalWatcher};

// Re-export cluster scheduling
pub use ahma_mcp::cluster::scheduler::TaskManifest;
pub use ahma_mcp::cluster::{ClusterScheduler, PeerInfo, WorkerRegistry};

// Re-export bundle management
pub use ahma_mcp::bundle::signing::{BundleSigner, BundleVerifier, audit_bundle};
pub use ahma_mcp::bundle::{BundleAuditResult, BundleAuditSeverity, BundleIndex};

// Re-export artifact generation
pub use ahma_mcp::artifact::{ArtifactBuilder, ArtifactHtml, ArtifactServer};

// Re-export TUI
pub use ahma_mcp::tui::{TuiApp, TuiEvent};

// Re-export core MCP types for embedders
pub use ahma_mcp::config::DecomposeConfig;
pub use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor, OperationStatus};
pub use ahma_mcp::sandbox::{Sandbox, SandboxMode};
pub use ahma_mcp::{Adapter, AhmaMcpService};

// Re-export LLM client for direct use
pub use ahma_llm_monitor::LlmClient;
