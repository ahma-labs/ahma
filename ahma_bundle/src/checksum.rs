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
//! against a key the attacker cannot write, and it is not implemented.
//!
//! ## Naming
//!
//! Nothing here may be called signing or a signature, on any surface. An
//! over-promise is worse than a missing feature: a missing feature gets built,
//! whereas a promise that reads as kept is planned around. The digest is SHA-256
//! so that "the manifest says SHA-256" is true.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    fn record(
        &mut self,
        severity: BundleAuditSeverity,
        file: &str,
        description: impl Into<String>,
        recommendation: &str,
    ) {
        self.findings.push(AuditFinding {
            severity,
            file: file.to_string(),
            description: description.into(),
            recommendation: recommendation.into(),
        });
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

        for (path, filename) in json_files_in_dir(bundle_dir)? {
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
/// root. When there is a real signing scheme (`ahma_bundle/SPEC.md`) it will need a key
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
            if !verify_file_digest(bundle_dir, filename, expected_hash)? {
                return Ok(false);
            }
        }

        Ok(true)
    }
}

fn verify_file_digest(bundle_dir: &Path, filename: &str, expected_hash: &str) -> Result<bool> {
    let file_path = bundle_dir.join(filename);
    if !file_path.exists() {
        warn!("Bundle missing file: {filename}");
        return Ok(false);
    }
    let contents = std::fs::read(&file_path)?;
    if sha256_hex(&contents) != expected_hash {
        warn!("Bundle file hash mismatch: {filename}");
        return Ok(false);
    }
    Ok(true)
}

// ─────────────────────────────────────────────────────────────────────────────
// Supply-chain auditor
// ─────────────────────────────────────────────────────────────────────────────

/// Audit a bundle directory for security issues.
pub fn audit_bundle(bundle_dir: &Path) -> Result<BundleAuditResult> {
    let mut result = BundleAuditResult::default();

    for (path, filename) in json_files_in_dir(bundle_dir)? {
        result.files_checked += 1;

        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        check_embedded_secrets(&filename, &contents, &mut result);
        check_missing_path_format(&filename, &contents, &mut result);
        check_description_injection(&filename, &contents, &mut result);
    }

    result.finalize();
    Ok(result)
}

fn json_files_in_dir(bundle_dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(bundle_dir)
        .with_context(|| format!("Cannot read bundle dir: {}", bundle_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        files.push((path, filename));
    }
    Ok(files)
}

const SECRET_PREFIXES: &[&str] = &["sk-", "AKIA", "ghp_", "glpat-", "xoxb-", "xoxp-", "AIzaSy"];
const INJECTION_MARKERS: &[&str] = &["ignore previous", "jailbreak", "system:", "assistant:"];
const PATH_ARG_MARKERS: &[&str] = &["\"path\"", "\"file\"", "\"dir\""];
const PATH_FORMAT_MARKERS: &[&str] = &["\"format\": \"path\"", "\"format\":\"path\""];

fn check_forbidden_substrings(
    filename: &str,
    haystack: &str,
    patterns: &[&str],
    label: &str,
    recommendation: &str,
    result: &mut BundleAuditResult,
) {
    for pattern in patterns.iter().copied().filter(|p| haystack.contains(p)) {
        result.record(
            BundleAuditSeverity::Critical,
            filename,
            format!("{label} `{pattern}`"),
            recommendation,
        );
    }
}

fn check_embedded_secrets(filename: &str, contents: &str, result: &mut BundleAuditResult) {
    // Pattern: embedded secrets (API keys)
    check_forbidden_substrings(
        filename,
        contents,
        SECRET_PREFIXES,
        "Possible embedded secret matching prefix",
        "Remove credentials from the bundle JSON.",
        result,
    );
}

fn check_missing_path_format(filename: &str, contents: &str, result: &mut BundleAuditResult) {
    // Pattern: missing format: "path" on path-like argument names
    let lower = contents.to_ascii_lowercase();
    let has_path_arg = PATH_ARG_MARKERS.iter().any(|m| lower.contains(m));
    let has_path_format = PATH_FORMAT_MARKERS.iter().any(|m| lower.contains(m));
    if has_path_arg && !has_path_format {
        result.record(
            BundleAuditSeverity::Warning,
            filename,
            "Path-like argument may be missing `format: \"path\"` — potential sandbox escape",
            "Add `\"format\": \"path\"` to all path-type arguments.",
        );
    }
}

fn check_description_injection(filename: &str, contents: &str, result: &mut BundleAuditResult) {
    // Pattern: prompt injection via long description / hint
    let parsed: serde_json::Value = serde_json::from_str(contents).unwrap_or_default();
    let Some(desc) = parsed["description"].as_str() else {
        return;
    };
    if desc.len() > 512 {
        result.record(
            BundleAuditSeverity::Warning,
            filename,
            "Unusually long description — possible prompt injection payload",
            "Review description for embedded instructions.",
        );
    }
    check_forbidden_substrings(
        filename,
        &desc.to_ascii_lowercase(),
        INJECTION_MARKERS,
        "Possible prompt injection: description contains",
        "Remove or rewrite the suspicious description.",
        result,
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Lowercase hex SHA-256, shared with the release-artifact check.
///
/// Reuses [`ahma_common::digest::sha256_hex`] rather than adding a second encoder: the
/// two produce the same `SHA256SUMS`-style format, and there is no version of
/// this project where they should be allowed to differ.
///
/// This replaced a 64-bit DJB2 string hash that the surrounding docs described as
/// "SHA-256-like". A checksum has no reason to be weaker than the standard one,
/// and a manifest that says SHA-256 should contain SHA-256. It strengthens the
/// *corruption* check only — it does not turn the manifest into tamper-evidence,
/// because the manifest is not signed and travels with the bundle. See the module
/// header.
use ahma_common::digest::sha256_hex;

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_checksummed_bundle() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let json_path = tmp.path().join("tool.json");
        std::fs::write(&json_path, r#"{"name":"t","command":"echo"}"#).unwrap();
        BundleChecksummer::write_manifest(tmp.path()).unwrap();
        (tmp, json_path)
    }

    #[test]
    fn checksum_and_verify_roundtrip() {
        let (tmp, _) = setup_checksummed_bundle();
        assert!(BundleVerifier::new().verify(tmp.path()).unwrap());
    }

    #[test]
    fn verify_fails_after_a_file_changes() {
        let (tmp, json_path) = setup_checksummed_bundle();

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
        let (tmp, json_path) = setup_checksummed_bundle();

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
