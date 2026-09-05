//! In-memory [`PeerFactory`] adapters for testing the HTTP bridge without
//! spawning real subprocesses.
//!
//! # Why this module exists
//!
//! The HTTP bridge (`ahma_http_bridge`) manages per-session peer connections via
//! the [`PeerFactory`] port defined in `ahma_http_bridge::peer`.  In production
//! this port is implemented by [`SubprocessPeerFactory`] which forks an
//! `ahma_mcp` process for every session.  In tests, forking is expensive and
//! flaky on constrained CI runners.
//!
//! This module provides two test doubles:
//!
//! | Type | Use-case |
//! |------|----------|
//! | [`NullPeerFactory`](crate::test_utils::bridge_peer::NullPeerFactory) | Lifecycle / state-machine tests. The peer side is a silent sink; the bridge I/O loop sees EOF and stops quickly. Equivalent to using `echo` as the subprocess. |
//! | [`InProcessMcpPeerFactory`](crate::test_utils::bridge_peer::InProcessMcpPeerFactory) | Full protocol tests. The peer side is a real [`AhmaMcpService`](crate::mcp_service::AhmaMcpService) wired through a `tokio::io::duplex` channel. |
//!
//! Both types are `Send + Sync + 'static` and can be stored as
//! `Arc<dyn PeerFactory>` inside [`SessionManagerConfig::peer_factory`].
//!
//! # Example — lifecycle test with NullPeerFactory
//!
//! ```rust,no_run
//! use ahma_http_bridge::session::{SessionManager, SessionManagerConfig};
//! use ahma_mcp::test_utils::bridge_peer::NullPeerFactory;
//! use std::sync::Arc;
//!
//! # #[tokio::main]
//! # async fn main() {
//! let config = SessionManagerConfig {
//!     handshake_timeout_secs: 5,
//!     max_sessions: 10,
//!     peer_factory: Some(Arc::new(NullPeerFactory)),
//!     ..Default::default()
//! };
//! let manager = SessionManager::new(config);
//! let id = manager.create_session().await.unwrap();
//! println!("created session {id}");
//! # }
//! ```
//!
//! [`PeerFactory`]: ahma_http_bridge::peer::PeerFactory
//! [`SubprocessPeerFactory`]: ahma_http_bridge::peer::SubprocessPeerFactory
//! [`AhmaMcpService`]: crate::mcp_service::AhmaMcpService
//! [`SessionManagerConfig::peer_factory`]: ahma_http_bridge::session::SessionManagerConfig::peer_factory

// Import PeerFactory types from ahma_common (P6 — back-edge elimination).
// Previously imported from ahma_http_bridge::peer, which created a test-time
// back-edge.  ahma_common has no dependency on ahma_http_bridge, so this
// severs the cycle cleanly.
use ahma_common::peer_factory::{BoxFuture, PeerFactory, PeerSpawnOptions, PeerStreams};
use std::sync::Arc;

// ─── NullPeerFactory ─────────────────────────────────────────────────────────

/// A [`PeerFactory`] that returns empty, EOF-signalling streams.
///
/// The bridge I/O loop receives EOF immediately from the peer "stdout", causing
/// it to shut down the session cleanly.  This is sufficient for tests that only
/// exercise session lifecycle, handshake state transitions, or sandbox locking —
/// without requiring actual MCP message exchanges.
///
/// Equivalent to using `echo` (or `true`) as the subprocess command, but without
/// spawning any process.
#[derive(Debug, Clone, Default)]
pub struct NullPeerFactory;

impl PeerFactory for NullPeerFactory {
    fn create(&self, _options: PeerSpawnOptions) -> BoxFuture<anyhow::Result<PeerStreams>> {
        Box::pin(async move {
            // A duplex pair — when the peer_end is dropped immediately the
            // bridge end sees EOF on reads.
            let (bridge_end, _peer_end) = tokio::io::duplex(256);
            // _peer_end is dropped here → bridge_read returns EOF instantly.
            let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
            Ok(PeerStreams {
                stdin: Box::new(bridge_write),
                stdout: Box::new(bridge_read),
                stderr: None,
                shutdown_fn: None,
                exit_cause: None,
            })
        })
    }
}

// ─── InProcessMcpPeerFactory ─────────────────────────────────────────────────

/// A [`PeerFactory`] that wires a real [`AhmaMcpService`] to the bridge via an
/// in-memory `tokio::io::duplex` channel.
///
/// Full MCP protocol messages (initialize, tools/list, tools/call, roots/list,
/// etc.) flow through the in-memory channel, making bridge integration tests
/// deterministic and fast without any network I/O.
///
/// # Construction
///
/// Use [`InProcessMcpPeerFactory::builder`] to configure the factory:
///
/// ```rust,no_run
/// use std::path::PathBuf;
/// use ahma_mcp::test_utils::bridge_peer::InProcessMcpPeerFactory;
///
/// # #[tokio::main]
/// # async fn main() {
/// let factory = InProcessMcpPeerFactory::builder()
///     .tools_dir(PathBuf::from("/tmp/test_tools"))
///     .build()
///     .await
///     .expect("failed to build InProcessMcpPeerFactory");
/// # }
/// ```
///
/// [`AhmaMcpService`]: crate::mcp_service::AhmaMcpService
pub struct InProcessMcpPeerFactory {
    /// Shared tool configuration loaded once; cloned per session.
    configs: Arc<std::collections::HashMap<String, crate::config::ToolConfig>>,
    /// Sandbox scope for the in-process MCP service.
    scopes: Vec<std::path::PathBuf>,
}

impl InProcessMcpPeerFactory {
    /// Start building an [`InProcessMcpPeerFactory`].
    pub fn builder() -> InProcessMcpPeerFactoryBuilder {
        InProcessMcpPeerFactoryBuilder::default()
    }
}

impl std::fmt::Debug for InProcessMcpPeerFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessMcpPeerFactory")
            .field("num_tools", &self.configs.len())
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl PeerFactory for InProcessMcpPeerFactory {
    fn create(&self, _options: PeerSpawnOptions) -> BoxFuture<anyhow::Result<PeerStreams>> {
        use crate::adapter::Adapter;
        use crate::mcp_service::{AhmaMcpService, GuidanceConfig};
        use crate::operation_monitor::{MonitorConfig, OperationMonitor};
        use crate::sandbox::{Sandbox, SandboxMode};
        use crate::shell_pool::{ShellPoolConfig, ShellPoolManager};
        use anyhow::Context as _;
        use rmcp::{ServiceExt, transport::async_rw::AsyncRwTransport};

        let configs = Arc::clone(&self.configs);
        let scopes = self.scopes.clone();

        Box::pin(async move {
            // Detect nested sandbox environment (Cursor, Docker, etc.)
            let mode = if super::client::is_nested_sandbox_environment() {
                SandboxMode::Test
            } else {
                SandboxMode::Strict
            };

            // Build the sandbox — use current dir as fallback scope.
            let sandbox_scopes = if scopes.is_empty() {
                vec![std::env::current_dir().context("Cannot determine cwd")?]
            } else {
                scopes
            };

            let sandbox = Sandbox::new(sandbox_scopes, mode, false, false, false)
                .context("Sandbox init failed")?;

            let monitor_config = MonitorConfig::with_timeout(std::time::Duration::from_secs(300));
            let operation_monitor = Arc::new(OperationMonitor::new(monitor_config));
            let shell_pool = Arc::new(ShellPoolManager::new(ShellPoolConfig::default()));
            let adapter = Arc::new(
                Adapter::new(
                    Arc::clone(&operation_monitor),
                    shell_pool,
                    Arc::new(sandbox),
                )
                .context("Adapter construction failed")?,
            );

            let service = AhmaMcpService::new(
                adapter,
                operation_monitor,
                configs,
                Arc::new(None::<GuidanceConfig>),
                false, // force_synchronous
                true,  // defer_sandbox — the bridge sends roots later
            )
            .await
            .context("AhmaMcpService init failed")?;

            // Wire the bridge side and the service side through a duplex channel.
            // bridge_end  ←→  peer_end
            //   bridge reads peer responses from bridge_end
            //   bridge writes requests to bridge_end
            let (bridge_end, peer_end) = tokio::io::duplex(65536);
            let (bridge_read, bridge_write) = tokio::io::split(bridge_end);
            let (peer_read, peer_write) = tokio::io::split(peer_end);

            // Start the MCP service loop in the background.  The service task
            // keeps running until the bridge closes the write half (bridge_write
            // is dropped on session termination) or the session is terminated.
            let server_transport = AsyncRwTransport::new_server(peer_read, peer_write);
            tokio::spawn(async move {
                let _ = service.serve(server_transport).await;
            });

            Ok(PeerStreams {
                stdin: Box::new(bridge_write),
                stdout: Box::new(bridge_read),
                stderr: None,
                shutdown_fn: None,
                exit_cause: None,
            })
        })
    }
}

// ─── Builder ─────────────────────────────────────────────────────────────────

/// Builder for [`InProcessMcpPeerFactory`].
#[derive(Default)]
pub struct InProcessMcpPeerFactoryBuilder {
    tools_dir: Option<std::path::PathBuf>,
    configs: Option<std::collections::HashMap<String, crate::config::ToolConfig>>,
    scopes: Vec<std::path::PathBuf>,
}

impl InProcessMcpPeerFactoryBuilder {
    /// Load tools from `dir` at build time.
    pub fn tools_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.tools_dir = Some(dir);
        self
    }

    /// Use pre-built tool configs instead of loading from disk.
    pub fn with_configs(
        mut self,
        configs: std::collections::HashMap<String, crate::config::ToolConfig>,
    ) -> Self {
        self.configs = Some(configs);
        self
    }

    /// Restrict the in-process sandbox to these scopes.
    pub fn scopes(mut self, scopes: Vec<std::path::PathBuf>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Build the factory, loading tool configurations if needed.
    pub async fn build(self) -> anyhow::Result<InProcessMcpPeerFactory> {
        let configs = if let Some(c) = self.configs {
            c
        } else if let Some(ref dir) = self.tools_dir {
            use crate::config::load_tool_configs;
            use crate::shell::cli::AppConfig;
            load_tool_configs(&AppConfig::default(), Some(dir))
                .await
                .unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };

        Ok(InProcessMcpPeerFactory {
            configs: Arc::new(configs),
            scopes: self.scopes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_factory_is_debug() {
        let f = NullPeerFactory;
        assert!(format!("{f:?}").contains("NullPeerFactory"));
    }

    #[test]
    fn null_factory_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NullPeerFactory>();
    }

    #[tokio::test]
    async fn null_factory_creates_valid_peer_streams() {
        let factory = NullPeerFactory;
        let streams = factory
            .create(PeerSpawnOptions::default())
            .await
            .expect("create should succeed");
        // stdin and stdout are valid (non-null) boxed trait objects
        let _ = streams.stdin;
        let _ = streams.stdout;
        assert!(streams.stderr.is_none());
        assert!(streams.shutdown_fn.is_none());
    }

    #[test]
    fn in_process_factory_builder_default_is_empty() {
        let builder = InProcessMcpPeerFactory::builder();
        assert!(builder.tools_dir.is_none());
        assert!(builder.configs.is_none());
        assert!(builder.scopes.is_empty());
    }
}
