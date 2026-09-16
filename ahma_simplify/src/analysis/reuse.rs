//! Language-agnostic duplicate code block detector (the "reuse" lens).
//!
//! Finds copy-pasted blocks of source across (or within) files so an AI agent can
//! decide whether to extract a shared helper. Comment stripping is purely textual,
//! so a comment delimiter inside a string literal (e.g. `"https://example.com"`) is
//! misread as a comment start; a real per-language lexer would fix this but is out
//! of proportion for this lens.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use rustc_hash::FxHasher;

use crate::models::{DuplicateGroup, DuplicateLocation, Language};

/// Configuration for [`find_duplicates`].
#[derive(Debug, Clone, Copy)]
pub struct ReuseConfig {
    /// Minimum number of consecutive matching normalized lines to report as a duplicate.
    pub min_lines: usize,
}

impl Default for ReuseConfig {
    fn default() -> Self {
        Self { min_lines: 4 }
    }
}

/// A source line after comment-stripping and whitespace-collapsing.
struct NormalizedLine {
    /// 1-based line number in the original source.
    original_line: usize,
    normalized: String,
    /// Untrimmed original text, used for sample output.
    raw: String,
}

/// Finds duplicated blocks of source across `files`.
///
/// Pure function: takes every file's path and full text as input and performs no I/O,
/// so it is directly unit-testable. The same input always produces byte-identical output.
pub fn find_duplicates(files: &[(PathBuf, String)], config: &ReuseConfig) -> Vec<DuplicateGroup> {
    let min_lines = config.min_lines.max(1);
    let normalized: Vec<Vec<NormalizedLine>> = files
        .iter()
        .map(|(path, text)| normalize_file(text, Language::from_path(path)))
        .collect();

    let mut buckets: BTreeMap<u64, Vec<(usize, usize)>> = BTreeMap::new();
    for (file_idx, lines) in normalized.iter().enumerate() {
        if lines.len() < min_lines {
            continue;
        }
        for start in 0..=(lines.len() - min_lines) {
            let hash = hash_window(&lines[start..start + min_lines]);
            buckets.entry(hash).or_default().push((file_idx, start));
        }
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    for occurrences in buckets.into_values() {
        if occurrences.len() < 2 {
            continue;
        }
        // FxHash is speed-optimized and non-cryptographic: two genuinely different
        // windows can share a hash, so occurrences must be re-grouped by actual
        // normalized content before they are trusted as duplicates.
        let mut content_groups: BTreeMap<Vec<&str>, Vec<(usize, usize)>> = BTreeMap::new();
        for (file_idx, start) in occurrences {
            let key: Vec<&str> = normalized[file_idx][start..start + min_lines]
                .iter()
                .map(|line| line.normalized.as_str())
                .collect();
            content_groups
                .entry(key)
                .or_default()
                .push((file_idx, start));
        }
        for group_occurrences in content_groups.into_values() {
            if group_occurrences.len() < 2 {
                continue;
            }
            if let Some(candidate) = build_candidate(&normalized, group_occurrences, min_lines) {
                candidates.push(candidate);
            }
        }
    }

    // Longest matches are accepted first so a long duplicate is never suppressed by
    // a shorter one that merely overlaps it.
    candidates.sort_by(|a, b| {
        b.line_count
            .cmp(&a.line_count)
            .then_with(|| a.first_occurrence().cmp(&b.first_occurrence()))
    });

    let mut accepted_ranges: Vec<Vec<(usize, usize)>> = vec![Vec::new(); files.len()];
    let mut accepted: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        let suppressed = candidate.occurrences.iter().any(|&(file_idx, start)| {
            let end = start + candidate.line_count - 1;
            accepted_ranges[file_idx]
                .iter()
                .any(|&(acc_start, acc_end)| acc_start <= start && end <= acc_end)
        });
        if suppressed {
            continue;
        }
        for &(file_idx, start) in &candidate.occurrences {
            let end = start + candidate.line_count - 1;
            accepted_ranges[file_idx].push((start, end));
        }
        accepted.push(candidate);
    }

    let mut groups: Vec<DuplicateGroup> = accepted
        .into_iter()
        .map(|candidate| candidate.into_duplicate_group(files, &normalized))
        .collect();

    groups.sort_by(|a, b| {
        let impact_a = a.line_count * a.locations.len();
        let impact_b = b.line_count * b.locations.len();
        impact_b
            .cmp(&impact_a)
            .then_with(|| first_location_key(a).cmp(&first_location_key(b)))
    });

    groups
}

fn first_location_key(group: &DuplicateGroup) -> (PathBuf, usize, usize) {
    group
        .locations
        .first()
        .map(|loc| (loc.file.clone(), loc.start_line, loc.end_line))
        .unwrap_or_default()
}

/// A verified group of matching windows before overlap suppression and output shaping.
struct Candidate {
    /// `(file_idx, start_index)` pairs, sorted, with same-file self-overlaps already removed.
    occurrences: Vec<(usize, usize)>,
    line_count: usize,
}

impl Candidate {
    fn first_occurrence(&self) -> (usize, usize) {
        self.occurrences[0]
    }

    fn into_duplicate_group(
        self,
        files: &[(PathBuf, String)],
        normalized: &[Vec<NormalizedLine>],
    ) -> DuplicateGroup {
        let (first_file, first_start) = self.occurrences[0];
        let first_window = &normalized[first_file][first_start..first_start + self.line_count];
        let hash = hash_window(first_window);
        let sample_text = first_window
            .iter()
            .map(|line| line.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let locations = self
            .occurrences
            .iter()
            .map(|&(file_idx, start)| DuplicateLocation {
                file: files[file_idx].0.clone(),
                start_line: normalized[file_idx][start].original_line,
                end_line: normalized[file_idx][start + self.line_count - 1].original_line,
            })
            .collect();

        DuplicateGroup {
            hash,
            line_count: self.line_count,
            locations,
            sample_text,
        }
    }
}

/// Extends a group of content-identical `min_lines` windows as far as every occurrence
/// agrees, then drops same-file occurrences that overlap an earlier one in the group —
/// a run of N identical consecutive lines is one repeated block, not N copies of itself.
fn build_candidate(
    normalized: &[Vec<NormalizedLine>],
    mut occurrences: Vec<(usize, usize)>,
    min_lines: usize,
) -> Option<Candidate> {
    occurrences.sort_unstable();

    let mut line_count = min_lines;
    loop {
        let mut next_content: Option<&str> = None;
        let mut can_extend = true;
        for &(file_idx, start) in &occurrences {
            let Some(line) = normalized[file_idx].get(start + line_count) else {
                can_extend = false;
                break;
            };
            match next_content {
                None => next_content = Some(line.normalized.as_str()),
                Some(expected) if expected == line.normalized => {}
                Some(_) => {
                    can_extend = false;
                    break;
                }
            }
        }
        if can_extend {
            line_count += 1;
        } else {
            break;
        }
    }

    let mut kept: Vec<(usize, usize)> = Vec::new();
    let mut last_end_by_file: Vec<Option<usize>> = vec![None; normalized.len()];
    for (file_idx, start) in occurrences {
        let end = start + line_count - 1;
        let overlaps = last_end_by_file[file_idx].is_some_and(|last_end| start <= last_end);
        if overlaps {
            continue;
        }
        last_end_by_file[file_idx] = Some(end);
        kept.push((file_idx, start));
    }

    if kept.len() < 2 {
        return None;
    }
    Some(Candidate {
        occurrences: kept,
        line_count,
    })
}

fn hash_window(window: &[NormalizedLine]) -> u64 {
    let mut hasher = FxHasher::default();
    for line in window {
        line.normalized.hash(&mut hasher);
    }
    hasher.finish()
}

fn normalize_file(text: &str, lang: Language) -> Vec<NormalizedLine> {
    let single = lang.single_line_comment();
    let multi = lang.multi_line_comment();
    let mut in_block_comment = false;
    let mut result = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let stripped = strip_comments(raw, single, multi, &mut in_block_comment);
        let normalized = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            continue;
        }
        result.push(NormalizedLine {
            original_line: idx + 1,
            normalized,
            raw: raw.to_string(),
        });
    }
    result
}

/// Strips single- and multi-line comments from one source line, tracking block-comment
/// state across calls via `in_block_comment` (mutated in place, one call per line).
fn strip_comments(
    line: &str,
    single: Option<&'static str>,
    multi: Option<(&'static str, &'static str)>,
    in_block_comment: &mut bool,
) -> String {
    let multi_start = multi.map(|(start, _)| start);
    let multi_end = multi.map(|(_, end)| end);
    let mut result = String::new();
    let mut rest = line;

    loop {
        if *in_block_comment {
            match multi_end.and_then(|end| rest.find(end)) {
                Some(pos) => {
                    let end_len = multi_end.map_or(0, str::len);
                    rest = &rest[pos + end_len..];
                    *in_block_comment = false;
                }
                None => return result,
            }
            continue;
        }

        let single_pos = single.and_then(|token| rest.find(token));
        let multi_pos = multi_start.and_then(|token| rest.find(token));
        let cut = match (single_pos, multi_pos) {
            (None, None) => None,
            (Some(sp), None) => Some((sp, false)),
            (None, Some(mp)) => Some((mp, true)),
            (Some(sp), Some(mp)) => Some(if sp <= mp { (sp, false) } else { (mp, true) }),
        };

        match cut {
            None => {
                result.push_str(rest);
                return result;
            }
            Some((pos, is_multi_start)) => {
                result.push_str(&rest[..pos]);
                if !is_multi_start {
                    return result;
                }
                let start_len = multi_start.map_or(0, str::len);
                rest = &rest[pos + start_len..];
                *in_block_comment = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const COMPUTE_BLOCK: &str = "fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let product = a * b;\n    sum + product\n}\n";

    // ── normalize_file ──────────────────────────────────────────────────────

    #[test]
    fn strips_trailing_single_line_comment() {
        let src = "let x = 1; // comment\nlet y = 2;\n";
        let lines = normalize_file(src, Language::Rust);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].normalized, "let x = 1;");
        assert_eq!(lines[1].normalized, "let y = 2;");
    }

    #[test]
    fn strips_multi_line_comment_spanning_lines() {
        let src = "let x = 1;\n/*\nthis is a comment\nspanning lines\n*/\nlet y = 2;\n";
        let lines = normalize_file(src, Language::Rust);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].normalized, "let x = 1;");
        assert_eq!(lines[1].normalized, "let y = 2;");
        assert_eq!(lines[1].original_line, 6);
    }

    #[test]
    fn strips_inline_multi_line_comment_mid_code() {
        let src = "let x = /* inline */ 1;\n";
        let lines = normalize_file(src, Language::Rust);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].normalized, "let x = 1;");
    }

    #[test]
    fn strips_multi_line_comment_that_opens_and_closes_on_different_lines_with_code_around() {
        let src = "let x = 1; /* start\nignored line\nend */ let y = 2;\n";
        let lines = normalize_file(src, Language::Rust);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].normalized, "let x = 1;");
        assert_eq!(lines[1].normalized, "let y = 2;");
        assert_eq!(lines[1].original_line, 3);
    }

    #[test]
    fn collapses_whitespace_runs_and_trims() {
        let src = "let   x    =\t1;   \n";
        let lines = normalize_file(src, Language::Rust);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].normalized, "let x = 1;");
        assert_eq!(lines[0].raw, "let   x    =\t1;   ");
    }

    #[test]
    fn strips_python_single_line_comment() {
        let src = "def add(a, b):\n    return a + b  # sum\n";
        let lines = normalize_file(src, Language::Python);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].normalized, "def add(a, b):");
        assert_eq!(lines[1].normalized, "return a + b");
    }

    // ── find_duplicates ─────────────────────────────────────────────────────

    #[test]
    fn reuse_config_default_min_lines_is_four() {
        assert_eq!(ReuseConfig::default().min_lines, 4);
    }

    #[test]
    fn finds_duplicate_across_two_files() {
        let files = vec![
            (
                PathBuf::from("src/a.rs"),
                format!("{COMPUTE_BLOCK}\nfn other_a() {{}}\n"),
            ),
            (
                PathBuf::from("src/b.rs"),
                format!("fn other_b() {{}}\n\n{COMPUTE_BLOCK}"),
            ),
        ];
        let groups = find_duplicates(&files, &ReuseConfig::default());
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.line_count, 5);
        assert_eq!(group.locations.len(), 2);
        assert!(
            group
                .locations
                .iter()
                .any(|loc| loc.file == Path::new("src/a.rs"))
        );
        assert!(
            group
                .locations
                .iter()
                .any(|loc| loc.file == Path::new("src/b.rs"))
        );
        let expected_sample: String = COMPUTE_BLOCK.lines().collect::<Vec<_>>().join("\n");
        assert_eq!(group.sample_text, expected_sample);
    }

    #[test]
    fn finds_duplicate_twice_within_one_file_non_overlapping() {
        let source = format!(
            "{COMPUTE_BLOCK}\nfn unrelated() {{\n    println!(\"noop\");\n}}\n{COMPUTE_BLOCK}"
        );
        let files = vec![(PathBuf::from("src/dup.rs"), source)];
        let groups = find_duplicates(&files, &ReuseConfig::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].locations.len(), 2);
        let (loc0, loc1) = (&groups[0].locations[0], &groups[0].locations[1]);
        assert_eq!(loc0.file, loc1.file);
        assert!(loc0.end_line < loc1.start_line || loc1.end_line < loc0.start_line);
    }

    #[test]
    fn self_overlapping_run_is_not_reported_as_duplicated() {
        let source =
            "let x = 1;\nlet x = 1;\nlet x = 1;\nlet x = 1;\nlet x = 1;\nlet x = 1;\n".to_string();
        let files = vec![(PathBuf::from("src/repeat.rs"), source)];
        let groups = find_duplicates(&files, &ReuseConfig::default());
        assert!(
            groups.is_empty(),
            "a single repetitive run must not be reported as copies of itself"
        );
    }

    #[test]
    fn near_miss_differing_by_one_line_is_not_reported() {
        let block_a = "fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let product = a * b;\n    sum + product\n}\n";
        let block_b = "fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let product = a - b;\n    sum + product\n}\n";
        let files = vec![
            (PathBuf::from("src/a.rs"), block_a.to_string()),
            (PathBuf::from("src/b.rs"), block_b.to_string()),
        ];
        let groups = find_duplicates(&files, &ReuseConfig::default());
        assert!(groups.is_empty());
    }

    #[test]
    fn blocks_shorter_than_min_lines_are_not_reported() {
        let block = "let a = 1;\nlet b = 2;\nlet c = 3;\n";
        let files = vec![
            (PathBuf::from("src/a.rs"), block.to_string()),
            (PathBuf::from("src/b.rs"), block.to_string()),
        ];
        let groups = find_duplicates(&files, &ReuseConfig::default());
        assert!(groups.is_empty());
    }

    #[test]
    fn respects_custom_min_lines() {
        let block = "let a = 1;\nlet b = 2;\nlet c = 3;\n";
        let files = vec![
            (PathBuf::from("src/a.rs"), block.to_string()),
            (PathBuf::from("src/b.rs"), block.to_string()),
        ];
        let groups = find_duplicates(&files, &ReuseConfig { min_lines: 2 });
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].line_count, 3);
    }

    #[test]
    fn is_deterministic_across_runs() {
        let files = vec![
            (PathBuf::from("src/a.rs"), format!("{COMPUTE_BLOCK}\n")),
            (PathBuf::from("src/b.rs"), format!("{COMPUTE_BLOCK}\n")),
        ];
        let config = ReuseConfig::default();
        let first = find_duplicates(&files, &config);
        let second = find_duplicates(&files, &config);
        assert!(!first.is_empty());
        assert_eq!(first, second);
    }
}
