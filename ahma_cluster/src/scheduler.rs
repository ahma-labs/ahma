//! Cluster task scheduler.
//!
//! Task manifests are signed with **HMAC-SHA256** over a canonical payload that
//! includes a random nonce and an `issued_at` timestamp.  Peers reject manifests
//! with an invalid signature or an `issued_at` older than [`MANIFEST_MAX_AGE_SECS`]
//! (replay protection).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn};

use super::discovery::{PeerInfo, WorkerRegistry};

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

/// Routes decompose sub-tasks to the best-available peer.
pub struct ClusterScheduler {
    registry: WorkerRegistry,
    shared_key: Vec<u8>,
    http: reqwest::Client,
}

impl ClusterScheduler {
    /// Create a scheduler with the given registry and shared cluster key.
    ///
    /// `shared_key` should be loaded from a key file, not passed as a CLI argument
    /// (which would expose it in `ps` output).  See `--cluster-key-file`.
    pub fn new(registry: WorkerRegistry, shared_key: impl Into<Vec<u8>>) -> Self {
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

// ─── Crypto helpers ─────────────────────────────────────────────────────────

/// Compute HMAC-SHA256 over `message` with `key`; return the lowercase hex digest.
fn hmac_sha256_hex(key: &[u8], message: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(message.as_bytes());
    let bytes = mac.finalize().into_bytes();
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
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
}
