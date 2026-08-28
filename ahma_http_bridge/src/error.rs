//! Error types for the HTTP bridge

use thiserror::Error;

/// Errors that can occur during bridge operation
#[derive(Error, Debug)]
pub enum BridgeError {
    /// Underlying IO failure
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization failure
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Failure managing the MCP server subprocess
    #[error("Server process error: {0}")]
    ServerProcess(String),

    /// The concurrent-session cap was hit and no stale session could be
    /// evicted. Matched structurally (not by message text) to return HTTP 429
    /// instead of 500. The Display text keeps the historical `ServerProcess`
    /// prefix so the user-facing message is unchanged.
    #[error("Server process error: Session limit exceeded (max: {max})")]
    SessionLimitExceeded {
        /// The configured maximum number of concurrent sessions.
        max: usize,
    },

    /// Operator configuration the bridge cannot start with.
    ///
    /// Distinct from the runtime variants above because the remedy is different:
    /// nothing is wrong with the machine or the peer, a flag value is wrong, and
    /// the message must name it. Previously such a case (an unusable
    /// `--rate-limit-rps` / `--rate-limit-burst` pair) was an `.expect()` that
    /// panicked with a message naming neither value.
    #[error("Invalid configuration: {0}")]
    Config(String),

    /// Protocol or communication failure with subprocess
    #[error("Communication error: {0}")]
    Communication(String),

    /// A single request exceeded its wait window while the subprocess is still
    /// alive and the operation is still running. This is *recoverable* — it must
    /// NOT be surfaced as a transport-fatal error, or the whole proxy session
    /// would be torn down (see `forward_request` / `proxy_client`).
    #[error("Request timed out")]
    Timeout,

    /// HTTP server binding or runtime error
    #[error("HTTP server error: {0}")]
    HttpServer(String),
}

/// Convenience result type for bridge operations.
pub type Result<T> = std::result::Result<T, BridgeError>;
