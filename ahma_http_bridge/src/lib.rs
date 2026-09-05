//! # Ahma HTTP Bridge: The Session Multiplexer
//!
//! The Ahma HTTP Bridge is a specialized proxy that enables standard, stdio-based
//! MCP servers to support modern web-based workflows with **Session Isolation**.
//!
//! ## Concept: Stdio-to-HTTP Translation
//!
//! Most MCP servers are designed to communicate over standard input/output within
//! a single process. The bridge provides a high-performance HTTP/SSE interface
//! around these servers, allowing multiple clients (IDEs, web agents) to share
//! a single server while maintaining completely separate security contexts.
//!
//! ## Session Isolation & The Handshake Anchor
//!
//! The bridge's most critical feature is its ability to multiplex sessions:
//!
//! 1. **Isolated Subprocesses**: For every new client (identified by a unique
//!    `Mcp-Session-Id`), the bridge spawns a dedicated instance of the target
//!    MCP server.
//! 2. **Handshake Security Anchor**: The security sandbox for each subprocess
//!    is not fixed at startup. Instead, it is "anchored" during the initial MCP
//!    handshake. The bridge waits for the client to provide its workspace roots
//!    (`roots/list`), which are then used to lock the sandbox for that session.
//! 3. **Protocol Fidelity**: The bridge implements the complete MCP HTTP transport
//!    specification, including Server-Sent Events (SSE) for real-time notifications
//!    and reconnection resilience.
//!
//! ## Security
//!
//! The bridge has two transport-layer security features that operate independently:
//!
//! - **Bearer authentication** — set [`BridgeConfig::require_token`] or
//!   `--bearer-token` / `AHMA_BEARER_TOKEN` to require `Authorization: Bearer
//!   <token>` on all requests.  The `/health` endpoint is always exempt so
//!   load-balancers can probe without a credential.  The comparison is
//!   constant-time (timing-safe).  The token can be hot-reloaded at runtime via
//!   SIGHUP without restarting the bridge.
//!
//! - **Per-IP rate limiting** — pass `--rate-limit-per-minute` to cap requests
//!   per client IP using a token-bucket governor.  This protects against both
//!   accidental and malicious request floods.
//!
//! ## Practical Use
//!
//! This crate is ideal for deploying Ahma in multi-user environments (like a
//! central agent server) or when integrating with web-based AI tools that cannot
//! interact with local stdio processes directly.
//!
//! ```rust,no_run
//! use ahma_http_bridge::{BridgeConfig, start_bridge};
//! use std::path::PathBuf;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Configure the bridge
//!     let config = BridgeConfig {
//!         bind_addr: "127.0.0.1:3000".parse().unwrap(),
//!         server_command: "ahma_mcp".to_string(), // Path to your MCP server binary
//!         // Optional fallback for clients that do not support roots/list
//!         default_sandbox_scope: Some(PathBuf::from("/path/to/project")),
//!         ..BridgeConfig::default()
//!     };
//!     
//!     // Start the bridge server
//!     start_bridge(config).await?;
//!     Ok(())
//! }
//! ```

/// HTTP bridge server implementation.
pub mod bridge;
/// Error types for bridge operations.
pub mod error;
/// Peer connection abstraction (transport port for bridge sessions).
pub mod peer;
/// QUIC / HTTP/3 server support.
pub mod quic;
/// Session lifecycle management for HTTP clients.
pub mod session;

pub use bridge::{BridgeConfig, DaemonExit, ListenerKind, start_bridge};
pub use error::{BridgeError, Result};
pub use peer::{PeerFactory, PeerShutdownFn, PeerStreams, SubprocessPeerFactory};
pub use session::{
    DEFAULT_HANDSHAKE_TIMEOUT_SECS, DEFAULT_MAX_SESSIONS, DEFAULT_REQUEST_TIMEOUT_SECS,
    DEFAULT_TOOL_CALL_TIMEOUT_SECS, McpRoot, Session, SessionManager, SessionManagerConfig,
    SessionTerminationReason,
};

/// Request handler for HTTP bridge.
pub mod request_handler;
