//! Worker peer discovery for the local cluster.
//!
//! Peers advertise themselves via:
//! 1. **mDNS** (LAN): `_ahma-worker._tcp.local` service records (stub — planned).
//! 2. **Static config** (`~/.ahma/cluster/peers.json`): for Tailscale or pre-configured machines.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Information about an `ahma worker` peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Unique peer ID (hostname or UUID).
    pub id: String,
    /// HTTP address of the peer's ahma HTTP bridge.
    pub addr: String,
    /// Models available on this peer (`ollama list` output).
    pub models: Vec<String>,
    /// Number of currently active operations (free RAM proxy).
    pub active_ops: usize,
    /// Whether this peer is reachable.
    #[serde(default = "default_reachable")]
    pub reachable: bool,
}

fn default_reachable() -> bool {
    true
}

impl PeerInfo {
    /// Return `true` if this peer has the given model loaded.
    pub fn has_model(&self, model: &str) -> bool {
        self.models
            .iter()
            .any(|m| m == model || m.starts_with(&format!("{model}:")))
    }

    /// Relative load score (lower is better).
    pub fn load_score(&self) -> usize {
        self.active_ops
    }
}

/// Thread-safe registry of known `ahma worker` peers.
#[derive(Clone)]
pub struct WorkerRegistry {
    inner: Arc<RwLock<RegistryInner>>,
}

struct RegistryInner {
    peers: HashMap<String, (PeerInfo, Instant)>,
    ttl: Duration,
}

impl WorkerRegistry {
    /// Create a registry with a peer TTL of `ttl_secs` seconds.
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                peers: HashMap::new(),
                ttl: Duration::from_secs(ttl_secs),
            })),
        }
    }

    /// Insert or update a peer.
    pub fn upsert(&self, peer: PeerInfo) {
        let mut inner = self.inner.write().unwrap();
        inner.peers.insert(peer.id.clone(), (peer, Instant::now()));
    }

    /// Remove a peer by ID.
    pub fn remove(&self, id: &str) {
        let mut inner = self.inner.write().unwrap();
        inner.peers.remove(id);
    }

    /// Return all live, reachable peers that have the given model.
    pub fn peers_for_model(&self, model: &str) -> Vec<PeerInfo> {
        let inner = self.inner.read().unwrap();
        let now = Instant::now();
        inner
            .peers
            .values()
            .filter(|(peer, ts)| peer.reachable && now.duration_since(*ts) < inner.ttl)
            .filter(|(peer, _)| peer.has_model(model))
            .map(|(peer, _)| peer.clone())
            .collect()
    }

    /// Return all live peers.
    pub fn all_live(&self) -> Vec<PeerInfo> {
        let inner = self.inner.read().unwrap();
        let now = Instant::now();
        inner
            .peers
            .values()
            .filter(|(_, ts)| now.duration_since(*ts) < inner.ttl)
            .map(|(peer, _)| peer.clone())
            .collect()
    }

    /// Load static peers from `~/.ahma/cluster/peers.json` if it exists.
    pub fn load_static_peers(&self) -> Result<usize> {
        let Some(home) = dirs::home_dir() else {
            return Ok(0);
        };
        let path = home.join(".ahma").join("cluster").join("peers.json");
        if !path.exists() {
            return Ok(0);
        }
        let contents = std::fs::read_to_string(&path)?;
        let peers: Vec<PeerInfo> = serde_json::from_str(&contents)?;
        let count = peers.len();
        for peer in peers {
            info!("Loaded static peer: {} at {}", peer.id, peer.addr);
            self.upsert(peer);
        }
        Ok(count)
    }

    /// Start mDNS discovery (stub — logs warning until mdns-sd is added).
    pub async fn start_mdns_discovery(&self) {
        warn!(
            "mDNS peer discovery is not yet implemented (requires mdns-sd crate). \
             Static peers in ~/.ahma/cluster/peers.json are still active."
        );
        debug!(
            "Cluster registry initialized with {} static peers",
            self.all_live().len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_peer(id: &str, model: &str) -> PeerInfo {
        PeerInfo {
            id: id.to_string(),
            addr: "http://127.0.0.1:3000".to_string(),
            models: vec![model.to_string()],
            active_ops: 0,
            reachable: true,
        }
    }

    #[test]
    fn registry_finds_peer_by_model() {
        let reg = WorkerRegistry::new(60);
        reg.upsert(make_peer("worker-a", "gemma:4b"));
        reg.upsert(make_peer("worker-b", "llama3.2"));

        let gemma_peers = reg.peers_for_model("gemma");
        assert_eq!(gemma_peers.len(), 1);
        assert_eq!(gemma_peers[0].id, "worker-a");
    }

    #[test]
    fn remove_peer_removes_from_registry() {
        let reg = WorkerRegistry::new(60);
        reg.upsert(make_peer("worker-a", "gemma:4b"));
        reg.remove("worker-a");
        assert!(reg.peers_for_model("gemma").is_empty());
    }

    #[test]
    fn peer_has_model_prefix_match() {
        let peer = make_peer("x", "llama3.2:3b");
        assert!(peer.has_model("llama3.2"));
        assert!(!peer.has_model("gemma"));
    }
}
