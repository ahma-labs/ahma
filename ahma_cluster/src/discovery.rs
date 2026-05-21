//! Worker peer discovery for the local cluster.
//!
//! Peers advertise themselves via:
//! 1. **Heartbeat** (`POST /cluster/heartbeat`): peers post signed [`PeerCapabilities`]
//!    to the coordinator every `heartbeat_interval_seconds` (see `~/.ahma/config.toml`).
//!    This is the primary mechanism for dynamic capability advertisement.
//! 2. **Static bootstrap** (`~/.ahma/cluster/peers.json`): initial URL list for Tailscale
//!    or pre-configured machines.  Updated at runtime by heartbeats.
//! 3. **mDNS** (`_ahma-worker._tcp.local`): zero-config LAN discovery.
//!    **Not yet implemented** — planned for v0.8.  See `TODO(v0.8)` in `SPEC.md`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Capabilities advertised by a worker peer via [`WorkerRegistry::receive_heartbeat`].
///
/// A peer POSTs this (along with a signature) to the coordinator's
/// `POST /cluster/heartbeat` endpoint every `heartbeat_interval_seconds`.
/// The coordinator uses it to route tasks to the best-available worker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerCapabilities {
    /// All models available (downloaded but not necessarily loaded).
    pub models_available: Vec<String>,
    /// Models currently resident in GPU/CPU RAM — loading is instant.
    pub models_loaded: Vec<String>,
    /// Maximum concurrent inference operations this peer supports.
    pub max_concurrent: usize,
    /// How many inference operations are currently running.
    pub active_ops: usize,
    /// Free VRAM in MiB, if the peer can report it.
    pub vram_free_mb: Option<u64>,
}

/// Information about an `ahma worker` peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Unique peer ID (hostname or UUID).
    pub id: String,
    /// HTTP address of the peer's ahma HTTP bridge.
    pub addr: String,
    /// Models available on this peer.  Updated by heartbeats; falls back to
    /// the static `peers.json` list when no heartbeat has been received yet.
    pub models: Vec<String>,
    /// Number of currently active operations.
    #[serde(default)]
    pub active_ops: usize,
    /// Whether this peer is reachable.
    #[serde(default = "default_reachable")]
    pub reachable: bool,
    /// Rich capabilities last reported by this peer's heartbeat.
    /// `None` for static-only peers that have never sent a heartbeat.
    #[serde(default)]
    pub capabilities: Option<PeerCapabilities>,
}

fn default_reachable() -> bool {
    true
}

impl PeerInfo {
    /// Return `true` if this peer has `model` available (not necessarily loaded).
    pub fn has_model(&self, model: &str) -> bool {
        self.models
            .iter()
            .any(|m| m == model || m.starts_with(&format!("{model}:")))
    }

    /// Return `true` if `model` is currently loaded in memory on this peer.
    pub fn has_model_loaded(&self, model: &str) -> bool {
        self.capabilities.as_ref().is_some_and(|c| {
            c.models_loaded
                .iter()
                .any(|m| m == model || m.starts_with(&format!("{model}:")))
        })
    }

    /// Relative load score for scheduling (lower is better).
    ///
    /// Scoring formula:
    /// - `+0` if the model is already loaded in RAM; `+100` if it must be loaded
    /// - `+active_ops * 10` (each in-flight op costs 10 points)
    /// - `+50` if VRAM is critically low (< 1 GiB free)
    ///
    /// Peers with the same score are ordered by insertion time (stable sort).
    pub fn load_score_for(&self, model: &str) -> u64 {
        let load_penalty: u64 = if self.has_model_loaded(model) { 0 } else { 100 };
        let active_penalty: u64 = self.active_ops as u64 * 10;
        let vram_penalty: u64 = match self.capabilities.as_ref().and_then(|c| c.vram_free_mb) {
            Some(free_mb) if free_mb < 1024 => 50,
            _ => 0,
        };
        load_penalty + active_penalty + vram_penalty
    }

    /// Generic load score (no model context). Used as a tiebreaker.
    pub fn load_score(&self) -> u64 {
        let active_penalty: u64 = self.active_ops as u64 * 10;
        let vram_penalty: u64 = match self.capabilities.as_ref().and_then(|c| c.vram_free_mb) {
            Some(free_mb) if free_mb < 1024 => 50,
            _ => 0,
        };
        active_penalty + vram_penalty
    }

    /// Apply a [`PeerCapabilities`] heartbeat to update this peer's state.
    pub fn apply_capabilities(&mut self, caps: PeerCapabilities) {
        self.active_ops = caps.active_ops;
        // Merge: heartbeat available models win over static list.
        if !caps.models_available.is_empty() {
            self.models = caps.models_available.clone();
        }
        self.capabilities = Some(caps);
        self.reachable = true;
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

    /// Accept a heartbeat from peer `id` with new capabilities.
    ///
    /// If the peer is not yet in the registry, it must have been pre-populated via
    /// [`load_static_peers`].  This method updates an existing entry; it does not
    /// create new peers from heartbeats alone (prevents unauthed peers from joining).
    pub fn receive_heartbeat(&self, id: &str, caps: PeerCapabilities) -> bool {
        let mut inner = self.inner.write().unwrap();
        if let Some((peer, ts)) = inner.peers.get_mut(id) {
            peer.apply_capabilities(caps);
            *ts = Instant::now();
            debug!(
                "Heartbeat received from peer {id}: {} active ops",
                peer.active_ops
            );
            true
        } else {
            warn!("Heartbeat from unknown peer {id} — add it to peers.json to authorize");
            false
        }
    }

    /// Return all live, reachable peers that have `model` available.
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
            capabilities: None,
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

    #[test]
    fn load_score_for_loaded_model_is_lower() {
        let mut peer = make_peer("x", "gemma:4b");
        let score_unloaded = peer.load_score_for("gemma");
        // Apply heartbeat with model loaded.
        let caps = PeerCapabilities {
            models_available: vec!["gemma:4b".into()],
            models_loaded: vec!["gemma:4b".into()],
            active_ops: 0,
            max_concurrent: 4,
            vram_free_mb: Some(8192),
        };
        peer.apply_capabilities(caps);
        let score_loaded = peer.load_score_for("gemma");
        assert!(
            score_loaded < score_unloaded,
            "loaded model should score lower: loaded={score_loaded} unloaded={score_unloaded}"
        );
    }

    #[test]
    fn heartbeat_updates_peer_active_ops() {
        let reg = WorkerRegistry::new(60);
        reg.upsert(make_peer("worker-a", "gemma:4b"));

        let caps = PeerCapabilities {
            models_available: vec!["gemma:4b".into()],
            models_loaded: vec!["gemma:4b".into()],
            active_ops: 3,
            max_concurrent: 4,
            vram_free_mb: Some(4096),
        };
        assert!(reg.receive_heartbeat("worker-a", caps));

        let peers = reg.peers_for_model("gemma");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].active_ops, 3);
    }

    #[test]
    fn heartbeat_from_unknown_peer_is_rejected() {
        let reg = WorkerRegistry::new(60);
        let caps = PeerCapabilities::default();
        assert!(!reg.receive_heartbeat("unknown-peer", caps));
    }

    #[test]
    fn vram_penalty_applied_when_low() {
        let mut peer = make_peer("x", "model");
        let caps = PeerCapabilities {
            models_available: vec!["model".into()],
            models_loaded: vec!["model".into()],
            active_ops: 0,
            max_concurrent: 4,
            vram_free_mb: Some(512), // < 1024 MiB threshold
        };
        peer.apply_capabilities(caps);
        // Score should include the 50-point VRAM penalty even with model loaded.
        assert_eq!(peer.load_score_for("model"), 50);
    }
}
