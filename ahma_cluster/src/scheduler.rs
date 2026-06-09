//! Cluster task scheduler.
//!
//! Task manifests are signed with **HMAC-SHA256** over a canonical payload that
//! includes a random nonce and an `issued_at` timestamp.  Peers reject manifests
//! with an invalid signature or an `issued_at` older than [`MANIFEST_MAX_AGE_SECS`]
//! (replay protection).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ahma_common::{
    config::TransportMode,
    peer_transport::PeerDispatch,
};
use anyhow::{Context, Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn};

use super::discovery::{PeerInfo, WorkerRegistry};
use super::transport::{new_cluster_dispatch, default_transport_preference};

/// HMAC-SHA256 type alias.
type HmacSha256 = Hmac<Sha256>;

/// Reject manifests older than this many seconds (replay-protection window).
pub const MANIFEST_MAX_AGE_SECS: u64 = 60;

/// A signed task manifest sent to a remote worker peer.
///
/// # Security
///
/// The `signature` field is an HMAC-SHA256 hex digest computed over the canonical
/// payload `task_id|prompt|model|llm_base_url|nonce|issued_at`.  The `nonce`
/// (random UUID) and `issued_at` (Unix seconds) together prevent replay attacks
/// within the configured [`MANIFEST_MAX_AGE_SECS`] window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskManifest {
    pub task_id: String,
    pub prompt: String,
    pub model: String,
    pub llm_base_url: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    /// Random UUID — prevents replay even if the same task is dispatched twice.
    pub nonce: String,
    /// Unix timestamp (seconds) when this manifest was created.
    pub issued_at: u64,
    /// HMAC-SHA256 hex over `task_id|prompt|model|llm_base_url|nonce|issued_at`.
    pub signature: String,
}

impl TaskManifest {
    /// Sign a manifest with `shared_key`.
    ///
    /// Sets a fresh random `nonce` and the current `issued_at` timestamp, then
    /// computes HMAC-SHA256 over the canonical payload.
    pub fn sign(mut self, shared_key: &[u8]) -> Self {
        self.nonce = uuid::Uuid::new_v4().to_string();
        self.issued_at = unix_now_secs();
        let payload = self.canonical_payload();
        self.signature = hmac_sha256_hex(shared_key, &payload);
        self
    }

    /// Verify this manifest's signature and freshness.
    ///
    /// Returns `Ok(())` if the signature is valid and the manifest is within the
    /// replay-protection window.
    pub fn verify(&self, shared_key: &[u8]) -> Result<()> {
        let age = unix_now_secs().saturating_sub(self.issued_at);
        if age > MANIFEST_MAX_AGE_SECS {
            bail!(
                "Manifest replay rejected: issued_at is {}s ago (max {}s)",
                age,
                MANIFEST_MAX_AGE_SECS
            );
        }
        let expected = hmac_sha256_hex(shared_key, &self.canonical_payload());
        if expected.as_bytes().ct_eq(self.signature.as_bytes()).into() {
            Ok(())
        } else {
            bail!("Manifest signature verification failed")
        }
    }

    /// Verify signature and freshness, then record the nonce in `cache`.
    ///
    /// Returns `Err` if the signature is invalid, the manifest is expired, or
    /// the nonce has already been seen (replay attack).
    pub fn verify_with_nonce_cache(&self, shared_key: &[u8], cache: &NonceCache) -> Result<()> {
        self.verify(shared_key)?;
        if !cache.check_and_record(&self.nonce, self.issued_at) {
            bail!(
                "Manifest replay rejected: nonce '{}' already seen",
                self.nonce
            );
        }
        Ok(())
    }

    fn canonical_payload(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.task_id, self.prompt, self.model, self.llm_base_url, self.nonce, self.issued_at
        )
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

/// Thread-safe in-memory cache of seen manifest nonces.
///
/// Protects against replay attacks by rejecting any nonce that was already
/// accepted within the [`MANIFEST_MAX_AGE_SECS`] window.
/// Stale entries are evicted on each insert to bound memory growth.
#[derive(Clone, Default)]
pub struct NonceCache {
    inner: Arc<Mutex<HashMap<String, u64>>>,
}

impl NonceCache {
    /// Attempt to record `nonce` with the `issued_at` Unix timestamp.
    ///
    /// Returns `true` if the nonce is new and was recorded.
    /// Returns `false` if the nonce was already seen (replay).
    ///
    /// Evicts entries whose `issued_at` is older than [`MANIFEST_MAX_AGE_SECS`]
    /// before checking, so the map stays bounded to the active window.
    pub fn check_and_record(&self, nonce: &str, issued_at: u64) -> bool {
        let now = unix_now_secs();
        let mut inner = self.inner.lock().expect("NonceCache lock poisoned");
        // Evict entries that have fallen outside the replay-protection window.
        inner.retain(|_, &mut ts| now.saturating_sub(ts) <= MANIFEST_MAX_AGE_SECS);
        match inner.entry(nonce.to_owned()) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(issued_at);
                true
            }
        }
    }
}

/// Routes decompose sub-tasks to the best-available peer.
pub struct ClusterScheduler {
    registry: WorkerRegistry,
    shared_key: Vec<u8>,
    /// Cluster peer transport — production wraps `ClusterTransport` (HTTP/QUIC);
    /// tests can inject [`ahma_common::peer_transport::InMemoryPeerDispatch`].
    transport: Arc<dyn PeerDispatch>,
    /// Nonce cache used by the receiver-side `verify_with_nonce_cache`.
    #[allow(dead_code)]
    nonce_cache: NonceCache,
}

impl ClusterScheduler {
    /// Create a scheduler with the given registry and shared cluster key.
    ///
    /// `shared_key` should be loaded from a key file, not passed as a CLI argument
    /// (which would expose it in `ps` output).  See `--cluster-key-file`.
    ///
    /// Logs the first 16 hex characters of the SHA-256 fingerprint of the key at
    /// `info` level so operators can verify that all cluster nodes share the same
    /// key without exposing the key itself.
    pub fn new(registry: WorkerRegistry, shared_key: impl Into<Vec<u8>>) -> Self {
        let key_bytes: Vec<u8> = shared_key.into();

        // Log a short fingerprint of the shared key so operators can cross-check
        // that all nodes were configured with the same secret without leaking the
        // key itself.
        let fingerprint = sha256_hex_fingerprint(&key_bytes);
        info!("Cluster key SHA-256 fingerprint: {}…", &fingerprint[..16]);

        Self {
            registry,
            shared_key: key_bytes,
            transport: new_cluster_dispatch(default_transport_preference(), None),
            nonce_cache: NonceCache::default(),
        }
    }

    /// Configure a custom transport preference and optional TLS CA certificate.
    ///
    /// Call this on a newly-created scheduler to override the default transport
    /// preference (`[Quic, Http2, Http1]`):
    ///
    /// ```no_run
    /// use ahma_cluster::ClusterScheduler;
    /// use ahma_common::config::TransportMode;
    ///
    /// # async fn example() {
    /// let sched = ClusterScheduler::new(todo!(), b"key")
    ///     .with_transport(vec![TransportMode::Http2, TransportMode::Http1], None);
    /// # }
    /// ```
    #[must_use]
    pub fn with_transport(mut self, preference: Vec<TransportMode>, ca_pem: Option<&str>) -> Self {
        self.transport = new_cluster_dispatch(preference, ca_pem);
        self
    }

    /// Inject a custom [`PeerDispatch`] implementation.
    ///
    /// Use this in tests to inject an [`ahma_common::peer_transport::InMemoryPeerDispatch`]
    /// without spawning real network connections.
    #[must_use]
    pub fn with_peer_dispatch(mut self, dispatch: Arc<dyn PeerDispatch>) -> Self {
        self.transport = dispatch;
        self
    }

    /// Switch to the MCP-based cluster dispatch (P3).
    ///
    /// Replaces the default raw HTTP/QUIC transport with an [`McpPeerDispatch`]
    /// that calls `tools/call` on the peer's MCP endpoint, authenticated via
    /// `X-Ahma-Cluster-Manifest` (HMAC-SHA256 signed with the scheduler's
    /// shared key).  This is the preferred dispatch method for P3+ clusters.
    ///
    /// * `preference` — transport preference order (same semantics as
    ///   [`with_transport`]).  Pass [`default_transport_preference()`] for the
    ///   recommended QUIC → HTTP/2 → HTTP/1 order.
    /// * `ca_pem` — optional PEM CA cert for QUIC TLS peer verification.
    ///
    /// [`McpPeerDispatch`]: crate::mcp_dispatch::McpPeerDispatch
    /// [`with_transport`]: Self::with_transport
    /// [`default_transport_preference()`]: crate::transport::default_transport_preference
    #[must_use]
    pub fn use_mcp_dispatch(
        mut self,
        preference: Vec<TransportMode>,
        ca_pem: Option<&str>,
    ) -> Self {
        use crate::mcp_dispatch::McpPeerDispatch;
        self.transport =
            McpPeerDispatch::new(self.shared_key.clone(), preference, ca_pem).into_arc_dispatch();
        self
    }

    /// Pick the best peer for `model` and dispatch `manifest` to it.
    ///
    /// Returns `None` if no suitable peer is available (caller falls back to local execution).
    /// Pick the best peer for `model` and dispatch `manifest` to it.
    ///
    /// Returns `None` if no suitable peer is available; the caller falls back to
    /// running the sub-task locally.
    pub async fn schedule(&self, manifest: TaskManifest) -> Option<TaskResult> {
        let mut peers = self.registry.peers_for_model(&manifest.model);
        if peers.is_empty() {
            debug!("No remote peers available for model {}", manifest.model);
            return None;
        }

        // Sort by scoring function: prefer loaded model, then fewer active_ops, then free VRAM.
        let model = manifest.model.clone();
        peers.sort_by_key(|p| p.load_score_for(&model));
        let peer = &peers[0];

        debug!(
            "Selected peer {} (score={}) for model {}",
            peer.id,
            peer.load_score_for(&manifest.model),
            manifest.model
        );

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
        info!("Dispatching task {} to peer {}", signed.task_id, peer.id);

        // Build an MCP `tools/call` JSON-RPC payload (P3).
        //
        // `McpPeerDispatch` performs the full MCP session lifecycle
        // (initialize → notifications/initialized → tools/call → DELETE).
        // The HMAC-signed cluster manifest travels in the
        // `X-Ahma-Cluster-Manifest` header; authentication is fully
        // decoupled from the tool arguments.
        //
        // `ClusterTransport` (legacy) ignores the JSON-RPC envelope and
        // posts the raw value to `/tasks` — the path parameter keeps it
        // working until all peers are upgraded.
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "run_llm_task",
                "arguments": {
                    "task_id": signed.task_id,
                    "prompt": signed.prompt,
                    "model": signed.model,
                    "llm_base_url": signed.llm_base_url,
                    "max_tokens": signed.max_tokens,
                    "timeout_secs": signed.timeout_secs,
                    "nonce": signed.nonce,
                    "issued_at": signed.issued_at,
                    "signature": signed.signature,
                }
            }
        });

        let response_value = self
            .transport
            .dispatch(&peer.addr, "/tasks", payload)
            .await
            .with_context(|| format!("Failed to dispatch task to peer {}", peer.id))?;

        let result: TaskResult = serde_json::from_value(response_value)
            .context("Failed to parse task result from peer")?;

        Ok(result)
    }
}

// ─── Crypto helpers ─────────────────────────────────────────────────────────

/// Compute HMAC-SHA256 over `message` with `key`; return the lowercase hex digest.
pub(crate) fn hmac_sha256_hex(key: &[u8], message: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(message.as_bytes());
    let bytes = mac.finalize().into_bytes();
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Return the lowercase hex SHA-256 digest of `key` for use as a key fingerprint.
///
/// Only used in log messages so operators can verify cluster nodes share the same
/// key without exposing the key itself.
fn sha256_hex_fingerprint(key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(key);
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_manifest() -> TaskManifest {
        TaskManifest {
            task_id: "t1".into(),
            prompt: "What is 2+2?".into(),
            model: "gemma:4b".into(),
            llm_base_url: "http://localhost:11434/v1".into(),
            max_tokens: 256,
            timeout_secs: 30,
            nonce: String::new(),
            issued_at: 0,
            signature: String::new(),
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        let key = b"my-shared-key";
        let signed = base_manifest().sign(key);
        signed.verify(key).expect("fresh manifest must verify");
    }

    #[test]
    fn wrong_key_fails_verification() {
        let signed = base_manifest().sign(b"correct-key");
        assert!(signed.verify(b"wrong-key").is_err(), "wrong key must fail");
    }

    #[test]
    fn tampered_prompt_fails_verification() {
        let key = b"test-key";
        let mut signed = base_manifest().sign(key);
        signed.prompt = "Injected prompt".into();
        assert!(signed.verify(key).is_err(), "tampered field must fail");
    }

    #[test]
    fn tampered_signature_fails_verification() {
        let key = b"test-key";
        let mut signed = base_manifest().sign(key);
        let mut chars: Vec<char> = signed.signature.chars().collect();
        chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
        signed.signature = chars.into_iter().collect();
        assert!(signed.verify(key).is_err(), "tampered signature must fail");
    }

    #[test]
    fn expired_manifest_fails_verification() {
        let key = b"test-key";
        let mut signed = base_manifest().sign(key);
        signed.issued_at = unix_now_secs().saturating_sub(MANIFEST_MAX_AGE_SECS + 1);
        assert!(signed.verify(key).is_err(), "expired manifest must fail");
    }

    /// Regression: prove that a djb2 collision cannot forge an HMAC-SHA256 signature.
    ///
    /// The old `hmac_stub` used djb2, which accepts collisions trivially.
    /// HMAC-SHA256 must produce distinct digests for distinct messages.
    #[test]
    fn djb2_collision_does_not_forge_hmac() {
        let key = b"any-key";
        let msg1 = "task1|prompt1|model|url|nonce1|1000000";
        let msg2 = "task2|prompt2|model|url|nonce2|1000000";
        assert_ne!(
            hmac_sha256_hex(key, msg1),
            hmac_sha256_hex(key, msg2),
            "HMAC-SHA256 must not produce the same digest for different payloads"
        );
    }

    #[test]
    fn nonces_are_unique_per_signing() {
        let key = b"k";
        let s1 = base_manifest().sign(key);
        let s2 = base_manifest().sign(key);
        assert_ne!(s1.nonce, s2.nonce, "each signing must use a fresh nonce");
        assert_ne!(
            s1.signature, s2.signature,
            "different nonces produce different signatures"
        );
    }

    #[test]
    fn scheduler_returns_none_when_no_peers() {
        let reg = WorkerRegistry::new(60);
        let scheduler = ClusterScheduler::new(reg, b"test-key".to_vec());
        assert!(scheduler.registry.all_live().is_empty());
    }

    // ── Nonce-cache / replay-protection tests ────────────────────────────────

    #[test]
    fn nonce_cache_accepts_fresh_nonce() {
        let cache = NonceCache::default();
        assert!(
            cache.check_and_record("unique-nonce-1", unix_now_secs()),
            "fresh nonce must be accepted"
        );
    }

    #[test]
    fn nonce_cache_rejects_duplicate_nonce() {
        let cache = NonceCache::default();
        let ts = unix_now_secs();
        assert!(cache.check_and_record("dup-nonce", ts));
        assert!(
            !cache.check_and_record("dup-nonce", ts),
            "duplicate nonce must be rejected"
        );
    }

    #[test]
    fn verify_with_nonce_cache_allows_first_verify() {
        let key = b"replay-test-key";
        let signed = base_manifest().sign(key);
        let cache = NonceCache::default();
        signed
            .verify_with_nonce_cache(key, &cache)
            .expect("first verification must succeed");
    }

    #[test]
    fn verify_with_nonce_cache_rejects_replay() {
        let key = b"replay-test-key";
        let signed = base_manifest().sign(key);
        let cache = NonceCache::default();
        signed
            .verify_with_nonce_cache(key, &cache)
            .expect("first verification must succeed");
        let err = signed
            .verify_with_nonce_cache(key, &cache)
            .expect_err("replayed manifest must be rejected");
        assert!(
            err.to_string().contains("already seen"),
            "error must mention replay: {err}"
        );
    }

    #[test]
    fn verify_with_nonce_cache_rejects_wrong_key_before_recording_nonce() {
        let signed = base_manifest().sign(b"correct-key");
        let cache = NonceCache::default();
        let err = signed
            .verify_with_nonce_cache(b"wrong-key", &cache)
            .expect_err("wrong key must fail");
        assert!(
            err.to_string().contains("signature"),
            "error must mention signature failure: {err}"
        );
        // After a wrong-key rejection, the nonce must NOT be recorded —
        // so a correct-key retry should succeed.
        signed
            .verify_with_nonce_cache(b"correct-key", &cache)
            .expect("correct-key retry must succeed after failed wrong-key attempt");
    }

    #[tokio::test]
    async fn test_scheduler_schedule_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let mut buf = [0; 1024];
                let _ = stream.read(&mut buf).await;
                let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 79\r\nConnection: close\r\n\r\n{\"task_id\":\"t1\",\"text\":\"scheduler success\",\"success\":true,\"worker_id\":\"node-a\"}\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let reg = WorkerRegistry::new(60);
        let peer = PeerInfo {
            id: "node-a".to_string(),
            addr: format!("http://127.0.0.1:{port}"),
            models: vec!["gemma:4b".to_string()],
            active_ops: 0,
            reachable: true,
            capabilities: None,
        };
        reg.upsert(peer);

        let shared_key = b"cluster-key".to_vec();
        let scheduler =
            ClusterScheduler::new(reg, shared_key).with_transport(vec![TransportMode::Http1], None);

        let manifest = base_manifest();
        let result = scheduler
            .schedule(manifest)
            .await
            .expect("scheduling failed");

        assert_eq!(result.task_id, "t1");
        assert_eq!(result.text, "scheduler success");
        assert!(result.success);
        assert_eq!(result.worker_id, "node-a");
    }
}
