//! Worker peer discovery for the local cluster.
//!
//! Peers advertise themselves via:
//! 1. **Heartbeat** (`POST /cluster/heartbeat`): peers post signed [`PeerCapabilities`]
//!    to the coordinator every `heartbeat_interval_seconds` (see `~/.ahma/config.toml`).
//!    This is the primary mechanism for dynamic capability advertisement.
//! 2. **Static bootstrap** (`~/.ahma/cluster/peers.json`): initial URL list for Tailscale
//!    or pre-configured machines.  Updated at runtime by heartbeats.
//! 3. **mDNS** (`_ahma-worker._tcp.local`): zero-config LAN discovery.
//!    Peers browse for `_ahma-worker._tcp.local.` and upsert resolved entries into the
//!    registry; `announce_self` registers the local process so remote peers find it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use tracing::{debug, info, warn};

/// mDNS service type used by ahma worker peers.
const MDNS_SERVICE_TYPE: &str = "_ahma-worker._tcp.local.";

/// Capabilities advertised by a worker peer via [`WorkerRegistry::receive_heartbeat`].
///
/// A peer POSTs this (along with a signature) to the coordinator's
/// `POST /cluster/heartbeat` endpoint every `heartbeat_interval_seconds`.
/// The coordinator uses it to route tasks to the best-available worker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerCapabilities {
    /// All models available (downloaded but not necessarily loaded).
    pub model_available: Vec<String>,
    /// Models currently resident in GPU/CPU RAM — loading is instant.
    pub model_loaded: Vec<String>,
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
            c.model_loaded
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
        if !caps.model_available.is_empty() {
            self.models = caps.model_available.clone();
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

    /// Accept a **signed** heartbeat from peer `id`.
    ///
    /// The caller must supply the HMAC-SHA256 hex signature computed over the
    /// canonical JSON serialisation of `caps` using the cluster shared key.
    /// Rejects the heartbeat if:
    /// - the signature doesn't match (wrong key or tampered payload), or
    /// - the peer ID is not pre-registered in the registry.
    ///
    /// Returns `Ok(true)` when the peer was updated, `Ok(false)` when the peer
    /// is unknown (caller may log/drop accordingly), or `Err` on bad signature.
    pub fn receive_signed_heartbeat(
        &self,
        id: &str,
        caps: PeerCapabilities,
        signature: &str,
        shared_key: &[u8],
    ) -> Result<bool> {
        let payload = serde_json::to_string(&caps).expect("PeerCapabilities serialises infallibly");
        let expected = crate::scheduler::hmac_sha256_hex(shared_key, &payload);
        let sig_match: bool = expected.as_bytes().ct_eq(signature.as_bytes()).into();
        if !sig_match {
            bail!(
                "Heartbeat from peer {id}: HMAC-SHA256 signature mismatch — \
                 check that both sides use the same cluster key"
            );
        }
        Ok(self.receive_heartbeat(id, caps))
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
        self.load_static_peers_from(&home)
    }

    /// Load static peers from a custom base directory.
    pub fn load_static_peers_from(&self, home: &std::path::Path) -> Result<usize> {
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

    /// Start mDNS discovery: browse for `_ahma-worker._tcp.local.` peers and
    /// upsert them into the registry as they appear or disappear.
    ///
    /// This method spawns a background Tokio task and returns immediately.
    /// Resolved peers are registered with an HTTP address derived from the
    /// mDNS TXT record (`id`, `models` comma-separated) or defaults.
    pub async fn start_mdns_discovery(&self) {
        let registry = self.clone();
        tokio::spawn(async move {
            if let Err(e) = run_mdns_browse(registry).await {
                warn!("mDNS discovery stopped with error: {e}");
            }
        });
        info!(
            "mDNS peer discovery started (browsing {})",
            MDNS_SERVICE_TYPE
        );
        debug!(
            "Cluster registry initialized with {} static peers",
            self.all_live().len()
        );
    }

    /// Announce this process as an `ahma worker` peer via mDNS.
    ///
    /// Other peers on the LAN will discover this node via `start_mdns_discovery`.
    ///
    /// # Parameters
    /// * `peer_id` — unique name for this instance (e.g. `hostname-uuid`)
    /// * `port`    — HTTP port the local ahma bridge is listening on
    /// * `models`  — models available on this node (comma-separated in TXT record)
    pub fn announce_self(&self, peer_id: &str, port: u16, models: &[String]) -> Result<()> {
        let daemon =
            ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mDNS daemon start failed: {e}"))?;

        let hostname = hostname_string();
        let mut properties = HashMap::new();
        properties.insert("id".to_owned(), peer_id.to_owned());
        properties.insert("models".to_owned(), models.join(","));

        let svc = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            peer_id,
            &hostname,
            (),
            port,
            Some(properties),
        )
        .map_err(|e| anyhow::anyhow!("Failed to create mDNS ServiceInfo: {e}"))?;

        daemon
            .register(svc)
            .map_err(|e| anyhow::anyhow!("mDNS register failed: {e}"))?;

        info!(
            "Announced self as mDNS peer '{}' on port {} with models: [{}]",
            peer_id,
            port,
            models.join(", ")
        );
        // SAFETY: The ServiceDaemon MUST outlive this function — it keeps
        // broadcasting our mDNS record until the process exits.  We
        // intentionally leak it here; the OS reclaims the memory at exit.
        // Callers must not call `announce_self` more than once per process.
        std::mem::forget(daemon);
        Ok(())
    }
}

/// Browse for `_ahma-worker._tcp.local.` via mDNS and upsert/remove peers in `registry`.
async fn run_mdns_browse(registry: WorkerRegistry) -> anyhow::Result<()> {
    let daemon =
        ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mDNS daemon start failed: {e}"))?;
    let browse = daemon
        .browse(MDNS_SERVICE_TYPE)
        .map_err(|e| anyhow::anyhow!("mDNS browse failed: {e}"))?;

    loop {
        let event = browse
            .recv_async()
            .await
            .map_err(|e| anyhow::anyhow!("mDNS channel closed: {e}"))?;

        match event {
            ServiceEvent::ServiceResolved(info) => {
                if let Some(peer) = mdns_info_to_peer(&info) {
                    info!("mDNS: discovered peer '{}' at {}", peer.id, peer.addr);
                    registry.upsert(peer);
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                // The instance name is the first label of the fullname.
                let id = fullname.split('.').next().unwrap_or(&fullname).to_owned();
                debug!("mDNS: peer '{}' departed", id);
                registry.remove(&id);
            }
            ServiceEvent::SearchStarted(ty) => {
                debug!("mDNS: browse search started for {ty}");
            }
            ServiceEvent::SearchStopped(ty) => {
                debug!("mDNS: browse search stopped for {ty}");
            }
            _ => {}
        }
    }
}

/// Convert an mDNS `ServiceInfo` into a `PeerInfo`, returning `None` if the
/// service has no usable address.
fn mdns_info_to_peer(info: &ServiceInfo) -> Option<PeerInfo> {
    // Prefer an IPv4 address; fall back to the first address or the hostname.
    let addr_str = info
        .get_addresses()
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| info.get_addresses().iter().next())
        .map(|a| a.to_string())
        .unwrap_or_else(|| info.get_hostname().trim_end_matches('.').to_owned());

    let port = info.get_port();
    let http_addr = format!("http://{addr_str}:{port}");

    // Peer ID from TXT `id` property; fall back to the instance name.
    let instance_name = info
        .get_fullname()
        .split('.')
        .next()
        .unwrap_or("")
        .to_owned();
    let id = info
        .get_property_val_str("id")
        .unwrap_or(&instance_name)
        .to_owned();

    if id.is_empty() {
        return None;
    }

    // Models from TXT `models` property (comma-separated).
    let models: Vec<String> = info
        .get_property_val_str("models")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    Some(PeerInfo {
        id,
        addr: http_addr,
        models,
        active_ops: 0,
        reachable: true,
        capabilities: None,
    })
}

/// Return a suitable mDNS hostname for this machine (e.g. `mymac.local.`).
///
/// Reads `HOSTNAME` (Unix) or `COMPUTERNAME` (Windows) environment variables;
/// falls back to `"ahma-worker"` if neither is set.
fn hostname_string() -> String {
    let base = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "ahma-worker".to_owned());
    if base.ends_with('.') {
        base
    } else if base.contains('.') {
        format!("{base}.")
    } else {
        format!("{base}.local.")
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
            model_available: vec!["gemma:4b".into()],
            model_loaded: vec!["gemma:4b".into()],
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
            model_available: vec!["gemma:4b".into()],
            model_loaded: vec!["gemma:4b".into()],
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
            model_available: vec!["model".into()],
            model_loaded: vec!["model".into()],
            active_ops: 0,
            max_concurrent: 4,
            vram_free_mb: Some(512), // < 1024 MiB threshold
        };
        peer.apply_capabilities(caps);
        // Score should include the 50-point VRAM penalty even with model loaded.
        assert_eq!(peer.load_score_for("model"), 50);
    }

    // ── Signed heartbeat tests ────────────────────────────────────────────────

    fn sign_caps(caps: &PeerCapabilities, key: &[u8]) -> String {
        let payload = serde_json::to_string(caps).unwrap();
        crate::scheduler::hmac_sha256_hex(key, &payload)
    }

    #[test]
    fn signed_heartbeat_accepted_with_correct_key() {
        let key = b"cluster-shared-key";
        let reg = WorkerRegistry::new(60);
        reg.upsert(make_peer("node-a", "gemma:4b"));

        let caps = PeerCapabilities {
            model_available: vec!["gemma:4b".into()],
            model_loaded: vec![],
            active_ops: 1,
            max_concurrent: 4,
            vram_free_mb: Some(8192),
        };
        let sig = sign_caps(&caps, key);

        let result = reg.receive_signed_heartbeat("node-a", caps, &sig, key);
        assert!(result.unwrap(), "valid signed heartbeat must be accepted");

        let peers = reg.peers_for_model("gemma");
        assert_eq!(peers[0].active_ops, 1, "peer active_ops must be updated");
    }

    #[test]
    fn signed_heartbeat_rejected_with_wrong_key() {
        let reg = WorkerRegistry::new(60);
        reg.upsert(make_peer("node-a", "gemma:4b"));

        let caps = PeerCapabilities::default();
        let sig = sign_caps(&caps, b"correct-key");

        let err = reg
            .receive_signed_heartbeat("node-a", caps, &sig, b"wrong-key")
            .expect_err("wrong key must fail");
        assert!(
            err.to_string().contains("signature mismatch"),
            "error must mention signature mismatch: {err}"
        );
    }

    #[test]
    fn signed_heartbeat_rejected_for_unknown_peer() {
        let key = b"key";
        let reg = WorkerRegistry::new(60);
        // No upsert — peer is unknown.
        let caps = PeerCapabilities::default();
        let sig = sign_caps(&caps, key);

        let accepted = reg
            .receive_signed_heartbeat("ghost-node", caps, &sig, key)
            .unwrap();
        assert!(
            !accepted,
            "unknown peer must not be accepted even with valid signature"
        );
    }

    #[test]
    fn signed_heartbeat_rejected_with_tampered_payload() {
        let key = b"key";
        let reg = WorkerRegistry::new(60);
        reg.upsert(make_peer("node-a", "gemma:4b"));

        let caps = PeerCapabilities {
            active_ops: 2,
            ..PeerCapabilities::default()
        };
        let sig = sign_caps(&caps, key);

        // Tamper: different active_ops from what was signed.
        let tampered = PeerCapabilities {
            active_ops: 99,
            ..PeerCapabilities::default()
        };
        let err = reg
            .receive_signed_heartbeat("node-a", tampered, &sig, key)
            .expect_err("tampered payload must fail");
        assert!(
            err.to_string().contains("signature mismatch"),
            "error must mention signature mismatch: {err}"
        );
    }

    #[test]
    fn test_hostname_string() {
        let orig_host = std::env::var("HOSTNAME").ok();
        let orig_comp = std::env::var("COMPUTERNAME").ok();

        unsafe {
            // 1. Hostname with dot
            std::env::set_var("HOSTNAME", "myhost.");
            std::env::remove_var("COMPUTERNAME");
            assert_eq!(hostname_string(), "myhost.");

            // 2. Hostname with internal dot
            std::env::set_var("HOSTNAME", "myhost.foo.bar");
            assert_eq!(hostname_string(), "myhost.foo.bar.");

            // 3. Hostname without dot
            std::env::set_var("HOSTNAME", "myhost");
            assert_eq!(hostname_string(), "myhost.local.");

            // 4. Fallback to COMPUTERNAME
            std::env::remove_var("HOSTNAME");
            std::env::set_var("COMPUTERNAME", "winhost");
            assert_eq!(hostname_string(), "winhost.local.");

            // Restore env vars
            if let Some(h) = orig_host {
                std::env::set_var("HOSTNAME", h);
            } else {
                std::env::remove_var("HOSTNAME");
            }
            if let Some(c) = orig_comp {
                std::env::set_var("COMPUTERNAME", c);
            } else {
                std::env::remove_var("COMPUTERNAME");
            }
        }
    }

    #[test]
    fn test_mdns_info_to_peer() {
        let mut properties = HashMap::new();
        properties.insert("id".to_owned(), "node-abc".to_owned());
        properties.insert("models".to_owned(), "gemma,llama3.2".to_owned());

        let info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            "node-abc",
            "mymac.local.",
            "192.168.1.50",
            8000,
            Some(properties),
        )
        .expect("failed to create ServiceInfo");

        let peer = mdns_info_to_peer(&info).expect("conversion failed");
        assert_eq!(peer.id, "node-abc");
        assert_eq!(peer.addr, "http://192.168.1.50:8000");
        assert_eq!(peer.models, vec!["gemma", "llama3.2"]);
        assert!(peer.reachable);
        assert!(peer.capabilities.is_none());

        // Test with empty properties fallback to instance name
        let info_no_props = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            "node-fallback",
            "mymac.local.",
            "192.168.1.50",
            8000,
            None::<HashMap<String, String>>,
        )
        .expect("failed to create ServiceInfo");
        let peer_no_props = mdns_info_to_peer(&info_no_props).expect("conversion failed");
        assert_eq!(peer_no_props.id, "node-fallback");
    }

    #[test]
    fn test_load_static_peers() {
        let temp = tempfile::tempdir().unwrap();
        let ahma_dir = temp.path().join(".ahma").join("cluster");
        std::fs::create_dir_all(&ahma_dir).unwrap();

        let peers_json = r#"[
            {
                "id": "static-peer-1",
                "addr": "http://10.0.0.10:9000",
                "models": ["gemma"],
                "active_ops": 0,
                "reachable": true
            }
        ]"#;
        std::fs::write(ahma_dir.join("peers.json"), peers_json).unwrap();

        let reg = WorkerRegistry::new(60);
        let count = reg.load_static_peers_from(temp.path()).unwrap();

        assert_eq!(count, 1);
        let peers = reg.peers_for_model("gemma");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].id, "static-peer-1");
        assert_eq!(peers[0].addr, "http://10.0.0.10:9000");
    }
}
