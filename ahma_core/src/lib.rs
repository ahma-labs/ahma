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
//! ## Contents
//!
//! | Item | Purpose |
//! |------|---------|
//! | [`Sandbox`], [`SandboxMode`] | Kernel-level filesystem sandbox (from `ahma_mcp::sandbox`) |
//! | [`OperationMonitor`], [`OperationStatus`], [`MonitorConfig`] | Operation tracking (from `ahma_mcp::operation_monitor`) |
//! | [`Adapter`], [`AhmaMcpService`] | Tool execution and the full MCP service (from `ahma_mcp`) |
//! | [`LlmClient`] | OpenAI- and Anthropic-compatible LLM client (from `ahma_llm_monitor`) |
//! | [`agent`] | The chat agent loop that drives ahma's tools ([`execute_agent_turn`]) |
//! | [`approvals`] | Persistent per-workspace "always allow" tool grants |
//! | [`tool_menu`] | Which tools a model is offered, and how a small model asks for more |
//!
//! Requirements: `ahma_core/SPEC.md`.
//!
//! ## AGPL-licensed sibling crates
//!
//! The following primitives are available in separate AGPL-licensed crates.
//! Embedders who need them must add those crates directly to their
//! `Cargo.toml` and accept the applicable license terms for those crates:
//!
//! | Crate | License | Primitives |
//! |-------|---------|-----------|
//! | `ahma_tui` | AGPL-3.0 | `run_tui` and the terminal UI |
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
pub mod approvals;
pub mod tool_menu;
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
