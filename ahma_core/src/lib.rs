//! # Ahma Core Library
//!
//! This crate exposes Ahma's permissive secure execution primitives as an
//! embeddable Rust library.  Use it to add kernel-enforced sandboxing and
//! async operation tracking to your own Rust application.
//!
//! ## License
//!
//! This crate is licensed under **MIT OR Apache-2.0**.
//!
//! ## Key re-exports
//!
//! | Type | From | Purpose |
//! |------|------|---------|
//! | [`Sandbox`] | `ahma_mcp::sandbox` | Kernel-level FS sandbox |
//! | [`SandboxMode`] | `ahma_mcp::sandbox` | Sandbox enforcement mode |
//! | [`OperationMonitor`] | `ahma_mcp::operation_monitor` | Async operation tracking |
//! | [`OperationStatus`] | `ahma_mcp::operation_monitor` | Operation status enum |
//! | [`MonitorConfig`] | `ahma_mcp::operation_monitor` | Monitor configuration |
//! | [`AhmaMcpService`] | `ahma_mcp` | Full MCP server service |
//! | [`LlmClient`] | `ahma_llm_monitor` | OpenAI-compatible LLM client |
//!
//! ## AGPL-licensed sibling crates
//!
//! The following primitives are available in separate AGPL-licensed crates.
//! Embedders who need them must add those crates directly to their
//! `Cargo.toml` and accept the applicable license terms for those crates:
//!
//! | Crate | License | Primitives |
//! |-------|---------|-----------|
//! | `ahma_vault` | AGPL-3.0-or-later | `TaskVault`, `AuditWriter`, `TrashManager` |
//! | `ahma_decompose` | AGPL-3.0-or-later | `DecomposeOrchestrator`, `ReduceMode`, `Reducer` |
//! | `ahma_task_tree` | AGPL-3.0-or-later | `TaskTreeOrchestrator`, `TaskTree`, `TaskNode` |
//! | `ahma_worker` | AGPL-3.0-or-later | `WorkerRunner`, `WorkerConfig`, `WorkerLanguage` |
//! | `ahma_renewal` | AGPL-3.0-or-later | `RenewalWatcher`, `RenewalConfig` |
//! | `ahma_tui` | AGPL-3.0-or-later | `TuiApp`, `TuiEvent`, `run_tui` |
//! | `ahma_cluster` | **AGPL-3.0-or-later** | `ClusterScheduler`, `WorkerRegistry`, `TaskManifest` |
//!
//! Linking any of these AGPL crates into a binary means any modified version
//! offered to remote users over a network must provide source access per
//! AGPL-3.0 §13.
//!
//! Each crate's `Cargo.toml` is the authoritative license declaration.
//!
//! ## Quickstart
//!
//! ```no_run
//! use ahma_core::{Sandbox, SandboxMode, OperationMonitor, MonitorConfig};
//! use std::time::Duration;
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let sandbox = Arc::new(Sandbox::new(vec![], SandboxMode::Strict, false, false, false)?);
//!     let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
//!         Duration::from_secs(300),
//!     )));
//!     println!("Sandbox and monitor ready.");
//!     Ok(())
//! }
//! ```

// Re-export core MCP types for embedders
pub use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor, OperationStatus};
pub use ahma_mcp::sandbox::{Sandbox, SandboxMode};
pub use ahma_mcp::{Adapter, AhmaMcpService};

// Re-export LLM client for direct use
pub use ahma_llm_monitor::LlmClient;

pub mod agent;
pub use agent::{
    AgentApprovalGate, AgentEvent, McpChatConfig, execute_agent_turn, spawn_agent_task,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reexported_monitor_config_is_constructible() {
        let config = MonitorConfig::with_timeout(Duration::from_secs(7));

        assert_eq!(config.default_timeout, Duration::from_secs(7));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(30));
    }

    #[test]
    fn reexported_sandbox_mode_is_available() {
        assert_eq!(format!("{:?}", SandboxMode::Strict), "Strict");
        assert_ne!(SandboxMode::Strict, SandboxMode::Test);
    }
}
