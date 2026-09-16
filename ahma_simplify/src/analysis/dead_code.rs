//! Exported-symbol reachability lens (the "dead code" lens).
//!
//! Flags public/crate-visible functions whose name appears nowhere else in the
//! scanned corpus. Grep-style reference counting cannot see a downstream crate
//! consuming a library's public API, macro-generated call sites, reflection by
//! string name, or a trait method reached only through a trait object — so every
//! result here is a candidate for review, never a verdict.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::analysis::source_tree::{FunctionNode, SourceTree, Visibility};
use crate::models::{DeadSymbol, Language, SymbolKind};

static IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("valid identifier regex"));

const SUPPRESSION_MARKERS: &[&str] = &[
    "#[allow(dead_code)]",
    "#[cfg(test)]",
    "#![allow(dead_code)]",
    "@SuppressWarnings",
    "# noqa",
    "eslint-disable",
    "pub use",
];

// A trait method reached only through a trait object has no textual call site, so
// name-based grep can never confirm these Rust names dead; they are overwhelmingly
// required trait implementations rather than genuinely unused code.
const RUST_TRAIT_LIKE_NAMES: &[&str] = &[
    "new", "default", "from", "try_from", "fmt", "drop", "clone", "eq", "hash", "next", "poll",
];

/// Finds exported functions with no apparent reference anywhere in `files`.
///
/// Pure function: takes every scanned file's path and full text plus the successfully
/// parsed subset (`trees`), and performs no I/O, so it is directly unit-testable. The
/// same input always produces identical output.
pub fn find_dead_symbols(files: &[(PathBuf, String)], trees: &[SourceTree]) -> Vec<DeadSymbol> {
    let counts = count_identifiers(files);
    let sources: HashMap<&Path, &str> = files
        .iter()
        .map(|(path, text)| (path.as_path(), text.as_str()))
        .collect();

    let mut dead = Vec::new();
    for tree in trees {
        if is_test_file(&tree.path) {
            continue;
        }
        let source = sources.get(tree.path.as_path()).copied().unwrap_or("");
        for function in &tree.functions {
            if !matches!(function.visibility, Visibility::Public | Visibility::Crate) {
                continue;
            }
            if !is_candidate_function(function, tree.language, source) {
                continue;
            }
            // A count of exactly 1 means the only occurrence in the whole corpus is
            // the definition itself; anything else has at least one other mention.
            let count = counts.get(function.name.as_str()).copied().unwrap_or(0);
            if count != 1 {
                continue;
            }
            dead.push(DeadSymbol {
                name: function.name.clone(),
                kind: if function.qualifier.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                file: tree.path.clone(),
                line: function.start_line,
                visibility: visibility_label(function.visibility).to_string(),
                references_found: count.saturating_sub(1),
            });
        }
    }

    dead.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });
    dead
}

fn is_candidate_function(function: &FunctionNode, language: Language, source: &str) -> bool {
    if function.name == "main" {
        return false;
    }
    if function.name.starts_with("test_") {
        return false;
    }
    if language == Language::Rust && RUST_TRAIT_LIKE_NAMES.contains(&function.name.as_str()) {
        return false;
    }
    !has_suppression_marker(source, function.start_line)
}

fn has_suppression_marker(source: &str, start_line: usize) -> bool {
    let fn_line_index = start_line.saturating_sub(1);
    let preceding_start = fn_line_index.saturating_sub(3);
    source
        .lines()
        .skip(preceding_start)
        .take(fn_line_index - preceding_start)
        .any(|line| {
            SUPPRESSION_MARKERS
                .iter()
                .any(|marker| line.contains(*marker))
        })
}

fn is_test_file(path: &Path) -> bool {
    let in_tests_dir = path
        .components()
        .any(|c| c.as_os_str().to_str() == Some("tests"));
    let test_suffix = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with("_test.rs") || name.ends_with("_tests.rs"));
    in_tests_dir || test_suffix
}

fn visibility_label(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "pub",
        Visibility::Crate => "pub(crate)",
        Visibility::Private => "private",
    }
}

fn count_identifiers(files: &[(PathBuf, String)]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_, text) in files {
        for m in IDENTIFIER.find_iter(text) {
            *counts.entry(m.as_str().to_string()).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::source_tree::parse_source_tree;
    use std::fs;
    use tempfile::TempDir;

    fn make_function(name: &str, visibility: Visibility, start_line: usize) -> FunctionNode {
        FunctionNode {
            name: name.to_string(),
            qualifier: None,
            visibility,
            start_line,
            end_line: start_line + 1,
            body_statements: 1,
            calls: vec![],
        }
    }

    fn make_tree(path: &str, language: Language, functions: Vec<FunctionNode>) -> SourceTree {
        SourceTree {
            path: PathBuf::from(path),
            language,
            functions,
        }
    }

    #[test]
    fn pub_fn_with_no_references_is_reported() {
        let files = vec![(
            PathBuf::from("src/lib.rs"),
            "pub fn orphan() {}\n".to_string(),
        )];
        let trees = vec![make_tree(
            "src/lib.rs",
            Language::Rust,
            vec![make_function("orphan", Visibility::Public, 1)],
        )];
        let dead = find_dead_symbols(&files, &trees);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].name, "orphan");
        assert_eq!(dead[0].kind, SymbolKind::Function);
        assert_eq!(dead[0].visibility, "pub");
        assert_eq!(dead[0].references_found, 0);
    }

    #[test]
    fn pub_fn_called_from_another_file_is_not_reported() {
        let files = vec![
            (
                PathBuf::from("src/lib.rs"),
                "pub fn used() {}\n".to_string(),
            ),
            (
                PathBuf::from("src/main.rs"),
                "fn main() { used(); }\n".to_string(),
            ),
        ];
        let trees = vec![
            make_tree(
                "src/lib.rs",
                Language::Rust,
                vec![make_function("used", Visibility::Public, 1)],
            ),
            make_tree(
                "src/main.rs",
                Language::Rust,
                vec![make_function("main", Visibility::Crate, 1)],
            ),
        ];
        let dead = find_dead_symbols(&files, &trees);
        assert!(dead.is_empty());
    }

    #[test]
    fn private_fn_is_never_reported() {
        let files = vec![(PathBuf::from("src/lib.rs"), "fn orphan() {}\n".to_string())];
        let trees = vec![make_tree(
            "src/lib.rs",
            Language::Rust,
            vec![make_function("orphan", Visibility::Private, 1)],
        )];
        let dead = find_dead_symbols(&files, &trees);
        assert!(dead.is_empty());
    }

    #[test]
    fn main_function_is_skipped() {
        let files = vec![(PathBuf::from("src/main.rs"), "fn main() {}\n".to_string())];
        let trees = vec![make_tree(
            "src/main.rs",
            Language::Rust,
            vec![make_function("main", Visibility::Crate, 1)],
        )];
        let dead = find_dead_symbols(&files, &trees);
        assert!(dead.is_empty());
    }

    #[test]
    fn allow_dead_code_annotated_fn_is_skipped() {
        let source = "#[allow(dead_code)]\npub fn orphan() {}\n";
        let files = vec![(PathBuf::from("src/lib.rs"), source.to_string())];
        let trees = vec![make_tree(
            "src/lib.rs",
            Language::Rust,
            vec![make_function("orphan", Visibility::Public, 2)],
        )];
        let dead = find_dead_symbols(&files, &trees);
        assert!(dead.is_empty());
    }

    #[test]
    fn test_prefixed_fn_is_skipped() {
        let files = vec![(
            PathBuf::from("src/lib.rs"),
            "pub fn test_helper() {}\n".to_string(),
        )];
        let trees = vec![make_tree(
            "src/lib.rs",
            Language::Rust,
            vec![make_function("test_helper", Visibility::Public, 1)],
        )];
        let dead = find_dead_symbols(&files, &trees);
        assert!(dead.is_empty());
    }

    #[test]
    fn file_under_tests_directory_is_skipped() {
        let files = vec![(
            PathBuf::from("tests/helpers.rs"),
            "pub fn orphan_helper() {}\n".to_string(),
        )];
        let trees = vec![make_tree(
            "tests/helpers.rs",
            Language::Rust,
            vec![make_function("orphan_helper", Visibility::Public, 1)],
        )];
        let dead = find_dead_symbols(&files, &trees);
        assert!(dead.is_empty());
    }

    #[test]
    fn rust_trait_like_name_is_skipped() {
        let files = vec![(PathBuf::from("src/lib.rs"), "pub fn new() {}\n".to_string())];
        let trees = vec![make_tree(
            "src/lib.rs",
            Language::Rust,
            vec![make_function("new", Visibility::Public, 1)],
        )];
        let dead = find_dead_symbols(&files, &trees);
        assert!(dead.is_empty());
    }

    #[test]
    fn is_deterministic_across_runs() {
        let files = vec![
            (
                PathBuf::from("src/a.rs"),
                "pub fn orphan_a() {}\n".to_string(),
            ),
            (
                PathBuf::from("src/b.rs"),
                "pub fn orphan_b() {}\n".to_string(),
            ),
        ];
        let trees = vec![
            make_tree(
                "src/a.rs",
                Language::Rust,
                vec![make_function("orphan_a", Visibility::Public, 1)],
            ),
            make_tree(
                "src/b.rs",
                Language::Rust,
                vec![make_function("orphan_b", Visibility::Public, 1)],
            ),
        ];
        let first = find_dead_symbols(&files, &trees);
        let second = find_dead_symbols(&files, &trees);
        assert_eq!(first.len(), 2);
        assert_eq!(first, second);
    }

    #[test]
    fn end_to_end_with_real_parser() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("lib.rs");
        let source = "pub fn orphan() {}\n\nfn caller() {\n    used();\n}\n\npub fn used() {}\n";
        fs::write(&file_path, source).unwrap();
        let tree = parse_source_tree(&file_path).expect("parses");
        let files = vec![(file_path.clone(), source.to_string())];
        let trees = vec![tree];

        let dead = find_dead_symbols(&files, &trees);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].name, "orphan");
        assert_eq!(dead[0].file, file_path);
    }
}
