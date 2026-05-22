//! Deterministic reducers for aggregating sub-task results.
//!
//! Reducers are pure functions: given a list of sub-task text results, they
//! produce a single aggregated text result without any LLM call.  This keeps
//! the aggregation step fast, auditable, and free of additional cloud egress.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// ReduceMode
// ─────────────────────────────────────────────────────────────────────────────

/// How to combine sub-task results into a single answer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReduceMode {
    /// Concatenate each result under a numbered heading.
    #[default]
    Summarize,
    /// Collect unique key=value pairs extracted from each result.
    ExtractFields,
    /// Majority-vote classification: pick the label that appears most often.
    Classify,
    /// Append all non-empty results separated by double newlines.
    Concat,
    /// Return only the first non-empty result (useful for "find first match").
    First,
}

// ─────────────────────────────────────────────────────────────────────────────
// Reducer
// ─────────────────────────────────────────────────────────────────────────────

/// Combines sub-task results into a single aggregated answer.
pub struct Reducer {
    mode: ReduceMode,
}

impl Reducer {
    pub fn new(mode: ReduceMode) -> Self {
        Self { mode }
    }

    /// Aggregate `results` into a single string.
    ///
    /// `results` is a slice of `(sub_task_label, text_output)` pairs.
    pub fn reduce(&self, results: &[(&str, &str)]) -> String {
        match &self.mode {
            ReduceMode::Summarize => self.summarize(results),
            ReduceMode::ExtractFields => self.extract_fields(results),
            ReduceMode::Classify => self.classify(results),
            ReduceMode::Concat => self.concat(results),
            ReduceMode::First => self.first(results),
        }
    }

    // ── reduce strategies ────────────────────────────────────────────────────

    fn summarize(&self, results: &[(&str, &str)]) -> String {
        if results.is_empty() {
            return String::from("No sub-task results to summarize.");
        }
        let mut out = String::new();
        for (i, (label, text)) in results.iter().enumerate() {
            out.push_str(&format!(
                "## Sub-task {} — {}\n\n{}\n\n",
                i + 1,
                label,
                text.trim()
            ));
        }
        out.trim_end().to_string()
    }

    fn extract_fields(&self, results: &[(&str, &str)]) -> String {
        // Each sub-task result is expected to contain lines of the form `key: value`.
        // We collect unique pairs and format them as a sorted table.
        use std::collections::BTreeMap;
        let mut fields: BTreeMap<String, String> = BTreeMap::new();

        for (_, text) in results {
            for line in text.lines() {
                if let Some((key, val)) = line.split_once(':') {
                    let k = key.trim().to_lowercase();
                    let v = val.trim().to_string();
                    if !k.is_empty() && !v.is_empty() {
                        fields.entry(k).or_insert(v);
                    }
                }
            }
        }

        if fields.is_empty() {
            return String::from("No key-value fields extracted from sub-task results.");
        }

        fields
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn classify(&self, results: &[(&str, &str)]) -> String {
        use std::collections::HashMap;
        let mut votes: HashMap<String, usize> = HashMap::new();

        for (_, text) in results {
            // Treat the first non-empty trimmed line as the classification label.
            if let Some(label) = text.lines().find(|l| !l.trim().is_empty()) {
                *votes.entry(label.trim().to_lowercase()).or_insert(0) += 1;
            }
        }

        if votes.is_empty() {
            return String::from("No classification labels found in sub-task results.");
        }

        let winner = votes
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(label, count)| {
                format!("{label} (votes: {count}/{total})", total = results.len())
            })
            .unwrap_or_else(|| "unknown".to_string());

        format!("Classification result: {winner}")
    }

    fn concat(&self, results: &[(&str, &str)]) -> String {
        results
            .iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .map(|(_, text)| text.trim().to_string())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn first(&self, results: &[(&str, &str)]) -> String {
        results
            .iter()
            .find(|(_, text)| !text.trim().is_empty())
            .map(|(_, text)| text.trim().to_string())
            .unwrap_or_else(|| "No results returned.".to_string())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_numbers_sections() {
        let r = Reducer::new(ReduceMode::Summarize);
        let results = [("Part A", "Answer A"), ("Part B", "Answer B")];
        let out = r.reduce(&results);
        assert!(out.contains("Sub-task 1"), "first section heading");
        assert!(out.contains("Part A"), "label present");
        assert!(out.contains("Answer A"), "content present");
        assert!(out.contains("Sub-task 2"), "second section heading");
    }

    #[test]
    fn extract_fields_collects_key_values() {
        let r = Reducer::new(ReduceMode::ExtractFields);
        let results = [
            ("task1", "status: ok\ncount: 42"),
            ("task2", "status: ok\nerror: none"),
        ];
        let out = r.reduce(&results);
        assert!(out.contains("status: ok"));
        assert!(out.contains("count: 42"));
    }

    #[test]
    fn classify_picks_majority() {
        let r = Reducer::new(ReduceMode::Classify);
        let results = [("t1", "positive"), ("t2", "positive"), ("t3", "negative")];
        let out = r.reduce(&results);
        assert!(out.contains("positive"), "majority label wins");
        assert!(out.contains("2/3"), "vote count shown");
    }

    #[test]
    fn first_returns_first_non_empty() {
        let r = Reducer::new(ReduceMode::First);
        let results = [("t1", ""), ("t2", "  "), ("t3", "hello")];
        assert_eq!(r.reduce(&results), "hello");
    }

    #[test]
    fn concat_joins_non_empty() {
        let r = Reducer::new(ReduceMode::Concat);
        let results = [("t1", "foo"), ("t2", ""), ("t3", "bar")];
        let out = r.reduce(&results);
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
        assert!(!out.contains("  "));
    }
}
