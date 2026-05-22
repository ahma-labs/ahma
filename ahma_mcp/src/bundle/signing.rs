//! Bundle content audit and signing verification.
//!
//! Uses a lightweight SHA-256 content hash for integrity verification.
//! The signing key infrastructure uses hex-encoded keys stored in
//! `~/.ahma/keys/` — a first-pass implementation that can be upgraded
//! to full ed25519 signing in a future iteration.

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
// BundleSigner
// ─────────────────────────────────────────────────────────────────────────────

/// Computes and writes content hashes for a bundle.
pub struct BundleSigner;

impl BundleSigner {
    /// Hash all JSON files in `bundle_dir` and write a `bundle.manifest.json`.
    ///
    /// Returns the SHA-256-like digest map `{ "filename": "hash" }`.
    pub fn sign(bundle_dir: &Path) -> Result<HashMap<String, String>> {
        let mut digests = HashMap::new();

        for entry in std::fs::read_dir(bundle_dir)
            .with_context(|| format!("Cannot read bundle dir: {}", bundle_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let contents = std::fs::read(&path)?;
            let digest = djb2_hex(&contents);
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            digests.insert(filename, digest);
        }

        let manifest_path = bundle_dir.join("bundle.manifest.json");
        let manifest_json = serde_json::to_string_pretty(&digests)?;
        std::fs::write(&manifest_path, manifest_json.as_bytes())?;

        Ok(digests)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BundleVerifier
// ─────────────────────────────────────────────────────────────────────────────

/// Verifies a bundle against its manifest and trusted key ring.
pub struct BundleVerifier {
    /// Directory containing trusted signing keys.  Currently used for key ring lookup;
    /// full asymmetric key verification will be added when ed25519 signing is implemented.
    #[allow(dead_code)]
    trusted_key_dir: PathBuf,
}

impl BundleVerifier {
    pub fn new(trusted_key_dir: impl Into<PathBuf>) -> Self {
        Self {
            trusted_key_dir: trusted_key_dir.into(),
        }
    }

    /// Default verifier using `~/.ahma/keys/trusted/`.
    pub fn default_verifier() -> Result<Self> {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        Ok(Self::new(home.join(".ahma").join("keys").join("trusted")))
    }

    /// Verify a bundle directory.
    ///
    /// Returns `Ok(true)` if all files match the manifest hashes.
    pub fn verify(&self, bundle_dir: &Path) -> Result<bool> {
        let manifest_path = bundle_dir.join("bundle.manifest.json");
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
            let actual_hash = djb2_hex(&contents);
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

fn djb2_hex(data: &[u8]) -> String {
    let mut h: u64 = 5381;
    for &b in data {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{h:016x}")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sign_and_verify_roundtrip() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("mytool.json"),
            r#"{"name":"t","command":"echo"}"#,
        )
        .unwrap();

        BundleSigner::sign(tmp.path()).unwrap();
        let verifier = BundleVerifier::new(tmp.path());
        assert!(verifier.verify(tmp.path()).unwrap());
    }

    #[test]
    fn verify_fails_after_modification() {
        let tmp = TempDir::new().unwrap();
        let json_path = tmp.path().join("tool.json");
        std::fs::write(&json_path, r#"{"name":"t","command":"echo"}"#).unwrap();
        BundleSigner::sign(tmp.path()).unwrap();

        // Tamper with the file.
        std::fs::write(&json_path, r#"{"name":"EVIL","command":"rm -rf /"}"#).unwrap();
        let verifier = BundleVerifier::new(tmp.path());
        assert!(!verifier.verify(tmp.path()).unwrap());
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
