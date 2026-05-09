//! Standalone detekt-cli analyzer for Kotlin complexity analysis.
//!
//! Unlike [`super::detekt::DetektAnalyzer`] (which requires a Gradle project
//! with the detekt plugin pre-configured), this analyzer invokes the standalone
//! `detekt` CLI binary directly — no Gradle, no per-project setup needed.
//!
//! # Installation
//!
//! ```sh
//! brew install detekt          # macOS
//! # or download from https://github.com/detekt/detekt/releases
//! ```
//!
//! The binary is named `detekt` when installed via Homebrew and `detekt-cli`
//! on some other distributions; both are checked.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::checkstyle;
use super::external::{ExternalAnalyzer, ExternalMetrics};
use crate::simplify::models::Language;

/// Analyzer that invokes the standalone `detekt` CLI binary (no Gradle required).
pub struct DetektCliAnalyzer;

impl ExternalAnalyzer for DetektCliAnalyzer {
    fn name(&self) -> &'static str {
        "detekt-cli"
    }

    fn supports_language(&self, language: Language) -> bool {
        matches!(language, Language::Kotlin)
    }

    fn is_available(&self, _project_dir: &Path) -> bool {
        detekt_cli_binary().is_some()
    }

    fn setup_hint(&self, _project_dir: &Path) -> Option<String> {
        Some(
            "Install standalone detekt CLI for zero-Gradle Kotlin analysis:\n  \
             brew install detekt\n  \
             or download from https://github.com/detekt/detekt/releases\n  \
             Alternatively, `pip install lizard` provides lighter-weight \
             zero-config complexity metrics."
                .to_string(),
        )
    }

    fn analyze(&self, project_dir: &Path) -> Result<HashMap<PathBuf, ExternalMetrics>> {
        let detekt = detekt_cli_binary().context("detekt binary not found in PATH")?;

        // detekt CLI needs a file path for --report xml:<path>; write to temp file.
        let tmp = tempfile::Builder::new()
            .suffix(".xml")
            .tempfile()
            .context("Failed to create temp file for detekt-cli report")?;
        let report_path = tmp.path().to_path_buf();

        eprintln!(
            "  [detekt-cli] running {} --input {} ...",
            detekt.display(),
            project_dir.display()
        );

        // detekt exits non-zero when violations are found — this is expected.
        let output = Command::new(&detekt)
            .args([
                "--input",
                project_dir.to_str().unwrap_or("."),
                "--report",
                &format!("xml:{}", report_path.display()),
            ])
            .current_dir(project_dir)
            .output()
            .context("Failed to spawn detekt-cli")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "Detekt finished" in stderr is normal; anything else warrants a log line.
            if !stderr.trim().is_empty() && !stderr.contains("Detekt finished") {
                eprintln!(
                    "  [detekt-cli] exited {} — stderr: {}",
                    output.status,
                    stderr.trim()
                );
            }
        }

        if report_path.exists() {
            checkstyle::parse_checkstyle_xml(&report_path, "detekt-cli")
        } else {
            Ok(HashMap::new())
        }
    }
}

/// Find the `detekt` CLI binary in PATH.
///
/// Homebrew installs it as `detekt`; some other distributions use `detekt-cli`.
fn detekt_cli_binary() -> Option<PathBuf> {
    for candidate in &["detekt", "detekt-cli"] {
        if let Ok(output) = Command::new("which").arg(candidate).output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout);
                let path = PathBuf::from(path_str.trim());
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supports_kotlin_only() {
        let analyzer = DetektCliAnalyzer;
        assert!(analyzer.supports_language(Language::Kotlin));
        assert!(!analyzer.supports_language(Language::Rust));
        assert!(!analyzer.supports_language(Language::Swift));
        assert!(!analyzer.supports_language(Language::Java));
    }

    #[test]
    fn test_setup_hint_always_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hint = DetektCliAnalyzer.setup_hint(tmp.path());
        assert!(hint.is_some());
        let msg = hint.unwrap();
        assert!(msg.contains("detekt"));
    }

    #[test]
    fn test_parse_checkstyle_xml_roundtrip() {
        // Verify the shared Checkstyle parser produces correct metrics via this analyzer.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<checkstyle version="8.0">
  <file name="/project/src/main/kotlin/Foo.kt">
    <error line="10" column="1" severity="warning"
           message="The function doWork has a cyclomatic complexity of 7 (threshold = 1)"
           source="detekt.complexity.CyclomaticComplexMethod"/>
  </file>
</checkstyle>"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), xml).unwrap();
        let result = checkstyle::parse_checkstyle_xml(tmp.path(), "detekt-cli").unwrap();
        assert_eq!(result.len(), 1);
        let metrics = result.values().next().unwrap();
        assert_eq!(metrics.analyzer, "detekt-cli");
        assert_eq!(metrics.cyclomatic, Some(7.0));
    }
}
