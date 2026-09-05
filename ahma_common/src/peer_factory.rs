//! Transport-agnostic MCP peer factory abstraction (P6).
//!
//! [`PeerFactory`](crate::peer_factory::PeerFactory) is the "port" for per-session MCP peer creation in the HTTP
//! bridge.  Moving this type from `ahma_http_bridge::peer` to `ahma_common`
//! severs the test-time back-edge that existed when `ahma_mcp::test_utils`
//! needed to import it from the bridge crate.
//!
//! ## Dependency graph (after P6)
//!
//! ```text
//! ahma_common    (defines PeerFactory, PeerStreams)
//!     ↑               ↑
//! ahma_http_bridge  ahma_mcp::test_utils
//!     ↑ (dev-dep)
//! ahma_mcp tests
//! ```
//!
//! The old cycle was:
//! - `ahma_mcp` → `ahma_http_bridge` (for `PeerFactory` in `test_utils`)
//! - `ahma_http_bridge` dev → `ahma_mcp` (for `NullPeerFactory`, `InProcessMcpPeerFactory`)
//!
//! With `PeerFactory` in `ahma_common`, `test_utils::bridge_peer` imports from
//! `ahma_common` and the test-time coupling to the bridge crate is removed.

use std::{future::Future, pin::Pin};

/// A boxed, `Send`-able, `'static` async future.
///
/// Used as the return type of [`PeerFactory::create`] to keep the trait
/// object-safe.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// An async, single-use cleanup function invoked when the session terminates.
///
/// For subprocess peers this kills the child process.
/// For in-process peers this is usually `None` — drop semantics handle cleanup.
pub type PeerShutdownFn =
    Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + 'static>;

/// I/O streams returned by [`PeerFactory::create`] for a new bridge session.
///
/// `stdin` / `stdout` form a framed JSON-RPC channel (newline-delimited).
/// `stderr` is optional debug output captured from subprocess peers.
pub struct PeerStreams {
    /// Write side: bridge → peer (newline-delimited JSON-RPC).
    pub stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin + 'static>,
    /// Read side: peer → bridge (newline-delimited JSON-RPC).
    pub stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>,
    /// Optional debug stderr (provided by subprocess peers when colored output
    /// is enabled; absent for in-process peers).
    pub stderr: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>>,
    /// Async cleanup hook invoked on explicit session termination.
    pub shutdown_fn: Option<PeerShutdownFn>,
    /// Receives the classified abnormal-exit description (R-SIGN.5) when the
    /// peer dies by signal (e.g. the macOS code-signing SIGKILL cause).
    /// `None` for peers without an exit monitor (in-process test peers).
    pub exit_cause: Option<tokio::sync::oneshot::Receiver<String>>,
}

/// Factory that produces [`PeerStreams`] for each new bridge session.
///
/// Implement this trait to swap the subprocess backend (production) for an
/// in-memory `AhmaMcpService` (testing) without changing any session logic.
///
/// # Object safety
///
/// The trait is object-safe — callers hold `Arc<dyn PeerFactory>`.
pub trait PeerFactory: Send + Sync + 'static {
    /// Create a new peer connection for a fresh session.
    ///
    /// `options` carries what this session asked for and nobody else did: its
    /// id, and the arguments derived from its own client's configuration
    /// (SPEC R-DAEMON.4). They used to be process-wide, so the first client to
    /// start the bridge configured every later one.
    ///
    /// Returns an error (as `anyhow::Error`) if the backend cannot be
    /// initialised.
    fn create(&self, options: PeerSpawnOptions) -> BoxFuture<anyhow::Result<PeerStreams>>;
}

/// What distinguishes one session's peer from another's.
#[derive(Debug, Clone, Default)]
pub struct PeerSpawnOptions {
    /// The session this peer serves. Handed to the worker so its hub
    /// registration is stable across re-registration.
    pub session_id: String,
    /// Extra worker arguments this client asked for.
    pub extra_args: Vec<String>,
}
