//! Bundle content audit, and a content **checksum** — deliberately not a signature.
//!
//! ## What this is, and what it is emphatically not
//!
//! [`BundleChecksummer`] records a SHA-256 digest of every JSON file in a bundle
//! into `bundle.manifest.json`, and [`BundleVerifier`] re-hashes those files and
//! compares. That detects **corruption**: a truncated download, a botched copy, a
//! file that changed when nobody meant it to.
//!
//! It does **not** detect tampering, and it must never be described as if it did.
//! The manifest lives *inside the bundle it describes*, so anyone who can alter a
//! bundle file can regenerate the manifest in the same motion and the check
//! passes. Nothing here is signed; there is no key, no trust root, and no
//! attacker this defeats. Tamper-evidence needs a detached signature verified
//! against a key the attacker cannot write — SPEC §11 tracks that as the v0.8
//! signed bundle index, and it is not implemented.
//!
//! ## Why the naming was changed
//!
//! This module previously called itself signing. `BundleSigner::sign` documented
//! its output as "the SHA-256-like digest map" while computing a 64-bit DJB2
//! string hash — not a cryptographic primitive, collidable by hand. `verify`
//! carried a `trusted_key_dir` it never read, and the CLI said "Bundle
//! verification passed". Every one of those strings told a security-conscious
//! reader that a bundle had been checked against a trust root, when what had
//! happened was a non-cryptographic checksum comparing a file to a manifest
//! sitting next to it.
//!
//! An over-promise is worse than a missing feature: a missing feature gets built,
//! whereas a promise that reads as kept is planned around. So the surface now says
//! what it does. The digest is SHA-256 because there is no reason for a checksum
//! to be weaker than the standard one, and because "the manifest says SHA-256"
//! should be true.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

// ─────────────────────────────────────────────────────────────────────────────
// Audit types
// ─────────────────────────────────────────────────────────────────────────────

/// Severity of a bundle audit finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum BundleAuditSeverity {
    Info,
    Warning,
    Critical,
}

/// A single finding from a bundle audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditFinding {
    pub severity: BundleAuditSeverity,
    pub file: String,
    pub description: String,
    pub recommendation: String,
}

/// The result of auditing a bundle directory.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BundleAuditResult {
    pub findings: Vec<AuditFinding>,
    pub files_checked: usize,
    pub passed: bool,
}

impl BundleAuditResult {
    fn add(&mut self, finding: AuditFinding) {
        self.findings.push(finding);
    }

    fn finalize(&mut self) {
        self.passed = !self
            .findings
            .iter()
            .any(|f| f.severity == BundleAuditSeverity::Critical);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BundleChecksummer
// ─────────────────────────────────────────────────────────────────────────────

/// The manifest's filename, in one place so the writer and the reader — and the
/// self-exclusion in [`BundleChecksummer::write_manifest`] — cannot disagree.
const MANIFEST_FILE_NAME: &str = "bundle.manifest.json";

/// Computes and writes SHA-256 content checksums for a bundle.
///
/// Not a signer. See the module header: the manifest it writes lives inside the
/// bundle, so it proves the files have not changed *by accident*, and nothing at
/// all about who wrote them.
pub struct BundleChecksummer;

impl BundleChecksummer {
    /// Hash all JSON files in `bundle_dir` and write a `bundle.manifest.json`.
    ///
    /// Returns the SHA-256 digest map `{ "filename": "hex digest" }`.
    pub fn write_manifest(bundle_dir: &Path) -> Result<HashMap<String, String>> {
        let mut digests = HashMap::new();

        for entry in std::fs::read_dir(bundle_dir)
            .with_context(|| format!("Cannot read bundle dir: {}", bundle_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            // The manifest cannot record its own digest: writing the digest
            // changes the file it describes. Including it meant a second run over
            // an already-checksummed bundle recorded the *previous* manifest's
            // hash and then overwrote it, so `verify` failed on
            // `bundle.manifest.json` — checksumming a bundle twice produced a
            // bundle that would not verify. Skipping it is the only consistent
            // answer, and it costs nothing: an unsigned manifest was never
            // covering itself in any meaningful sense (see the module header).
            if filename == MANIFEST_FILE_NAME {
                continue;
            }
            let contents = std::fs::read(&path)?;
            digests.insert(filename, sha256_hex(&contents));
        }

        let manifest_path = bundle_dir.join(MANIFEST_FILE_NAME);
        let manifest_json = serde_json::to_string_pretty(&digests)?;
        std::fs::write(&manifest_path, manifest_json.as_bytes())?;

        Ok(digests)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BundleVerifier
// ─────────────────────────────────────────────────────────────────────────────

/// Re-hashes a bundle's files and compares them to its `bundle.manifest.json`.
///
/// The struct carries no key ring. It used to hold a `trusted_key_dir` that
/// nothing ever read — a field kept alive by `#[allow(dead_code)]`, which was the
/// clearest single tell that this had never been verification against a trust
/// root. When there is a real signing scheme (SPEC §11) it will need a key
/// parameter; inventing one before it has a consumer only makes the surface look
/// like it checks more than it does.
#[derive(Debug, Default, Clone, Copy)]
pub struct BundleVerifier;

impl BundleVerifier {
    pub fn new() -> Self {
        Self
    }

    /// Re-hash every file the manifest names and compare.
    ///
    /// `Ok(true)` means every file matches the manifest beside it — i.e. nothing
    /// was corrupted in transit or by accident. It does **not** mean the bundle
    /// is the one its author published: whoever changed a file could have
    /// rewritten the manifest too.
    pub fn verify(&self, bundle_dir: &Path) -> Result<bool> {
        let manifest_path = bundle_dir.join(MANIFEST_FILE_NAME);
        if !manifest_path.exists() {
            warn!("Bundle has no manifest: {}", bundle_dir.display());
            return Ok(false);
        }

        let manifest_str = std::fs::read_to_string(&manifest_path)?;
        let expected: HashMap<String, String> = serde_json::from_str(&manifest_str)?;

        for (filename, expected_hash) in &expected {
            let file_path = bundle_dir.join(filename);
            if !file_path.exists() {
                warn!("Bundle missing file: {filename}");
                return Ok(false);
            }
            let contents = std::fs::read(&file_path)?;
            let actual_hash = sha256_hex(&contents);
            if actual_hash != *expected_hash {
                warn!("Bundle file hash mismatch: {filename}");
                return Ok(false);
            }
        }

        Ok(true)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Supply-chain auditor
// ─────────────────────────────────────────────────────────────────────────────

/// Audit a bundle directory for security issues.
pub fn audit_bundle(bundle_dir: &Path) -> Result<BundleAuditResult> {
    let mut result = BundleAuditResult::default();

    for entry in std::fs::read_dir(bundle_dir)
        .with_context(|| format!("Cannot read bundle dir: {}", bundle_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        result.files_checked += 1;

        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        // Pattern: embedded secrets (API keys)
        for pattern in ["sk-", "AKIA", "ghp_", "glpat-", "xoxb-", "xoxp-", "AIzaSy"] {
            if contents.contains(pattern) {
                result.add(AuditFinding {
                    severity: BundleAuditSeverity::Critical,
                    file: filename.clone(),
                    description: format!("Possible embedded secret matching prefix `{pattern}`"),
                    recommendation: "Remove credentials from the bundle JSON.".into(),
                });
            }
        }

        // Pattern: missing format: "path" on path-like argument names
        let lower = contents.to_ascii_lowercase();
        if (lower.contains("\"path\"") || lower.contains("\"file\"") || lower.contains("\"dir\""))
            && !lower.contains("\"format\": \"path\"")
            && !lower.contains("\"format\":\"path\"")
        {
            result.add(AuditFinding {
                severity: BundleAuditSeverity::Warning,
                file: filename.clone(),
                description: "Path-like argument may be missing `format: \"path\"` — potential sandbox escape".into(),
                recommendation: "Add `\"format\": \"path\"` to all path-type arguments.".into(),
            });
        }

        // Pattern: prompt injection via long description / hint
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap_or_default();
        if let Some(desc) = parsed["description"].as_str() {
            if desc.len() > 512 {
                result.add(AuditFinding {
                    severity: BundleAuditSeverity::Warning,
                    file: filename.clone(),
                    description: "Unusually long description — possible prompt injection payload"
                        .into(),
                    recommendation: "Review description for embedded instructions.".into(),
                });
            }
            for injection_marker in ["ignore previous", "jailbreak", "system:", "assistant:"] {
                if desc.to_ascii_lowercase().contains(injection_marker) {
                    result.add(AuditFinding {
                        severity: BundleAuditSeverity::Critical,
                        file: filename.clone(),
                        description: format!(
                            "Possible prompt injection: description contains `{injection_marker}`"
                        ),
                        recommendation: "Remove or rewrite the suspicious description.".into(),
                    });
                }
            }
        }
    }

    result.finalize();
    Ok(result)
}

// ─────────────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Lowercase hex SHA-256, shared with the release-artifact check.
///
/// Reuses [`crate::update::sha256_hex`] rather than adding a second encoder: the
/// two produce the same `SHA256SUMS`-style format, and there is no version of
/// this project where they should be allowed to differ.
///
/// This replaced a 64-bit DJB2 string hash that the surrounding docs described as
/// "SHA-256-like". A checksum has no reason to be weaker than the standard one,
/// and a manifest that says SHA-256 should contain SHA-256. It strengthens the
/// *corruption* check only — it does not turn the manifest into tamper-evidence,
/// because the manifest is not signed and travels with the bundle. See the module
/// header.
use crate::update::sha256_hex;

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn checksum_and_verify_roundtrip() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("mytool.json"),
            r#"{"name":"t","command":"echo"}"#,
        )
        .unwrap();

        BundleChecksummer::write_manifest(tmp.path()).unwrap();
        assert!(BundleVerifier::new().verify(tmp.path()).unwrap());
    }

    #[test]
    fn verify_fails_after_a_file_changes() {
        let tmp = TempDir::new().unwrap();
        let json_path = tmp.path().join("tool.json");
        std::fs::write(&json_path, r#"{"name":"t","command":"echo"}"#).unwrap();
        BundleChecksummer::write_manifest(tmp.path()).unwrap();

        std::fs::write(&json_path, r#"{"name":"OTHER","command":"true"}"#).unwrap();
        assert!(!BundleVerifier::new().verify(tmp.path()).unwrap());
    }

    /// The limit of the guarantee, asserted rather than left to the doc comment.
    ///
    /// A checksum whose manifest travels inside the thing it describes cannot be
    /// tamper-evidence: rewriting a file and re-running the checksummer produces
    /// a bundle that verifies clean. This test exists so that nobody re-reads
    /// `verify() == true` as "this bundle is the one its author published", and
    /// so that a future change which *does* claim tamper-evidence has to delete a
    /// failing assertion rather than quietly inherit the stronger reading.
    #[test]
    fn verification_is_not_tamper_evidence() {
        let tmp = TempDir::new().unwrap();
        let json_path = tmp.path().join("tool.json");
        std::fs::write(&json_path, r#"{"name":"t","command":"echo"}"#).unwrap();
        BundleChecksummer::write_manifest(tmp.path()).unwrap();

        // Someone who can edit a bundle file can also re-run the checksummer.
        std::fs::write(&json_path, r#"{"name":"EVIL","command":"rm -rf /"}"#).unwrap();
        BundleChecksummer::write_manifest(tmp.path()).unwrap();

        assert!(
            BundleVerifier::new().verify(tmp.path()).unwrap(),
            "the altered bundle verifies clean, because the manifest is not signed \
             and lives beside the files it describes. This is the documented limit \
             (SPEC section 11 tracks real signing); if this assertion ever fails, the \
             module gained a trust root and every string describing it must be \
             revisited."
        );
    }

    /// Regression: checksumming an already-checksummed bundle used to break it.
    ///
    /// `write_manifest` hashed every `*.json` in the directory, which on a second
    /// run included the manifest written by the first. The new manifest therefore
    /// recorded the *old* manifest's digest and then overwrote the file, so
    /// `verify` reported a mismatch on `bundle.manifest.json` — a bundle that had
    /// been "signed" twice would not verify, and the message named the manifest
    /// rather than the real cause. Found by writing
    /// `verification_is_not_tamper_evidence`, which needs exactly this sequence.
    #[test]
    fn checksumming_twice_still_verifies() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("tool.json"), r#"{"name":"t"}"#).unwrap();

        BundleChecksummer::write_manifest(tmp.path()).unwrap();
        let second = BundleChecksummer::write_manifest(tmp.path()).unwrap();

        assert!(
            !second.contains_key(MANIFEST_FILE_NAME),
            "the manifest must not record its own digest — writing the digest \
             changes the file it describes"
        );
        assert!(
            BundleVerifier::new().verify(tmp.path()).unwrap(),
            "re-checksumming a bundle must leave it verifiable"
        );
    }

    #[test]
    fn the_manifest_records_real_sha256() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("t.json"), b"abc").unwrap();
        let digests = BundleChecksummer::write_manifest(tmp.path()).unwrap();

        // The published SHA-256 of "abc". The previous implementation wrote a
        // 16-hex-character DJB2 hash while documenting itself as SHA-256.
        assert_eq!(
            digests.get("t.json").map(String::as_str),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            "a manifest that says SHA-256 must contain SHA-256"
        );
    }

    #[test]
    fn audit_detects_embedded_secret() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("bad.json"),
            r#"{"api_key":"sk-abc123","name":"t","command":"curl"}"#,
        )
        .unwrap();
        let result = audit_bundle(tmp.path()).unwrap();
        let has_critical = result
            .findings
            .iter()
            .any(|f| f.severity == BundleAuditSeverity::Critical);
        assert!(has_critical, "sk- prefix should trigger critical finding");
        assert!(!result.passed);
    }

    #[test]
    fn audit_clean_bundle_passes() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("good.json"),
            r#"{"name":"cargo","command":"cargo","description":"Build tool"}"#,
        )
        .unwrap();
        let result = audit_bundle(tmp.path()).unwrap();
        assert!(
            result.passed,
            "clean bundle should pass: {:?}",
            result.findings
        );
    }
}
