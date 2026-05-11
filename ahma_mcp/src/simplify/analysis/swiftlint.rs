//! SwiftLint analyzer for Swift complexity analysis.
//!
//! [SwiftLint](https://github.com/realm/SwiftLint) is the de-facto Swift linter
//! with built-in `cyclomatic_complexity` and `cognitive_complexity` rules.
//! When run with `--reporter checkstyle`, it emits Checkstyle-format XML that
//! this module parses via the shared [`super::checkstyle`] parser.
//!
//! # Installation
//!
//! ```sh
//! brew install swiftlint        # macOS — official Homebrew package
//! ```
//!
//! # How it works
//!
//! ```sh
//! swiftlint lint --reporter checkstyle --quiet --path <project_dir>
//! ```
//!
//! Output goes to stdout. Only the `cyclomatic_complexity` and
//! `cognitive_complexity` rules are meaningful for simplicity scoring;
//! other rules (style, naming, etc.) are captured as issues but do not affect
//! the cyclomatic/cognitive aggregate metrics.
//!
//! # Configuration
//!
//! If the project contains a `.swiftlint.yml` at its root, SwiftLint uses it
//! automatically. Otherwise sensible defaults are applied. Projects can lower
//! default thresholds in their config to get more granular signals.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::checkstyle;
use super::external::{ExternalAnalyzer, ExternalMetrics};
use crate::simplify::models::Language;

/// Analyzer that invokes SwiftLint in Checkstyle-report mode.
pub struct SwiftLintAnalyzer;

impl ExternalAnalyzer for SwiftLintAnalyzer {
    fn name(&self) -> &'static str {
        "swiftlint"
    }

    fn supports_language(&self, language: Language) -> bool {
        matches!(language, Language::Swift)
    }

    fn is_available(&self, _project_dir: &Path) -> bool {
        swiftlint_in_path()
    }

    fn setup_hint(&self, _project_dir: &Path) -> Option<String> {
        Some(
            "Install SwiftLint for Swift complexity analysis:\n  \
             brew install swiftlint\n  \
             Optionally add a .swiftlint.yml to the project root to tune thresholds.\n  \
             Alternatively, `pip install lizard` provides lighter-weight zero-config metrics."
                .to_string(),
        )
    }

    fn analyze(&self, project_dir: &Path) -> Result<HashMap<PathBuf, ExternalMetrics>> {
        eprintln!("  [swiftlint] linting {} ...", project_dir.display());

        // SwiftLint writes Checkstyle XML to stdout when --reporter checkstyle is used.
        // --quiet suppresses the default progress banner.
        // swiftlint exits non-zero when violations are found — this is expected.
        let output = Command::new("swiftlint")
            .args(["lint", "--reporter", "checkstyle", "--quiet", "--path"])
            .arg(project_dir)
            .current_dir(project_dir)
            .output()
            .context("Failed to spawn swiftlint")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(HashMap::new());
        }

        checkstyle::parse_checkstyle_xml_str(&stdout, "swiftlint")
    }
}

/// Returns `true` if `swiftlint` is on PATH.
fn swiftlint_in_path() -> bool {
    Command::new("which")
        .arg("swiftlint")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal Checkstyle XML as produced by `swiftlint lint --reporter checkstyle`.
    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<checkstyle version="4.3">
  <file name="/project/Sources/MyApp/ViewController.swift">
    <error line="42" column="1" severity="warning"
           message="Cyclomatic Complexity Violation: Function should have complexity 10 or less: currently 14 (cyclomatic_complexity)"
           source="SwiftLint.CyclomaticComplexityRule"/>
    <error line="80" column="1" severity="warning"
           message="Cognitive Complexity Violation: Function should have complexity 5 or less: currently 9 (cognitive_complexity)"
           source="SwiftLint.CognitiveComplexityRule"/>
    <error line="5" column="1" severity="warning"
           message="File Length Violation: File should contain 400 lines or less: currently 450 (file_length)"
           source="SwiftLint.FileLengthRule"/>
  </file>
</checkstyle>"#;

    #[test]
    fn test_supports_swift_only() {
        let analyzer = SwiftLintAnalyzer;
        assert!(analyzer.supports_language(Language::Swift));
        assert!(!analyzer.supports_language(Language::Kotlin));
        assert!(!analyzer.supports_language(Language::Rust));
        assert!(!analyzer.supports_language(Language::ObjectiveC));
    }

    #[test]
    fn test_setup_hint_always_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hint = SwiftLintAnalyzer.setup_hint(tmp.path());
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("swiftlint"));
    }

    #[test]
    fn test_parse_checkstyle_xml_cyclomatic() {
        let result = checkstyle::parse_checkstyle_xml_str(SAMPLE_XML, "swiftlint").unwrap();
        assert_eq!(result.len(), 1, "should find 1 file");
        let vc = &result[&PathBuf::from("/project/Sources/MyApp/ViewController.swift")];
        // "currently 14" → cyclomatic = 14
        assert_eq!(vc.cyclomatic, Some(14.0));
    }

    #[test]
    fn test_parse_checkstyle_xml_cognitive() {
        let result = checkstyle::parse_checkstyle_xml_str(SAMPLE_XML, "swiftlint").unwrap();
        let vc = &result[&PathBuf::from("/project/Sources/MyApp/ViewController.swift")];
        // "currently 9" → cognitive = 9
        assert_eq!(vc.cognitive, Some(9.0));
    }

    #[test]
    fn test_parse_checkstyle_xml_issue_count() {
        let result = checkstyle::parse_checkstyle_xml_str(SAMPLE_XML, "swiftlint").unwrap();
        let vc = &result[&PathBuf::from("/project/Sources/MyApp/ViewController.swift")];
        // All 3 errors should be captured as issues.
        assert_eq!(vc.issues.len(), 3);
    }

    #[test]
    fn test_analyzer_tag() {
        let result = checkstyle::parse_checkstyle_xml_str(SAMPLE_XML, "swiftlint").unwrap();
        let vc = result.values().next().unwrap();
        assert_eq!(vc.analyzer, "swiftlint");
    }
}
