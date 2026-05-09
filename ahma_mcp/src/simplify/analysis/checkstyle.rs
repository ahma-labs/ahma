//! Shared Checkstyle XML parser for external complexity analyzers.
//!
//! Detekt (Gradle), detekt-cli, and SwiftLint all emit Checkstyle-format XML.
//! This module provides a single parser that all three can share, parameterised
//! by the `analyzer_name` string written into [`ExternalMetrics::analyzer`].
//!
//! # Expected XML structure
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <checkstyle version="8.0">
//!   <file name="/abs/path/to/File.kt">
//!     <error line="10" column="5" severity="warning"
//!            message="The function foo has a cyclomatic complexity of 12 (threshold = 1)"
//!            source="detekt.complexity.CyclomaticComplexMethod"/>
//!   </file>
//! </checkstyle>
//! ```

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::external::{ExternalIssue, ExternalMetrics, Severity};

/// Parse a Checkstyle-format XML string and return per-file [`ExternalMetrics`].
///
/// `analyzer_name` is written verbatim into [`ExternalMetrics::analyzer`] for
/// every file entry (e.g. `"detekt"`, `"detekt-cli"`, `"swiftlint"`, `"lizard"`).
pub fn parse_checkstyle_xml_str(
    content: &str,
    analyzer_name: &str,
) -> Result<HashMap<PathBuf, ExternalMetrics>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut metrics: HashMap<PathBuf, ExternalMetrics> = HashMap::new();
    let mut current_file: Option<PathBuf> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"file" => {
                current_file = get_attr(e, b"name").map(PathBuf::from);
                if let Some(ref p) = current_file {
                    metrics.entry(p.clone()).or_insert_with(|| ExternalMetrics {
                        analyzer: analyzer_name.to_string(),
                        ..ExternalMetrics::default()
                    });
                }
            }
            Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"error"
                    && let Some(ref file_path) = current_file
                    && let Some(issue) = parse_error_element(e)
                {
                    let entry =
                        metrics
                            .entry(file_path.clone())
                            .or_insert_with(|| ExternalMetrics {
                                analyzer: analyzer_name.to_string(),
                                ..ExternalMetrics::default()
                            });
                    accumulate_issue(entry, &issue);
                    entry.issues.push(issue);
                }
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"file" => {
                current_file = None;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                eprintln!(
                    "  [{}] XML parse error at position {}: {e}",
                    analyzer_name,
                    reader.error_position()
                );
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(metrics)
}

/// Parse a Checkstyle-format XML file from disk and return per-file metrics.
pub fn parse_checkstyle_xml(
    path: &Path,
    analyzer_name: &str,
) -> Result<HashMap<PathBuf, ExternalMetrics>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read Checkstyle report '{}'", path.display()))?;
    parse_checkstyle_xml_str(&content, analyzer_name)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Extract the value of an XML attribute by name, unescaping entity references.
pub fn get_attr(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| a.unescape_value().ok())
        .map(|v| v.into_owned())
}

/// Parse a single `<error …/>` element into an [`ExternalIssue`].
pub fn parse_error_element(e: &quick_xml::events::BytesStart<'_>) -> Option<ExternalIssue> {
    let start_line: u32 = get_attr(e, b"line")?.parse().unwrap_or(0);
    let message = get_attr(e, b"message").unwrap_or_default();
    let source = get_attr(e, b"source").unwrap_or_default();
    let severity_str = get_attr(e, b"severity").unwrap_or_default();

    // Extract the bare rule name from the fully-qualified source ID
    // e.g. "detekt.complexity.CyclomaticComplexMethod" → "CyclomaticComplexMethod"
    let rule = source.rsplit('.').next().unwrap_or(&source).to_string();

    let severity = match severity_str.as_str() {
        "error" => Severity::Error,
        "info" => Severity::Info,
        _ => Severity::Warning,
    };

    Some(ExternalIssue {
        rule,
        severity,
        function_name: extract_function_name(&message),
        complexity_value: extract_complexity_value(&message),
        message,
        start_line,
    })
}

/// Accumulate an [`ExternalIssue`] into the per-file aggregate metrics.
///
/// Cyclomatic and cognitive totals are summed across all reported functions.
pub fn accumulate_issue(entry: &mut ExternalMetrics, issue: &ExternalIssue) {
    let increment = issue.complexity_value.unwrap_or(1.0);
    match rule_kind(&issue.rule) {
        RuleKind::Cyclomatic => {
            *entry.cyclomatic.get_or_insert(0.0) += increment;
        }
        RuleKind::Cognitive => {
            *entry.cognitive.get_or_insert(0.0) += increment;
        }
        RuleKind::Other => {}
    }
}

/// Classify a rule name as cyclomatic, cognitive, or other.
pub enum RuleKind {
    Cyclomatic,
    Cognitive,
    Other,
}

pub fn rule_kind(rule: &str) -> RuleKind {
    let lower = rule.to_lowercase();
    if lower.contains("cyclomatic") {
        RuleKind::Cyclomatic
    } else if lower.contains("cognitive") {
        RuleKind::Cognitive
    } else {
        RuleKind::Other
    }
}

/// Extract the function name from an analyzer message.
///
/// Handles both plain (`The function processData …`) and
/// backtick-quoted (`The function \`handleResult\` …`) forms, as well as
/// the SwiftLint form (`Function body should span X lines or less (currently Y)`
/// with the function name coming from the `message` context rather than
/// the `source` attribute, which SwiftLint puts in the `source` field).
pub fn extract_function_name(message: &str) -> Option<String> {
    // Detekt/standard pattern: "function <name>" or "function `<name>`"
    let prefix = "function ";
    if let Some(start) = message.find(prefix) {
        let rest = &message[start + prefix.len()..];
        let name = if let Some(inner) = rest.strip_prefix('`') {
            let end = inner.find('`')?;
            inner[..end].to_string()
        } else {
            rest.split_whitespace().next()?.to_string()
        };
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Extract a numeric complexity value from an analyzer message.
///
/// Handles patterns such as:
/// - `"…cyclomatic complexity of 12 (threshold…"` → `12.0`
/// - `"…is too long (85/1)…"` → `85.0`
/// - SwiftLint: `"…should span 40 lines or less (currently 85)…"` → `85.0`
pub fn extract_complexity_value(message: &str) -> Option<f64> {
    // Pattern: " of <N>" (used by CyclomaticComplexMethod, CognitiveComplexMethod)
    if let Some(pos) = message.find(" of ") {
        let rest = &message[pos + " of ".len()..];
        let num: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }

    // SwiftLint/generic pattern: "(currently <N>)"
    if let Some(pos) = message.find("currently ") {
        let rest = &message[pos + "currently ".len()..];
        let num: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }

    // Pattern: "(<N>/<threshold>)" used by LongMethod and similar rules.
    if let Some(start) = message.rfind('(') {
        let rest = &message[start + 1..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !num.is_empty()
            && let Ok(v) = num.parse::<f64>()
        {
            return Some(v);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<checkstyle version="8.0">
  <file name="/project/src/main/kotlin/Foo.kt">
    <error line="10" column="5" severity="warning"
           message="The function processData has a cyclomatic complexity of 12 (threshold = 1)"
           source="detekt.complexity.CyclomaticComplexMethod"/>
    <error line="30" column="1" severity="warning"
           message="The function `handleResult` has a cognitive complexity of 8 (threshold = 1)"
           source="detekt.complexity.CognitiveComplexMethod"/>
  </file>
  <file name="/project/src/main/kotlin/Bar.kt">
    <error line="5" column="1" severity="error"
           message="The function init has a cyclomatic complexity of 3 (threshold = 1)"
           source="detekt.complexity.CyclomaticComplexMethod"/>
  </file>
</checkstyle>"#;

    #[test]
    fn test_parse_two_files() {
        let result = parse_checkstyle_xml_str(SAMPLE_XML, "detekt").unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_cyclomatic_accumulated() {
        let result = parse_checkstyle_xml_str(SAMPLE_XML, "detekt").unwrap();
        let foo = result
            .get(&PathBuf::from("/project/src/main/kotlin/Foo.kt"))
            .unwrap();
        // Only CyclomaticComplexMethod contributes to cyclomatic
        assert_eq!(foo.cyclomatic, Some(12.0));
    }

    #[test]
    fn test_cognitive_accumulated() {
        let result = parse_checkstyle_xml_str(SAMPLE_XML, "detekt").unwrap();
        let foo = result
            .get(&PathBuf::from("/project/src/main/kotlin/Foo.kt"))
            .unwrap();
        assert_eq!(foo.cognitive, Some(8.0));
    }

    #[test]
    fn test_analyzer_name_propagated() {
        let result = parse_checkstyle_xml_str(SAMPLE_XML, "detekt-cli").unwrap();
        for m in result.values() {
            assert_eq!(m.analyzer, "detekt-cli");
        }
    }

    #[test]
    fn test_function_name_extraction_plain() {
        assert_eq!(
            extract_function_name("The function processData has a cyclomatic complexity of 12"),
            Some("processData".to_string())
        );
    }

    #[test]
    fn test_function_name_extraction_backtick() {
        assert_eq!(
            extract_function_name("The function `handleResult` has a cognitive complexity of 8"),
            Some("handleResult".to_string())
        );
    }

    #[test]
    fn test_complexity_value_of_pattern() {
        assert_eq!(
            extract_complexity_value("cyclomatic complexity of 12 (threshold = 1)"),
            Some(12.0)
        );
    }

    #[test]
    fn test_complexity_value_currently_pattern() {
        assert_eq!(
            extract_complexity_value("Function body should span 40 lines or less (currently 85)"),
            Some(85.0)
        );
    }
}
