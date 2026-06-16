//! Lizard universal complexity analyzer integration.
//!
//! [Lizard](https://github.com/terryyin/lizard) is a zero-configuration
//! cyclomatic complexity analyzer that supports many languages for which
//! `rust-code-analysis` produces no usable complexity metrics: Kotlin (rca
//! only stub-parses it, so `conversion::analyze_file` skips it), Swift (rca
//! does not parse it at all), Go, C#, Objective-C, Ruby, PHP, and more. It also
//! covers Java/JavaScript/TypeScript as a fallback when rca is unavailable.
//!
//! # Installation
//!
//! ```sh
//! pip install lizard          # any platform
//! brew install lizard         # macOS (if available via homebrew)
//! ```
//!
//! # How it works
//!
//! Lizard is invoked as:
//! ```sh
//! lizard <project_dir> --json -l <lang1> -l <lang2> ...
//! ```
//! producing a JSON array of per-file records, each with a `function_list`.
//! The CCN (cyclomatic complexity number) for each function is summed
//! per-file and stored as `ExternalMetrics::cyclomatic`.
//!
//! Lizard does not compute cognitive complexity, so `cognitive` is left `None`.
//!
//! # Threshold
//!
//! Only functions with CCN > [`CCN_THRESHOLD`] generate an [`ExternalIssue`].
//! This filters out trivial getters/setters that dilute the hotspot signal.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::external::{ExternalAnalyzer, ExternalIssue, ExternalMetrics, Severity};
use crate::simplify::models::Language;

/// Only emit issues for functions whose cyclomatic complexity exceeds this.
const CCN_THRESHOLD: f64 = 5.0;

/// Analyzer that invokes the `lizard` command-line tool.
pub struct LizardAnalyzer;

impl ExternalAnalyzer for LizardAnalyzer {
    fn name(&self) -> &'static str {
        "lizard"
    }

    fn supports_language(&self, language: Language) -> bool {
        matches!(
            language,
            Language::Kotlin
                | Language::Swift
                | Language::Java
                | Language::Go
                | Language::CSharp
                | Language::ObjectiveC
                | Language::JavaScript
                | Language::TypeScript
        )
    }

    fn is_available(&self, _project_dir: &Path) -> bool {
        Command::new("lizard")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn setup_hint(&self, _project_dir: &Path) -> Option<String> {
        Some(
            "Install Lizard for zero-config Kotlin/Swift/Java/Go/C# complexity analysis:\n  \
             pip install lizard\n  \
             Lizard requires no project setup and analyzes all supported languages immediately."
                .to_string(),
        )
    }

    fn analyze(&self, project_dir: &Path) -> Result<HashMap<PathBuf, ExternalMetrics>> {
        eprintln!("  [lizard] analyzing {} ...", project_dir.display());

        let output = Command::new("lizard")
            .arg(project_dir)
            .arg("--json")
            .current_dir(project_dir)
            .output()
            .context("Failed to spawn lizard")?;

        // lizard exits non-zero when it finds complex functions — treat as normal.
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(HashMap::new());
        }

        parse_lizard_json(&stdout)
    }
}

/// Parse the JSON output from `lizard --json` into per-file metrics.
///
/// The JSON is an array of file records:
/// ```json
/// [
///   {
///     "filename": "path/to/File.kt",
///     "function_list": [
///       {
///         "name": "myFunction",
///         "cyclomatic_complexity": 5,
///         "start_line": 10
///       }
///     ]
///   }
/// ]
/// ```
pub fn parse_lizard_json(json: &str) -> Result<HashMap<PathBuf, ExternalMetrics>> {
    let doc: Value = serde_json::from_str(json).context("Failed to parse lizard JSON output")?;

    // lizard --json can emit either an array or an object with a "files" key.
    let files = match &doc {
        Value::Array(arr) => arr.as_slice(),
        Value::Object(obj) => {
            if let Some(Value::Array(arr)) = obj.get("files") {
                arr.as_slice()
            } else {
                return Ok(HashMap::new());
            }
        }
        _ => return Ok(HashMap::new()),
    };

    let mut metrics: HashMap<PathBuf, ExternalMetrics> = HashMap::new();

    for file_record in files {
        let Some(filename) = file_record.get("filename").and_then(|v| v.as_str()) else {
            continue;
        };
        let file_path = PathBuf::from(filename);

        let Some(functions) = file_record.get("function_list").and_then(|v| v.as_array()) else {
            continue;
        };

        let entry = metrics.entry(file_path).or_insert_with(|| ExternalMetrics {
            analyzer: "lizard".to_string(),
            ..ExternalMetrics::default()
        });

        for func in functions {
            let ccn = func
                .get("cyclomatic_complexity")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0);
            let start_line = func.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let fn_name = func
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Accumulate per-file cyclomatic total.
            *entry.cyclomatic.get_or_insert(0.0) += ccn;

            // Only emit issues for complex-enough functions.
            if ccn > CCN_THRESHOLD {
                entry.issues.push(ExternalIssue {
                    rule: "CyclomaticComplexity".to_string(),
                    severity: Severity::Warning,
                    message: format!(
                        "Function '{}' has a cyclomatic complexity of {} (threshold = {})",
                        fn_name, ccn as u32, CCN_THRESHOLD as u32
                    ),
                    function_name: if fn_name.is_empty() {
                        None
                    } else {
                        Some(fn_name)
                    },
                    start_line,
                    complexity_value: Some(ccn),
                });
            }
        }
    }

    Ok(metrics)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_JSON: &str = r#"[
  {
    "filename": "/project/src/main/kotlin/Manager.kt",
    "function_list": [
      {
        "name": "processData",
        "cyclomatic_complexity": 12,
        "start_line": 15
      },
      {
        "name": "simpleHelper",
        "cyclomatic_complexity": 2,
        "start_line": 80
      }
    ]
  },
  {
    "filename": "/project/src/main/kotlin/Service.kt",
    "function_list": [
      {
        "name": "doWork",
        "cyclomatic_complexity": 4,
        "start_line": 10
      }
    ]
  }
]"#;

    #[test]
    fn test_parse_finds_both_files() {
        let result = parse_lizard_json(SAMPLE_JSON).unwrap();
        assert_eq!(result.len(), 2, "Should find 2 files");
    }

    #[test]
    fn test_parse_sums_cyclomatic_per_file() {
        let result = parse_lizard_json(SAMPLE_JSON).unwrap();
        let manager = &result[&PathBuf::from("/project/src/main/kotlin/Manager.kt")];
        // 12 + 2 = 14
        assert_eq!(manager.cyclomatic, Some(14.0));
    }

    #[test]
    fn test_parse_cognitive_always_none() {
        let result = parse_lizard_json(SAMPLE_JSON).unwrap();
        let manager = &result[&PathBuf::from("/project/src/main/kotlin/Manager.kt")];
        assert!(
            manager.cognitive.is_none(),
            "Lizard does not produce cognitive complexity"
        );
    }

    #[test]
    fn test_parse_issues_above_threshold() {
        let result = parse_lizard_json(SAMPLE_JSON).unwrap();
        let manager = &result[&PathBuf::from("/project/src/main/kotlin/Manager.kt")];
        // processData (12) → issue; simpleHelper (2) → below threshold
        assert_eq!(manager.issues.len(), 1);
        assert_eq!(
            manager.issues[0].function_name.as_deref(),
            Some("processData")
        );
        assert_eq!(manager.issues[0].complexity_value, Some(12.0));
    }

    #[test]
    fn test_parse_service_below_threshold() {
        let result = parse_lizard_json(SAMPLE_JSON).unwrap();
        let service = &result[&PathBuf::from("/project/src/main/kotlin/Service.kt")];
        // doWork (4) is below threshold
        assert_eq!(service.issues.len(), 0);
        assert_eq!(service.cyclomatic, Some(4.0));
    }

    #[test]
    fn test_parse_empty_json() {
        let result = parse_lizard_json("[]").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_supports_kotlin_swift_java() {
        let analyzer = LizardAnalyzer;
        assert!(analyzer.supports_language(Language::Kotlin));
        assert!(analyzer.supports_language(Language::Swift));
        assert!(analyzer.supports_language(Language::Java));
        assert!(analyzer.supports_language(Language::Go));
        assert!(analyzer.supports_language(Language::CSharp));
        assert!(analyzer.supports_language(Language::ObjectiveC));
        assert!(analyzer.supports_language(Language::JavaScript));
        assert!(analyzer.supports_language(Language::TypeScript));
    }

    #[test]
    fn test_does_not_support_rust() {
        let analyzer = LizardAnalyzer;
        assert!(!analyzer.supports_language(Language::Rust));
        assert!(!analyzer.supports_language(Language::Cpp));
        assert!(!analyzer.supports_language(Language::Css));
    }

    #[test]
    fn test_setup_hint_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let hint = LizardAnalyzer.setup_hint(tmp.path());
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("lizard"));
    }
}
