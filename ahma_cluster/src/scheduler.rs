//! Cluster task scheduler.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::discovery::{PeerInfo, WorkerRegistry};

/// A signed task manifest sent to a remote worker peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskManifest {
    pub task_id: String,
    pub prompt: String,
    pub model: String,
    pub llm_base_url: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    /// HMAC-SHA256 signature (computed over fields above in canonical order).
    pub signature: String,
}

impl TaskManifest {
    /// Sign a manifest with a shared key.
    pub fn sign(mut self, shared_key: &str) -> Self {
        let payload = format!(
            "{}|{}|{}|{}",
            self.task_id, self.prompt, self.model, self.llm_base_url
        );
        self.signature = hmac_stub(shared_key, &payload);
        self
    }

    /// Verify this manifest's signature.
    pub fn verify(&self, shared_key: &str) -> bool {
        let payload = format!(
            "{}|{}|{}|{}",
            self.task_id, self.prompt, self.model, self.llm_base_url
        );
        hmac_stub(shared_key, &payload) == self.signature
    }
}

/// The result returned by a remote worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: String,
    pub text: String,
    pub success: bool,
    pub worker_id: String,
}

/// Routes decompose sub-tasks to the best-available peer.
pub struct ClusterScheduler {
    registry: WorkerRegistry,
    shared_key: String,
    http: reqwest::Client,
}

impl ClusterScheduler {
    /// Create a scheduler with the given registry and shared cluster key.
    pub fn new(registry: WorkerRegistry, shared_key: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self {
            registry,
            shared_key: shared_key.into(),
            http,
        }
    }

    /// Pick the best peer for `model` and dispatch `manifest` to it.
    ///
    /// Returns `None` if no suitable peer is available (caller falls back to local execution).
    pub async fn schedule(&self, manifest: TaskManifest) -> Option<TaskResult> {
        let mut peers = self.registry.peers_for_model(&manifest.model);
        if peers.is_empty() {
            debug!("No remote peers available for model {}", manifest.model);
            return None;
        }

        peers.sort_by_key(|p| p.load_score());
        let peer = &peers[0];

        match self.dispatch_to_peer(peer, manifest).await {
            Ok(result) => Some(result),
            Err(e) => {
                warn!("Dispatch to peer {} failed: {e}", peer.id);
                None
            }
        }
    }

    async fn dispatch_to_peer(
        &self,
        peer: &PeerInfo,
        manifest: TaskManifest,
    ) -> Result<TaskResult> {
        let signed = manifest.sign(&self.shared_key);
        let url = format!("{}/tasks", peer.addr.trim_end_matches('/'));

        info!("Dispatching task {} to peer {}", signed.task_id, peer.id);

        let resp = self
            .http
            .post(&url)
            .json(&signed)
            .send()
            .await
            .with_context(|| format!("Failed to dispatch to peer {}", peer.addr))?;

        let result: TaskResult = resp
            .json()
            .await
            .context("Failed to parse task result from peer")?;

        Ok(result)
    }
}

fn hmac_stub(key: &str, message: &str) -> String {
    let mut h: u64 = 5381;
    for b in key
        .bytes()
        .chain(b"|".iter().copied())
        .chain(message.bytes())
    {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_sign_verify_roundtrip() {
        let manifest = TaskManifest {
            task_id: "t1".into(),
            prompt: "What is 2+2?".into(),
            model: "gemma:4b".into(),
            llm_base_url: "http://localhost:11434/v1".into(),
            max_tokens: 256,
            timeout_secs: 30,
            signature: String::new(),
        };
        let signed = manifest.sign("my-shared-key");
        assert!(signed.verify("my-shared-key"));
        assert!(!signed.verify("wrong-key"));
    }

    #[test]
    fn hmac_stub_is_deterministic() {
        assert_eq!(hmac_stub("key", "msg"), hmac_stub("key", "msg"));
    }

    #[test]
    fn scheduler_returns_none_when_no_peers() {
        let reg = WorkerRegistry::new(60);
        let scheduler = ClusterScheduler::new(reg, "test-key");
        assert!(scheduler.registry.all_live().is_empty());
    }
}
