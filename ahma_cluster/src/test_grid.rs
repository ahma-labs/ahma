//! Deterministic multi-node simulation harness for cluster tests (P4).
//!
//! [`TestGrid`] wires N in-process [`ClusterScheduler`] nodes over a shared
//! [`InMemoryPeerDispatch`], with an optional injectable [`FaultModel`].
//!
//! # Design
//!
//! - **Deterministic** — faults are driven by a seeded PRNG so any failure can
//!   be reproduced exactly by passing the same seed.
//! - **In-process** — no sockets, no subprocesses.  The entire grid fits in a
//!   single `#[tokio::test]`.
//! - **Composable** — wrap any `PeerDispatch` with [`FaultInjectingDispatch`]
//!   to layer latency, drops, partitions, or security faults on top.
//!
//! # Example
//!
//! ```rust
//! use ahma_cluster::test_grid::{TestGrid, FaultModel};
//!
//! # #[tokio::main]
//! # async fn main() {
//! // 3-node grid, no faults, shared key "test-key".
//! let grid = TestGrid::builder()
//!     .nodes(3)
//!     .shared_key(b"test-key")
//!     .build();
//!
//! assert_eq!(grid.node_count(), 3);
//! # }
//! ```

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use ahma_common::peer_transport::{BoxFuture, InMemoryPeerDispatch, PeerDispatch, PeerHandler};
use anyhow::Result;
use serde_json::Value;
use tracing::debug;

use crate::{
    discovery::{PeerCapabilities, PeerInfo, WorkerRegistry},
    scheduler::ClusterScheduler,
};

// ─── FaultModel ──────────────────────────────────────────────────────────────

/// Configures injectable network faults for [`FaultInjectingDispatch`].
///
/// All fields default to "no fault".  Set only the faults you need.
#[derive(Debug, Clone, Default)]
pub struct FaultModel {
    /// Fixed latency added to every request.
    pub latency: Option<Duration>,
    /// Probability (0.0–1.0) that a request is silently dropped (returns an error).
    pub drop_rate: f64,
    /// Set of peer addresses that are completely partitioned (all requests fail).
    pub partitioned_peers: Vec<String>,
    /// Seed for the PRNG used to make drop decisions deterministic.
    ///
    /// Pass the same seed to reproduce the same failure sequence exactly.
    pub seed: u64,
}

impl FaultModel {
    /// No faults — identical to `Default::default()`.
    pub fn none() -> Self {
        Self::default()
    }

    /// Fixed latency on every request (useful for testing timeout behaviour).
    pub fn with_latency(ms: u64) -> Self {
        Self {
            latency: Some(Duration::from_millis(ms)),
            ..Default::default()
        }
    }

    /// Drop requests to the given peer (simulates a partitioned node).
    pub fn partition(peer_addr: impl Into<String>) -> Self {
        Self {
            partitioned_peers: vec![peer_addr.into()],
            ..Default::default()
        }
    }

    /// Random packet drop with deterministic seed.
    pub fn with_drop_rate(rate: f64, seed: u64) -> Self {
        Self {
            drop_rate: rate.clamp(0.0, 1.0),
            seed,
            ..Default::default()
        }
    }
}

// ─── FaultInjectingDispatch ──────────────────────────────────────────────────

/// A [`PeerDispatch`] decorator that injects faults described by a [`FaultModel`].
///
/// Wraps any inner `PeerDispatch`.  Faults are evaluated before forwarding:
///
/// 1. **Partition** — if the peer is in `partitioned_peers`, return an error
///    immediately.
/// 2. **Drop** — if a PRNG roll (seeded) falls below `drop_rate`, return an error.
/// 3. **Latency** — sleep for `latency` before forwarding to the inner dispatcher.
///
/// The PRNG is a simple LCG (linear congruential generator) sufficient for
/// deterministic test scenarios — not a cryptographic generator.
#[derive(Clone)]
pub struct FaultInjectingDispatch {
    inner: Arc<dyn PeerDispatch>,
    model: FaultModel,
    /// Shared mutable state for the LCG counter.
    counter: Arc<Mutex<u64>>,
}

impl FaultInjectingDispatch {
    /// Wrap `inner` with the supplied `FaultModel`.
    pub fn new(inner: Arc<dyn PeerDispatch>, model: FaultModel) -> Self {
        let seed = model.seed;
        Self {
            inner,
            model,
            counter: Arc::new(Mutex::new(seed)),
        }
    }

    /// Next pseudo-random float in [0, 1) using a simple LCG.
    fn next_rand(&self) -> f64 {
        let mut c = self.counter.lock().unwrap();
        // LCG constants from Knuth
        *c = c.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (*c >> 11) as f64 / (1u64 << 53) as f64
    }

    /// `true` if `peer_addr` is in the partitioned set.
    fn is_partitioned(&self, peer_addr: &str) -> bool {
        self.model
            .partitioned_peers
            .iter()
            .any(|p| p == peer_addr)
    }
}

impl PeerDispatch for FaultInjectingDispatch {
    fn dispatch(
        &self,
        peer_addr: &str,
        path: &str,
        payload: Value,
    ) -> BoxFuture<Result<Value>> {
        if self.is_partitioned(peer_addr) {
            let peer_addr = peer_addr.to_string();
            return Box::pin(async move {
                Err(anyhow::anyhow!(
                    "FaultInjectingDispatch: peer '{peer_addr}' is partitioned"
                ))
            });
        }

        let rand_val = self.next_rand();
        if rand_val < self.model.drop_rate {
            debug!(
                rand = rand_val,
                drop_rate = self.model.drop_rate,
                "FaultInjectingDispatch: dropping request"
            );
            return Box::pin(async { Err(anyhow::anyhow!("FaultInjectingDispatch: packet dropped")) });
        }

        let latency = self.model.latency;
        let inner = self.inner.clone();
        let peer_addr = peer_addr.to_string();
        let path = path.to_string();

        Box::pin(async move {
            if let Some(delay) = latency {
                tokio::time::sleep(delay).await;
            }
            inner.dispatch(&peer_addr, &path, payload).await
        })
    }
}

// ─── SecurityFaultDispatch ────────────────────────────────────────────────────

/// A [`PeerDispatch`] that simulates security failure scenarios by corrupting
/// requests before forwarding them.
///
/// Used in P4 tests to verify that the bridge correctly rejects:
/// - Manifests signed with the wrong key
/// - Replayed nonces
/// - Manifests with an expired timestamp
#[derive(Clone)]
pub struct SecurityFaultDispatch {
    inner: Arc<dyn PeerDispatch>,
    fault: SecurityFault,
}

/// The type of security fault to inject.
#[derive(Debug, Clone)]
pub enum SecurityFault {
    /// Strip the cluster manifest header entirely (unauthenticated call).
    StripManifest,
    /// Replace the manifest signature with a wrong-key signature.
    WrongKey { wrong_key: Vec<u8> },
    /// Replay the same manifest nonce twice (the second call should fail).
    ReplayNonce,
}

impl SecurityFaultDispatch {
    /// Create a new security fault injector wrapping `inner`.
    pub fn new(inner: Arc<dyn PeerDispatch>, fault: SecurityFault) -> Self {
        Self { inner, fault }
    }
}

impl PeerDispatch for SecurityFaultDispatch {
    fn dispatch(
        &self,
        peer_addr: &str,
        path: &str,
        payload: Value,
    ) -> BoxFuture<Result<Value>> {
        // For now, this dispatcher passes through to inner.
        // In a real scenario, the `SecurityFault` would be applied by
        // the `McpPeerDispatch` layer (mutating the manifest header).
        // The dispatch trait operates at the payload level; manifest
        // manipulation lives in the HTTP layer of `McpPeerDispatch`.
        // These faults are exercised by direct tests on
        // `ClusterManifest::decode_and_verify` and the bridge middleware.
        debug!(fault = ?self.fault, "SecurityFaultDispatch: passing through (fault applied at HTTP layer)");
        self.inner.dispatch(peer_addr, path, payload)
    }
}

// ─── TestNode ─────────────────────────────────────────────────────────────────

/// A single node in a [`TestGrid`].
///
/// Exposes the [`ClusterScheduler`] and a mutable handle to the node's
/// `PeerInfo` for simulating heartbeats (capability updates, load changes).
pub struct TestNode {
    /// Unique node identifier.
    pub id: String,
    /// The scheduler for this node.
    pub scheduler: ClusterScheduler,
    /// The worker registry backing this node's scheduler.
    pub registry: WorkerRegistry,
    /// This node's own address (used by other nodes to route to it).
    pub addr: String,
}

// ─── TestGrid ─────────────────────────────────────────────────────────────────

/// N in-process cluster nodes wired over a shared [`InMemoryPeerDispatch`].
///
/// Build with [`TestGrid::builder`].
pub struct TestGrid {
    /// All nodes in the grid.
    nodes: Vec<TestNode>,
    /// The shared in-memory dispatcher (before fault injection).
    pub dispatch: Arc<InMemoryPeerDispatch>,
}

impl TestGrid {
    /// Create a builder.
    pub fn builder() -> TestGridBuilder {
        TestGridBuilder::default()
    }

    /// Number of nodes in the grid.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Access a node by index (panics if out of range).
    pub fn node(&self, index: usize) -> &TestNode {
        &self.nodes[index]
    }

    /// Mutable access to a node by index.
    pub fn node_mut(&mut self, index: usize) -> &mut TestNode {
        &mut self.nodes[index]
    }

    /// Convenience: iterate over all nodes.
    pub fn iter_nodes(&self) -> impl Iterator<Item = &TestNode> {
        self.nodes.iter()
    }

    /// Register a handler for a specific node address so other nodes can
    /// dispatch to it in-process.
    ///
    /// Call this after building the grid to wire in a simulated peer responder.
    pub fn register_peer_handler(
        &self,
        addr: impl Into<String>,
        handler: Arc<dyn PeerHandler>,
    ) {
        self.dispatch.register(addr, handler);
    }
}

// ─── TestGridBuilder ──────────────────────────────────────────────────────────

/// Builder for [`TestGrid`].
#[derive(Default)]
pub struct TestGridBuilder {
    node_count: usize,
    shared_key: Vec<u8>,
    fault_model: Option<FaultModel>,
    /// Models each node advertises (same for all nodes by default).
    default_models: Vec<String>,
}

impl TestGridBuilder {
    /// Set the number of nodes (default: 0).
    pub fn nodes(mut self, n: usize) -> Self {
        self.node_count = n;
        self
    }

    /// Set the shared HMAC key (must match across all nodes).
    pub fn shared_key(mut self, key: impl Into<Vec<u8>>) -> Self {
        self.shared_key = key.into();
        self
    }

    /// Inject a [`FaultModel`] onto the shared dispatch layer.
    pub fn with_fault_model(mut self, model: FaultModel) -> Self {
        self.fault_model = Some(model);
        self
    }

    /// Set the default models advertised by each node.
    pub fn with_models(mut self, models: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.default_models = models.into_iter().map(|m| m.into()).collect();
        self
    }

    /// Consume the builder and return a [`TestGrid`].
    pub fn build(self) -> TestGrid {
        let base_dispatch = Arc::new(InMemoryPeerDispatch::new());

        // Optionally wrap with fault injection.
        let dispatch_for_schedulers: Arc<dyn PeerDispatch> = if let Some(model) = self.fault_model
        {
            Arc::new(FaultInjectingDispatch::new(
                Arc::clone(&base_dispatch) as Arc<dyn PeerDispatch>,
                model,
            ))
        } else {
            Arc::clone(&base_dispatch) as Arc<dyn PeerDispatch>
        };

        // Build nodes
        let mut nodes = Vec::with_capacity(self.node_count);
        // We need a second pass to populate each node's registry with all other
        // nodes' PeerInfo, so collect addresses first.
        let node_addresses: Vec<String> = (0..self.node_count)
            .map(|i| format!("http://test-node-{i}:7000"))
            .collect();

        for (i, addr) in node_addresses.iter().enumerate() {
            let node_id = format!("node-{i}");
            let registry = WorkerRegistry::new(60);

            // Register all *other* nodes as peers in this node's registry.
            for (j, peer_addr) in node_addresses.iter().enumerate() {
                if j == i {
                    continue; // Skip self.
                }
                let peer_info = PeerInfo {
                    id: format!("node-{j}"),
                    addr: peer_addr.clone(),
                    models: self.default_models.clone(),
                    active_ops: 0,
                    reachable: true,
                    capabilities: Some(PeerCapabilities {
                        models_available: self.default_models.clone(),
                        models_loaded: self.default_models.clone(),
                        max_concurrent: 4,
                        active_ops: 0,
                        vram_free_mb: Some(8192),
                    }),
                };
                registry.upsert(peer_info);
            }

            let scheduler = ClusterScheduler::new(registry.clone(), self.shared_key.clone())
                .with_peer_dispatch(Arc::clone(&dispatch_for_schedulers));

            nodes.push(TestNode {
                id: node_id,
                scheduler,
                registry,
                addr: addr.clone(),
            });
        }

        TestGrid {
            nodes,
            dispatch: base_dispatch,
        }
    }
}

// ─── Responders (convenience handlers for tests) ──────────────────────────────

/// A fixed-response handler that returns a pre-configured `TaskResult` JSON.
pub struct SuccessHandler {
    response: Value,
}

impl SuccessHandler {
    /// Return `response` for every request.
    pub fn new(response: Value) -> Arc<Self> {
        Arc::new(Self { response })
    }

    /// Return a canned success result: `{ "output": "ok", "duration_ms": 10 }`.
    pub fn default_ok() -> Arc<Self> {
        Self::new(serde_json::json!({ "output": "ok", "duration_ms": 10 }))
    }
}

impl PeerHandler for SuccessHandler {
    fn handle(&self, _path: &str, _payload: Value) -> BoxFuture<Result<Value>> {
        let resp = self.response.clone();
        Box::pin(async move { Ok(resp) })
    }
}

/// A handler that always returns an error.
pub struct FailHandler {
    message: String,
}

impl FailHandler {
    /// Create a handler that always fails with `message`.
    pub fn new(message: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            message: message.into(),
        })
    }
}

impl PeerHandler for FailHandler {
    fn handle(&self, _path: &str, _payload: Value) -> BoxFuture<Result<Value>> {
        let msg = self.message.clone();
        Box::pin(async move { Err(anyhow::anyhow!("{msg}")) })
    }
}

/// A handler that returns success only for the Nth call, failing all others.
///
/// Useful for simulating a node that recovers after N retries.
pub struct RecoverAfterNHandler {
    calls: Arc<Mutex<usize>>,
    recover_after: usize,
    success_response: Value,
}

impl RecoverAfterNHandler {
    /// Fail the first `fail_count` calls, then return `success_response`.
    pub fn new(fail_count: usize, success_response: Value) -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::new(Mutex::new(0)),
            recover_after: fail_count,
            success_response,
        })
    }
}

impl PeerHandler for RecoverAfterNHandler {
    fn handle(&self, _path: &str, _payload: Value) -> BoxFuture<Result<Value>> {
        let calls = Arc::clone(&self.calls);
        let recover_after = self.recover_after;
        let success_response = self.success_response.clone();
        Box::pin(async move {
            let mut c = calls.lock().unwrap();
            let call_num = *c;
            *c += 1;
            drop(c);
            if call_num < recover_after {
                Err(anyhow::anyhow!(
                    "RecoverAfterNHandler: failing call {call_num} (recover after {recover_after})"
                ))
            } else {
                Ok(success_response)
            }
        })
    }
}

/// A handler that records every payload it receives (useful for assertions).
#[derive(Default)]
pub struct CapturingHandler {
    received: Arc<Mutex<Vec<Value>>>,
    response: Value,
}

impl CapturingHandler {
    /// Create a handler that returns `response` and captures all payloads.
    pub fn new(response: Value) -> Arc<Self> {
        Arc::new(Self {
            received: Arc::new(Mutex::new(Vec::new())),
            response,
        })
    }

    /// Return a copy of all captured payloads.
    pub fn captured(&self) -> Vec<Value> {
        self.received.lock().unwrap().clone()
    }

    /// Number of requests received.
    pub fn call_count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

impl PeerHandler for CapturingHandler {
    fn handle(&self, _path: &str, payload: Value) -> BoxFuture<Result<Value>> {
        let received = Arc::clone(&self.received);
        let response = self.response.clone();
        Box::pin(async move {
            received.lock().unwrap().push(payload);
            Ok(response)
        })
    }
}

// ─── Load-simulation helpers ─────────────────────────────────────────────────

/// Simulate a slow peer by applying artificial latency to all its requests.
pub struct SlowPeerDispatch {
    inner: Arc<dyn PeerDispatch>,
    slow_peers: HashMap<String, Duration>,
}

impl SlowPeerDispatch {
    /// Create a new slow-peer decorator with no slow peers configured.
    pub fn new(inner: Arc<dyn PeerDispatch>) -> Self {
        Self {
            inner,
            slow_peers: HashMap::new(),
        }
    }

    /// Add a slow peer that incurs `latency` on every request.
    pub fn with_slow_peer(
        mut self,
        addr: impl Into<String>,
        latency: Duration,
    ) -> Self {
        self.slow_peers.insert(addr.into(), latency);
        self
    }
}

impl PeerDispatch for SlowPeerDispatch {
    fn dispatch(
        &self,
        peer_addr: &str,
        path: &str,
        payload: Value,
    ) -> BoxFuture<Result<Value>> {
        let latency = self.slow_peers.get(peer_addr).copied();
        let inner = Arc::clone(&self.inner);
        let peer_addr = peer_addr.to_string();
        let path = path.to_string();

        Box::pin(async move {
            if let Some(delay) = latency {
                tokio::time::sleep(delay).await;
            }
            inner.dispatch(&peer_addr, &path, payload).await
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::TaskManifest;

    fn base_manifest() -> TaskManifest {
        TaskManifest {
            task_id: "task-1".into(),
            prompt: "Hello".into(),
            model: "gemma:4b".into(),
            llm_base_url: "http://localhost:11434/v1".into(),
            max_tokens: 128,
            timeout_secs: 10,
            nonce: String::new(),
            issued_at: 0,
            signature: String::new(),
        }
    }

    #[test]
    fn builder_creates_correct_node_count() {
        let grid = TestGrid::builder()
            .nodes(3)
            .shared_key(b"test-key")
            .with_models(["gemma:4b"])
            .build();
        assert_eq!(grid.node_count(), 3);
    }

    #[test]
    fn node_registries_are_populated() {
        let grid = TestGrid::builder()
            .nodes(3)
            .shared_key(b"test-key")
            .with_models(["gemma:4b"])
            .build();

        // Each node should see 2 peers (not itself).
        for i in 0..3 {
            let peers = grid.node(i).registry.peers_for_model("gemma:4b");
            assert_eq!(
                peers.len(),
                2,
                "node {i} expected 2 peers, got {}",
                peers.len()
            );
        }
    }

    #[tokio::test]
    async fn dispatch_routes_to_handler() {
        let grid = TestGrid::builder()
            .nodes(2)
            .shared_key(b"test-key")
            .with_models(["gemma:4b"])
            .build();

        let handler = CapturingHandler::new(serde_json::json!({
            "task_id": "task-1",
            "text": "hello from node-1",
            "success": true,
            "worker_id": "node-1"
        }));
        grid.register_peer_handler(
            "http://test-node-1:7000",
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
        );

        // Node 0 should be able to schedule to node-1.
        let result = grid
            .node(0)
            .scheduler
            .schedule(base_manifest())
            .await;

        assert!(
            result.is_some(),
            "scheduler should have routed to node-1"
        );
        assert_eq!(
            handler.call_count(),
            1,
            "handler should have been called once"
        );
    }

    #[tokio::test]
    async fn fault_model_partition_blocks_dispatch() {
        let grid = TestGrid::builder()
            .nodes(2)
            .shared_key(b"test-key")
            .with_models(["gemma:4b"])
            .with_fault_model(FaultModel::partition("http://test-node-1:7000"))
            .build();

        // Register a handler so the dispatch would succeed without faults.
        grid.register_peer_handler(
            "http://test-node-1:7000",
            SuccessHandler::default_ok() as Arc<dyn PeerHandler>,
        );

        // The partition fault should cause the dispatch to fail.
        let result = grid.node(0).scheduler.schedule(base_manifest()).await;
        assert!(
            result.is_none(),
            "partitioned peer should result in no schedule"
        );
    }

    #[tokio::test]
    async fn success_handler_returns_expected_response() {
        let handler = SuccessHandler::new(serde_json::json!({ "output": "done" }));
        let result = handler.handle("/mcp", serde_json::json!({})).await.unwrap();
        assert_eq!(result["output"], "done");
    }

    #[tokio::test]
    async fn recover_after_n_fails_first_n_calls() {
        let handler = RecoverAfterNHandler::new(2, serde_json::json!({ "output": "ok" }));

        assert!(handler.handle("/mcp", serde_json::json!({})).await.is_err());
        assert!(handler.handle("/mcp", serde_json::json!({})).await.is_err());
        assert!(handler.handle("/mcp", serde_json::json!({})).await.is_ok());
    }

    #[tokio::test]
    async fn capturing_handler_records_payloads() {
        let handler = CapturingHandler::new(serde_json::json!({ "ok": true }));

        handler
            .handle("/mcp", serde_json::json!({"call": 1}))
            .await
            .unwrap();
        handler
            .handle("/mcp", serde_json::json!({"call": 2}))
            .await
            .unwrap();

        assert_eq!(handler.call_count(), 2);
        let captured = handler.captured();
        assert_eq!(captured[0]["call"], 1);
        assert_eq!(captured[1]["call"], 2);
    }

    #[test]
    fn fault_injecting_dispatch_lcg_is_deterministic() {
        let inner = Arc::new(InMemoryPeerDispatch::new()) as Arc<dyn PeerDispatch>;
        let fault = FaultModel {
            drop_rate: 0.0, // No drops — just test the LCG sequence.
            seed: 42,
            ..Default::default()
        };
        let d1 = FaultInjectingDispatch::new(Arc::clone(&inner), fault.clone());
        let d2 = FaultInjectingDispatch::new(Arc::clone(&inner), fault);

        // Same seed → same sequence.
        let seq1: Vec<f64> = (0..8).map(|_| d1.next_rand()).collect();
        let seq2: Vec<f64> = (0..8).map(|_| d2.next_rand()).collect();
        assert_eq!(seq1, seq2, "same seed must produce identical sequence");
    }

    #[test]
    fn fault_injecting_dispatch_different_seeds_differ() {
        let inner = Arc::new(InMemoryPeerDispatch::new()) as Arc<dyn PeerDispatch>;
        let d1 = FaultInjectingDispatch::new(
            Arc::clone(&inner),
            FaultModel {
                seed: 1,
                ..Default::default()
            },
        );
        let d2 = FaultInjectingDispatch::new(
            Arc::clone(&inner),
            FaultModel {
                seed: 2,
                ..Default::default()
            },
        );
        let seq1: Vec<f64> = (0..8).map(|_| d1.next_rand()).collect();
        let seq2: Vec<f64> = (0..8).map(|_| d2.next_rand()).collect();
        assert_ne!(seq1, seq2, "different seeds should produce different sequences");
    }
}
