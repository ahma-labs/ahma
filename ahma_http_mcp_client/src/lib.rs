//! # Ahma HTTP MCP Client
//!
//! Outbound MCP over HTTP for every in-workspace consumer. Requirements:
//! `ahma_http_mcp_client/SPEC.md`.
//!
//! - [`streamable`]: the one implementation of the Streamable HTTP handshake
//!   (initialize → SSE → initialized → `roots/list` → `tools/call`) used for the
//!   ahma bridge and daemon.
//! - [`client::HttpMcpTransport`]: an `rmcp` `Transport` for external MCP servers,
//!   with optional OAuth 2.0 + PKCE. The OAuth endpoints are currently Atlassian's.
//!   Tokens persist in `~/.ahma/mcp_http_token.json`.
//! - `unix_client` (Unix only): the same transport over a Unix domain socket.
//!
//! ## Usage
//!
//! The main entry point is [`client::HttpMcpTransport`]. You construct it with the target URL
//! and optional OAuth2 credentials.
//!
//! ```no_run
//! use ahma_http_mcp_client::client::HttpMcpTransport;
//! use url::Url;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let url = Url::parse("https://api.atlassian.com/mcp")?;
//! let client = HttpMcpTransport::new(
//!     url,
//!     Some("client_id".to_string()),
//!     Some("client_secret".to_string())
//! )?;
//!
//! // Ensure we have a valid token before making requests
//! client.ensure_authenticated().await?;
//! # Ok(())
//! # }
//! ```

/// HTTP transport implementation for MCP clients.
pub mod client;
/// Error types for HTTP MCP client operations.
pub mod error;
/// `oauth2` HTTP adapter over the workspace `reqwest` client.
pub mod oauth_http;
/// Shared MCP Streamable-HTTP client (handshake, SSE, roots, 409 gate).
pub mod streamable;
/// Unix domain socket MCP transport (Unix only).
#[cfg(unix)]
pub mod unix_client;
