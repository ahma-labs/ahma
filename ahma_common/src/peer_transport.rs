//! Transport-agnostic peer dispatch abstraction (P6 / P1 foundation).
//!
//! [`PeerDispatch`] is the single "port" in the ports-and-adapters design for
//! cluster node-to-node communication.  Swapping implementations enables:
//!
//! - **Production**: an HTTP adapter (wraps `ClusterTransport` / reqwest)
//! - **Testing**: [`InMemoryPeerDispatch`] — in-process, no sockets required
//! - **Fault injection**: a decorator that injects latency, drops, or
//!   partitions on top of any other `PeerDispatch` impl
//!
//! The trait is object-safe: callers hold `Arc<dyn PeerDispatch>` and can
//! swap implementations without recompiling.

use anyhow::Result;
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

/// A boxed, `Send`-able, `'static` async future.
///
/// Used as the return type of object-safe trait methods that are async.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

// ─── PeerDispatch ─────────────────────────────────────────────────────────────

/// Dispatches a JSON payload to a remote (or in-process) MCP peer.
///
/// This is the core "port" for cluster transport.  Both the HTTP production
/// client and the in-memory test double implement this trait, so the scheduler
/// can be tested without real network sockets.
///
/// # Object safety
///
/// The trait is intentionally object-safe.  Do not add generic methods or
/// `Self`-typed parameters.
pub trait PeerDispatch: Send + Sync + 'static {
    /// POST `payload` to `{peer_addr}{path}` and return the JSON response body.
    ///
    /// `peer_addr` — base URL, e.g. `http://10.0.0.5:7000`.
    /// `path`      — request path, e.g. `/mcp` or `/tasks`.
    fn dispatch(
        &self,
        peer_addr: &str,
        path: &str,
        payload: Value,
    ) -> BoxFuture<Result<Value>>;
}

// ─── PeerHandler (test helper) ────────────────────────────────────────────────

/// Handler invoked by [`InMemoryPeerDispatch`] for a specific peer address.
///
/// Implement this in tests to simulate peer responses without any network I/O.
pub trait PeerHandler: Send + Sync + 'static {
    fn handle(&self, path: &str, payload: Value) -> BoxFuture<Result<Value>>;
}

// ─── InMemoryPeerDispatch ─────────────────────────────────────────────────────

/// In-memory [`PeerDispatch`] for unit/integration tests and grid simulation.
///
/// Register test responders with [`Self::register`], then inject the instance
/// into the scheduler or other component under test.
///
/// # Example
///
/// ```rust
/// use ahma_common::peer_transport::{
///     BoxFuture, InMemoryPeerDispatch, PeerDispatch, PeerHandler,
/// };
/// use anyhow::Result;
/// use serde_json::Value;
/// use std::sync::Arc;
///
/// struct EchoHandler;
/// impl PeerHandler for EchoHandler {
///     fn handle(&self, path: &str, payload: Value) -> BoxFuture<Result<Value>> {
///         let path = path.to_string();
///         Box::pin(async move { Ok(serde_json::json!({ "echo": path })) })
///     }
/// }
///
/// let dispatch = InMemoryPeerDispatch::new();
/// dispatch.register("http://peer1:7000", Arc::new(EchoHandler));
/// ```
#[derive(Clone, Default)]
pub struct InMemoryPeerDispatch {
    handlers: Arc<Mutex<HashMap<String, Arc<dyn PeerHandler>>>>,
}

impl InMemoryPeerDispatch {
    /// Create a new, empty in-memory dispatcher.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler that responds to all requests directed at `addr`.
    ///
    /// Subsequent calls with the same `addr` overwrite the previous handler.
    pub fn register(&self, addr: impl Into<String>, handler: Arc<dyn PeerHandler>) {
        self.handlers.lock().unwrap().insert(addr.into(), handler);
    }

    /// Return the number of registered peer addresses.
    pub fn peer_count(&self) -> usize {
        self.handlers.lock().unwrap().len()
    }
}

impl PeerDispatch for InMemoryPeerDispatch {
    fn dispatch(
        &self,
        peer_addr: &str,
        path: &str,
        payload: Value,
    ) -> BoxFuture<Result<Value>> {
        let handlers = self.handlers.clone();
        let peer_addr = peer_addr.to_string();
        let path = path.to_string();
        Box::pin(async move {
            let handler = {
                let guard = handlers.lock().unwrap();
                guard.get(&peer_addr).cloned()
            };
            let handler = handler.ok_or_else(|| {
                anyhow::anyhow!(
                    "InMemoryPeerDispatch: no handler registered for '{}'",
                    peer_addr
                )
            })?;
            handler.handle(&path, payload).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoHandler;
    impl PeerHandler for EchoHandler {
        fn handle(&self, path: &str, payload: Value) -> BoxFuture<Result<Value>> {
            let path = path.to_string();
            Box::pin(async move {
                Ok(serde_json::json!({ "echo_path": path, "echo_payload": payload }))
            })
        }
    }

    struct FailHandler;
    impl PeerHandler for FailHandler {
        fn handle(&self, _path: &str, _payload: Value) -> BoxFuture<Result<Value>> {
            Box::pin(async { Err(anyhow::anyhow!("simulated peer failure")) })
        }
    }

    #[tokio::test]
    async fn routes_to_registered_handler() {
        let dispatch = InMemoryPeerDispatch::new();
        dispatch.register("http://peer1:7000", Arc::new(EchoHandler));

        let result = dispatch
            .dispatch(
                "http://peer1:7000",
                "/mcp",
                serde_json::json!({"hello": "world"}),
            )
            .await
            .unwrap();

        assert_eq!(result["echo_path"], "/mcp");
        assert_eq!(result["echo_payload"]["hello"], "world");
    }

    #[tokio::test]
    async fn error_on_unregistered_peer() {
        let dispatch = InMemoryPeerDispatch::new();
        let err = dispatch
            .dispatch("http://unknown:9999", "/mcp", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no handler registered"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn handler_failure_propagates() {
        let dispatch = InMemoryPeerDispatch::new();
        dispatch.register("http://broken:7000", Arc::new(FailHandler));
        let err = dispatch
            .dispatch("http://broken:7000", "/mcp", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated peer failure"));
    }

    #[test]
    fn peer_count_tracks_registrations() {
        let dispatch = InMemoryPeerDispatch::new();
        assert_eq!(dispatch.peer_count(), 0);
        dispatch.register("http://a:1", Arc::new(EchoHandler));
        assert_eq!(dispatch.peer_count(), 1);
        dispatch.register("http://b:2", Arc::new(EchoHandler));
        assert_eq!(dispatch.peer_count(), 2);
        // Overwrite same key — count stays the same.
        dispatch.register("http://a:1", Arc::new(EchoHandler));
        assert_eq!(dispatch.peer_count(), 2);
    }
}
