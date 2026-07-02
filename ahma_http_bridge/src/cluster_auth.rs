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
//! - Signature: HMAC-SHA256 over `task_id|tool_name|issued_at|nonce|scope|body_hash`
//! - Freshness: manifests older than [`MANIFEST_MAX_AGE_SECS`] are rejected
//! - Replay protection: each nonce is recorded in a [`ManifestNonceCache`]
//! - Body binding: for `tools/call` requests, `body_hash` binds the manifest
//!   to the *exact* request body (tool name **and** arguments) that was
//!   authorized when the manifest was signed (see [`ClusterManifest::verify_body`]).
//!   Without this, the manifest only proves "some call to `tool_name` was
//!   authorized" — a tamperer who can rewrite the request body in transit
//!   (or a bug that reuses a manifest across calls) could smuggle arbitrary
//!   arguments under a validly-signed manifest.
//!
//! [`ClusterManifest`]: ClusterManifest

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    /// SHA-256 hex digest of the exact `tools/call` request body this manifest
    /// authorizes, binding the signature to the tool name **and** arguments —
    /// not just the tool name carried in [`Self::tool_name`]. `None` for the
    /// `initialize` / `notifications/initialized` / session-close requests
    /// that carry no attacker-influenced payload; see [`Self::verify_body`].
    pub body_hash: Option<String>,
    /// HMAC-SHA256 hex over `task_id|tool_name|issued_at|nonce|scope|body_hash`.
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

    /// Compute the SHA-256 hex digest of a request body, for binding a
    /// manifest to (or checking it against) the exact bytes of a `tools/call`
    /// request.
    pub fn hash_body(body: &[u8]) -> String {
        let digest = Sha256::digest(body);
        digest.iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// Verify that `body` is the exact request this manifest authorizes.
    ///
    /// Callers **must** invoke this for every `tools/call` request — the
    /// signature alone only proves *some* call to `tool_name` was signed, not
    /// which arguments. Constant-time compared, like [`Self::verify`]. Fails
    /// if the manifest carries no `body_hash` at all: a `tools/call` manifest
    /// must always be body-bound.
    pub fn verify_body(&self, body: &[u8]) -> Result<(), ClusterAuthError> {
        let Some(ref expected) = self.body_hash else {
            return Err(ClusterAuthError::MissingBodyHash);
        };
        let actual = Self::hash_body(body);
        if expected.as_bytes().ct_eq(actual.as_bytes()).into() {
            Ok(())
        } else {
            Err(ClusterAuthError::BodyHashMismatch)
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
        // `body_hash` MUST be included for the same reason: omitting it would
        // let a tamperer attach an unrelated (or absent) body_hash to an
        // otherwise-valid signature and defeat the body-binding check in
        // `verify_body`. `None` and `Some("")` are distinguished, as with `scope`.
        let body_hash = match &self.body_hash {
            Some(h) => format!("b:{h}"),
            None => "n".to_string(),
        };
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.task_id, self.tool_name, self.issued_at, self.nonce, scope, body_hash
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

    #[error(
        "Cluster manifest has no body_hash — a tools/call manifest must be bound to its request body"
    )]
    MissingBodyHash,

    #[error("Cluster manifest body_hash does not match the request body")]
    BodyHashMismatch,
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
            body_hash: None,
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
    fn hash_body_is_deterministic_and_content_sensitive() {
        let a = ClusterManifest::hash_body(b"{\"command\":\"ls\"}");
        let b = ClusterManifest::hash_body(b"{\"command\":\"ls\"}");
        let c = ClusterManifest::hash_body(b"{\"command\":\"rm -rf /\"}");
        assert_eq!(a, b, "hashing the same bytes must be deterministic");
        assert_ne!(a, c, "different bodies must hash differently");
    }

    #[test]
    fn verify_body_accepts_matching_and_rejects_tampered_body() {
        let key = b"cluster-key";
        let body = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"run_terminal_command","arguments":{"command":"ls"}}}"#;
        let mut manifest = test_manifest("run_terminal_command");
        manifest.body_hash = Some(ClusterManifest::hash_body(body));
        let manifest = manifest.sign(key);

        assert!(manifest.verify(key).is_ok());
        assert!(manifest.verify_body(body).is_ok());

        let tampered = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"run_terminal_command","arguments":{"command":"rm -rf /"}}}"#;
        assert!(matches!(
            manifest.verify_body(tampered),
            Err(ClusterAuthError::BodyHashMismatch)
        ));
    }

    #[test]
    fn verify_body_fails_when_manifest_has_no_body_hash() {
        let key = b"cluster-key";
        let manifest = test_manifest("run_terminal_command").sign(key);
        assert!(manifest.verify(key).is_ok(), "baseline manifest is valid");
        assert!(
            matches!(
                manifest.verify_body(b"anything"),
                Err(ClusterAuthError::MissingBodyHash)
            ),
            "a tools/call manifest without a body_hash must never verify a body"
        );
    }

    #[test]
    fn body_hash_is_bound_into_signature() {
        // A signature computed with one body_hash must not verify once the
        // body_hash field is swapped for another — even though `verify()`
        // alone doesn't look at the request body, the signature must still
        // cover which body_hash it authorized.
        let key = b"cluster-key";
        let mut manifest = test_manifest("run_terminal_command");
        manifest.body_hash = Some(ClusterManifest::hash_body(b"original"));
        let manifest = manifest.sign(key);
        assert!(manifest.verify(key).is_ok(), "baseline must verify");

        let mut swapped = manifest.clone();
        swapped.body_hash = Some(ClusterManifest::hash_body(b"attacker-controlled"));
        assert!(
            matches!(swapped.verify(key), Err(ClusterAuthError::InvalidSignature)),
            "a swapped body_hash must invalidate the signature"
        );
    }

    #[test]
    fn missing_and_present_body_hash_are_not_interchangeable() {
        // A manifest signed with `body_hash: None` must not verify if a
        // body_hash is later attached (and vice versa) — the `None`/`Some`
        // marker in the canonical payload must prevent this confusion.
        let key = b"cluster-key";
        let unbound = test_manifest("t").sign(key);
        let mut forged = unbound.clone();
        forged.body_hash = Some(ClusterManifest::hash_body(b"anything"));
        assert!(
            matches!(forged.verify(key), Err(ClusterAuthError::InvalidSignature)),
            "attaching a body_hash to an unbound manifest's signature must fail"
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
