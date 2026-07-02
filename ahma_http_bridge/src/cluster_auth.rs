//! Cluster peer authentication for forwarded MCP calls (P3).
//!
//! When a remote `ahma` cluster peer forwards a `tools/call` to this bridge, it
//! attaches an HMAC-signed [`ClusterManifest`] in the `X-Ahma-Cluster-Manifest`
//! request header.  The bridge verifies the signature and freshness before
//! letting the request proceed.
//!
//! ## Why a custom header rather than a bearer token?
//!
//! Bearer tokens are single-value secrets; they cannot carry structured metadata
//! such as the originating `task_id`, `issued_at` timestamp, or nonce needed for
//! replay protection.  The cluster manifest bundles all of that into a single
//! JSON blob, serialised as compact JSON, then base64url-encoded and placed in
//! `X-Ahma-Cluster-Manifest`.
//!
//! ## Wire format
//!
//! ```text
//! X-Ahma-Cluster-Manifest: <base64url(compact_json(ClusterManifest))>
//! ```
//!
//! The encoding uses **URL-safe base64 without padding** (RFC 4648 §5).
//!
//! ## Security
//!
//! - Signature: HMAC-SHA256 over `task_id|tool_name|issued_at|nonce|scope`
//! - Freshness: manifests older than [`MANIFEST_MAX_AGE_SECS`] are rejected
//! - Replay protection: each nonce is recorded in a [`ManifestNonceCache`]
//!
//! [`ClusterManifest`]: ClusterManifest

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq as _;

/// Header name for the cluster manifest.
pub const CLUSTER_MANIFEST_HEADER: &str = "x-ahma-cluster-manifest";

/// Reject manifests older than this many seconds (replay-protection window).
pub const MANIFEST_MAX_AGE_SECS: u64 = 60;

type HmacSha256 = Hmac<Sha256>;

// ─── ClusterManifest ──────────────────────────────────────────────────────────

/// Authenticated manifest forwarded by a cluster peer along with an MCP
/// `tools/call` request.
///
/// Mirrors (but does not depend on) the `TaskManifest` in `ahma_cluster::scheduler`,
/// carrying just the fields the bridge needs to authenticate and route the call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterManifest {
    /// Caller-assigned task identifier (used in log correlation).
    pub task_id: String,
    /// MCP tool name being called on behalf of the cluster peer.
    pub tool_name: String,
    /// Optional workspace root to use as the session's sandbox scope.
    ///
    /// When `None`, the bridge falls back to its configured
    /// `default_sandbox_scope`.
    pub scope: Option<String>,
    /// Random UUID — prevents replay even if the same task is dispatched twice.
    pub nonce: String,
    /// Unix timestamp (seconds) when this manifest was created.
    pub issued_at: u64,
    /// HMAC-SHA256 hex over `task_id|tool_name|issued_at|nonce|scope`.
    pub signature: String,
}

impl ClusterManifest {
    /// Sign the manifest with `shared_key`.
    ///
    /// Generates a fresh `nonce` and `issued_at`, then computes the HMAC.
    pub fn sign(mut self, shared_key: &[u8]) -> Self {
        self.nonce = uuid::Uuid::new_v4().to_string();
        self.issued_at = unix_now_secs();
        let payload = self.canonical_payload();
        self.signature = hmac_hex(shared_key, &payload);
        self
    }

    /// Verify signature and freshness.
    pub fn verify(&self, shared_key: &[u8]) -> Result<(), ClusterAuthError> {
        let age = unix_now_secs().saturating_sub(self.issued_at);
        if age > MANIFEST_MAX_AGE_SECS {
            return Err(ClusterAuthError::Expired { age_secs: age });
        }
        let expected = hmac_hex(shared_key, &self.canonical_payload());
        if expected.as_bytes().ct_eq(self.signature.as_bytes()).into() {
            Ok(())
        } else {
            Err(ClusterAuthError::InvalidSignature)
        }
    }

    /// Verify, then record nonce in `cache` to block replays.
    pub fn verify_with_nonce_cache(
        &self,
        shared_key: &[u8],
        cache: &ManifestNonceCache,
    ) -> Result<(), ClusterAuthError> {
        self.verify(shared_key)?;
        if !cache.check_and_record(&self.nonce, self.issued_at) {
            return Err(ClusterAuthError::ReplayedNonce {
                nonce: self.nonce.clone(),
            });
        }
        Ok(())
    }

    /// Encode as URL-safe base64 for the `X-Ahma-Cluster-Manifest` header.
    pub fn to_header_value(&self) -> Result<String, serde_json::Error> {
        let json = serde_json::to_string(self)?;
        Ok(URL_SAFE_NO_PAD.encode(json.as_bytes()))
    }

    /// Decode from the `X-Ahma-Cluster-Manifest` header value.
    pub fn from_header_value(value: &str) -> Result<Self, ClusterAuthError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value.trim())
            .map_err(|e| ClusterAuthError::Malformed(format!("base64 decode: {e}")))?;
        let json = std::str::from_utf8(&bytes)
            .map_err(|e| ClusterAuthError::Malformed(format!("UTF-8 decode: {e}")))?;
        serde_json::from_str(json)
            .map_err(|e| ClusterAuthError::Malformed(format!("JSON parse: {e}")))
    }

    /// Decode from the header value, verify signature and freshness, and record
    /// the nonce in `cache` to block replays.
    ///
    /// Convenience wrapper combining [`from_header_value`] and
    /// [`verify_with_nonce_cache`].
    pub fn decode_and_verify(
        value: &str,
        shared_key: &[u8],
        cache: &ManifestNonceCache,
    ) -> Result<Self, ClusterAuthError> {
        let manifest = Self::from_header_value(value)?;
        manifest.verify_with_nonce_cache(shared_key, cache)?;
        Ok(manifest)
    }

    fn canonical_payload(&self) -> String {
        // `scope` MUST be included: it selects the session's sandbox scope, so
        // leaving it out of the signature would let a tamperer widen or redirect
        // the scope of an otherwise-valid manifest. `None` and `Some("")` are
        // distinguished by a marker so they cannot be forged into each other.
        let scope = match &self.scope {
            Some(s) => format!("s:{s}"),
            None => "n".to_string(),
        };
        format!(
            "{}|{}|{}|{}|{}",
            self.task_id, self.tool_name, self.issued_at, self.nonce, scope
        )
    }
}

// ─── ClusterAuthError ────────────────────────────────────────────────────────

/// Errors returned by [`ClusterManifest::verify`] and friends.
#[derive(Debug, thiserror::Error)]
pub enum ClusterAuthError {
    #[error("Cluster manifest signature is invalid")]
    InvalidSignature,

    #[error("Cluster manifest expired: issued_at is {age_secs}s ago (max {max}s)", max = MANIFEST_MAX_AGE_SECS)]
    Expired { age_secs: u64 },

    #[error("Cluster manifest nonce replayed: {nonce}")]
    ReplayedNonce { nonce: String },

    #[error("Cluster manifest is malformed: {0}")]
    Malformed(String),

    #[error("Cluster manifest header missing")]
    Missing,
}

// ─── ManifestNonceCache ──────────────────────────────────────────────────────

/// Thread-safe nonce cache that prevents manifest replay attacks.
///
/// Entries expire automatically once they are older than
/// `MANIFEST_MAX_AGE_SECS * 2` (generous buffer to handle clock skew and
/// delayed eviction).
#[derive(Debug, Default)]
pub struct ManifestNonceCache {
    seen: Mutex<HashMap<String, u64>>,
}

impl ManifestNonceCache {
    /// Create a new, empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `nonce` as seen at `issued_at` and return `true` if it was fresh.
    ///
    /// Returns `false` (replay detected) if the nonce was already present.
    pub fn check_and_record(&self, nonce: &str, issued_at: u64) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());

        // Evict stale entries (older than 2× the replay window).
        let evict_before = unix_now_secs().saturating_sub(MANIFEST_MAX_AGE_SECS * 2);
        seen.retain(|_, ts| *ts >= evict_before);

        if seen.contains_key(nonce) {
            return false;
        }
        seen.insert(nonce.to_string(), issued_at);
        true
    }
}

// ─── Crypto helpers ──────────────────────────────────────────────────────────

fn hmac_hex(key: &[u8], message: &str) -> String {
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
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest(tool_name: &str) -> ClusterManifest {
        ClusterManifest {
            task_id: "task-1".into(),
            tool_name: tool_name.into(),
            scope: None,
            nonce: String::new(),
            issued_at: 0,
            signature: String::new(),
        }
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let key = b"test-secret-key";
        let manifest = test_manifest("cargo_build").sign(key);
        assert!(manifest.verify(key).is_ok());
    }

    #[test]
    fn tampering_with_scope_fails_verification() {
        // The scope selects the session's sandbox scope, so a valid manifest
        // whose scope is altered in transit must fail verification.
        let key = b"cluster-key";
        let mut manifest = test_manifest("run_terminal_command").sign(key);
        assert!(manifest.verify(key).is_ok(), "baseline must verify");
        manifest.scope = Some("/etc".into()); // tamper: widen the scope
        assert!(
            matches!(
                manifest.verify(key),
                Err(ClusterAuthError::InvalidSignature)
            ),
            "a tampered scope must invalidate the signature"
        );
    }

    #[test]
    fn scope_is_bound_into_signature() {
        // Two manifests identical except for scope must not share a signature,
        // and each must reject the other's scope.
        let key = b"cluster-key";
        let mut a = test_manifest("t");
        a.scope = Some("/a".into());
        let a = a.sign(key);
        let mut b = ClusterManifest {
            scope: Some("/b".into()),
            ..a.clone()
        };
        // Recompute b's signature would differ; but even reusing a's signature
        // with b's scope must fail.
        b.signature = a.signature.clone();
        assert!(
            b.verify(key).is_err(),
            "a's signature must not cover b's scope"
        );
    }

    #[test]
    fn wrong_key_fails_verification() {
        let key = b"correct-key";
        let wrong_key = b"wrong-key";
        let manifest = test_manifest("cargo_test").sign(key);
        assert!(matches!(
            manifest.verify(wrong_key),
            Err(ClusterAuthError::InvalidSignature)
        ));
    }

    #[test]
    fn header_encode_decode_roundtrip() {
        let key = b"test-key";
        let original = test_manifest("run_terminal_command").sign(key);
        let encoded = original.to_header_value().expect("encode");
        let decoded = ClusterManifest::from_header_value(&encoded).expect("decode");
        assert_eq!(original.task_id, decoded.task_id);
        assert_eq!(original.tool_name, decoded.tool_name);
        assert_eq!(original.signature, decoded.signature);
    }

    #[test]
    fn nonce_cache_blocks_replay() {
        let cache = ManifestNonceCache::new();
        let nonce = "unique-nonce-123";
        let ts = unix_now_secs();
        assert!(
            cache.check_and_record(nonce, ts),
            "first record should succeed"
        );
        assert!(
            !cache.check_and_record(nonce, ts),
            "replay should be rejected"
        );
    }

    #[test]
    fn nonce_cache_allows_different_nonces() {
        let cache = ManifestNonceCache::new();
        let ts = unix_now_secs();
        assert!(cache.check_and_record("nonce-a", ts));
        assert!(cache.check_and_record("nonce-b", ts));
    }

    #[test]
    fn verify_with_nonce_cache_blocks_replay() {
        let key = b"test-key";
        let manifest = test_manifest("cargo_build").sign(key);
        let cache = ManifestNonceCache::new();
        assert!(manifest.verify_with_nonce_cache(key, &cache).is_ok());
        assert!(matches!(
            manifest.verify_with_nonce_cache(key, &cache),
            Err(ClusterAuthError::ReplayedNonce { .. })
        ));
    }
}
