//! GitHub Build Provenance Attestation API client and Sigstore-bundle plumbing.
//!
//! This module owns everything between "we have an artifact digest" and "we have
//! Sigstore bundles to verify": the REST call to
//! `GET /repos/{owner}/{repo}/attestations/sha256:{digest}`, the optional
//! `bundle_url` indirection, and the in-toto statement parsing that binds a
//! bundle to *our* artifact.
//!
//! It replaces the equivalent code that used to live in the `sigstore-verification`
//! crate (`src/api.rs` and the statement handling in `src/verify.rs`), so the wire
//! behaviour is deliberately identical — see the per-item notes below.

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// GitHub's REST API host. Overridable in tests (wiremock), matching the
/// `api_base` convention already used by [`crate::release`].
pub(crate) const GITHUB_API_BASE: &str = "https://api.github.com";

/// Page size for the attestation listing.
///
/// The previous implementation (`sigstore-verification`) sent `per_page=30` and
/// never paginated; a subject has at most a handful of attestations, so one page
/// is the whole set in practice. Kept identical so a release that verified before
/// still verifies now.
const PER_PAGE: usize = 30;

/// Pinned GitHub REST API version, as sent by the previous implementation.
const GITHUB_API_VERSION: &str = "2022-11-28";

/// Content type GitHub uses for Snappy-compressed bundle blobs served from
/// `bundle_url` (the URL ends in `.json.sn`).
const SNAPPY_CONTENT_TYPE: &str = "application/x-snappy";

/// One entry of the `attestations` array returned by the GitHub attestation API.
///
/// GitHub returns the Sigstore bundle inline in `bundle` for the responses we
/// have observed, but the API contract also allows handing out a `bundle_url`
/// instead, so both are supported (as they were before).
#[derive(Debug, Deserialize)]
struct AttestationEntry {
    #[serde(default)]
    bundle: Option<serde_json::Value>,
    #[serde(default)]
    bundle_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AttestationsResponse {
    #[serde(default)]
    attestations: Vec<AttestationEntry>,
}

/// Fetch every Sigstore bundle GitHub holds for `sha256:{digest}` in `owner/repo`.
///
/// Returns an empty vector when GitHub has no attestation for the digest (HTTP
/// 404), which the caller turns into the "no attestation found" message. Any
/// other non-success status, or a transport error, is an `Err`.
///
/// `token` is threaded through for parity with the previous implementation; the
/// `ahma verify` / `ahma update` callers pass `None` because release attestations
/// on a public repository are readable unauthenticated.
pub(crate) async fn fetch_bundles(
    client: &reqwest::Client,
    api_base: &str,
    owner: &str,
    repo: &str,
    digest: &str,
    token: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    let url = format!("{api_base}/repos/{owner}/{repo}/attestations/sha256:{digest}");

    let mut request = client
        .get(&url)
        .header("User-Agent", "ahma-updater")
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .query(&[("per_page", PER_PAGE.to_string())]);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("Failed to fetch attestations from {url}"))?;

    // 404 means "no attestation for this digest", not an error: the previous
    // implementation returned an empty list here and the caller renders the
    // "not produced by the official pipeline" message.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        bail!("GitHub attestation API returned {status}: {body}");
    }

    let parsed: AttestationsResponse = response
        .json()
        .await
        .with_context(|| format!("Failed to parse the attestation response from {url}"))?;

    let mut bundles = Vec::new();
    for entry in parsed.attestations {
        if let Some(bundle) = entry.bundle {
            bundles.push(bundle);
        } else if let Some(bundle_url) = entry.bundle_url {
            bundles.push(download_bundle(client, &bundle_url).await?);
        }
    }
    Ok(bundles)
}

/// Download a bundle served out-of-band via `bundle_url`.
///
/// GitHub serves these Snappy-compressed (`application/x-snappy`, `.json.sn`);
/// the previous implementation sniffed the content type and decompressed, so we
/// do the same. No credentials are attached: the URL is already a signed,
/// short-lived blob URL on a different host than the API.
async fn download_bundle(client: &reqwest::Client, bundle_url: &str) -> Result<serde_json::Value> {
    let response = client
        .get(bundle_url)
        .header("User-Agent", "ahma-updater")
        .send()
        .await
        .with_context(|| format!("Failed to download attestation bundle from {bundle_url}"))?;

    if !response.status().is_success() {
        bail!(
            "Attestation bundle download from {bundle_url} returned {}",
            response.status()
        );
    }

    let snappy = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(SNAPPY_CONTENT_TYPE));

    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("Failed to read the attestation bundle from {bundle_url}"))?;

    let json = if snappy {
        let decompressed = snap::raw::Decoder::new()
            .decompress_vec(&bytes)
            .with_context(|| format!("Failed to decompress the bundle from {bundle_url}"))?;
        serde_json::from_slice(&decompressed)
    } else {
        serde_json::from_slice(&bytes)
    };
    json.with_context(|| format!("Failed to parse the bundle JSON from {bundle_url}"))
}

/// The subjects of the in-toto statement carried by a bundle's DSSE envelope,
/// in statement order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatementSubjects {
    /// `subject[i].digest.sha256`, lowercase hex, `None` when that subject
    /// carries no sha256 digest (GitHub's release attestations, for instance,
    /// name the repository itself with a `sha1`).
    pub(crate) sha256: Vec<Option<String>>,
}

impl StatementSubjects {
    /// Index of the first subject whose sha256 digest equals `digest`.
    pub(crate) fn position_of(&self, digest: &str) -> Option<usize> {
        self.sha256
            .iter()
            .position(|s| s.as_deref() == Some(digest))
    }
}

/// Extract the DSSE payload (the in-toto statement) from a bundle's JSON.
///
/// These are the very bytes Sigstore verifies the DSSE signature over: both this
/// function and `sigstore`'s own bundle parsing read `dsseEnvelope.payload` from
/// the same JSON value, so a subject read out here is a subject the signature
/// covers.
pub(crate) fn dsse_payload(bundle: &serde_json::Value) -> Result<Vec<u8>> {
    let payload = bundle
        .get("dsseEnvelope")
        .and_then(|envelope| envelope.get("payload"))
        .and_then(|payload| payload.as_str())
        .context("Attestation bundle has no dsseEnvelope.payload")?;
    BASE64
        .decode(payload)
        .context("Attestation bundle dsseEnvelope.payload is not valid base64")
}

/// Parse the subject digests of an in-toto statement.
pub(crate) fn statement_subjects(payload: &[u8]) -> Result<StatementSubjects> {
    let statement: serde_json::Value =
        serde_json::from_slice(payload).context("Attestation payload is not valid JSON")?;
    let subjects = statement
        .get("subject")
        .and_then(|s| s.as_array())
        .context("Attestation statement has no subject array")?;

    let sha256 = subjects
        .iter()
        .map(|subject| {
            subject
                .get("digest")
                .and_then(|d| d.get("sha256"))
                .and_then(|d| d.as_str())
                .map(str::to_ascii_lowercase)
        })
        .collect();
    Ok(StatementSubjects { sha256 })
}

/// Check that the bundle's Rekor transparency-log entry actually describes *this*
/// envelope, and that the signing certificate was still valid when the entry was
/// logged.
///
/// This is the CVE-2022-36056 class of attack — a valid signature paired with a
/// transparency-log entry for something else — plus the short-lived-certificate
/// invariant that makes a ten-minute Fulcio certificate meaningful in the first
/// place.
///
/// `sigstore` 0.14 performs its own version of both checks, but it cannot
/// complete them for GitHub-issued bundles: its `envelopeHash` step re-serialises
/// the parsed DSSE envelope and compares the hash against what Rekor logged,
/// which does not round-trip, so every GitHub attestation fails with
/// `SignatureErrorKind::Transparency` before either check finishes. [`super`]
/// therefore tolerates exactly that error and calls this instead.
///
/// The checks performed here bind the log entry to every part of the envelope:
///
/// * `payloadHash` is `sha256(payload)` — the statement,
/// * `signatures[0].signature` is the envelope's signature,
/// * `signatures[0].verifier` is the bundle's own signing certificate,
/// * `integratedTime` lies inside that certificate's validity window.
///
/// `envelopeHash` is deliberately *not* re-derived: it is the hash of the exact
/// bytes the signer submitted to Rekor, which are not recoverable from a parsed
/// bundle (that irreproducibility is the upstream bug above). It also adds
/// nothing — an envelope is its payload, payload type and signature; the payload
/// and signature are pinned here, and the payload type is covered by the DSSE
/// pre-authentication encoding that `sigstore` already verified the signature
/// over.
pub(crate) fn verify_log_entry(
    bundle: &serde_json::Value,
    payload: &[u8],
    certificate: &x509_cert::Certificate,
) -> Result<()> {
    let entries = bundle
        .get("verificationMaterial")
        .and_then(|m| m.get("tlogEntries"))
        .and_then(|e| e.as_array())
        .context("Attestation bundle has no verificationMaterial.tlogEntries")?;
    let [entry] = entries.as_slice() else {
        bail!(
            "Attestation bundle must carry exactly one transparency-log entry, found {}",
            entries.len()
        );
    };

    let body_b64 = entry
        .get("canonicalizedBody")
        .and_then(|b| b.as_str())
        .context("Transparency-log entry has no canonicalizedBody")?;
    let body_bytes = BASE64
        .decode(body_b64)
        .context("Transparency-log entry canonicalizedBody is not valid base64")?;
    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .context("Transparency-log entry canonicalizedBody is not valid JSON")?;

    let kind = body
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or_default();
    let api_version = body
        .get("apiVersion")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if kind != "dsse" || api_version != "0.0.1" {
        bail!("Unsupported transparency-log entry {kind} v{api_version} (expected dsse v0.0.1)");
    }

    let spec = body.get("spec").context("Log entry body has no spec")?;

    let logged_algorithm = spec
        .get("payloadHash")
        .and_then(|h| h.get("algorithm"))
        .and_then(|a| a.as_str())
        .context("Log entry body has no spec.payloadHash.algorithm")?;
    if logged_algorithm != "sha256" {
        bail!("Log entry payloadHash uses {logged_algorithm}, expected sha256");
    }
    let logged_payload_hash = spec
        .get("payloadHash")
        .and_then(|h| h.get("value"))
        .and_then(|v| v.as_str())
        .context("Log entry body has no spec.payloadHash.value")?;
    let actual_payload_hash = hex_lower(&Sha256::digest(payload));
    if logged_payload_hash.to_ascii_lowercase() != actual_payload_hash {
        bail!("Transparency-log entry describes a different attestation payload");
    }

    let logged_signature = spec
        .get("signatures")
        .and_then(|s| s.as_array())
        .and_then(|s| s.first())
        .and_then(|s| s.get("signature"))
        .and_then(|s| s.as_str())
        .context("Log entry body has no spec.signatures[0].signature")?;
    let envelope_signature = bundle
        .get("dsseEnvelope")
        .and_then(|e| e.get("signatures"))
        .and_then(|s| s.as_array())
        .and_then(|s| s.first())
        .and_then(|s| s.get("sig"))
        .and_then(|s| s.as_str())
        .context("Attestation bundle has no dsseEnvelope.signatures[0].sig")?;
    if logged_signature != envelope_signature {
        bail!("Transparency-log entry records a different signature than the bundle");
    }

    // The log entry must name the same signing certificate the bundle carries,
    // otherwise the entry proves the existence of some *other* signer's record.
    // Compared as DER so PEM line wrapping cannot make equal certificates differ.
    let logged_verifier = spec
        .get("signatures")
        .and_then(|s| s.as_array())
        .and_then(|s| s.first())
        .and_then(|s| s.get("verifier"))
        .and_then(|s| s.as_str())
        .context("Log entry body has no spec.signatures[0].verifier")?;
    let logged_verifier_pem = BASE64
        .decode(logged_verifier)
        .context("Log entry verifier is not valid base64")?;
    let logged_verifier_der = der_from_pem(&logged_verifier_pem)
        .context("Log entry verifier is not a PEM certificate")?;
    let bundle_certificate_der = certificate_der(bundle)?;
    if logged_verifier_der != bundle_certificate_der {
        bail!("Transparency-log entry names a different signing certificate than the bundle");
    }

    // A Fulcio certificate lives for ~10 minutes; the transparency log is what
    // proves the signature was made while it was valid. Without this an expired
    // (or future-dated) certificate would still verify.
    let integrated_time = entry
        .get("integratedTime")
        .and_then(|t| {
            t.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| t.as_i64())
        })
        .context("Transparency-log entry has no integratedTime")?;
    let validity = &certificate.tbs_certificate.validity;
    let not_before = validity.not_before.to_unix_duration().as_secs() as i64;
    let not_after = validity.not_after.to_unix_duration().as_secs() as i64;
    if integrated_time < not_before || integrated_time > not_after {
        bail!(
            "Attestation was logged at {integrated_time}, outside the signing certificate's \
             validity window ({not_before}..={not_after})"
        );
    }

    Ok(())
}

/// The bundle's own signing certificate, DER-encoded.
pub(crate) fn certificate_der(bundle: &serde_json::Value) -> Result<Vec<u8>> {
    let raw = bundle
        .get("verificationMaterial")
        .and_then(|m| m.get("certificate"))
        .and_then(|c| c.get("rawBytes"))
        .and_then(|b| b.as_str())
        .context("Attestation bundle has no verificationMaterial.certificate.rawBytes")?;
    BASE64
        .decode(raw)
        .context("Attestation bundle certificate is not valid base64")
}

/// Extract the DER body of a single PEM certificate.
fn der_from_pem(pem: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(pem).context("PEM is not UTF-8")?;
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .flat_map(|line| line.chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    if body.is_empty() {
        bail!("PEM contains no certificate body");
    }
    BASE64.decode(body).context("PEM body is not valid base64")
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real GitHub Build Provenance attestation for ahma v0.19.7, captured from
    /// `GET /repos/ahma-labs/ahma/attestations/sha256:<archive digest>`.
    const BUNDLE: &str = include_str!("../../tests/fixtures/ahma_build_provenance_bundle.json");
    /// sha256 of `ahma-release-linux-x86_64.tar.gz` — subject[0] of the fixture.
    const ARCHIVE_DIGEST: &str = "4bd93abdc592c4fd25428f965e8664fa9de641362f85490006c4b0ff0e5778d2";
    /// sha256 of the raw `ahma` binary — subject[1] of the fixture. This is the
    /// digest `ahma verify --self` looks up.
    const BINARY_DIGEST: &str = "837aed25c6f56eaa71a245ee90a1e52971f9bb796ee949fa8824608b665aa7e2";

    fn fixture() -> serde_json::Value {
        serde_json::from_str(BUNDLE).expect("fixture bundle must be valid JSON")
    }

    fn fixture_certificate(bundle: &serde_json::Value) -> x509_cert::Certificate {
        use x509_cert::der::Decode as _;
        x509_cert::Certificate::from_der(&certificate_der(bundle).unwrap())
            .expect("fixture certificate must be valid DER")
    }

    #[test]
    fn statement_subjects_lists_both_release_subjects_in_order() {
        let bundle = fixture();
        let payload = dsse_payload(&bundle).unwrap();
        let subjects = statement_subjects(&payload).unwrap();
        assert_eq!(
            subjects.sha256,
            vec![
                Some(ARCHIVE_DIGEST.to_string()),
                Some(BINARY_DIGEST.to_string())
            ]
        );
    }

    #[test]
    fn position_of_finds_first_and_later_subjects() {
        let bundle = fixture();
        let payload = dsse_payload(&bundle).unwrap();
        let subjects = statement_subjects(&payload).unwrap();
        assert_eq!(subjects.position_of(ARCHIVE_DIGEST), Some(0));
        assert_eq!(subjects.position_of(BINARY_DIGEST), Some(1));
    }

    #[test]
    fn position_of_rejects_an_unlisted_digest() {
        let bundle = fixture();
        let payload = dsse_payload(&bundle).unwrap();
        let subjects = statement_subjects(&payload).unwrap();
        assert_eq!(subjects.position_of(&"0".repeat(64)), None);
    }

    #[test]
    fn subjects_without_sha256_are_none_not_an_error() {
        // GitHub's *release* attestations name the repository with a sha1 only.
        let payload = br#"{"subject":[{"uri":"pkg:github/o/r@v1","digest":{"sha1":"abc"}},
                           {"name":"a","digest":{"sha256":"AB12"}}]}"#;
        let subjects = statement_subjects(payload).unwrap();
        assert_eq!(subjects.sha256, vec![None, Some("ab12".to_string())]);
    }

    #[test]
    fn statement_subjects_rejects_a_payload_without_subjects() {
        let err = statement_subjects(br#"{"predicateType":"x"}"#).unwrap_err();
        assert!(
            err.to_string().contains("no subject array"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn dsse_payload_rejects_a_bundle_without_an_envelope() {
        let err = dsse_payload(&serde_json::json!({"mediaType": "x"})).unwrap_err();
        assert!(
            err.to_string().contains("dsseEnvelope.payload"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn log_entry_matches_the_real_envelope() {
        let bundle = fixture();
        let payload = dsse_payload(&bundle).unwrap();
        let cert = fixture_certificate(&bundle);
        verify_log_entry(&bundle, &payload, &cert).expect("real bundle must be self-consistent");
    }

    #[test]
    fn log_entry_rejects_a_payload_it_does_not_describe() {
        let bundle = fixture();
        let cert = fixture_certificate(&bundle);
        let err = verify_log_entry(&bundle, b"a different attestation", &cert).unwrap_err();
        assert!(
            err.to_string().contains("different attestation payload"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn log_entry_rejects_a_swapped_signature() {
        let mut bundle = fixture();
        let payload = dsse_payload(&bundle).unwrap();
        let cert = fixture_certificate(&bundle);
        bundle["dsseEnvelope"]["signatures"][0]["sig"] = serde_json::json!("bm90LXRoZS1zaWc=");
        let err = verify_log_entry(&bundle, &payload, &cert).unwrap_err();
        assert!(
            err.to_string().contains("different signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn log_entry_rejects_a_swapped_signing_certificate() {
        // Swap the bundle's certificate for another real Fulcio leaf: the log
        // entry still names the original, so the entry no longer describes this
        // bundle's signer.
        let mut bundle = fixture();
        let payload = dsse_payload(&bundle).unwrap();
        let cert = fixture_certificate(&bundle);
        bundle["verificationMaterial"]["certificate"]["rawBytes"] =
            serde_json::json!(OTHER_LEAF_CERT_B64);
        let err = verify_log_entry(&bundle, &payload, &cert).unwrap_err();
        assert!(
            err.to_string().contains("different signing certificate"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn log_entry_rejects_a_timestamp_outside_the_certificate_validity() {
        let mut bundle = fixture();
        let payload = dsse_payload(&bundle).unwrap();
        let cert = fixture_certificate(&bundle);
        // Fulcio certificates live ~10 minutes; a log entry from 2001 cannot
        // belong to this one.
        bundle["verificationMaterial"]["tlogEntries"][0]["integratedTime"] =
            serde_json::json!("1000000000");
        let err = verify_log_entry(&bundle, &payload, &cert).unwrap_err();
        assert!(
            err.to_string().contains("validity window"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn log_entry_rejects_a_bundle_with_no_log_entry() {
        // GitHub's release attestations carry an RFC3161 timestamp and no Rekor
        // entry; they must not sneak past this check.
        let bundle = fixture();
        let cert = fixture_certificate(&bundle);
        let err = verify_log_entry(
            &serde_json::json!({"verificationMaterial": {"certificate": {}}}),
            b"{}",
            &cert,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("tlogEntries"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn der_from_pem_round_trips_the_bundle_certificate() {
        let bundle = fixture();
        let der = certificate_der(&bundle).unwrap();
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            BASE64.encode(&der)
        );
        assert_eq!(der_from_pem(pem.as_bytes()).unwrap(), der);
    }

    #[test]
    fn der_from_pem_rejects_an_empty_body() {
        let err =
            der_from_pem(b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n").unwrap_err();
        assert!(
            err.to_string().contains("no certificate body"),
            "unexpected error: {err}"
        );
    }

    /// A second, unrelated real Fulcio leaf (GitHub's `dotcom.releases.github.com`
    /// release-attestation signer), used to prove the certificate binding bites.
    const OTHER_LEAF_CERT_B64: &str = "MIICKzCCAbCgAwIBAgIUdkRBqOh332mFwwbiaqkdO+NCHGowCgYIKoZIzj0EAwMwODEVMBMGA1UEChMMR2l0SHViLCBJbmMuMR8wHQYDVQQDExZGdWxjaW8gSW50ZXJtZWRpYXRlIGwxMB4XDTI1MTExMzAwMDAwMFoXDTI2MTExMzAwMDAwMFowKjEVMBMGA1UEChMMR2l0SHViLCBJbmMuMREwDwYDVQQDEwhBdHRlc3RlcjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABGR+ZMdEcHEuf1YbMBlhXTtqPmyCqQAIDyQk+TVHPywC1ZNxk5LQlRb8Vol/XZA1utIrSNdt8lkoi3RdrOhOWlOjgaUwgaIwDgYDVR0PAQH/BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMDMAwGA1UdEwEB/wQCMAAwHQYDVR0OBBYEFJYm8uweBLpLZHSfo3kCjGBsvV8gMB8GA1UdIwQYMBaAFMDhuFKkS08+3no4EQbPSY6hRZszMC0GA1UdEQQmMCSGImh0dHBzOi8vZG90Y29tLnJlbGVhc2VzLmdpdGh1Yi5jb20wCgYIKoZIzj0EAwMDaQAwZgIxAN2T1WebCAcTXlyWDN4a+crAVMmFTR+zCiaknrR2MRG13xaDH28D6/ddwSCOsS3s/AIxAOVCzN/I7KBuRkVSlMsJRZxK1ZKkhxmhF2TSjI0Z2qY8I1zsXt5d0ZVT13WGAY5A7A==";

    #[test]
    fn hex_lower_is_lowercase_and_padded() {
        assert_eq!(hex_lower(&[0x0a, 0xff, 0x00]), "0aff00");
    }
}
